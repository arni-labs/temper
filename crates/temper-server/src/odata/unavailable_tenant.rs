//! Answer for a tenant whose stored specs did not load.
//!
//! Startup leaves such a tenant out of the registry so the others are served.
//! Its own requests get a 503 naming that state, not the 404 an unknown
//! entity set would give, so the tenant and its operator can see it. The
//! reason itself is in the server log: it can quote the tenant's stored rows.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;

use super::common::extract_tenant;
use crate::response::odata_error;
use crate::state::ServerState;

/// Refuse `/tdata` requests for a tenant marked unavailable at startup.
///
/// The tenant is the one each handler would act for: the credential's tenant
/// when the request is authenticated, otherwise the `X-Tenant-Id` header.
pub(crate) async fn refuse_unavailable_tenant(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let tenant = match request.extensions().get::<AuthenticatedRequestContext>() {
        Some(context) => Some(context.tenant().clone()),
        None => extract_tenant(request.headers(), &state).ok(),
    };
    let unavailable = tenant.filter(|tenant| {
        state
            .registry
            .read()
            .unwrap() // ci-ok: RwLock read — poisoned lock = prior panic, fail-fast correct
            .tenant_unavailable_reason(tenant)
            .is_some()
    });
    if let Some(tenant) = unavailable {
        return odata_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "TenantUnavailable",
            &format!(
                "Tenant '{tenant}' is unavailable: its stored specs did not load at startup. \
                 The server log names the row; fix it and restart."
            ),
        )
        .into_response();
    }
    next.run(request).await
}
