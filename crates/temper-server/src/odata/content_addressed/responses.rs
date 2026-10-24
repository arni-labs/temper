//! Error and metadata-only response helpers for raw Blob ingest.

use axum::http::StatusCode;
use axum::response::IntoResponse as _;

use super::super::create_answer::{CreateTarget, EXISTING_ENTITY_STATUS, taken_key_answer};
use super::super::response::annotate_entity;
use crate::blob_store::BlobStageError;
use crate::entity_actor::EntityResponse;
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

pub(in crate::odata) fn stage_error_response(error: BlobStageError) -> axum::response::Response {
    let (status, code) = match &error {
        BlobStageError::BodyStream(_) => (StatusCode::BAD_REQUEST, "BodyStreamError"),
        BlobStageError::BodyExceedsDeclaredLength { .. } => {
            (StatusCode::BAD_REQUEST, "BodyExceedsContentLength")
        }
        BlobStageError::BodyShorterThanDeclaredLength { .. } => {
            (StatusCode::BAD_REQUEST, "BodyShorterThanContentLength")
        }
        BlobStageError::IdleTimeout { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestIdleTimeout")
        }
        BlobStageError::TotalDeadline { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestDeadlineExceeded")
        }
        BlobStageError::ThroughputTooLow { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestThroughputTooLow")
        }
        BlobStageError::StagingBudgetExhausted { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "BlobIngestStagingBudgetExhausted",
        ),
        BlobStageError::Storage(_) => (StatusCode::INSUFFICIENT_STORAGE, "BlobStagingFailed"),
    };
    let message = if matches!(&error, BlobStageError::Storage(_)) {
        tracing::error!(%error, "raw Blob staging failed");
        "Blob staging failed".to_string()
    } else {
        error.to_string()
    };
    odata_error(status, code, &message).into_response()
}

pub(in crate::odata) fn blob_store_error_response(error: &str) -> axum::response::Response {
    tracing::error!(%error, "raw Blob object-store write failed");
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "BlobStoreWriteFailed",
        "Blob object-store write failed",
    )
    .into_response()
}

fn remove_binary_fields_from_create_response(state: &mut serde_json::Value) {
    if let Some(fields) = state
        .get_mut("fields")
        .and_then(serde_json::Value::as_object_mut)
    {
        fields.remove("Content");
        fields.remove("CanonicalBytes");
    }
}

/// [`taken_key_answer`] as a raw Blob create's response.
pub(super) async fn taken_blob_answer(
    state: &ServerState,
    target: &CreateTarget<'_>,
) -> axum::response::Response {
    match taken_key_answer(state, target).await {
        Ok(existing) => blob_create_response(target, EXISTING_ENTITY_STATUS, &existing),
        Err(response) => response,
    }
}

/// The answer to a raw Blob create: the row without its binary fields.
pub(super) fn blob_create_response(
    target: &CreateTarget<'_>,
    status: StatusCode,
    response: &EntityResponse,
) -> axum::response::Response {
    let set_name = target.set_name;
    let object_id = target.entity_id;
    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
    remove_binary_fields_from_create_response(&mut state_json);
    let body = annotate_entity(
        state_json,
        format!("$metadata#{set_name}/$entity"),
        Some(format!("{set_name}('{object_id}')")),
    );
    ODataResponse { status, body }.into_response()
}
