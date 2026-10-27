//! Every rule, over one record or several, is proved by induction first: with the
//! lemmas its spec gives, or with the prover's guesses, which the verdict
//! lists. A rule whose proof does not hold is walked as before and says why;
//! a proved rule whose premise no run meets proves nothing, found by the
//! solver when the proof rules the premise out and by a walk otherwise.

use std::time::Duration;

use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// The parent's last move: a sealing parent closes.
const CLOSE: &str = r#"
[[action]]
name = "Close"
kind = "input"
from = ["Sealing"]
to = "Closed"
"#;

/// A parent locked when it starts sealing, and closed after. `seal_effect`
/// is added to `Seal`'s effects.
fn parent(seal_effect: &str) -> String {
    format!(
        r#"
[automaton]
name = "Parent"
states = ["Open", "Sealing", "Closed"]
initial = "Open"

[[state]]
name = "locked"
type = "bool"
initial = false

[[action]]
name = "Seal"
kind = "input"
from = ["Open"]
to = "Sealing"
effect = ["locked = true"{seal_effect}]
{CLOSE}"#
    )
}

/// A child whose rule reads its parent: a closed parent is locked. It holds
/// alone in no state the walk reaches, but induction needs to know a sealing
/// parent is locked too. Each of `lemmas` is written beside the rule.
fn child(lemmas: &[&str]) -> String {
    let mut out = String::from(
        r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"

[[action]]
name = "Finish"
kind = "input"
from = ["Waiting"]
to = "Done"

[[invariant]]
name = "closed_parent_is_locked"
assert = "Parent[Id].status == 'Closed' => Parent[Id].locked == true"
"#,
    );
    for (i, lemma) in lemmas.iter().enumerate() {
        out.push_str(&format!(
            "\n[[invariant.lemma]]\nname = \"lemma{i}\"\nassert = \"{lemma}\"\n"
        ));
    }
    out
}

/// Every status past Open is locked: what the proof needs.
const LOCKED_PAST_OPEN: &str = "(=> (not (= $Parent.status Parent/Open)) $Parent.locked)";

/// True, but not enough: a sealing parent may be unlocked for all it says.
const LOCKED_WHEN_CLOSED: &str = "(=> (= $Parent.status Parent/Closed) $Parent.locked)";

/// A tally counted once before it is ready, and a watcher whose rule reads
/// it: a done tally was counted. It holds, but no guess about a flag says a
/// ready tally was counted, so induction does not prove it alone.
const TALLY: &str = r#"
[automaton]
name = "Tally"
states = ["Counting", "Ready", "Done"]
initial = "Counting"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Count"
kind = "input"
from = ["Counting"]
to = "Ready"
effect = ["n += 1"]

[[action]]
name = "Finish"
kind = "input"
from = ["Ready"]
to = "Done"
"#;

const WATCHER: &str = r#"
[automaton]
name = "Watcher"
states = ["Idle"]
initial = "Idle"

[[action]]
name = "Poke"
kind = "input"
from = ["Idle"]
to = "Idle"

[[invariant]]
name = "done_tally_counted"
assert = "Tally[Id].status == 'Done' => Tally[Id].n > 0"
"#;

