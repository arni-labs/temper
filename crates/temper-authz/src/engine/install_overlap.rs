//! A tenant decided under two versions' policies while one is installed.

use cedar_policy::PolicySet;

use super::platform_policy::merge_system_platform_policy;
use super::{AuthzEngine, AuthzError, CompiledPolicies, TenantPolicies};

impl AuthzEngine {
    /// Load a version's policy for `tenant` while its specs are not in yet,
    /// keeping the policy in force before it: until
    /// [`Self::end_install_overlap`], a request is allowed only when both
    /// allow it.
    ///
    /// An install cannot put a policy and its specs in at one instant, and
    /// either order alone lets a move through that one version forbids. The
    /// new policy first lets a caller with a broad permit run an action the
    /// update removes, with the denial it removes too, while the old spec is
    /// still live; the old policy last lets a reaction the update adds make
    /// a write the new policy forbids. Under both at once, a move is allowed
    /// only what each version allows. If parsing fails, nothing changes.
    pub fn reload_tenant_policies_for_install(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);
        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;
        // The policy in force before the first install still under way is
        // the one to keep: a second install over it must not drop it.
        let before = tenants
            .remove(tenant)
            .map(|previous| previous.before.unwrap_or(previous.policies));
        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policies: CompiledPolicies::new(new_policy_set),
                source_text: policy_text.to_string(),
                before,
            },
        );
        Ok(())
    }

    /// The version being installed in `tenant` has its specs in: from now on
    /// only its policy decides ([`Self::reload_tenant_policies_for_install`]).
    pub fn end_install_overlap(&self, tenant: &str) {
        if let Ok(mut tenants) = self.tenant_policies.write()
            && let Some(policies) = tenants.get_mut(tenant)
        {
            policies.before = None;
        }
    }
}
