//! One append and every row its commit writes, in one SQL statement.
//!
//! A move writes its events, its declared keys, its vector rows, the scheduled
//! actions it arms and fires, the record's catalog row and field-index rows,
//! its timer row and its projection-gap mark in a single data-modifying CTE.
//! The statement is its own transaction: it commits all of them or none, in one
//! round trip, and its text never changes, so each connection prepares it once.
//!
//! Every write reads `w`, which has a row only when `chk.ok` holds: the journal
//! head is the expected sequence and no other record holds a key this append
//! claims. A stale expected sequence therefore writes nothing and comes back as
//! `ok = false` with the head it found. Two writers racing past that check meet
//! on the journal's unique key: the second statement aborts, and with it every
//! row it would have written. Every write also reads `ev`'s output through `w`,
//! so the events are inserted before any other row, and a racing writer of the
//! same record fails on the journal rather than on one of its other rows.
//!
//! Every writer of a record's rows takes the record's catalog row first: this
//! statement in `locked` (which every write depends on, through `chk`), the
//! repair writer (`upsert_query_projection_with_state`) by `FOR UPDATE` before
//! its index and mark. After the catalog, this statement writes the events and
//! the record's other rows, then the catalog row, and only then the field
//! index and the gap mark (`cat`, which they join), the order the repair
//! writer takes them in, so the two cannot deadlock. A batch runs this
//! statement per record in persistence-id order. If the catalog row changed
//! between the statement's snapshot and its lock (`raced`: a writer committed
//! while it waited), the snapshot cannot see that writer's field-index rows,
//! so the statement writes nothing and runs once more, on a snapshot taken
//! while its connection already holds the lock (`append_on`), which no writer
//! can then change under it.
//!
//! A record with no catalog row yet has nothing to lock. While the server
//! serves, only the record's own commit creates that row: boot repair creates
//! the missing ones before serving, and the snapshot backfill that runs while
//! serving rewrites existing rows only
//! (`refresh_query_projection_with_state`). Two commits of one record meet on
//! the journal's unique key.
//!
//! The sub-statements of one statement see one snapshot and must not change the
//! same row twice, so each table's writes touch disjoint rows: the field-index
//! delete takes the names the record no longer indexes and the upsert the names
//! it does, the key delete takes a name's old hash and the insert its new one,
//! and so on for segments, vectors, scheduled actions and the gap mark.
//!
//! The rows are the ones the projection writer
//! (`PostgresEventStore::upsert_query_projection_with_state`) writes, byte for
//! byte: the same status, stored state, hash and scalar index.

use std::time::Instant;

use temper_runtime::persistence::{
    CommitRows, EntityKeyRow, PersistenceEnvelope, PersistenceError, ProjectionRow, StateTimerRow,
    catalog_state_for_storage, pack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::PostgresEventStore;
use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_projection_index_fields,
};
use crate::platform::{canonical_projection_status, json_hash, scalar_index_fields, storage_error};
use crate::schema::tombstone_sql;

const EVENT_APPEND_OPERATION: &str = "event_append";

/// Postgres's SQLSTATE for a unique-constraint violation.
const UNIQUE_VIOLATION: &str = "23505";

/// The append statement. Parameters, in order:
/// `$1..$3` the record; `$4` the expected sequence; `$5..$7` the events' types,
/// payloads and metadata; `$8, $9` the declared keys; `$10` whether the record's
/// vector rows become `$11..$13`; `$14..$17` the scheduled actions armed; `$18`
/// the one fired; `$19` the projection (`upsert`, `remove` or NULL) with
/// `$20..$26` its status, fields, stored state, hash, index names and values
/// and whether its fields are wrapped where the index cannot see; `$27` whether
/// the events hold the tombstone; `$28` the timer (`arm`, `end` or NULL) with
/// `$29..$34` its row.
pub(crate) const APPEND_COMMIT_SQL: &str = concat!(
    "WITH locked AS (
    SELECT c.sequence_nr, c.projection_hash FROM {schema}entity_catalog c
    WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3
    FOR UPDATE),
head AS (
    SELECT COALESCE(MAX(sequence_nr), 0)::bigint AS seq FROM {schema}events
    WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3),
