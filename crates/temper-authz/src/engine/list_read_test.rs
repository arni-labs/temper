//! A list read decided in two parts gives every row the decision the whole
//! policy set gives it.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::context::{Principal, PrincipalKind, SecurityContext};

fn reader(kind: PrincipalKind, role: Option<&str>) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "reader".to_string(),
            kind,
            role: role.map(str::to_string),
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "list-read".to_string(),
        cause: None,
        credential: None,
    }
}

fn row(id: &str, owner: Option<&str>, status: &str) -> HashMap<String, serde_json::Value> {
    let mut attrs = HashMap::from([
        ("id".to_string(), json!(id)),
        ("status".to_string(), json!(status)),
    ]);
    if let Some(owner) = owner {
        attrs.insert("owner".to_string(), json!(owner));
    }
    attrs
}

fn rows() -> Vec<HashMap<String, serde_json::Value>> {
    let mut rows = Vec::new();
    for (index, owner) in [Some("reader"), Some("other"), None]
        .into_iter()
        .enumerate()
    {
        for status in ["Open", "Closed"] {
            rows.push(row(&format!("o{index}{status}"), owner, status));
        }
    }
    rows.push(row("o1", Some("other"), "Open"));
    rows
}

/// Each row's decision through the list read, beside the whole set's.
fn decisions(policy: &str, who: &SecurityContext) -> (Vec<bool>, Vec<bool>, &'static str) {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies("tenant", policy)
        .expect("policy should parse");
    let probe = row("_list", None, "");
    let list = engine.list_read_for_tenant("tenant", who, "read", "Order", &probe);
    let mode = match &list {
        ListRead::AllowAll => "allow_all",
        ListRead::DenyAll => "deny_all",
        ListRead::PerRow(_) => "per_row",
    };
    let mut split = Vec::new();
    let mut whole = Vec::new();
    for attrs in rows() {
        split.push(match &list {
            ListRead::AllowAll => true,
            ListRead::DenyAll => false,
            ListRead::PerRow(rule) => {
                engine.list_row_allowed("tenant", who, "read", "Order", rule, &attrs)
            }
        });
        whole.push(
            engine
                .authorize_row_for_tenant("tenant", who, "read", "Order", &attrs)
                .is_allowed(),
        );
    }
    (split, whole, mode)
}

const POLICIES: &[&str] = &[
    // A list-wide permit and a forbid on a field each row fills.
    r#"permit(principal, action == Action::"read", resource is Order);
       forbid(principal, action == Action::"read", resource is Order)
       when { context has owner && context.owner != principal.id };"#,
    // Only a permit that reads the row.
    r#"permit(principal, action == Action::"read", resource is Order)
       when { resource has owner && resource.owner == principal.id };"#,
    // A list-wide forbid for one role beside a list-wide permit.
    r#"permit(principal, action == Action::"read", resource is Order);
       forbid(principal, action, resource is Order)
       when { principal has role && principal.role like "guest*" };"#,
    // A broad forbid that names the row's id, beside a list-wide permit.
    r#"permit(principal, action in [Action::"read", Action::"list"], resource is Order);
       forbid(principal, action, resource)
       when { principal has role && principal.role == "file-upload" }
       unless { resource has id && resource.id == principal.id };"#,
    // A permit for one row by its id, and one by status.
    r#"permit(principal, action == Action::"read", resource == Order::"o1");
       permit(principal, action == Action::"read", resource is Order)
       when { resource.status == "Open" };"#,
    // No permit at all.
    r#"forbid(principal, action == Action::"read", resource is Order)
       when { resource.status == "Closed" };"#,
    // A row read in a shape the classifier cannot project, beside a forbid on status.
    r#"permit(principal, action == Action::"read", resource is Order)
       when { resource has owner && resource.owner like "re*" };
       permit(principal is Agent, action, resource);
       forbid(principal, action == Action::"read", resource is Order)
       when { context has status && context.status == "Closed" };"#,
    // Policies for other types and actions only.
    r#"permit(principal, action == Action::"read", resource is Invoice);
       permit(principal, action == Action::"Pay", resource is Order);"#,
];

#[test]
fn every_row_gets_the_whole_sets_decision() {
    let readers = [
        reader(PrincipalKind::Customer, None),
        reader(PrincipalKind::Customer, Some("guest:link")),
        reader(PrincipalKind::Agent, Some("file-upload")),
        reader(PrincipalKind::Agent, None),
    ];
    for policy in POLICIES {
        for who in &readers {
            let (split, whole, mode) = decisions(policy, who);
            assert_eq!(
                split, whole,
                "{mode} differs for {:?} under:\n{policy}",
                who.principal
            );
        }
    }
}

/// The cases above cover all three outcomes, so a split that always fell
/// back to one of them would not pass for the right reason.
#[test]
fn the_cases_reach_every_outcome() {
    let customer = reader(PrincipalKind::Customer, None);
    let guest = reader(PrincipalKind::Customer, Some("guest:link"));
    assert_eq!(decisions(POLICIES[0], &customer).2, "per_row");
    assert_eq!(decisions(POLICIES[2], &customer).2, "allow_all");
    assert_eq!(decisions(POLICIES[2], &guest).2, "deny_all");
    assert_eq!(decisions(POLICIES[5], &customer).2, "deny_all");
}

/// A row is checked only against the policies that can tell rows apart.
#[test]
fn a_row_evaluates_only_the_policies_it_can_change() {
    let engine = AuthzEngine::empty();
    let mut policy = String::new();
    for index in 0..50 {
        policy.push_str(&format!(
            "permit(principal, action == Action::\"read\", resource is Order) when {{ principal.id == \"reader{index}\" || principal.id == \"reader\" }};\n"
        ));
    }
    policy.push_str(
        r#"forbid(principal, action == Action::"read", resource is Order)
           when { resource has owner && resource.owner != principal.id };"#,
    );
    engine
        .reload_tenant_policies("tenant", &policy)
        .expect("policy should parse");
    let who = reader(PrincipalKind::Customer, None);
    let probe = row("_list", None, "");
    let ListRead::PerRow(rule) =
        engine.list_read_for_tenant("tenant", &who, "read", "Order", &probe)
    else {
        panic!("a forbid on the owner tells rows apart");
    };
    // The 50 permits read only the principal, so they are decided once.
    assert_eq!(rule.row_policy_count, 1);
    assert!(engine.list_row_allowed(
        "tenant",
        &who,
        "read",
        "Order",
        &rule,
        &row("a", Some("reader"), "Open")
    ));
    assert!(!engine.list_row_allowed(
        "tenant",
        &who,
        "read",
        "Order",
        &rule,
        &row("b", Some("other"), "Open")
    ));
}
