//! A text literal reaches only the ids and fields that can meet it.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A note with `messages` actions that each write their own message, which
/// nothing compares with an id or with the box's fields (and which the walk
/// does not keep: anyone may also edit it), and that hands its box the
/// literal holder `'main'` when sent. The box's rule reads the note
/// the holder names, and is `rule` or else its tag is `'kept'`.
fn note_and_box(messages: usize, rule: &str) -> [String; 2] {
    let mut note = String::from(
        r#"
[automaton]
name = "Note"
states = ["Open", "Sent", "Gone"]
initial = "Open"

[[state]]
name = "message"
type = "string"
initial = ""

[[action]]
name = "Edit"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "message", type = "string" }]

[[action]]
name = "Send"
kind = "input"
from = ["Open"]
to = "Sent"

[[action.triggers]]
name = "hand"
kind = "entity"
target_entity = "Box"
target_action = "Hold"

[action.triggers.args]
holder = "'main'"

[action.triggers.resolve_target]
kind = "same_id"
"#,
    );
    for i in 0..messages {
        note.push_str(&format!(
            r#"
[[action]]
name = "Write{i}"
kind = "input"
from = ["Open"]
to = "Open"
guard = "message != 'note text number {i}, long enough to read as a message'"
effect = ["message = 'note text number {i}, long enough to read as a message'"]
"#
        ));
    }
    let boxed = format!(
        r#"
[automaton]
name = "Box"
states = ["Empty", "Held"]
initial = "Empty"

[[state]]
name = "holder"
type = "string"
initial = ""

[[state]]
name = "tag"
type = "string"
initial = ""

[[action]]
name = "Hold"
kind = "internal"
from = ["Empty"]
to = "Held"
params = [{{ name = "holder", type = "string" }}]

[[action]]
name = "Tag"
kind = "input"
from = ["Empty"]
to = "Empty"
params = [{{ name = "tag", type = "string" }}]

[[invariant]]
name = "rule"
assert = "{rule} || tag == 'kept'"
"#
    );
    [note, boxed]
}

fn check(sources: &[String]) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, None, bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn literals_no_id_or_field_meets_do_not_multiply_the_walk() {
    // No action reaches Gone: the rule holds, and the whole walk is made.
    let rule = "Note[holder].status != 'Gone'";
    let few = check(&note_and_box(1, rule));
    let many = check(&note_and_box(12, rule));
    assert!(matches!(few.outcome, JointOutcome::Holds), "{few}");
    assert!(matches!(many.outcome, JointOutcome::Holds), "{many}");
    // The messages are neither ids nor the box's holder or tag: eleven more
    // of them start and walk no more states.
    assert_eq!(few.states, many.states, "{few}\n{many}");
}

#[test]
fn a_literal_that_names_a_record_still_reaches_its_id() {
    // The box holds 'main' once the note is sent: the rule breaks when the
    // note's id is 'main', which the walk must still start from.
    let result = check(&note_and_box(12, "Note[holder].status != 'Sent'"));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(
        path.iter().any(|step| step.state.contains("holder='main'")),
        "{result}"
    );
}
