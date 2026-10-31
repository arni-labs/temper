//! SMT symbolic verification (Level 0 of the verification cascade).
//!
//! Uses the Z3 SMT solver to verify properties algebraically without
//! enumerating states:
//!
//! 1. **Guard satisfiability** — Encode each guard as a Z3 formula over
//!    integer counters (each up to its L1 bound) and boolean variables.
//!    Check SAT: if UNSAT, the guard is dead code (the action can never fire).
//!
//! 2. **Invariant induction** — An invariant is proved inductive when every
//!    initial state satisfies it and, for every transition, assuming the
//!    proved invariants and the transition's source states and guard in S and
//!    applying its effects to get S', the invariant's negation in S' is UNSAT.
//!    A proved invariant holds in every state the model check (L1) reaches;
//!    one L0 cannot prove is reported as not proved, with why, and L1 decides.
//!
//! 3. **Unreachable state detection** — BFS from initial state through
//!    transition targets to find states that can never be reached.

use std::collections::{BTreeMap, BTreeSet};

use z3::ast::{Bool, Int};
use z3::{SatResult, Solver};

use stateright::Model;
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Operand, Set, Truth, unmodelable};

use crate::model::builder::build_model_from_ioa;
use crate::model::semantics::{collect_list_contains_pairs, counter_bound, list_bound, truth};
use crate::model::types::{ModelEffect, ResolvedTransition, TemperModel};

/// Result of symbolic verification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmtResult {
    /// For each action, whether its guard is satisfiable (can ever fire).
    pub guard_satisfiability: Vec<(String, bool)>,
    /// For each invariant, whether L0 proved it inductive: true in every
    /// initial state and kept by every transition. `false` means not proved,
    /// not broken: [`SmtResult::not_proved`] says why, and the model check
    /// (L1) decides whether a reachable state breaks it.
    pub inductive_invariants: Vec<(String, bool)>,
    /// Why each invariant L0 did not prove inductive was not proved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_proved: Vec<(String, NotProved)>,
    /// States that cannot be reached from the initial state.
    pub unreachable_states: Vec<String>,
    /// Whether symbolic checks rely on bounded/abstract encodings.
    pub approximate: bool,
    /// Human-readable approximation notes for downstream reporting.
    pub approximation_notes: Vec<String>,
    /// Whether L0 found no problem: every guard can fire. An invariant L0
    /// cannot prove inductive is not a problem by itself; L1 decides it.
    pub all_passed: bool,
}

/// Why L0 did not prove an invariant inductive.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotProved {
    /// An initial state does not satisfy it.
    Initial,
    /// This action can take a state where every proved invariant holds to
    /// one where this invariant is false. That state may be unreachable.
    BrokenBy(String),
    /// It reads values L0 does not model (strings, related records).
    Unmodeled,
}

impl std::fmt::Display for NotProved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotProved::Initial => f.write_str("an initial state breaks it"),
            NotProved::BrokenBy(action) => write!(f, "{action} can break it"),
            NotProved::Unmodeled => f.write_str("reads values L0 does not model"),
        }
    }
}

/// Run symbolic verification on an IOA spec using the Z3 SMT solver.
///
/// This is the Level 0 entry point. It checks:
/// 1. Guard satisfiability: is there any state in which each guard can fire?
/// 2. Invariant induction: does each invariant hold after every transition?
/// 3. Unreachable states: can each declared state be reached?
pub fn verify_symbolic(ioa_toml: &str, max_counter: usize) -> SmtResult {
    let model = build_model_from_ioa(ioa_toml, max_counter)
        .expect("SMT: IOA spec should have been validated before symbolic verification");
    let approximation_notes = approximation_notes(&model);
    let approximate = !approximation_notes.is_empty();

    let guard_sat = check_guard_satisfiability(&model);
    let (inductive, not_proved) = check_invariant_induction(&model);
    let unreachable = check_unreachable_states(&model);

    // Unreachable states are warnings, not failures — specs may declare states
    // that are only reachable through composition or external actions. An
    // invariant L0 cannot prove is not a failure either: L1 decides it.
    let all_passed = guard_sat.iter().all(|(_, sat)| *sat);

    SmtResult {
        guard_satisfiability: guard_sat,
        inductive_invariants: inductive,
        not_proved,
        unreachable_states: unreachable,
        approximate,
        approximation_notes,
        all_passed,
    }
}

