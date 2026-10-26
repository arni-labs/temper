//! Reading the remote's record of this file's pushes, outside the engine.
//!
//! The engine pushes only changes numbered above the remote's record for this
//! file (`turso_sync_last_change_id`), and never says what that record was.
//! Temper reads it itself to tell whether the file is behind the remote, and
//! whether a push the remote refused still moved it.

use std::time::Duration;

use super::DriverError;

/// One request for the record.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A place in a file's change log. The engine numbers changes from 1 again
/// after every pull from the remote (a new generation), so a change number
/// alone does not order two places across a pull; the pair does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Mark {
    pub(super) generation: i64,
    pub(super) change: i64,
}

impl std::fmt::Display for Mark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.generation {
            0 => write!(f, "{}", self.change),
            generation => write!(f, "{} in generation {generation}", self.change),
        }
    }
}

/// Where the remote is and who this file is to it.
pub(super) struct Remote {
    pipeline_url: String,
    auth_token: String,
    client_id: String,
    http: reqwest::Client,
}

impl Remote {
    pub(super) fn new(remote_url: &str, auth_token: &str, client_id: String) -> Self {
        let base = remote_url
            .trim()
            .trim_end_matches('/')
            .replacen("libsql://", "https://", 1)
            .replacen("turso://", "https://", 1);
        Self {
            pipeline_url: format!("{base}/v2/pipeline"),
            auth_token: auth_token.to_string(),
            client_id,
            http: reqwest::Client::new(),
        }
    }

    /// The last change of this file the remote has applied; the start of the
    /// log when it has none.
    pub(super) async fn last_change(&self) -> Result<Mark, DriverError> {
        let results = self
            .pipeline(vec![serde_json::json!({
                "sql": "SELECT pull_gen, change_id FROM turso_sync_last_change_id WHERE client_id = ?",
                "args": [{ "type": "text", "value": self.client_id }],
            })])
            .await?;
        let result = &results[0];
        if result["type"] == "error" {
            let message = result["error"]["message"].as_str().unwrap_or_default();
            // Nothing has been pushed to this remote yet.
            if message.contains("no such table") {
                return Ok(Mark::default());
            }
            return Err(failed(message.to_string()));
        }
        // Any other answer is an error, never a 0: a 0 would turn off the
        // checks that read this.
        let unexpected = || failed(format!("unexpected answer: {result}"));
        let rows = rows(result).ok_or_else(unexpected)?;
        // No row: nothing has been pushed from this file yet.
        let Some(row) = rows.first() else {
            return Ok(Mark::default());
        };
        Ok(Mark {
            generation: integer(&row[0]).ok_or_else(unexpected)?,
            change: integer(&row[1]).ok_or_else(unexpected)?,
        })
    }

    /// The number of rows the remote holds in each of `tables`, in order.
    pub(super) async fn row_counts(&self, tables: &[String]) -> Result<Vec<i64>, DriverError> {
        let statements = tables
            .iter()
            .map(|table| serde_json::json!({ "sql": format!("SELECT COUNT(*) FROM {}", quote(table)) }))
            .collect();
        let results = self.pipeline(statements).await?;
        tables
            .iter()
            .zip(&results)
            .map(|(table, result)| {
                rows(result)
                    .and_then(|rows| integer(&rows.first()?[0]))
                    .ok_or_else(|| failed(format!("count {table}: {result}")))
            })
            .collect()
    }

    /// Run `statements` on the remote in one request; one result each.
    async fn pipeline(
        &self,
        statements: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, DriverError> {
        let wanted = statements.len();
        let requests: Vec<_> = statements
            .into_iter()
            .map(|stmt| serde_json::json!({ "type": "execute", "stmt": stmt }))
            .collect();
        let answer: serde_json::Value = self
            .http
            .post(&self.pipeline_url)
            .bearer_auth(&self.auth_token)
            .json(&serde_json::json!({ "requests": requests }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| failed(error.to_string()))?
            .json()
            .await
            .map_err(|error| failed(error.to_string()))?;
        match answer["results"].as_array() {
            Some(results) if results.len() == wanted => Ok(results.clone()),
            _ => Err(failed(format!("unexpected answer: {answer}"))),
        }
    }
}

fn failed(what: String) -> DriverError {
    DriverError::SyncSetup(format!("ask the remote copy: {what}"))
}

/// The rows of a statement that succeeded.
fn rows(result: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    if result["type"] != "ok" {
        return None;
    }
    result["response"]["result"]["rows"].as_array()
}

/// An integer cell; integers arrive as strings in the Hrana protocol, and a
/// null counts as 0.
fn integer(cell: &serde_json::Value) -> Option<i64> {
    match &cell["value"] {
        serde_json::Value::String(text) => text.parse().ok(),
        serde_json::Value::Number(number) => number.as_i64(),
        _ if cell["type"] == "null" => Some(0),
        _ => None,
    }
}

pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The client id the engine gave this file, from its sync metadata.
pub(super) fn client_id(info: &serde_json::Value) -> String {
    info["client_unique_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
