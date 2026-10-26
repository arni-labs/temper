//! A rule carries the hints its proof by induction is given, beside it.

use super::parse_automaton;

const SPEC: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Sealed", "Done"]
initial = "Draft"

[[state]]
name = "locked"
type = "bool"
initial = false

[[action]]
name = "Seal"
kind = "input"
from = ["Draft"]
to = "Sealed"
effect = ["locked = true"]

[[action]]
name = "Finish"
kind = "internal"
from = ["Sealed"]
to = "Done"

[[invariant]]
name = "done_is_locked"
assert = "status == 'Done' => locked == true"

[[invariant.lemma]]
name = "sealed_is_locked"
assert = "(=> (= $Doc.status Doc/Sealed) $Doc.locked)"

[[liveness]]
name = "sealed_finishes"
eventually = "status == 'Sealed' => status == 'Done'"
measure = ["(ite (= $Doc.status Doc/Sealed) 1 0)"]

[[liveness.lemma]]
name = "sealed_is_locked"
assert = "(=> (= $Doc.status Doc/Sealed) $Doc.locked)"

[[liveness.region]]
name = "sealed"
region = "(= $Doc.status Doc/Sealed)"
step = "Doc.Finish"
"#;

#[test]
fn rules_carry_their_proof_hints() {
    let automaton = parse_automaton(SPEC).expect("spec with hints parses");
    let invariant = &automaton.invariants[0];
    assert_eq!(invariant.lemma.len(), 1);
    assert_eq!(invariant.lemma[0].name, "sealed_is_locked");
    assert_eq!(
        invariant.lemma[0].assert,
        "(=> (= $Doc.status Doc/Sealed) $Doc.locked)"
    );
    let liveness = &automaton.liveness[0];
    assert_eq!(liveness.lemma.len(), 1);
    assert_eq!(liveness.measure, ["(ite (= $Doc.status Doc/Sealed) 1 0)"]);
    assert_eq!(liveness.region.len(), 1);
    assert_eq!(liveness.region[0].step, "Doc.Finish");
    assert_eq!(liveness.region[0].witness, None);
}

#[test]
fn a_hint_with_an_unknown_key_is_refused() {
    let misspelt = SPEC.replacen(
        "name = \"sealed_is_locked\"",
        "nme = \"sealed_is_locked\"",
        1,
    );
    let error = parse_automaton(&misspelt).expect_err("a lemma with an unknown key");
    assert!(error.to_string().contains("nme"), "{error}");
    let misspelt = SPEC.replace("step = \"Doc.Finish\"", "stp = \"Doc.Finish\"");
    let error = parse_automaton(&misspelt).expect_err("a region with an unknown key");
    assert!(error.to_string().contains("stp"), "{error}");
}
