//! Event times for catalog rows written before the kernel kept them
//! (ADR-0187, "Backfill"; `temper_runtime::persistence::JournalTimes`).
//!
//! Each start counts, writing nothing, unless `TEMPER_EVENT_TIME_BACKFILL=fill`;
//! then it fills, resumably, in batches that hold the write gate as briefly as
//! the creation-time migration's ([`super::first_event`]). A store whose fill
//! has finished does neither.
use std::time::{Duration, Instant};

use super::unchanged_projections::projection_hash;
use super::{TursoEventStore, WritePriority};
use crate::driver::{Row, Rows, params};
use temper_runtime::persistence::{
    EventTimeBackfillMode, EventTimeFill, EventTimeGaps, EventTimeTally, FirstFill, JournalTimes,
    LastFill, PersistenceError, first_event_at_text, last_event_at_value, log_event_time_gaps,
    storage_error,
};

pub(super) const CREATE_BACKFILL_TABLE: &str = "CREATE TABLE IF NOT EXISTS event_time_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tenant TEXT NOT NULL DEFAULT '', entity_type TEXT NOT NULL DEFAULT '', entity_id TEXT NOT NULL DEFAULT '',
    records INTEGER NOT NULL DEFAULT 0, first_filled INTEGER NOT NULL DEFAULT 0,
    last_filled INTEGER NOT NULL DEFAULT 0, rows_written INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0
)";
/// The most rows one fill batch visits, however small they are.
const BATCH_SIZE: usize = 128;
/// Rows one counting read takes; the count holds no write gate.
const COUNT_PAGE: i64 = 256;

/// Whether a catalog row lacks each time, by key order. Its key's index:
/// without it the planner may read every row of a type (driver/plan_check.rs).
const ROWS_AFTER: &str = "SELECT tenant, entity_type, entity_id, first_event_at IS NULL,
        json_extract(state, '$.last_event_at') IS NULL, COALESCE(json_type(state) = 'object', 0)
     FROM entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     WHERE (tenant, entity_type, entity_id) > (?1, ?2, ?3)
     ORDER BY tenant, entity_type, entity_id LIMIT ?4";
/// [`ROWS_AFTER`] within one tenant.
const TENANT_ROWS_AFTER: &str = "SELECT tenant, entity_type, entity_id, first_event_at IS NULL,
        json_extract(state, '$.last_event_at') IS NULL, COALESCE(json_type(state) = 'object', 0)
     FROM entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     WHERE tenant = ?1 AND (entity_type, entity_id) > (?2, ?3)
     ORDER BY entity_type, entity_id LIMIT ?4";
/// What the record's journal holds: the creation event's time, the newest
/// sequence and its time, and the snapshot's sequence. An event's time is the
/// one the actor stamped (its payload's, which its metadata repeats); the
/// journal row's `created_at` only when neither has one. Each probe seeks
/// the record's key.
const JOURNAL_TIMES: &str = "SELECT
    (SELECT COALESCE(json_extract(payload, '$.timestamp'), json_extract(metadata, '$.timestamp'), created_at)
       FROM events INDEXED BY idx_events_entity
       WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr = 1),
    (SELECT MAX(sequence_nr) FROM events INDEXED BY idx_events_entity
       WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3),
    (SELECT COALESCE(json_extract(payload, '$.timestamp'), json_extract(metadata, '$.timestamp'), created_at)
       FROM events INDEXED BY idx_events_entity
       WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
       ORDER BY sequence_nr DESC LIMIT 1),
    (SELECT sequence_nr FROM snapshots INDEXED BY idx_snapshots_checkpoint
       WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3)";
/// The parts of a catalog row its projection hash covers.
const HASHED_ROW: &str = "SELECT status, fields, state, sequence_nr
     FROM entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3";
/// Each write sets only a time that is still missing, so it never replaces
/// one and a second run writes nothing. A state write also needs the row at
/// the sequence it was read at. `updated_at` and `sequence_nr` stay: this is
/// not a move.
const SET_FIRST: &str = "UPDATE entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     SET first_event_at = ?4
     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND first_event_at IS NULL";
