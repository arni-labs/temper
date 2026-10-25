//! Secret management API endpoints.
//!
//! Handles encrypted secret storage, retrieval, and deletion for tenants.
//! Secrets are encrypted at rest using the configured vault key.

use std::collections::BTreeMap;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::persistence::SecretPrecondition;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::authz::{
    ResourceAuthorization, require_authenticated_context, require_resource_authorization,
    require_tenant_match,
};
use crate::state::{SecretWriteOutcome, ServerState};

const SECRET_COLLECTION_RESOURCE_ID: &str = "__keys__";

pub(super) struct SecretAuthed(AuthenticatedRequestContext);

impl SecretAuthed {
    fn tenant(&self) -> &TenantId {
        self.0.tenant()
    }
}

/// Require Cedar to permit `action` on `Secret::"<resource_id>"` in the
/// caller's tenant.
fn authorize_secret(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    action: &str,
    resource_id: &str,
) -> Result<(), StatusCode> {
    require_resource_authorization(
        state,
        authenticated,
        ResourceAuthorization {
            action,
            resource_type: "Secret",
            resource_id,
            resource_attrs: BTreeMap::from([
                (
                    "tenant".to_string(),
                    serde_json::Value::String(authenticated.tenant().as_str().to_string()),
                ),
                (
                    "key_name".to_string(),
                    serde_json::Value::String(resource_id.to_string()),
                ),
            ]),
        },
    )
}

impl FromRequestParts<ServerState> for SecretAuthed {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let Path(params) = Path::<BTreeMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let Some(tenant) = params.get("tenant") else {
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };
        let authenticated =
            require_authenticated_context(parts.extensions.get::<AuthenticatedRequestContext>())
                .map_err(IntoResponse::into_response)?;
        require_tenant_match(authenticated, tenant).map_err(IntoResponse::into_response)?;
        let resource_id = params
            .get("key_name")
            .map(String::as_str)
            .unwrap_or(SECRET_COLLECTION_RESOURCE_ID);
        authorize_secret(state, authenticated, "manage_secrets", resource_id)
            .map_err(IntoResponse::into_response)?;
        Ok(Self(authenticated.clone()))
    }
}

/// Check if an error message indicates that the backend is not supported.
fn is_backend_not_supported_error(err: &str) -> bool {
    err.to_ascii_lowercase().contains("not supported")
}

/// The condition a PUT's `If-None-Match` or `If-Match` header puts on the write.
///
/// The agreed API contract in PR #114 restricts conditional PUTs to
/// `If-None-Match: *` and one strong `If-Match` etag:
/// https://github.com/arni-labs/temper/pull/114
/// That contract explicitly chooses 400 for `If-Match: *`, etag lists, weak
/// etags, non-wildcard `If-None-Match`, and both headers together. These checks
/// preserve that settled restriction; supporting existence or version-set
/// predicates requires an owner decision to expand the contract. Never ignore
/// a rejected condition, which would turn the request into an unconditional
/// overwrite.
fn write_precondition(headers: &HeaderMap) -> Result<SecretPrecondition<'_>, &'static str> {
    let if_none_match = single(headers, header::IF_NONE_MATCH)
        .map_err(|()| "If-None-Match must be one header holding *")?;
    let if_match = single(headers, header::IF_MATCH)
        .map_err(|()| "If-Match must be one header holding one etag")?;
    match (if_none_match, if_match) {
        (None, None) => Ok(SecretPrecondition::Any),
        (Some(_), Some(_)) => Err("Send If-None-Match or If-Match, not both"),
        (Some("*"), None) => Ok(SecretPrecondition::Absent),
        (Some(_), None) => Err("If-None-Match supports only *"),
        (None, Some(tag)) => tag
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|id| !id.is_empty() && !id.contains('"'))
            .map(SecretPrecondition::Version)
            .ok_or("If-Match takes one strong etag, as a PUT returned it in ETag"),
    }
}

/// The one value of header `name`, trimmed: `Ok(None)` when absent, `Err`
/// when repeated or not visible ASCII.
fn single(headers: &HeaderMap, name: header::HeaderName) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => value.to_str().map(|v| Some(v.trim())).map_err(|_| ()),
        (Some(_), Some(_)) => Err(()),
    }
}

/// `If-Value-Sha256: <64 hex digits>`: replace the key only while SHA-256 of
/// its stored value is this. For a caller that knows the value it was given
/// but not the key's etag. It is a header of its own, not an `If-Match` form,
/// because etags are listed and must never come from the value.
const IF_VALUE_SHA256: header::HeaderName = header::HeaderName::from_static("if-value-sha256");

/// What a PUT asks of the key before it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutCondition<'a> {
    /// A condition the store checks in its write statement.
    Store(SecretPrecondition<'a>),
    /// `If-Value-Sha256`: the stored value's SHA-256 must be this.
    ValueSha256([u8; 32]),
}

impl PutCondition<'_> {
    fn is_conditional(&self) -> bool {
        *self != Self::Store(SecretPrecondition::Any)
    }
}

/// Read a PUT's condition headers. `If-Value-Sha256` stands alone: combined
/// with `If-Match` or `If-None-Match` it is refused, since the two conditions
/// could disagree and neither may be silently dropped.
fn put_condition(headers: &HeaderMap) -> Result<PutCondition<'_>, &'static str> {
    let value_sha256 = single(headers, IF_VALUE_SHA256)
        .map_err(|()| "If-Value-Sha256 must be one header holding 64 hex digits")?;
    let store = write_precondition(headers)?;
    match (value_sha256, store) {
        (None, store) => Ok(PutCondition::Store(store)),
        (Some(_), SecretPrecondition::Any) => value_sha256
            .and_then(parse_sha256_hex)
            .map(PutCondition::ValueSha256)
            .ok_or("If-Value-Sha256 takes 64 hex digits"),
        (Some(_), _) => Err("Send If-Value-Sha256 alone, without If-Match or If-None-Match"),
    }
}

