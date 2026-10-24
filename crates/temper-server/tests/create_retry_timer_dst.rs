//! Seeded simulations of a create that fails after its entity is stored and
//! is then retried.
//!
//! A create stores its entity, writes its query projection, then arms the
//! timer of the state it was created into. When the projection write fails,
//! the create answers 500 with its entity already stored, and the retry is
//! answered from that entity. The retry finishes the create: the entity's
//! timer is armed and fires when its budget is spent, counted from the
//! create. A later retry leaves the armed timer and its deadline alone.
//!
//! Each seed drives one single-threaded execution on the simulated journal,
//! with tokio's clock paused so timers wait on simulated time only. The
//! create path's cooperative yield points take a seeded number of yields, so
//! the two concurrent retries interleave differently on each seed.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::odata_requests::send;
use serde_json::{Value, json};
use temper_runtime::buggify::install_buggify_context;
use temper_runtime::scheduler::{LogicalClock, install_deterministic_context};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;

/// Anyone may create, only the principal an order's number names may read
/// it, and its timer may expire it.
const ORDER_POLICIES: &str = r#"
permit(principal, action == Action::"create", resource is Order);
permit(principal, action == Action::"read", resource is Order)
when { resource has OrderNumber && resource.OrderNumber == principal.id };
permit(principal, action == Action::"Expire", resource is Order);
"#;

/// An Order that expires once it has been a draft for ten seconds.
const ORDER_EXPIRING_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Expired"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Expired"]

[[action]]
name = "Expire"
kind = "input"
from = ["Draft"]
to = "Expired"

[[state_timeout]]
state = "Draft"
after_seconds = 10
on_timeout = "Expire"
"#;

/// The draft timer's budget, as the spec declares it.
const EXPIRY: Duration = Duration::from_secs(10);

/// Milliseconds per tick of the simulation's logical clock.
const CLOCK_TICK_MS: u64 = 100;

/// Upper bound on the scheduler turns a fired timer's action takes to land.
const SETTLE_TURNS: usize = 200;

/// Seeds explored on every run. Without the fix no seed passes: the retries
/// never arm the timer, whatever their interleaving.
const SEEDS: u64 = 32;

fn order() -> Value {
    json!({
        "Id": "retried-key",
        "OrderNumber": "alice",
        "Notes": "alice private words",
    })
}

/// Move simulated time forward by `by`: the logical clock that stamps
/// events, and the paused tokio clock that timers wait on.
async fn pass(clock: &LogicalClock, by: Duration) {
    let millis = u64::try_from(by.as_millis()).expect("a test duration fits in u64");
    clock.advance_by(millis / CLOCK_TICK_MS);
    tokio::time::advance(by).await;
}

/// The order's status once nothing more is ready to run, within
/// [`SETTLE_TURNS`] scheduler turns.
async fn settled_status(state: &ServerState) -> Result<String, String> {
    let tenant = TenantId::default();
    let mut status = String::new();
    for _ in 0..SETTLE_TURNS {
        tokio::task::yield_now().await;
        status = state
            .get_tenant_entity_state(&tenant, "Order", "retried-key")
            .await
            .map_err(|error| format!("the order cannot be read: {error}"))?
            .state
            .status;
        if status != "Draft" {
            break;
        }
    }
    Ok(status)
}

/// The create's projection write fails, then the owner retries twice at once
/// and once more later. Every retry answers 201, the order stays a draft
/// until its budget from the create is spent, and then its timer expires it.
/// `Err` names the broken invariant.
async fn retried_create_arms_its_timer(seed: u64) -> Result<(), String> {
    let (_sim, clock, _ids) = install_deterministic_context(seed);
    let (state, projection) = common::order_projection::order_state(
        seed,
        &format!("create-retry-timer-{seed}"),
        ORDER_EXPIRING_IOA,
        ORDER_POLICIES,
    );

    projection.fail_next_write();
    let (status, body) = send(&state, "POST", "/tdata/Orders", "alice", Some(order())).await;
    if status != StatusCode::INTERNAL_SERVER_ERROR || projection.holds("Order", "retried-key") {
        return Err(format!(
            "the create whose projection write failed got {status} {body}"
        ));
    }

    let _faults = install_buggify_context(seed);
    let (first, second) = tokio::join!(
        send(&state, "POST", "/tdata/Orders", "alice", Some(order())),
        send(&state, "POST", "/tdata/Orders", "alice", Some(order())),
    );
    for (status, body) in [first, second] {
        if status != StatusCode::CREATED {
            return Err(format!("a retry of the create got {status} {body}"));
        }
    }
    if !projection.holds("Order", "retried-key") {
        return Err("the retries left the order out of the query projection".to_string());
    }

    // Most of the budget passes; a later retry must not restart it.
    pass(&clock, EXPIRY * 6 / 10).await;
    let (status, body) = send(&state, "POST", "/tdata/Orders", "alice", Some(order())).await;
    if status != StatusCode::CREATED {
        return Err(format!("a later retry of the create got {status} {body}"));
    }
    let early = settled_status(&state).await?;
    if early != "Draft" {
        return Err(format!(
            "the order left Draft before its budget was spent: {early}"
        ));
    }

    // The budget, counted from the create, is now spent.
    pass(&clock, EXPIRY * 5 / 10).await;
    let after = settled_status(&state).await?;
    if after != "Expired" {
        return Err(format!(
            "the order is {after} once its budget from the create was spent: \
             the retries left it with no timer, or restarted its timer"
        ));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_retry_that_finishes_a_create_arms_its_timer_without_restarting_it() {
    for seed in 0..SEEDS {
        if let Err(violation) = retried_create_arms_its_timer(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}
