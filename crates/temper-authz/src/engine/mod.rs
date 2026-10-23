//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityUid, PolicyId, PolicySet, Request,
    Response as CedarResponse,
};

use crate::context::{PrincipalKind, SecurityContext, is_cedar_authority_context_key};
use crate::error::{AuthzDenial, AuthzError};
use crate::groups::GroupMemberships;
use crate::metrics::{CedarCheck, CedarDecisionMetric, CedarPhaseOutcome};

mod candidates;
mod group_candidates;
mod list_uniformity;
mod statement_cache;
mod tenant_reload;

use tenant_reload::merge_system_platform_policy;

pub use list_uniformity::ListReadUniformity;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stack_tests;

#[cfg(test)]
mod groups_tests;

#[cfg(test)]
mod statement_cache_tests;

/// The result of an authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// The request is allowed, with the policy IDs that contributed to the permit.
    Allow { policy_ids: Vec<String> },
    /// The request is denied with typed denial details.
    Deny(AuthzDenial),
}

impl AuthzDecision {
    /// Returns `true` if the authorization decision is `Allow`.
    pub fn is_allowed(&self) -> bool {
        matches!(self, AuthzDecision::Allow { .. })
    }

    /// Returns the denial details if the decision is `Deny`.
    pub fn denial(&self) -> Option<&AuthzDenial> {
        match self {
            AuthzDecision::Allow { .. } => None,
            AuthzDecision::Deny(d) => Some(d),
        }
    }

    /// Returns the policy IDs that contributed to the allow decision.
    pub fn policy_ids(&self) -> &[String] {
        match self {
            AuthzDecision::Allow { policy_ids } => policy_ids,
            AuthzDecision::Deny(_) => &[],
        }
    }
}

/// Compiled policy data used by Cedar and the request-time candidate selector.
struct CompiledPolicies {
    policy_set: PolicySet,
    candidate_index: candidates::CandidatePolicyIndex,
    /// The groups these statements can ask a principal about (ADR-0183).
    groups: group_candidates::GroupCandidates,
}

impl CompiledPolicies {
    fn new(policy_set: PolicySet) -> Self {
        let statements: Vec<group_candidates::StatementGroups> = policy_set
            .policies()
            .map(group_candidates::StatementGroups::of)
            .collect();
        Self::with_groups(policy_set, &statements)
    }

    /// Compile a set whose statements' groups are already known, so nothing is
    /// parsed or walked again (ADR-0183).
    fn with_groups(
        policy_set: PolicySet,
        statements: &[group_candidates::StatementGroups],
    ) -> Self {
        let candidate_index = candidates::CandidatePolicyIndex::new(&policy_set);
        let groups = group_candidates::GroupCandidates::from_statements(statements);
        Self {
            policy_set,
            candidate_index,
            groups,
        }
    }
}

/// Per-tenant policy data: the compiled policies, the source text, and its
/// statements parsed one by one for the next reload.
struct TenantPolicies {
    policies: CompiledPolicies,
    source_text: String,
    statements: Arc<statement_cache::StatementCache>,
}

/// The authorization engine. Holds per-tenant compiled Cedar policies and
/// evaluates authorization requests. Supports hot-reload of policies via
/// [`reload_tenant_policies`](AuthzEngine::reload_tenant_policies).
///
/// Uses `BTreeMap` for deterministic iteration order (DST compliance).
pub struct AuthzEngine {
    /// Per-tenant policy sets. Each tenant has its own isolated PolicySet.
    tenant_policies: RwLock<BTreeMap<String, TenantPolicies>>,
    /// Fallback global policy set for callers that don't specify a tenant.
    /// Deprecated: callers should migrate to `authorize_for_tenant`.
    fallback_policy_set: RwLock<CompiledPolicies>,
    /// Immutable platform policy used when a tenant has no app policy set.
    /// Only System principals are evaluated against this set; all externally
    /// resolvable principal kinds still fail closed for an unloaded tenant.
    platform_policy_set: CompiledPolicies,
    authorizer: Authorizer,
    /// Every tenant's group memberships, kept by the server from the
    /// `GroupMembership` records (ADR-0183).
    group_memberships: Arc<GroupMemberships>,
    /// Cedar statements parsed by tenant reloads since start: a reload parses
    /// only statements it has not seen, so this grows by what changed.
    statements_parsed: AtomicU64,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text (loaded into the
    /// fallback global policy set). ADR-0046: the built-in `system-platform`
    /// policy is merged in so System principals are authorized by an
    /// explicit, auditable policy rather than a hard-coded bypass.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        // Parse the user policy first — return an error if it's malformed.
        let mut policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut policy_set);

