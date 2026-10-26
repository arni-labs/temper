# Predicates

Every condition in an IOA spec is one string in one grammar (ADR-0179):

| Where | Key | Reads |
|---|---|---|
| `[[action]]` | `guard` | the entity before the action |
| `[[action.triggers]]` | `guard` | the entity after the action |
| `[[invariant]]` | `assert` | every reachable state (proven by the verification cascade) |
| `[[field_invariant]]` | `assert` | the entity's fields after a write (checked on writes) |

An action's `effect` is a list of statements over the same literals and names ([Effects](#effects)), and trigger and state-timeout `args` are values in it ([Arguments](#arguments)).

```toml
[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > 0 && has_address"

[[invariant]]
name = "ShippedHasItems"
assert = "status in ['Shipped', 'Delivered'] => items > 0"

[[field_invariant]]
name = "ExternalNeedsUrl"
assert = "kind == 'external' => !empty(url)"
message = "external links need a url"
```

## Grammar

```text
expr    = implies
implies = or [ "=>" implies ]          lowest precedence, right-associative
or      = and { "||" and }
and     = unary { "&&" unary }
unary   = "!" unary | atom
atom    = "(" expr ")" | "empty" "(" name ")" | name | operand cmp operand
        | operand [ "not" ] "in" ( "[" literal, ... "]" | name )
cmp     = "==" | "!=" | "<" | "<=" | ">" | ">="
operand = "status" | name | "len" "(" name ")" | Type "[" name "]" "." "status" | literal
literal = integer | 'string' | true | false | null
```

- **Strings use single quotes**, so an expression sits inside an ordinary TOML `"..."` string.
- **`status`** is the entity's status. **Bare names** are declared state variables in action guards and invariants (plus reference fields inside `empty(...)` and `Type[...]`), and any entity field in trigger guards and field invariants.
- **A bare name is a boolean test**: `ready`, `!ready`. Anything else must be compared.
- **`null` means absent**: `x == null`. **`empty(x)`** is absent, null, `''` or `[]`.
- **`len(list)`** and **`'v' in list`** read list state variables.
- **`Type[ref].status`** is the status of the related `Type` entity whose id is in field `ref` (one id, or a list of ids). An unset reference, or one whose entity is not found, reads as `null`. A comparison must hold for **every** related entity.
  - required: `Workspace[workspace_id].status in ['Active']`
  - optional: `empty(workspace_id) || Workspace[workspace_id].status in ['Active']`
  - only a bad state blocks: `Workspace[workspace_id].status not in ['Frozen', 'Archived']`
- **`=>`** is implication: `status in ['Done'] => approved`.

## Effects

`effect` is a list of statements, type-checked when the spec loads (ADR-0180):

```toml
effect = [
  "items += 1",                         # counter add: int, counter var, or params.p
  "retries -= 1",                       # counter subtract, stops at 0
  "quota_limit = params.quota_limit",   # counter set
  "ready = true",                       # bool set: true/false, bool var, or params.p
  "append(tags, params.tag)",           # list append: 'string' or params.p
  "remove_at(tags, params.tag_index)",  # list remove by index; out of range is a no-op
  "schedule('Expire', 3600)",           # dispatch an action on this entity after N seconds
  "schedule_at('Expire', expires_at)",  # dispatch at the timestamp in a field
  "spawn('Task', 'Create', last_task_id)",  # create a child, run 'Create' on it, store its id
]
```

```text
effect  = assign | call
assign  = name ( "=" | "+=" | "-=" ) arg
call    = "append" "(" name "," arg ")" | "remove_at" "(" name "," arg ")"
        | "schedule" "(" 'action' "," integer ")" | "schedule_at" "(" 'action' "," name ")"
        | "spawn" "(" 'type' "," 'action' [ "," name [ "," arg ] ] ")"
arg     = integer | 'string' | true | false | name | "params" "." name
```

