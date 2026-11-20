//! Event times for catalog rows written before the kernel kept them
//! (ADR-0187, "Backfill"; `temper_runtime::persistence::JournalTimes`).
//!
//! Each start counts, writing nothing, unless `TEMPER_EVENT_TIME_BACKFILL=fill`;
//! then it fills, resumably, at most 128 rows per transaction, as the
//! creation-time migration does ([`crate::first_event`]). A store whose fill
//! has finished does neither.
use std::time::{Duration, Instant};

use crate::PostgresEventStore;
use temper_runtime::persistence::{
    EventTimeBackfillMode, EventTimeFill, EventTimeGaps, EventTimeTally, FirstFill, JournalTimes,
    LastFill, PersistenceError, first_event_at_text, last_event_at_value, log_event_time_gaps,
    storage_error,
};

/// Rows one read or one fill transaction takes.
const PAGE: i64 = 128;

/// A catalog row's key and whether it lacks each time, and whether its state
/// is an object a time can be set in.
type CatalogRow = (String, String, String, bool, bool, bool);
/// The creation event's time, the newest sequence and its time, and the
/// snapshot's sequence.
type JournalRow = (Option<String>, Option<i64>, Option<String>, Option<i64>);

const ROWS_AFTER: &str = "SELECT tenant, entity_type, entity_id, first_event_at IS NULL,
        (state ->> 'last_event_at') IS NULL, COALESCE(jsonb_typeof(state) = 'object', false)
     FROM {schema}entity_catalog
     WHERE (tenant, entity_type, entity_id) > ($1, $2, $3)
     ORDER BY tenant, entity_type, entity_id LIMIT $4";
const TENANT_ROWS_AFTER: &str = "SELECT tenant, entity_type, entity_id, first_event_at IS NULL,
        (state ->> 'last_event_at') IS NULL, COALESCE(jsonb_typeof(state) = 'object', false)
     FROM {schema}entity_catalog
     WHERE tenant = $1 AND (entity_type, entity_id) > ($2, $3)
     ORDER BY entity_type, entity_id LIMIT $4";
