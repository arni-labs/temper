# ADR-0183: Permissions as records

- Status: Proposed
- Date: 2026-10-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0032: Granular Cedar policy storage (the `policies` table this extends)
  - ADR-0039: Authz policy traceability (`@source` statement names)
  - ADR-0045: Field overflow inline ceiling
  - `crates/temper-authz/src/engine/mod.rs`, `crates/temper-platform/src/os_apps/mod.rs`, `crates/temper-server/src/state/dispatch/effects.rs`

## Context

Line numbers are at arni-labs/temper `7ee9aa7c` and katagami `b7f4648d7`.

**Policies are one text per tenant.** An app install takes the tenant's live Cedar text, cuts out what that app wrote last time, pastes in the bundle's Cedar (`os_apps/mod.rs:1205-1217`), and stores the result as one row per tenant (`tenant_policies`, `temper-store-turso/src/schema.rs:185`; written at `os_apps/mod.rs:1294`). It then re-parses the whole text into a new `PolicySet` (`engine/mod.rs:174-186`, called at `os_apps/mod.rs:1419-1428`). A second copy of the same text is kept in memory (`ServerState::tenant_policies`, `state/mod.rs:525`). ADR-0032's per-entry `policies` table (`schema.rs:198`) holds whole blobs per entry, and its rows are concatenated at boot (`temper-cli/src/serve/bootstrap.rs:325-360`). More than ten other paths rebuild a tenant's text and reload it whole, for example `hooks.rs:225-243`, `api/decisions.rs:226`, `api/mod.rs:372` and `operator_manage_policies.rs:83`. Changing one statement re-parses every statement in the tenant.

**Memberships reach Cedar as lists.** Cedar's principal is built with no parents (`engine/mod.rs:578-581`), so a policy cannot ask "is this person in this group". Apps write their people into data or into policy text instead:

- **In a field.** Katagami's control app keeps `member_subs` and `removed_subs` on the app record, and its Cedar calls `resource.member_subs.contains(principal.id)` (`policies/katagami_app.cedar:33`). Cedar's resource attributes are the record's fields (`entity_ops.rs:386`). A field over 131,072 bytes of JSON leaves the record as a blob ref (`entity_actor/effects.rs:893`, `:1062`), and `.contains` on a ref errors. So katagami refuses a Join past that size (`apps_build/src/people.rs:23`). That is about 3,360 members.
- **In policy text.** An app's own Temper learns its people only from its Cedar. Each `{{is_member}}` expands to the whole member list, twice (`apps_build/src/terms.rs:49`): 1,440 bytes per member for a 16-statement app. So every Join writes a new git commit holding all of the app's Cedar (`apps_build/src/versions.rs:748-752`). The app then serves it only after a Deploy, which stops the app's Temper and starts it again on the new commit (`specs/katagami_app_install.ioa.toml:639`).

Measured with cedar-policy 4.9.1: at 3,359 members that Cedar is 4.84 MB and takes 181 ms to parse; at 1,000 members, 1.44 MB and 60 ms. Each Join pays that, plus the commit and the restart.

## Decision

Each permission is one record, and each membership is one record. Cedar reads memberships as the principal's parents.

### Policies: one record per statement

- **Storage.** A row in `policies` is one Cedar statement, keyed `(tenant, name)`. `name` is the statement's existing `@source` label, `<app>/<file>#<n>` (ADR-0039), and the row stores the statement's text and hash.
- **Install.** An install diffs its bundle's statements against that app's rows by name and hash: it adds new rows, replaces changed ones and deletes gone ones. Nothing else in the tenant is read or rewritten. The other reload paths write rows the same way.
- **Cache.** The engine keeps each tenant's statements parsed, keyed by name and hash. A changed row re-parses only that statement. The tenant's `PolicySet` and candidate index are then rebuilt from the already-parsed statements, which is a clone, not a parse. Membership changes never touch policies.
- **Removed.** The `tenant_policies` text, `ServerState::tenant_policies`, and the whole-text reload paths. `reload_tenant_policies` takes row changes instead of text.

### Memberships: one record each