/// A record that tells itself to finish when it starts, so an asked record
/// finishes. With `hints`, its rule carries a measure, a lemma and regions.
fn asker(hints: bool) -> String {
    let hints = if hints {
        r#"
measure = ["$n.Asker.Finish"]

[[liveness.lemma]]
name = "finish_in_flight"
assert = "(=> (= $Asker.status Asker/Asked) (exists ((m Msg/Asker.Finish)) ($has.Asker.Finish m)))"

[[liveness.region]]
name = "finish_lands"
region = "(= $Asker.status Asker/Asked)"
witness = "Asker.Finish"
step = "Asker.Finish arrives"

[[liveness.region]]
name = "late_finish_refused"
region = "(and (not (= $Asker.status Asker/Asked)) (exists ((m Msg/Asker.Finish)) ($has.Asker.Finish m)))"
witness = "Asker.Finish"
step = "Asker.Finish refused"
"#
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Asker"
states = ["Idle", "Asked", "Done"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Asker"
target_action = "Finish"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Finish"
kind = "internal"
from = ["Asked"]
to = "Done"

[[liveness]]
name = "asked_finishes"
eventually = "status == 'Asked' => status == 'Done'"
{hints}"#
    )
}

/// How long each rule's proof may run.
const TIME: Duration = Duration::from_secs(30);

fn check(sources: &[&str], time: Duration) -> JointCheck {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    check_joint(&refs, None, super::super::room::test_bound(), time)
}

/// The one invariant's verdict.
fn invariant(sources: &[&str], time: Duration) -> JointVerdict {
    let mut invariants = check(sources, time).invariants;
    assert_eq!(invariants.len(), 1, "{invariants:?}");
    invariants.remove(0)
}

/// A single-state job for name-collision regressions.
fn job(rule: &str, safe: bool, lemma: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"

[[state]]
name = "safe"
type = "bool"
initial = {safe}

[[action]]
name = "Wait"
kind = "input"
from = ["Idle"]
to = "Idle"

[[invariant]]
name = "{rule}"
assert = "safe == true"
{lemma}"#
    )
}

#[test]
fn a_rule_named_like_a_guess_cannot_be_omitted_or_dropped() {
    for rule in ["is_safe", "Job in Idle: safe", "Job in Idle: not safe"] {
        let verdict = invariant(&[&job(rule, false, "")], TIME);
        let JointVerdict::Invariant(walked, Some(proof)) = &verdict else {
            panic!("an initially false invariant must be walked: {verdict}");
        };
        assert!(!walked.holds(), "{verdict}");
        assert!(!proof.proved(), "{verdict}");
        assert_eq!(verdict.tag(), "FAIL");
    }
}

#[test]
fn a_broken_lemma_named_like_its_rule_is_dropped_and_named() {
    let lemma = r#"
[[invariant.lemma]]
name = "is_safe"
assert = "false"
"#;
    let verdict = invariant(&[&job("is_safe", true, lemma)], TIME);
    let JointVerdict::Proved(proof, _) = &verdict else {
        panic!("proved without the same-named broken lemma: {verdict}");
    };
    assert_eq!(
        proof.dropped.get("is_safe").map(String::as_str),
        Some("fails after start"),
        "{verdict}"
    );
    assert!(
        verdict
            .to_string()
            .contains("dropped the written lemma is_safe, which fails after start"),
        "{verdict}"
    );
}

#[test]
fn a_refused_lemma_named_like_its_rule_is_dropped_and_named() {
    let lemma = r#"
[[invariant.lemma]]
name = "is_safe"
assert = "(=> $Job.nope true)"
"#;
    let verdict = invariant(&[&job("is_safe", true, lemma)], TIME);
    let JointVerdict::Proved(proof, _) = &verdict else {
        panic!("proved without the same-named refused lemma: {verdict}");
    };
    let why = proof.dropped.get("is_safe").expect("is_safe dropped");
    assert!(why.starts_with("the solver refuses: "), "{why}");
    assert!(why.contains("Job.nope"), "{why}");
    assert!(
        verdict
            .to_string()
            .contains("dropped the written lemma is_safe, which the solver refuses:"),
        "{verdict}"
    );
}

#[test]
fn a_rule_with_its_lemmas_is_proved_and_not_walked() {
    let verdict = invariant(&[&parent(""), &child(&[LOCKED_PAST_OPEN])], TIME);
    let JointVerdict::Proved(proof, Premise::Met { .. }) = &verdict else {
        panic!("proved, and a run meets its premise: {verdict}");
    };
    assert!(proof.found.is_empty(), "{verdict}");
    assert!(verdict.holds());
    assert_eq!(verdict.tag(), "PROVED");
    assert!(
        verdict.to_string().contains("PROVED for every run"),
        "{verdict}"
    );
}

