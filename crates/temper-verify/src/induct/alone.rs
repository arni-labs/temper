//! A type's invariants proved on their own, as the model check of one type
//! reads them ([`crate::checker`]): any action but an output may come at any
//! time its status and guard allow, with any parameters, and what another
//! record holds is unknown. A rule proved here holds at every state that
//! check can reach, so the check needs no walk for it.

use std::time::Instant;

use temper_spec::automaton::{ActionKind, Automaton};
use temper_spec::predicate::Effect;

use super::{Hints, RuleProof, prove_some};

/// Prove each invariant of `automaton` that reads no other record, with the
/// lemmas written beside it. Solvers still checking at `deadline` stop, and
/// their rules are not proved.
pub fn prove_alone(automaton: &Automaton, deadline: Option<Instant>) -> Vec<RuleProof> {
    let alone = alone(automaton);
    let own = |_: &str, rule: &str| {
        alone
            .invariants
            .iter()
            .any(|i| i.name == rule && i.assert.related_types().is_empty())
    };
    prove_some(&[&alone], None, &Hints::default(), own, deadline)
}

/// `automaton` as the one-type check runs it: each action but an output
/// may be sent at any time, and nothing it sets off (a call, a reaction, a
/// timer, a spawn) waits to happen, since the check follows none of them.
fn alone(automaton: &Automaton) -> Automaton {
    let mut alone = automaton.clone();
    alone.actions.retain(|a| a.kind != ActionKind::Output);
    for action in &mut alone.actions {
        action.kind = ActionKind::Input;
        action.triggers.clear();
        action.effect.retain(|effect| {
            !matches!(
                effect,
                Effect::Schedule { .. } | Effect::ScheduleAt { .. } | Effect::Spawn { .. }
            )
        });
    }
    alone.state_timeouts.clear();
    alone.liveness.clear();
    alone
}

#[cfg(test)]
#[path = "alone_test.rs"]
mod tests;
