use super::*;

/// The observe API's history response for a gate, read as the system.
async fn history_response(state: &ServerState, id: &str) -> (StatusCode, serde_json::Value) {
    let mut request = Request::get(format!("/observe/entities/Gate/{id}/history"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

/// The record's history from the observe API, read as the system.
async fn history(state: &ServerState, id: &str) -> Vec<serde_json::Value> {
    let (status, json) = history_response(state, id).await;
    assert_eq!(status, StatusCode::OK);
    json["events"].as_array().cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timers_cause_names_the_history_entry_of_the_arming_move() {
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(80), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g3"}"#, "member").await,
        StatusCode::CREATED
    );
    // More moves than the record keeps in memory, so its oldest events are
    // only in the journal when the timer fires.
    let kept_in_memory = temper_server::entity_actor::types::recent_events_budget();
    for _ in 0..kept_in_memory + 10 {
        let noted = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Gate",
                "g3",
                "Note",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("a note is recorded");
        assert!(noted.success, "{:?}", noted.error);
    }
    let added = post(&state, "/tdata/Gates('g3')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");
    let (status, _, _) = settle(&state, "g3", Duration::from_secs(6)).await;
    assert_eq!(status, "Passed", "the state timer did not move the gate on");

    let events = history(&state, "g3").await;
    let pass = events
        .iter()
        .find(|event| event["action"] == "Pass")
        .expect("the history shows the timer's move");
    let cause = &pass["caused_by"]["sequence_nr"];
    let named = events
        .iter()
        .find(|event| &event["sequence"] == cause)
        .unwrap_or_else(|| panic!("no history entry has the cause's sequence {cause}"));
    assert_eq!(
        named["action"], "Add",
        "the timer's cause names the arming move's entry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loaded_records_history_shows_the_same_point_in_time_as_its_state() {
    let journal = SimEventStore::no_faults(81);
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(journal.clone(), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g4"}"#, "member").await,
        StatusCode::CREATED
    );

    // The history request reads the actor's state, then the journal. A move
    // commits between those two reads.
    let mut held = journal.hold_next_read("default:Gate:g4");
    let reader = tokio::spawn({
        let state = state.clone();
        async move { history_response(&state, "g4").await }
    });
    held.reached().await;
    let added = post(&state, "/tdata/Gates('g4')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");
    held.release();

    let (status, body) = reader.await.expect("the history request finishes");
    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().cloned().unwrap_or_default();
    let last = events
        .last()
        .expect("the history shows the record's Created");
    assert_eq!(
        last["to_state"], body["current_state"],
        "the history's newest move must end in the state the same response shows: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_history_whose_journal_cannot_be_read_says_so() {
    let journal = SimEventStore::no_faults(82);
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(journal.clone(), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g5"}"#, "member").await,
        StatusCode::CREATED
    );

    journal.fail_next_reads("default:Gate:g5", 1);
    let (status, body) = history_response(&state, "g5").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a history that could not read the journal must not number its events itself: {body}"
    );
}
