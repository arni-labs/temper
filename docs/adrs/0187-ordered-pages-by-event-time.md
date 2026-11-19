# ADR-0187: Ordered Pages by Event Time

- Status: Proposed
- Date: 2026-11-19
- Deciders: Temper core maintainers
- Related:
  - ADR-0135: Authorized Query Plane Pages
  - ADR-0143: Bounded Source Cursor Coverage
  - ADR-0154: OData Read Surface Truthfulness
  - `docs/kernel-event-metadata.md` (the event-time properties)
  - `crates/temper-runtime/src/persistence/catalog_page.rs`
  - `crates/temper-server/src/odata/query_plane_read/` (`pagination.rs`, `scan/`)
  - `crates/temper-store-turso/src/store/query_page.rs`, `src/schema/event_time_order.rs`
  - `crates/temper-store-postgres/src/query_page.rs`, `migrations/0029_event_time_order_indexes.sql`

## Context

A list of records ordered by when they last moved (`$orderby=last_event_at desc`)
is the read every activity view and agent inbox makes. On a type with more
records than the scan budget (10 × `max_entities`, so 10,000 at the default
limits) it failed four ways:

1. `$orderby` on `first_event_at` or `last_event_at` was never native. The read
   materialized every candidate to sort in memory and answered 413 past the
   budget.
2. A `$skiptoken` continuation was only folded into `$filter`. SQL could not
   seek to it, so each page rescanned from the start and a walk answered 413
   after about 10,010 rows, even for an order SQL could serve (a status filter).
3. Native pages that did run used `OFFSET`, which reads and drops every earlier
   row.
4. `$count=true` counted by materializing candidates and answered 413 above
   10,000 matching rows.

Measured on a 1,300-record type with `TEMPER_ODATA_MAX_ENTITIES=20` (budget
210) at `8c92153e1`: both event-time orders, a status-filtered event-time walk,
every `$count`, and an event-time bound answered 413, and a plain
`status eq 'Open'` walk answered 413 on page 42 after 205 rows.

A fifth report, that `$top` equal to `max_entities` is refused with 413, was not
reproduced: at default limits and at `max_entities` 20, `$top=max_entities` read
as an operator returned a full page with a continuation for every query shape
tried. Every 413 site needs more than 10 × `max_entities` candidates, and the
pagination layer already gives the budget one row of headroom for its probe row.
The production report (Story Islands members, 14 November) was likely a
per-row Cedar read on a type above the budget, which is case 1 or 4 above.

## Decision

### Sub-Decision 1: One fixed-width key per event time

The kernel writes event times as RFC 3339 UTC text with 0 to 9 fraction digits
(`…:05Z`, `…:05.1Z`, `…:05.123456789Z`). Text order is not instant order across
widths (`05.1Z` sorts after `05.123Z` but `05Z` sorts after both). Each store
orders by a 29-character key, `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`, built in SQL as

```
COALESCE(substr(T,1,19) || '.' || substr(rtrim(substr(T,21),'Z') || '000000000', 1, 9),
         '9999-99-99T99:99:99.999999999')
```

where `T` is `json_extract(state,'$.last_event_at')` (Turso),
`state ->> 'last_event_at'` (Postgres, under `COLLATE "C"`), or the
`first_event_at` column, which both stores already fill from state on upsert and
overlay on catalog reads. Rust computes the same key in
`temper_runtime::persistence::event_time_key`, used for cursors, bounds and the
in-memory sort, so all three agree.

A record without a time gets the sentinel `MISSING_EVENT_TIME_KEY`, which sorts
after every time. This matches the in-memory order (absent last ascending,
first descending).

**Why a sentinel, not NULL**: turso_core 0.7.2 will not use an index for an
`ORDER BY` with `NULLS FIRST`/`NULLS LAST`, and SQLite's default NULL placement is
the opposite of the in-memory order for one of the two directions. A sentinel
gives one order in every direction with no NULL clause, so one expression serves
both the index and the query.

