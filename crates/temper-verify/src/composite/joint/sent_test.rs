//! A reaction's literal the walk carries as another of its kind: its trace
//! says what the reaction really sends, and replays as it is written.

use std::collections::BTreeMap;

use temper_spec::automaton::{Automaton, parse_automaton};

use super::{JointReplay, TraceMove, replay_joint_invariants};

fn sent() -> Vec<Automaton> {
    [
        include_str!("../../../tests/race_models/sent/Card.ioa.toml"),
        include_str!("../../../tests/race_models/sent/Note.ioa.toml"),
    ]
    .iter()
    .map(|source| parse_automaton(source).expect("model parses"))
    .collect()
}

fn mv(entity: &str, action: &str, params: &[(&str, &str)]) -> TraceMove {
    TraceMove {
        entity: entity.to_string(),
        action: action.to_string(),
        params: params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
    }
}

/// Praise sends `'well done'`, which the walk carries as `'thank you'`: no
/// rule tells them apart. Then Card.Write with `text` as written.
fn praised_then_written(text: &str) -> JointReplay {
    let automatons = sent();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let same: BTreeMap<String, String> = [("Note", "n"), ("Card", "n")]
        .iter()
        .map(|(entity, id)| (entity.to_string(), id.to_string()))
        .collect();
    let trace = [
        mv("Note", "Add", &[]),
        mv("Card", "Add", &[]),
        mv("Note", "Praise", &[]),
        mv("Card", "Write", &[("text", text)]),
    ];
    replay_joint_invariants(&refs, "Card", &same, &trace).expect("the trace is allowed")
}

#[test]
fn a_merged_sent_literal_replays_as_the_reaction_sends_it() {
    let replay = praised_then_written("'well done'");
    let last = replay.steps.last().expect("a step");
    assert!(!last.state.contains("in flight"), "{}", last.state);
    let said = last.action.as_deref().expect("a move");
    assert!(
        said.contains("text='well done'") && said.contains("walked as 'thank you'"),
        "{said}"
    );
}

#[test]
fn the_literal_a_reaction_never_sends_does_not_take_its_dispatch() {
    let replay = praised_then_written("'thank you'");
    let last = &replay.steps.last().expect("a step").state;
    assert!(last.contains("in flight: Card.Write"), "{last}");
}
