//! Liveness by a measure and helpful moves.
//!
//! A rule `eventually = goal` settles when, from every reachable state, the
//! system's own moves (dispatches arriving, timers, module answers and
//! failures, and the actions the rule relies on while `goal` is false) can
//! reach a state with nothing in flight where `goal` holds: the joint walk's
//! reading. Its proof gives a measure, a list of integer terms compared in
//! order, and regions that cover every state where the lemmas hold and the
//! target does not. Each region names a helpful move. From any state in the
//! region the move can happen, and every way it can go reaches the target
//! or a state where the measure is smaller. The lemmas are proved inductive
//! alongside, and the measure is never negative where they hold, so every
//! reachable state reaches the target.
//!
//! A region may name a witness `w`, a dispatch or module call of one kind in
//! flight: the region holds when some such one satisfies it, and its move
//! takes that one. A helpful move's parameters are the ones its region pins
//! (the system chooses them, as the walk lets a module answer or a relied-on
//! sender pass what its record takes); the others may be anything, and the
//! move must work whatever they are.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::May;
use temper_spec::predicate::Expr;

use super::encode::{Enc, counted};
use super::moves::{Move, Moves, Step};
use super::solver::Z3;
use super::vocab::Vocab;
use super::{Obligation, check};

/// The measure of an `eventually` rule: terms over `$`-framed names,
/// compared in order.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Measure {
    pub rule: String,
    pub terms: Vec<String>,
}

/// One region of a liveness proof.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Region {
    /// The rule it is for.
    pub rule: String,
    pub name: String,
    /// SMT-LIB over `$`-framed names, and `w` when it names a witness.
    pub region: String,
    /// The kind of dispatch or call `w` is, when the region names one.
    #[serde(default)]
    pub witness: Option<String>,
    /// The helpful move, by its label.
    pub step: String,
    /// The helpful move's parameters, as SMT-LIB over `$`-framed names and
    /// `w`; `passed.<p>` says whether a parameter is passed.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl Region {
    /// The region read in `frame`: with a witness, that some dispatch or
    /// call in flight satisfies it.
    fn read(&self, frame: &str) -> String {
        let body = self.read_at_w(frame);
        match &self.witness {
            Some(kind) => format!("(exists ((w Msg/{kind})) {body})"),
            None => body,
        }
    }

    /// The region read in `frame` about the move's own witness `w`, which
    /// is in flight.
    fn read_at_w(&self, frame: &str) -> String {
        let body = self.region.replace('$', &format!("{frame}."));
        match &self.witness {
            Some(kind) => format!("(and (> (select {frame}.pend.{kind} w) 0) {body})"),
            None => body,
        }
    }
}

/// An `eventually` rule and what its proof was given.
pub(super) struct Live<'r> {
    pub(super) owner: &'r str,
    pub(super) goal: &'r Expr,
    pub(super) relies_on: &'r [String],
    pub(super) measure: Option<&'r Measure>,
    pub(super) regions: Vec<&'r Region>,
}

