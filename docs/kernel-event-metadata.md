# Kernel event metadata

Every entity row exposes three read-only kernel properties, alongside `entity_id`,
`status`, and `sequence_nr`:

| Property | Value |
| --- | --- |
| `last_event_at` | The newest committed event's UTC timestamp (RFC 3339). |
| `last_event_via` | `request`, `timer`, or `reaction`. |
| `last_event_by` | The authenticated subject for a request; null for timers and reactions. |

A state timer and a scheduled action are `timer`. A spec trigger, reaction, or
integration callback is `reaction`, including a reaction to a timer. The identity
used to authorize such work is separate from the subject that made a request.
The kernel never attributes a timer or a reaction to that acting identity.

Creation and field updates are events too. Refused actions and idempotent retries
do not change these properties. Time comes from the event, through the kernel's
simulation clock; applications do not need a clock or bookkeeping parameters.

The properties are top-level row values on OData reads, support `$select`,
`$filter`, and `$orderby`, and are retained independently of the bounded event
history in snapshots and query projections. For example:

```
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
snapshot written by an older kernel without this metadata cannot recover facts
from the journal before its snapshot boundary; its properties stay null until a
later event is applied or the whole journal is replayed.
