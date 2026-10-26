//! One obligation for every way a record takes one action.
//!
//! A record takes an action sent from outside, on a timer, as a reaction,
//! as a spawn's first move, as a dispatch arriving, or as a call's answer
//! or failure: one move each, and a call whose trigger names no answer
//! adds one for every action of its record. Those moves differ only in the
//! parameters they pass, what else must hold for them to happen, and which
//! dispatch or call in flight they end. The general move takes the action
//! with any parameters, wherever its state, guard and constraints allow,
//! ending any one of those or none. Each of the moves is one of its cases,
//! so when it keeps the clauses they all do. When it does not, the proof
//! checks the moves themselves.

use std::collections::BTreeMap;

use temper_spec::automaton::Action;

use super::moves::{Move, Moves, Param, Step};
use super::run::Ask;
use super::vocab::Sort;

/// One obligation the proof checks first: a move as it is, or a general
/// move standing for several.
pub(super) struct Group {
    pub(super) general: Option<Move>,
    /// The moves it stands for, by their place.
    pub(super) members: Vec<usize>,
}

/// Steps that take an action, and so can share a general move.
fn takes_action(step: Step) -> bool {
    matches!(
        step,
        Step::Outside | Step::Timer | Step::Outer | Step::Arrive | Step::Answer | Step::Fail
    )
}

impl Moves<'_, '_, '_> {
    /// `moves`, with those that take the same action on the same record
    /// grouped under one general move, in the order each first appears.
    pub(super) fn groups(&self, moves: &[Move]) -> Vec<Group> {
        let v = self.enc.v;
        let mut groups: Vec<(Group, Option<&Action>)> = Vec::new();
        let mut at: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for (i, mv) in moves.iter().enumerate() {
            // A move that takes no action of its record, such as a call
            // failing with none to fire, stands alone.
            let action = (takes_action(mv.step))
                .then(|| v.action(&mv.entity, &mv.action))
                .flatten();
            let key = (mv.entity.as_str(), mv.action.as_str());
            match action.and(at.get(&key)) {
                Some(&g) => groups[g].0.members.push(i),
                None => {
                    if action.is_some() {
                        at.insert(key, groups.len());
                    }
                    let members = vec![i];
                    groups.push((
                        Group {
                            general: None,
                            members,
                        },
                        action,
                    ));
                }
            }
        }
        let general = |(mut group, action): (Group, Option<&Action>)| {
            if let (Some(action), [first, _, ..]) = (action, group.members.as_slice()) {
                let first = &moves[*first];
                let ends: Vec<Option<&str>> = (group.members.iter())
                    .map(|&i| moves[i].witness.as_deref())
                    .collect();
                group.general = Some(self.general(&first.entity, action, &ends));
            }
            group
        };
        groups.into_iter().map(general).collect()
    }

    /// `entity` takes `action` with any parameters, ending a dispatch or
    /// call of one of the kinds `ends` names (`None`: ending none).
    fn general(&self, entity: &str, action: &Action, ends: &[Option<&str>]) -> Move {
        let v = self.enc.v;
        let mut kinds: Vec<&str> = ends.iter().flatten().copied().collect();
        kinds.sort_unstable();
        kinds.dedup();
        let mut guard = Vec::new();
        let mut params = BTreeMap::new();
        for param in &action.params {
            let name = param.name();
            let sort = v.param(action, name).unwrap_or(Sort::Str);
            let term = self.enc.fresh(&format!("p.{name}"), sort.smt());
            let passed = self.enc.fresh(&format!("passed.{name}"), "Bool");
            // A count its sender chooses is never negative; one a dispatch
            // carries was read from its sender's counter.
            let carried = kinds.iter().any(|kind| {
                v.kinds
                    .iter()
                    .any(|k| k.name == *kind && k.fields.iter().any(|(f, _)| f == name))
            });
            if let (Sort::Int { counter: true }, false) = (sort, carried) {
                guard.push(format!("(>= {term} 0)"));
            }
            params.insert(name.to_string(), Param { term, passed });
        }
        guard.extend(self.enabled(entity, action, &params));
        let mut changed = BTreeMap::new();
        if !kinds.is_empty() {
            let pick = self.enc.fresh("ends", "Int");
            let lowest = if ends.contains(&None) { 0 } else { 1 };
            guard.push(format!("(<= {lowest} {pick} {})", kinds.len()));
            for (j, kind) in (1..).zip(&kinds) {
                let w = self.enc.fresh(&format!("w.{kind}"), &format!("Msg/{kind}"));
                let (pend, n) = (format!("pre.pend.{kind}"), format!("pre.n.{kind}"));
                guard.push(format!("(=> (= {pick} {j}) (> (select {pend} {w}) 0))"));
                changed.insert(
                    format!("post.pend.{kind}"),
                    format!(
                        "(ite (= {pick} {j}) (store {pend} {w} (- (select {pend} {w}) 1)) {pend})"
                    ),
                );
                changed.insert(
                    format!("post.n.{kind}"),
                    format!("(ite (= {pick} {j}) (- {n} 1) {n})"),
                );
            }
        }
        self.commit(entity, action, &params, &mut changed);
        self.sends(entity, action, &params, &mut changed);
        let defs = self.enc.frame_rest("pre", "post", &changed);
        let label = format!("{entity}.{} (every way it is taken)", action.name);
        self.made(
            label,
            guard,
            defs,
            None,
            Step::Outside,
            (entity, &action.name),
        )
    }
}

/// The asks of `groups`, in order, from `ask`, which writes a move's: a
/// general move's is followed by those of the moves it stands for, which
/// are checked only when it does not hold.
pub(super) fn lay_out(
    groups: &[Group],
    moves: &[Move],
    ask: &impl Fn(&Move) -> Ask,
    out: &mut Vec<Ask>,
) {
    for group in groups {
        let members = group.members.iter().map(|&m| ask(&moves[m]));
        match &group.general {
            Some(general) => {
                let first = out.len() + 1;
                let members: Vec<Ask> = members.collect();
                let at = (first..first + members.len()).collect();
                out.push(Ask {
                    members: at,
                    ..ask(general)
                });
                out.extend(members);
            }
            None => out.extend(members),
        }
    }
}

#[cfg(test)]
#[path = "general_test.rs"]
mod tests;
