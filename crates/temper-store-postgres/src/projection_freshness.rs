//! Which projection catalog rows still match their entity's journal.

use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;
use crate::platform::storage_error;

impl PostgresEventStore {
    /// Every projection catalog row of `entity_type` as `(entity_id, current)`.
    ///
    /// `current` is true when the entity's journal holds no event newer than
    /// the one the row was projected from, so the row and its field index
    /// entries hold the entity's committed fields. It is false for a committed
    /// change whose projection has not landed, and for a deleted entity whose
    /// row is still there. The `EXISTS` probe is one seek on the journal's
    /// `(tenant, entity_type, entity_id, sequence_nr)` unique index.
    pub async fn projection_catalog_freshness_pg(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<(String, bool)>, PersistenceError> {
        crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT c.entity_id, \
                    NOT EXISTS ( \
                      SELECT 1 \
                      FROM {schema}events e \
                      WHERE e.tenant = c.tenant \
                        AND e.entity_type = c.entity_type \
                        AND e.entity_id = c.entity_id \
                        AND e.sequence_nr > c.sequence_nr \
                    ) AS current \
             FROM {schema}entity_catalog c \
             WHERE c.tenant = $1 AND c.entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(self.pool())
        .await
        .map_err(storage_error)
    }
}
