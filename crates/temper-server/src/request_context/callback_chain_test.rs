//! Deterministic scheduling of the production callback context transitions.
//!
//! A seed chooses inline callbacks, detached successes and compensation
//! identities. Identity provisioning is a deterministic fixture: the production
//! service constructor generates an unrelated correlation UUID. The chain, task
//! boundary and context inheritance methods run unmodified. Real WASM and
//! compensation wiring is exercised separately by strict_native_callbacks.
use super::*;
use temper_runtime::scheduler::DeterministicRng;

/// A loop through detached tasks takes the same step lap after lap, far past
/// the 512 laps the removed hop budget allowed, and its callback depth keeps
/// counting them; inline, the step a context is inside of is still refused on
/// every lap.
#[test]
fn detached_laps_run_on_while_inline_repeats_stop() {
    // Four times the removed 512-hop budget.
    const LAPS: u32 = 2048;
    let mut coverage = [0_u32; 4];
    for seed in 1..=256 {
        let mut rng = DeterministicRng::new(seed);
        let mut context = AgentContext {
            session_id: Some("session".into()),
            workflow_run_id: Some("workflow".into()),
            intent: Some("long workflow".into()),
            observation_metadata: BTreeMap::from([("test.seed".into(), seed.to_string())]),
            ..AgentContext::default()
        };
        for lap in 1..=LAPS {
            let operation = rng.next_bound(4);
            coverage[operation] += 1;
            if operation == 0 {
                // Inline, every lap is a new step of the same chain.
                let previous_steps = context.inline_chain.steps().len();
                context = context
                    .for_callback("Job", "job", &format!("Step{lap}"))
                    .unwrap_or_else(|error| panic!("seed={seed}, lap={lap}: {error}"));
                assert_eq!(
                    context.inline_chain.steps().len(),
                    previous_steps + 1,
                    "seed={seed}"
                );
            } else {
                // Across a detached boundary the loop takes the same step again.
                let depth = context.callback_depth;
                context = context.for_background_task();
                assert!(context.inline_chain.is_empty(), "seed={seed}");
                assert_eq!(context.callback_depth, depth, "seed={seed}");
                let service = match operation {
                    1 => "wasm-runtime",
                    2 => "platform-dispatch",
                    3 => "integration-compensation",
                    _ => unreachable!("bounded operation"),
                };
                context = service_identity(service).inherit_observability_from(&context);
                assert_eq!(context.agent_type.as_deref(), Some(service), "seed={seed}");
                context = context
                    .for_callback("Job", "job", "Tick")
                    .unwrap_or_else(|error| {
                        panic!("seed={seed}, lap={lap}, operation={operation}: {error}")
                    });
            }
            // Every lap is one callback deeper, whichever boundary it crossed.
            assert_eq!(context.callback_depth, u64::from(lap), "seed={seed}");
            // Inline, the step the context is inside of cannot run again.
            let inside = context.inline_chain.steps().last().cloned().unwrap();
            assert!(
                matches!(
                    context.for_callback("Job", "job", &inside.action),
                    Err(RepeatedCallback(_))
                ),
                "seed={seed}, lap={lap}"
            );
            assert_eq!(context.session_id.as_deref(), Some("session"));
            assert_eq!(context.workflow_run_id.as_deref(), Some("workflow"));
            assert_eq!(context.intent.as_deref(), Some("long workflow"));
            assert_eq!(context.observation_metadata["test.seed"], seed.to_string());
        }
    }
    assert!(coverage.iter().all(|count| *count > 0), "{coverage:?}");
}

// Provisioning identity is outside the callback chain's state machine. The
// production preservation method below is the one used by for_service_inheriting.
fn service_identity(service: &str) -> AgentContext {
    AgentContext {
        agent_id: Some(format!("service:{service}")),
        agent_type: Some(service.into()),
        ..AgentContext::default()
    }
}

/// Inline callbacks nest as deep as their steps differ (the old bound refused
/// the ninth), and stop at the first step that repeats, naming the loop.
#[test]
fn inline_callbacks_stop_at_their_first_repeated_step() {
    let mut context = AgentContext::default().entering("Job", "job", "Start");
    for admitted in 1..=64 {
        context = context
            .for_callback("Job", "job", &format!("Step{admitted}"))
            .unwrap();
        assert_eq!(context.callback_depth, admitted);
    }
    let inherited = AgentContext::for_service_inheriting("wasm-runtime", &context);
    let Err(RepeatedCallback(repeat)) = inherited.for_callback("Job", "job", "Step60") else {
        panic!("a repeated step must be refused");
    };
    assert_eq!(
        repeat.path(),
        ["Step60", "Step61", "Step62", "Step63", "Step64", "Step60"]
            .map(|action| format!("Job(job).{action}"))
    );
    // The same action on another entity is a different step.
    assert!(inherited.for_callback("Job", "other", "Step60").is_ok());
    assert_eq!(inherited.callback_depth, context.callback_depth);
}

#[test]
fn request_headers_cannot_supply_runtime_owned_context() {
    let mut headers = HeaderMap::new();
    for name in [
        "x-callback-depth",
        "x-callback-hops",
        "x-temper-callback-depth",
        "x-temper-callback-hops",
    ] {
        headers.insert(name, axum::http::HeaderValue::from_static("511"));
    }
    let context = extract_agent_context(&headers);
    assert!(context.inline_chain.is_empty());
    assert_eq!(context.callback_depth, 0);
    assert!(context.security_ctx.is_none());
}

#[test]
fn detached_boundary_preserves_authority_and_idempotency() {
    let context = AgentContext {
        callback_depth: 40,
        idempotency_key: Some("request-key".into()),
        ..AgentContext::for_service("invoker")
    }
    .entering("Job", "job", "Start");
    let detached = context.for_background_task();
    assert!(!context.inline_chain.is_empty());
    assert!(detached.inline_chain.is_empty());
    assert_eq!(detached.callback_depth, 40);
    assert_eq!(detached.agent_id, context.agent_id);
    assert_eq!(detached.agent_type, context.agent_type);
    assert_eq!(detached.idempotency_key, context.idempotency_key);
    assert_eq!(
        serde_json::to_value(&detached.security_ctx).unwrap(),
        serde_json::to_value(&context.security_ctx).unwrap()
    );
    // The detached task's first step may be the one its parent is inside of,
    // one callback deeper.
    let next = detached.for_callback("Job", "job", "Start").unwrap();
    assert_eq!(next.callback_depth, 41);
    assert!(context.for_callback("Job", "job", "Start").is_err());
}
