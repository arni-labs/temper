//! Recover the successor rows of a scheduled fire whose answer was lost.

use super::super::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};
use super::{ScheduledTimer, scheduled_timer};
use crate::state::ServerState;

impl ServerState {
    /// Arm the rows `timer`'s fire committed, after a retried fire. A fire
    /// whose answer was lost commits its successors' rows, but the duplicate
    /// that answers its retry hands back no schedules. Only the fire's own
    /// rows are armed: any other row of the record belongs to a dispatch that
    /// arms it itself, after its integrations. Err: the journal or the rows
    /// could not be read, and nothing was armed. Ok: whether the fire already
    /// committed, including when it scheduled no successors.
    async fn arm_fired_rows(&self, timer: &ScheduledTimer, key: &str) -> Result<bool, String> {
        let Some((store, _)) = self.event_journal() else {
            return Ok(false);
        };
        // The fire's event comes after the event that armed it.
        let armed_seq = timer
            .arm_id
            .split_once('.')
            .and_then(|(seq, _)| seq.parse::<u64>().ok())
            .unwrap_or(0);
        let persistence_id = format!("{}:{}:{}", timer.tenant, timer.entity_type, timer.entity_id);
        let events = store
            .read_events(&persistence_id, armed_seq.saturating_sub(1))
            .await
            .map_err(|error| error.to_string())?;
        let Some(fire_seq) = events
            .iter()
            .find(|event| event.payload["idempotency_key"].as_str() == Some(key))
            .map(|event| event.sequence_nr)
        else {
            return Ok(false);
        };
        let rows = store
            .list_scheduled_actions(timer.tenant.as_str())
            .await
            .map_err(|error| error.to_string())?;
        let prefix = format!("{fire_seq}.");
        let fired = rows.iter().filter(|row| {
            row.entity_type == timer.entity_type
                && row.entity_id == timer.entity_id
                && row.arm_id.starts_with(&prefix)
        });
        for row in fired {
            if let Ok(waiting) = scheduled_timer(&timer.tenant, row) {
                self.spawn_scheduled_action(waiting, None);
            }
        }
        Ok(true)
    }

    /// [`Self::arm_fired_rows`] until the rows can be read, waiting longer
    /// after each failure: until then the fire's successors have no timer.
    pub(super) async fn arm_fired_rows_retrying(&self, timer: &ScheduledTimer, key: &str) -> bool {
        let mut wait = FIRST_RETRY_WAIT;
        loop {
            match self.arm_fired_rows(timer, key).await {
                Ok(committed) => return committed,
                Err(error) => {
                    tracing::error!(tenant = %timer.tenant, entity_type = %timer.entity_type,
                        entity_id = %timer.entity_id, arm_id = %timer.arm_id, %error,
                        retry_in_ms = wait.as_millis() as u64,
                        "rows of a retried scheduled fire not readable; retrying");
                    tokio::time::sleep(wait).await; // determinism-ok: retry backoff
                    wait = (wait * 2).min(LAST_RETRY_WAIT);
                }
            }
        }
    }
}
