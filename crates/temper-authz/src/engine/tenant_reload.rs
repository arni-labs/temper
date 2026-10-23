//! Loading a tenant's Cedar: statements parsed once and reused (ADR-0183).

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use cedar_policy::{Policy, PolicyId, PolicySet};

use super::{
    AuthzEngine, CompiledPolicies, SYSTEM_PLATFORM_POLICY_ID_PREFIX, TenantPolicies,
    VERIFIED_OPERATOR_WHEN, group_candidates, statement_cache,
};
use crate::error::AuthzError;

impl AuthzEngine {
    /// Hot-reload Cedar policies for a specific tenant. Parses and validates
    /// the new policy text, then atomically swaps the tenant's policy set.
    /// If parsing fails, existing policies remain in effect. Only statements
    /// the tenant's last reload did not have are parsed (ADR-0183).
    pub fn reload_tenant_policies(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), AuthzError> {
        self.reload_tenant_statements(tenant, &[(None, policy_text)])
    }

    /// Hot-reload Cedar policies for a tenant from individually named policy
    /// entries. Each `(policy_id, cedar_text)` pair is parsed individually and
    /// assigned a meaningful `PolicyId` of the form `"{tenant}:{policy_id}"`.
    ///
    /// Multiple permit/forbid statements in one `cedar_text` are suffixed:
    /// `"{tenant}:{policy_id}:0"`, `":1"`, etc.
    ///
    /// This enables meaningful policy IDs in denial diagnostics instead of
    /// auto-generated names like `"policy0"`.
    pub fn reload_tenant_policies_named(
        &self,
        tenant: &str,
        policies: &[(String, String)], // (policy_id, cedar_text)
    ) -> Result<(), AuthzError> {
        let entries: Vec<(Option<&str>, &str)> = policies
            .iter()
            .map(|(policy_id, cedar_text)| (Some(policy_id.as_str()), cedar_text.as_str()))
            .collect();
        self.reload_tenant_statements(tenant, &entries)
    }

    /// Cedar statements parsed by tenant reloads since this engine started.
    pub fn statements_parsed(&self) -> u64 {
        self.statements_parsed.load(Ordering::Relaxed)
    }

    /// Statements held parsed for a tenant's next reload.
    pub fn cached_statement_count(&self, tenant: &str) -> usize {
        self.tenant_policies
            .read()
            .ok()
            .and_then(|tenants| tenants.get(tenant).map(|tp| tp.statements.len()))
            .unwrap_or(0)
    }

    /// Build a tenant's set from `entries`: unnamed text keeps Cedar's positional
    /// ids (`policy{i}`), a named entry's statements are `{tenant}:{name}` (or
    /// `{tenant}:{name}:{i}` for several). Statements the tenant's last reload
    /// parsed are reused, so the set is rebuilt without re-parsing them.
    fn reload_tenant_statements(
        &self,
        tenant: &str,
        entries: &[(Option<&str>, &str)],
    ) -> Result<(), AuthzError> {
        let previous = self
            .tenant_policies
            .read()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?
            .get(tenant)
            .map(|tp| Arc::clone(&tp.statements))
            .unwrap_or_default();
        let mut next = statement_cache::StatementCache::default();
        let mut policy_set = PolicySet::new();
        let mut groups = Vec::new();
        let mut parsed = 0;
        let mut source_text = String::new();
        let add = |set: &mut PolicySet, policy: Policy| {
            set.add(policy)
                .map_err(|e| AuthzError::PolicyParse(e.to_string()))
        };
        for (name, cedar_text) in entries {
            let (statements, count) =
                previous
                    .parse(cedar_text, &mut next)
                    .map_err(|error| match (name, error) {
                        (Some(name), AuthzError::PolicyParse(e)) => {
                            AuthzError::PolicyParse(format!("{name}: {e}"))
                        }
                        (_, error) => error,
                    })?;
            parsed += count;
            let id = |index: usize, of: usize| match name {
                None => format!("policy{index}"),
                Some(name) if of == 1 => format!("{tenant}:{name}"),
                Some(name) => format!("{tenant}:{name}:{index}"),
            };
            match statements {
                statement_cache::ParsedText::Statements(statements) => {
                    for (index, statement) in statements.iter().enumerate() {
                        let policy = statement
                            .policy
                            .clone()
                            .new_id(PolicyId::new(id(index, statements.len())));
                        add(&mut policy_set, policy)?;
                        groups.push(statement.groups.clone());
                    }
                }
                statement_cache::ParsedText::Whole(whole) => {
                    if name.is_none() {
                        // Unnamed text keeps its templates, as a whole parse did.
                        for template in whole.templates() {
                            policy_set
                                .add_template(template.clone())
                                .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
                        }
                    }
                    let count = whole.policies().count();
                    for (index, policy) in whole.policies().enumerate() {
                        groups.push(group_candidates::StatementGroups::of(policy));
                        let policy = match name {
                            None => policy.clone(),
                            Some(_) => policy.clone().new_id(PolicyId::new(id(index, count))),
                        };
                        add(&mut policy_set, policy)?;
                    }
                }
            }
            if !source_text.is_empty() {
                source_text.push('\n');
            }
            source_text.push_str(cedar_text);
        }
        merge_system_platform_policy(&mut policy_set);
        groups.extend(system_platform_statements().iter().map(|p| p.1.clone()));
        let policies = CompiledPolicies::with_groups(policy_set, &groups);

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;
        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policies,
                source_text,
                statements: Arc::new(next),
            },
        );
        self.statements_parsed
            .fetch_add(parsed as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// The built-in system-platform statements, named, parsed once per process.
fn system_platform_statements() -> &'static [(Policy, group_candidates::StatementGroups)] {
    static STATEMENTS: OnceLock<Vec<(Policy, group_candidates::StatementGroups)>> = OnceLock::new();
    STATEMENTS.get_or_init(|| {
        let Ok(system_set) = system_platform_policy().parse::<PolicySet>() else {
            return Vec::new();
        };
        system_set
            .policies()
            .enumerate()
            .map(|(idx, policy)| {
                let named = policy.clone().new_id(PolicyId::new(format!(
                    "{SYSTEM_PLATFORM_POLICY_ID_PREFIX}broad-permit-{idx}"
                )));
                let groups = group_candidates::StatementGroups::of(&named);
                (named, groups)
            })
            .collect()
    })
}

pub(super) fn merge_system_platform_policy(combined: &mut PolicySet) {
    for (policy, _) in system_platform_statements() {
        let _ = combined.add(policy.clone());
    }
}

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

// ADR-0183: being in a group is authority. In a tenant several apps share, one
// app's broad statement such as `permit(principal, action == Action::"create",
// resource)` would let any signed-in person StartMembership themselves into
// Group::"app:<id>:edit" of any app. So memberships are written only by the
// platform or by an installed module (context.module, which only the kernel
// sets); who may read them stays with the tenant's Cedar.
@id("system-platform:protect-group-membership-writes")
forbid(principal, action in [Action::"StartMembership", Action::"EndMembership", Action::"create", Action::"Create", Action::"update", Action::"delete", Action::"Delete"], resource is GroupMembership)
unless {{ principal is System || principal is Admin || ({op}) || context has module }};
"#,
        op = VERIFIED_OPERATOR_WHEN
    )
}
