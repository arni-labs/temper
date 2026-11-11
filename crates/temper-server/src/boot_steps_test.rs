//! `/healthz` names the boot step a starting server is in.

use std::sync::{Arc, RwLock};

use axum::extract::State;
use temper_runtime::ActorSystem;
use tracing_subscriber::layer::SubscriberExt;

use super::BootSteps;
use crate::registry::SpecRegistry;
use crate::state::ServerState;

/// `/healthz`'s answer: its status code and body.
async fn health(state: &ServerState) -> (u16, serde_json::Value) {
    let response = crate::write_health::healthz(State(state.clone())).await;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the answer body");
    let body = serde_json::from_slice(&bytes).expect("the answer is JSON");
    (status, body)
}

#[tokio::test]
async fn a_starting_server_names_the_boot_step_it_is_in_and_a_serving_one_none() {
    let steps = BootSteps::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(steps.clone()));
    let mut state = ServerState::from_registry_shared(
        ActorSystem::new("boot-steps"),
        Arc::new(RwLock::new(SpecRegistry::new())),
    );
    state.boot_steps = steps;
    state.mark_starting();

    let runtime = tracing::info_span!("start_runtime", otel.name = "boot.start_runtime");
    let compare = runtime.in_scope(|| {
        tracing::info_span!(
            "projection_compare",
            otel.name = "boot.projection_compare",
            tenant = %"default"
        )
    });
    let (status, body) = health(&state).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "starting", "{body}");
    assert_eq!(body["boot"]["step"], "projection_compare", "{body}");
    assert_eq!(body["boot"]["tenant"], "default", "{body}");
    assert!(body["boot"]["since_secs"].is_u64(), "{body}");

    drop(compare);
    let (_, body) = health(&state).await;
    assert_eq!(
        body["boot"]["step"], "start_runtime",
        "the step it is back in: {body}"
    );
    assert!(body["boot"].get("tenant").is_none(), "{body}");

    let other = tracing::info_span!("other", otel.name = "dispatch.action");
    let (_, body) = health(&state).await;
    assert_eq!(
        body["boot"]["step"], "start_runtime",
        "only boot spans are steps: {body}"
    );
    drop(other);

    drop(runtime);
    state.mark_serving();
    let (status, body) = health(&state).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "serving", "{body}");
    assert!(
        body.get("boot").is_none(),
        "a serving server names no boot step: {body}"
    );
}
