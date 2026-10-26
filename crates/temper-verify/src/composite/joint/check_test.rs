//! Every rule over several records is proved by induction first: with the
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
fn a_rule_whose_proof_fails_is_walked_and_says_why() {
    let verdict = invariant(&[&parent(""), &child(&[LOCKED_WHEN_CLOSED])], TIME);
    let JointVerdict::Invariant(walked, Some(tried)) = &verdict else {
        panic!("walked with the failed proof: {verdict}");
    };
    // The rule holds, so the walk passes it; the proof only lacked a lemma.
    assert!(walked.holds(), "{walked}");
    assert!(!tried.proved());
    let text = verdict.to_string();
    assert!(text.contains("not proved by induction"), "{text}");
    assert!(text.contains("Parent.Close"), "{text}");
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
    assert!(verdict.to_string().contains("PROVES NOTHING"), "{verdict}");
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
    // arms it.
    let never_waits = "(not ($has.Parent.Unlock.scheduled mk/Parent.Unlock.scheduled))";
    let rule = child(&[LOCKED_PAST_OPEN, never_waits]);
    let verdict = invariant(&[&unlocks, &rule], TIME);
    let JointVerdict::Invariant(_, Some(tried)) = &verdict else {
        panic!("proved with a lemma that the unlock never waits: {verdict}");
    };
    let why = tried.shortfall().expect("not proved");
    assert!(why.contains("lemma1 fails after Parent.Close"), "{why}");
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