/// The liveness obligations of `live`, after its lemmas `clauses` are
/// proved inductive: the regions cover every state short of the target, the
/// measure is not negative there, and each region's move can happen and
/// reaches the target or lowers the measure, whichever way it goes.
pub(super) fn obligations(
    z3: &mut Z3,
    enc: &Enc<'_, '_>,
    moves: &[Move],
    live: &Live<'_>,
    clauses: &dyn Fn(&str) -> Vec<(String, String)>,
    out: &mut Vec<Obligation>,
    errors: &mut Vec<String>,
) -> std::io::Result<()> {
    let v = enc.v;
    let goal = |frame: &str| enc.expr(live.goal, live.owner, frame);
    let target = |frame: &str| format!("(and {frame}.quiet {})", goal(frame));
    let Some(measure) = live.measure else {
        errors.push("no measure given".to_string());
        return Ok(());
    };
    let terms = |frame: &str| -> Vec<String> {
        measure
            .terms
            .iter()
            .map(|t| t.replace('$', &format!("{frame}.")))
            .collect()
    };
    // What every liveness obligation starts from: the lemmas hold and the
    // target does not.
    let short = |extra: &[String], decls: &[String]| -> String {
        let not_there = format!("(not {})", target("pre"));
        let mut text = format!("(push 1)\n{}", counted(v));
        let mut seen = BTreeSet::new();
        for d in enc.take_decls().iter().chain(decls) {
            if seen.insert(d.clone()) {
                text.push_str(&format!("{d}\n"));
            }
        }
        for (_, c) in clauses("pre") {
            text.push_str(&format!("(assert {c})\n"));
        }
        text.push_str(&format!("(assert {not_there})\n"));
        for fact in extra {
            text.push_str(&format!("(assert {fact})\n"));
        }
        text
    };
    let before = clauses("pre");
    let any: Vec<String> = live.regions.iter().map(|r| r.read("pre")).collect();
    let covered = vec![(
        "some region".to_string(),
        format!("(or false {})", any.join(" ")),
    )];
    let text = short(&[], &[]);
    out.push(check(z3, v, "covers", text, &covered, &before)?);
    let floor: Vec<String> = terms("pre").iter().map(|t| format!("(>= {t} 0)")).collect();
    let bounded = vec![(
        "measure not negative".to_string(),
        format!("(and true {})", floor.join(" ")),
    )];
    let text = short(&[], &[]);
    out.push(check(z3, v, "measure bounded", text, &bounded, &before)?);
    let lower = lexically_less(&terms("post"), &terms("pre"));
    for region in &live.regions {
        // A region that pins no parameter takes its move as the induction
        // built it.
        let pinned;
        let moves = if region.params.is_empty() {
            moves
        } else {
            let pins = region
                .params
                .iter()
                .map(|(name, term)| (name.clone(), term.replace('$', "pre.")))
                .collect();
            pinned = Moves::pinned(enc, pins).all();
            &pinned
        };
        let outcomes = match helpful(v, live, region, moves) {
            Ok(outcomes) => outcomes,
            Err(error) => {
                errors.push(format!("region {}: {error}", region.name));
                continue;
            }
        };
        // A lone move whose guard reads what the proof does not hold (a
        // list, a record found through a field) would let the solver say
        // it can happen by choosing that value; one with a refusal beside
        // it goes one way or the other whatever the value is.
        let unknown = |text: &String| text.split([' ', '(', ')']).any(|t| t.starts_with("h!"));
        if outcomes.len() == 1 && outcomes[0].guard.iter().any(unknown) {
            errors.push(format!(
                "region {}: whether `{}` can happen reads what the proof does not hold",
                region.name, region.step
            ));
            continue;
        }
        let decls: Vec<String> = outcomes.iter().flat_map(|m| m.decls.clone()).collect();
        let at_w = region.read_at_w("pre");
        let can: Vec<String> = outcomes
            .iter()
            .map(|m| format!("(and true {})", m.guard.join(" ")))
            .collect();
        let mut happens = vec![("can happen".to_string(), format!("(or {})", can.join(" ")))];
        if outcomes[0].step == Step::Outside {
            happens.push((
                "relied on while false".to_string(),
                format!("(not {})", goal("pre")),
            ));
        }
        let text = short(std::slice::from_ref(&at_w), &decls);
        let name = format!("region {}: {} can happen", region.name, region.step);
        out.push(check(z3, v, &name, text, &happens, &before)?);
        for outcome in &outcomes {
            let reaches = vec![(
                "target or lower measure".to_string(),
                format!("(or {} {lower})", target("post")),
            )];
            let mut extra = vec![at_w.clone()];
            extra.extend(outcome.body().cloned());
            let text = short(&extra, &decls);
            let name = format!("region {}: {}", region.name, outcome.label);
            out.push(check(z3, v, &name, text, &reaches, &before)?);
        }
    }
    Ok(())
}

/// The ways `region`'s helpful move can go: a dispatch taken or refused, a
/// call's failure fired or refused, or the one move.
fn helpful<'m>(
    v: &Vocab<'_>,
    live: &Live<'_>,
    region: &Region,
    moves: &'m [Move],
) -> Result<Vec<&'m Move>, String> {
    let first = moves
        .iter()
        .find(|m| m.label == region.step)
        .ok_or_else(|| format!("no move is labelled `{}`", region.step))?;
    if first.witness != region.witness {
        return Err(format!(
            "its move takes {:?} but the region names {:?}",
            first.witness, region.witness
        ));
    }
    let relied = first.step == Step::Outside && live.relies_on.contains(&first.action);
    if !first.step.by_system() && !relied {
        return Err(format!("`{}` is not a move the system makes", first.label));
    }
    // A relied-on action is sent by someone Cedar lets send it.
    let may = match (first.step, &first.timer) {
        (Step::Outside, _) => May::Yes,
        (Step::Timer, Some(state)) => v.timer_may(&first.entity, state, &first.action),
        (Step::Answer | Step::Fail, _) if !first.action.is_empty() => {
            v.runtime_may(&first.entity, &first.action)
        }
        _ => May::Yes,
    };
    if may != May::Yes {
        return Err(format!("Cedar may refuse `{}` to its sender", first.label));
    }
    let sibling = match first.step {
        Step::Arrive => Some(Step::Refuse),
        Step::Fail => Some(Step::FailRefused),
        _ => None,
    };
    let mut out = vec![first];
    out.extend(
        moves
            .iter()
            .filter(|m| Some(m.step) == sibling && m.witness == first.witness),
    );
    Ok(out)
}

/// `after` is lexically less than `before`.
fn lexically_less(after: &[String], before: &[String]) -> String {
    match (after.split_first(), before.split_first()) {
        (Some((a, rest_a)), Some((b, rest_b))) => format!(
            "(or (< {a} {b}) (and (= {a} {b}) {}))",
            lexically_less(rest_a, rest_b)
        ),
        _ => "false".to_string(),
    }
}