- **Record.** A kernel type `GroupMembership`, bootstrapped beside `PrincipalGeneration`. Its fields are `group` (a Cedar entity uid such as `KatagamiApp::"a1k7p3x9m2qd"`) and `member` (a principal id). Its states are `Active` and `Ended`, with actions `StartMembership` and `EndMembership` (not `Join`, so an app's `Action::"Join"` permit never covers membership writes). Its id is the hex sha256 of `group` and `member`, so a person is in a group at most once, and the record's own id can be checked. Who may write memberships is the tenant's Cedar, as for any record.
- **A Join is one write:** `POST /tdata/GroupMemberships('<id>')/Temper.StartMembership {group, member}`. A removal or a leave is `EndMembership` on the same record. A field update or a delete of the record is applied the same way. Neither writes policy text, a commit or a list.
- **How authorization finds a principal's groups.** The engine asks only about groups the request can test: the request's resource, plus the entity literals the tenant's policies compare with `principal in`, collected when the statements are parsed. For each candidate it makes one lookup in an in-memory set of active memberships, keyed `(tenant, group, member)`, for the principal's id and for the person an agent acts for. Each hit becomes a parent of the principal entity, so `principal in resource` and `principal in KatagamiApp::"<id>"` work. The bound on lookups per check is 1 plus the number of `principal in` literals in the tenant's own policies, so it does not grow with members. Each lookup is O(log n). The set holds one entry per active membership record, so its memory grows with stored data, as `entity_index` does. A principal never carries all of its groups: at 10,000 memberships that would be 10,000 parents on every check, and on every row of a list read.
- **Keeping the set current.** Every committed change of a `GroupMembership` (an action through the post-dispatch path, `dispatch/effects.rs:643`, a field update or a delete) updates the set. Each entry carries the record's sequence number, so a late hydration read never overwrites a newer transition. A tenant's set is filled from its records before the tenant's first authorization: at boot, and in the identity resolver as `PrincipalGeneration` is. If the set cannot be filled, it fails closed.
- **An app's own Temper** holds the memberships of its own app. A Join writes one record there, replacing the commit and the restart, and the app's Cedar says `principal in KatagamiApp::"<id>"`. That Cedar is the same size for any number of people.

## Rollout Plan

1. **Kernel release A.** Add `GroupMembership`, the lookup and per-statement rows. At boot, rows are read if present, and otherwise the old text is split into rows once. For one release, each row change also rewrites the `tenant_policies` text from the rows, so the kernel before A can still boot on this database. Rollback is to redeploy the kernel before A.
2. **Katagami.** The control plane writes a `GroupMembership` for every existing member. This is idempotent, because the id is deterministic. The control Cedar then switches to `principal in resource`, while `member_subs` and `removed_subs` are still kept. Each app's next Deploy carries Cedar with `principal in KatagamiApp::"<id>"` and writes its members into that app's Temper. Rollback until step 3 is to restore the previous Cedar, because the lists are still current.
3. **Remove.** Katagami stops writing the lists, the commit per Join and the 131,072-byte refusals (`app_full`, `removed_full`). Kernel release B stops writing the `tenant_policies` text and drops the whole-text reload paths. From here, going back means restoring Cedar that reads lists. So step 3 waits until step 2 has been live and checked.

## Measurements to take

Before step 1 ships, on one machine with the same inputs before and after:

- **One authorization**, time and the engine's memory, at 10, 1,000 and 10,000 active memberships for one principal, and at 10, 1,000 and 10,000 policy statements in the tenant. The expectation is flat in memberships and linear only in the statements Cedar evaluates.
- **A list read of 1,000 rows** with `principal in resource`: the per-row cost.
- **Filling the membership set** for a tenant with 10,000 memberships, cold, and how long the first request waits for it.
- **Changing one statement** in a tenant of 10,000 statements: today's whole re-parse against a one-statement parse plus the rebuild.
- **A Join, before and after:** today a 4.84 MB Cedar commit plus a Deploy restart at 3,359 members (time to the joiner's first allowed request), against one `GroupMembership` write.

## Consequences

- A permission or a membership can be read, audited and changed on its own. No list grows with an app's people.
- Policies that read lists keep working, so apps move when they are ready.
- One server serves a tenant today. A second server for the same tenant would need membership and statement changes delivered to it, and this ADR does not add that.
