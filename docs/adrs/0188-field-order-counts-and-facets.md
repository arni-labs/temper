# ADR-0188: Field Orders, Exact Comparisons, Unbounded Counts and Facets

- Status: Proposed
- Date: 2026-11-19
- Deciders: Temper core maintainers
- Supersedes: ADR-0187's "still answers 413" statements (Sub-Decision 6 and
  Negative), and the 413 contract of ADR-0135 and ADR-0154 for reads under
  per-row authorization
- Related:
  - ADR-0135: Authorized Query Plane Pages
  - ADR-0154: OData Read Surface Truthfulness
  - ADR-0187: Ordered Pages by Event Time
  - `docs/api/read-aggregates.md` (the facet read)
  - `crates/temper-runtime/src/persistence/catalog_page.rs`, `catalog_field.rs`, `catalog_facet.rs`
  - `crates/temper-server/src/odata/filter_sql/field_compare.rs`
  - `crates/temper-server/src/odata/query_plane_read/` (`scan/`, `pagination/`, `facets.rs`)
  - `crates/temper-store-turso/src/store/` (`catalog_sql.rs`, `query_page.rs`, `query_facets.rs`)
  - `crates/temper-store-postgres/src/` (`catalog_sql.rs`, `query_page.rs`, `query_facets.rs`)

## Context

After ADR-0187, a list or count of a large entity set still failed in four
ways that any app with a few thousand records meets:

1. `$filter=Status ne 'X'` and `$filter=field ne 'v'` were not pushed down, so
   past the scan budget (10 × `max_entities`) the read answered 413.
2. Under a policy that judges each row (`ListRead::PerRow`), a `$count`, and a
   list page whose matches were sparse, answered 413 once the read had judged
   the budget's worth of candidates.
3. `$orderby` on an app field sorted every candidate in memory and resumed by
   re-reading from the start, so deep pages answered 413.
4. A range (`rank gt 5`, `said_at ge '2026-11-01'`) on an app field was never
   pushed down. A string equality was read off the field index's text, which
   also holds a number or boolean with the same text, so `kind eq '5'` also
   matched a record whose `kind` is the number 5.

And there was no way to ask how many records hold each value of a field
without listing them all.

The kernel knows nothing of who owns an app or what is shared; these are
generic OData reads. The owner's rule on limits applies: a limit needs a reason
that holds outside the code. The scan budget had none for a read that answers
correctly if it keeps going.

## Decision

### Sub-Decision 1: `ne` is exact in SQL

`Status ne 'X'` becomes `status != ?`: the catalog's status is never NULL and
every record reads it, so the rows SQL keeps are exactly the records the
evaluator keeps. `field ne <literal>` on a declared, non-nullable scalar of the
literal's type becomes "holds a value, and that value is not the literal of
that type" on the row's typed JSON (`field_comparison_sql`): a value of another
type is unequal, a JSON null or no value meets no comparison, as in
`query_eval`. A declared default is honoured: the comparison also lets through
rows without the key (`or_missing`), and the read rechecks those in memory
unless no candidate lacks the key.

Stays in memory, because SQL and memory can disagree there:
- `ne null`, and `ne` with a literal of another type than the field's;
- a field that is undeclared, nullable, kept out of the projection
  (`query_indexed = false`), or named like a property the record holds at its
  top (`RECORD_PROPERTIES`), which the evaluator reads first;
- `ne` inside an `or`, or under `not`.

### Sub-Decision 2: No scan budget; the limit is time

A count or list under per-row authorization walks the store's rows a page at a
time, each page starting after the last row of the one before (a keyset the
store seeks to), keeping only the requested page and a running count. A list
page keeps walking until it is full. The read-source walk (no native page) does
the same in batches. `QueryTooLarge` and its 413 are gone; nothing replaces
them with another cap.

Measured on 2026-11-19 on an Apple M3 Max (16 cores, 128 GB, macOS 26.5; the
machine was not idle), release `temper serve --storage turso` on a file store,
12,000 Card records, a policy that permits `list` and `read` and forbids `read`
when the status is Done or Held (6,000 hidden), read over HTTP by a bearer
`AgentCredential` that is not an admin:

