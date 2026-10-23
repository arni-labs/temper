//! The groups a tenant's policies can ask a principal about (ADR-0183).
//!
//! Only `principal in X` can be true because of a membership, so only the `X`
//! the statements name are looked up: an entity literal (in the scope or in a
//! condition, alone or in a set), and the request's resource when a statement
//! says `principal in resource`. Collected once per parse of the statements.

use std::collections::BTreeSet;
use std::str::FromStr;

use cedar_policy::{EntityUid, Policy, PrincipalConstraint};

/// What one statement can ask: kept with its parse, so a reload that parses
/// one statement does not walk the others again.
#[derive(Debug, Clone, Default)]
pub(super) struct StatementGroups {
    resource: bool,
    literals: BTreeSet<String>,
}

impl StatementGroups {
    pub(super) fn of(policy: &Policy) -> Self {
        let mut groups = Self::default();
        match policy.principal_constraint() {
            PrincipalConstraint::In(uid) | PrincipalConstraint::IsIn(_, uid) => {
                groups.literals.insert(uid.to_string());
            }
            PrincipalConstraint::Any | PrincipalConstraint::Eq(_) | PrincipalConstraint::Is(_) => {}
        }
        if let Ok(json) = policy.to_json() {
            walk(
                &json["conditions"],
                &mut groups.resource,
                &mut groups.literals,
            );
        }
        groups
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct GroupCandidates {
    /// A statement says `principal in resource` (or `is T in resource`).
    pub(super) resource: bool,
    /// Entity literals statements put on the right of `principal in`.
    pub(super) literals: Vec<EntityUid>,
}

impl GroupCandidates {
    pub(super) fn from_statements<'a>(
        statements: impl IntoIterator<Item = &'a StatementGroups>,
    ) -> Self {
        let mut resource = false;
        let mut literals = BTreeSet::new();
        for statement in statements {
            resource |= statement.resource;
            literals.extend(statement.literals.iter().cloned());
        }
        Self {
            resource,
            literals: literals
                .into_iter()
                .filter_map(|uid| EntityUid::from_str(&uid).ok())
                .collect(),
        }
    }
}

fn walk(value: &serde_json::Value, resource: &mut bool, literals: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                walk(item, resource, literals);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(op) = map.get("in")
                && is_var(&op["left"], "principal")
            {
                right_side(&op["right"], resource, literals);
            }
            if let Some(op) = map.get("is")
                && is_var(&op["left"], "principal")
                && let Some(right) = op.get("in")
            {
                right_side(right, resource, literals);
            }
            for child in map.values() {
                walk(child, resource, literals);
            }
        }
        _ => {}
    }
}

fn right_side(value: &serde_json::Value, resource: &mut bool, literals: &mut BTreeSet<String>) {
    if is_var(value, "resource") {
        *resource = true;
    } else if let Some(entity) = value.get("Value").and_then(|v| v.get("__entity")) {
        if let (Some(ty), Some(id)) = (entity["type"].as_str(), entity["id"].as_str())
            && let Ok(uid) = EntityUid::from_str(&format!("{ty}::{}", serde_json::Value::from(id)))
        {
            literals.insert(uid.to_string());
        }
    } else if let Some(items) = value.get("Set").and_then(|s| s.as_array()) {
        for item in items {
            right_side(item, resource, literals);
        }
    }
}

fn is_var(value: &serde_json::Value, name: &str) -> bool {
    value.get("Var").and_then(|v| v.as_str()) == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(text: &str) -> GroupCandidates {
        let set = text.parse::<cedar_policy::PolicySet>().unwrap();
        let statements: Vec<StatementGroups> = set.policies().map(StatementGroups::of).collect();
        GroupCandidates::from_statements(&statements)
    }

    #[test]
    fn the_groups_policies_name_are_the_candidates_and_nothing_else() {
        let found = candidates(
            r#"permit(principal in Team::"a", action, resource);
               permit(principal, action, resource) when { principal in [Team::"b", Team::"c"] || principal is Customer in Org::"o" };
               permit(principal, action, resource) when { resource.owner == "x" && Team::"z" in Team::"y" };"#,
        );
        let names: Vec<String> = found.literals.iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            ["Org::\"o\"", "Team::\"a\"", "Team::\"b\"", "Team::\"c\""]
        );
        assert!(!found.resource, "no statement asks principal in resource");
        assert!(
            candidates("permit(principal, action, resource) when { principal in resource };")
                .resource
        );
        let none =
            candidates("permit(principal, action, resource) when { principal.id == \"x\" };");
        assert!(none.literals.is_empty() && !none.resource);
    }
}
