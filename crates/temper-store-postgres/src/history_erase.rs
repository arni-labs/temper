//! Erase a deleted entity's history in one transaction (see `HistoryErase`).

use sqlx::{PgConnection, Row};
use temper_runtime::persistence::{
    HistoryErase, HistoryErased, PersistenceError, overflow_blob_keys,
};

use crate::PostgresEventStore;

fn storage(error: sqlx::Error) -> PersistenceError {
    PersistenceError::Storage(error.to_string())
}

impl PostgresEventStore {
    /// Remove every event before the tombstone, every snapshot, trajectory, WASM
    /// invocation and pending decision of one deleted entity, keeping the
    /// tombstone (redacted) and one minimal snapshot just before it.
    ///
    /// # Errors
    ///
    /// Refuses, changing nothing, when the journal does not end in a `Deleted`
    /// event at `tombstone_sequence_nr`.
    pub async fn erase_entity_history(
        &self,
        erase: HistoryErase<'_>,
    ) -> Result<HistoryErased, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage)?;
        let erased = erase_in(&self.schema, &mut tx, &erase).await?;
        tx.commit().await.map_err(storage)?;
        Ok(erased)
    }
}

async fn erase_in(
    schema: &crate::PostgresSchema,
    conn: &mut PgConnection,
    erase: &HistoryErase<'_>,
) -> Result<HistoryErased, PersistenceError> {
    let HistoryErase {
        tenant,
        entity_type,
        entity_id,
        tombstone_sequence_nr,
        tombstone_payload,
        snapshot,
    } = *erase;
    let seq = tombstone_sequence_nr as i64;
    let q = |sql: &'static str| schema.qualify_sql(sql);

    let last = sqlx::query(&q(
        "SELECT sequence_nr, event_type, segment_index FROM {schema}events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
         ORDER BY sequence_nr DESC LIMIT 1 FOR UPDATE",
    ))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    let Some(last) = last else {
        return Err(PersistenceError::Storage(format!(
            "{entity_type}:{entity_id} has no journal"
        )));
    };
    let (last_seq, last_type, segment): (i64, String, i64) =
        (last.get(0), last.get(1), last.get(2));
    if last_seq != seq || last_type != "Deleted" {
        return Err(PersistenceError::Storage(format!(
            "{entity_type}:{entity_id} does not end in a Deleted event at sequence {seq}"
        )));
    }

    // Overflow blobs the erased payloads and snapshots point at.
    let mut candidates = std::collections::BTreeSet::new();
    for sql in [
        "SELECT payload::text FROM {schema}events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr < $4",
        "SELECT convert_from(state, 'UTF8') FROM {schema}snapshots \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr <= $4",
        "SELECT convert_from(state, 'UTF8') FROM {schema}snapshot_history \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr <= $4",
    ] {
        let texts: Vec<(String,)> = sqlx::query_as(&q(sql))
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(seq)
            .fetch_all(&mut *conn)
            .await
            .map_err(storage)?;
        for (text,) in texts {
            candidates.extend(overflow_blob_keys(&text));
        }
    }

    let mut erased = HistoryErased {
        events: sqlx::query(&q("DELETE FROM {schema}events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr < $4"))
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(seq)
        .execute(&mut *conn)
        .await
        .map_err(storage)?
        .rows_affected(),
        ..HistoryErased::default()
    };
    let payload: serde_json::Value = serde_json::from_str(tombstone_payload)
        .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
    sqlx::query(&q("UPDATE {schema}events SET payload = $5 \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr = $4"))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(seq)
    .bind(payload)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    erased.snapshots = delete_rows(
        &mut *conn,
        &q("DELETE FROM {schema}snapshot_history \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"),
        erase,
    )
    .await?;
    erased.snapshots += delete_rows(&mut *conn, &q("DELETE FROM {schema}snapshots WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"), erase).await?;
    if seq > 1 {
        sqlx::query(&q(
            "INSERT INTO {schema}snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5)",
        ))
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(seq - 1)
        .bind(snapshot)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
    }
    delete_rows(
        &mut *conn,
        &q("DELETE FROM {schema}event_segments \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"),
        erase,
    )
    .await?;
    sqlx::query(&q(
        "INSERT INTO {schema}event_segments (tenant, entity_type, entity_id, segment_index, \
         start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, 1, now())",
    ))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(segment)
    .bind(seq)
    .bind((seq > 1).then_some(seq - 1))
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    erased.trajectories = delete_rows(&mut *conn, &q("DELETE FROM {schema}trajectories WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"), erase).await?;
    erased.wasm_invocations = delete_rows(
        &mut *conn,
        &q("DELETE FROM {schema}wasm_invocation_logs \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"),
        erase,
    )
    .await?;
    erased.pending_decisions = delete_rows(
        &mut *conn,
        &q("DELETE FROM {schema}pending_decisions WHERE tenant = $1 \
         AND data->>'resource_type' = $2 AND data->>'resource_id' = $3"),
        erase,
    )
    .await?;

    // A blob another event or snapshot of this type still names stays.
    for key in candidates {
        let pattern = format!("%{key}%");
        let held: Option<(i32,)> = sqlx::query_as(&q(
            "SELECT 1 FROM {schema}events WHERE tenant = $1 AND entity_type = $2 \
               AND payload::text LIKE $3 \
             UNION ALL SELECT 1 FROM {schema}snapshots WHERE tenant = $1 AND entity_type = $2 \
               AND convert_from(state, 'UTF8') LIKE $3 LIMIT 1",
        ))
        .bind(tenant)
        .bind(entity_type)
        .bind(&pattern)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        if held.is_none() {
            erased.orphaned_blobs.push(key);
        }
    }
    Ok(erased)
}

/// Delete the rows of `sql` (bound to the tenant, type and id) and count them.
async fn delete_rows(
    conn: &mut PgConnection,
    sql: &str,
    erase: &HistoryErase<'_>,
) -> Result<u64, PersistenceError> {
    Ok(sqlx::query(sql)
        .bind(erase.tenant)
        .bind(erase.entity_type)
        .bind(erase.entity_id)
        .execute(conn)
        .await
        .map_err(storage)?
        .rows_affected())
}
