//! A trigger or timer on a strict type passes only parameters its target
//! action declares: otherwise the kernel refuses every dispatch it makes.

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

fn parse(source: &str) -> Result<(), String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A `Ticket` whose `Answer` retries itself, passing `args`; `Retry` declares
/// only `note`.
fn spec(strict: bool, args: &str) -> String {
    format!(
        r#"
[automaton]
name = "Ticket"
states = ["Open", "Stuck"]
initial = "Open"
strict_action_params = {strict}

[[action]]
name = "Answer"
kind = "input"
from = ["Stuck"]
params = ["note"]

[[action.triggers]]
name = "again"
kind = "entity"
target_entity = "Ticket"
target_action = "Retry"
[action.triggers.args]
{args}
[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Retry"
kind = "input"
from = ["Stuck"]
to = "Open"
params = ["note"]
"#
    )
}

#[track_caller]
fn refused(spec: &str, needles: &[&str]) {
    match parse(spec) {
        Ok(()) => panic!("loaded a spec that should be refused:\n{spec}"),
        Err(error) => {
            for needle in needles {
                assert!(error.contains(needle), "`{needle}` not in: {error}");
            }
        }
    }
}

#[test]
fn a_self_trigger_passing_an_undeclared_parameter_is_refused() {
    refused(
        &spec(true, "note = \"note\"\nmoved_via = \"'the line'\""),
        &[
            "trigger 'again' on action 'Answer'",
            "'moved_via'",
            "'Retry'",
        ],
    );
}

#[test]
fn a_self_trigger_passing_declared_parameters_loads() {
    parse(&spec(true, "note = \"note\"")).expect("declared args load");
}

#[test]
fn a_type_that_is_not_strict_accepts_any_parameter() {
    parse(&spec(false, "moved_via = \"'the line'\"")).expect("lenient type loads");
}

#[test]
fn a_timer_passing_an_undeclared_parameter_is_refused() {
    let timer = "\n[[state_timeout]]\nstate = \"Stuck\"\nafter_seconds = 60\non_timeout = \"Retry\"\n\
                 [state_timeout.args]\nwhy = \"'timed out'\"\n";
    refused(
        &(spec(true, "note = \"note\"") + timer),
        &["state_timeout for 'Stuck'", "'why'", "'Retry'"],
    );
}
