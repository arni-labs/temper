use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_state_timers_move_is_recorded_as_the_timers_with_the_arming_move_as_its_cause() {
    let state = build_state(GATE_IOA);
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g1"}"#, "member").await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g1')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");

    let (status, passed_by, events) = settle(&state, "g1", Duration::from_secs(6)).await;
    assert_eq!(status, "Passed", "the state timer did not move the gate on");
    assert_eq!(
        passed_by, "member",
        "the timer's move runs with the arming move's authority"
    );

    let add = events
        .iter()
        .find(|event| event.action == "Add")
        .expect("the history keeps the member's Add");
    assert_eq!(
        add.origin, None,
        "a caller's own move names no runtime actor or cause"
    );

    let pass = events.last().expect("the history keeps the timer's move");
    assert_eq!(pass.action, "Pass");
    let origin = pass
        .origin
        .as_ref()
        .expect("the timer's move records who made it and what led to it");
    assert_eq!(
        origin.actor, STATE_TIMER_ACTOR,
        "the history says the state timer made the move"
    );
    let cause = &origin.caused_by;
    assert_eq!(cause.agent_id.as_deref(), Some("member"));
    assert_eq!(cause.action.as_deref(), Some("Add"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_state_timers_move_calls_out_as_the_arming_agent_and_its_follow_ons_name_no_timer() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/notify"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "ok" } })),
        )
        .mount(&service)
        .await;
    let state = build_state(&notifying_gate_ioa(&service.uri()));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g2"}"#, "member").await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g2')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");

    let (status, _, events) = settle(&state, "g2", Duration::from_secs(8)).await;
    assert_eq!(
        status, "Passed",
        "the timer's move or its callback did not run"
    );

    let calls = service.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1, "the timer's move calls the service once");
    let call: serde_json::Value =
        serde_json::from_slice(&calls[0].body).expect("the adapter sends JSON");
    assert_eq!(
        call["agent_ctx"]["agent_id"], "member",
        "the timer's move calls out as the agent whose move armed it: {call}"
    );

    let pass = events
        .iter()
        .find(|event| event.action == "Pass")
        .expect("the history keeps the timer's move");
    let origin = pass
        .origin
        .as_ref()
        .expect("the timer's move names its maker");
    assert_eq!(origin.actor, STATE_TIMER_ACTOR);
    assert_eq!(origin.caused_by.action.as_deref(), Some("Add"));
    let notified = events
        .iter()
        .find(|event| event.action == "Notified")
        .expect("the history keeps the service's answer");
    assert_eq!(
        notified.origin, None,
        "the move the service's answer makes is its own, not the timer's"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_to_a_state_timers_move_is_recorded_as_its_own() {
    let state = build_tenant(
        BELL_CSDL_XML,
        &[("Gate", RINGING_GATE_IOA), ("Bell", BELL_IOA)],
        RINGING_POLICY,
    );
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g6"}"#, "member").await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g6')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");

    let (status, _, events) = settle(&state, "g6", Duration::from_secs(6)).await;
    assert_eq!(status, "Passed", "the state timer did not move the gate on");
    let pass = events
        .iter()
        .find(|event| event.action == "Pass")
        .expect("the history keeps the timer's move");
    assert_eq!(
        pass.origin.as_ref().map(|origin| origin.actor.as_str()),
        Some(STATE_TIMER_ACTOR),
        "the gate's pass is the state timer's"
    );

    // The reaction runs after the gate's pass commits.
    let deadline = Instant::now() + Duration::from_secs(4); // determinism-ok: real-time timer test
    let bell = loop {
        let bell = state
            .get_tenant_entity_state(&TenantId::default(), "Bell", "g6")
            .await
            .expect("the reaction reaches the bell");
        let now = Instant::now(); // determinism-ok: real-time timer test
        if bell.state.status == "Rung" || now >= deadline {
            break bell;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time timer test
    };
    assert_eq!(
        bell.state.status, "Rung",
        "the gate's pass did not ring the bell"
    );
    let ring = bell
        .state
        .events
        .iter()
        .find(|event| event.action == "Ring")
        .expect("the bell's history keeps the reaction's move");
    assert_eq!(
        ring.origin, None,
        "the reaction's move is its own: it names neither the state timer nor the gate's cause"
    );
}
