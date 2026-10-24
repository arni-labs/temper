//! Pending scheduled actions, kept so a restart arms them again. They are
//! written in the transaction of the event that armed them, and removed in the
//! transaction of the event that fired them.

use sqlx::PgConnection;
use temper_runtime::persistence::{PersistenceError, ScheduledActionRecord, storage_error};

use crate::PostgresEventStore;

type ScheduledActionColumns = (String, String, String, String, String, String);

impl PostgresEventStore {
    /// Write the scheduled actions an appended event armed, on the append's
    /// transaction. Writing the same arm again leaves its row as it is.
    pub(crate) async fn insert_scheduled_actions(
        &self,
        conn: &mut PgConnection,
        actions: &[ScheduledActionRecord],
    ) -> Result<(), PersistenceError> {
        for action in actions {
            crate::dbm::postgres_query!(
                &self.schema,
                "INSERT INTO {schema}scheduled_actions \
                     (tenant, entity_type, entity_id, arm_id, action, due_at, armed_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (tenant, entity_type, entity_id, arm_id) DO NOTHING",
            )
            .bind(&action.tenant)
            .bind(&action.entity_type)
            .bind(&action.entity_id)
            .bind(&action.arm_id)
            .bind(&action.action)
            .bind(&action.due_at)
            .bind(&action.armed_by)
            .execute(&mut *conn)
            .await
            .map_err(storage_error)?;
        }
        Ok(())
    }

    /// Remove one scheduled action's row.
    pub(crate) async fn remove_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self.pool().acquire().await.map_err(storage_error)?;
        self.delete_scheduled_action_row(&mut conn, tenant, entity_type, entity_id, arm_id)
            .await
    }

    /// Remove one scheduled action's row on `conn`, which may be an append's
    /// transaction.
    pub(crate) async fn delete_scheduled_action_row(
        &self,
        conn: &mut PgConnection,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}scheduled_actions \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND arm_id = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(arm_id)
        .execute(&mut *conn)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Every pending scheduled action for `tenant`.
    pub(crate) async fn scheduled_action_rows(
        &self,
        tenant: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        let rows: Vec<ScheduledActionColumns> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT entity_type, entity_id, arm_id, action, due_at, armed_by \
             FROM {schema}scheduled_actions WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(
                |(entity_type, entity_id, arm_id, action, due_at, armed_by)| {
                    ScheduledActionRecord {
                        tenant: tenant.to_string(),
                        entity_type,
                        entity_id,
                        arm_id,
                        action,
                        due_at,
                        armed_by,
                    }
                },
            )
            .collect())
    }
}
