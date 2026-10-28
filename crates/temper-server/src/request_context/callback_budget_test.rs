//! Deterministic scheduling of the production callback context transitions.
//!
//! A seed chooses inline callbacks and detached answers. A detached answer (a
//! module's, an adapter's, or a compensating move) keeps the context of the
//! move that called it, so whoever made that move still makes the answer. The
//! budget and task boundary methods run unmodified. Real WASM and compensation
//! wiring is exercised separately by strict_native_callbacks and
//! answers_act_for_the_asker.
use super::*;
use temper_runtime::scheduler::DeterministicRng;

#[test]
fn dst_detached_schedules_preserve_both_callback_bounds() {
    let mut coverage = [0_u32; 2];
    for seed in 1..=1024 {
        let mut rng = DeterministicRng::new(seed);
        let mut context = AgentContext {
            agent_id: Some("ann".into()),
            session_id: Some("session".into()),
            workflow_run_id: Some("workflow".into()),
            intent: Some("bounded workflow".into()),
            observation_metadata: BTreeMap::from([("test.seed".into(), seed.to_string())]),
            ..AgentContext::default()
        };
        // The oracle is the approved 512-hop contract, independent of the implementation constant.
        for admitted in 1..=512 {
            let operation = rng.next_bound(2);
            coverage[operation] += 1;
            // Inline, the step the context is inside of cannot run again,
            // however few steps the chain has.
            if let Some(inside) = context.inline_chain.steps().last() {
                assert!(
                    matches!(
                        context.for_callback("Job", "job", &inside.action, "test"),
                        Err(CallbackBudgetExceeded::RepeatedStep(_))
                    ),
                    "seed={seed}, admitted={admitted}"
                );
            }
            if operation != 0 {
                let previous_hops = context.callback_hops;
                context = context.for_background_task();
                assert!(context.inline_chain.is_empty(), "seed={seed}");
                assert_eq!(context.callback_hops, previous_hops, "seed={seed}");
            }
            let previous_steps = context.inline_chain.steps().len();
            context = context
                .for_callback("Job", "job", &format!("Step{admitted}"), "test")
                .unwrap_or_else(|error| {
                    panic!("seed={seed}, admitted={admitted}, operation={operation}: {error}")
                });
            if operation != 0 {
                let previous = context.clone();
                context = context.for_answer();
                assert_eq!(
                    context.inline_chain.steps(),
                    previous.inline_chain.steps(),
                    "seed={seed}"
                );
                assert_eq!(
                    context.idempotency_key, previous.idempotency_key,
                    "seed={seed}"
                );
            }
            assert_eq!(context.callback_hops, admitted, "seed={seed}");
            assert_eq!(context.agent_id.as_deref(), Some("ann"), "seed={seed}");
            assert_eq!(
                context.inline_chain.steps().len(),
                previous_steps + 1,
                "seed={seed}"
            );
            assert_eq!(context.session_id.as_deref(), Some("session"));
            assert_eq!(context.workflow_run_id.as_deref(), Some("workflow"));
            assert_eq!(context.intent.as_deref(), Some("bounded workflow"));
            assert_eq!(context.observation_metadata["test.seed"], seed.to_string());
        }
        assert_total_hops_exhausted(context, seed);
    }
    assert!(coverage.iter().all(|count| *count > 0), "{coverage:?}");
}

fn assert_total_hops_exhausted(context: AgentContext, seed: u64) {
    let context = context.for_background_task().for_answer();
    assert_eq!(context.callback_hops, 512, "seed={seed}");
    assert_eq!(
        context
            .for_callback("Job", "job", "Next", "test")
            .unwrap_err(),
        CallbackBudgetExceeded::TotalHops,
        "seed={seed}"
    );
}