clash AS (
    SELECT k.key_name, k.entity_id FROM {schema}entity_key_index k
    JOIN unnest($8::text[], $9::text[]) AS nk(key_name, key_hash)
      ON k.key_name = nk.key_name AND k.key_hash = nk.key_hash
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id <> $3
    ORDER BY k.key_name LIMIT 1),
prev AS (
    SELECT c.sequence_nr, c.projection_hash FROM {schema}entity_catalog c
    WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3),
raced AS (
    SELECT NOT ((NOT EXISTS (SELECT 1 FROM locked) AND NOT EXISTS (SELECT 1 FROM prev))
        OR EXISTS (SELECT 1 FROM locked l JOIN prev p
            ON l.sequence_nr = p.sequence_nr AND l.projection_hash = p.projection_hash))
        AS raced),
chk AS (
    SELECT head.seq AS head, raced.raced,
        head.seq = $4::bigint AND NOT EXISTS (SELECT 1 FROM clash) AND NOT raced.raced AS ok
    FROM head, raced),
open_seg AS (
    SELECT segment_index FROM {schema}event_segments
    WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sealed_at IS NULL
    ORDER BY segment_index DESC LIMIT 1),
seg AS (
    SELECT COALESCE((SELECT segment_index FROM open_seg),
        (SELECT COALESCE(MAX(segment_index), 0) FROM {schema}events
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)) AS idx,
        EXISTS (SELECT 1 FROM open_seg) AS is_open),
