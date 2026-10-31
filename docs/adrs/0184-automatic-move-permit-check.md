# ADR-0184: Check automatic moves against the principal that will send them

- Status: Proposed
- Date: 2026-10-31
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Unified action triggers (a reaction runs as its declared principal, or as the caller)
  - temper#194: a module's answer runs as whoever made the move
  - `crates/temper-server/src/automatic_moves.rs`
  - `crates/temper-server/src/trigger/principal.rs`
  - `crates/temper-platform/src/os_apps/install_checks.rs`

## Context

On 31 Oct 2026 the Katagami app Far Bank (`a7lo6uzfupl4`) stuck a round in Filing. Its Cedar allowed only `service:wasm-runtime` to Finish a Round. temper#194, merged 28 Oct, makes a module's answer and the reactions that follow it run as the principal that made the originating move. Finish therefore ran as the person and Cedar refused it: no matching permit.

#194's note grepped this repository for `wasm-runtime` policies. It did not see app policies. Katagami's build-time check (`follow_ups.rs`, katagami#1344) still treats the moves after a module callback as `service:wasm-runtime`, so the app was allowed to install. That check runs only when the app is built. A kernel change underneath a live app is not looked at again.

The joint model in `temper-verify` already describes the post-#194 principal, but it is a second write-up of the dispatcher, kept for proofs. The Far Bank miss is what a second write-up does.

## Decision

The kernel derives the principal of every automatic move by calling the functions the dispatcher uses, then asks Cedar whether that principal can be permitted. There is one principal rule.

### What is an automatic move

- An entity trigger (a reaction). Cedar is asked, as `try_reaction` asks.
- A module, adapter or webhook answer (`on_success` / `on_failure`). The answer itself is not action-authorized. It runs as the caller: `for_callback` then `for_answer`. The triggers that follow it are checked as that caller.
- A state timer. Its own fire is not action-authorized. It runs as whoever armed it: `timer_agent` then `mark_timer_fire`. The triggers that follow the fire are checked as that principal. A timer whose arming principal was not stored fires as `service:state-timer`; this check does not treat that boot fallback as a sender.
- A `schedule` or `schedule_at` effect. The fire runs as the caller (`for_scheduled_follow_on`, then `mark_timer_fire`, which is what the fire does). Triggers after it are checked as that principal.
- A `spawn` effect. The child runs as the caller (`for_spawned_child`). Triggers after the child's first move are checked as that principal.

An input or composite action starts a chain when Cedar does not refuse an outside principal for it. A trigger's `principal` switches the chain to that service from that hop on, because `resolve_trigger_principal` does.

### How Cedar is asked

Each identity kind has one representative: a Customer (`outside`) or `AgentContext::for_service` for a named service. The representative is passed through the dispatcher function. The security context that function returns is the one Cedar sees, including `context.via`.

`SendAnalysis` answers from the policies alone:

- `May::No` — Cedar refuses that kind whatever the record holds. The finding is `refused_follow_up` and the install stops.
- `May::Yes` — Cedar allows it. No finding.
- `May::Maybe` — it depends on something the policies-alone question does not know. The real authorizer is then asked for the representative, with an empty record. If that representative is allowed, there is no finding: a member permit (`principal is Customer`) is this case, and it passes. If the representative is refused, the finding is `conditional_follow_up`, a warning, and the install proceeds.

`May::Maybe` is a warning because the missing fact is a record attribute (`resource.owner`) or which outside principal is sending. Those are true or false per record at runtime. Refusing the install would block apps whose permits are deliberately about the record. `May::No` is the Far Bank failure: no record makes the principal acceptable.

A permit that names `Agent::"service:<name>"` is `dead_service_permit` when no move of these specs runs as that service. It is an error even when another permit allows the follow-up. The permit looks like authorization and is not. Forbids are ignored: forbidding a service that never sends changes nothing. The platform's built-in policies name no service principal.

An empty policy permits every move. A policy that does not parse is an error. The check parses specs with liveness in warn-only mode; liveness is a different check.

### When it runs

