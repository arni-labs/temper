//! Tenant access validation for authenticated requests.

use axum::extract::{FromRequestParts, MatchedPath, RawPathParams, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use crate::state::PlatformState;

/// Validate that the credential-bound tenant is the tenant addressed by the
/// request, then apply routed-storage membership checks for GitHub users.
pub async fn tenant_access_check(
    State(state): State<PlatformState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(authenticated) = req
        .extensions()
        .get::<AuthenticatedRequestContext>()
        .cloned()
    else {
        // The outer bearer edge permits only exact public routes without a
        // context. It remains authoritative for that classification.
        return Ok(next.run(req).await);
    };

    let credential_tenant = authenticated.tenant().as_str();
    if let Some(path_tenant) = tenant_from_request(&mut req).await?
        && path_tenant != credential_tenant
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Membership rows may already have been removed by the authorized deletion.
    // This middleware-only capability is scoped to that same generation's self-delete.
    if req
        .extensions()
        .get::<crate::deletion_retries::AuthorizedDeletionRetry>()
        .is_some()
    {
        return Ok(next.run(req).await);
    }

    let principal = &authenticated.security_context().principal;
    if principal.kind == PrincipalKind::Agent || !principal.id.starts_with("github:") {
        return Ok(next.run(req).await);
    }

    let Some(provider) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
    else {
        return Ok(next.run(req).await);
    };
    if !provider.supports_tenant_admin() {
        return Ok(next.run(req).await);
    }

    match provider.tenants_for_user(&principal.id).await {
        Ok(user_tenants)
            if user_tenants
                .iter()
                .any(|tenant| tenant.tenant_id == credential_tenant) =>
        {
            Ok(next.run(req).await)
        }
        Ok(_) => Err(StatusCode::FORBIDDEN),
        Err(error) => {
            tracing::error!(
                principal = %principal.id,
                tenant = credential_tenant,
                error = %error,
                "failed to check tenant access"
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// The tenant named by a matched tenant API route, using Axum's decoded path parameters.
pub(crate) async fn tenant_from_request(req: &mut Request) -> Result<Option<String>, StatusCode> {
    let Some(matched) = req.extensions().get::<MatchedPath>() else {
        return Ok(None);
    };
    if !matched.as_str().starts_with("/api/tenants/") {
        return Ok(None);
    }
    let (mut parts, body) =
        std::mem::replace(req, Request::new(axum::body::Body::empty())).into_parts();
    let params = RawPathParams::from_request_parts(&mut parts, &()).await;
    *req = Request::from_parts(parts, body);
    let params = params.map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(params
        .iter()
        .find(|(name, _)| *name == "tenant")
        .or_else(|| params.iter().find(|(name, _)| *name == "id"))
        .map(|(_, id)| id.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, middleware, routing::get};
    use tower::ServiceExt;

    #[tokio::test]
    async fn decoded_tenant_guards_both_platform_and_kernel_routes() {
        let app = Router::new()
            .route("/api/tenants/{id}/users", get(|| async { "ok" }))
            .route(
                "/api/tenants/{tenant}/decisions/{id}",
                get(|| async { "ok" }),
            )
            .route("/tdata", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                PlatformState::new(None),
                tenant_access_check,
            ));
        for (path, status) in [
            ("/api/tenants/%61cme/users", StatusCode::OK),
            ("/api/tenants/%62eta/users", StatusCode::FORBIDDEN),
            ("/api/tenants/%61cme/decisions/other-id", StatusCode::OK),
            ("/api/tenants/%62eta/decisions/acme", StatusCode::FORBIDDEN),
            ("/tdata", StatusCode::OK),
        ] {
            let mut req = Request::get(path).body(Body::empty()).unwrap();
            req.extensions_mut()
                .insert(AuthenticatedRequestContext::new(
                    temper_runtime::tenant::TenantId::new("acme"),
                    temper_authz::SecurityContext::from_resolved_identity(
                        "agent", "operator", None,
                    ),
                ));
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                status,
                "{path}"
            );
        }
    }
}