ev AS (
    INSERT INTO {schema}events
        (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
    SELECT $1, $2, $3, $4::bigint + e.ord, seg.idx, e.event_type, e.payload, e.metadata
    FROM unnest($5::text[], $6::jsonb[], $7::jsonb[])
        WITH ORDINALITY AS e(event_type, payload, metadata, ord), seg, chk
    WHERE chk.ok
    RETURNING sequence_nr),
w AS (
    SELECT COALESCE((SELECT MAX(sequence_nr) FROM ev), $4::bigint) AS seq
    FROM chk WHERE chk.ok),
seg_new AS (
    INSERT INTO {schema}event_segments AS s
        (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, event_count)
    SELECT $1, $2, $3, seg.idx, $4::bigint + 1, w.seq, w.seq - $4::bigint
    FROM seg, w WHERE NOT seg.is_open AND w.seq > $4::bigint
    ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO UPDATE SET
        end_sequence_nr = EXCLUDED.end_sequence_nr,
        event_count = GREATEST(EXCLUDED.end_sequence_nr - s.start_sequence_nr + 1, 0)),
seg_upd AS (
    UPDATE {schema}event_segments s SET end_sequence_nr = w.seq,
        event_count = GREATEST(w.seq - s.start_sequence_nr + 1, 0)
    FROM seg, w
    WHERE seg.is_open AND w.seq > $4::bigint
      AND s.tenant = $1 AND s.entity_type = $2 AND s.entity_id = $3
      AND s.segment_index = seg.idx),
held AS (
    SELECT k.key_name, k.key_hash FROM {schema}entity_key_index k
    JOIN unnest($8::text[], $9::text[]) AS nk(key_name, key_hash)
      ON k.key_name = nk.key_name AND k.key_hash = nk.key_hash
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id = $3),
key_keep AS (
    UPDATE {schema}entity_key_index k SET sequence_nr = w.seq FROM held h, w
    WHERE k.tenant = $1 AND k.entity_type = $2
      AND k.key_name = h.key_name AND k.key_hash = h.key_hash),
key_drop AS (
    DELETE FROM {schema}entity_key_index k
    USING unnest($8::text[], $9::text[]) AS nk(key_name, key_hash), w
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id = $3
      AND k.key_name = nk.key_name AND k.key_hash <> nk.key_hash),
key_add AS (
    INSERT INTO {schema}entity_key_index
        (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr)
    SELECT $1, $2, nk.key_name, nk.key_hash, $3, w.seq
    FROM unnest($8::text[], $9::text[]) AS nk(key_name, key_hash), w
    WHERE NOT EXISTS (SELECT 1 FROM held h
        WHERE h.key_name = nk.key_name AND h.key_hash = nk.key_hash)),
vec_drop AS (
    DELETE FROM {schema}entity_vector_index v USING w
    WHERE $10::boolean AND v.tenant = $1 AND v.entity_type = $2 AND v.entity_id = $3
      AND NOT EXISTS (SELECT 1 FROM unnest($11::text[], $12::text[]) AS nv(decl_name, model_tag)
          WHERE nv.decl_name = v.decl_name AND nv.model_tag = v.model_tag)),
vec_put AS (
    INSERT INTO {schema}entity_vector_index AS v
        (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr)
    SELECT $1, $2, nv.decl_name, nv.model_tag, $3, nv.vector, w.seq
    FROM unnest($11::text[], $12::text[], $13::bytea[]) AS nv(decl_name, model_tag, vector), w
    WHERE $10::boolean
    ON CONFLICT (tenant, entity_type, decl_name, model_tag, entity_id) DO UPDATE SET
        vector = EXCLUDED.vector, sequence_nr = EXCLUDED.sequence_nr),
sched_add AS (
    INSERT INTO {schema}scheduled_actions
        (tenant, entity_type, entity_id, arm_id, action, due_at, armed_by)
    SELECT $1, $2, $3, s.arm_id, s.action, s.due_at, s.armed_by
    FROM unnest($14::text[], $15::text[], $16::text[], $17::text[])
        AS s(arm_id, action, due_at, armed_by), w
    WHERE s.arm_id IS DISTINCT FROM $18::text
    ON CONFLICT (tenant, entity_type, entity_id, arm_id) DO NOTHING),
sched_fire AS (
    DELETE FROM {schema}scheduled_actions s USING w
    WHERE s.tenant = $1 AND s.entity_type = $2 AND s.entity_id = $3
      AND s.arm_id = $18::text),
go AS (
    SELECT w.seq,
        NOT EXISTS (SELECT 1 FROM prev WHERE prev.projection_hash = $23::text) AS reindex
    FROM w
    WHERE $19::text = 'upsert'
      AND NOT EXISTS (SELECT 1 FROM prev WHERE prev.sequence_nr > w.seq)),
cat_put AS (
    INSERT INTO {schema}entity_catalog AS c
        (tenant, entity_type, entity_id, status, fields, state, sequence_nr,
         projection_version, projection_hash, updated_at, first_event_at)
    SELECT $1, $2, $3, $20::text, $21::jsonb, $22::jsonb, go.seq, 2, $23::text, now(),
        $22::jsonb->>'first_event_at'
    FROM go
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        status = EXCLUDED.status,
        fields = CASE WHEN c.projection_hash IS DISTINCT FROM EXCLUDED.projection_hash
            THEN EXCLUDED.fields ELSE c.fields END,
        sequence_nr = EXCLUDED.sequence_nr,
        projection_version = 2,
        projection_hash = EXCLUDED.projection_hash,
        first_event_at = COALESCE(c.first_event_at, EXCLUDED.first_event_at),
        state = EXCLUDED.state,
        updated_at = now()
    WHERE c.sequence_nr <= EXCLUDED.sequence_nr
    RETURNING 1),
cat_drop AS (
    DELETE FROM {schema}entity_catalog c USING w
    WHERE $19::text = 'remove'
      AND c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3
    RETURNING 1),
cat AS (
    SELECT (SELECT count(*) FROM cat_put) + (SELECT count(*) FROM cat_drop) AS written),
fi_drop AS (
    DELETE FROM {schema}entity_field_index f USING w, cat
    WHERE f.tenant = $1 AND f.entity_type = $2 AND f.entity_id = $3
      AND ($19::text = 'remove' OR EXISTS (SELECT 1 FROM go
          WHERE go.reindex AND f.field_name <> ALL($24::text[])))),
fi_put AS (
    INSERT INTO {schema}entity_field_index AS f
        (tenant, entity_type, entity_id, field_name, field_value)
    SELECT $1, $2, $3, i.field_name, i.field_value
    FROM unnest($24::text[], $25::text[]) AS i(field_name, field_value), go, cat
    WHERE go.reindex
    ON CONFLICT (tenant, entity_type, entity_id, field_name) DO UPDATE SET
        field_value = EXCLUDED.field_value, status = NULL
    WHERE f.field_value IS DISTINCT FROM EXCLUDED.field_value),
mark AS (
    INSERT INTO {schema}entity_projection_delta AS m
        (tenant, entity_type, entity_id, sequence_nr, terminal)
    SELECT $1, $2, $3,
        CASE WHEN $19::text = 'remove' THEN GREATEST(w.seq, COALESCE(
            (SELECT sn.sequence_nr FROM {schema}snapshots sn
             WHERE sn.tenant = $1 AND sn.entity_type = $2 AND sn.entity_id = $3), 0))
        ELSE w.seq END,
        $27::boolean OR ($19::text = 'remove' AND EXISTS (SELECT 1 FROM {schema}events d
            WHERE d.tenant = $1 AND d.entity_type = $2 AND d.entity_id = $3 AND ",
    tombstone_sql!(),
    "))
    FROM w, cat
    WHERE CASE $19::text
        WHEN 'remove' THEN true
        WHEN 'upsert' THEN $26::boolean OR NOT EXISTS (SELECT 1 FROM go)
        ELSE w.seq > $4::bigint END
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        sequence_nr = GREATEST(m.sequence_nr, EXCLUDED.sequence_nr),
        terminal = m.terminal OR EXCLUDED.terminal),
retire AS (
    DELETE FROM {schema}entity_projection_delta m USING go, cat
    WHERE NOT $26::boolean
      AND m.tenant = $1 AND m.entity_type = $2 AND m.entity_id = $3
      AND m.sequence_nr <= go.seq AND NOT m.terminal
      AND CASE WHEN go.reindex THEN cardinality($24::text[]) > 0
          ELSE EXISTS (SELECT 1 FROM {schema}entity_field_index f
              WHERE f.tenant = $1 AND f.entity_type = $2 AND f.entity_id = $3) END),
timer_arm AS (
    INSERT INTO {schema}state_timers AS t
        (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by)
    SELECT $1, $2, $3, $29::text, $30::text, $31::bigint, $32::text, $33::text, $34::text
    FROM w WHERE $28::text = 'arm'
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        state = EXCLUDED.state,
        arm_id = EXCLUDED.arm_id,
        sequence_nr = EXCLUDED.sequence_nr,
        started_at = EXCLUDED.started_at,
        params = EXCLUDED.params,
        armed_by = EXCLUDED.armed_by
    WHERE EXCLUDED.sequence_nr >= t.sequence_nr),
timer_end AS (
    DELETE FROM {schema}state_timers t USING w
    WHERE $28::text = 'end'
      AND t.tenant = $1 AND t.entity_type = $2 AND t.entity_id = $3)
SELECT chk.ok, chk.head, (SELECT seq FROM w),
    (SELECT key_name FROM clash), (SELECT entity_id FROM clash), chk.raced
FROM chk"
);

/// What the statement answers: whether the checks held, the journal head it
/// found, the sequence it wrote at, and the key another record holds.
type AppendAnswer = (bool, i64, Option<i64>, Option<String>, Option<String>, bool);

/// One append's parameters, owned and laid out as the statement's arrays.
struct AppendParams<'a> {
    record: [&'a str; 3],
    expected_sequence: u64,
    event_types: Vec<&'a str>,
    payloads: Vec<&'a serde_json::Value>,
    metadata: Vec<serde_json::Value>,
    tombstone: bool,
    key_names: Vec<&'a str>,
    key_hashes: Vec<&'a str>,
    reconcile_vectors: bool,
    vector_decls: Vec<&'a str>,
    vector_models: Vec<&'a str>,
    vectors: Vec<Vec<u8>>,
    arm_ids: Vec<&'a str>,
    arm_actions: Vec<&'a str>,
    arm_due_at: Vec<&'a str>,
    arm_armed_by: Vec<&'a str>,
    fired: Option<&'a str>,
    projection: ProjectionParams<'a>,
    timer: TimerParams<'a>,
}

/// The catalog and field-index rows, as the projection writer computes them.
#[derive(Default)]
struct ProjectionParams<'a> {
    op: Option<&'static str>,
    status: &'a str,
    fields: Option<&'a serde_json::Value>,
    stored_state: serde_json::Value,
    hash: String,
    index_names: Vec<String>,
    index_values: Vec<String>,
    wrapped: bool,
    indexed_fields: u64,
    skipped_fields: u64,
}

/// The timer row: `arm` with its row, `end`, or nothing.
#[derive(Default)]
struct TimerParams<'a> {
    op: Option<&'static str>,
    state: &'a str,
    arm_id: &'a str,
    sequence_nr: i64,
    started_at: &'a str,
    params: &'a str,
    armed_by: &'a str,
}