| Read | Median | Runs |
| --- | --- | --- |
| `$count=true&$top=0`, the reader (judges 12,000 rows) | 2.76 s | 7 |
| same, an admin (one SQL count, `AllowAll`) | 0.010 s | 3 |
| `$filter=kind eq 'steer'&$count=true&$top=0`, the reader | 1.35 s | 5 |

About 4,350 rows judged per second (12,000 / 2.76 s), so a count over a
million such rows takes about four minutes. The number is in a comment next to
the loop (`scan/native.rs`).

A count never shows a row the reader may not read (each row is judged before
it is counted), and a continuation token is built from the last row the reader
was shown, so it names no hidden row (`tests/per_row_walk.rs` decodes every
token and checks its id is a visible one).

### Sub-Decision 3: An order by a declared field resumes in SQL

`$orderby` on a declared, non-nullable number or string the projection holds
(and the record does not also hold at its top) orders and resumes in SQL, with
a keyset clause `KeysetValue::Field` beside the catalog keys. SQL's order is
`sort_entities`' order:

- numbers compare as doubles. serde_json's `float_roundtrip` is enabled for the
  workspace so the evaluator parses a number to the nearest double, as Turso
  (Rust's `parse::<f64>`, in `turso_core` 0.7.2 `json/mod.rs`) and Postgres
  (`CAST AS double precision`, strtod) do;
- strings compare by bytes: Turso's default `BINARY`, Postgres `COLLATE "C"`;
- a record without a value sorts last ascending and first descending.

`sort_entities` finds a value of another JSON type (or a JSON null) equal to
everything, which is not a total order and cannot be matched in SQL. So before
it trusts SQL's order the read asks the store for one row of the filter holding
a value of another type (`mistyped`, `LIMIT 1`); if there is one, the read
walks in memory on the folded keyset, as before.

The keyset is written flat: one term for each key the row can be past the
cursor on, holding the cursor's value on every key before it. The nested form
(`a > x OR (a = x AND (…))`) is the same condition, but `turso_core` plans each
level deeper on the stack: on 2026-11-19 a debug build needed 2,048 KiB of
thread stack for a two-field page nested and 1,280 KiB flat (a test thread has
2 MiB; the server's threads have 16 MiB).

Kept on the folded keyset: booleans (the in-memory sort does not compare them),
nullable, undeclared or unprojected fields, record properties, and any order
whose rows hold a mixed type at read time.

**Cost.** No index serves a JSON field's order, so each page reads every row
the filter selects and keeps the top `$top` (Postgres top-N heapsort, Turso a
sorter); the type check is one more pass over the same rows that stops at the
first hit. On the measurement database, a page of `Status eq 'Open'` ordered by
`rank desc` (3,000 candidate rows) took 132 ms on Turso; Postgres reads them by
`idx_entity_catalog_status`. A cheap general index would be a typed,
order-preserving sort key column on `entity_field_index` (numbers as a
sign-adjusted fixed-width encoding of the double, strings as themselves) with
one index on `(tenant, entity_type, field_name, sort_key, entity_id)`: one
index for every field of every type. It is proposed, not built: it changes the
projection's write and needs a back-fill.

### Sub-Decision 4: Comparisons on declared scalars are exact

`eq`, `ne`, `gt`, `ge`, `lt` and `le` of a declared non-nullable number or
string with a literal of its own type, and `eq`/`ne` of a boolean, are made on
the row's typed JSON value, minding declared defaults as in Sub-Decision 1. An
equality of a string or boolean also stays in the translated filter so the
field index narrows the rows by a lookup; a number's does not, because its
index text (`3.0`) need not be the literal's (`3`). A string equality now
also requires a string, which fixes `kind eq '5'` matching the number 5.

### Sub-Decision 5: Facets

`GET /tdata/<Set>?$apply=groupby((<field>),aggregate($count as count))`, with
optional `$filter`, `$top`, `$skiptoken`, and `$orderby` of `<field> asc` (the
default) or `count desc` then `<field> asc`. The field is a declared scalar
property or `Status`. The answer is `{"value":[{"<field>":v,"count":n}],
"@odata.nextLink":…}`; the cursor is the last group's count and value, and a
page resumes strictly after it in the requested order. The parser is
`temper_odata::query::parse_apply`. Anything else under `$apply`, and `$apply`
with `$expand`, `$select`, `$count`, `$skip`, `$search` or `actions=true`,
answers 400.

One SQL `GROUP BY` (`CatalogFacetQuery::statement`) answers when every row is
allowed, the filter is exact in SQL and not an equality conjunction (which has
its own reconciled read, ADR-0153), and the key is `Status` or a field SQL
orders exactly (Sub-Decision 3) with no declared default and no value of
another type among the filter's rows. Otherwise the records are walked as a
list read walks them, each row authorized, and counted in memory: the walk
keeps one count per distinct value and one page of records (`BTreeMap`), and
every page of groups walks the records again. A hidden row is never counted.

## Rollout Plan

1. **Phase 0** — this change. No migration, no new index, no setting.
2. **Phase 1** — the typed sort-key index of Sub-Decision 3, if field-ordered
   pages on large types prove slow in use.

## Consequences

### Positive
- `ne`, ranges and orders on declared fields answer exactly at any size.
- A per-row count or sparse page always answers, with the right number.
- Facets count by value without listing records.

### Negative
- A read that used to stop at 413 now runs until it is done; under per-row
  policies that is about four minutes per million rows judged on the measured
  machine, held by one request.
- A field-ordered page reads every row of its filter.
- A walked facet re-walks every matching record on each page of groups.

### Risks
- **Long requests.** Proxies and clients with request timeouts may cut a long
  per-row count off; the server keeps working until the walk ends or the
  connection drops.
- **String equality is stricter.** `field eq '5'` no longer returns records
  whose value is the number 5. That was a wrong answer, but a client may have
  leaned on it.
- **`$apply` is read.** It was ignored and the read listed records; now a
  facet answers, and an unsupported `$apply` answers 400 on every path, since
  query options are parsed before the path is dispatched.
- **`float_roundtrip`.** Every serde_json number parse in the workspace now
  rounds to the nearest double; a value that parsed one ULP off before now
  parses exactly. Stored text is unchanged.
- **Concurrent writes.** A walk is not a snapshot: a record that moves during
  a long walk can be counted on the wrong side of a filter or a group.
- **Postgres collation.** Field orders and the status facet compare under
  `COLLATE "C"`; status and `entity_id` orders keep the database collation, as
  before.

### DST Compliance
- No clock, randomness or `HashMap` added in temper-server; group counts are a
  `BTreeMap`. The sim store has no query plane, so its reads and facets take
  the in-memory walk; the tests check it gives the same answers as Turso and
  Postgres.

## Non-Goals
- Aggregates other than a count, several group properties, nested `$apply`.
- `$apply` on a single entity or a navigation property.
- A new index (Sub-Decision 3 proposes one).

## Alternatives Considered

1. **Raise the scan budget.** Moves the 413 without a reason that holds outside
   the code. Rejected.
2. **Answer a per-row count as an estimate.** A count that is sometimes wrong
   cannot be told from one that is right. Rejected.
3. **Order mixed-type fields in SQL by type then value.** The in-memory sort
   has no such order; the two would disagree on the page boundaries. Rejected
   for the type check.
4. **Facets by listing every record to the client.** What this replaces.

## Rollback Policy

Revert the commits. No migration was added and no stored data changed, so an
older release runs on the same database. Continuation tokens keep their format;
a facet cursor given to an older release is ignored with the `$apply` it came
with, and that release lists records. Reverting `float_roundtrip` in the
workspace `Cargo.toml` returns number parsing to serde_json's default.