### Sub-Decision 2: Four expression indexes per store

Each store has `idx_entity_catalog_{last,first}_event_{asc,desc}` on
`(tenant, entity_type, key [DESC], entity_id, status)`. Turso creates them at
boot with `CREATE INDEX IF NOT EXISTS` (a failure is logged and the store still
serves, falling back to the in-memory read); Postgres creates them in migration
`0029`.

**Why four**: a page orders by the key in the caller's direction and always by
`entity_id` ascending to break ties. A mixed-direction order cannot be read
backwards from one index, so each direction gets its own. `status` comes last so
a status filter is checked from the index entry without reading the row.

The query's `ORDER BY` repeats the index expression text exactly; both
planners match an expression index only on identical text.

### Sub-Decision 3: The planner treats event-time order as native

`native_order_property_is_lossless` now accepts `first_event_at` and
`last_event_at`: the SQL order equals the in-memory order. A read whose every
order clause is a catalog key (`entity_id`, `status`, or an event time; see
`CatalogOrderKey`) can be served by the store's page.

`should_try_native_page_first` also holds for an unfiltered read and for a
counted read, not only for one with a filter SQL can narrow.

### Sub-Decision 4: The continuation seeks in SQL

The token format is unchanged and the keyset is still folded into `$filter`,
so the in-memory re-check still honors it. When every order clause is a catalog
key, `pagination.rs` also builds a typed `SqlKeyset { filter, after }`, the
caller's unfolded filter and the boundary row's key values. The store turns
`after` into

```
k <= ?v AND (k < ?v OR (k = ?v AND entity_id > ?id))      -- descending
```

with `LIMIT` and no `OFFSET`. The leading bound (`k <= ?v`) is what lets
turso_core seek; it also needs `INDEXED BY` on the event-time index to pick it
over the status index. A page therefore reads about `$top` index entries
however deep the walk is.

Orders on application fields keep the folded keyset alone: their SQL order is
not guaranteed to equal the in-memory one, so the store cannot be trusted to
resume them.

### Sub-Decision 5: Event-time range filters push down

`filter_sql::split_event_time_bounds` lifts top-level `AND` conjuncts that
compare an event time with `eq`, `gt`, `ge`, `lt` or `le` against a literal into
`EventTimeBound`s on the key, after normalizing the literal's UTC offset. A
reversed comparison is flipped. `gt` and `ge` also exclude the sentinel. The rest
of the filter is translated as before.

### Sub-Decision 6: The native page first, and a count is one SQL count

When the read's filter is exactly what SQL evaluates and the policy is
`AllowAll`, the store's page is the answer (`scan/uniform.rs`): its rows, and
for `$count=true` one `COUNT(*)` with the same predicate. A count-only read
(`$top=0`) runs only the count. An event-time page counts in a separate
statement rather than a window `COUNT(*) OVER ()`, which would read every match.
If the count is smaller than the rows the page returned, the page is not used
and the read falls back.

Under per-row Cedar policies the native page still supplies candidates in SQL
order, resuming from the cursor; the per-row loop advances its own offset
relative to that cursor within one request. Its count is still the bounded
authorized count, so a per-row `$count` above the budget still answers 413.

### Sub-Decision 7: The simulation store keeps the in-memory path

The sim store has no catalog index. Its reads keep the in-memory sort, which now
uses `event_time_key`, so a deterministic simulation sees the same order as SQL.

### Sub-Decision 8: The Turso plan check follows a named index

The debug-only `plan_check` refused a page that searched an index by fewer
`= ?` columns than another index offered. An `INDEXED BY` page deliberately
reads in its index's order and stops at `LIMIT`; a status index with more
equality columns would sort every match. The check now holds a statement that
names its index to that index, and an index's column list stops at its first
expression column (no `= ?` can supply an expression).