/// Inline callbacks nest as deep as their steps differ (the old bound refused
/// the ninth), and stop at the first step that repeats, naming the loop.
#[test]
fn inline_callbacks_stop_at_their_first_repeated_step() {
    let mut context = AgentContext::default().entering("Job", "job", "Start");
    for admitted in 1..=64 {
        context = context
            .for_callback("Job", "job", &format!("Step{admitted}"), "test")
            .unwrap();
        assert_eq!(context.callback_hops, admitted);
    }
    let inherited = AgentContext::for_service_inheriting("wasm-runtime", &context);
    let Err(CallbackBudgetExceeded::RepeatedStep(repeat)) =
        inherited.for_callback("Job", "job", "Step60", "test")
    else {
        panic!("a repeated step must be refused");
    };
    assert_eq!(
        repeat.path(),
        ["Step60", "Step61", "Step62", "Step63", "Step64", "Step60"]
            .map(|action| format!("Job(job).{action}"))
    );
    // The same action on another entity is a different step.
    assert!(
        inherited
            .for_callback("Job", "other", "Step60", "test")
            .is_ok()
    );
    assert_eq!(inherited.callback_hops, context.callback_hops);
}

#[test]
fn request_headers_cannot_supply_callback_budgets() {
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
    assert_eq!(context.callback_hops, 0);
    assert!(context.security_ctx.is_none());
}

#[test]
fn detached_boundary_preserves_authority_and_idempotency() {
    let context = AgentContext {
        callback_hops: 40,
        idempotency_key: Some(crate::idempotency::IdempotencyKey::Request(
            "request-key".into(),
        )),
        ..AgentContext::for_service("invoker")
    }
    .entering("Job", "job", "Start");
    let detached = context.for_background_task();
    assert!(!context.inline_chain.is_empty());
    assert!(detached.inline_chain.is_empty());
    assert_eq!(detached.callback_hops, 40);
    assert_eq!(detached.agent_id, context.agent_id);
    assert_eq!(detached.agent_type, context.agent_type);
    assert_eq!(detached.idempotency_key, context.idempotency_key);
    assert_eq!(
        serde_json::to_value(&detached.security_ctx).unwrap(),
        serde_json::to_value(&context.security_ctx).unwrap()
    );
}

/// An answer keeps its caller and its derived key, without firing the timer
/// or scheduled action that the caller fired a second time.
#[test]
fn answer_preserves_authority_and_its_own_key_without_the_askers_fire_marks() {
    let context = AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_resolved_identity(
            "ann", "person", None,
        )),
        agent_id: Some("ann".into()),
        callback_hops: 40,
        idempotency_key: Some(IdempotencyKey::Request("asker-key".into())),
        fires_scheduled_action: Some(ScheduledFire {
            tenant: "tenant-a".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            arm_id: "arm".into(),
        }),
        fires_state_timer: Some(StateTimerFire {
            tenant: "tenant-a".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            action: "Ask".into(),
            armed_seq: 7,
        }),
        ..AgentContext::default()
    }
    .entering("Job", "job", "Ask");
    let callback = context
        .for_background_task()
        .for_callback("Job", "job", "Answer", "module")
        .unwrap();
    assert_ne!(callback.idempotency_key, context.idempotency_key);
    assert!(matches!(
        callback.idempotency_key,
        Some(IdempotencyKey::FollowUp(_))
    ));
    assert!(callback.fires_scheduled_action.is_some());
    assert!(callback.fires_state_timer.is_some());
    let answer = callback.for_answer();
    assert_eq!(answer.agent_id, context.agent_id);
    assert_eq!(
        serde_json::to_value(&answer.security_ctx).unwrap(),
        serde_json::to_value(&context.security_ctx).unwrap()
    );
    assert_eq!(answer.callback_hops, 41);
    assert_eq!(answer.inline_chain.steps(), callback.inline_chain.steps());
    assert_eq!(answer.idempotency_key, callback.idempotency_key);
    assert!(answer.fires_scheduled_action.is_none());
    assert!(answer.fires_state_timer.is_none());
}
