//! Decide whether a list read's row decision is the same for every entity.
//!
//! A collection read otherwise evaluates Cedar once per row and emits a span
//! per evaluation. When every policy that can match this principal, action,
//! and entity type ignores per-row state, one evaluation is the whole answer.
//! A policy that names an entity id, a parent, or a resource attribute stays
//! on the per-row path so a denied row is still removed.

use std::collections::BTreeSet;
use std::str::FromStr;

use cedar_policy::{
    ActionConstraint, EntityTypeName, EntityUid, Policy, PolicySet, PrincipalConstraint,
    ResourceConstraint,
};

use crate::context::{PrincipalKind, SecurityContext};

use super::typed_uid;

/// How a list read may share one Cedar decision across rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListReadUniformity {
    /// Every row of this entity type receives the same read decision.
    Uniform,
    /// Rows can differ.
    ///
    /// `attribute_keys` lists the resource attributes the applicable policies
    /// read, excluding `has_spec` (constant for the type) and the entity id.
    /// `None` means the policy shape was not recognized; the caller must
    /// evaluate each full resource.
    PerEntity {
        /// Resource attributes that can change the decision from row to row.
        attribute_keys: Option<BTreeSet<String>>,
    },
}

/// Classify the policies that can match this list read.
pub(super) fn classify(
    policies: &PolicySet,
    security_ctx: &SecurityContext,
    action: &str,
    resource_type: &str,
) -> ListReadUniformity {
    let Some(principal_uid) = principal_uid(security_ctx) else {
        return per_entity_unknown();
    };
    let Ok(action_uid) = typed_uid("Action", action) else {
        return per_entity_unknown();
    };
    let Ok(resource_type_name) = EntityTypeName::from_str(resource_type) else {
        return per_entity_unknown();
    };

    let mut varies = false;
    let mut keys = Some(BTreeSet::new());
    for policy in policies.policies() {
        if !policy_might_apply(policy, &principal_uid, &action_uid, &resource_type_name) {
            continue;
        }
        if !policy.is_static() {
            return per_entity_unknown();
        }
        match row_variance(policy, &resource_type_name) {
            RowVariance::Uniform => {}
            RowVariance::Varies(policy_keys) => {
                varies = true;
                match policy_keys {
                    Some(policy_keys) => {
                        if let Some(keys) = keys.as_mut() {
                            keys.extend(policy_keys);
                        }
                    }
                    None => keys = None,
                }
            }
            RowVariance::Unknown => return per_entity_unknown(),
        }
    }

    if varies {
        ListReadUniformity::PerEntity {
            attribute_keys: keys,
        }
    } else {
        ListReadUniformity::Uniform
    }
}

fn per_entity_unknown() -> ListReadUniformity {
    ListReadUniformity::PerEntity {
        attribute_keys: None,
    }
}

enum RowVariance {
    Uniform,
    Varies(Option<BTreeSet<String>>),
    Unknown,
}

fn row_variance(policy: &Policy, resource_type: &EntityTypeName) -> RowVariance {
    let scope_varies = match policy.resource_constraint() {
        ResourceConstraint::Any => false,
        ResourceConstraint::Is(expected) => {
            if &expected != resource_type {
                return RowVariance::Uniform;
            }
            false
        }
        ResourceConstraint::Eq(expected) => {
            if expected.type_name() != resource_type {
                return RowVariance::Uniform;
            }
            true
        }
        ResourceConstraint::In(_) => true,
        ResourceConstraint::IsIn(expected, _) => {
            if &expected != resource_type {
                return RowVariance::Uniform;
            }
            true
        }
    };

    let json = match policy.to_json() {
        Ok(json) => json,
        Err(_) => return RowVariance::Unknown,
    };
    let mut scan = AttrScan::default();
    if let Some(conditions) = json.get("conditions").and_then(|value| value.as_array()) {
        for condition in conditions {
            let body = condition.get("body").unwrap_or(condition);
            scan.merge(collect_resource_attrs(body));
        }
    } else if json.get("conditions").is_some() {
        return RowVariance::Unknown;
    }

    if scan.unknown {
        return RowVariance::Unknown;
    }
    if scope_varies || scan.identity || !scan.keys.is_empty() {
        RowVariance::Varies(Some(scan.keys))
    } else {
        RowVariance::Uniform
    }
}