        Ok(Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            platform_policy_set: compiled_system_platform_policies(),
            authorizer: Authorizer::new(),
            group_memberships: Arc::new(GroupMemberships::new()),
            statements_parsed: AtomicU64::new(0),
        })
    }

    /// Create an AuthzEngine with no user policies, but with the built-in
    /// `system-platform` policy installed (ADR-0046). System principals
    /// remain authorized; everything else hits Cedar's default-deny.
    ///
    /// Use this to test deny behavior for non-System principals. For test
    /// setups that need all requests to be allowed, use
    /// [`permissive`](Self::permissive) instead.
    pub fn empty() -> Self {
        let mut policy_set = PolicySet::new();
        merge_system_platform_policy(&mut policy_set);
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            platform_policy_set: compiled_system_platform_policies(),
            authorizer: Authorizer::new(),
            group_memberships: Arc::new(GroupMemberships::new()),
            statements_parsed: AtomicU64::new(0),
        }
    }

    /// Create an AuthzEngine that permits all requests.
    ///
    /// Loads a single catch-all `permit(principal, action, resource);` policy
    /// so that Cedar evaluates to Allow for every principal kind (System or
    /// otherwise). Used in tests and permissive dev environments.
    pub fn permissive() -> Self {
        let mut policy_set =
            PolicySet::from_str("permit(principal, action, resource);").unwrap_or_default();
        // Even a permit-all fallback (e.g. the ARN-230 fail-open path) must keep
        // the system-platform forbids — the god-mode identity entities
        // (TrustedIssuer / PrincipalGeneration) stay System/Admin-only, so a
        // fail-open tenant can never become an authz-takeover (ARN-255).
        merge_system_platform_policy(&mut policy_set);
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            platform_policy_set: compiled_system_platform_policies(),
            authorizer: Authorizer::new(),
            group_memberships: Arc::new(GroupMemberships::new()),
            statements_parsed: AtomicU64::new(0),
        }
    }

    /// The memberships this engine reads for `principal in` (ADR-0183). The
    /// server applies every `GroupMembership` change to it and fills each
    /// tenant before its first check.
    pub fn group_memberships(&self) -> &Arc<GroupMemberships> {
        &self.group_memberships
    }

    /// Parse and validate policy text without changing the active policy set.
    pub fn validate_tenant_policies(&self, policy_text: &str) -> Result<(), AuthzError> {
        policy_text
            .parse::<PolicySet>()
            .map(|_| ())
            .map_err(|error| AuthzError::PolicyParse(error.to_string()))
    }

    /// Remove a tenant's policy set entirely.
    pub fn remove_tenant(&self, tenant: &str) {
        if let Ok(mut tenants) = self.tenant_policies.write() {
            tenants.remove(tenant);
        }
    }

    /// Get the combined Cedar policy text for a tenant.
    pub fn get_tenant_policy_text(&self, tenant: &str) -> Option<String> {
        self.tenant_policies
            .read()
            .ok()
            .and_then(|t| t.get(tenant).map(|tp| tp.source_text.clone()))
    }

    /// Hot-reload Cedar policies into the fallback global policy set.
    ///
    /// **Deprecated**: Use [`reload_tenant_policies`](Self::reload_tenant_policies)
    /// for per-tenant isolation. This method exists for backward compatibility
    /// during migration.
    pub fn reload_policies(&self, policy_text: &str) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);

        let mut current = self
            .fallback_policy_set
            .write()
            .map_err(|e| AuthzError::Engine(format!("policy lock poisoned: {e}")))?;
        *current = CompiledPolicies::new(new_policy_set);
        Ok(())
    }

    /// Returns the total number of policies across all tenants + fallback.
    pub fn policy_count(&self) -> usize {
        let tenant_count: usize = self
            .tenant_policies
            .read()
            .map(|t| {
                t.values()
                    .map(|tp| count_user_policies(&tp.policies.policy_set))
                    .sum()
            })
            .unwrap_or(0);
        let fallback_count = self
            .fallback_policy_set
            .read()
            .map_or(0, |ps| count_user_policies(&ps.policy_set));
        tenant_count + fallback_count
    }

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
        let policy_set = match self.fallback_policy_set.read() {
            Ok(ps) => ps,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "policy lock poisoned: {e}"
                )));
            }
        };
        self.evaluate_request(
            None,
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            &policy_set,
            CedarCheck::Gate,
        )
    }

    /// Evaluate an authorization request against a specific tenant's policy set.
    ///
    /// If the tenant has no app policy set loaded, externally resolvable
    /// principals fail closed. Kernel System authority is evaluated only
    /// against the immutable built-in platform policy, never the process-wide
    /// compatibility policy. This preserves internal bootstrap/recovery while
    /// preventing a missing tenant policy from inheriting another scope
    /// (ARN-230).
    pub fn authorize_for_tenant(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize_for_tenant_checked(
            tenant,
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            CedarCheck::Gate,
        )
    }

    /// The same decision as [`Self::authorize_for_tenant`], for the per-row check a list read makes to leave out the
    /// rows the caller may not see. Only its metric differs: it is counted as `check:row_filter`, never as a refusal.
    pub fn authorize_row_for_tenant(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize_for_tenant_checked(
            tenant,
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            CedarCheck::RowFilter,
        )
    }

    fn authorize_for_tenant_checked(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
        check: CedarCheck,
    ) -> AuthzDecision {
        let tenants = match self.tenant_policies.read() {
            Ok(t) => t,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "tenant policy lock poisoned: {e}"
                )));
            }
        };

        if let Some(tp) = tenants.get(tenant) {
            return self.evaluate_request(
                Some(tenant),
                security_ctx,
                action,
                resource_type,
                resource_attrs,
                &tp.policies,
                check,
            );
        }

        if Self::is_system(security_ctx) {
            return self.evaluate_request(
                Some(tenant),
                security_ctx,
                action,
                resource_type,
                resource_attrs,
                &self.platform_policy_set,
                check,
            );
        }

        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit)
    }

    /// Core Cedar evaluation logic shared by both `authorize` and
    /// `authorize_for_tenant`.
    fn evaluate_request(
        &self,
        tenant: Option<&str>,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
        policies: &CompiledPolicies,
        check: CedarCheck,
    ) -> AuthzDecision {
        let mut recorder = CedarEvaluationRecorder::start(check);

        // Build Cedar principal
        let principal_type = match security_ctx.principal.kind {
            PrincipalKind::Customer => "Customer",
            PrincipalKind::Agent => "Agent",
            PrincipalKind::Admin => "Admin",
            PrincipalKind::System => "System",
        };

        let principal_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            principal_type, security_ctx.principal.id
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail(
                    "principal_uid",
                    AuthzDenial::InvalidPrincipal(e.to_string()),
                );
            }
        };
        recorder.finish_phase("principal_uid");

        // Build Cedar action
        let action_uid = match EntityUid::from_str(&format!("Action::\"{}\"", action)) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail("action_uid", AuthzDenial::InvalidAction(e.to_string()));
            }
        };
        recorder.finish_phase("action_uid");

        // Build Cedar resource
        let resource_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            resource_type,
            resource_id_from_attrs(resource_attrs)
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail("resource_uid", AuthzDenial::InvalidResource(e.to_string()));
            }
        };
        recorder.finish_phase("resource_uid");

        // Build Cedar context from security context attrs + resource attrs
        let mut ctx_map: HashMap<String, cedar_policy::RestrictedExpression> = HashMap::new();

        // Add principal attributes to context
        if let Some(ref role) = security_ctx.principal.role {
            ctx_map.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        if let Some(ref acting_for) = security_ctx.principal.acting_for {
            ctx_map.insert(
                "actingFor".to_string(),
                cedar_policy::RestrictedExpression::new_string(acting_for.clone()),
            );
        }

        // Add context attributes
        for (key, value) in &security_ctx.context_attrs {
            insert_json_as_cedar(&mut ctx_map, key.clone(), value);
        }

        // Resource fields may appear as context.key for domain policies.
        // Authority keys (sessionId, agentType, role, …) stay on SecurityContext.
        for (key, value) in resource_attrs {
            if is_cedar_authority_context_key(key) || ctx_map.contains_key(key) {
                continue;
            }
            insert_json_as_cedar(&mut ctx_map, key.clone(), value);
        }
        crate::metrics::record_cedar_request_attribute_count("context", ctx_map.len());

        // Build context and request
        let context = match Context::from_pairs(ctx_map) {
            Ok(c) => c,
            Err(e) => {
                return recorder.fail("context_attrs", AuthzDenial::InvalidContext(e.to_string()));
            }
        };
        recorder.finish_phase("context_attrs");

        // Build principal entity with attributes so Cedar can resolve both
        // exact UID matches (`principal == Agent::"bot-1"`) and attribute
        // access (`principal.agent_type in [...]`).
        let mut principal_attrs: HashMap<String, cedar_policy::RestrictedExpression> =
            HashMap::new();
        principal_attrs.insert(
            "id".to_string(),
            cedar_policy::RestrictedExpression::new_string(security_ctx.principal.id.clone()),
        );
        principal_attrs.insert(
            "accountId".to_string(),
            cedar_policy::RestrictedExpression::new_string(security_ctx.principal.id.clone()),
        );
        if let Some(ref agent_type) = security_ctx.principal.agent_type {
            principal_attrs.insert(
                "agent_type".to_string(),
                cedar_policy::RestrictedExpression::new_string(agent_type.clone()),
            );
        }
        if let Some(ref role) = security_ctx.principal.role {
            principal_attrs.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        for (key, value) in &security_ctx.principal.attributes {
            insert_json_as_cedar(&mut principal_attrs, key.clone(), value);
        }
        crate::metrics::record_cedar_request_attribute_count("principal", principal_attrs.len());
        recorder.finish_phase("principal_attrs");

        let mut resource_entity_attrs: HashMap<String, cedar_policy::RestrictedExpression> =
            HashMap::new();
        for (key, value) in resource_attrs {
            insert_json_as_cedar(&mut resource_entity_attrs, key.clone(), value);
        }
        crate::metrics::record_cedar_request_attribute_count(
            "resource",
            resource_entity_attrs.len(),
        );
        recorder.finish_phase("resource_attrs");

        // Entity schema validation is intentionally None: app specs define
        // tenant-specific attributes that cannot be predicted by a static
        // platform schema. Policy-level type checks still apply.
        let parents = match tenant {
            Some(tenant) => self.principal_groups(
                tenant,
                policies,
                security_ctx,
                &principal_uid,
                &resource_uid,
            ),
            None => HashSet::new(),
        };
        recorder.finish_phase("principal_groups");

        let principal_entity = match Entity::new(
            principal_uid.clone(),
            principal_attrs.clone(),
            parents.clone(),
        ) {
            Ok(entity) => entity,
            Err(e) => {
                return recorder.fail(
                    "entities",
                    AuthzDenial::EngineError(format!("failed to build principal entity: {e}")),
                );
            }
        };
        let resource_entity = if resource_uid == principal_uid {
            let mut merged_attrs = principal_attrs;
            merged_attrs.extend(resource_entity_attrs);
            match Entity::new(resource_uid.clone(), merged_attrs, parents) {
                Ok(entity) => entity,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!(
                            "failed to build merged principal/resource entity: {e}"
                        )),
                    );
                }
            }
        } else {
            match Entity::new(resource_uid.clone(), resource_entity_attrs, HashSet::new()) {
                Ok(entity) => entity,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build resource entity: {e}")),
                    );
                }
            }
        };

        let entities = if resource_uid == principal_uid {
            match Entities::from_entities([resource_entity], None) {
                Ok(e) => e,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build entity store: {e}")),
                    );
                }
            }
        } else {
            match Entities::from_entities([principal_entity, resource_entity], None) {
                Ok(e) => e,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build entity store: {e}")),
                    );
                }
            }
        };
        recorder.finish_phase("entities");

        let request = match Request::new(
            principal_uid.clone(),
            action_uid.clone(),
            resource_uid.clone(),
            context,
            None, // schema-less: actions/resources are tenant-defined
        ) {
            Ok(r) => r,
            Err(e) => {
                return recorder.fail(
                    "request",
                    AuthzDenial::EngineError(format!("invalid request: {e}")),
                );
            }
        };
        recorder.finish_phase("request");

        let candidate_selection =
            policies
                .candidate_index
                .select(&principal_uid, &action_uid, &resource_uid);
        crate::metrics::record_cedar_policy_candidate_counts(
            candidate_selection.counts.full,
            candidate_selection.counts.candidate,
            candidate_selection.counts.outcome.as_str(),
        );
        recorder.finish_phase("policy_candidates");

        let effective_policy_set = candidate_selection
            .policy_set
            .as_ref()
            .unwrap_or(&policies.policy_set);
        // Cedar checks remaining stack and skips policies that exhaust it.
        // HTTP call depth must not change a permit or discard a forbid.
        let response: CedarResponse = stacker::grow(8 * 1024 * 1024, || {
            self.authorizer
                .is_authorized(&request, effective_policy_set, &entities)
        });
        recorder.finish_phase("authorizer");

        if response.diagnostics().errors().any(|error| {
            matches!(error,
                cedar_policy::AuthorizationError::PolicyEvaluationError(error)
                    if matches!(error.inner(), cedar_policy::EvaluationError::RecursionLimit(_)))
        }) {
            return recorder.fail(
                "authorizer",
                AuthzDenial::EngineError("Cedar policy evaluation exceeded its stack limit".into()),
            );
        }

        let decision = response.decision();
        recorder.finish(match decision {
            Decision::Allow => CedarDecisionMetric::Allow,
            Decision::Deny => CedarDecisionMetric::Deny,
        });

        // A statement labelled with its source (`@source("app/file.cedar#2")`,
        // `statements::label_statements`) is named by it; others keep Cedar's id.
        let named = |id: &PolicyId| {
            effective_policy_set
                .annotation(id, crate::statements::SOURCE_ANNOTATION)
                .map_or_else(|| id.to_string(), str::to_string)
        };
        match decision {
            Decision::Allow => {
                let policy_ids: Vec<String> = response.diagnostics().reason().map(named).collect();
                AuthzDecision::Allow { policy_ids }
            }
            Decision::Deny => {
                let policy_ids: Vec<String> = response.diagnostics().reason().map(named).collect();
                if policy_ids.is_empty() {
                    AuthzDecision::Deny(AuthzDenial::NoMatchingPermit)
                } else {
                    AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids })
                }
            }
        }
    }

    /// The groups `principal in` can find the principal in, for this request:
    /// each candidate the tenant's statements name (and the resource, when one
    /// says `principal in resource`) that the principal, or the person an agent
    /// acts for, is an active member of. One lookup per candidate, so a check
    /// costs what the policy text asks, however many groups the person is in.
    fn principal_groups(
        &self,
        tenant: &str,
        policies: &CompiledPolicies,
        security_ctx: &SecurityContext,
        principal_uid: &EntityUid,
        resource_uid: &EntityUid,
    ) -> HashSet<EntityUid> {
        let memberships = &self.group_memberships;
        let groups = &policies.groups;
        let resource = groups.resource.then_some(resource_uid);
        let mut parents = HashSet::new(); // determinism-ok: Cedar API requires HashSet
        for group in resource.into_iter().chain(groups.literals.iter()) {
            if group == principal_uid || parents.contains(group) {
                continue;
            }
            let key = group.to_string();
            let member = |who: &str| memberships.contains(tenant, &key, who);
            if member(&security_ctx.principal.id)
                || security_ctx
                    .principal
                    .acting_for
                    .as_deref()
                    .is_some_and(member)
            {
                parents.insert(group.clone());
            }
        }
        parents
    }

    /// Whether a list's `read` decision is the same for every row of `resource_type`.
    ///
    /// Callers that get [`ListReadUniformity::Uniform`] evaluate Cedar once and
    /// apply that decision to the page. A per-entity result still filters each
    /// row; `attribute_keys` is the set of resource fields that can change the
    /// answer, so the caller does not have to load the rest of the document.
    pub fn list_read_uniformity(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
    ) -> ListReadUniformity {
        let tenants = match self.tenant_policies.read() {
            Ok(tenants) => tenants,
            Err(_) => {
                return ListReadUniformity::PerEntity {
                    attribute_keys: None,
                };
            }
        };
        if let Some(tenant_policies) = tenants.get(tenant) {
            return list_uniformity::classify(
                &tenant_policies.policies.policy_set,
                security_ctx,
                action,
                resource_type,
            );
        }
        drop(tenants);
        if Self::is_system(security_ctx) {
            return list_uniformity::classify(
                &self.platform_policy_set.policy_set,
                security_ctx,
                action,
                resource_type,
            );
        }
        // No tenant policy and not a system principal: Cedar default-deny.
        // That denial does not depend on which row is being read.
        ListReadUniformity::Uniform
    }

    /// Quick check: is this a system principal?
    ///
    /// Since ADR-0046, this no longer bypasses authorization. It is kept as a
    /// convenience predicate for callers that want to branch on principal
    /// kind for reasons other than authorization (logging, telemetry tagging).
    /// Actual authorization of System principals flows through the normal
    /// Cedar evaluation, matching the built-in `system-platform` policy
    /// installed at engine construction time (see `system_platform_policy`).
    pub fn is_system(security_ctx: &SecurityContext) -> bool {
        security_ctx.principal.kind == PrincipalKind::System
    }

    /// Authorize through the fallback global policy set.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform` Cedar
    /// policy; delegating straight to [`authorize`] ensures every request is
    /// policy-checked and logged.
    pub fn authorize_or_bypass(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize(security_ctx, action, resource_type, resource_attrs)
    }

    /// Authorize for a specific tenant through Cedar.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform`
    /// policy merged into the fallback policy set; this function simply
    /// delegates to [`authorize_for_tenant`].
    pub fn authorize_for_tenant_or_bypass(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize_for_tenant(tenant, security_ctx, action, resource_type, resource_attrs)
    }
}