#[test]
fn a_rule_its_lemmas_do_not_prove_is_proved_with_the_provers_guesses_too() {
    // The lemma holds but is not enough: a sealing parent may be unlocked
    // for all it says. The prover guesses as for a rule with no lemmas.
    let verdict = invariant(&[&parent(""), &child(&[LOCKED_WHEN_CLOSED])], TIME);
    let JointVerdict::Proved(proof, Premise::Met { .. }) = &verdict else {
        panic!("proved with the prover's guesses: {verdict}");
    };
    assert!(
        proof
            .found
            .iter()
            .any(|l| l.name == "Parent in Sealing: locked"),
        "{verdict}"
    );
    assert!(proof.dropped.is_empty(), "{verdict}");
}

#[test]
fn a_written_lemma_a_move_breaks_is_dropped_and_named() {
    let never_locked = "(not $Parent.locked)";
    let verdict = invariant(
        &[&parent(""), &child(&[LOCKED_PAST_OPEN, never_locked])],
        TIME,
    );
    let JointVerdict::Proved(proof, _) = &verdict else {
        panic!("proved without the broken lemma: {verdict}");
    };
    assert_eq!(
        proof.dropped.get("lemma1").map(String::as_str),
        Some("fails after Parent.Seal (from outside)"),
        "{verdict}"
    );
    assert!(
        verdict
            .to_string()
            .contains("dropped the written lemma lemma1, which fails after Parent.Seal"),
        "{verdict}"
    );
}

#[test]
fn a_written_lemma_the_solver_refuses_is_dropped_and_named() {
    let unknown = "(=> $Parent.nope true)";
    let verdict = invariant(&[&parent(""), &child(&[LOCKED_PAST_OPEN, unknown])], TIME);
    let JointVerdict::Proved(proof, _) = &verdict else {
        panic!("proved without the refused lemma: {verdict}");
    };
    let why = proof.dropped.get("lemma1").expect("lemma1 dropped");
    assert!(why.starts_with("the solver refuses: "), "{why}");
    assert!(why.contains("Parent.nope"), "{why}");
}

#[test]
fn a_rule_without_hints_is_proved_with_the_provers_guesses() {
    let verdict = invariant(&[&parent(""), &child(&[])], TIME);
    let JointVerdict::Proved(proof, Premise::Met { .. }) = &verdict else {
        panic!("proved with the prover's guesses: {verdict}");
    };
    assert!(verdict.holds());
    let sealing = proof
        .found
        .iter()
        .find(|lemma| lemma.name == "Parent in Sealing: locked")
        .unwrap_or_else(|| panic!("the guess the proof needs is listed: {verdict}"));
    assert_eq!(
        sealing.assert,
        "(=> (= $Parent.status Parent/Sealing) $Parent.locked)"
    );
    assert!(
        verdict.to_string().contains("with the prover's guesses"),
        "{verdict}"
    );
    // Written beside the rule, the guesses prove it with no more guessing.
    let pinned: Vec<&str> = proof.found.iter().map(|l| l.assert.as_str()).collect();
    let verdict = invariant(&[&parent(""), &child(&pinned)], TIME);
    let JointVerdict::Proved(proof, Premise::Met { .. }) = &verdict else {
        panic!("proved with the pinned guesses: {verdict}");
    };
    assert!(proof.found.is_empty(), "{verdict}");
}

#[test]
fn a_rule_the_guesses_do_not_prove_is_walked_and_says_why() {
    let verdict = invariant(&[TALLY, WATCHER], TIME);
    let JointVerdict::Invariant(walked, Some(tried)) = &verdict else {
        panic!("walked with the failed proof: {verdict}");
    };
    assert!(walked.holds(), "{walked}");
    assert!(!tried.proved());
    let text = verdict.to_string();
    assert!(text.contains("not proved by induction"), "{text}");
    assert!(text.contains("Tally.Finish"), "{text}");
}

