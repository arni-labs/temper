use std::collections::BTreeSet;

use temper_authz::statements::{StatementKey, remove_statements, statement_keys};
use temper_server::storage::PolicyStoreRow;

use super::{AppBundle, merge_bundle_policies};
use crate::state::PlatformState;

/// Row written by `PUT /api/tenants/{tenant}/policies`. At boot it replaces the
/// legacy aggregate, so it is a snapshot of every app's Cedar, not an owner.
const PRIMARY_POLICY_ID: &str = "primary";

pub(crate) fn os_app_policy_row_id(app_name: &str, relative_path: &str) -> String {
    let source = relative_path
        .trim_start_matches('/')
        .strip_prefix("policies/")
        .unwrap_or_else(|| relative_path.trim_start_matches('/'))
        .strip_suffix(".cedar")
        .unwrap_or(relative_path);
    let mut slug = String::new();
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            slug.push(ch);
        } else if ch == '/' || ch == '.' {
            slug.push('-');
        } else {
            slug.push('_');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("{app_name}-policy")
    } else {
        format!("{app_name}-{slug}")
    }
}

fn os_app_policy_owner(app_name: &str) -> String {
    format!("os-app:{app_name}")
}

pub(super) async fn persist_bundle_policy_rows(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<(), String> {
    if bundle.cedar_policy_sources.is_empty() {
        return Ok(());
    }
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(());
    };
    let created_by = os_app_policy_owner(app_name);
    for source in &bundle.cedar_policy_sources {
        let cedar_text = source.text.trim();
        if cedar_text.is_empty() {
            continue;
        }
        let policy_id = os_app_policy_row_id(app_name, &source.relative_path);
        // `save_policy` skips an insert when an enabled row already has
        // this cedar_text (ARN-286/399). Reinstall must not grow duplicates.
        policy_store
            .save_policy(tenant, &policy_id, cedar_text, &created_by)
            .await
            .map_err(|error| {
                format!(
                    "Failed to persist OS app Cedar policy row '{policy_id}' for '{app_name}': {error}"
                )
            })?;
    }
    Ok(())
}

/// The Cedar a reinstall of one app replaces.
///
/// An earlier install of the app recorded each of its policy files as a row.
/// Statements in those rows that the new bundle no longer carries are
/// superseded: they must leave the live set, the legacy aggregate and any
/// `primary` snapshot, or an old forbid outlives its replacement (forbid beats
/// every permit). Statements are compared parsed, so layout does not matter.
#[derive(Debug, Default)]
pub(super) struct SupersededAppPolicies {
    /// Superseded statements that no other owner holds.
    statements: BTreeSet<StatementKey>,
    /// This app's rows whose source file is gone from the new bundle.
    removed_row_ids: Vec<String>,
    /// The tenant's `primary` snapshot row, when one exists.
    primary: Option<PolicyStoreRow>,
}

impl SupersededAppPolicies {
    pub(super) fn has_statements(&self) -> bool {
        !self.statements.is_empty()
    }

    /// The tenant's Cedar after installing `bundle`: `live_text` without the
    /// superseded statements, plus the bundle's policies.
    ///
    /// # Errors
    ///
    /// Returns an error when the live text does not parse or the result is
    /// not valid Cedar; the install then stops before persisting anything.
    pub(super) fn replace_in(
        &self,
        state: &PlatformState,
        live_text: &str,
        bundle: &AppBundle,
    ) -> Result<String, String> {
        let kept = remove_statements(live_text, &self.statements)
            .map_err(|error| format!("Failed to read the tenant's live Cedar: {error}"))?;
        let combined = merge_bundle_policies(&kept, &bundle.cedar_policies);
        if self.has_statements() {
            state
                .server
                .authz
                .validate_tenant_policies(&combined)
                .map_err(|error| {
                    format!("Cedar after replacing the app's previous policies is invalid: {error}")
                })?;
        }
        Ok(combined)
    }
}

