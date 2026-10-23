use super::*;

/// A gate a timer passes three seconds after it is added.
const GATE: &str = r#"
[automaton]
name = "KgGate"
states = ["Unmade", "Closed", "Passed"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Closed", "Passed"]

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Closed"

[[state_timeout]]
state = "Closed"
after_seconds = 3
on_timeout = "Pass"

[[action]]
name = "Pass"
kind = "input"
from = ["Closed"]
to = "Passed"
"#;

/// A hold that starts in its timed state.
const HOLD: &str = r#"
[automaton]
name = "KgHold"
states = ["Waiting", "Expired"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Expired"]

[[action]]
name = "Expire"
kind = "input"
from = ["Waiting"]
to = "Expired"

[[state_timeout]]
state = "Waiting"
after_seconds = 5
on_timeout = "Expire"
"#;

const IS_MEMBER: &str = r#"((principal is Customer && ["owner", "member"].contains(principal.id)) ||
   (context has actingFor && ["owner", "member"].contains(context.actingFor)))"#;
const IS_OWNER: &str = r#"((principal is Customer && principal.id == "owner") ||
   (context has actingFor && context.actingFor == "owner"))"#;

fn specs(entity_type: &str, source: &str) -> Vec<(String, String)> {
    vec![(entity_type.to_string(), source.to_string())]
}

/// Members may add a gate and pass one, but a forbid keeps passing to the owner.
fn gate_policy(add_allowed_to: &str) -> String {
    format!(
        r#"
permit(principal, action in [Action::"read", Action::"Add"], resource is KgGate) when {{ {add_allowed_to} }};
permit(principal, action == Action::"Pass", resource is KgGate) when {{ {IS_MEMBER} }};
forbid(principal, action == Action::"Pass", resource is KgGate) unless {{ {IS_OWNER} }};
"#
    )
}

#[test]
fn a_timer_making_a_move_cedar_forbids_to_those_who_arm_it_is_reported() {
    let warnings = state_timer_policy_warnings(&specs("KgGate", GATE), &gate_policy(IS_MEMBER));

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let warning = &warnings[0];
    assert_eq!(warning.entity_type, "KgGate");
    assert_eq!(warning.state, "Closed");
    assert_eq!(warning.on_timeout, "Pass");
    assert_eq!(warning.entered_by, vec!["Add".to_string()]);
    assert!(
        warning
            .principals
            .contains(&"Customer::\"member\"".to_string()),
        "{:?}",
        warning.principals
    );
    assert!(
        warning
            .principals
            .contains(&"Agent acting for \"member\"".to_string()),
        "{:?}",
        warning.principals
    );
    assert!(
        !warning.principals.iter().any(|p| p.contains("\"owner\"")),
        "the owner may pass, so the timer makes no move the owner is refused: {:?}",
        warning.principals
    );
    assert!(warning.message.contains("Pass"), "{}", warning.message);
}

#[test]
fn no_warning_when_only_those_allowed_the_timers_move_may_arm_it() {
    let warnings = state_timer_policy_warnings(&specs("KgGate", GATE), &gate_policy(IS_OWNER));
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn a_timer_action_no_statement_permits_is_not_reported() {
    // Only the timer passes a gate: nothing permits Pass and nothing forbids it.
    let policy = format!(
        r#"permit(principal, action == Action::"Add", resource is KgGate) when {{ {IS_MEMBER} }};"#
    );
    let warnings = state_timer_policy_warnings(&specs("KgGate", GATE), &policy);
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn a_create_that_lands_in_the_timed_state_counts_as_entering_it() {
    let policy = format!(
        r#"
permit(principal, action == Action::"create", resource is KgHold) when {{ {IS_MEMBER} }};
forbid(principal, action == Action::"Expire", resource is KgHold) unless {{ {IS_OWNER} }};
"#
    );
    let warnings = state_timer_policy_warnings(&specs("KgHold", HOLD), &policy);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].entered_by, vec!["create".to_string()]);
    assert!(
        warnings[0]
            .principals
            .contains(&"Customer::\"member\"".to_string())
    );
}

#[test]
fn a_principal_the_policies_do_not_name_is_asked_about_too() {
    let policy = r#"
permit(principal, action == Action::"Add", resource is KgGate);
forbid(principal, action == Action::"Pass", resource is KgGate) unless { principal.id == "owner" };
"#;
    let warnings = state_timer_policy_warnings(&specs("KgGate", GATE), policy);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]
            .principals
            .iter()
            .any(|p| p.contains(UNNAMED_PRINCIPAL)),
        "{:?}",
        warnings[0].principals
    );
}
