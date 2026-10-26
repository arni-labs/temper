//! Core types for the Temper verification model.
//!
//! Contains the state, action, transition, invariant, liveness, and model struct
//! definitions used by the Stateright model checker.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use std::collections::BTreeSet;

use temper_spec::predicate::{Arg, Expr, ParamKind};

/// The state tracked by the Temper model during verification.
///
/// Multi-variable state: status + named counters + named booleans.
/// This generalises the old `(status, item_count)` to handle arbitrary
/// IOA `[[state]]` variable declarations.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelState {
    /// Current status value (mirrors the specification's `status` variable).
    pub status: String,
    /// Named counter variables (e.g. `items`, `quantity`).
    pub counters: BTreeMap<String, usize>,
    /// Named boolean variables (e.g. `has_address`, `payment_captured`).
    pub booleans: BTreeMap<String, bool>,
    /// Named list variables (e.g. `tags`, `labels`).
    pub lists: BTreeMap<String, Vec<String>>,
    /// The string variables an invariant reads, as symbols: `''`, a literal
    /// the spec compares the variable with, or `#n`, a value that is equal
    /// only to itself. The verifier tracks which strings are equal, not what
    /// they say, so `#n` are renumbered in order of first use after every
    /// step ([`super::semantics::canonical_strings`]). Empty for a spec whose
    /// invariants read no string.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub strings: BTreeMap<String, String>,
}

impl fmt::Display for TemperModelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.status)?;
        if !self.counters.is_empty() {
            let pairs: Vec<String> = self
                .counters
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            write!(f, "({})", pairs.join(","))?;
        }
        if !self.booleans.is_empty() {
            let pairs: Vec<String> = self
                .booleans
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.clone())
                .collect();
            if !pairs.is_empty() {
                write!(f, "[{}]", pairs.join(","))?;
            }
        }
        if !self.lists.is_empty() {
            let pairs: Vec<String> = self
                .lists
                .iter()
                .map(|(k, v)| format!("{k}#{}", v.len()))
                .collect();
            write!(f, "{{{}}}", pairs.join(","))?;
        }
        if !self.strings.is_empty() {
            let pairs: Vec<String> = self
                .strings
                .iter()
                .map(|(k, v)| format!("{k}='{v}'"))
                .collect();
            write!(f, "<{}>", pairs.join(","))?;
        }
        Ok(())
    }
}

/// The value the model chose for an action parameter in one step.
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ParamValue {
    /// A non-negative integer.
    Count(usize),
    /// A boolean.
    Bool(bool),
    /// A string some guard or invariant compares a list against.
    Str(String),
    /// A string no guard or invariant mentions.
    Fresh,
}

impl fmt::Display for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamValue::Count(n) => write!(f, "{n}"),
            ParamValue::Bool(b) => write!(f, "{b}"),
            ParamValue::Str(s) => write!(f, "'{s}'"),
            ParamValue::Fresh => f.write_str("<fresh>"),
        }
    }
}

