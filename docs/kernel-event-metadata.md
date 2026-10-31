# Kernel event metadata

Every entity row exposes four read-only kernel properties, alongside `entity_id`,
`status`, and `sequence_nr`:

| Property | Value |
| --- | --- |
| `first_event_at` | The first committed event's UTC timestamp, with fractional seconds; set once. |
| `last_event_at` | The newest committed event's UTC timestamp (RFC 3339). |
| `last_event_via` | `request`, `timer`, or `reaction`. |
| `last_event_by` | The authenticated subject for a request; null for timers and reactions. |

A state timer and a scheduled action are `timer`. A spec trigger, reaction, or
integration callback is `reaction`, including a reaction to a timer. The identity
used to authorize such work is separate from the subject that made a request.
The kernel never attributes a timer or a reaction to that acting identity.

Creation and field updates are events too. `first_event_at` stays fixed across
requests, timers, reactions, field updates and deletion. The three `last_event_*`
properties follow each committed event. Refused actions and idempotent retries
do not change these properties. Time comes from the event, through the kernel's
simulation clock; applications do not need a clock or bookkeeping parameters.

The properties are top-level row values on OData reads, support `$select`,
`$filter`, and `$orderby`, and are retained independently of the bounded event
history in snapshots and query projections. For example:

```
GET /tdata/Jobs?$orderby=first_event_at asc
GET /tdata/Jobs?$orderby=last_event_at desc
GET /tdata/Jobs?$filter=last_event_via eq 'timer'
```

These names are reserved: a spec cannot declare them as state or action
parameters. Client-supplied values are removed by the existing kernel field
sanitizer. Application fields such as `moved_at`, `moved_via`, and `moved_by` remain
separate and can be migrated by applications at their own pace.

Observe history returns each event's `via` and `by` beside its `timestamp`.
Older events lack provenance: their `via` and `by` are null rather than guessed
from action names or parameters. Replaying them recovers their timestamp. A
snapshot written by an older kernel without latest-event metadata cannot recover
that provenance before its snapshot boundary; the `last_event_*` properties stay
null until a later event is applied or the whole journal is replayed.

Creation time has a separate recovery rule: keep a saved snapshot or catalog
value; otherwise read exactly journal sequence 1, even for an older snapshot. Never use the earliest retained event
when a prefix or segment has been compacted. If sequence 1 is gone and no saved
creation time exists, `first_event_at` is null, including after subsequent moves.

## One-time catalog back-fill

`entity_catalog.first_event_at` sits beside `updated_at`. Both stores copy the
creation time from kernel state on writes and preserve an existing non-null
value. Full catalog reads overlay the column on the JSON state, including rows
whose older stored state predates the property. Snapshots and cached idempotent
replies carry it with the other event metadata.

At startup, Turso databases (including routed tenant stores) and PostgreSQL serve
while a background task walks the catalog in key order, 128 records per
transaction. Each record probes sequence 1 using the existing
`(tenant, entity_type, entity_id, sequence_nr)` journal index. The task writes only
the new column, leaves `updated_at` alone, yields between batches, and saves its
key and counts atomically in `first_event_backfill`. A restart resumes the cursor;
a completed database does not scan again. Missing first events are counted as
null and marked visited too. Errors are logged and leave the cursor resumable.

Completion logs `first_event_at backfill complete` with `elapsed_ms`, `records`,
and `null_records`. The elapsed time is for this process's run; counts include
any batches completed before a restart. The singleton progress row can also be
read directly:

```sql
SELECT records, null_records, complete FROM first_event_backfill WHERE id = 1;
```

## Release verification with infra

After the binary release, identify the biggest staging Temper by catalog size.
Record its deployed commit, record count, completion log (`elapsed_ms` and
`null_records`), and whether this was uninterrupted or resumed. Confirm that
serving began before the migration completed. These measurements must come
from staging, not a local fixture or an estimate.

Install a disposable `kgsmoke` app through staging's normal app-install route.
Create records at distinct times, move the oldest, and read them over OData with
`$select=entity_id,first_event_at,last_event_at,last_event_via,last_event_by` and
`$orderby=first_event_at asc`. Check that the creation values are fractional UTC
timestamps and remain unchanged, and that the order is still creation order.
Delete the test app through the normal app-delete route afterwards. Record the
OData result and successful app deletion in the PR. Production follows on the
next binary roll (or infra's held roll if timing permits); site surfaces and
`read_app` mappings, then control bookkeeping removal, are separate changes.