fn approximation_notes(model: &TemperModel) -> Vec<String> {
    let cross_entity_guard_count = model
        .transitions
        .iter()
        .filter(|transition| reads_related_entities(&transition.guard))
        .count();
    if cross_entity_guard_count == 0 {
        return Vec::new();
    }

    vec![format!(
        "{cross_entity_guard_count} transition(s) use abstract cross-entity guards; single-entity SMT reads their cross-entity parts as unknowns and leaves them out of reachability"
    )]
}

/// Whether a guard reads a related entity's status (not modeled here).
fn reads_related_entities(guard: &Expr) -> bool {
    !guard.cross_refs().is_empty()
}

// ---------------------------------------------------------------------------
// Guard satisfiability
// ---------------------------------------------------------------------------

/// For each transition, encode its guard as a Z3 formula and check SAT.
///
/// A guard is satisfiable if there exists an assignment of counter values
/// (each up to its bound) and boolean values that makes the guard true.
fn check_guard_satisfiability(model: &TemperModel) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|t| {
            let solver = Solver::new();

            // Check that at least one from_state exists in the state space
            if !t.from_states.is_empty() {
                let has_valid_from = t.from_states.iter().any(|s| model.states.contains(s));
                if !has_valid_from {
                    return (t.name.clone(), false);
                }
            }

            // Create Z3 variables for each counter, over the values L1 explores
            let counter_vars = make_counter_vars(model, &solver);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, "");
            let status_var = make_status_var(model, &solver);

            if !t.from_states.is_empty() {
                let from_formula = encode_state_membership(&status_var, &t.from_states, model);
                solver.assert(&from_formula);
            }

            // Encode the guard as a Z3 formula and assert it
            let symbols = Symbols {
                counters: counter_vars.into_iter().collect(),
                bools: bool_vars.into_iter().collect(),
                lists: &list_vars,
                status: status_var,
                prefix: "",
            };
            solver.assert(encode(&t.guard, &symbols, model));

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Invariant induction
// ---------------------------------------------------------------------------

/// Each invariant L0 did not prove, with why.
type NotProvedInvariants = Vec<(String, NotProved)>;

/// Which invariants are inductive, and why each of the others is not proved.
///
/// An invariant is proved when every initial state satisfies it and no
/// transition can break it from a state where every proved invariant holds,
/// within the transition's source states and guard. Candidates start as the
/// invariants the initial states satisfy; a transition that can break one
/// drops it, and the rest are checked again without it, until a pass drops
/// none (Houdini). The proved set is then inductive as a whole, so each holds
/// in every reachable state, and the model check (L1) cannot break it.
///
/// Every transition is checked, those that stay in their state and those
/// whose guards read related entities (the unknown parts of a guard are free,
/// so the guard may hold). Counters are only non-negative, so a proof about
/// counters holds at every count, not just up to L1's bound; lists are as
/// long as L1 explores them, so a proof about a list's length holds up to that
/// bound, as L1's check does. An invariant over values L0 does not model is
/// not proved. `TypeInvariant` and `Terminal(..)` are
/// structural: every target is a declared state, and no transition leaves a
/// terminal state.
fn check_invariant_induction(model: &TemperModel) -> (Vec<(String, bool)>, NotProvedInvariants) {
    let mut not_proved: BTreeMap<usize, NotProved> = BTreeMap::new();
    let inits = model.init_states();
    let mut candidates = Vec::new();
    for (index, inv) in model.invariants.iter().enumerate() {
        if unmodelable(&inv.assert, &model.var_kinds).is_some() {
            not_proved.insert(index, NotProved::Unmodeled);
        } else if inits
            .iter()
            .any(|init| truth(&inv.assert, &model.var_kinds, init) != Truth::True)
        {
            not_proved.insert(index, NotProved::Initial);
        } else {
            candidates.push(index);
        }
    }

    loop {
        let mut dropped = BTreeSet::new();
        for t in &model.transitions {
            for index in broken_by(model, t, &candidates) {
                not_proved
                    .entry(index)
                    .or_insert_with(|| NotProved::BrokenBy(t.name.clone()));
                dropped.insert(index);
            }
        }
        if dropped.is_empty() {
            break;
        }
        candidates.retain(|index| !dropped.contains(index));
    }

    let mut results = vec![(
        "TypeInvariant".to_string(),
        model
            .transitions
            .iter()
            .all(|t| t.to_state.as_ref().is_none_or(|s| model.states.contains(s))),
    )];
    results.extend(
        model
            .invariants
            .iter()
            .enumerate()
            .map(|(index, inv)| (inv.name.clone(), !not_proved.contains_key(&index))),
    );
    for state in &model.terminal {
        let closed = !model
            .transitions
            .iter()
            .any(|t| t.from_states.contains(state) || t.from_states.is_empty());
        results.push((format!("Terminal({state})"), closed));
    }
    let not_proved = not_proved
        .into_iter()
        .map(|(index, why)| (model.invariants[index].name.clone(), why))
        .collect();
    (results, not_proved)
}

