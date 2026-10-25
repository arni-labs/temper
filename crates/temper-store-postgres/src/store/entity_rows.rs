//! A record's query-plane row and timer row, written in the transaction of
//! the events that change them (`EventStore::commits_entity_rows`).
//!
//! Its catalog row, field-index rows and timer row live in the same database
//! as its journal, so the append writes them before it commits: the move is
//! one commit, and a caller that hangs up after it cannot leave its rows
//! behind.

use sqlx::{Postgres, Transaction};
use temper_runtime::persistence::{PersistenceError, ProjectionRow, StateTimerRow};

use crate::PostgresEventStore;
use crate::metrics::{
    record_postgres_projection_index_fields, record_postgres_projection_index_reconciliation,
};
use crate::platform::projection_rows::{self, ProjectionWrite};

/// The record an append writes rows for, and the sequence its events leave
/// it at.
pub(super) struct RowsAt<'a> {
    pub(super) tenant: &'a str,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) sequence_nr: u64,
}

impl PostgresEventStore {
    /// Write the record's query-plane row and timer row in `tx`. None leaves
    /// a row as it is.
    pub(super) async fn write_entity_rows(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        at: RowsAt<'_>,
        projection: Option<ProjectionRow<'_>>,
        state_timer: Option<StateTimerRow<'_>>,
        operation: &'static str,
    ) -> Result<(), PersistenceError> {
        match projection {
            Some(ProjectionRow::Upsert {
                status,
                fields,
                state,
            }) => {
                let written = projection_rows::write_projection(
                    &self.schema,
                    tx,
                    ProjectionWrite {
                        tenant: at.tenant,
                        entity_type: at.entity_type,
                        entity_id: at.entity_id,
                        status,
                        fields,
                        state,
                        sequence_nr: at.sequence_nr,
                    },
                )
                .await?;
                record_postgres_projection_index_fields(
                    operation,
                    at.entity_type,
                    written.indexed_fields,
                    written.skipped_fields,
                );
                record_postgres_projection_index_reconciliation(operation, written.path);
            }
            Some(ProjectionRow::Remove) => {
                projection_rows::remove_projection(
                    &self.schema,
                    tx,
                    at.tenant,
                    at.entity_type,
                    at.entity_id,
                )
                .await?;
            }
            None => {}
        }
        match state_timer {
            Some(StateTimerRow::Arm(timer)) => {
                debug_assert_eq!(
                    (
                        timer.tenant.as_str(),
                        timer.entity_type.as_str(),
                        timer.entity_id.as_str()
                    ),
                    (at.tenant, at.entity_type, at.entity_id),
                    "a commit's timer row is its own record's"
                );
                self.write_state_timer_row(tx, timer).await?;
            }
            Some(StateTimerRow::End) => {
                self.end_state_timer_row(
                    tx,
                    at.tenant,
                    at.entity_type,
                    at.entity_id,
                    at.sequence_nr,
                )
                .await?;
            }
            None => {}
        }
        Ok(())
    }
}
