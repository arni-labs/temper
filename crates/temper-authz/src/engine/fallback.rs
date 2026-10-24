//! Decisions against the fallback global policy set.

use std::collections::HashMap;

use super::{AuthzDecision, AuthzEngine};
use crate::context::SecurityContext;
use crate::error::AuthzDenial;
use crate::metrics::CedarCheck;

impl AuthzEngine {
    /// Evaluate an authorization request against the fallback global policy set.
    ///
    /// **Prefer [`authorize_for_tenant`](Self::authorize_for_tenant)** for
    /// per-tenant isolation. This method exists for backward compatibility.
    pub fn authorize(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.decide_with_fallback(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            CedarCheck::Gate,
        )
    }

    /// Ask the fallback policy set what it would decide for a request, as a
    /// question about the policies rather than a check of a real request:
    /// the same decision as [`Self::authorize`], counted as `check:review`.
    pub fn review(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.decide_with_fallback(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            CedarCheck::Review,
        )
    }

    fn decide_with_fallback(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
        check: CedarCheck,
    ) -> AuthzDecision {
        let policy_set = match self.fallback_policy_set.read() {
            Ok(ps) => ps,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "policy lock poisoned: {e}"
                )));
            }
        };
        self.evaluate_request(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            &policy_set,
            check,
        )
    }
}
