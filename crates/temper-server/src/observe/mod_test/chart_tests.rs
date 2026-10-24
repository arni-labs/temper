use super::*;

// -- What a statechart is drawn from: `says` and `[[group]]` in spec detail --

/// The Order fixture with two labelled actions and nested groups.
fn order_with_chart_keys() -> String {
    let mut spec = ORDER_IOA
        .replacen(
            "name = \"SubmitOrder\"\n",
            "name = \"SubmitOrder\"\nsays = \"send it in\"\n",
            1,
        )
        .replacen(
            "name = \"CancelOrder\"\n",
            "name = \"CancelOrder\"\nsays = \"call it off\"\n",
            1,
        );
    spec.push_str(
        r#"
[[group]]
name = "Live"
says = "while the order is live"
states = ["Open", "On its way"]

[[group]]
name = "Open"
states = ["Draft", "Submitted", "Confirmed"]

[[group]]
name = "On its way"
says = "the order is with the courier"
states = ["Processing", "Shipped", "Delivered"]
"#,
    );
    spec
}

#[tokio::test]
async fn spec_detail_carries_what_actions_say_and_the_groups() {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let spec = order_with_chart_keys();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", spec.as_str())],
    );
    let state = ServerState::from_registry(ActorSystem::new("test-chart"), registry);
    let app = Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state);

    let raw = observe_json(app, "/observe/specs/Order").await;

    let action = |name: &str| {
        raw["actions"]
            .as_array()
            .expect("actions is a list")
            .iter()
            .find(|a| a["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no action {name}"))
    };
    assert_eq!(action("SubmitOrder")["says"], "send it in");
    assert_eq!(action("CancelOrder")["says"], "call it off");
    assert!(
        action("ConfirmOrder")["says"].is_null(),
        "an action that says nothing has no label: {}",
        action("ConfirmOrder")
    );

    assert_eq!(
        raw["groups"],
        serde_json::json!([
            {
                "name": "Live",
                "says": "while the order is live",
                "states": ["Open", "On its way"]
            },
            {
                "name": "Open",
                "states": ["Draft", "Submitted", "Confirmed"]
            },
            {
                "name": "On its way",
                "says": "the order is with the courier",
                "states": ["Processing", "Shipped", "Delivered"]
            }
        ])
    );
    // Group names are not states: the state list is unchanged.
    assert_eq!(raw["states"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn spec_detail_of_a_spec_without_groups_lists_none() {
    let raw = observe_json(build_test_app(), "/observe/specs/Order").await;
    assert_eq!(raw["groups"], serde_json::json!([]));
}
