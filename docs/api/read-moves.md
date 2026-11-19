# The moves a read lists

A read of one entity tells its reader what it could do with the record now:

```json
{
  "entity_id": "dep",
  "status": "Waiting",
  "fields": {"needs": "first"},
  "@odata.actions": [],
  "@temper.refused": [
    {
      "name": "Unlock",
      "reason": "Action 'Unlock' blocked from state 'Waiting': guard 'Task[needs].status == 'Done'' failed (needs = first, Task[needs].status = Ready)",
      "refusal": {"code": "GuardFailed", "guard": "Task[needs].status == 'Done'", "conditions": []}
    }
  ]
}
```

- `@odata.actions`: the moves the reader could make now, each `{name, target, hint}`
  with its bound action path. A move is listed only when its rule starts from the
  record's status, its guard holds, and Cedar lets this reader make it.
- `@temper.refused`: the moves that start from the record's status and the reader
  could not make now, each `{name, reason, refusal}`. `refusal` is the typed
  refusal a dispatch of the move gets when the record's guard is why
  (`GuardFailed`), and null when Cedar is why; `reason` is the message either
  way.
- A move in neither list does not start from the record's status.

Both lists come from the check every move goes through
(`TransitionTable::blocked`), in a dispatch's order: Cedar on the record's
attributes as a dispatch builds them, then the state and the guard. A guard
that reads another record (`Task[needs].status == 'Done'`) has that record's
status looked up from the record's own reference, as a move looks it up. A
guard that reads the call's parameters (`worker == params.worker`) is decided
by its parts that read none: the move is listed when some call could make it.
A client does not evaluate guards or ask Cedar a second time to know what to
offer.

They describe the record as read. A move that is sent is decided again, so a
listed move can still be refused when the record or a record it reads has
changed since.

## On list reads

A list read carries no moves unless asked: every row would pay for them. The
custom query option `actions=true` asks for them:

```
GET /tdata/Tasks?actions=true&$top=50
```

Each row then carries the same two lists as a read of that entity. The option
is carried by `@odata.nextLink` to the pages after. `$select` still shapes
the rows: the moves are judged on the whole record before the selection is
applied, so a read that selects two fields is answered about all of them.

## Cost

For each record: one Cedar evaluation per move that starts from its status
(counted as `check:offer` in `temper_cedar_evaluations_total`), and one status
lookup for each related record that the guards of the moves Cedar lets through
read, from the status cache when it holds it. The record is not read again and
nothing is written. A list read asked with `actions=true` reads whole rows
even when it selects fields.

`POST /api/authorize` ([authorize.md](authorize.md)) answers the same question
for a caller that holds a record's values and no read of its own, and for a
record that is not made yet.