The check also showed what the new indexes cost elsewhere: with them, turso_core
0.7.2 planned the catalog row delete (`DELETE FROM entity_catalog WHERE tenant =
?1 AND entity_type = ?2 AND entity_id = ?3`) as a search of an event-time index
by tenant and type, reading every row of the type to delete one. That statement
now names the primary-key index, as the store's other keyed catalog writes
already do. The other catalog statements were compared on the same database
before and after: none searches by fewer columns than before.

## Consequences

### Positive
- Event-time-ordered lists, with or without an `entity_id` or `status` filter,
  page to their last record at about `$top` rows per page, on Turso and
  Postgres.
- Every continuation whose order is catalog keys resumes in SQL.
- `$count` on an `AllowAll` read is one SQL count at any size.
- An event-time range filter narrows in SQL.
- Measured on 11,000 records at default limits: `$count` in 0.24 s, and full
  walks of 11,000, 11,000 and 7,333 rows matched the in-memory order.

### Negative
- Four more indexes per store, each computing `json_extract` (Turso) or `->>`
  (Postgres) on every catalog write: write amplification on the hottest table.
- The sentinel is visible only inside SQL; a reader of the SQL has to know it
  stands for "no time".
- A filter `ne` on status is still not pushed down (it is not lossless against
  NULL), so `status ne 'X'` on a large type still answers 413.

### Risks
- **Index build at boot.** Turso builds the indexes during store schema setup
  on first start of the new binary, reading every catalog row once; the store
  serves after it finishes. Postgres `CREATE INDEX` without `CONCURRENTLY`
  (sqlx runs each migration in a transaction) blocks writes to
  `entity_catalog` while it builds.
- **Malformed state.** A row whose `state` is not valid JSON would make
  `json_extract` fail in Turso, in the index build and on write. The kernel
  writes `state` as serialized JSON, so no such row is expected.
- **Collation.** The Postgres page compares the key under `COLLATE "C"`;
  `entity_id` keeps the column's collation, as the existing pages already do.
- **Rare status.** A page filtered to a rare status reads index entries in time
  order until it finds `$top` matches, which can be many entries.
- **Non-UTC text.** The key assumes stored times end in `Z`, which the kernel
  guarantees; a stored offset other than `Z` would key wrongly.
- **Count consistency.** The separate count is not in the page's snapshot; a
  write between them can shift it. The count-below-rows check falls back rather
  than returning an impossible answer.

### DST Compliance
- No new clock or randomness. `event_time_key` is a pure function of the stored
  text; the sim store's in-memory sort uses it.
- No `HashMap` or `SystemTime` added in temper-server.

## Non-Goals
- Pushing down `ne`, `or` across different properties, or functions on event
  times.
- Native ordering by application fields, or seeking on their keyset.
- Unbounded per-row-authorized counts.
- Changing the continuation token format.

## Alternatives Considered

1. **NULL for a missing time with `NULLS LAST`.** The design brief's choice.
   turso_core 0.7.2 then sorts every match instead of reading the index, so
   the deep pages stay slow. Rejected for the sentinel.
2. **One index per column, read in either direction.** The tiebreak is always
   `entity_id` ascending, so a descending key gives a mixed-direction order that
   one index cannot serve. Rejected.
3. **A stored, normalized column per event time.** It would avoid
   `json_extract` on write and the expression match, but it adds a schema
   migration and a back-fill for `last_event_at` on every catalog. The
   expression index gives the same plan with no new column. Rejected for now.
4. **Replacing the folded keyset with the SQL keyset.** The in-memory re-check
   (per-row policy, lossy filters) would then lose the boundary. Kept both.

## Rollback Policy

Reverting the code is enough: the old planner never orders by the key, so the
indexes go unused, and an older release runs on a database that has them
(migration `0029` is additive; an older Turso release ignores indexes it does
not know). To recover the write cost later, a new Postgres migration and the
Turso schema can `DROP INDEX IF EXISTS
idx_entity_catalog_{last,first}_event_{asc,desc}`, marked `additive-ok` (no
release relies on the indexes for correctness). `0029` itself is never edited
once released.
