//! `principal_record = true` marks a type whose record, keyed by a user's
//! `sub`, describes that user to Cedar as `principal.<Type>`.

use super::parser::{LivenessEnforcement, parse_automaton_with_liveness};

fn member_spec(name: &str) -> String {
    format!(
        r#"
[automaton]
name = "{name}"
states = ["Active", "Removed"]
initial = "Active"
allow_indefinite_states = ["Active", "Removed"]
principal_record = true

[[action]]
name = "Remove"
kind = "input"
from = ["Active"]
to = "Removed"
"#
    )
}

fn parse(source: &str) -> Result<super::types::Automaton, String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly).map_err(|e| e.to_string())
}

#[test]
fn a_type_marked_principal_record_loads() {
    if let Err(error) = parse(&member_spec("Member")) {
        panic!("a principal record spec loads: {error}");
    }
}

#[test]
fn a_principal_record_named_like_a_principal_attribute_is_refused() {
    // Each is an attribute the kernel sets on a principal, or the shape of
    // one: a record type with that name would stand in for it in Cedar.
    for name in ["role", "agentTypeVerified", "id", "actingForMember"] {
        let error = parse(&member_spec(name)).expect_err(name);
        assert!(
            error.contains(&format!(
                "principal_record type '{name}' must start with an uppercase letter"
            )),
            "{name}: {error}"
        );
    }
}