impl<'a> AppendParams<'a> {
    /// Lay out `rows` for the statement.
    ///
    /// # Errors
    ///
    /// A row of another record, or a declared key named twice: the statement
    /// would write it under this record, or change one key row twice.
    fn new(
        record: [&'a str; 3],
        expected_sequence: u64,
        events: &'a [PersistenceEnvelope],
        rows: CommitRows<'a>,
    ) -> Result<Self, PersistenceError> {
        let metadata = events
            .iter()
            .map(|event| serde_json::to_value(&event.metadata))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        check_distinct_key_names(rows.key_rows)?;
        for action in rows.scheduled_actions {
            check_same_record(
                record,
                [&action.tenant, &action.entity_type, &action.entity_id],
            )?;
        }
        let vector_rows = if rows.reconcile_vectors {
            rows.vector_rows
        } else {
            &[]
        };
        let params = Self {
            record,
            expected_sequence,
            event_types: events.iter().map(|e| e.event_type.as_str()).collect(),
            payloads: events.iter().map(|e| &e.payload).collect(),
            metadata,
            tombstone: events.iter().any(PersistenceEnvelope::is_tombstone),
            key_names: rows.key_rows.iter().map(|k| k.key_name.as_str()).collect(),
            key_hashes: rows.key_rows.iter().map(|k| k.key_hash.as_str()).collect(),
            reconcile_vectors: rows.reconcile_vectors,
            vector_decls: vector_rows.iter().map(|v| v.decl_name.as_str()).collect(),
            vector_models: vector_rows.iter().map(|v| v.model_tag.as_str()).collect(),
            vectors: vector_rows.iter().map(|v| pack_f32_le(&v.vector)).collect(),
            arm_ids: rows
                .scheduled_actions
                .iter()
                .map(|a| a.arm_id.as_str())
                .collect(),
            arm_actions: rows
                .scheduled_actions
                .iter()
                .map(|a| a.action.as_str())
                .collect(),
            arm_due_at: rows
                .scheduled_actions
                .iter()
                .map(|a| a.due_at.as_str())
                .collect(),
            arm_armed_by: rows
                .scheduled_actions
                .iter()
                .map(|a| a.armed_by.as_str())
                .collect(),
            fired: rows.fired_scheduled_action,
            projection: ProjectionParams::new(rows.projection),
            timer: TimerParams::new(record, rows.state_timer)?,
        };
        debug_assert_eq!(params.event_types.len(), params.metadata.len());
        debug_assert!(params.index_lengths_match());
        Ok(params)
    }

    fn index_lengths_match(&self) -> bool {
        self.key_names.len() == self.key_hashes.len()
            && self.vector_decls.len() == self.vectors.len()
            && self.arm_ids.len() == self.arm_armed_by.len()
            && self.projection.index_names.len() == self.projection.index_values.len()
    }
}

impl<'a> ProjectionParams<'a> {
    fn new(projection: Option<ProjectionRow<'a>>) -> Self {
        match projection {
            None => Self::default(),
            Some(ProjectionRow::Remove) => Self {
                op: Some("remove"),
                ..Self::default()
            },
            Some(ProjectionRow::Upsert {
                status,
                fields,
                state,
            }) => {
                let (index, indexed_fields, skipped_fields) = scalar_index_fields(fields);
                let (index_names, index_values) = index.into_iter().unzip();
                Self {
                    op: Some("upsert"),
                    status: canonical_projection_status(status, state),
                    fields: Some(fields),
                    // The row holds each field once: in `fields`, not again in `state`.
                    stored_state: catalog_state_for_storage(fields, state),
                    hash: json_hash(fields),
                    index_names,
                    index_values,
                    // The field index does not look inside wrapped legacy
                    // fields, so such a record stays in the gap.
                    wrapped: fields
                        .get("fields")
                        .is_some_and(serde_json::Value::is_object),
                    indexed_fields,
                    skipped_fields,
                }
            }
        }
    }
}

impl<'a> TimerParams<'a> {
    fn new(
        record: [&'a str; 3],
        timer: Option<StateTimerRow<'a>>,
    ) -> Result<Self, PersistenceError> {
        Ok(match timer {
            None => Self::default(),
            Some(StateTimerRow::End) => Self {
                op: Some("end"),
                ..Self::default()
            },
            Some(StateTimerRow::Arm(row)) => {
                check_same_record(record, [&row.tenant, &row.entity_type, &row.entity_id])?;
                Self {
                    op: Some("arm"),
                    state: &row.state,
                    arm_id: &row.arm_id,
                    sequence_nr: stored_sequence(row.sequence_nr),
                    started_at: &row.started_at,
                    params: &row.params,
                    armed_by: &row.armed_by,
                }
            }
        })
    }
}

impl PostgresEventStore {
    /// Append `events` to the record's journal with every row in `rows`, in
    /// one statement. Returns the record's sequence after the append.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::ConcurrencyViolation`] when the journal is not at
    /// `expected_sequence`, or another writer appended first; a storage error
    /// when another record holds a declared key, or the statement fails. Either
    /// way nothing was written.
    pub(crate) async fn append_commit(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        let mut timer = PostgresTransactionTimer::start(EVENT_APPEND_OPERATION);
        let acquire_started = Instant::now();
        let acquired = self.pool().acquire().await;
        record_postgres_pool_acquire_duration(
            acquire_started.elapsed(),
            EVENT_APPEND_OPERATION,
            if acquired.is_ok() { "ok" } else { "error" },
        );
        let mut conn = acquired.map_err(storage_error)?;
        let written = self
            .append_on(
                &mut conn,
                persistence_id,
                expected_sequence,
                events,
                rows,
                AppendScope::Alone,
                &mut timer,
            )
            .await?;
        timer.set_outcome("ok");
        Ok(written)
    }