const SET_LAST: &str = "UPDATE entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     SET state = ?4, projection_hash = ?5
     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr = ?6
       AND json_extract(state, '$.last_event_at') IS NULL";
const SET_BOTH: &str = "UPDATE entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1
     SET first_event_at = ?4, state = ?5, projection_hash = ?6
     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr = ?7
       AND first_event_at IS NULL AND json_extract(state, '$.last_event_at') IS NULL";

/// A catalog row's key and which times it lacks.
struct CatalogRow {
    key: (String, String, String),
    first_missing: bool,
    last_missing: bool,
    state_is_object: bool,
}

impl CatalogRow {
    fn from_row(row: &Row) -> Result<Self, PersistenceError> {
        Ok(Self {
            key: (
                row.get(0).map_err(storage_error)?,
                row.get(1).map_err(storage_error)?,
                row.get(2).map_err(storage_error)?,
            ),
            first_missing: row.get::<i64>(3).map_err(storage_error)? != 0,
            last_missing: row.get::<i64>(4).map_err(storage_error)? != 0,
            state_is_object: row.get::<i64>(5).map_err(storage_error)? != 0,
        })
    }

    fn missing_any(&self) -> bool {
        self.first_missing || self.last_missing
    }

    fn journal_times(&self, row: &Row) -> Result<JournalTimes, PersistenceError> {
        let newest_sequence_nr: Option<i64> = row.get(1).map_err(storage_error)?;
        let snapshot_sequence_nr: Option<i64> = row.get(3).map_err(storage_error)?;
        Ok(JournalTimes {
            first_missing: self.first_missing,
            last_missing: self.last_missing,
            state_is_object: self.state_is_object,
            has_events: newest_sequence_nr.is_some(),
            creation_at: row.get(0).map_err(storage_error)?,
            newest_sequence_nr: newest_sequence_nr.map(|seq| seq.max(0) as u64),
            newest_at: row.get(2).map_err(storage_error)?,
            snapshot_sequence_nr: snapshot_sequence_nr.map(|seq| seq.max(0) as u64),
        })
    }
}

async fn catalog_rows(mut rows: Rows) -> Result<Vec<CatalogRow>, PersistenceError> {
    let mut found = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage_error)? {
        found.push(CatalogRow::from_row(&row)?);
    }
    Ok(found)
}