#[derive(Default)]
struct AttrScan {
    keys: BTreeSet<String>,
    /// The condition names the row's id (`resource.id`, `resource.Id`, or `resource.entity_id`).
    identity: bool,
    /// The condition reads the resource in a shape this walk does not project.
    unknown: bool,
}

impl AttrScan {
    fn merge(&mut self, other: Self) {
        self.keys.extend(other.keys);
        self.identity |= other.identity;
        self.unknown |= other.unknown;
    }
}

fn collect_resource_attrs(value: &serde_json::Value) -> AttrScan {
    let mut scan = AttrScan::default();
    collect_resource_attrs_into(value, &mut scan);
    scan
}

fn collect_resource_attrs_into(value: &serde_json::Value, scan: &mut AttrScan) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_resource_attrs_into(item, scan);
            }
        }
        serde_json::Value::Object(map) => {
            if json_var_is(value, "resource") {
                // A bare `resource` (entity equality, a function argument, …)
                // can depend on more than the id. Keep the full document.
                scan.unknown = true;
            }
            if let Some(node) = map.get(".").or_else(|| map.get("has")) {
                match node.get("attr").and_then(|attr| attr.as_str()) {
                    Some(attr) => {
                        let target = node.get("expr").or_else(|| node.get("left"));
                        classify_getattr(attr, target, scan);
                    }
                    None => scan.unknown = true,
                }
                // The getattr target is classified above. Walk the rest of
                // that node (the attribute expression can itself be a call)
                // without treating its `resource` variable as a bare use.
                if let Some(node_map) = node.as_object() {
                    for (key, child) in node_map {
                        if key == "expr" || key == "left" {
                            collect_except_resource_var(child, scan);
                        } else {
                            collect_resource_attrs_into(child, scan);
                        }
                    }
                }
            }
            for (key, child) in map {
                if key == "." || key == "has" {
                    continue;
                }
                collect_resource_attrs_into(child, scan);
            }
        }
        _ => {}
    }
}

/// Walk a getattr target, ignoring the `resource`/`context` variable that
/// the getattr itself already classified.
fn collect_except_resource_var(value: &serde_json::Value, scan: &mut AttrScan) {
    if json_var_is(value, "resource") || json_var_is(value, "context") {
        return;
    }
    collect_resource_attrs_into(value, scan);
}

fn classify_getattr(attr: &str, target: Option<&serde_json::Value>, scan: &mut AttrScan) {
    let Some(target) = target else {
        scan.unknown = true;
        return;
    };
    if json_var_is(target, "resource") {
        if matches!(attr, "id" | "Id" | "entity_id") {
            // The id is already on every row. It still makes the decision
            // per-entity, and it is not a catalog JSON field to project.
            scan.identity = true;
        } else if attr != "has_spec" {
            scan.keys.insert(attr.to_string());
        }
        return;
    }
    if json_var_is(target, "context") {
        // Request context does not change between rows. Authority keys and
        // other context attributes are both decided by the single probe.
        return;
    }
    if json_contains_var(target, "resource") {
        // `resource.owner.name` and other nested reads are not a field list
        // this walker can project safely.
        scan.unknown = true;
    }
}

fn json_var_is(value: &serde_json::Value, name: &str) -> bool {
    value.get("Var").and_then(|var| var.as_str()) == Some(name)
}

fn json_contains_var(value: &serde_json::Value, name: &str) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(|item| json_contains_var(item, name)),
        serde_json::Value::Object(map) => {
            json_var_is(value, name) || map.values().any(|child| json_contains_var(child, name))
        }
        _ => false,
    }
}

fn policy_might_apply(
    policy: &Policy,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_type: &EntityTypeName,
) -> bool {
    if !policy.is_static() {
        return true;
    }
    principal_might_match(policy.principal_constraint(), principal_uid)
        && action_might_match(policy.action_constraint(), action_uid)
        && resource_type_might_match(policy.resource_constraint(), resource_type)
}

fn principal_might_match(constraint: PrincipalConstraint, principal_uid: &EntityUid) -> bool {
    match constraint {
        PrincipalConstraint::Any | PrincipalConstraint::In(_) => true,
        PrincipalConstraint::Eq(expected) => expected == *principal_uid,
        PrincipalConstraint::Is(expected) => principal_uid.type_name() == &expected,
        PrincipalConstraint::IsIn(expected, _) => principal_uid.type_name() == &expected,
    }
}

