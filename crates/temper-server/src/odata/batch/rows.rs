//! A `$batch` request's rows, read and checked before any of them runs.
//!
//! The body is OData 4.01's JSON batch form: `{"requests": [{"id", "method",
//! "url", "headers", "body"}]}`. Each row is a write to `/tdata`, answered on
//! its own: the batch runs nothing when any row is malformed, and nothing a
//! row does depends on another row's answer.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::Query;
use axum::http::header::HeaderName;
use axum::http::{HeaderMap, HeaderValue, Uri};
use percent_encoding::percent_decode_str;
use serde_json::Value;
use temper_odata::path::{ODataPath, parse_path};

use super::super::common::extract_key;
use super::super::write_key::IDEMPOTENCY_KEY;
use crate::authz::edge::is_caller_authority_header;

/// One row of a batch, ready for the `/tdata` POST handler.
#[derive(Debug, Clone)]
pub(crate) struct BatchRow {
    /// The caller's id for the row, named again on its answer.
    pub(crate) id: String,
    /// The row's path under `/tdata`, percent-decoded, without a leading `/`.
    pub(crate) path: String,
    /// The row's query options, such as `await_integration`.
    pub(crate) query: BTreeMap<String, String>,
    /// The row's own headers: its idempotency key, and no caller authority.
    pub(crate) headers: HeaderMap,
    /// The row's idempotency key, as its headers carry it.
    pub(crate) key: String,
    /// The row's body, as the handler reads a request's body.
    pub(crate) body: bytes::Bytes,
}

/// Read every row of `body`, or say why the batch runs none of them.
///
/// # Errors
///
/// A message for the caller when the body is not a JSON batch, when a row is
/// not a POST under `/tdata`, has no idempotency key or asks for a grouping
/// the batch does not take, or when two rows share an id or name one record.
pub(crate) fn read_rows(body: &[u8]) -> Result<Vec<BatchRow>, String> {
    let batch: Value =
        serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?;
    let requests = batch
        .get("requests")
        .and_then(Value::as_array)
        .ok_or("the body has no `requests` array")?;
    let mut rows = Vec::with_capacity(requests.len());
    let mut ids = BTreeSet::new();
    let mut records: BTreeMap<(String, String), String> = BTreeMap::new();
    for (index, request) in requests.iter().enumerate() {
        let row = read_row(index, request)?;
        if !ids.insert(row.id.clone()) {
            return Err(format!("two rows have the id '{}'", row.id));
        }
        // Rows run at once, so two moves on one record would have no order
        // between them: each record is named once.
        if let Some(record) = record_of(&row.path)
            && let Some(first) = records.insert(record.clone(), row.id.clone())
        {
            return Err(format!(
                "rows '{first}' and '{}' both name {}('{}'); send each record once",
                row.id, record.0, record.1
            ));
        }
        rows.push(row);
    }
    debug_assert_eq!(rows.len(), requests.len());
    Ok(rows)
}

/// The `index`th row of the batch.
fn read_row(index: usize, request: &Value) -> Result<BatchRow, String> {
    let request = request
        .as_object()
        .ok_or_else(|| format!("row {index} is not an object"))?;
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("row {index} has no `id`"))?
        .to_string();
    for member in ["atomicityGroup", "dependsOn", "if"] {
        if request.contains_key(member) {
            return Err(format!(
                "row '{id}' has `{member}`: each row is answered on its own"
            ));
        }
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    if !method.eq_ignore_ascii_case("POST") {
        return Err(format!(
            "row '{id}' is a {method:?}: a batch carries POST rows (creates and actions)"
        ));
    }
    let url = request
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("row '{id}' has no `url`"))?;
    let (path, query) = read_url(&id, url)?;
    let (headers, key) = read_headers(&id, request.get("headers"))?;
    let body = match request.get("body") {
        None | Some(Value::Null) => bytes::Bytes::new(),
        Some(body) => bytes::Bytes::from(body.to_string()),
    };
    Ok(BatchRow {
        id,
        path,
        query,
        headers,
        key,
        body,
    })
}

/// A row's url as the handler's path and query. It is relative to `/tdata`,
/// or a path under it.
fn read_url(id: &str, url: &str) -> Result<(String, BTreeMap<String, String>), String> {
    let (raw_path, raw_query) = url.split_once('?').unwrap_or((url, ""));
    let relative = raw_path
        .strip_prefix("/tdata/")
        .or_else(|| (!raw_path.starts_with('/')).then_some(raw_path))
        .filter(|path| !path.is_empty() && !path.contains("://"))
        .ok_or_else(|| format!("row '{id}' has url {url:?}: give a path under /tdata"))?;
    let path = percent_decode_str(relative)
        .decode_utf8()
        .map_err(|_| format!("row '{id}' has a url that is not UTF-8"))?
        .into_owned();
    if path.starts_with('$') {
        return Err(format!(
            "row '{id}' names {path:?}: a row is a create or an action"
        ));
    }
    let uri: Uri = format!("/?{raw_query}")
        .parse()
        .map_err(|_| format!("row '{id}' has a query that is not a URL's"))?;
    let Query(query) = Query::<BTreeMap<String, String>>::try_from_uri(&uri)
        .map_err(|e| format!("row '{id}' has a query that cannot be read: {e}"))?;
    Ok((path, query))
}

/// A row's headers. A caller-authority header is dropped, as the server's edge
/// drops it from every request: the batch's caller is each row's caller.
fn read_headers(id: &str, headers: Option<&Value>) -> Result<(HeaderMap, String), String> {
    let mut map = HeaderMap::new();
    let entries = match headers {
        None | Some(Value::Null) => None,
        Some(Value::Object(entries)) => Some(entries),
        Some(_) => return Err(format!("row '{id}' has `headers` that are not an object")),
    };
    for (name, value) in entries.into_iter().flatten() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("row '{id}' has a header named {name:?}"))?;
        let value = value
            .as_str()
            .and_then(|value| HeaderValue::from_str(value).ok())
            .ok_or_else(|| format!("row '{id}' has a header {name} that is not text"))?;
        if !is_caller_authority_header(&name) {
            map.insert(name, value);
        }
    }
    // A batch answered "not confirmed" is sent again row by row under the
    // same keys, so each row must carry the key its caller holds.
    let key = map
        .get(&IDEMPOTENCY_KEY)
        .and_then(|key| key.to_str().ok())
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "row '{id}' has no Idempotency-Key header: each row needs the key it is sent again with"
            )
        })?;
    Ok((map, key))
}

/// The record a row names in its path: its entity set and key.
fn record_of(path: &str) -> Option<(String, String)> {
    match parse_path(&format!("/{path}")).ok()? {
        ODataPath::Entity(set, key) => Some((set, extract_key(&key))),
        ODataPath::BoundAction { parent, .. } => match *parent {
            ODataPath::Entity(set, key) => Some((set, extract_key(&key))),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
#[path = "rows_test.rs"]
mod tests;
