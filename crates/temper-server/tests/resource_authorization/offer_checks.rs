use super::*;

fn register_governance(state: &ServerState) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let csdl = include_str!("../../../temper-platform/src/specs/model.csdl.xml");
    let ioa = include_str!("../../../temper-platform/src/specs/GovernanceDecision.ioa.toml");
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        "temper-system",
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.to_string(),
        &[("GovernanceDecision", ioa)],
    );
    registry.set_verification_status(
        &TenantId::new("temper-system"),
        "GovernanceDecision",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-26T00:00:00Z".to_string(),
        }),
    );
}

async fn answer(app: &axum::Router, body: serde_json::Value, tenant: &str) -> serde_json::Value {
    let mut request = json_request(Method::POST, "/api/authorize", body, tenant, "agent-1");
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security,
        ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// `order-1`, to which only a person may add an item; an agent may not even read it.
async fn order_only_a_person_may_add_to(state: &ServerState) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(
            TENANT,
            temper_spec::csdl::parse_csdl(csdl).unwrap(),
            csdl.to_string(),
            &[("Order", ioa)],
        );
        registry.set_verification_status(
            &TenantId::new(TENANT),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-10-26T00:00:00Z".to_string(),
            }),
        );
    }
    state
        .get_or_create_tenant_entity(
            &TenantId::new(TENANT),
            "Order",
            "order-1",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(principal is Customer, action == Action::"AddItem", resource is Order);"#,
        )
        .unwrap();
}

/// `request` as agent-1 inside an interactive session, the case that files decisions.
fn as_agent_in_session(mut request: Request<Body>) -> Request<Body> {
    request.headers_mut().insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("session-1"),
    );
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(TenantId::new(TENANT), security)
            .with_session_id(Some("session-1".to_string())),
    );
    request
}

/// Pending decisions, GovernanceDecision records and denial patterns filed so far.
async fn filed(state: &ServerState, store: &TursoEventStore) -> (usize, usize, usize) {
    (
        store.query_all_decisions(None).await.unwrap().len(),
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .len(),
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .len(),
    )
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .expect("request should build")
}

/// The decision a refused request answered with, read from its 403 body.
async fn refused_with(response: axum::response::Response) -> String {
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    body["decision_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a refused request offers a decision: {body}"))
        .to_string()
}

/// An agent in a session asks 20 times whether it may add an item, which files
/// nothing. It then reads the record 20 times and is refused each time: the
/// first refusal files one decision and every retry gets that same decision
/// back. The move it then sends and is refused files one more.
#[tokio::test]
async fn questions_file_nothing_and_a_retried_refusal_reuses_its_waiting_decision() {
    let (state, store, _temp) = state_with_turso("retry-reuses-decision").await;
    register_governance(&state);
    order_only_a_person_may_add_to(&state).await;
    let app = build_router(state.clone());
    for _ in 0..20 {
        let offer = app
            .clone()
            .oneshot(as_agent_in_session(json_request(
                Method::POST,
                "/api/authorize",
                serde_json::json!({
                    "agent_id": "agent-1", "action": "AddItem", "resource_type": "Order",
                    "resource_id": "order-1", "context": {"status": "Draft"}
                }),
                TENANT,
                "agent-1",
            )))
            .await
            .unwrap();
        assert_eq!(offer.status(), StatusCode::OK);
    }
    assert_eq!(
        filed(&state, &store).await,
        (0, 0, 0),
        "a question filed a decision"
    );

    let mut read_decisions = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let read = app
            .clone()
            .oneshot(as_agent_in_session(get("/tdata/Orders('order-1')")))
            .await
            .unwrap();
        read_decisions.insert(refused_with(read).await);
    }
    assert_eq!(
        read_decisions.len(),
        1,
        "every retry answers with the waiting decision"
    );
    assert_eq!(
        filed(&state, &store).await,
        (1, 1, 1),
        "20 refused reads of one record file one decision"
    );

    let refused = app
        .oneshot(as_agent_in_session(json_request(
            Method::POST,
            "/tdata/Orders('order-1')/Temper.AddItem",
            serde_json::json!({"ProductId": "p1", "Quantity": 1}),
            TENANT,
            "agent-1",
        )))
        .await
        .unwrap();
    let move_decision = refused_with(refused).await;
    assert!(!read_decisions.contains(&move_decision));
    assert_eq!(
        filed(&state, &store).await,
        (2, 2, 2),
        "a refused move is its own decision"
    );
}