fn action_might_match(constraint: ActionConstraint, action_uid: &EntityUid) -> bool {
    match constraint {
        ActionConstraint::Any => true,
        ActionConstraint::Eq(expected) => expected == *action_uid,
        ActionConstraint::In(expected) => expected.iter().any(|candidate| candidate == action_uid),
    }
}

fn resource_type_might_match(
    constraint: ResourceConstraint,
    resource_type: &EntityTypeName,
) -> bool {
    match constraint {
        ResourceConstraint::Any | ResourceConstraint::In(_) => true,
        ResourceConstraint::Is(expected) | ResourceConstraint::IsIn(expected, _) => {
            &expected == resource_type
        }
        ResourceConstraint::Eq(expected) => expected.type_name() == resource_type,
    }
}

fn principal_uid(security_ctx: &SecurityContext) -> Option<EntityUid> {
    let kind = match security_ctx.principal.kind {
        PrincipalKind::Customer => "Customer",
        PrincipalKind::Agent => "Agent",
        PrincipalKind::Admin => "Admin",
        PrincipalKind::System => "System",
    };
    typed_uid(kind, &security_ctx.principal.id).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::context::{Principal, SecurityContext};
    use crate::engine::AuthzEngine;

    fn customer() -> SecurityContext {
        SecurityContext {
            principal: Principal {
                id: "reader".to_string(),
                kind: PrincipalKind::Customer,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: HashMap::new(),
            },
            context_attrs: HashMap::new(),
            correlation_id: "list-uniformity".to_string(),
        }
    }

    fn classify_for(policy: &str, resource_type: &str) -> ListReadUniformity {
        let engine = AuthzEngine::empty();
        engine
            .reload_tenant_policies("tenant", policy)
            .expect("policy should parse");
        engine.list_read_uniformity("tenant", &customer(), "read", resource_type)
    }

    #[test]
    fn type_level_read_permit_is_uniform() {
        let decision = classify_for(
            r#"permit(principal, action == Action::"read", resource is DesignLanguage);"#,
            "DesignLanguage",
        );
        assert_eq!(decision, ListReadUniformity::Uniform);
    }

    #[test]
    fn has_spec_condition_stays_uniform() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is Order)
            when { resource.has_spec == true };
            "#,
            "Order",
        );
        assert_eq!(decision, ListReadUniformity::Uniform);
    }

    #[test]
    fn status_condition_is_per_entity() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is DesignLanguage)
            when { resource.status == "published" };
            "#,
            "DesignLanguage",
        );
        match decision {
            ListReadUniformity::PerEntity {
                attribute_keys: Some(keys),
            } => {
                assert!(keys.contains("status"), "keys={keys:?}");
                assert!(!keys.contains("has_spec"));
            }
            other => panic!("expected per-entity status, got {other:?}"),
        }
    }

    #[test]
    fn resource_id_condition_is_per_entity_without_json_fields() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is Order)
            when { resource.id == "ord-01" };
            "#,
            "Order",
        );
        match decision {
            ListReadUniformity::PerEntity {
                attribute_keys: Some(keys),
            } => assert!(
                keys.is_empty(),
                "resource.id is the row id, not a catalog JSON field: {keys:?}"
            ),
            other => panic!("expected per-entity id condition, got {other:?}"),
        }
    }

    #[test]
    fn nested_resource_attribute_is_not_projected() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is Order)
            when { resource.owner.name == "ada" };
            "#,
            "Order",
        );
        assert_eq!(
            decision,
            ListReadUniformity::PerEntity {
                attribute_keys: None
            }
        );
    }

    #[test]
    fn entity_id_forbid_is_per_entity_without_loading_json_fields() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is Order);
            forbid(principal, action == Action::"read", resource == Order::"ord-01");
            "#,
            "Order",
        );
        match decision {
            ListReadUniformity::PerEntity {
                attribute_keys: Some(keys),
            } => assert!(
                keys.is_empty(),
                "an id forbid needs the entity id, not catalog JSON: {keys:?}"
            ),
            other => panic!("expected per-entity id forbid, got {other:?}"),
        }
    }

    #[test]
    fn unrelated_type_and_action_do_not_force_per_row_reads() {
        let decision = classify_for(
            r#"
            permit(principal, action == Action::"read", resource is Order);
            forbid(principal, action == Action::"read", resource is TrustedIssuer);
            forbid(principal, action == Action::"delete", resource == Order::"ord-01");
            "#,
            "Order",
        );
        assert_eq!(decision, ListReadUniformity::Uniform);
    }
}
