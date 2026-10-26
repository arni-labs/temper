//! Lemmas the prover guesses for an invariant the agent gave none.
//!
//! Most rules with no hints that do not prove alone need one of two kinds of
//! lemma: the rule at every state, not only where nothing is in flight; or,
//! in some state, a flag the rule reads. The prover adds every such guess,
//! and drops each one a move or the start breaks, until the rest hold
//! (Houdini). Dropping guesses only weakens what a move assumes, so a rule
//! that fails with some guesses held fails without them too.

use std::collections::BTreeSet;

use temper_spec::predicate::{Expr, Name};

use super::Lemma;
use super::encode::Enc;
use super::vocab::{Sort, Vocab};

#[derive(Debug, Clone)]
pub(super) enum Guess {
    /// The rule at every state.
    Everywhere,
    /// While `entity` is in `state`, its flag `var` is `holds`.
    Flag {
        entity: String,
        state: String,
        var: String,
        holds: bool,
    },
}

impl Guess {
    /// The guesses for `rule`, an invariant of `owner`.
    pub(super) fn all(v: &Vocab<'_>, owner: &str, rule: &Expr) -> Vec<Guess> {
        let mut flags = BTreeSet::new();
        rule.for_each_name(&mut |name| match name {
            Name::Var(var) => {
                flags.insert((owner.to_string(), var.to_string()));
            }
            Name::CrossField {
                entity_type, field, ..
            } => {
                flags.insert((entity_type.to_string(), field.to_string()));
            }
            _ => {}
        });
        let mut out = vec![Guess::Everywhere];
        for (entity, var) in flags {
            if v.var(&entity, &var) != Some(Sort::Bool) {
                continue;
            }
            for state in &v.automaton(&entity).automaton.states {
                for holds in [true, false] {
                    out.push(Guess::Flag {
                        entity: entity.clone(),
                        state: state.clone(),
                        var: var.clone(),
                        holds,
                    });
                }
            }
        }
        out
    }

    pub(super) fn name(&self, rule: &str) -> String {
        match self {
            Guess::Everywhere => format!("{rule} at every state"),
            Guess::Flag {
                entity,
                state,
                var,
                holds,
            } => {
                let not = if *holds { "" } else { "not " };
                format!("{entity} in {state}: {not}{var}")
            }
        }
    }

    /// The guess read in `frame`; `rule` is the invariant's own clause there.
    pub(super) fn read(&self, frame: &str, rule: &str) -> String {
        match self {
            Guess::Everywhere => rule.to_string(),
            Guess::Flag {
                entity,
                state,
                var,
                holds,
            } => {
                let flag = Enc::var(frame, entity, var);
                let flag = if *holds {
                    flag
                } else {
                    format!("(not {flag})")
                };
                format!(
                    "(=> (= {} {}) {flag})",
                    Enc::status(frame, entity),
                    Enc::state(entity, state)
                )
            }
        }
    }

    /// The guess as a lemma the agent can write beside `rule`, whose clause
    /// before a move is `holds`.
    pub(super) fn lemma(&self, rule: &str, holds: &str) -> Lemma {
        Lemma {
            rule: rule.to_string(),
            name: self.name(rule),
            assert: dollar_framed(&self.read("pre", holds)).replace('$', "pre.") /* MUTANT: not $-framed */,
        }
    }
}

/// `text` with each name framed `pre.` framed `$` instead, as lemmas are
/// written.
fn dollar_framed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("pre.") {
        out.push_str(&rest[..at]);
        let starts = out
            .chars()
            .next_back()
            .is_none_or(|c| c == '(' || c.is_whitespace());
        out.push_str(if starts { "$" } else { "pre." });
        rest = &rest[at + 4..];
    }
    out.push_str(rest);
    out
}