#[tokio::test]
async fn factory_sized_offer_batch_matches_single_checks_without_filing_decisions() {
    let (state, store, _temp) = state_with_turso("offer-batch").await;
    register_governance(&state);
    // A record with 100 moves: agents may make the even moves, only a person may make the odd ones.
    let policy = (0..100).map(|i| format!(
        "permit(principal is {}, action == Action::\"Move{i}\", resource is Release) when {{ resource.owner == \"person-1\" }};",
        if i % 2 == 0 { "Agent" } else { "Customer" }
    )).collect::<Vec<_>>().join("\n");
    state.authz.reload_tenant_policies(TENANT, &policy).unwrap();
    state.authz.reload_tenant_policies("other", "").unwrap();
    let app = build_router(state.clone());
    let actions = (0..100).map(|i| format!("Move{i}")).collect::<Vec<_>>();
    let context = serde_json::json!({"owner": "person-1", "id": "spoofed"});
    let batch = answer(&app, serde_json::json!({
        "agent_id": "agent-1", "records": [
            {"resource_type": "Release", "resource_id": "release-1", "actions": actions, "context": context},
            {"resource_type": "Release", "resource_id": "release-2", "actions": ["Move0"], "context": {"owner":"other"}}
        ]
    }), TENANT).await;
    let records = batch["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["resource_id"], "release-1");
    let offers = records[0]["actions"].as_array().unwrap();
    assert_eq!(offers.len(), 100);
    for (i, offer) in offers.iter().enumerate() {
        let single = answer(
            &app,
            serde_json::json!({
                "agent_id":"agent-1", "resource_type":"Release", "resource_id":"release-1",
                "action":actions[i], "context":context
            }),
            TENANT,
        )
        .await;
        assert_eq!(offer["action"], actions[i]);
        assert_eq!(offer["allowed"], i % 2 == 0);
        assert_eq!(offer["allowed"], single["allowed"]);
        assert_eq!(offer["reason"], single["reason"]);
        assert!(offer.get("decision_id").is_none());
        if i % 2 != 0 {
            assert!(offer["reason"].as_str().is_some_and(|r| !r.is_empty()));
        }
    }
    assert_eq!(records[1]["actions"][0]["allowed"], false);
    let other = answer(&app, serde_json::json!({"agent_id":"agent-1", "records":[{
        "resource_type":"Release", "resource_id":"release-1", "actions":["Move0"], "context":context
    }]}), "other").await;
    assert_eq!(other["records"][0]["actions"][0]["allowed"], false);
    assert!(store.query_all_decisions(None).await.unwrap().is_empty());
    assert!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .is_empty()
    );
}