/// An action that the model can take, corresponding to a specification transition.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelAction {
    /// The transition name (e.g. "SubmitOrder", "CancelOrder").
    pub name: String,
    /// The target status after taking this action (if deterministic).
    pub target_state: Option<String>,
    /// The values chosen for the parameters the transition's effects read.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.params.is_empty() {
            let pairs: Vec<String> = self
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            write!(f, "({})", pairs.join(", "))?;
        }
        if let Some(target) = &self.target_state {
            write!(f, " -> {target}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Guards and effects — self-contained in temper-verify (mirror JIT types)
// ---------------------------------------------------------------------------

/// A state effect applied when a transition fires. `params.p` values are
/// chosen per step (see [`TemperModelAction::params`]).
#[derive(Clone, Debug)]
pub enum ModelEffect {
    /// `var = value` on a counter.
    SetCounter { var: String, value: Arg },
    /// `var += value` on a counter.
    AddCounter { var: String, value: Arg },
    /// `var -= value` on a counter (saturating).
    SubCounter { var: String, value: Arg },
    /// `var = value` on a bool.
    SetBool { var: String, value: Arg },
    /// `append(var, value)`.
    ListAppend { var: String, value: Arg },
    /// `remove_at(var, index)`; out of range is a no-op.
    ListRemoveAt { var: String, index: Arg },
}

/// A resolved transition used internally by the model, pre-computed from a
/// specification action for efficient matching during state exploration.
#[derive(Clone, Debug)]
pub struct ResolvedTransition {
    /// The action name.
    pub name: String,
    /// States from which this transition can fire.
    pub from_states: Vec<String>,
    /// The target state (if deterministic).
    pub to_state: Option<String>,
    /// Precondition beyond `from_states`.
    pub guard: Expr,
    /// Effects applied when the transition fires.
    pub effects: Vec<ModelEffect>,
    /// The action parameters the effects read, by kind.
    pub params: BTreeMap<String, ParamKind>,
    /// The action parameters that write a tracked string variable of the
    /// same name (the runtime writes every parameter into its field).
    pub string_params: Vec<StringParam>,
}

/// An action parameter that writes the tracked string variable of its name,
/// with what the action's constraints allow it to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringParam {
    /// The parameter, and the variable it writes.
    pub name: String,
    /// `param_nonempty`: never `''`.
    pub nonempty: bool,
    /// `param_equals_field`: the value of this variable.
    pub equals: Option<String>,
    /// `param_not_equals_field`: never the value of these variables.
    pub not_equals: Vec<String>,
}

/// A safety invariant: `assert` must not be false in any reachable state.
#[derive(Clone, Debug)]
pub struct ResolvedInvariant {
    /// The invariant name.
    pub name: String,
    /// The assertion. Values the model does not track read as unknown, and
    /// an unknown result is not a violation.
    pub assert: Expr,
}

// ---------------------------------------------------------------------------
// Liveness properties
// ---------------------------------------------------------------------------

/// The kind of liveness property.
#[derive(Clone, Debug)]
pub enum LivenessKind {
    /// From any `from` state, eventually reaches one of `targets`.
    /// Checked via Stateright's `Property::eventually` (acyclic paths only).
    ReachesState {
        from: Vec<String>,
        targets: Vec<String>,
    },
    /// From any `from` state, there is always at least one enabled action.
    /// This is actually a safety property (always has actions).
    NoDeadlock { from: Vec<String> },
}

/// A resolved liveness property.
#[derive(Clone, Debug)]
pub struct ResolvedLiveness {
    /// The property name.
    pub name: String,
    /// The kind of liveness check.
    pub kind: LivenessKind,
}

/// The Stateright model generated from an I/O Automaton specification.
///
/// This struct holds all the pre-computed transition, invariant, and liveness
/// data needed to implement the `Model` trait efficiently. Invariant data is
/// stored here (rather than captured in closures) because Stateright's
/// `Property::always` requires a bare `fn` pointer.
#[derive(Clone, Debug)]
pub struct TemperModel {
    /// All valid status values from the specification.
    pub states: Vec<String>,
    /// Pre-resolved transitions.
    pub transitions: Vec<ResolvedTransition>,
    /// Pre-resolved safety invariants (accessible to property fn pointers via &self).
    pub invariants: Vec<ResolvedInvariant>,
    /// Pre-resolved liveness properties.
    pub liveness: Vec<ResolvedLiveness>,
    /// States from which no transition may be enabled.
    pub terminal: Vec<String>,
    /// Declared state variable types, for evaluating guards and invariants.
    pub var_kinds: BTreeMap<String, temper_spec::predicate::VarKind>,
    /// The initial status (first state from Init, typically "Draft").
    pub(crate) initial_status: String,
    /// Initial counter values from [[state]] declarations.
    pub(crate) initial_counters: BTreeMap<String, usize>,
    /// Initial boolean values from [[state]] declarations.
    pub(crate) initial_booleans: BTreeMap<String, bool>,
    /// Initial list values from [[state]] declarations.
    pub(crate) initial_lists: BTreeMap<String, Vec<String>>,
    /// Initial values of the tracked string variables.
    pub(crate) initial_strings: BTreeMap<String, String>,
    /// The string variables the verifier tracks: those an invariant reads,
    /// and those whose value reaches one through a constraint or a reaction.
    pub tracked_strings: BTreeSet<String>,
    /// String literals each tracked variable is compared with in a guard or
    /// invariant: the values worth choosing for it besides `''` and others'.
    pub(crate) string_literals: BTreeMap<String, BTreeSet<String>>,
    /// Per-counter upper bounds for bounded exploration.
    pub(crate) counter_bounds: BTreeMap<String, usize>,
    /// Default upper bound for counters not in counter_bounds.
    pub(crate) default_max_counter: usize,
    /// String literals each list is compared against (`'x' in list`) in any
    /// guard or invariant: the values worth choosing for `append(list, params.p)`.
    pub(crate) list_literals: BTreeMap<String, BTreeSet<String>>,
    /// How far this model may be explored, when a budgeted cascade built it.
    /// `None` explores to the end.
    pub(crate) limit: Option<ExploreLimit>,
}

/// How far one model may be explored: a [`crate::Budget`] in force.
#[derive(Clone, Debug)]
pub(crate) struct ExploreLimit {
    /// Past this instant the model offers no actions, so every walk of it
    /// (model check, simulation, property tests) ends at once.
    pub(crate) until: Instant,
    /// States the model check may generate before it stops (a state reached
    /// twice counts twice).
    pub(crate) states: usize,
    /// Set once a walk is over, so the model offers its actions again:
    /// Stateright rebuilds a found path by stepping the model from the start.
    pub(crate) lifted: Arc<AtomicBool>,
}

impl ExploreLimit {
    /// The same limit for one walk of its own, lifted without lifting it for
    /// the other walks of the same budget.
    pub(crate) fn for_one_walk(&self) -> Self {
        Self {
            lifted: Arc::new(AtomicBool::new(false)),
            ..self.clone()
        }
    }

    pub(crate) fn lift(&self) {
        self.lifted.store(true, Ordering::Relaxed);
    }
}

impl TemperModel {
    /// Whether the time this model may be explored for is spent.
    pub fn out_of_time(&self) -> bool {
        self.limit.as_ref().is_some_and(|limit| {
            !limit.lifted.load(Ordering::Relaxed) && Instant::now() >= limit.until
        })
    }

    /// How many transitions each status is from one `target` picks, by the
    /// statuses its transitions name. A status left out leads to none.
    pub(crate) fn distances(&self, target: impl Fn(&str) -> bool) -> HashMap<&str, usize> {
        let mut near: HashMap<&str, usize> = self
            .states
            .iter()
            .filter(|status| target(status))
            .map(|status| (status.as_str(), 0))
            .collect();
        let mut queue: VecDeque<&str> = near.keys().copied().collect();
        while let Some(status) = queue.pop_front() {
            let further = near[status] + 1;
            let into = self
                .transitions
                .iter()
                .filter(|t| t.to_state.as_deref() == Some(status));
            for transition in into {
                // A transition that names no status starts anywhere.
                let from = match transition.from_states.is_empty() {
                    true => &self.states,
                    false => &transition.from_states,
                };
                for from in from {
                    if !near.contains_key(from.as_str()) {
                        near.insert(from, further);
                        queue.push_back(from);
                    }
                }
            }
        }
        near
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_display_status_only() {
        let s = TemperModelState {
            status: "Draft".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Draft");
    }

    #[test]
    fn state_display_with_counters() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::from([("items".into(), 3)]),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active(items=3)");
    }

    #[test]
    fn state_display_with_true_booleans() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::from([("ready".into(), true), ("done".into(), false)]),
            lists: BTreeMap::new(),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active[ready]");
    }

    #[test]
    fn state_display_with_lists() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::from([("tags".into(), vec!["a".into(), "b".into()])]),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active{tags#2}");
    }

    #[test]
    fn state_display_full() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::from([("items".into(), 2)]),
            booleans: BTreeMap::from([("ready".into(), true)]),
            lists: BTreeMap::from([("tags".into(), vec!["x".into()])]),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active(items=2)[ready]{tags#1}");
    }

    #[test]
    fn state_display_all_booleans_false_hides_brackets() {
        let s = TemperModelState {
            status: "X".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::from([("a".into(), false), ("b".into(), false)]),
            lists: BTreeMap::new(),
            strings: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "X");
    }

    #[test]
    fn action_display_with_target() {
        let a = TemperModelAction {
            name: "Submit".into(),
            target_state: Some("Active".into()),
            params: BTreeMap::new(),
        };
        assert_eq!(a.to_string(), "Submit -> Active");
        let a = TemperModelAction {
            params: BTreeMap::from([("n".into(), ParamValue::Count(2))]),
            ..a
        };
        assert_eq!(a.to_string(), "Submit(n=2) -> Active");
    }

    #[test]
    fn action_display_no_target() {
        let a = TemperModelAction {
            name: "AddItem".into(),
            target_state: None,
            params: BTreeMap::new(),
        };
        assert_eq!(a.to_string(), "AddItem");
    }

    #[test]
    fn state_serde_roundtrip() {
        let s = TemperModelState {
            status: "Draft".into(),
            counters: BTreeMap::from([("items".into(), 5)]),
            booleans: BTreeMap::from([("ready".into(), true)]),
            lists: BTreeMap::from([("tags".into(), vec!["vip".into()])]),
            strings: BTreeMap::new(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TemperModelState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn action_serde_roundtrip() {
        let a = TemperModelAction {
            name: "Submit".into(),
            target_state: Some("Active".into()),
            params: BTreeMap::from([("tag".into(), ParamValue::Fresh)]),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: TemperModelAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
