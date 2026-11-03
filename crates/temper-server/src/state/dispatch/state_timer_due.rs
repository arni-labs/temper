//! A `due_on` move brings its record's armed state timer forward.
//!
//! An action listed in a `[[state_timeout]]`'s `due_on`, taken while the
//! record is in that state, makes the timer the record has armed due now. The
//! timer then fires as it would have at its deadline: as whoever armed it,
//! with `via == "timer"`. The move's caller decides only when, never who or
//! what:
//!
//! - it acts on the timer the record had armed before the move, on that
//!   record only: it never arms a timer, and a record with none is left as it
//!   is;
//! - it only shortens: a due time later than the timer's deadline is ignored;
//! - the timer keeps its identity, arguments, clock and row. A timer the
//!   caller itself armed or re-armed (a `reset_on` move) is the caller's, and
//!   fires with the caller's standing.
//!
//! So a webhook that records a CI result may make the owner's polling timer
//! check it now, while every move the timer starts is still checked by Cedar
//! as the owner's timer, and the hook gains nothing it could not get by
//! waiting for the deadline.
//!
//! The earlier due time lives in this process only. A restart arms the timer
//! from its row, due at its deadline: the bring-forward is lost, never the
//! timer.

use chrono::{DateTime, Utc};

use super::state_timeouts::{ArmedTimer, Reservation};
use crate::state::ServerState;

impl ServerState {
    /// Bring `armed`, a timer its record had armed, forward to `due_by`, as
    /// the arming the record keeps at journal sequence `journal`. Whether it
    /// was brought forward: not when the record's armed timer is no longer
    /// that arming, or when it is due by then already.
    pub(super) async fn bring_state_timer_forward(
        &self,
        armed: &ArmedTimer,
        journal: u64,
        due_by: DateTime<Utc>,
    ) -> bool {
        let Some(current) =
            self.armed_state_timer(&armed.tenant, &armed.entity_type, &armed.entity_id)
        else {
            return false;
        };
        if current.arm_id != armed.arm_id || current.due_at() <= due_by {
            return false;
        }
        let deadline = current.due_at();
        let mut sooner = current;
        sooner.due_by = Some(due_by);
        // Fenced at the move that brought it forward, so its fire applies to
        // the record as that move left it.
        sooner.sequence_nr = sooner.sequence_nr.max(journal);
        debug_assert!(
            sooner.due_at() <= deadline,
            "POSTCONDITION: bringing a timer forward never makes it later"
        );
        let started = self.start_armed_timer(sooner, Reservation::Sooner).await;
        if started {
            tracing::info!(tenant = %armed.tenant, entity_type = %armed.entity_type,
                entity_id = %armed.entity_id, state = %armed.state, action = %armed.action,
                arm_id = %armed.arm_id, %due_by, %deadline,
                "state timer brought forward by a due_on move");
        }
        started
    }
}
