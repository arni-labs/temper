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

fn two_strings_model() -> TemperModel {
    crate::model::build_model_from_ioa(
        r#"
[automaton]
name = "Pair"
states = ["New", "Done"]
initial = "New"
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "b"
type = "string"
initial = ""
[[action]]
name = "Write"
kind = "input"
from = ["New"]
to = "Done"
params = [{ name = "a", type = "string" }, { name = "b", type = "string" }]
[[invariant]]
name = "equal"
assert = "a == b"
"#,
        3,
    )
    .unwrap()
}

#[test]
fn fresh_parameters_can_differ_from_each_other() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let successors = successors(&model, &model.transitions[0], &initial);
    assert!(successors.iter().any(|(_, s)| !s.strings["a"].is_empty()
        && !s.strings["b"].is_empty()
        && s.strings["a"] != s.strings["b"]));
}

#[test]
fn concrete_hash_literals_are_never_renamed_or_equal_to_symbols() {
    let mut values = BTreeMap::from([
        ("literal".into(), ModelString::from("#2")),
        ("symbol".into(), ModelString::Symbol(2)),
    ]);
    canonical_strings(&mut [&mut values]);
    assert_eq!(values["literal"], ModelString::from("#2"));
    assert_eq!(values["symbol"], ModelString::Symbol(1));
    let mut s = state("New");
    s.strings = values;
    let k = BTreeMap::from([
        ("literal".into(), VarKind::Str),
        ("symbol".into(), VarKind::Str),
    ]);
    assert_eq!(
        truth(
            &g("literal == '#2' && symbol != '#1' && literal != symbol"),
            &k,
            &s
        ),
        Truth::True
    );
}

#[test]
fn string_products_stop_inside_expansion() {
    let model = two_strings_model();
    let initial = stateright::Model::init_states(&model).remove(0);
    let budget = Expansion::default();
    budget.reset(2, None);
    let next = successors_budgeted(
        &model,
        &model.transitions[0],
        &initial,
        &BTreeSet::new(),
        &budget,
    );
    assert!(next.is_empty());
    assert!(budget.exhausted.get());
}
