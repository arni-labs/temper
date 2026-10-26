# Offer checks

`POST /api/authorize` asks whether the authenticated principal may perform actions.
It never creates a PendingDecision, GovernanceDecision, denial-pattern row, or
approval notification. A refused dispatch still follows the existing approval path.
The `agent_id` must match the authenticated principal; tenant and identity come
from the authenticated request, including for people.

A record or page uses the same request shape:

```json
{
  "agent_id": "agent-1",
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "context": {"status": "Draft", "Status": "Draft", "has_spec": true},
      "actions": ["Inspect", "Publish"]
    }
  ]
}
```

HTTP 200:

```json
{
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "actions": [
        {"action": "Inspect", "allowed": true, "reason": null},
        {"action": "Publish", "allowed": false, "reason": "<Cedar denial text>"}
      ]
    }
  ]
}
```

Records and actions retain input order, including duplicates and empty lists.
Every action has `allowed` and `reason` (null when allowed). No check answer
contains `decision_id`. An invalid context (anything besides an object or null)
rejects the entire request with HTTP 400 before evaluating any action. An identity
mismatch returns 403; authentication is required.

This is the existing context-based oracle: it evaluates the supplied record fields,
not an authoritative snapshot or a promise that a later dispatch will succeed.
Context defaults to empty. Its `id` is replaced by `resource_id` before evaluation.
A caller supplies a record and its context once, with all desired action names;
for a page, append records rather than repeating context for each move.
A batch costs one Cedar evaluation per requested action, zero record-store reads,
and one context preparation per record. Denied actions retain their existing
asynchronous trajectory entries with `spec_governed: false`; conformance verdicts
are unchanged. There are no decision or denial-pattern store writes. There is no
count cap; the existing Axum JSON body byte limit applies.

The single-pair form remains for the public Rust and TypeScript SDK clients:
`{"agent_id":"agent-1","action":"Publish","resource_type":"Release","resource_id":"release-1","context":{}}`.
Its answer is `{"allowed":false,"reason":"<Cedar denial text>"}` and is equally
free of approval work. Both SDK response types drop `decision_id`. `temper-mcp`
does not call this route; its decision IDs come from actual refused requests and
remain intact.

Katagami's following release should replace `authorizedRecord`'s per-move loop
with one request using `actions: record.moves.map(m => pascal(m.move))`. For pages,
`authorizedRecords` should send all records together and map answers by input
position. Preserve its existing context fields and principal selection. A failed
HTTP check still marks offers unconfirmed; it must never be retried as a dispatch.
The combined staging proof must measure one Factory-sized `read_app` with a test
agent on a test person's app: one authorize request, identical offers, no new
pending decisions, and the read below Rita's one-second bar in Datadog's
katagami-site `mcp read_app` and person-Temper authorize spans. This kernel change
alone does not establish that site latency or the completed site migration.
