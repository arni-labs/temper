//! The context a declared answer is made in: a module's or an adapter's
//! answer, or a compensating move.

use super::AgentContext;

impl AgentContext {
    /// Made as whoever made the move that asked for the answer, with that
    /// move's budgets and trace, but none of its per-request marks: an
    /// idempotency key or the timer or scheduled action it fires belong to
    /// that move, and an answer is a move of its own. With the asker's key,
    /// an answer on the asker's own record would be taken as a retry of the
    /// move that asked and never land.
    pub(crate) fn for_answer(&self) -> Self {
        Self {
            idempotency_key: None,
            fires_scheduled_action: None,
            fires_state_timer: None,
            ..self.clone()
        }
    }
}