/// Read which of `app_name`'s previously installed statements `bundle` replaces.
///
/// A statement another enabled row holds (an approval, a hand-added row or
/// another app, but not the `primary` snapshot) is kept. Without a durable
/// policy store nothing is recorded per app, so nothing is superseded.
///
/// # Errors
///
/// Returns an error when the tenant's policy rows cannot be read or parsed.
pub(super) async fn superseded_app_policies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<SupersededAppPolicies, String> {
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(SupersededAppPolicies::default());
    };
    let rows = policy_store
        .load_policies_for_tenant(tenant)
        .await
        .map_err(|error| format!("Failed to read Cedar policy rows for '{tenant}': {error}"))?;

    let owner = os_app_policy_owner(app_name);
    let mut bundle_statements = BTreeSet::new();
    let mut bundle_row_ids = BTreeSet::new();
    for source in &bundle.cedar_policy_sources {
        if source.text.trim().is_empty() {
            continue;
        }
        bundle_statements.extend(parsed_statements(&source.relative_path, &source.text)?);
        bundle_row_ids.insert(os_app_policy_row_id(app_name, &source.relative_path));
    }
    let mut previous = BTreeSet::new();
    let mut held_by_others = BTreeSet::new();
    let mut superseded = SupersededAppPolicies::default();
    for row in &rows {
        if row.created_by == owner {
            previous.extend(parsed_statements(&row.policy_id, &row.cedar_text)?);
            if !bundle_row_ids.contains(&row.policy_id) {
                superseded.removed_row_ids.push(row.policy_id.clone());
            }
        } else if row.policy_id == PRIMARY_POLICY_ID {
            superseded.primary = Some(row.clone());
        } else if row.enabled {
            held_by_others.extend(parsed_statements(&row.policy_id, &row.cedar_text)?);
        }
    }
    superseded.statements = previous
        .difference(&bundle_statements)
        .filter(|statement| !held_by_others.contains(*statement))
        .cloned()
        .collect();
    debug_assert!(superseded.statements.is_disjoint(&bundle_statements));
    debug_assert!(superseded.statements.is_disjoint(&held_by_others));
    Ok(superseded)
}

fn parsed_statements(source: &str, cedar_text: &str) -> Result<BTreeSet<StatementKey>, String> {
    statement_keys(cedar_text)
        .map_err(|error| format!("Failed to parse Cedar in '{source}': {error}"))
}

/// Remove the superseded statements from the `primary` snapshot, which boot
/// loads in place of the legacy aggregate.
///
/// Runs before anything else is written, so a failure leaves the previous
/// install fully in place.
///
/// # Errors
///
/// Returns an error when the snapshot cannot be parsed or rewritten.
pub(super) async fn rewrite_primary_snapshot(
    state: &PlatformState,
    tenant: &str,
    superseded: &SupersededAppPolicies,
) -> Result<(), String> {
    let (Some(policy_store), Some(primary)) = (state.server.policy_store(), &superseded.primary)
    else {
        return Ok(());
    };
    let stripped =
        remove_statements(&primary.cedar_text, &superseded.statements).map_err(|error| {
            format!("Failed to parse the '{PRIMARY_POLICY_ID}' policy row: {error}")
        })?;
    if stripped == primary.cedar_text {
        return Ok(());
    }
    policy_store
        .update_policy_text(tenant, PRIMARY_POLICY_ID, &stripped, &primary.created_by)
        .await
        .map_err(|error| {
            format!("Failed to rewrite the '{PRIMARY_POLICY_ID}' policy row: {error}")
        })?;
    Ok(())
}

/// Delete the rows of source files the bundle no longer has.
///
/// Runs after the bundle's rows are written, so the app is never left with
/// fewer policies than either version.
///
/// # Errors
///
/// Returns an error when a row cannot be deleted.
pub(super) async fn delete_dropped_rows(
    state: &PlatformState,
    tenant: &str,
    superseded: &SupersededAppPolicies,
) -> Result<(), String> {
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(());
    };
    for policy_id in &superseded.removed_row_ids {
        policy_store
            .delete_policy(tenant, policy_id)
            .await
            .map_err(|error| format!("Failed to delete Cedar policy row '{policy_id}': {error}"))?;
    }
    Ok(())
}