/// The single source of truth for "who is a verified platform operator" in
/// Cedar. Shared by the built-in [`system_platform_policy`] identity-entity
/// clauses here in temper-authz AND the seeded operator `manage_policies`
/// permit in temper-platform (which re-exports this const), so the two
/// authorization surfaces can never drift to different strength.
///
/// It requires BOTH `agent_type == "operator"` and `agentTypeVerified == true`:
/// a self-declared, credential-unverified context claiming the operator type
/// (e.g. one built via `SecurityContext::with_agent_context`) does NOT satisfy
/// it. The `has` guards make a principal lacking either attribute evaluate the
/// clause to `false` (a clean deny) rather than raising a Cedar evaluation
/// error.
///
/// "operator" is a privileged claim ultimately guarded by trusted-issuer
/// registration: a registered issuer could mint an operator-claim JWT, but
/// registering an issuer is itself gated to System/Admin/verified-operator by
/// the identity-entity clauses below.
pub const VERIFIED_OPERATOR_WHEN: &str = "principal has agent_type && principal.agent_type == \"operator\" && principal has agentTypeVerified && principal.agentTypeVerified == true";

/// Built the built-in system-platform Cedar policy, interpolating the shared
/// [`VERIFIED_OPERATOR_WHEN`] predicate into the four identity-entity clauses.
/// Built-in Cedar in every [`AuthzEngine`] (ADR-0046): System's broad
/// authority as an auditable policy instead of the old blanket bypass, and
/// what only the platform may touch (issuers, generations, HTTP routes).
fn system_platform_policy() -> String {
    format!(
        r#"
@id("system-platform:broad-permit")
permit(principal is System, action, resource);

@id("system-platform:identity-entities-permit-trusted-issuer")
permit(principal, action, resource is TrustedIssuer)
when {{ principal is System || principal is Admin || ({op}) }};

@id("system-platform:identity-entities-permit-principal-generation")
permit(principal, action, resource is PrincipalGeneration)
when {{ principal is System || principal is Admin || ({op}) }};

@id("system-platform:protect-trusted-issuer")
forbid(principal, action, resource is TrustedIssuer)
unless {{ principal is System || principal is Admin || ({op}) }};

@id("system-platform:protect-principal-generation")
forbid(principal, action, resource is PrincipalGeneration)
unless {{ principal is System || principal is Admin || ({op}) }};

// ADR-0069: a route reshapes the tenant's HTTP surface, so only the platform manages one.
@id("system-platform:http-endpoint-management-permit")
permit(principal, action in [Action::"create", Action::"Create", Action::"update", Action::"delete", Action::"Pause", Action::"Resume", Action::"Delete"], resource is HttpEndpoint)
when {{ principal is System || principal is Admin || ({op}) }};

@id("system-platform:protect-http-endpoint-management")
forbid(principal, action in [Action::"create", Action::"Create", Action::"update", Action::"delete", Action::"Pause", Action::"Resume", Action::"Delete"], resource is HttpEndpoint)
unless {{ principal is System || principal is Admin || ({op}) }};
"#,
        op = VERIFIED_OPERATOR_WHEN
    )
}