/// The invariants among `candidates` (indices into `model.invariants`) that
/// `t` can break: from a state where every candidate holds, `t`'s source
/// states and its guard, to a post-state (its effects applied, as L1 applies
/// them) where the invariant is false. A solver that cannot decide counts as
/// a break: L0 proves nothing it has not shown.
fn broken_by(model: &TemperModel, t: &ResolvedTransition, candidates: &[usize]) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let solver = Solver::new();
    let zero = Int::from_i64(0);

    let mut pre_counters = BTreeMap::new();
    for name in model.initial_counters.keys() {
        let pre = Int::new_const(var_name("pre:", "counter", name));
        solver.assert(pre.ge(&zero));
        pre_counters.insert(name.clone(), pre);
    }
    let pre_bools: BTreeMap<String, Bool> = model
        .initial_booleans
        .keys()
        .map(|name| {
            (
                name.clone(),
                Bool::new_const(var_name("pre:", "bool", name)),
            )
        })
        .collect();
    let pre_lists = make_list_vars(model, &solver, "pre:");
    let pre_status = make_status_var(model, &solver);

    // Apply the effects in order, as L1 does; `params.p` is a fresh unknown
    // (a non-negative integer or a boolean). A list an effect changes may be
    // any list afterwards.
    let mut post_counters = pre_counters.clone();
    let mut post_bools = pre_bools.clone();
    let mut changed_lists = BTreeSet::new();
    let count = |arg: &Arg, counters: &BTreeMap<String, Int>| -> Int {
        match arg {
            Arg::Lit(Literal::Int(n)) => Int::from_i64(*n),
            Arg::Var(name) => counters.get(name).cloned().unwrap_or_else(|| zero.clone()),
            Arg::Param(name) => {
                let param = Int::new_const(var_name("", "param", name));
                solver.assert(param.ge(&zero));
                param
            }
            Arg::Lit(_) => zero.clone(),
        }
    };
    for effect in &t.effects {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = count(value, &post_counters);
                post_counters.insert(var.clone(), value);
            }
            ModelEffect::AddCounter { var, value } => {
                let delta = count(value, &post_counters);
                if let Some(post) = post_counters.get_mut(var) {
                    *post = Int::add(&[&*post, &delta]);
                }
            }
            ModelEffect::SubCounter { var, value } => {
                // L1 and the runtime stop at 0: max(counter - delta, 0).
                let delta = count(value, &post_counters);
                if let Some(post) = post_counters.get_mut(var) {
                    let dec = Int::sub(&[&*post, &delta]);
                    *post = post.gt(&delta).ite(&dec, &zero);
                }
            }
            ModelEffect::SetBool { var, value } => {
                let value = match value {
                    Arg::Lit(Literal::Bool(b)) => Bool::from_bool(*b),
                    Arg::Var(name) => post_bools
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| Bool::from_bool(false)),
                    Arg::Param(name) => Bool::new_const(var_name("", "param", name)),
                    Arg::Lit(_) => Bool::from_bool(false),
                };
                post_bools.insert(var.clone(), value);
            }
            ModelEffect::ListAppend { var, .. } | ModelEffect::ListRemoveAt { var, .. } => {
                changed_lists.insert(var.clone());
            }
        }
    }
    let changed = make_list_vars(model, &solver, "post:");
    let mut post_lists = pre_lists.clone();
    for var in &changed_lists {
        if let (Some(len), Some(elements)) = (changed.len_vars.get(var), changed.elem_vars.get(var))
        {
            post_lists.len_vars.insert(var.clone(), len.clone());
            post_lists.elem_vars.insert(var.clone(), elements.clone());
        }
    }
    let post_status = match t
        .to_state
        .as_ref()
        .and_then(|to| model.states.iter().position(|s| s == to))
    {
        Some(idx) => Int::from_i64(idx as i64),
        None => pre_status.clone(),
    };

    let pre = Symbols {
        counters: pre_counters,
        bools: pre_bools,
        lists: &pre_lists,
        status: pre_status,
        prefix: "pre:",
    };
    let post = Symbols {
        counters: post_counters,
        bools: post_bools,
        lists: &post_lists,
        status: post_status,
        prefix: "post:",
    };
    if !t.from_states.is_empty() {
        solver.assert(encode_state_membership(&pre.status, &t.from_states, model));
    }
    solver.assert(encode(&t.guard, &pre, model));
    for &index in candidates {
        solver.assert(encode(&model.invariants[index].assert, &pre, model));
    }
    candidates
        .iter()
        .copied()
        .filter(|&index| {
            solver.push();
            solver.assert(encode(&model.invariants[index].assert, &post, model).not());
            let broken = !matches!(solver.check(), SatResult::Unsat);
            solver.pop(1);
            broken
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Z3 helpers
// ---------------------------------------------------------------------------

/// Create Z3 integer variables for each counter, each in `[0, bound]` with
/// the bound L1 explores it to, which covers every literal it is compared with.
fn make_counter_vars(model: &TemperModel, solver: &Solver) -> Vec<(String, Int)> {
    let zero = Int::from_i64(0);
    model
        .initial_counters
        .keys()
        .map(|name| {
            let var = Int::new_const(var_name("", "counter", name));
            let bound = i64::try_from(counter_bound(model, name)).unwrap_or(i64::MAX);
            solver.assert(var.ge(&zero));
            solver.assert(var.le(Int::from_i64(bound)));
            (name.clone(), var)
        })
        .collect()
}

/// Create Z3 boolean variables for each boolean state var.
fn make_bool_vars(model: &TemperModel) -> Vec<(String, Bool)> {
    model
        .initial_booleans
        .keys()
        .map(|name| {
            let var = Bool::new_const(var_name("", "bool", name));
            (name.clone(), var)
        })
        .collect()
}

#[derive(Clone, Default)]
struct ListSymbolicVars {
    len_vars: BTreeMap<String, Int>,
    elem_vars: BTreeMap<String, Vec<Int>>,
    value_atoms: BTreeMap<String, i64>,
}

/// Create exact bounded symbolic list variables, named after `prefix`, for
/// each list up to the length L1 explores it to (its bound):
/// - its length, in `[0, bound]`
/// - one element per position below `bound`
///
/// Every string a guard or invariant tests for membership, and every initial
/// element, gets its own atom, so `'v' in list` is never decided by default.
fn make_list_vars(model: &TemperModel, solver: &Solver, prefix: &str) -> ListSymbolicVars {
    let zero = Int::from_i64(0);
    let mut len_vars = BTreeMap::new();
    let mut elem_vars = BTreeMap::new();

    for name in model.initial_lists.keys() {
        let bound = list_bound(model, name);
        let max_val = Int::from_i64(i64::try_from(bound).unwrap_or(i64::MAX));
        let len_var = Int::new_const(var_name(prefix, "len", name));
        solver.assert(len_var.ge(&zero));
        solver.assert(len_var.le(&max_val));
        len_vars.insert(name.clone(), len_var);

        let elements = (0..bound)
            .map(|idx| Int::new_const(var_name(prefix, &format!("elem{idx}"), name)))
            .collect::<Vec<_>>();
        elem_vars.insert(name.clone(), elements);
    }

    let mut values = BTreeSet::new();
    let exprs = model
        .transitions
        .iter()
        .map(|t| &t.guard)
        .chain(model.invariants.iter().map(|inv| &inv.assert));
    for expr in exprs {
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(expr, &mut pairs);
        for (_, value) in pairs {
            values.insert(value);
        }
    }
    for list in model.initial_lists.values() {
        for value in list {
            values.insert(value.clone());
        }
    }

    let value_atoms = values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| (value, idx as i64))
        .collect::<BTreeMap<_, _>>();

    ListSymbolicVars {
        len_vars,
        elem_vars,
        value_atoms,
    }
}

/// A Z3 constant's name: the state it belongs to (`prefix`), the kind of
/// value, and the spec's own name last, so constants of different kinds never
/// share a name (a counter `tags_len` and list `tags`'s length are two
/// constants).
fn var_name(prefix: &str, kind: &str, name: &str) -> String {
    format!("{prefix}{kind}|{name}")
}

/// Create a symbolic status variable over `model.states` indices.
fn make_status_var(model: &TemperModel, solver: &Solver) -> Int {
    let var = Int::new_const("status");
    let zero = Int::from_i64(0);
    if model.states.is_empty() {
        solver.assert(var.eq(&zero));
        return var;
    }
    let max = Int::from_i64((model.states.len() - 1) as i64);
    solver.assert(var.ge(&zero));
    solver.assert(var.le(&max));
    var
}

/// Encode `status ∈ states` as a disjunction over symbolic status index.
fn encode_state_membership(status_var: &Int, states: &[String], model: &TemperModel) -> Bool {
    let disjuncts: Vec<Bool> = states
        .iter()
        .filter_map(|state| {
            model
                .states
                .iter()
                .position(|s| s == state)
                .map(|idx| status_var.eq(Int::from_i64(idx as i64)))
        })
        .collect();
    if disjuncts.is_empty() {
        Bool::from_bool(false)
    } else {
        Bool::or(&disjuncts)
    }
}

/// Encode exact bounded `contains(list, value)` over symbolic list slots.
fn encode_list_contains(var: &str, value: &str, lists: &ListSymbolicVars) -> Bool {
    let Some(len_var) = lists.len_vars.get(var) else {
        return Bool::from_bool(false);
    };
    let Some(elements) = lists.elem_vars.get(var) else {
        return Bool::from_bool(false);
    };
    let Some(atom_id) = lists.value_atoms.get(value) else {
        return Bool::from_bool(false);
    };

    if elements.is_empty() {
        return Bool::from_bool(false);
    }

    let atom = Int::from_i64(*atom_id);
    let disjuncts: Vec<Bool> = elements
        .iter()
        .enumerate()
        .map(|(idx, element)| {
            let idx_int = Int::from_i64(idx as i64);
            Bool::and(&[&len_var.gt(&idx_int), &element.eq(&atom)])
        })
        .collect();
    Bool::or(&disjuncts)
}

/// Symbolic values for one state.
struct Symbols<'a> {
    counters: BTreeMap<String, Int>,
    bools: BTreeMap<String, Bool>,
    lists: &'a ListSymbolicVars,
    status: Int,
    /// Names this state's free atoms (pre/post) in a solver dump.
    prefix: &'static str,
}

