use super::*;

fn kinds() -> BTreeMap<String, VarKind> {
    [
        ("items", VarKind::Counter),
        ("ready", VarKind::Bool),
        ("tags", VarKind::List),
        ("goal", VarKind::Str),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn state(status: &str) -> TemperModelState {
    TemperModelState {
        status: status.to_string(),
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        strings: BTreeMap::new(),
    }
}

fn g(source: &str) -> Expr {
    temper_spec::predicate::parse(source).unwrap()
}

#[test]
fn declared_variables_missing_from_state_read_as_zero_values() {
    let s = state("A");
    let k = kinds();
    assert!(evaluate_guard(&g("items == 0 && items < 3"), &k, &s));
    assert!(evaluate_guard(&g("!ready && len(tags) == 0"), &k, &s));
    assert!(!evaluate_guard(&g("items >= 2"), &k, &s));
}

#[test]
fn evaluates_modeled_state() {
    let mut s = state("Draft");
    s.counters.insert("items".into(), 2);
    s.booleans.insert("ready".into(), true);
    s.lists.insert("tags".into(), vec!["vip".into()]);
    let k = kinds();
    assert!(evaluate_guard(
        &g("status == 'Draft' && items >= 2 && ready && 'vip' in tags"),
        &k,
        &s
    ));
    assert!(!evaluate_guard(&g("status in ['Closed']"), &k, &s));
}

#[test]
fn unmodeled_values_are_free_for_exploration_and_absent_for_enablement() {
    let s = state("A");
    let k = kinds();
    for unknown in [
        "P[p].status == 'Done'",
        "goal != ''",
        "!(P[p].status == 'Done')",
    ] {
        let guard = g(unknown);
        assert!(guard_may_hold(&guard, &k, &s), "{unknown} may hold");
        assert!(
            !evaluate_guard(&guard, &k, &s),
            "{unknown} is not guaranteed"
        );
    }
    // A local condition that is false decides, whatever the unknown is.
    let guard = g("items > 5 && P[p].status == 'Done'");
    assert!(!guard_may_hold(&guard, &k, &s));
    // A local condition that is true decides a disjunction.
    let guard = g("items == 0 || P[p].status == 'Done'");
    assert!(evaluate_guard(&guard, &k, &s));
}

#[test]
fn collects_list_membership_atoms_through_any_connective() {
    let mut pairs = BTreeSet::new();
    collect_list_contains_pairs(&g("!('a' in tags) || ready => 'b' in tags"), &mut pairs);
    assert_eq!(
        pairs.into_iter().collect::<Vec<_>>(),
        vec![("tags".into(), "a".into()), ("tags".into(), "b".into())]
    );
}

#[test]
fn every_equality_on_a_string_parameter_must_hold() {
    let source = r#"
[automaton]
name = "Equalities"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "a"
type = "string"
initial = ""

[[state]]
name = "b"
type = "string"
initial = "b"

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"
params = [{ name = "p", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "a"

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "b"

[[invariant]]
name = "track_both"
assert = "a == b || a != b"
"#;
    let model = crate::model::build_model_from_ioa(source, 2).unwrap();
    let mut initial = stateright::Model::init_states(&model).remove(0);
    let t = &model.transitions[0];
    assert!(!t.string_params[0].writes);
    assert!(
        successors(&model, t, &initial).is_empty(),
        "different fields admit no value for p"
    );
    initial.strings.insert("a".into(), "b".into());
    let allowed = successors(&model, t, &initial);
    assert_eq!(allowed.len(), 1);
    assert_eq!(allowed[0].0["p"], ParamValue::Str("b".into()));
    assert_eq!(allowed[0].1.strings, initial.strings);
}
