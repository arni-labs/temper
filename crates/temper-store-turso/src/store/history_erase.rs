//! Erase a deleted entity's history in one transaction (see `HistoryErase`).

use crate::driver::params;
use temper_runtime::persistence::{
    HistoryErase, HistoryErased, PersistenceError, overflow_blob_keys, storage_error,
};

use super::TursoEventStore;
use super::write_gate::WritePriority;

impl TursoEventStore {
    /// Remove every event before the tombstone, every snapshot, trajectory, WASM
    /// invocation and pending decision of one deleted entity, keeping the
    /// tombstone (redacted) and one minimal snapshot just before it. Freed pages
    /// are zeroed, and a synced file pushes the erase to its remote copy before
    /// this returns.
    ///
    /// # Errors
    ///
    /// Refuses, changing nothing, when the journal does not end in a `Deleted`
    /// event at `tombstone_sequence_nr`; a failed push leaves the erase local.
    pub async fn erase_entity_history(
        &self,
        erase: HistoryErase<'_>,
    ) -> Result<HistoryErased, PersistenceError> {
        let HistoryErase {
            tenant,
            entity_type,
            entity_id,
            tombstone_sequence_nr,
            tombstone_payload,
            snapshot,
        } = erase;
        let seq = tombstone_sequence_nr as i64;
        let _write_permit = self
            .acquire_write_permit("turso.erase_entity_history", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        if !self.is_remote {
            conn.query("PRAGMA secure_delete=ON", ())
                .await
                .map_err(storage_error)?;
        }
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        let key = || params![tenant, entity_type, entity_id];

        let mut rows = tx
            .query(
                "SELECT sequence_nr, event_type, segment_index FROM events \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
                 ORDER BY sequence_nr DESC LIMIT 1",
                key(),
            )
            .await
            .map_err(storage_error)?;
        let Some(last) = rows.next().await.map_err(storage_error)? else {
            return Err(PersistenceError::Storage(format!(
                "{entity_type}:{entity_id} has no journal"
            )));
        };
        let last_seq = last.get::<i64>(0).map_err(storage_error)?;
        let last_type = last.get::<String>(1).map_err(storage_error)?;
        let segment = last.get::<i64>(2).map_err(storage_error)?;
        drop(rows);
        if last_seq != seq || last_type != "Deleted" {
            return Err(PersistenceError::Storage(format!(
                "{entity_type}:{entity_id} does not end in a Deleted event at sequence {seq}"
            )));
        }

        // Overflow blobs the erased payloads and snapshots point at.
        let mut candidates = std::collections::BTreeSet::new();
        for sql in [
            "SELECT payload FROM events \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr < ?4",
            "SELECT CAST(snapshot AS TEXT) FROM snapshots \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr <= ?4",
            "SELECT CAST(snapshot AS TEXT) FROM snapshot_history \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr <= ?4",
        ] {
            let mut rows = tx
                .query(sql, params![tenant, entity_type, entity_id, seq])
                .await
                .map_err(storage_error)?;
            while let Some(row) = rows.next().await.map_err(storage_error)? {
                candidates.extend(overflow_blob_keys(
                    &row.get::<String>(0).map_err(storage_error)?,
                ));
            }
        }

        let mut erased = HistoryErased {
            events: tx
                .execute(
                    "DELETE FROM events WHERE tenant = ?1 AND entity_type = ?2 \
                     AND entity_id = ?3 AND sequence_nr < ?4",
                    params![tenant, entity_type, entity_id, seq],
                )
                .await
                .map_err(storage_error)?,
            ..HistoryErased::default()
        };
        tx.execute(
            "UPDATE events SET payload = ?5 WHERE tenant = ?1 AND entity_type = ?2 \
             AND entity_id = ?3 AND sequence_nr = ?4",
            params![tenant, entity_type, entity_id, seq, tombstone_payload],
        )
        .await
        .map_err(storage_error)?;
        for table in ["snapshot_history", "snapshots"] {
            erased.snapshots += tx
                .execute(
                    &format!(
                        "DELETE FROM {table} WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3"
                    ),
                    key(),
                )
                .await
                .map_err(storage_error)?;
        }
        if seq > 1 {
            tx.execute(
                "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![tenant, entity_type, entity_id, seq - 1, snapshot.to_vec()],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.execute(
            "DELETE FROM event_segments WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            key(),
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO event_segments (tenant, entity_type, entity_id, segment_index, \
             start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 1, datetime('now'))",
            params![
                tenant,
                entity_type,
                entity_id,
                segment,
                seq,
                (seq > 1).then_some(seq - 1)
            ],
        )
        .await
        .map_err(storage_error)?;
        erased.trajectories = tx
            .execute(
                "DELETE FROM trajectories WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                key(),
            )
            .await
            .map_err(storage_error)?;
        erased.wasm_invocations = tx
            .execute(
                "DELETE FROM wasm_invocation_logs \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                key(),
            )
            .await
            .map_err(storage_error)?;
        erased.pending_decisions = tx
            .execute(
                "DELETE FROM pending_decisions WHERE tenant = ?1 \
                 AND json_extract(data, '$.resource_type') = ?2 \
                 AND json_extract(data, '$.resource_id') = ?3",
                key(),
            )
            .await
            .map_err(storage_error)?;

        // A blob another event or snapshot of this type still names stays.
        for blob in candidates {
            let mut rows = tx
                .query(
                    "SELECT 1 FROM events WHERE tenant = ?1 AND entity_type = ?2 \
                       AND instr(payload, ?3) > 0 \
                     UNION ALL SELECT 1 FROM snapshots WHERE tenant = ?1 AND entity_type = ?2 \
                       AND instr(CAST(snapshot AS TEXT), ?3) > 0 LIMIT 1",
                    params![tenant, entity_type, blob.as_str()],
                )
                .await
                .map_err(storage_error)?;
            if rows.next().await.map_err(storage_error)?.is_none() {
                erased.orphaned_blobs.push(blob);
            }
        }
        tx.commit().await.map_err(storage_error)?;
        // The write-ahead log still holds the pages as they were: fold it into
        // the (zeroed) file, after the remote copy has the erase.
        if let Some(copy) = self.sync_copy() {
            copy.push_committed().await?;
            copy.checkpoint_if_wal_over(0).await?;
        } else if !self.is_remote {
            conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await
                .map_err(storage_error)?;
        }
        Ok(erased)
    }
}