- **`params.p`** is the action parameter `p`.
- **Only counters and bools are assigned**; `+=` and `-=` only on counters. Lists take `append`/`remove_at`, and their elements are strings.
- **`schedule`/`schedule_at` targets** must be declared actions.
- **`spawn`** creates a child with a fresh id, or the id in its optional fourth argument (`'string'` or `params.p`). The child's initial action receives the parent action's params plus `parent_type`, `parent_id` and `<parent>_id`.
- **Nothing is implied by an action's name.** An action with no `effect` changes only `status`.
- **Work on other entities, modules, adapters, webhooks and platform hooks** is an `[[action.triggers]]` entry, not an effect ([reactions.md](reactions.md)).

## Arguments

`[[action.triggers]]` and `[[state_timeout]]` pass parameters to the action they dispatch with one `args` table (ADR-0181). Each value is a string holding a literal or a name:

```toml
args = { source = "'doc'", attempts = "3", urgent = "true", reason = "null", doc_id = "Id", owner = "owner_id" }
```

- **Literals:** `'text'` in single quotes (a literal cannot contain `'`), an integer, `true`, `false`, `null`.
- **Names:** `Id` is the entity's id; any other name is that field, read after the source action commits (triggers) or when the timer is armed (timeouts). A trigger may name a declared state variable or a parameter of its action; a timeout a declared state variable. Anything else fails to load, so a string written without its single quotes is caught.
- **A missing field** leaves its key out of the dispatched params (logged).

## Terminal states

States no action may leave are listed on the automaton, not written as an invariant:

```toml
[automaton]
states = ["Draft", "Done", "Cancelled"]
initial = "Draft"
terminal = ["Done", "Cancelled"]
```

## What the verifier can prove

`[[invariant]]` asserts are proven over every reachable state, so they may only read what the model tracks: `status`, `counter`, `bool` and `list` state variables. An assertion over a string or number variable, an entity field, or a related entity fails to load; state it as a `[[field_invariant]]` instead, which is checked whenever the entity is written.

Guards may also read values the model does not track (string variables, reference fields, related entities). The verifier treats those as unknown: an action gated on one may fire, but is not guaranteed to.

Effects that read `params.p` are verified with `p` unknown: a counter takes every value in `0..=bound`, a bool both values, and a list element every string literal a guard or invariant compares that list against, plus one fresh value.

## Proof hints

Every `[[invariant]]`, over one record or several (one that reads `Type[...]`), and every `[[liveness]]` with `eventually`, is checked when an app is installed, and by `temper verify`. Each rule is proved by induction first, for every run, and walked on the joint model of its type and the types it names, which covers the runs a bounded walk reaches, only when the proof does not hold. Both read the kernel's own moves and the app's Cedar, which the per-entity cascade does not. A WASM module's or adapter's answer is a kernel move: the trigger's `on_success` when it names one, and otherwise any action of the record, since the module's result names it and the kernel dispatches it with no Cedar check. The proof and the walk take exactly those answers. The callee chooses returned parameters independently of call-time fields; constraints compare them with the record when the answer arrives. Background WASM answers run as `service:wasm-runtime`, and adapter answers as `service:platform-dispatch`; an entity reaction without an explicit principal inherits that sender. An invariant with no lemmas is tried at every state, then with the prover's own guesses about the flags it reads. Hints written beside the rule give the proof what it cannot guess:

```toml
[[invariant]]
name = "closed_parent_is_locked"
assert = "Parent[Id].status == 'Closed' => Parent[Id].locked == true"

[[invariant.lemma]]
name = "locked_past_open"
assert = "(=> (not (= $Parent.status Parent/Open)) $Parent.locked)"
```

- `[[invariant.lemma]]` and `[[liveness.lemma]]`: `name` and `assert`, a clause the proof holds alongside the rule, true at the start and kept by every move. It is SMT-LIB over `$`-framed names: `$Type.field`; `$Type.status`, compared with `Type/State`; `$n.Kind`, how many dispatches of a kind are in flight; and `($has.Kind m)` for a dispatch `m` of sort `Msg/Kind`. A reaction's kind is `Type.Action`, a scheduled action's `Type.Action.scheduled`, a module call's `Type@trigger`.
- `measure`, on a `[[liveness]]` with `eventually`: integer terms over the same names, compared in order, that each helpful move lowers.
- `[[liveness.region]]`: `name`; `region`, the states it covers; `step`, the move the system makes there, by its label (such as `Type.Action arrives` or `Type.Action (answer to Type@trigger)`); `witness`, the kind of the dispatch or call `w` names, when the step takes one; and `params` for the step's parameters. The regions cover every state short of the goal, and each step reaches the goal or lowers the measure.