On os-app install, when the install brings specs or Cedar. The specs are the tenant's, with the bundle's on top. Errors refuse the install before anything is written. Warnings are returned on the install result as `automatic_moves`.

The same function is `temper_server::automatic_moves::check`. Katagami should call it from app build and delete `katagami-control/wasm/apps_build/src/follow_ups.rs`. That file still marks a module callback's callers with the service bit. This function is the result API: a `Report` of errors and warnings, with `refused_follow_up`, `dead_service_permit` and `conditional_follow_up`. Linking `temper-server` is the cost of calling the dispatcher functions rather than copying them. A thinner crate would be a second home for those functions.

### Follow-up: recheck installed apps when the kernel changes

FOLLOW-UP(kernel-upgrade-recheck). Not in this change.

When a process starts on a kernel other than the one that admitted an installed app, run `check` over that app's specs and Cedar. Record the report against the app. Keep serving the app. Do not refuse to boot, and do not uninstall: a refusal at boot would take down every app the new kernel flags, including ones a person is in the middle of using. The install path already stops a new or upgraded app. The boot path is how Far Bank would have been visible on 31 Oct without waiting for the next install.

`KERNEL_UPGRADE_RECHECK` in `automatic_moves` is the marker. The install comment points at the same gap.

## Rollout Plan

1. **This change** — the check, its tests (including the Far Bank chain), and the install refusal. No boot scan. No deploy. No change to a running tenant.
2. **Katagami** — call `check` at app build and delete `follow_ups.rs`. Do this after apps run a kernel that contains this check, so the two do not disagree during a rollout.
3. **Kernel upgrade recheck** — the boot report described above.

## Consequences

### Positive

- A change to who sends a follow-up and the check that permits it ship in the same functions.
- An app whose Finish only `service:wasm-runtime` may send is refused at install, instead of sticking a record in Filing.
- A permit that names a service the dispatcher never uses is an error, so a grep of this repository is not the only place that notices it.

### Negative

- A `May::Maybe` warning can be noisy for permits that are about the record. It does not refuse the install.
- The boot fallback `service:state-timer` is not a sender here. A permit that names only that service does not cover the principal who armed the timer, and is reported as a dead permit.
- Katagami's owner, member and guest bits are finer than `outside`. This check does not reproduce them. `outside` is one Customer representative. A permit that allows only an agent, or only one person's id, is a warning when the representative Customer is refused, not a hard refusal.

### Risks

- Install of an app that still permits `service:wasm-runtime`, and does not declare that principal, starts failing until the permit is removed or the trigger declares the principal. That is the point of the check. Live apps keep running until they are installed again, or until the boot recheck exists.
- A spec whose given type name is not its automaton name is refused. The registry's type and the automaton's name are the same type today.

### DST Compliance

The check calls the dispatcher's principal functions and does not run inside the simulator. It adds no clock, no unordered map that holds simulation state, and no I/O. `authorize` takes a `HashMap`; the call site is marked `determinism-ok` because the map is the Cedar API's argument and the check does not iterate it. `for_spawned_child` is the spawn path moved onto `AgentContext`; the key it builds is the one spawn already built.

## Non-Goals

- Boot-time recheck of installed apps (the follow-up above).
- Replacing Katagami's owner, member, guest and hook-token matrix. That stays until Katagami calls this function and decides what of that matrix the kernel's report does not cover.
- Authorizing a timer's own fire or a module answer. The dispatcher does not ask Cedar about those actions.
- Changing who a follow-up runs as.

## Alternatives Considered

1. **Keep the check in Katagami and update the service bit.** It would still be a second write-up, and it would still run only at app build. Rejected: that is the miss.
2. **Refuse on `May::Maybe`.** A permit constrained by `resource.owner` would block install of apps that work for the records they are about. Rejected.
3. **Treat `service:wasm-runtime` as the callback principal again.** That would make Far Bank's permit true and the person unable to chain a run after a module answer, which #194 changed on purpose. Rejected.

## Rollback Policy

Revert the change. The check refuses installs; it does not rewrite stored specs, policies or records. Apps installed before it keep the policies they have.