#[test]
fn a_proved_rule_whose_premise_no_run_meets_proves_nothing() {
    let never_closes = parent("").replace(CLOSE, "");
    let verdict = invariant(&[&never_closes, &child(&[])], TIME);
    assert!(
        matches!(&verdict, JointVerdict::Proved(_, Premise::Never { .. })),
        "{verdict}"
    );
    assert!(!verdict.holds());
    assert_eq!(verdict.tag(), "VACUOUS");
    assert!(
        verdict.to_string().contains("CHECKS NOTHING AS WRITTEN"),
        "{verdict}"
    );
}

#[test]
fn a_premise_the_proof_rules_out_needs_no_walk() {
    let never_closes = parent("").replace(CLOSE, "");
    let not_closed = "(not (= $Parent.status Parent/Closed))";
    let verdict = invariant(&[&never_closes, &child(&[not_closed])], TIME);
    assert!(
        matches!(&verdict, JointVerdict::Proved(_, Premise::Excluded { .. })),
        "{verdict}"
    );
    assert!(!verdict.holds());
    assert_eq!(verdict.tag(), "VACUOUS");
    assert!(
        verdict
            .to_string()
            .contains("the proof shows no state meets"),
        "{verdict}"
    );
}

#[test]
fn a_rule_without_a_premise_has_none_to_meet() {
    let rule = child(&[LOCKED_PAST_OPEN]).replace(
        "Parent[Id].status == 'Closed' => Parent[Id].locked == true",
        "Parent[Id].status != 'Closed' || Parent[Id].locked == true",
    );
    let verdict = invariant(&[&parent(""), &rule], TIME);
    assert!(
        matches!(&verdict, JointVerdict::Proved(_, Premise::None)),
        "{verdict}"
    );
    assert!(verdict.holds());
    assert!(!verdict.to_string().contains("premise"), "{verdict}");
}

/// An action nothing outside may send: a closed parent unlocks.
const UNLOCK: &str = r#"
[[action]]
name = "Unlock"
kind = "internal"
from = ["Closed"]
to = "Closed"
effect = ["locked = false"]
"#;

/// An action nothing outside may send: an open parent closes unlocked.
const FORCE: &str = r#"
[[action]]
name = "Force"
kind = "internal"
from = ["Open"]
to = "Closed"
"#;

/// A type outside the rule's scope whose move spawns a parent and runs
/// `first` on it.
fn maker(first: &str) -> String {
    format!(
        r#"
[automaton]
name = "Maker"
states = ["Idle"]
initial = "Idle"

[[action]]
name = "Make"
kind = "input"
from = ["Idle"]
to = "Idle"
effect = ["spawn('Parent', '{first}')"]
"#
    )
}

#[test]
fn a_scheduled_action_fires_in_the_proof() {
    let closes_itself = parent(", \"schedule('Close', 60)\"");
    let verdict = invariant(&[&closes_itself, &child(&[LOCKED_PAST_OPEN])], TIME);
    assert!(matches!(&verdict, JointVerdict::Proved(..)), "{verdict}");
    // Closing schedules an unlock a minute later, which breaks the rule.
    let unlocks = format!(
        "{}effect = [\"schedule('Unlock', 60)\"]\n{UNLOCK}",
        parent("")
    );
    let verdict = invariant(&[&unlocks, &child(&[LOCKED_PAST_OPEN])], TIME);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("proved without the scheduled unlock: {verdict}");
    };
    let why = tried.shortfall().expect("not proved");
    assert!(why.contains("after Parent.Unlock.scheduled fires"), "{why}");
    // A lemma that the unlock never waits does not get round it: closing
    // arms it, so the proof drops it.
    let never_waits = "(not ($has.Parent.Unlock.scheduled mk/Parent.Unlock.scheduled))";
    let rule = child(&[LOCKED_PAST_OPEN, never_waits]);
    let verdict = invariant(&[&unlocks, &rule], TIME);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("proved with a lemma that the unlock never waits: {verdict}");
    };
    assert_eq!(
        tried.dropped.get("lemma1").map(String::as_str),
        Some("fails after Parent.Close (from outside)"),
        "{verdict}"
    );
    let why = tried.shortfall().expect("not proved");
    assert!(why.contains("after Parent.Unlock.scheduled fires"), "{why}");
}

