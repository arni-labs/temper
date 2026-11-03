# ADR-0185: A `due_on` Move Brings the Record's Armed Timer Forward

- Status: Accepted
- Date: 2026-11-02
- Deciders: Temper core maintainers
- Related:
  - ADR-0049: State-entry timeouts and the durable scheduler
  - ADR-0056: Durable state timeouts
  - `crates/temper-spec/src/automaton/types/mod.rs` (`StateTimeout::due_on`)
  - `crates/temper-server/src/state/dispatch/state_timer_due.rs`
  - `crates/temper-server/tests/due_on_brings_timer_forward.rs`

## Context

A WASM module's answer acts as the person who called it (#194), a timer fires as whoever armed it, and since #203 a module's own calls are checked as requests. An app that takes outside data, such as a GitHub webhook, keeps that data from gaining its owner's standing: its Cedar refuses work, like starting a run, that a hook's move sets off. That rule is right.

But then a webhook that records "CI finished" on a record cannot start the owner's follow-on work, and the app polls instead: a state timer armed by the owner checks every few minutes. The event has arrived, and the record waits for the next poll.

A reaction that took the owner's identity from the record (#338, reverted in a40a47fc) let outside data choose whose identity a move carries. That is the attack the guard exists to stop.

## Decision

A `[[state_timeout]]` may list actions in `due_on`. When one of them is taken while the record is in that state, the timer the record already has armed is due now. It fires as it would have at its deadline: as whoever armed it, `via == "timer"`, with the arguments read when it was armed.

```toml
[[state_timeout]]
state = "WaitingForCi"
after_seconds = 300
on_timeout = "CheckCi"
reset_on = ["CheckCi"]
due_on = ["CiReported"]
```

The move's caller decides only when, never who or what:

- Same record, same state: the key sits on the state's own timer, and a move acts only on its own record.
- Only an armed timer: the timer the record had armed before the move. Nothing is armed, and a record with no timer is left as it is.
- Only sooner: a due time is never later than the declared deadline.
- Identity never changes. A timer the caller armed or re-armed itself (entering the state, or a `reset_on` move) is the caller's, and fires with the caller's standing. An action may not be in both `reset_on` and `due_on`.

Cedar still decides every move the fire sets off, as the owner's timer. A hook gains nothing it could not get by waiting for the deadline.

**Why a `[[state_timeout]]` key and not an effect**: the timer belongs to a state, and `reset_on` already lists the moves that change it. An effect on the action would have to name a state's timer, would mean nothing when the action is taken from a state with none, and would add a statement that every effect interpreter (the transition table, the verifier's models) must learn although it writes no state. The joint model needs no change: it fires an armed timer at any moment, so an earlier fire is a behaviour it already explores, and a `due_on` move does not change who armed the timer.

## Consequences

### Positive
- A webhook's event reaches the owner's follow-on work at once, with no change to who may start it.
- Polling stays the fallback: a lost bring-forward fires at the deadline.

### Negative
- The earlier due time is held in this process only. A restart arms the timer from its row, at its deadline. A deploy overlap in which another process holds the timer leaves it at its deadline.

### Risks
- A repeating timer (`reset_on` lists its own action) re-arms as its armer on each fire, so a caller who may make the `due_on` move often may make the timer fire as often. Each fire is still the owner's move, checked by Cedar; the app's guards and Cedar decide how often the `due_on` move may be made.

### DST Compliance
- Times come from `sim_now()`; the bring-forward is a new arming of the same timer through `start_armed_timer`, so the existing fences (journal sequence, cancellation value, arm id) order it against every other arm.
