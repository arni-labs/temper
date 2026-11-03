//! The deadline of a module call a move starts in the background.
//!
//! A background WASM call whose trigger names an `on_failure` always ends in
//! an answer. The move that starts it writes, in its own commit, a scheduled
//! action row for the call: the `on_failure` action, due when the call's
//! deadline passes. The call's answer removes the row in the answer's commit
//! ([`AgentContext::ends_call`]); a call that ends any other way the kernel
//! sees (a refused answer, a silent exit, a module that cannot run) has its
//! row removed right after. An answer whose write the store failed is owed
//! and keeps the row until it lands. So a row is left only for a call the
//! process lost: it stopped while the call ran or while its answer was being
//! written.
//!
//! At boot the row is armed again like any scheduled action, and when it is
//! due it answers the call with its `on_failure`, as whoever made the move,
//! under the key the call's own answer would carry. A record that has moved
//! on refuses it like any late fire, and whichever of the two answers
//! commits first answers the other as its duplicate. A running process
//! never arms these rows: there the engine stops the call at its deadline
//! and the call answers through its own path.

use std::time::Duration;

use super::AgentContext;

/// What separates a call deadline's arm id, `<seq>.call:<integration>`, from
/// a `schedule` effect's `<seq>.<index>`.
const CALL_ARM: &str = ".call:";

/// A background call a move may start, by the custom effect that starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCall {
    /// The custom effect (trigger) that starts the call.
    pub trigger: String,
    /// The integration's name: the call's answer is made under it.
    pub integration: String,
    /// The action that answers the call when it fails or is lost.
    pub on_failure: String,
    /// How long the call may run: its `timeout_secs`, else the engine's
    /// default limit.
    pub deadline: Duration,
}

/// The arm id of the deadline of the call to `integration` that the event at
/// journal `sequence` started.
pub(crate) fn call_arm_id(sequence: u64, integration: &str) -> String {
    format!("{sequence}{CALL_ARM}{integration}")
}

/// The integration a scheduled-action row is the call deadline of, if it is
/// one.
pub(crate) fn called_integration(arm_id: &str) -> Option<&str> {
    arm_id
        .split_once(CALL_ARM)
        .map(|(_, integration)| integration)
}

impl AgentContext {
    /// The context the call to `integration` this context starts answers in,
    /// as [`AgentContext::for_callback`] makes it: the next callback hop, and
    /// the key derived for the integration's run. A lost call's deadline
    /// fires as it, so its answer and the call's own are one follow-up.
    pub(crate) fn for_call_answer(&self, integration: &str) -> Self {
        let mut next = self.for_background_task();
        next.callback_hops = self.callback_hops.saturating_add(1);
        next.idempotency_key = self.callback_key(integration);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_deadline_names_its_integration_and_a_schedule_does_not() {
        let arm_id = call_arm_id(12, "lookup.v2");
        assert_eq!(arm_id, "12.call:lookup.v2");
        assert_eq!(called_integration(&arm_id), Some("lookup.v2"));
        assert_eq!(called_integration("12.0"), None);
    }
}
