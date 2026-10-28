use super::*;

/// An item spec with `actions` after a header of three states and a field of
/// each kind the guard language compares.
fn item(actions: &str) -> Automaton {
    let source = format!(
        r#"
[automaton]
name = "Item"
states = ["Waiting", "Paused", "Done"]
initial = "Waiting"

[[state]]
name = "ready"
type = "bool"
initial = false

[[state]]
name = "approved"
type = "bool"
initial = false

[[state]]
name = "tries"
type = "counter"
initial = 0

[[state]]
name = "limit"
type = "counter"
initial = 0

[[state]]
name = "stage"
type = "string"
initial = ""

[[action]]
name = "Pause"
kind = "input"
from = ["Waiting"]
to = "Paused"

[[action]]
name = "Resume"
kind = "input"
from = ["Paused"]
to = "Waiting"
{actions}"#
    );
    temper_spec::automaton::parse_automaton(&source).expect("the item spec parses")
}

/// A schedules B and C after `delays`, B needs `b` and C needs `c`; all three
/// are made from Waiting.
fn pair(delays: (u64, u64), b: &str, c: &str) -> Findings {
    check(&item(&format!(
        r#"
[[action]]
name = "A"
from = ["Waiting"]
effect = ["schedule('B', {})", "schedule('C', {})"]

[[action]]
name = "B"
from = ["Waiting"]
guard = "{b}"

[[action]]
name = "C"
from = ["Waiting"]
guard = "{c}"
"#,
        delays.0, delays.1
    )))
}

fn refused(findings: &Findings) -> &str {
    let [text] = findings.refused.as_slice() else {
        panic!("one refusal: {findings:?}");
    };
    assert!(findings.undecided.is_empty(), "{findings:?}");
    text
}

fn passes(findings: Findings) {
    assert_eq!(findings, Findings::default());
}

#[test]
fn moves_fired_together_whose_guards_exclude_each_other_are_refused() {
    let text = refused(&pair((10, 10), "ready && approved", "!ready")).to_string();
    for named in [
        "A schedules B and C",
        "after 10 s",
        "`ready && approved`",
        "`!ready`",
    ] {
        assert!(text.contains(named), "{text}");
    }
    // Numbers on either side of each other, and different strings.
    refused(&pair((10, 10), "tries > 3", "tries < 2"));
    refused(&pair((10, 10), "stage == 'a'", "stage in ['b', 'c']"));
    refused(&pair((10, 10), "empty(stage)", "stage == 'a'"));
}

#[test]
fn moves_fired_together_that_can_both_be_made_pass() {
    passes(pair((10, 10), "ready && approved", "approved"));
    passes(pair((10, 10), "tries > 3", "tries < 5"));
    passes(pair((10, 10), "stage != 'a'", "stage != 'b'"));
    passes(pair((10, 10), "ready => approved", "!approved"));
}

#[test]
fn moves_fired_at_different_times_are_not_refused() {
    // The record may change between the two fires.
    passes(pair((10, 3600), "ready", "!ready"));
}

#[test]
fn a_move_that_may_make_the_other_possible_is_not_refused() {
    let findings = check(&item(
        r#"
[[action]]
name = "A"
from = ["Waiting"]
effect = ["schedule('B', 10)", "schedule('C', 10)"]

[[action]]
name = "B"
from = ["Waiting"]
guard = "!ready"
effect = ["ready = true"]

[[action]]
name = "C"
from = ["Waiting"]
guard = "ready"
"#,
    ));
    passes(findings);
}

#[test]
fn one_scheduled_move_with_a_guard_passes() {
    let findings = check(&item(
        r#"
[[action]]
name = "A"
from = ["Waiting"]
effect = ["schedule('B', 10)"]

[[action]]
name = "B"
from = ["Waiting"]
to = "Done"
guard = "ready"
"#,
    ));
    passes(findings);
}

#[test]
fn guards_the_check_cannot_read_are_undecided_not_refused() {
    let findings = pair((10, 10), "ready", "tries < limit");
    assert!(findings.refused.is_empty(), "{findings:?}");
    let [text] = findings.undecided.as_slice() else {
        panic!("one undecided pair: {findings:?}");
    };
    assert!(text.contains("tries < limit"), "{text}");
    // What it can read decides the pair, whatever the rest is.
    refused(&pair((10, 10), "ready && tries < limit", "!ready"));
    passes(pair((10, 10), "ready || tries < limit", "ready"));
}

#[test]
fn a_scheduled_move_the_record_cannot_reach_a_state_for_is_refused() {
    let findings = check(&item(
        r#"
[[action]]
name = "A"
from = ["Waiting"]
to = "Done"
effect = ["schedule('B', 10)"]

[[action]]
name = "B"
from = ["Waiting"]
"#,
    ));
    let text = refused(&findings);
    for named in ["A schedules B", "only from Waiting", "from Done"] {
        assert!(text.contains(named), "{text}");
    }
}

#[test]
fn a_scheduled_move_another_move_brings_the_record_to_passes() {
    // A leaves the record Paused; Resume takes it back to Waiting.
    let findings = check(&item(
        r#"
[[action]]
name = "A"
from = ["Waiting"]
to = "Paused"
effect = ["schedule('B', 10)"]

[[action]]
name = "B"
from = ["Waiting"]
"#,
    ));
    passes(findings);
}

#[test]
fn moves_valid_from_different_states_are_refused_together() {
    // At one moment the record is in one state, so B or C is refused.
    let findings = check(&item(
        r#"
[[action]]
name = "A"
from = ["Waiting", "Paused"]
effect = ["schedule('B', 10)", "schedule('C', 10)"]

[[action]]
name = "B"
from = ["Waiting"]

[[action]]
name = "C"
from = ["Paused"]
"#,
    ));
    let text = refused(&findings);
    assert!(text.contains("B needs from Waiting"), "{text}");
}
