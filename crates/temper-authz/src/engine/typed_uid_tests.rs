//! Request entities are matched exactly as the caller names them.
use super::*;

const TENANT: &str = "typed-uid";

fn engine(policy: &str) -> AuthzEngine {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies(TENANT, policy)
        .expect("test policy loads");
    engine
}

fn decide(engine: &AuthzEngine, action: &str, resource_type: &str, id: &str) -> AuthzDecision {
    let context = SecurityContext::from_resolved_identity("agent-a", "worker", None);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!(id));
    engine.authorize_for_tenant(TENANT, &context, action, resource_type, &attrs)
}

#[test]
fn a_resource_id_is_matched_as_written() {
    let engine = engine(
        r#"permit(principal == Agent::"agent-a", action == Action::"read", resource == Order::"A");"#,
    );
    assert!(decide(&engine, "read", "Order", "A").is_allowed());
    let escaped = decide(&engine, "read", "Order", r"\u{41}");
    assert!(!escaped.is_allowed(), "{escaped:?}");
}

#[test]
fn an_action_name_with_a_quote_is_its_own_action() {
    let engine = engine(
        r#"permit(principal == Agent::"agent-a", action == Action::"say \"hi\"", resource is Order);"#,
    );
    let quoted = decide(&engine, "say \"hi\"", "Order", "o-1");
    assert!(quoted.is_allowed(), "{quoted:?}");
    assert!(!decide(&engine, "say", "Order", "o-1").is_allowed());
}

#[test]
fn a_resource_type_that_is_not_a_type_name_is_denied() {
    let engine = engine(r#"permit(principal == Agent::"agent-a", action, resource);"#);
    let decision = decide(&engine, "read", "Order Item", "o-1");
    assert!(
        matches!(
            decision,
            AuthzDecision::Deny(AuthzDenial::InvalidResource(_))
        ),
        "{decision:?}"
    );
}