/// 64 hex digits, either case, as 32 bytes.
fn parse_sha256_hex(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let pair = std::str::from_utf8(pair).ok()?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// The `ETag` header value for a secret's version id.
fn etag(version_id: &str) -> String {
    format!("\"{version_id}\"")
}

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
///
/// Answers 204 with the new `ETag`. With `If-None-Match: *` the write happens
/// only if the key does not exist, and with `If-Match: "<etag>"` only if the
/// key's current etag is that one; with `If-Value-Sha256: <hex>` only if the
/// key exists and SHA-256 of its stored value is that one. Otherwise 412,
/// which carries no etag.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "PUT /api/tenants/{tenant}/secrets/{key_name}"))]
pub(super) async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((_tenant, key_name)): Path<(String, String)>,
    auth: SecretAuthed,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let condition = match put_condition(&headers) {
        Ok(condition) => condition,
        Err(message) => {
            tracing::warn!(reason = message, "unsupported secret write condition");
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };
    // A value condition also needs permission to read the secret. Without
    // it, the difference between 204 and 412 would let a caller who may write
    // but not read test guesses against the stored value.
    if matches!(condition, PutCondition::ValueSha256(_))
        && let Err(denied) = authorize_secret(&state, &auth.0, "access_secret", &key_name)
    {
        return denied.into_response();
    }
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured (missing TEMPER_VAULT_KEY)",
        )
            .into_response();
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in put secret request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let value = match body_json.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            tracing::warn!("missing 'value' field in put secret request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing \'value\' field in request body",
            )
                .into_response();
        }
    };

    if value.len() > crate::secrets::vault::MAX_SECRET_VALUE_BYTES {
        tracing::warn!(size = value.len(), "secret value exceeds maximum size");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Secret value exceeds maximum size of {} bytes",
                crate::secrets::vault::MAX_SECRET_VALUE_BYTES
            ),
        )
            .into_response();
    }

    // Encrypt the value.
    let (ciphertext, nonce) = match vault.encrypt(value.as_bytes()) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "encryption failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encryption failed: {e}"),
            )
                .into_response();
        }
    };

    // Persist the encrypted secret; the store checks the condition in the
    // same statement that writes.
    let outcome = match condition {
        PutCondition::Store(precondition) => {
            state
                .write_secret(tenant, &key_name, &ciphertext, &nonce, precondition)
                .await
        }
        PutCondition::ValueSha256(expected) => {
            state
                .replace_secret_if_value(tenant, &key_name, &expected, &ciphertext, &nonce)
                .await
        }
    };
    let version_id = match outcome {
        Ok(SecretWriteOutcome::Written { version_id }) => version_id,
        Ok(SecretWriteOutcome::PreconditionFailed) => {
            tracing::info!("secret write precondition failed");
            return (
                StatusCode::PRECONDITION_FAILED,
                "The secret does not meet the request's condition",
            )
                .into_response();
        }
        Err(e) => {
            let status = if condition.is_conditional() && is_backend_not_supported_error(&e) {
                StatusCode::NOT_IMPLEMENTED
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::error!(error = %e, "secret persistence failed");
            return (status, format!("Persistence failed: {e}")).into_response();
        }
    };

    // Cache in memory after successful persistence.
    vault.cache_secret(tenant, &key_name, value.to_string());

    let mut response = StatusCode::NO_CONTENT.into_response();
    match HeaderValue::from_str(&etag(&version_id)) {
        Ok(value) => {
            response.headers_mut().insert(header::ETAG, value);
        }
        // A version id is 32 hex digits, so this cannot happen; if it did, the
        // write has still happened and the caller can read the etag back.
        Err(e) => tracing::error!(error = %e, "secret version id is not a header value"),
    }
    response
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "DELETE /api/tenants/{tenant}/secrets/{key_name}"))]
pub(super) async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((_tenant, key_name)): Path<(String, String)>,
    auth: SecretAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    match state.delete_secret(tenant, &key_name).await {
        Ok(true) => {
            vault.remove_secret(tenant, &key_name);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            tracing::warn!("secret not found for deletion");
            vault.remove_secret(tenant, &key_name);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            let status = if is_backend_not_supported_error(&e) {
                StatusCode::NOT_IMPLEMENTED
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::error!(error = %e, "secret deletion failed");
            (status, format!("Delete failed: {e}")).into_response()
        }
    }
}

/// GET /api/tenants/{tenant}/secrets — list secret key names (never values).
///
/// `keys` names every secret the tenant can read, platform secrets included.
/// `etags` maps each stored tenant secret to its current `ETag`, for a later
/// `If-Match`.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/secrets"))]
pub(super) async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: SecretAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str();
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "Secrets vault not configured"})),
        )
            .into_response();
    };

    let keys = vault.list_keys(tenant);
    let etags: BTreeMap<String, String> = match state.secret_versions(tenant).await {
        Ok(versions) => versions
            .into_iter()
            .map(|(key_name, version_id)| (key_name, etag(&version_id)))
            .collect(),
        Err(e) => {
            tracing::error!(error = %e, "secret version listing failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("Listing failed: {e}")})),
            )
                .into_response();
        }
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"keys": keys, "etags": etags})),
    )
        .into_response()
}