#[tokio::test]
async fn refused_dispatch_still_files_exactly_one_decision_after_offer_checks() {
    let (state, store, _temp) = state_with_turso("offer-then-dispatch").await;
    register_governance(&state);
    order_only_a_person_may_add_to(&state).await;
    let app = build_router(state.clone());
    let offers = answer(&app, serde_json::json!({"agent_id":"agent-1", "records":[{
        "resource_type":"Order", "resource_id":"order-1", "actions":["AddItem"], "context":{"status":"Draft"}
    }]}), TENANT).await;
    assert_eq!(offers["records"][0]["actions"][0]["allowed"], false);
    assert!(
        store
            .query_decisions(TENANT, None)
            .await
            .unwrap()
            .is_empty()
    );
    let mut request = json_request(
        Method::POST,
        "/tdata/Orders('order-1')/Temper.AddItem",
        serde_json::json!({"ProductId":"p1", "Quantity":1}),
        TENANT,
        "agent-1",
    );
    let mut security = customer_security_context("agent-1");
    security.principal.kind = PrincipalKind::Agent;
    // An agent in a session, which waits for the decision: only such a
    // refusal files one (`authz::denial_can_be_answered`).
    request.headers_mut().insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("session-1"),
    );
    request.extensions_mut().insert(
        AuthenticatedRequestContext::new(TenantId::new(TENANT), security)
            .with_session_id(Some("session-1".to_string())),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    let decisions = store.query_decisions(TENANT, None).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        state
            .list_entity_ids(&TenantId::new("temper-system"), "GovernanceDecision")
            .len(),
        1
    );
    let decision: serde_json::Value = serde_json::from_str(&decisions[0]).unwrap();
    assert_eq!(body["decision_id"], decision["id"]);
    assert_eq!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn offer_batches_validate_the_whole_request_and_authenticated_principal() {
    let (state, store, _temp) = state_with_turso("offer-batch-validation").await;
    let app = build_router(state);
    for (body, expected) in [
        (
            serde_json::json!({"agent_id":"intruder", "records":[]}),
            StatusCode::FORBIDDEN,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[
                {"resource_type":"Release", "resource_id":"r1", "actions":["Approve"]},
                {"resource_type":"Release", "resource_id":"r2", "actions":["Approve"], "context":42}
            ]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[], "action":"Approve", "resource_type":"Release", "resource_id":"r1"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            serde_json::json!({"agent_id":"reader", "records":[{"resource_type":"Release", "resource_id":"r1", "actions":[], "context":{"large":"x".repeat(2 * 1024 * 1024)}}]}),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/authorize",
                body,
                TENANT,
                "reader",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(store.query_all_decisions(None).await.unwrap().is_empty());
    assert!(
        store
            .load_policy_denial_patterns(TENANT)
            .await
            .unwrap()
            .is_empty()
    );
    let empty = answer(
        &app,
        serde_json::json!({"agent_id":"agent-1", "records":[]}),
        TENANT,
    )
    .await;
    assert_eq!(empty, serde_json::json!({"records":[]}));
}

/// A task that waits on another: `Unlock` reads the status of the record its
/// `needs` names, `Renew` reads a parameter of the call, `Complete` reads the
/// record's own counter.
const WAITING_TASK: &str = r#"
[automaton]
name = "Order"
states = ["Ready", "Waiting", "Claimed", "Done"]
initial = "Ready"

[[state]]
name = "needs"
type = "string"
initial = ""

[[state]]
name = "worker"
type = "string"
initial = ""

[[state]]
name = "renewals"
type = "counter"
initial = 0

[[action]]
name = "Wait"
kind = "input"
from = ["Ready"]
to = "Waiting"
params = ["needs"]

[[action]]
name = "Unlock"
kind = "input"
from = ["Waiting"]
to = "Ready"
guard = "Order[needs].status == 'Done'"

[[action]]
name = "Claim"
kind = "input"
from = ["Ready"]
to = "Claimed"
params = ["worker"]

[[action]]
name = "Renew"
kind = "input"
from = ["Claimed"]
to = "Claimed"
guard = "worker == params.worker"
params = ["worker"]
effect = ["renewals += 1"]

[[action]]
name = "Complete"
kind = "input"
from = ["Claimed"]
to = "Done"
guard = "renewals > 0"
"#;

/// Make `action` on `Orders('{id}')` as agent-1: the answer's status and body.
async fn make(
    app: &axum::Router,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let uri = format!("/tdata/Orders('{id}')/Temper.{action}");
    let response = app
        .clone()
        .oneshot(json_request(Method::POST, &uri, params, TENANT, "agent-1"))
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

/// What agent-1 reads at `uri`.
async fn read_json(app: &axum::Router, uri: &str) -> serde_json::Value {
    let read = app
        .clone()
        .oneshot(json_request(
            Method::GET,
            uri,
            serde_json::json!({}),
            TENANT,
            "agent-1",
        ))
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK, "{uri}");
    serde_json::from_slice(
        &axum::body::to_bytes(read.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// An app of waiting tasks ([`WAITING_TASK`]) holding `ids`, each Ready,
/// under the Cedar `policy`.
async fn waiting_tasks(
    name: &str,
    policy: &str,
    ids: &[&str],
) -> (axum::Router, TursoEventStore, tempfile::TempDir) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let (state, store, temp) = state_with_turso(name).await;
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(
            TENANT,
            temper_spec::csdl::parse_csdl(csdl).unwrap(),
            csdl.to_string(),
            &[("Order", WAITING_TASK)],
        );
        registry.set_verification_status(
            &TenantId::new(TENANT),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-11-19T00:00:00Z".to_string(),
            }),
        );
    }
    state.authz.reload_tenant_policies(TENANT, policy).unwrap();
    for id in ids {
        state
            .get_or_create_tenant_entity(&TenantId::new(TENANT), "Order", id, serde_json::json!({}))
            .await
            .unwrap();
    }
    (build_router(state), store, temp)
}

/// Take the Ready task `id` to Done.
async fn finish(app: &axum::Router, id: &str) {
    for (action, params) in [
        ("Claim", serde_json::json!({"worker": "w1"})),
        ("Renew", serde_json::json!({"worker": "w1"})),
        ("Complete", serde_json::json!({})),
    ] {
        let made = make(app, id, action, params).await;
        assert_eq!(made.0, StatusCode::OK, "{id} {action}: {}", made.1);
    }
}

/// What agent-1 is offered on `Orders('{id}')` for `actions`, asked as a page
/// asks: the record is read, and the row read is sent back as its snapshot,
/// with `claims` written over what was read.
async fn offered(
    app: &axum::Router,
    id: &str,
    actions: &[&str],
    claims: &[(&str, &str)],
) -> Vec<serde_json::Value> {
    let row = read_json(app, &format!("/tdata/Orders('{id}')")).await;
    let mut context = serde_json::Map::new();
    for part in ["fields", "counters", "booleans", "lists"] {
        if let Some(values) = row.get(part).and_then(|values| values.as_object()) {
            context.extend(values.clone());
        }
    }
    context.insert("status".to_string(), row["status"].clone());
    for (name, value) in claims {
        context.insert(name.to_string(), serde_json::json!(value));
    }
    let answered = answer(
        app,
        serde_json::json!({"agent_id": "agent-1", "records": [{
            "resource_type": "Order", "resource_id": id, "actions": actions, "context": context
        }]}),
        TENANT,
    )
    .await;
    answered["records"][0]["actions"]
        .as_array()
        .expect("one answer per action")
        .clone()
}

/// An offer says what a dispatch would: a move its record's state or guard
/// rules out is not offered, with the refusal the move itself gets, and the
/// same move is offered once the guard holds. Cedar lets agent-1 make every
/// move here, so only the record decides.
#[tokio::test]
async fn an_offer_is_refused_as_the_move_would_be_by_state_and_guard() {
    let (app, _store, _temp) = waiting_tasks(
        "offer-state-and-guard",
        "permit(principal, action, resource);",
        &["first", "other", "dep", "held"],
    )
    .await;
    finish(&app, "other").await;
    let waits = make(&app, "dep", "Wait", serde_json::json!({"needs": "first"})).await;
    assert_eq!(waits.0, StatusCode::OK, "{}", waits.1);

    // The stored record names the record it waits on: a snapshot that names
    // one that is Done is answered about `first` all the same.
    let claimed_other = offered(&app, "dep", &["Unlock"], &[("needs", "other")]).await;
    assert_eq!(claimed_other[0]["allowed"], false, "{}", claimed_other[0]);

    // `dep` waits on `first`, which is not Done: Unlock's guard fails, and
    // Claim does not start from Waiting.
    let offers = offered(&app, "dep", &["Unlock", "Claim"], &[]).await;
    assert_eq!(claimed_other[0], offers[0]);
    assert_eq!(offers[0]["allowed"], false, "{}", offers[0]);
    assert_eq!(offers[0]["refusal"]["code"], "GuardFailed");
    assert_eq!(offers[1]["allowed"], false, "{}", offers[1]);
    assert_eq!(
        offers[1]["refusal"],
        serde_json::json!({"code": "InvalidState", "state": "Waiting", "from": ["Ready"]})
    );
    for (offer, action) in offers.iter().zip(["Unlock", "Claim"]) {
        let (status, body) = make(&app, "dep", action, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let mut refused = body["error"].clone();
        let message = refused.as_object_mut().unwrap().remove("message");
        assert_eq!(Some(offer["reason"].clone()), message, "{action}");
        assert_eq!(offer["refusal"], refused, "{action}");
    }

    // Once `first` is Done the same move is offered, and can be made.
    finish(&app, "first").await;
    let offers = offered(&app, "dep", &["Unlock"], &[]).await;
    assert_eq!(
        offers[0],
        serde_json::json!({"action": "Unlock", "allowed": true, "reason": null, "refusal": null})
    );
    let unlocked = make(&app, "dep", "Unlock", serde_json::json!({})).await;
    assert_eq!(unlocked.0, StatusCode::OK, "{}", unlocked.1);

    // A guard on the record's own counter is judged on the snapshot; one that
    // reads the call's parameters is offered, since some call can make it.
    let claimed = make(&app, "held", "Claim", serde_json::json!({"worker": "w1"})).await;
    assert_eq!(claimed.0, StatusCode::OK, "{}", claimed.1);
    let offers = offered(&app, "held", &["Renew", "Complete", "Read"], &[]).await;
    assert_eq!(offers[0]["allowed"], true, "{}", offers[0]);
    assert_eq!(offers[1]["allowed"], false, "{}", offers[1]);
    assert_eq!(offers[1]["refusal"]["code"], "GuardFailed");
    // Not a move of the record: Cedar alone answers.
    assert_eq!(offers[2]["allowed"], true, "{}", offers[2]);

    // A question with no record state is Cedar's alone, as before.
    let bare = answer(
        &app,
        serde_json::json!({
            "agent_id": "agent-1", "action": "Unlock", "resource_type": "Order", "resource_id": "held"
        }),
        TENANT,
    )
    .await;
    assert_eq!(
        bare,
        serde_json::json!({"allowed": true, "reason": null, "refusal": null})
    );
}

/// The names in one of a body's move lists (`@odata.actions`, `@temper.refused`).
fn names(body: &serde_json::Value, list: &str) -> Vec<String> {
    body[list]
        .as_array()
        .unwrap_or_else(|| panic!("{list} is a list: {body}"))
        .iter()
        .map(|entry| entry["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A read tells its reader which moves it could make on the record now, and
/// which of the moves that start from where the record is it could not, with
/// why: what a dispatch of each would answer. A list read says the same of
/// each row when asked with `actions=true`.
#[tokio::test]
async fn a_read_lists_the_moves_its_reader_could_make_now() {
    let (app, _store, _temp) = waiting_tasks(
        "read-moves",
        r#"permit(principal, action, resource);
forbid(principal, action == Action::"Claim", resource) when { resource.id == "barred" };"#,
        &["first", "dep", "barred"],
    )
    .await;
    let waits = make(&app, "dep", "Wait", serde_json::json!({"needs": "first"})).await;
    assert_eq!(waits.0, StatusCode::OK, "{}", waits.1);

    // `dep` waits on `first`, which is not Done. Unlock is its one move from
    // Waiting and its guard fails: no move is listed, and Unlock is refused
    // with what a dispatch of it answers.
    let dep = read_json(&app, "/tdata/Orders('dep')").await;
    assert_eq!(names(&dep, "@odata.actions"), Vec::<String>::new(), "{dep}");
    assert_eq!(names(&dep, "@temper.refused"), ["Unlock"], "{dep}");
    let (status, body) = make(&app, "dep", "Unlock", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let mut refused = body["error"].clone();
    let message = refused.as_object_mut().unwrap().remove("message");
    assert_eq!(
        dep["@temper.refused"][0],
        serde_json::json!({"name": "Unlock", "reason": message, "refusal": refused})
    );

    // Cedar forbids this reader Claim on `barred`: Wait is listed with its
    // path, and Claim is refused with Cedar's reason and no typed refusal.
    let barred = read_json(&app, "/tdata/Orders('barred')").await;
    assert_eq!(names(&barred, "@odata.actions"), ["Wait"], "{barred}");
    assert_eq!(
        barred["@odata.actions"][0]["target"],
        "Orders('barred')/Temper.Wait"
    );
    assert_eq!(names(&barred, "@temper.refused"), ["Claim"], "{barred}");
    let claim = &barred["@temper.refused"][0];
    assert!(claim["reason"].as_str().is_some_and(|why| !why.is_empty()));
    assert_eq!(claim["refusal"], serde_json::Value::Null);

    // A list read carries no moves unless asked, and then the same ones on
    // each row, whichever fields it selects.
    let plain = read_json(&app, "/tdata/Orders").await;
    for row in plain["value"].as_array().unwrap() {
        assert!(row.get("@odata.actions").is_none(), "{row}");
        assert!(row.get("@temper.refused").is_none(), "{row}");
    }
    let asked = read_json(&app, "/tdata/Orders?actions=true&$select=status").await;
    let rows = asked["value"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{asked}");
    let row = |id: &str| {
        let odata_id = format!("Orders('{id}')");
        rows.iter()
            .find(|row| row["@odata.id"] == odata_id.as_str())
            .unwrap_or_else(|| panic!("a row for {id}: {asked}"))
    };
    assert!(row("dep").get("fields").is_none(), "only what was selected");
    assert_eq!(row("dep")["@odata.actions"], dep["@odata.actions"]);
    assert_eq!(row("dep")["@temper.refused"], dep["@temper.refused"]);
    assert_eq!(row("barred")["@odata.actions"], barred["@odata.actions"]);
    assert_eq!(row("barred")["@temper.refused"], barred["@temper.refused"]);
    assert_eq!(names(row("first"), "@odata.actions"), ["Wait", "Claim"]);

    // Once `first` is Done, Unlock is listed, and can be made.
    finish(&app, "first").await;
    let dep = read_json(&app, "/tdata/Orders('dep')").await;
    assert_eq!(names(&dep, "@odata.actions"), ["Unlock"], "{dep}");
    assert_eq!(names(&dep, "@temper.refused"), Vec::<String>::new());
    let unlocked = make(&app, "dep", "Unlock", serde_json::json!({})).await;
    assert_eq!(unlocked.0, StatusCode::OK, "{}", unlocked.1);
}

/// A `Payment` waits a day in `Waiting`, then lapses back to `Open`.
const DAY_TIMER: &str = r#"
[automaton]
name = "Payment"
states = ["Open", "Waiting"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "Lapse"
kind = "internal"
from = ["Waiting"]
to = "Open"

[[state_timeout]]
state = "Waiting"
after_seconds = 86400
on_timeout = "Lapse"
"#;

/// Give `state` the day-timer payments ([`DAY_TIMER`]), open to every caller.
fn register_day_timer(state: &ServerState) {
    use temper_server::registry::{EntityVerificationResult, VerificationStatus};
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        TENANT,
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.to_string(),
        &[("Payment", DAY_TIMER)],
    );
    registry.set_verification_status(
        &TenantId::new(TENANT),
        "Payment",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-11-19T00:00:00Z".to_string(),
        }),
    );
    drop(registry);
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
}

/// A read says when a record's timer is past due, from the kernel's own
/// timer row: the action that should have run, when it was due and how long
/// ago. A record whose timer has time left says null, on a read of one and
/// on a list row alike.
#[tokio::test]
async fn a_read_says_when_a_records_timer_is_past_due() {
    use temper_runtime::persistence::StateTimerRecord;
    let (first, store, temp) = state_with_turso("read-overdue-first").await;
    register_day_timer(&first);
    for id in ["late", "fresh"] {
        first
            .get_or_create_tenant_entity(
                &TenantId::new(TENANT),
                "Payment",
                id,
                serde_json::json!({}),
            )
            .await
            .unwrap();
    }
    let app = build_router(first);
    for id in ["late", "fresh"] {
        let waited = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("/tdata/Payments('{id}')/Temper.Wait"),
                serde_json::json!({}),
                TENANT,
                "agent-1",
            ))
            .await
            .unwrap();
        assert_eq!(waited.status(), StatusCode::OK, "{id}");
    }
    drop(app);

    // `late`'s day ran out a day ago: its row as a timer that never fired
    // leaves it.
    let armed = store
        .state_timer_row(TENANT, "Payment", "late")
        .await
        .expect("row reads")
        .expect("Wait armed a timer row");
    let started = chrono::Utc::now() - chrono::Duration::days(2);
    let due = started + chrono::Duration::days(1);
    store
        .upsert_state_timer(&StateTimerRecord {
            started_at: started.to_rfc3339(),
            due_at: Some(due.to_rfc3339()),
            ..armed
        })
        .await
        .expect("row rewritten");

    // Another process on the same store armed neither timer: the rows are
    // all it has, as after a restart whose timers did not come back.
    let mut second =
        ServerState::from_registry(ActorSystem::new("read-overdue-second"), SpecRegistry::new());
    second.data_dir = temp.path().join("data");
    second.set_storage_stack(StorageStack::from_turso(store.clone()));
    register_day_timer(&second);
    let app = build_router(second);

    let late = read_json(&app, "/tdata/Payments('late')").await;
    let told = &late["@temper.overdue"];
    assert_eq!(told["action"], "Lapse", "{late}");
    let told_due = chrono::DateTime::parse_from_rfc3339(told["due_at"].as_str().unwrap()).unwrap();
    assert_eq!(told_due.timestamp(), due.timestamp(), "{late}");
    let seconds = told["late_seconds"].as_u64().unwrap();
    assert!((86_400..86_400 + 600).contains(&seconds), "{late}");

    let fresh = read_json(&app, "/tdata/Payments('fresh')").await;
    assert_eq!(fresh["@temper.overdue"], serde_json::Value::Null, "{fresh}");

    let plain = read_json(&app, "/tdata/Payments").await;
    for row in plain["value"].as_array().unwrap() {
        assert!(row.get("@temper.overdue").is_none(), "{row}");
    }
    let asked = read_json(&app, "/tdata/Payments?actions=true").await;
    let rows = asked["value"].as_array().unwrap();
    let row = |id: &str| {
        let odata_id = format!("Payments('{id}')");
        rows.iter()
            .find(|row| row["@odata.id"] == odata_id.as_str())
            .unwrap_or_else(|| panic!("a row for {id}: {asked}"))
    };
    assert_eq!(row("late")["@temper.overdue"]["action"], "Lapse");
    assert_eq!(row("late")["@temper.overdue"]["due_at"], told["due_at"]);
    assert_eq!(row("fresh")["@temper.overdue"], serde_json::Value::Null);
}
