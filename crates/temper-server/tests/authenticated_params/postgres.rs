//! The PostgreSQL OData branch must bind before enqueueing, independently of native dispatch.
use std::sync::Arc;

use temper_actor_runtime::{ActorSystem, SchedulerConfig, SpecDrivenActor};

use super::*;

#[tokio::test]
async fn postgres_rejects_owner_overrides_and_persists_authenticated_subject() {
    let (pool, _container) = temper_actor_runtime::test_utils::setup_test_pg().await;
    let actors = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    actors
        .register(Arc::new(SpecDrivenActor::from_ioa(IOA).unwrap()))
        .await
        .unwrap();
    // Reuse the verified spec and Cedar policies from the native tests, but route
    // OData through the real PostgreSQL actor system. No scheduler is started:
    // we explicitly activate the queued message after inspecting its receipt.
    let mut state = fixture("");
    state.pg_actor_system = Some(actors.clone());
    state.actor_backed_types.insert("OwnedInstance".into());
    let id = format!(
        "pg-authenticated-owner-{}",
        temper_runtime::scheduler::sim_uuid()
    );
    let namespace = format!("cp/{id}");
    let action_path = format!("Instances('{id}')/Probe.Create");
    let (status, body) = post(&state, "Instances", json!({"id": id})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let initial = actors
        .load_state(&namespace, "OwnedInstance")
        .await
        .unwrap()
        .unwrap();

    // Even supplying the correct subject must fail before anything is queued.
    for owner in ["bob", "alice"] {
        let (status, body) = post(&state, &action_path, json!({"owner_id": owner})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "AuthenticatedParameter", "{body}");
        let count: i64 = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM odp_temper.actor_messages WHERE namespace = $1",
                &[&namespace],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "an override must not enqueue a message");
        assert_eq!(
            actors
                .load_state(&namespace, "OwnedInstance")
                .await
                .unwrap(),
            Some(initial.clone()),
            "a rejected override changed persisted state"
        );
    }

    let (status, receipt) = post(&state, &action_path, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{receipt}");
    let message_id = receipt["message_id"].as_i64().unwrap();
    assert_eq!(
        actors
            .load_state(&namespace, "OwnedInstance")
            .await
            .unwrap(),
        Some(initial),
        "acceptance must only enqueue the action"
    );
    let handle = temper_actor_runtime::ActorHandle::new(&namespace, "OwnedInstance");
    assert!(actors.activate_now(&handle).await.unwrap());

    // A fresh actor system must observe the committed owner in PostgreSQL,
    // rather than any native entity cache or response projection.
    let restored = ActorSystem::new(pool.clone(), SchedulerConfig::default());
    let persisted = restored
        .load_state(&namespace, "OwnedInstance")
        .await
        .unwrap()
        .unwrap();
    let persisted: Value = serde_json::from_slice(&persisted).unwrap();
    assert_eq!(persisted["status"], "Running");
    assert_eq!(persisted["fields"]["owner_id"], "alice");
    let cursor: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT last_msg_id FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = 'OwnedInstance'",
            &[&namespace],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(cursor, message_id, "the accepted action must have executed");
}
