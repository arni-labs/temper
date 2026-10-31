use super::*;
use temper_spec::automaton::parse_automaton;

#[test]
fn a_chain_to_a_distinct_id_can_be_refused_by_cedar() {
    let a = r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "B"
target_action = "Forward"
principal = "chain"
[action.triggers.resolve_target]
kind = "same_id"
"#;
    let b = r#"
[automaton]
name = "B"
states = ["Ready", "Done"]
initial = "Ready"
[[state]]
name = "destination"
type = "string"
initial = ""
[[action]]
name = "Forward"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "second"
kind = "entity"
target_entity = "C"
target_action = "Finish"
[action.triggers.resolve_target]
kind = "field"
id_field = "destination"
"#;
    let c = r#"
[automaton]
name = "C"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Finish"
kind = "input"
from = ["Ready"]
to = "Done"
"#;
    let automatons: Vec<_> = [a, b, c]
        .iter()
        .map(|text| parse_automaton(text).unwrap())
        .collect();
    let by_name = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), a))
        .collect();
    let policies = SendAnalysis::new(
        r#"
permit(principal, action == Action::"Start", resource);
permit(principal == Agent::"service:chain", action, resource) when {
    context has triggerSourceEntityId && resource has id &&
    context.triggerSourceEntityId == resource.id
};
"#,
    )
    .unwrap();
    let model = JointModel::new(
        &by_name,
        &BTreeSet::from(["A".into(), "B".into(), "C".into()]),
        "A",
        &[],
        Sending {
            policies: Some(&policies),
            kernel_moves: true,
            relies_on: BTreeSet::new(),
        },
        false,
    );
    for same_id in [false, true] {
        let ids = BTreeMap::from([
            ("A".into(), "id1".into()),
            ("B".into(), "id1".into()),
            ("C".into(), if same_id { "id1" } else { "id2" }.into()),
        ]);
        let (mut state, _) = model.init_with_ids(&ids);
        let destination = state.records["C"].strings["Id"].clone();
        state
            .records
            .get_mut("B")
            .unwrap()
            .strings
            .insert("destination".into(), destination);
        // Start and Forward each have one permitted outcome.
        for _ in 0..2 {
            let moves = model.moves(&state);
            assert_eq!(moves.len(), 1);
            let (mut next, _) = model.next(&state, &moves[0]);
            assert_eq!(next.len(), 1);
            state = next.remove(0).state;
        }
        assert_eq!(state.records["B"].status, "Done");
        assert_eq!(state.inflight.len(), 1);
        let moves = model.moves(&state);
        assert_eq!(moves.len(), 1);
        let (next, _) = model.next(&state, &moves[0]);
        // A field hop forgets that the source id is the receiver's, so the
        // precomputed answer is Maybe. It is decided when it lands, from A's
        // id (the one the inherited service keeps) and C's: Cedar refuses
        // id1 != id2 and allows id1 == id1.
        let refused = next
            .iter()
            .any(|n| n.state.records["C"].status == "Ready" && n.state.inflight.is_empty());
        let done = next.iter().any(|n| n.state.records["C"].status == "Done");
        assert_eq!((refused, done), (!same_id, same_id), "same_id {same_id}");
    }
}
