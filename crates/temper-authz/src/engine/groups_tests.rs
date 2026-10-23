//! `principal in` decided by `GroupMembership` records (ADR-0183).

use super::*;
use crate::context::{PrincipalKind, SecurityContext};
use crate::groups::{MembershipRecord, group_membership_id};

const APP: &str = "KatagamiApp::\"a1\"";
const POLICY: &str = r#"
permit(principal, action == Action::"read", resource is KatagamiApp) when { principal in resource };
permit(principal in Team::"ops", action == Action::"Pin", resource);
"#;

fn engine() -> AuthzEngine {
    let engine = AuthzEngine::empty();
    engine.reload_tenant_policies("t", POLICY).unwrap();
    engine
}

fn join(engine: &AuthzEngine, group: &str, member: &str, active: bool, seq: u64) {
    let id = group_membership_id(group, member).unwrap();
    engine
        .group_memberships()
        .apply(&MembershipRecord {
            tenant: "t",
            record_id: &id,
            group,
            member,
            active,
            sequence_nr: seq,
        })
        .unwrap();
}

fn person(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(id, PrincipalKind::Customer, None, None, None, None)
}

fn allowed(engine: &AuthzEngine, who: &SecurityContext, action: &str, ty: &str, id: &str) -> bool {
    let attrs = HashMap::from([("id".to_string(), serde_json::json!(id))]);
    engine
        .authorize_for_tenant("t", who, action, ty, &attrs)
        .is_allowed()
}

#[test]
fn a_member_of_the_resource_reads_it_and_nobody_else_does() {
    let engine = engine();
    engine.group_memberships().mark_filled("t");
    join(&engine, APP, "alice", true, 1);
    assert!(allowed(
        &engine,
        &person("alice"),
        "read",
        "KatagamiApp",
        "a1"
    ));
    assert!(
        !allowed(&engine, &person("bob"), "read", "KatagamiApp", "a1"),
        "not a member"
    );
    assert!(
        !allowed(&engine, &person("alice"), "read", "KatagamiApp", "a2"),
        "a member of another app only"
    );
    join(&engine, APP, "alice", false, 2);
    assert!(
        !allowed(&engine, &person("alice"), "read", "KatagamiApp", "a1"),
        "her membership ended"
    );
}

#[test]
fn an_agent_is_in_the_groups_of_the_person_it_acts_for() {
    let engine = engine();
    engine.group_memberships().mark_filled("t");
    join(&engine, APP, "alice", true, 1);
    let for_alice = SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude"),
        Some("alice"),
        None,
        None,
    );
    let for_bob = SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude"),
        Some("bob"),
        None,
        None,
    );
    assert!(allowed(&engine, &for_alice, "read", "KatagamiApp", "a1"));
    assert!(!allowed(&engine, &for_bob, "read", "KatagamiApp", "a1"));
}

#[test]
fn a_group_named_in_the_scope_is_found_too() {
    let engine = engine();
    engine.group_memberships().mark_filled("t");
    join(&engine, "Team::\"ops\"", "carol", true, 1);
    assert!(allowed(&engine, &person("carol"), "Pin", "Note", "n1"));
    assert!(!allowed(&engine, &person("alice"), "Pin", "Note", "n1"));
}

#[test]
fn a_tenant_whose_memberships_are_not_read_yet_lets_nobody_in_by_one() {
    let engine = engine();
    join(&engine, APP, "alice", true, 1);
    assert!(
        !allowed(&engine, &person("alice"), "read", "KatagamiApp", "a1"),
        "fail closed until filled"
    );
    engine.group_memberships().mark_filled("t");
    assert!(allowed(
        &engine,
        &person("alice"),
        "read",
        "KatagamiApp",
        "a1"
    ));
}
