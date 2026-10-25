//! A record's query-plane rows (its `entity_catalog` row and its
//! `entity_field_index` rows), written inside a transaction the caller owns.
//!
//! The query plane's own upsert and removal run these in a transaction of
//! their own. An append that carries the record's row
//! (`CommitRows::projection`) runs them in the event's transaction, so the
//! event and its row commit together.

use sqlx::{Postgres, Transaction};
use temper_runtime::persistence::PersistenceError;

use super::{
    CatalogProjectionFingerprint, QueryProjectionCatalogUpdate, canonical_projection_status,
    json_hash, reconcile_query_projection_field_index, scalar_index_fields, storage_error,
    update_query_projection_catalog_row,
};

/// A record's query-plane row at `sequence_nr`.
pub(crate) struct ProjectionWrite<'a> {
    pub(crate) tenant: &'a str,
    pub(crate) entity_type: &'a str,
    pub(crate) entity_id: &'a str,
    pub(crate) status: &'a str,
    pub(crate) fields: &'a serde_json::Value,
    pub(crate) state: &'a serde_json::Value,
    pub(crate) sequence_nr: u64,
}

/// What a projection write did, for its metrics.
pub(crate) struct ProjectionWritten {
    /// `insert`, `diff`, `skipped_unchanged` (the field index already held
    /// these values), or `stale_skipped` (a newer row was already there).
    pub(crate) path: &'static str,
    pub(crate) indexed_fields: u64,
    pub(crate) skipped_fields: u64,
}

/// The catalog row's status, hash and sequence, locked for this transaction.
async fn locked_catalog_row(
    schema: &crate::PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    write: &ProjectionWrite<'_>,
) -> Result<Option<CatalogProjectionFingerprint>, PersistenceError> {
    crate::dbm::postgres_query_as!(
        schema,
        "SELECT status, projection_hash, sequence_nr \
         FROM {schema}entity_catalog \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
         FOR UPDATE",
    )
    .bind(write.tenant)
    .bind(write.entity_type)
    .bind(write.entity_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(storage_error)
}

/// Write the record's catalog row and reconcile its field index, unless a
/// row at a higher sequence is already there.
pub(crate) async fn write_projection(
    schema: &crate::PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    write: ProjectionWrite<'_>,
) -> Result<ProjectionWritten, PersistenceError> {
    let status = canonical_projection_status(write.status, write.state);
    let projection_hash = json_hash(write.fields);
    let (new_index, indexed_fields, skipped_fields) = scalar_index_fields(write.fields);
    let written = |path| ProjectionWritten {
        path,
        indexed_fields,
        skipped_fields,
    };
    let new_sequence_nr = write.sequence_nr as i64;
    let is_newer = |row: &Option<CatalogProjectionFingerprint>| {
        row.as_ref()
            .is_some_and(|(_, _, existing)| *existing > new_sequence_nr)
    };
    let update = QueryProjectionCatalogUpdate {
        tenant: write.tenant,
        entity_type: write.entity_type,
        entity_id: write.entity_id,
        status,
        fields: write.fields,
        state: write.state,
        sequence_nr: write.sequence_nr,
        projection_hash: projection_hash.as_str(),
    };

    let mut previous = locked_catalog_row(schema, tx, &write).await?;
    if is_newer(&previous) {
        return Ok(written("stale_skipped"));
    }
    if previous.is_some() {
        update_query_projection_catalog_row(schema, tx, update).await?;
    } else {
        let inserted: Option<i32> = crate::dbm::postgres_query_scalar!(schema,
            "INSERT INTO {schema}entity_catalog \
             (tenant, entity_type, entity_id, status, fields, state, sequence_nr, projection_version, projection_hash, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 2, $8, now()) \
             ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING \
             RETURNING 1",
        )
        .bind(write.tenant)
        .bind(write.entity_type)
        .bind(write.entity_id)
        .bind(status)
        .bind(write.fields)
        .bind(write.state)
        .bind(new_sequence_nr)
        .bind(projection_hash.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage_error)?;
        if inserted.is_none() {
            // Another writer inserted the row between the read and the insert.
            previous = locked_catalog_row(schema, tx, &write).await?;
            if is_newer(&previous) {
                return Ok(written("stale_skipped"));
            }
            update_query_projection_catalog_row(schema, tx, update).await?;
        }
    }

    let unchanged = previous.as_ref().is_some_and(|(old_status, old_hash, _)| {
        old_status.as_str() == status && old_hash.as_str() == projection_hash.as_str()
    });
    if unchanged {
        return Ok(written("skipped_unchanged"));
    }
    reconcile_query_projection_field_index(
        schema,
        tx,
        write.tenant,
        write.entity_type,
        write.entity_id,
        status,
        &new_index,
    )
    .await?;
    Ok(written(if previous.is_some() { "diff" } else { "insert" }))
}

/// Remove the record's catalog row and its field-index rows.
pub(crate) async fn remove_projection(
    schema: &crate::PostgresSchema,
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        schema,
        "DELETE FROM {schema}entity_catalog WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    crate::dbm::postgres_query!(
        schema,
        "DELETE FROM {schema}entity_field_index WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(storage_error)?;
    Ok(())
}
