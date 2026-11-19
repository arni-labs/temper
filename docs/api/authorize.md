# Authorization questions

`POST /api/authorize` asks which moves could be made, without making one. A
move is offered (`allowed: true`) only when the record's state and the move's
guard let it be made and Cedar lets the caller make it. A question
writes nothing: no PendingDecision, GovernanceDecision, denial-pattern or
trajectory row, and nothing for the suggestion engine. A real request an agent
sends and is refused (a read by id, a list it may not make, a dispatch) still
files one decision its person can approve. While that decision waits, the same
agent refused the same action on the same resource gets it back instead of a
new one.
Authentication and tenant resolution use the same credential-derived context as
before. `agent_id` must match that principal (including person principals).

For one record or a page, send a single request:

```json
{
  "agent_id": "agent-1",
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "context": {"owner": "person-1", "status": "Open", "has_spec": true},
      "actions": ["Read", "Approve"]
    }
  ]
}
```

The HTTP 200 answer preserves record order and action order, including duplicates:

```json
{
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "actions": [
        {"action": "Read", "allowed": true, "reason": null, "refusal": null},
        {"action": "Approve", "allowed": false, "reason": "Authorization denied ...", "refusal": null},
        {"action": "Reopen", "allowed": false,
         "reason": "Action 'Reopen' not valid from state 'Open'",
         "refusal": {"code": "InvalidState", "state": "Open", "from": ["Closed"]}}
      ]
    }
  ]
}
```

## State and guard

When `resource_type` has a spec and the snapshot carries its `status`, each
asked action that is a move of the spec is first judged as a dispatch of it
would be: its rule must start from that status and its guard must hold. The
kernel evaluates the guard itself, with the code every move runs, on the values
in the snapshot. A value the snapshot lacks reads as its declared initial value,
so send the row as read. A guard that reads the call's parameters
(`worker == params.worker`) is decided by its parts that read none: the move is
offered when some call could make it.

A move whose guard reads another record (`Task[needs].status == 'Done'`) is
decided on the stored record instead of the snapshot, in a dispatch's own
order: the kernel reads the record, asks Cedar on its stored attributes, and
judges the guard with the related record's status looked up from the stored
record's own reference. The snapshot's values play no part in that answer.

A move its state or guard rules out answers `allowed: false` with the message
and the typed `refusal` a dispatch of it gets (`InvalidState` or `GuardFailed`,
as in the action's own 409). A Cedar denial has a null `refusal`. Callers do
not evaluate guards themselves: a guard a caller cannot read would otherwise be
shown as possible and then refused.

An action the spec has no rule for (`Read`), a type without a spec, and a
snapshot without `status` are Cedar's alone, as before.

`reason` is the Cedar denial text or the refusal's message; an allow has a null reason. No check
answer contains `decision_id`. Empty record/action lists return empty answers.
A malformed context (anything except an object or null) rejects the entire page
with HTTP 400 before evaluation. A mismatched principal returns HTTP 403.
Mixed single/batch bodies are rejected rather than silently dropping questions.

The caller supplies each record snapshot once in `context`, with exactly the
attributes it previously supplied to each single check. Omitted/null context is
an empty object. The kernel overwrites `context.id` with `resource_id`, as before.
This endpoint does not retrieve or grant access to records; callers must read
records through the governed read surface first. Offers describe that read's
snapshot. Dispatch always reloads and authorizes the actual move independently.

For R records with M total requested moves, the cost is at most M Cedar
evaluations, R decoded context maps and **zero store writes**. A record is read
only when an asked move that starts from its status has a guard that reads
another record: one read of that record, and one status lookup per related
record, answered from the status cache when it holds it. Every other record
costs **zero store reads**.
No per-move record/context reload is performed. A denied check is visible only
as the authz wide event every Cedar check emits. Conformance verdicts retain
their existing meaning.
There is no arbitrary item cap: the existing Axum JSON request-body byte limit
(default 2 MiB on this route) bounds input. Pages over that body limit must be
split by bytes, not by an invented move count.

The published Rust and TypeScript SDKs also retain their single-pair form:
`{agent_id, action, resource_type, resource_id, context?}` returns
`{allowed, reason}` without a decision ID. Both SDKs expose a batch method.
MCP does not call this check route; its decision IDs come from refused mutations
and remain available for human resolution.