    /// Run the append statement for one record on `conn`: on its own it is
    /// one transaction; inside a batch's transaction it is one of its
    /// statements, and the batch commits them together.
    pub(crate) async fn append_on(
        &self,
        conn: &mut sqlx::PgConnection,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
        scope: AppendScope,
        timer: &mut PostgresTransactionTimer,
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let params = AppendParams::new(
            [tenant, entity_type, entity_id],
            expected_sequence,
            events,
            rows,
        )?;
        let written = match self
            .run_append(conn, &params, expected_sequence, entity_type, timer)
            .await?
        {
            Some(written) => written,
            // Raced: run once more on a snapshot taken after this connection
            // holds the record's catalog row lock, so no writer can change the
            // row between that snapshot and the statement's own lock, and the
            // second run cannot race. In a batch, the first run's `locked`
            // took that lock and the batch's transaction still holds it. On
            // its own, the first run committed and let it go, so the second
            // takes it first, in a transaction of its own.
            None => {
                let raced_again = || {
                    PersistenceError::Storage(format!(
                        "the {entity_type} record's catalog row changed while this append \
                         held its lock"
                    ))
                };
                match scope {
                    AppendScope::InBatch => self
                        .run_append(conn, &params, expected_sequence, entity_type, timer)
                        .await?
                        .ok_or_else(raced_again)?,
                    AppendScope::Alone => {
                        retry_seam::before_retry(self.schema.prefix()).await;
                        let mut tx = sqlx::Connection::begin(&mut *conn)
                            .await
                            .map_err(storage_error)?;
                        if retry_seam::takes_retry_lock(self.schema.prefix()) {
                            let _held: Option<i32> = crate::dbm::postgres_query_scalar!(
                                &self.schema,
                                "SELECT 1 FROM {schema}entity_catalog \
                                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                                 FOR UPDATE",
                            )
                            .bind(tenant)
                            .bind(entity_type)
                            .bind(entity_id)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(storage_error)?;
                        }
                        // A second race returns before the commit: the
                        // transaction rolls back with nothing written.
                        let written = self
                            .run_append(&mut tx, &params, expected_sequence, entity_type, timer)
                            .await?
                            .ok_or_else(raced_again)?;
                        tx.commit().await.map_err(storage_error)?;
                        written
                    }
                }
            }
        };
        if params.projection.op == Some("upsert") {
            record_postgres_projection_index_fields(
                EVENT_APPEND_OPERATION,
                entity_type,
                params.projection.indexed_fields,
                params.projection.skipped_fields,
            );
        }
        debug_assert!(written >= expected_sequence);
        debug_assert_eq!(written, expected_sequence + events.len() as u64);
        Ok(written)
    }

