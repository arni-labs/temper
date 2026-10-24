//! A scheduled follow-on is named by the transition and effect that made it.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::ScheduledAction;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;

fn ring() -> ScheduledAction {
    ScheduledAction {
        action: "Ring".into(),
        delay_seconds: 1,
    }
}

fn schedule(state: &ServerState, parent_event: usize, actions: &[ScheduledAction]) {
    state.dispatch_scheduled_actions(
        &TenantId::default(),
        "Bell",
        "b1",
        parent_event,
        actions,
        &AgentContext::default(),
    );
}

fn state() -> ServerState {
    ServerState::from_registry(ActorSystem::new("scheduled-follow-on"), SpecRegistry::new())
}

// Paused tokio time keeps the follow-on timers from firing during the test.
#[tokio::test(start_paused = true)]
async fn a_replayed_reply_starts_no_second_copy_of_its_follow_ons() {
    let state = state();
    schedule(&state, 3, &[ring()]);
    schedule(&state, 3, &[ring()]);
    assert_eq!(state.pending_follow_ons.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn two_transitions_scheduling_the_same_action_at_the_same_moment_both_run() {
    let state = state();
    schedule(&state, 3, &[ring()]);
    schedule(&state, 4, &[ring()]);
    assert_eq!(state.pending_follow_ons.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn each_schedule_effect_of_one_transition_runs() {
    let state = state();
    schedule(&state, 3, &[ring(), ring()]);
    assert_eq!(state.pending_follow_ons.len(), 2);
}
