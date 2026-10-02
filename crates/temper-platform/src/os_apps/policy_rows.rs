use std::collections::BTreeSet;

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
/// Text in those rows that the new bundle no longer carries is superseded: it
/// must leave the live set, the legacy aggregate and any `primary` snapshot,
/// or an old forbid outlives its replacement (forbid beats every permit).
#[derive(Debug, Default)]
pub(super) struct SupersededAppPolicies {
    /// Superseded policy texts, trimmed, each recorded once.
    texts: Vec<String>,
    /// This app's rows whose source file is gone from the new bundle.
    removed_row_ids: Vec<String>,
    /// The tenant's `primary` snapshot row, when one exists.
    primary: Option<PolicyStoreRow>,
}

impl SupersededAppPolicies {
    pub(super) fn has_texts(&self) -> bool {
        !self.texts.is_empty()
    }

    /// The tenant's Cedar after installing `bundle`: `live_text` without the
    /// superseded texts, plus the bundle's policies.
    ///
    /// # Errors
    ///
    /// Returns an error when removing the superseded texts leaves Cedar that
    /// does not parse; the install then stops before persisting anything.
    pub(super) fn replace_in(
        &self,
        state: &PlatformState,
        live_text: &str,
        bundle: &AppBundle,
    ) -> Result<String, String> {
        let combined = merge_bundle_policies(
            &strip_policy_texts(live_text, &self.texts),
            &bundle.cedar_policies,
        );
        if self.has_texts() {
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

/// Read which of `app_name`'s previously installed Cedar `bundle` replaces.
///
/// Text another owner (an approval, a hand-added row or another app) holds
/// verbatim is kept. Without a durable policy store nothing is recorded per
/// app, so nothing is superseded.
///
/// # Errors
///
/// Returns an error when the tenant's policy rows cannot be read.
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
    let bundle_texts: BTreeSet<&str> = bundle
        .cedar_policy_sources
        .iter()
        .map(|source| source.text.trim())
        .filter(|text| !text.is_empty())
        .collect();
    let bundle_row_ids: BTreeSet<String> = bundle
        .cedar_policy_sources
        .iter()
        .filter(|source| !source.text.trim().is_empty())
        .map(|source| os_app_policy_row_id(app_name, &source.relative_path))
        .collect();
    let held_by_others: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row.enabled && row.created_by != owner && row.policy_id != PRIMARY_POLICY_ID)
        .map(|row| row.cedar_text.trim())
        .collect();

    let mut superseded = SupersededAppPolicies {
        primary: rows
            .iter()
            .find(|row| row.policy_id == PRIMARY_POLICY_ID)
            .cloned(),
        ..SupersededAppPolicies::default()
    };
    for row in rows.iter().filter(|row| row.created_by == owner) {
        if !bundle_row_ids.contains(&row.policy_id) {
            superseded.removed_row_ids.push(row.policy_id.clone());
        }
        let text = row.cedar_text.trim();
        if text.is_empty() || bundle_texts.contains(text) || held_by_others.contains(text) {
            continue;
        }
        if !superseded.texts.iter().any(|known| known == text) {
            superseded.texts.push(text.to_string());
        }
    }
    debug_assert!(
        superseded
            .texts
            .iter()
            .all(|text| !bundle_texts.contains(text.as_str()))
    );
    Ok(superseded)
}

/// Drop the rows of source files the bundle no longer has, and strip the
/// superseded texts from the `primary` snapshot, which boot loads instead of
/// the legacy aggregate.
///
/// # Errors
///
/// Returns an error when a row cannot be deleted or rewritten.
pub(super) async fn retire_superseded_rows(
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
    let Some(primary) = superseded.primary.as_ref() else {
        return Ok(());
    };
    let stripped = strip_policy_texts(&primary.cedar_text, &superseded.texts);
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

/// Remove every occurrence of each text that spans whole lines.
///
/// Installs join policy files with newlines, so a file's text always starts
/// and ends on a line boundary; a match inside a line belongs to something
/// else and stays.
fn strip_policy_texts(policy_text: &str, texts: &[String]) -> String {
    let mut stripped = policy_text.to_string();
    for text in texts {
        stripped = strip_line_bounded(&stripped, text.trim());
    }
    stripped
}

fn strip_line_bounded(haystack: &str, needle: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let mut kept = String::with_capacity(haystack.len());
    let mut rest = haystack;
    let mut removed = false;
    while let Some(position) = rest.find(needle) {
        let end = position + needle.len();
        let starts_line = if position == 0 {
            kept.is_empty() || kept.ends_with('\n')
        } else {
            rest.as_bytes()[position - 1] == b'\n'
        };
        let ends_line = end == rest.len() || rest.as_bytes()[end] == b'\n';
        if starts_line && ends_line {
            kept.push_str(&rest[..position]);
            rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
            removed = true;
        } else {
            kept.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    if !removed {
        return haystack.to_string();
    }
    kept.push_str(rest);
    // A text removed from the end leaves the newline that joined it.
    let trimmed_len = kept.trim_end_matches('\n').len();
    kept.truncate(trimmed_len);
    debug_assert!(kept.len() < haystack.len());
    kept
}

#[cfg(test)]
mod tests {
    use super::strip_line_bounded;

    #[test]
    fn strips_whole_line_occurrences_only() {
        let old = "forbid(principal, action == Action::\"Withdraw\", resource);";
        let text = format!("permit(principal, action, resource);\n{old}\n// note {old}\n{old}");
        let stripped = strip_line_bounded(&text, old);
        assert_eq!(
            stripped,
            format!("permit(principal, action, resource);\n// note {old}")
        );
    }

    #[test]
    fn leaves_text_without_the_needle_unchanged() {
        let text = "permit(principal, action, resource);";
        assert_eq!(strip_line_bounded(text, "forbid(x);"), text);
        assert_eq!(strip_line_bounded(text, ""), text);
    }
}