/// PolicyId prefix used for the built-in system-platform policies
/// (ADR-0046). Used to exclude them from user-facing counts.
const SYSTEM_PLATFORM_POLICY_ID_PREFIX: &str = "system-platform:";

/// Merge the built-in system-platform policy into an existing [`PolicySet`].
///
/// Policies are added with explicit `PolicyId`s prefixed by
/// [`SYSTEM_PLATFORM_POLICY_ID_PREFIX`] so downstream code can filter them
/// out of user-facing reports (see [`count_user_policies`]). If the
/// hard-coded system policy fails to parse, the combined set is left
/// unchanged — preserving availability at the cost of System auth.
fn compiled_system_platform_policies() -> CompiledPolicies {
    let mut policy_set = PolicySet::new();
    merge_system_platform_policy(&mut policy_set);
    CompiledPolicies::new(policy_set)
}

/// Count user-authored policies in a [`PolicySet`], excluding the built-in
/// `system-platform` policies (ADR-0046). Tenants should reason about their
/// own policy surface without the platform's internals polluting the count.
fn count_user_policies(ps: &PolicySet) -> usize {
    ps.policies()
        .filter(|p| {
            !p.id()
                .to_string()
                .starts_with(SYSTEM_PLATFORM_POLICY_ID_PREFIX)
        })
        .count()
}

