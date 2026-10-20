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

    /// The number of the last change of this file the remote has applied; 0
    /// when it has none.
    pub(super) async fn last_change(&self) -> Result<i64, DriverError> {
        let body = serde_json::json!({
            "requests": [{
                "type": "execute",
                "stmt": {
                    "sql": "SELECT change_id FROM turso_sync_last_change_id WHERE client_id = ?",
                    "args": [{ "type": "text", "value": self.client_id }],
                },
            }],
        });
        let failed = |what: String| {
            DriverError::SyncSetup(format!("read the remote copy's progress: {what}"))
        };
        let answer: serde_json::Value = self
            .http
            .post(&self.pipeline_url)
            .bearer_auth(&self.auth_token)
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| failed(error.to_string()))?
            .json()
            .await
            .map_err(|error| failed(error.to_string()))?;
        let result = &answer["results"][0];
        if result["type"] == "error" {
            let message = result["error"]["message"].as_str().unwrap_or_default();
            // Nothing has been pushed to this remote yet.
            if message.contains("no such table") {
                return Ok(0);
            }
            return Err(failed(message.to_string()));
        }
        let value = &result["response"]["result"]["rows"][0][0];
        // Integers arrive as strings in the Hrana protocol.
        Ok(match &value["value"] {
            serde_json::Value::String(text) => text.parse().unwrap_or(0),
            serde_json::Value::Number(number) => number.as_i64().unwrap_or(0),
            _ => 0,
        })
    }
}

/// The client id the engine gave this file, from its sync metadata.
pub(super) fn client_id(info: &serde_json::Value) -> String {
    info["client_unique_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