impl TursoEventStore {
    pub(super) async fn start_event_time_backfill(&self) -> Result<(), PersistenceError> {
        let mode = EventTimeBackfillMode::from_env();
        // Constant-size probes: a finished fill or an empty catalog needs no
        // background work, and they write nothing.
        let idle = {
            let conn = self.connection().await?;
            let mut rows = conn
                .query(
                    "SELECT (SELECT complete FROM event_time_backfill WHERE id = 1),
                            NOT EXISTS (SELECT 1 FROM entity_catalog LIMIT 1)",
                    (),
                )
                .await
                .map_err(storage_error)?;
            let row = rows
                .next()
                .await
                .map_err(storage_error)?
                .ok_or_else(|| PersistenceError::Storage("missing event time probe".into()))?;
            row.get::<Option<i64>>(0).map_err(storage_error)? == Some(1)
                || row.get::<i64>(1).map_err(storage_error)? != 0
        };
        if idle {
            return Ok(());
        }
        let store = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            if mode == EventTimeBackfillMode::Fill {
                match store.fill_event_times().await {
                    Ok(totals) => totals.log_complete(started.elapsed().as_millis() as u64),
                    Err(error) => {
                        tracing::error!(%error, "event time backfill stopped; resumes on next start");
                        return;
                    }
                }
            }
            // What is left after a fill, or everything a count would fill.
            match store.event_time_gaps(None).await {
                Ok(gaps) => log_event_time_gaps(mode, &gaps, started.elapsed().as_millis() as u64),
                Err(error) => tracing::error!(%error, "event time backfill count failed"),
            }
        });
        Ok(())
    }

    /// Fill every catalog row its journal can, batch after batch, from the
    /// durable cursor to the end; what a start runs under
    /// `TEMPER_EVENT_TIME_BACKFILL=fill`. Once finished it returns the totals
    /// and writes nothing more.
    pub async fn fill_event_times(&self) -> Result<EventTimeFill, PersistenceError> {
        loop {
            if let Some(totals) = self
                .backfill_event_time_batch(super::first_event::BATCH_HOLD)
                .await?
            {
                return Ok(totals);
            }
            // Let a waiting move take the gate between batches.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Count, per tenant and entity type, the catalog rows missing an event
    /// time and what their journals could fill; `tenant` limits it to one.
    /// Reads only: no write gate, nothing written.
    pub async fn event_time_gaps(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<EventTimeGaps>, PersistenceError> {
        let conn = self.connection().await?;
        let mut tally = EventTimeTally::default();
        let mut cursor = (
            tenant.unwrap_or_default().to_string(),
            String::new(),
            String::new(),
        );
        loop {
            let rows = match tenant {
                Some(tenant) => {
                    conn.query(
                        TENANT_ROWS_AFTER,
                        params![tenant, cursor.1.as_str(), cursor.2.as_str(), COUNT_PAGE],
                    )
                    .await
                }
                None => {
                    conn.query(
                        ROWS_AFTER,
                        params![
                            cursor.0.as_str(),
                            cursor.1.as_str(),
                            cursor.2.as_str(),
                            COUNT_PAGE
                        ],
                    )
                    .await
                }
            }
            .map_err(storage_error)?;
            let page = catalog_rows(rows).await?;
            for row in &page {
                let (first, last) = if row.missing_any() {
                    let (tenant, entity_type, entity_id) = &row.key;
                    let mut rows = conn
                        .query(
                            JOURNAL_TIMES,
                            params![tenant.as_str(), entity_type.as_str(), entity_id.as_str()],
                        )
                        .await
                        .map_err(storage_error)?;
                    let found =
                        rows.next().await.map_err(storage_error)?.ok_or_else(|| {
                            PersistenceError::Storage("missing journal probe".into())
                        })?;
                    row.journal_times(&found)?.plan()
                } else {
                    (FirstFill::Present, LastFill::Present)
                };
                tally.record(&row.key.0, &row.key.1, first, last);
            }
            match page.last() {
                Some(last) if page.len() as i64 == COUNT_PAGE => cursor = last.key.clone(),
                _ => return Ok(tally.into_gaps()),
            }
        }
    }

    /// The records after the cursor, in key order, one at a time, committed
    /// with the cursor once the batch has held the write gate for `hold` or
    /// visited [`BATCH_SIZE`] rows. Each record's times are written in one
    /// statement, so a stop leaves a record whole or untouched; the cursor
    /// commits with them, so a restart carries on after the last one.
    pub(super) async fn backfill_event_time_batch(
        &self,
        hold: Duration,
    ) -> Result<Option<EventTimeFill>, PersistenceError> {
        let conn = self
            .write_connection("event_time_backfill", WritePriority::Low)
            .await?;
        let held = Instant::now();
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        tx.execute(
            "INSERT INTO event_time_backfill (id) VALUES (1) ON CONFLICT(id) DO NOTHING",
            (),
        )
        .await
        .map_err(storage_error)?;
        let (mut cursor, mut totals, complete) = {
            let mut rows = tx
                .query(
                    "SELECT tenant, entity_type, entity_id, records, first_filled, last_filled,
                            rows_written, complete
                     FROM event_time_backfill WHERE id = 1",
                    (),
                )
                .await
                .map_err(storage_error)?;
            let row = rows
                .next()
                .await
                .map_err(storage_error)?
                .ok_or_else(|| PersistenceError::Storage("missing event time cursor".into()))?;
            let count = |index| row.get::<i64>(index).map(|n| n.max(0) as u64);
            (
                (
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                    row.get::<String>(2).map_err(storage_error)?,
                ),
                EventTimeFill {
                    records: count(3).map_err(storage_error)?,
                    first_filled: count(4).map_err(storage_error)?,
                    last_filled: count(5).map_err(storage_error)?,
                    rows_written: count(6).map_err(storage_error)?,
                },
                row.get::<i64>(7).map_err(storage_error)? != 0,
            )
        };
        if complete {
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some(totals));
        }
        let mut done = false;
        for _ in 0..BATCH_SIZE {
            // Each read closes before its row is written.
            let row = {
                let rows = tx
                    .query(
                        ROWS_AFTER,
                        params![
                            cursor.0.as_str(),
                            cursor.1.as_str(),
                            cursor.2.as_str(),
                            1_i64
                        ],
                    )
                    .await
                    .map_err(storage_error)?;
                catalog_rows(rows).await?.pop()
            };
            let Some(row) = row else {
                done = true;
                break;
            };
            if row.missing_any() {
                let plan = {
                    let (tenant, entity_type, entity_id) = &row.key;
                    let mut rows = tx
                        .query(
                            JOURNAL_TIMES,
                            params![tenant.as_str(), entity_type.as_str(), entity_id.as_str()],
                        )
                        .await
                        .map_err(storage_error)?;
                    let found =
                        rows.next().await.map_err(storage_error)?.ok_or_else(|| {
                            PersistenceError::Storage("missing journal probe".into())
                        })?;
                    row.journal_times(&found)?.plan()
                };
                let written = fill_row(&tx, &row.key, plan).await?;
                totals.add(plan, written);
            } else {
                totals.add((FirstFill::Present, LastFill::Present), 0);
            }
            cursor = row.key;
            if held.elapsed() >= hold {
                break;
            }
        }
        let (tenant, entity_type, entity_id) = cursor;
        tx.execute(
            "UPDATE event_time_backfill SET tenant = ?1, entity_type = ?2, entity_id = ?3,
                 records = ?4, first_filled = ?5, last_filled = ?6, rows_written = ?7, complete = ?8
             WHERE id = 1",
            params![
                tenant,
                entity_type,
                entity_id,
                totals.records as i64,
                totals.first_filled as i64,
                totals.last_filled as i64,
                totals.rows_written as i64,
                i64::from(done)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(done.then_some(totals))
    }
}

/// Write what `plan` fills on the row at `key`, in one statement. A state
/// write keeps the row's projection hash what a projection write of that
/// state computes, so the next move compares against what the row holds.
/// Returns the rows written: one, or none when nothing is filled.
async fn fill_row(
    tx: &crate::driver::Transaction<'_>,
    key: &(String, String, String),
    plan: (FirstFill, LastFill),
) -> Result<u64, PersistenceError> {
    let (tenant, entity_type, entity_id) = (key.0.as_str(), key.1.as_str(), key.2.as_str());
    let first = match plan.0 {
        FirstFill::Fill(at) => Some(first_event_at_text(at)),
        _ => None,
    };
    let LastFill::Fill(last) = plan.1 else {
        let Some(first) = first else { return Ok(0) };
        return tx
            .execute(SET_FIRST, params![tenant, entity_type, entity_id, first])
            .await
            .map_err(storage_error);
    };
    let (status, fields, state, sequence_nr) = {
        let mut rows = tx
            .query(HASHED_ROW, params![tenant, entity_type, entity_id])
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        (
            row.get::<String>(0).map_err(storage_error)?,
            row.get::<Option<String>>(1).map_err(storage_error)?,
            row.get::<String>(2).map_err(storage_error)?,
            row.get::<i64>(3).map_err(storage_error)?,
        )
    };
    let mut state: serde_json::Value = serde_json::from_str(&state)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let Some(entries) = state.as_object_mut() else {
        return Ok(0);
    };
    entries.insert("last_event_at".into(), last_event_at_value(last));
    let hash = projection_hash(&status, fields.as_deref().unwrap_or_default(), &state);
    let state = serde_json::to_string(&state)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    match first {
        Some(first) => {
            tx.execute(
                SET_BOTH,
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    first,
                    state,
                    hash,
                    sequence_nr
                ],
            )
            .await
        }
        None => {
            tx.execute(
                SET_LAST,
                params![tenant, entity_type, entity_id, state, hash, sequence_nr],
            )
            .await
        }
    }
    .map_err(storage_error)
}