/// An action nothing outside may send: a sealing or closed parent locks.
const LOCK: &str = r#"
[[action]]
name = "Lock"
kind = "internal"
from = ["Sealing", "Closed"]
effect = ["locked = true"]
"#;

#[test]
fn a_record_waiting_for_a_scheduled_action_is_settled() {
    // Closing unlocks the parent and locks it again a minute later; the
    // rule must hold in that minute too. With no lemma, only the rule's own
    // clause, read where nothing is in flight, can fail there.
    let locks_later = format!(
        "{}effect = [\"locked = false\", \"schedule('Lock', 60)\"]\n{LOCK}",
        parent("")
    );
    let verdict = invariant(&[&locks_later, &child(&[])], TIME);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("proved while the lock waits: {verdict}");
    };
    let why = tried.shortfall().expect("not proved");
    assert!(why.contains("after Parent.Close"), "{why}");
}

#[test]
fn a_spawned_records_first_action_is_a_move_of_the_proof() {
    let rule = child(&[LOCKED_PAST_OPEN]);
    let parent = format!("{}{FORCE}", parent(""));
    let verdict = invariant(&[&parent, &rule, &maker("Seal")], TIME);
    assert!(matches!(&verdict, JointVerdict::Proved(..)), "{verdict}");
    let verdict = invariant(&[&parent, &rule, &maker("Force")], TIME);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("proved without the spawn's first action: {verdict}");
    };
    let why = tried.shortfall().expect("not proved");
    assert!(
        why.contains("after Parent.Force (a spawn from Maker.Make)"),
        "{why}"
    );
}

#[test]
fn proofs_stop_when_the_time_is_spent() {
    let verdict = invariant(&[&parent(""), &child(&[LOCKED_PAST_OPEN])], Duration::ZERO);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("walked with the proof that ran out of time: {verdict}");
    };
    let why = tried.shortfall().expect("not proved");
    assert!(why.contains("not checked by the deadline"), "{why}");
}

#[test]
fn an_eventually_rule_with_its_measure_is_proved() {
    let mut hinted = check(&[&asker(true)], TIME).liveness;
    assert_eq!(hinted.len(), 1);
    let verdict = hinted.remove(0);
    assert!(
        matches!(verdict, JointVerdict::Proved(_, Premise::Met { .. })),
        "{verdict}"
    );
    // Nothing guesses a measure, so a rule without one is walked.
    let verdict = check(&[&asker(false)], TIME).liveness.remove(0);
    assert!(
        matches!(&verdict, JointVerdict::Liveness(walked, None) if walked.holds()),
        "{verdict}"
    );
}

#[test]
fn a_lemma_named_like_a_liveness_goal_cannot_drop_the_goal() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Idle", "Done"]
initial = "Idle"

[[liveness]]
name = "finishes"
eventually = "status == 'Done'"
measure = ["0"]

[[liveness.lemma]]
name = "some region"
assert = "true"
"#;
    for assertion in ["true", "false"] {
        // A true lemma must not be confused with the failing goal. A false
        // lemma really is dropped, but must not take that goal with it.
        let spec = spec.replace("assert = \"true\"", &format!("assert = {assertion:?}"));
        let verdict = check(&[&spec], TIME).liveness.remove(0);
        let JointVerdict::Liveness(walked, Some(proof)) = &verdict else {
            panic!("an unreachable target must remain unproved and be walked: {verdict}");
        };
        assert!(!walked.holds(), "{verdict}");
        assert!(!proof.proved(), "{verdict}");
        assert_eq!(
            proof.shortfall().as_deref(),
            Some("some region fails after covers")
        );
        if assertion == "true" {
            assert!(
                proof.dropped.is_empty(),
                "a true lemma still holds: {verdict}"
            );
        } else {
            assert_eq!(
                proof.dropped.get("some region").map(String::as_str),
                Some("fails after start")
            );
        }
    }
}

#[path = "check_test/one_type.rs"]
mod one_type;
