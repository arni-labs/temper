//! A transition's scheduled follow-ons are keyed at one scheduling moment.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, SimClock, install_sim_context};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::ScheduledAction;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// A clock one second further on each time it is read, so no two reads fall
/// in the same second.
struct SteppingClock(AtomicI64);

impl SimClock for SteppingClock {
    fn now(&self) -> DateTime<Utc> {
        let second = self.0.fetch_add(1, Ordering::SeqCst);
        Utc.timestamp_opt(second, 0)
            .single()
            .expect("test clock second is a valid timestamp")
    }

    fn tick(&self) -> u64 {
        0
    }
}

// Paused tokio time keeps the follow-on timers from firing during the test.
#[tokio::test(start_paused = true)]
async fn the_same_schedule_twice_in_one_transition_is_one_follow_on_across_a_second_boundary() {
    let state =
        ServerState::from_registry(ActorSystem::new("scheduled-follow-on"), SpecRegistry::new());
    let _clock = install_sim_context(
        Arc::new(SteppingClock(AtomicI64::new(1_700_000_000))),
        Arc::new(DeterministicIdGen::new(7)),
    );
    let ring = ScheduledAction {
        action: "Ring".into(),
        delay_seconds: 1,
    };

    state.dispatch_scheduled_actions(
        &TenantId::default(),
        "Twin",
        "w1",
        &[ring.clone(), ring],
        &AgentContext::default(),
    );

    assert_eq!(state.pending_follow_ons.len(), 1);
}
