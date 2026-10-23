# Authenticated action parameters

An IOA action can bind a string parameter to a value the runtime supplies:
`source = "authenticated_subject"` or `source = "dispatched_at"` (below).
These are the accepted sources; any other fails to parse.

The authenticated subject:

```toml
[automaton]
name = "Document"
states = ["Requested", "Active"]
initial = "Requested"
strict_action_params = true

[[state]]
name = "owner_id"
type = "string"
initial = ""

[[action]]
name = "Create"
from = ["Requested"]
to = "Active"
params = [{ name = "owner_id", type = "string", source = "authenticated_subject" }]
```

The caller submits `{}`. The runtime resolves `owner_id` from the verified
security context's `acting_for` subject, falling back to its principal ID.
Caller-supplied values are rejected, including a value equal to the real subject.
Missing, anonymous, system and service identities cannot supply this source.
An empty delegated subject is rejected rather than falling back to another owner.
Attribution headers and `AgentContext.agent_id` do not establish identity.

This requires `strict_action_params = true` and a string parameter. Omit the
bound parameter from the caller-facing CSDL action signature. The resolved value
participates in normal action validation, effects and durable event persistence.
Later actions must omit the owner parameter if ownership is immutable.

Binding does not grant permission to create or modify an entity. Cedar still
controls action access against pre-action state. A policy can allow authenticated
creation before an owner exists, then authorize later operations against the
stored owner. This feature does not project related entities into authorization.

The native core dispatcher and OData PostgreSQL actor path resolve inputs before
sending an action to the actor. Composite preflight resolves the same inputs
before atomic writes, while its fallback passes original inputs back to core. Native OData additionally validates a resolved
copy before materialization, then dispatch resolves the original request again.
Trusted actor messages and replay events are internal runtime interfaces, not
untrusted request entry points. Service reactions should inherit already-bound
values using ordinary declared parameters, not attempt to impersonate a caller.

## Dispatch time

A string parameter can also be bound to the time of the dispatch:

```toml
params = [{ name = "moved_at", type = "string", source = "dispatched_at" }]
```

The runtime sets `moved_at` on every dispatch of the action, whoever makes it:
a caller, a state timer, a reaction or a scheduled action. The value is the
simulation clock (`sim_now()`) in RFC 3339 UTC with milliseconds, for example
`2026-10-23T12:34:56.789Z`, the form JavaScript's `Date.prototype.toISOString`
writes, so stored values sort as text in time order. A caller cannot supply it:
a request that sends the parameter is refused exactly as for
`authenticated_subject`, including when the value is right. Unlike
`authenticated_subject` it needs no caller identity, so a state timer's or a
service's move gets its own time. The same rules apply: it requires
`strict_action_params = true`, a string type, and leaving the parameter out of
the caller-facing CSDL action signature.

Regression tests: `cargo test -p temper-server --test authenticated_params` and
`cargo test -p temper-server --lib authenticated_params`.
The tests use actual verification results, OData handlers, a local libSQL
journal, and the PostgreSQL actor runtime. The PostgreSQL regression rejects
caller overrides before enqueueing, executes an accepted action, and reads the
committed owner through a fresh actor system. It starts an isolated PostgreSQL
testcontainer (Docker required), or uses the existing test helper's
`TEMPER_ACTOR_TEST_DATABASE_URL` override for a local database named
`temper_test_*`. No database test is silently skipped.