A rule proved with the prover's guesses lists them in its verdict, as lemmas that can be written beside it so the next check proves it without guessing. A `[[liveness]]` is tried only when it carries a `measure`, which nothing guesses. A rule whose proof does not hold is walked as before, and its verdict says why, such as the move after which a clause fails, so the lemma it lacked can be added. `cargo run -p temper-verify --example induct <spec dir>` prints each failing move with a counterexample (`CTI=<move>`). A scheduled action waits on its own record, with no parameters, and fires at any time as whoever armed it; like a state timer it does not keep the record from being settled, and a `schedule_at` may not arm, since the proof does not read the timestamp. A spawn's first action on a type in scope is a move of its own, with any parameters, since the child may have another id. The joint walk makes neither move, so a rule whose proof does not hold is walked without them, as before. Each rule is proved on its own, for up to two minutes (`PROOF_TIME`: over quantified lemmas the solver may search without end), and its walk, when the proof does not hold, takes what memory the host leaves. The solvers running at once are at most the machine's cores. A verdict is kept by the digest of the rule's walk model: a rule the proof holds holds in every state that walk reaches. A proved rule whose premise no run meets proves nothing and fails like a vacuous walk. The solver decides that first: when no state the proof's clauses allow meets the premise, the verdict says so with no walk. Otherwise the joint model is walked to the first state that meets the premise, and the verdict gives that walk's states and time. An invariant whose premise no run meets only because the app's Cedar lets nobody send a move is checked again with every sender allowed. When a run meets the premise there and the rule still holds, its premise waits on Cedar the app has yet to write: the rule holds, and its verdict names the moves in scope that Cedar lets nobody send. One that breaks with every sender allowed holds only because Cedar refuses a move, and one whose premise no run meets even then is dead in its spec; both prove nothing, as before. An `eventually` rule is not checked again, since more senders may keep a record from its goal.

An install refuses a rule that neither proves nor walks, unless the tenant runs that rule now as it is: the digest of its model, from the specs and Cedar the tenant runs, is the same. The digest reads what Cedar decides, so an install's `@source` labels and a statement written twice leave it as it was. Then the rule keeps running, the install logs it and counts it in `temper_os_app_unproven_rule_running_total`, and the next change to it, or to what its model reads, is refused until it holds. A better checker in a new kernel so never takes a running app down at its next start; it blocks the next change instead. A type the tenant does not run yet has no such digest, so its rules must hold.

## Converting old specs

Specs written before this grammar (guard tables and clauses such as `is_true x` or `min x 3`, `when` + `assert`, `no_further_transitions`, trigger-guard and field-predicate tables, table or verb-string effects such as `{ type = "increment", var = "items" }` or `"set ready true"`, `trigger`/`emit` effects and `[[integration]]` blocks) fail to load with a hint. Convert them in place:

```bash
temper migrate-predicates path/to/*.ioa.toml
```

The converter rewrites effects as statements, moves integrations onto the actions that triggered them as `[[action.triggers]]`, writes out the effects the old action-name heuristic implied (`AddItem` incrementing counters), and prints a note for anything it drops: `emit` effects, integrations no action triggered, and webhook integrations without a `url` (which never fired).

It also converts the value spellings and key names that preceded strict typed reading (ADR-0181): string booleans and numbers (`"true"`, `"0"`) become TOML values; `[[state]] initial` is written in its type; the removed type names map to `string`, `bool`, `counter`, `int` and `list`; `Composite` becomes `composite`; trigger `to_state` becomes a `status == '...'` conjunct of `guard`; `params`/`params_from` become `args`; `resolve_target` `type`/`field` become `kind`/`id_field`; webhook `entity_lookup`/`entity_param` become `entity_id = "query.<name>"`; `adapter_type` becomes `adapter`. Keys the old reader ignored — unknown keys and sections, `[automaton.timeouts]`, keys of another trigger kind (such as a `guard` on a wasm trigger), composite-only keys on other actions — are dropped with a note, so the converted spec means what the old one did.
