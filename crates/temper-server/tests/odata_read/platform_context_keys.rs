//! A record's fields reach Cedar as `context.<field>` unless the request
//! fills that key. The platform fills some keys itself on a move: the module
//! making the call, and the trigger that made it. A record field of the same
//! name must not stand in for one of them on a move the platform did not
//! fill it for: a caller who can write that field would pass a policy meant
//! only for the platform's own moves.

use super::*;

/// Keys the platform sets on a move's context, with a value a policy would
/// trust: `context.module == "steps"` lets the steps module's calls make a
/// move, and the trigger keys let a trigger's declared principal make one.
fn platform_keys() -> [(&'static str, serde_json::Value); 4] {
    [
        ("module", serde_json::json!("steps")),
        ("triggerDeclaredPrincipal", serde_json::json!(true)),
        ("triggerRule", serde_json::json!("close-parent")),
        ("triggerSourceAction", serde_json::json!("Leave")),
    ]
}

/// A user cancels their own order under a policy that lets only a call
/// holding `key` cancel one: refused whatever the order holds.
async fn cancel_as_user(key: &str, value: &serde_json::Value, order_holds: bool) -> StatusCode {
    let (state, _store) = build_default_state(77, &format!("platform-key-{key}-{order_holds}"));
    let tenant = TenantId::default();
    let id = "ord-platform-key";
    let fields = if order_holds {
        serde_json::json!({ "Id": id, key: value })
    } else {
        serde_json::json!({ "Id": id })
    };
    let created = state
        .get_or_create_tenant_entity(&tenant, "Order", id, fields)
        .await
        .expect("create the order");
    assert!(created.success, "create: {:?}", created.error);
    if order_holds {
        assert_eq!(&created.state.fields[key], value, "the order holds {key}");
    }
    let policy = format!(
        r#"permit(principal, action == Action::"read", resource is Order);
           permit(principal, action == Action::"CancelOrder", resource is Order)
           when {{ context has {key} && context.{key} == {value} }};"#
    );
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), &policy)
        .expect("install the policy");
    let (status, _) = customer_json(
        &state,
        Method::POST,
        &format!("/tdata/Orders('{id}')/Temper.CancelOrder"),
        Some(serde_json::json!({ "Reason": "changed my mind" })),
    )
    .await;
    status
}

#[tokio::test]
async fn a_record_field_does_not_stand_in_for_a_platform_context_key() {
    let mut passed = Vec::new();
    for (key, value) in platform_keys() {
        // Control: without the field, the policy refuses the user.
        assert_eq!(
            cancel_as_user(key, &value, false).await,
            StatusCode::FORBIDDEN,
            "{key}: the policy must refuse a call that does not hold it"
        );
        let status = cancel_as_user(key, &value, true).await;
        if status != StatusCode::FORBIDDEN {
            passed.push(format!("{key}: {status}"));
        }
    }
    assert!(
        passed.is_empty(),
        "a record field passed for a platform context key: {passed:?}"
    );
}