    /// Run the append statement once on `conn`. None when it raced another
    /// writer of the record's rows and wrote nothing.
    async fn run_append(
        &self,
        conn: &mut sqlx::PgConnection,
        params: &AppendParams<'_>,
        expected_sequence: u64,
        entity_type: &str,
        timer: &mut PostgresTransactionTimer,
    ) -> Result<Option<u64>, PersistenceError> {
        retry_seam::ran(self.schema.prefix());
        let answer = params
            .bind(crate::dbm::postgres_query_as!(
                &self.schema,
                APPEND_COMMIT_SQL
            ))
            .fetch_one(&mut *conn)
            .await;
        match answer {
            Ok(answer) => settle(answer, expected_sequence, entity_type, timer),
            Err(error) => {
                let error = append_error(error, expected_sequence, entity_type);
                if matches!(error, PersistenceError::ConcurrencyViolation { .. }) {
                    timer.set_outcome("concurrency_violation");
                }
                Err(error)
            }
        }
    }
}

/// Hooks into a raced append's retry, for tests: they count the statement's
/// runs and can force the second run to race. Outside tests they do nothing.
#[cfg(not(test))]
mod retry_seam {
    pub(super) fn ran(_schema: &str) {}
    pub(super) async fn before_retry(_schema: &str) {}
    pub(super) fn takes_retry_lock(_schema: &str) -> bool {
        true
    }
}

/// Hooks into a raced append's retry, keyed by the store's schema so tests
/// running at once do not share them. A schema with no seam installed runs as
/// production does.
#[cfg(test)]
pub(crate) mod retry_seam {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    /// What one test asks of the appends in its schema.
    #[derive(Default)]
    pub(crate) struct Seam {
        /// How many times the append statement ran.
        pub(crate) runs: usize,
        /// Leave out the retry's lock statement, so its run can race again.
        pub(crate) skip_retry_lock: bool,
        /// Told when a retry starts; the retry waits for the answer.
        pub(crate) before_retry: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    }