/// Encode an expression as a Z3 formula. Each part over values the model
/// does not track (related-entity statuses, strings, fields, parameters) is a
/// fresh free atom, even where the same text appears twice: L1 reads every
/// such part as unknown, so `X && !X` may hold there, and L0 must not rule
/// out what L1 explores.
fn encode(expr: &Expr, sym: &Symbols<'_>, model: &TemperModel) -> Bool {
    let free = || Bool::fresh_const(&format!("{}unknown", sym.prefix));
    match expr {
        Expr::Const(value) => Bool::from_bool(*value),
        Expr::Not(inner) => encode(inner, sym, model).not(),
        Expr::And(parts) => Bool::and(
            &parts
                .iter()
                .map(|p| encode(p, sym, model))
                .collect::<Vec<_>>(),
        ),
        Expr::Or(parts) => Bool::or(
            &parts
                .iter()
                .map(|p| encode(p, sym, model))
                .collect::<Vec<_>>(),
        ),
        Expr::Implies(lhs, rhs) => encode(lhs, sym, model).implies(encode(rhs, sym, model)),
        Expr::Var(name) => sym.bools.get(name).cloned().unwrap_or_else(free),
        Expr::Empty(name) => match sym.lists.len_vars.get(name) {
            Some(len) => len.eq(Int::from_i64(0)),
            None => free(),
        },
        Expr::Compare { lhs, op, rhs } => {
            if let (Some(a), Some(b)) = (int_term(lhs, sym), int_term(rhs, sym)) {
                return match op {
                    CmpOp::Eq => a.eq(&b),
                    CmpOp::Ne => a.eq(&b).not(),
                    CmpOp::Lt => a.lt(&b),
                    CmpOp::Le => a.le(&b),
                    CmpOp::Gt => a.gt(&b),
                    CmpOp::Ge => a.ge(&b),
                };
            }
            let equal = match (lhs, rhs) {
                (Operand::Status, Operand::Lit(Literal::Str(state)))
                | (Operand::Lit(Literal::Str(state)), Operand::Status) => Some(
                    encode_state_membership(&sym.status, std::slice::from_ref(state), model),
                ),
                (Operand::Var(name), Operand::Lit(Literal::Bool(value)))
                | (Operand::Lit(Literal::Bool(value)), Operand::Var(name)) => sym
                    .bools
                    .get(name)
                    .map(|var| var.eq(Bool::from_bool(*value))),
                _ => None,
            };
            match (equal, op) {
                (Some(equal), CmpOp::Eq) => equal,
                (Some(equal), CmpOp::Ne) => equal.not(),
                _ => free(),
            }
        }
        Expr::In {
            value,
            set,
            negated,
        } => {
            let member = match (value, set) {
                (Operand::Status, Set::List(items)) => {
                    let states: Option<Vec<String>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Str(state) => Some(state.clone()),
                            _ => None,
                        })
                        .collect();
                    states.map(|states| encode_state_membership(&sym.status, &states, model))
                }
                (Operand::Lit(Literal::Str(value)), Set::Var(list))
                    if sym.lists.len_vars.contains_key(list) =>
                {
                    Some(encode_list_contains(list, value, sym.lists))
                }
                (value, Set::List(items)) => int_term(value, sym).and_then(|term| {
                    let options: Option<Vec<Bool>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Int(n) => Some(term.eq(Int::from_i64(*n))),
                            _ => None,
                        })
                        .collect();
                    options.map(|options| Bool::or(&options))
                }),
                _ => None,
            };
            match member {
                Some(member) if *negated => member.not(),
                Some(member) => member,
                None => free(),
            }
        }
    }
}

/// An integer-valued operand: a counter, a list length, or an integer.
fn int_term(operand: &Operand, sym: &Symbols<'_>) -> Option<Int> {
    match operand {
        Operand::Var(name) => sym.counters.get(name).cloned(),
        Operand::Len(name) => sym.lists.len_vars.get(name).cloned(),
        Operand::Lit(Literal::Int(n)) => Some(Int::from_i64(*n)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unreachable state detection (graph-based, no Z3 needed)
// ---------------------------------------------------------------------------

/// Check which states are unreachable from the initial state.
fn check_unreachable_states(model: &TemperModel) -> Vec<String> {
    let mut reachable: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = vec![&model.initial_status];

    while let Some(state) = queue.pop() {
        if !reachable.insert(state) {
            continue;
        }
        for t in &model.transitions {
            if reads_related_entities(&t.guard) {
                continue;
            }
            let can_fire_from =
                t.from_states.is_empty() || t.from_states.iter().any(|s| s == state);
            if can_fire_from
                && let Some(to) = &t.to_state
                && !reachable.contains(to.as_str())
            {
                queue.push(to);
            }
        }
    }

    model
        .states
        .iter()
        .filter(|s| !reachable.contains(s.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "smt_test.rs"]
mod tests;