/// An event's time is the one the actor stamped (its payload's, which its
/// metadata repeats); the journal row's `created_at` only when neither has one.
const JOURNAL_TIMES: &str = "SELECT
    (SELECT COALESCE(payload->>'timestamp', metadata->>'timestamp',
                     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'))
       FROM {schema}events WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr = 1),
    (SELECT MAX(sequence_nr) FROM {schema}events
       WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3),
    (SELECT COALESCE(payload->>'timestamp', metadata->>'timestamp',
                     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'))
       FROM {schema}events WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3
       ORDER BY sequence_nr DESC LIMIT 1),
    (SELECT sequence_nr FROM {schema}snapshots
       WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)";
/// Sets only a time that is still missing, both in one statement, so a stop
/// leaves a record whole or untouched and a second run writes nothing.
/// `updated_at` and `sequence_nr` stay: this is not a move. The Postgres
/// projection hash covers fields only, so it stays too.
const FILL_ROW: &str = "UPDATE {schema}entity_catalog SET
        first_event_at = COALESCE(first_event_at, $4),
        state = CASE WHEN $5::text IS NOT NULL AND (state ->> 'last_event_at') IS NULL
                          AND jsonb_typeof(state) = 'object'
                     THEN jsonb_set(state, ARRAY['last_event_at'], to_jsonb($5::text))
                     ELSE state END
     WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3
       AND (($4::text IS NOT NULL AND first_event_at IS NULL)
            OR ($5::text IS NOT NULL AND (state ->> 'last_event_at') IS NULL
                AND jsonb_typeof(state) = 'object'))";

fn journal_times(row: &CatalogRow, journal: JournalRow) -> JournalTimes {
    let (creation_at, newest_sequence_nr, newest_at, snapshot_sequence_nr) = journal;
    JournalTimes {
        first_missing: row.3,
        last_missing: row.4,
        state_is_object: row.5,
        has_events: newest_sequence_nr.is_some(),
        creation_at,
        newest_sequence_nr: newest_sequence_nr.map(|seq| seq.max(0) as u64),
        newest_at,
        snapshot_sequence_nr: snapshot_sequence_nr.map(|seq| seq.max(0) as u64),
    }
}

/// What `plan` writes: the creation time's text and the last time's JSON
/// string, each when filled.
fn fill_values(plan: (FirstFill, LastFill)) -> (Option<String>, Option<String>) {
    let first = match plan.0 {
        FirstFill::Fill(at) => Some(first_event_at_text(at)),
        _ => None,
    };
    let last = match plan.1 {
        LastFill::Fill(at) => last_event_at_value(at).as_str().map(str::to_string),
        _ => None,
    };
    (first, last)
}

impl PostgresEventStore {
    /// Start counting, or filling under `TEMPER_EVENT_TIME_BACKFILL=fill`,
    /// after schema migrations, without holding up the first serve.
    pub fn start_event_time_backfill(&self) {
        let mode = EventTimeBackfillMode::from_env();
        let store = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let complete: Result<Option<bool>, _> = crate::dbm::postgres_query_scalar!(
                &store.schema,
                "SELECT complete FROM {schema}event_time_backfill WHERE id = 1"
            )
            .fetch_optional(store.pool())
            .await;
            match complete {
                Ok(Some(true)) => return,
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(%error, "event time backfill could not read its cursor");
                    return;
                }
            }
            if mode == EventTimeBackfillMode::Fill {
                match store.fill_event_times().await {
                    Ok(totals) => totals.log_complete(started.elapsed().as_millis() as u64),
                    Err(error) => {
                        tracing::error!(%error, "event time backfill stopped; resumes on next start");
                        return;
                    }
                }
            }
            match store.event_time_gaps(None).await {
                Ok(gaps) => log_event_time_gaps(mode, &gaps, started.elapsed().as_millis() as u64),
                Err(error) => tracing::error!(%error, "event time backfill count failed"),
            }
        });
    }

    /// Fill every catalog row its journal can, from the durable cursor to the
    /// end; what a start runs under `TEMPER_EVENT_TIME_BACKFILL=fill`. Once
    /// finished it returns the totals and writes nothing more.
    pub async fn fill_event_times(&self) -> Result<EventTimeFill, PersistenceError> {
        loop {
            if let Some(totals) = self.backfill_event_time_batch().await? {
                return Ok(totals);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Count, per tenant and entity type, the catalog rows missing an event
    /// time and what their journals could fill; `tenant` limits it to one.
    /// Reads only.
    pub async fn event_time_gaps(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<EventTimeGaps>, PersistenceError> {
        let mut tally = EventTimeTally::default();
        let mut cursor = (
            tenant.unwrap_or_default().to_string(),
            String::new(),
            String::new(),
        );
        loop {
            let page: Vec<CatalogRow> = match tenant {
                Some(tenant) => {
                    crate::dbm::postgres_query_as!(&self.schema, TENANT_ROWS_AFTER)
                        .bind(tenant)
                        .bind(&cursor.1)
                        .bind(&cursor.2)
                        .bind(PAGE)
                        .fetch_all(self.pool())
                        .await
                }
                None => {
                    crate::dbm::postgres_query_as!(&self.schema, ROWS_AFTER)
                        .bind(&cursor.0)
                        .bind(&cursor.1)
                        .bind(&cursor.2)
                        .bind(PAGE)
                        .fetch_all(self.pool())
                        .await
                }
            }
            .map_err(storage_error)?;
            for row in &page {
                let plan = if row.3 || row.4 {
                    let journal: JournalRow =
                        crate::dbm::postgres_query_as!(&self.schema, JOURNAL_TIMES)
                            .bind(&row.0)
                            .bind(&row.1)
                            .bind(&row.2)
                            .fetch_one(self.pool())
                            .await
                            .map_err(storage_error)?;
                    journal_times(row, journal).plan()
                } else {
                    (FirstFill::Present, LastFill::Present)
                };
                tally.record(&row.0, &row.1, plan.0, plan.1);
            }
            match page.last() {
                Some(last) if page.len() as i64 == PAGE => {
                    cursor = (last.0.clone(), last.1.clone(), last.2.clone())
                }
                _ => return Ok(tally.into_gaps()),
            }
        }
    }

    /// The next [`PAGE`] records after the cursor, filled and committed with
    /// the cursor in one transaction, so a restart carries on after them.
    pub(crate) async fn backfill_event_time_batch(
        &self,
    ) -> Result<Option<EventTimeFill>, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}event_time_backfill (id) VALUES (1) ON CONFLICT (id) DO NOTHING"
        )
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        let (tenant, entity_type, entity_id, records, first_filled, last_filled, rows_written, complete): (
            String, String, String, i64, i64, i64, i64, bool,
        ) = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT tenant, entity_type, entity_id, records, first_filled, last_filled, rows_written, complete
             FROM {schema}event_time_backfill WHERE id = 1 FOR UPDATE"
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(storage_error)?;
        let mut totals = EventTimeFill {
            records: records.max(0) as u64,
            first_filled: first_filled.max(0) as u64,
            last_filled: last_filled.max(0) as u64,
            rows_written: rows_written.max(0) as u64,
        };
        if complete {
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some(totals));
        }
        let page: Vec<CatalogRow> = crate::dbm::postgres_query_as!(&self.schema, ROWS_AFTER)
            .bind(&tenant)
            .bind(&entity_type)
            .bind(&entity_id)
            .bind(PAGE)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage_error)?;
        for row in &page {
            if !(row.3 || row.4) {
                totals.add((FirstFill::Present, LastFill::Present), 0);
                continue;
            }
            let journal: JournalRow = crate::dbm::postgres_query_as!(&self.schema, JOURNAL_TIMES)
                .bind(&row.0)
                .bind(&row.1)
                .bind(&row.2)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
            let plan = journal_times(row, journal).plan();
            let (first, last) = fill_values(plan);
            let written = if first.is_some() || last.is_some() {
                crate::dbm::postgres_query!(&self.schema, FILL_ROW)
                    .bind(&row.0)
                    .bind(&row.1)
                    .bind(&row.2)
                    .bind(first)
                    .bind(last)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?
                    .rows_affected()
            } else {
                0
            };
            totals.add(plan, written);
        }
        let done = (page.len() as i64) < PAGE;
        let (tenant, entity_type, entity_id) = page
            .last()
            .map(|row| (row.0.clone(), row.1.clone(), row.2.clone()))
            .unwrap_or((tenant, entity_type, entity_id));
        crate::dbm::postgres_query!(
            &self.schema,
            "UPDATE {schema}event_time_backfill SET tenant = $1, entity_type = $2, entity_id = $3,
                 records = $4, first_filled = $5, last_filled = $6, rows_written = $7, complete = $8
             WHERE id = 1"
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(totals.records as i64)
        .bind(totals.first_filled as i64)
        .bind(totals.last_filled as i64)
        .bind(totals.rows_written as i64)
        .bind(done)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(done.then_some(totals))
    }
}

#[cfg(test)]
#[path = "event_time_backfill_test.rs"]
mod tests;