    static SEAMS: Mutex<BTreeMap<String, Seam>> = Mutex::new(BTreeMap::new());

    pub(crate) fn install(schema: &str, seam: Seam) {
        SEAMS.lock().unwrap().insert(schema.to_string(), seam);
    }

    pub(crate) fn runs(schema: &str) -> usize {
        SEAMS
            .lock()
            .unwrap()
            .get(schema)
            .map_or(0, |seam| seam.runs)
    }

    pub(super) fn ran(schema: &str) {
        if let Some(seam) = SEAMS.lock().unwrap().get_mut(schema) {
            seam.runs += 1;
        }
    }

    pub(super) async fn before_retry(schema: &str) {
        let pair = SEAMS
            .lock()
            .unwrap()
            .get_mut(schema)
            .and_then(|seam| seam.before_retry.take());
        if let Some((started, go)) = pair {
            let _ = started.send(());
            let _ = go.await;
        }
    }

    pub(super) fn takes_retry_lock(schema: &str) -> bool {
        SEAMS
            .lock()
            .unwrap()
            .get(schema)
            .is_none_or(|seam| !seam.skip_retry_lock)
    }
}

/// Whether the append statement commits on its own or as one statement of a
/// batch's transaction, which decides how a raced run takes its lock again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendScope {
    /// The statement is its own transaction.
    Alone,
    /// The statement runs inside a batch's transaction.
    InBatch,
}

impl AppendParams<'_> {
    /// Bind every parameter to `query`, in the statement's order.
    fn bind(
        &self,
        query: crate::dbm::PostgresQueryAs<AppendAnswer>,
    ) -> crate::dbm::PostgresQueryAs<AppendAnswer> {
        let [tenant, entity_type, entity_id] = self.record;
        let projection = &self.projection;
        let timer = &self.timer;
        query
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(stored_sequence(self.expected_sequence))
            .bind(&self.event_types)
            .bind(&self.payloads)
            .bind(&self.metadata)
            .bind(&self.key_names)
            .bind(&self.key_hashes)
            .bind(self.reconcile_vectors)
            .bind(&self.vector_decls)
            .bind(&self.vector_models)
            .bind(&self.vectors)
            .bind(&self.arm_ids)
            .bind(&self.arm_actions)
            .bind(&self.arm_due_at)
            .bind(&self.arm_armed_by)
            .bind(self.fired)
            .bind(projection.op)
            .bind(projection.status)
            .bind(projection.fields.unwrap_or(&serde_json::Value::Null))
            .bind(&projection.stored_state)
            .bind(projection.hash.as_str())
            .bind(&projection.index_names)
            .bind(&projection.index_values)
            .bind(projection.wrapped)
            .bind(self.tombstone)
            .bind(timer.op)
            .bind(timer.state)
            .bind(timer.arm_id)
            .bind(timer.sequence_nr)
            .bind(timer.started_at)
            .bind(timer.params)
            .bind(timer.armed_by)
    }
}

