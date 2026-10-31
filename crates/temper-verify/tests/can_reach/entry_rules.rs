//! Route checks retain guard-only values when the same model checks entries.

use super::{TOKEN, broken, model_check};

#[test]
fn an_entry_rule_does_not_hide_a_route_closed_by_a_string_input() {
    let spec = format!(
        "{}{}",
        TOKEN
            .replace("initial = \"\"", "initial = \"ok\"")
            .replace(
                "states = [\"Waiting\", \"Done\"]",
                "states = [\"Waiting\", \"Replaced\", \"Done\"]"
            )
            .replace("from = [\"Waiting\"]", "from = [\"Waiting\", \"Replaced\"]"),
        r#"
[[state]]
name = "replaced"
type = "bool"
initial = false

[[action]]
name = "ReplaceToken"
from = ["Waiting"]
to = "Replaced"
params = [{ name = "token", type = "string" }]
guard = "!replaced"
effect = ["replaced = true"]

[[entry_rule]]
name = "only_an_unused_replacement"
place = "Replaced"
assert = "!replaced"
"#
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert_eq!(moves, ["ReplaceToken"]);
    assert_eq!(place, "Replaced");
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.bounded_by.is_empty());
    assert!(result.unproved_can_reach.is_empty());
    let [refusal] = result.counterexamples.as_slice() else {
        panic!("expected only the broken route: {result:?}");
    };
    assert_eq!(refusal.never_reaches, ["Done"]);
    let (last, _) = refusal.trace.last().unwrap();
    assert!(last.booleans["replaced"]);
    assert_ne!(last.strings["token"], "ok".into());

    // Restoring the token keeps the route open while still satisfying
    // the entry rule on every move into Replaced.
    let recoverable = format!(
        "{spec}{}",
        r#"
[[action]]
name = "RestoreToken"
from = ["Replaced"]
to = "Waiting"
effect = ["token = 'ok'", "replaced = false"]
"#
    );
    let l1 = model_check(&recoverable);
    assert!(l1.passed, "{}", l1.summary);
}
