//! Armed state timers (ADR-0056), kept so a restart arms them again.

use sqlx::PgConnection;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord, storage_error};

use crate::PostgresEventStore;

type StateTimerColumns = (String, String, String, String, i64, String, String, String);

impl PostgresEventStore {
    /// Record an armed timer. A row armed at a higher sequence is never
    /// replaced by one armed earlier, whatever order the writes arrive in.
    pub async fn upsert_state_timer(
        &self,
        timer: &StateTimerRecord,
    ) -> Result<(), PersistenceError> {
        let mut conn = self.pool().acquire().await.map_err(storage_error)?;
        self.write_state_timer_row(&mut conn, timer).await
    }

    /// [`Self::upsert_state_timer`], on `conn`: in the transaction of the
    /// event that armed the timer, when the append carries its row.
    pub(crate) async fn write_state_timer_row(
        &self,
        conn: &mut PgConnection,
        timer: &StateTimerRecord,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}state_timers AS existing \
                 (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET \
                 state = EXCLUDED.state, \
                 arm_id = EXCLUDED.arm_id, \
                 sequence_nr = EXCLUDED.sequence_nr, \
                 started_at = EXCLUDED.started_at, \
                 params = EXCLUDED.params, \
                 armed_by = EXCLUDED.armed_by \
             WHERE EXCLUDED.sequence_nr >= existing.sequence_nr",
        )
        .bind(&timer.tenant)
        .bind(&timer.entity_type)
        .bind(&timer.entity_id)
        .bind(&timer.state)
        .bind(&timer.arm_id)
        .bind(i64::try_from(timer.sequence_nr).unwrap_or(i64::MAX))
        .bind(&timer.started_at)
        .bind(&timer.params)
        .bind(&timer.armed_by)
        .execute(&mut *conn)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Remove the entity's timer row, whoever armed it, in the transaction of
    /// the event at `sequence_nr` that left the entity where no timer waits.
    /// A row armed at a later sequence is kept.
    pub(crate) async fn end_state_timer_row(
        &self,
        conn: &mut PgConnection,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}state_timers \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr <= $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(i64::try_from(sequence_nr).unwrap_or(i64::MAX))
        .execute(&mut *conn)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// One entity's timer row, if it has one.
    pub async fn state_timer_row(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<StateTimerRecord>, PersistenceError> {
        let row: Option<StateTimerColumns> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by \
             FROM {schema}state_timers \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(|columns| timer_record(tenant, columns)))
    }

    /// Remove an entity's timer row if it still holds the arming `arm_id`.
    pub async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}state_timers \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND arm_id = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(arm_id)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every armed timer row for `tenant`.
    pub async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows: Vec<StateTimerColumns> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by \
             FROM {schema}state_timers WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|columns| timer_record(tenant, columns))
            .collect())
    }
}

fn timer_record(
    tenant: &str,
    (entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by): StateTimerColumns,
) -> StateTimerRecord {
    StateTimerRecord {
        tenant: tenant.to_string(),
        entity_type,
        entity_id,
        state,
        arm_id,
        sequence_nr: u64::try_from(sequence_nr).unwrap_or_default(),
        started_at,
        params,
        armed_by,
    }
}