/// The sequence the statement wrote at, None when it raced another writer
/// of the record's rows and wrote nothing, or why it refused.
fn settle(
    (ok, head, written, clash_key, clash_holder, raced): AppendAnswer,
    expected_sequence: u64,
    entity_type: &str,
    timer: &mut PostgresTransactionTimer,
) -> Result<Option<u64>, PersistenceError> {
    let head = u64::try_from(head).unwrap_or_default();
    if head != expected_sequence {
        debug_assert!(!ok, "a stale expected sequence never writes");
        timer.set_outcome("concurrency_violation");
        return Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: head,
        });
    }
    if let (Some(key_name), Some(holder)) = (clash_key, clash_holder) {
        debug_assert!(!ok, "a key another record holds is never claimed");
        return Err(PersistenceError::Storage(format!(
            "duplicate declared key '{key_name}' for {entity_type}: held by {holder}"
        )));
    }
    if raced {
        debug_assert!(!ok, "a raced append writes nothing");
        return Ok(None);
    }
    assert!(ok, "an append whose checks held writes its rows");
    let written = written.ok_or_else(|| {
        PersistenceError::Storage("append wrote no sequence although its checks held".into())
    })?;
    Ok(Some(u64::try_from(written).unwrap_or_default()))
}

/// What a failed statement means. Only a unique violation has a meaning of its
/// own: on the journal, another writer appended first; on the key index,
/// another record claimed the key since the statement's snapshot. The table
/// names it, not the message text.
fn append_error(error: sqlx::Error, expected_sequence: u64, entity_type: &str) -> PersistenceError {
    let unique_table = error
        .as_database_error()
        .filter(|db| db.code().as_deref() == Some(UNIQUE_VIOLATION))
        .and_then(|db| db.table().map(str::to_string));
    match unique_table.as_deref() {
        Some("events") => PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: expected_sequence + 1,
        },
        Some("entity_key_index") => PersistenceError::Storage(format!(
            "duplicate declared key for {entity_type}: claimed by another record ({error})"
        )),
        _ => storage_error(error),
    }
}

/// A row of another record would be written under this one.
fn check_same_record(record: [&str; 3], row: [&str; 3]) -> Result<(), PersistenceError> {
    if record == row {
        return Ok(());
    }
    Err(PersistenceError::Storage(format!(
        "an append to {} carries a row of {}",
        record.join(":"),
        row.join(":")
    )))
}

/// Each declared key names one row: one name twice would change it twice.
fn check_distinct_key_names(keys: &[EntityKeyRow]) -> Result<(), PersistenceError> {
    let mut names: Vec<&str> = keys.iter().map(|key| key.key_name.as_str()).collect();
    names.sort_unstable();
    match names.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(PersistenceError::Storage(format!(
            "an append declares key '{}' twice",
            pair[0]
        ))),
        None => Ok(()),
    }
}

/// Sequences are stored as BIGINT; one past its range is past every row.
fn stored_sequence(sequence_nr: u64) -> i64 {
    i64::try_from(sequence_nr).unwrap_or(i64::MAX)
}
