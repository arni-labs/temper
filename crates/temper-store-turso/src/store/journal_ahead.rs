//! Which live entities' journals are ahead of their projection catalog rows.

use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

/// Live ids of one entity type whose journal holds an event the catalog has
/// not projected: no catalog row at all, or a row whose projected sequence
/// (the row's, or a newer unchanged projection's in `entity_catalog_counters`)
/// is below the journal's newest. An entity with the kernel's tombstone is
/// left out. The no-row probe is a catalog primary-key seek per journal row;
/// the behind probe is one seek on `idx_events_entity` per catalog row; each
/// tombstone probe is served by `idx_events_deleted`.
const ENTITY_IDS_AHEAD_OF_CATALOG_SQL: &str = concat!(
    "\
SELECT entity_id
    FROM (
      SELECT DISTINCT e.entity_id
      FROM events e
      WHERE e.tenant = ?1
        AND e.entity_type = ?2
        AND NOT EXISTS (
          SELECT 1
          FROM entity_catalog c
          WHERE c.tenant = e.tenant
            AND c.entity_type = e.entity_type
            AND c.entity_id = e.entity_id
        )
        AND NOT EXISTS (
          SELECT 1
          FROM events d
          WHERE d.tenant = e.tenant
            AND d.entity_type = e.entity_type
            AND d.entity_id = e.entity_id
            ",
    "AND ",
    tombstone_sql!(),
    "
        )
      UNION ALL
      SELECT c.entity_id
      FROM entity_catalog c
      LEFT JOIN entity_catalog_counters k
        ON k.tenant = c.tenant AND k.entity_type = c.entity_type AND k.entity_id = c.entity_id
      WHERE c.tenant = ?1
        AND c.entity_type = ?2
        AND EXISTS (
          SELECT 1
          FROM events e
          WHERE e.tenant = c.tenant
            AND e.entity_type = c.entity_type
            AND e.entity_id = c.entity_id
            AND e.sequence_nr > MAX(c.sequence_nr, COALESCE(k.sequence_nr, 0))
        )
        AND NOT EXISTS (
          SELECT 1
          FROM events d
          WHERE d.tenant = c.tenant
            AND d.entity_type = c.entity_type
            AND d.entity_id = c.entity_id
            ",
    "AND ",
    tombstone_sql!(),
    "
        )
    )
    ORDER BY entity_id"
);

impl TursoEventStore {
    /// Live ids of `entity_type` whose journal is ahead of their catalog row:
    /// no row, or a row projected from an older event than the journal's
    /// newest. Read in one statement, in id order.
    #[instrument(skip_all, fields(
        otel.name = "turso.entity_ids_ahead_of_catalog",
        tenant, entity_type,
    ))]
    pub async fn entity_ids_ahead_of_catalog(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                ENTITY_IDS_AHEAD_OF_CATALOG_SQL,
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }
}