struct CedarEvaluationRecorder {
    started_at: Instant,
    phase_started_at: Instant,
    check: CedarCheck,
}

impl CedarEvaluationRecorder {
    fn start(check: CedarCheck) -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            phase_started_at: now,
            check,
        }
    }

    fn finish_phase(&mut self, phase: &'static str) {
        crate::metrics::record_cedar_phase_duration(
            phase,
            self.phase_started_at.elapsed(),
            CedarPhaseOutcome::Ok,
        );
        self.phase_started_at = Instant::now();
    }

    fn fail(&mut self, phase: &'static str, denial: AuthzDenial) -> AuthzDecision {
        crate::metrics::record_cedar_phase_duration(
            phase,
            self.phase_started_at.elapsed(),
            CedarPhaseOutcome::Error,
        );
        crate::metrics::record_cedar_evaluation(
            self.started_at.elapsed(),
            CedarDecisionMetric::Error,
            self.check,
        );
        AuthzDecision::Deny(denial)
    }

    fn finish(&self, decision: CedarDecisionMetric) {
        crate::metrics::record_cedar_evaluation(self.started_at.elapsed(), decision, self.check);
    }
}

/// Insert a `serde_json::Value` into a Cedar context map, converting to the
/// appropriate `RestrictedExpression` type. Supports strings, bools, integers,
/// and arrays of those types.
fn insert_json_as_cedar(
    map: &mut HashMap<String, cedar_policy::RestrictedExpression>,
    key: String,
    value: &serde_json::Value,
) {
    if let Some(s) = value.as_str() {
        map.insert(
            key,
            cedar_policy::RestrictedExpression::new_string(s.to_string()),
        );
    } else if let Some(b) = value.as_bool() {
        map.insert(key, cedar_policy::RestrictedExpression::new_bool(b));
    } else if let Some(n) = value.as_i64() {
        map.insert(key, cedar_policy::RestrictedExpression::new_long(n));
    } else if let Some(arr) = value.as_array() {
        let items: Vec<cedar_policy::RestrictedExpression> = arr
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(cedar_policy::RestrictedExpression::new_string(
                        s.to_string(),
                    ))
                } else if let Some(n) = item.as_i64() {
                    Some(cedar_policy::RestrictedExpression::new_long(n))
                } else {
                    item.as_bool()
                        .map(cedar_policy::RestrictedExpression::new_bool)
                }
            })
            .collect();
        map.insert(key, cedar_policy::RestrictedExpression::new_set(items));
    }
}

fn resource_id_from_attrs(attrs: &HashMap<String, serde_json::Value>) -> String {
    attrs
        .get("id")
        .or_else(|| attrs.get("Id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}
