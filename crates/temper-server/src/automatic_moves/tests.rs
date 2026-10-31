use super::*;

const ROUND: &str = r#"
[automaton]
name = "Round"
states = ["Open", "Running", "Ready", "Filed", "Closed"]
initial = "Open"

[[action]]
name = "Run"
kind = "input"
from = ["Open"]
to = "Running"

[[action.triggers]]
name = "work"
kind = "wasm"
module = "round"
on_success = "RunDone"

[[action]]
name = "RunDone"
from = ["Running"]
to = "Ready"

[[action.triggers]]
name = "file"
kind = "entity"
target_entity = "Round"
target_action = "File"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "File"
from = ["Ready"]
to = "Filed"

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Round"
target_action = "Finish"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Finish"
from = ["Filed"]
to = "Closed"
"#;

fn report(spec: &str, policy: &str) -> Report {
    check(&[("Round", spec)], policy).expect("spec and policy parse")
}

fn codes<'a>(findings: &'a [Finding], code: &str) -> Vec<&'a Finding> {
    findings
        .iter()
        .filter(|finding| finding.code == code)
        .collect()
}

/// Far Bank: Run's module answers RunDone as the caller, File follows,
/// and Filed's Finish follows as that same caller. A permit that allows
/// Finish only for service:wasm-runtime does not cover the caller.
#[test]
fn a_callback_chain_permitted_only_for_wasm_runtime_is_refused() {
    let policy = r#"
permit(principal, action == Action::"Run", resource is Round);
permit(principal, action == Action::"File", resource is Round);
permit(principal == Agent::"service:wasm-runtime", action == Action::"Finish", resource is Round);
"#;
    let found = report(ROUND, policy);
    let refused = codes(&found.errors, REFUSED_FOLLOW_UP);
    assert!(
        refused
            .iter()
            .any(|finding| { finding.action == "Finish" && finding.principal == "outside" }),
        "{:?}",
        found.errors
    );
    assert!(
        !refused.iter().any(|finding| finding.action == "File"),
        "File is permitted for the caller: {:?}",
        found.errors
    );
    assert!(
        codes(&found.errors, DEAD_SERVICE_PERMIT)
            .iter()
            .any(|finding| finding.principal == "service:wasm-runtime"),
        "{:?}",
        found.errors
    );
}

#[test]
fn a_member_permit_lets_the_same_callback_chain_finish() {
    let policy = r#"
permit(principal, action == Action::"Run", resource is Round);
permit(principal, action == Action::"File", resource is Round);
permit(principal is Customer, action == Action::"Finish", resource is Round);
"#;
    let found = report(ROUND, policy);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn a_declared_principal_is_who_the_follow_up_runs_as() {
    let spec = ROUND.replace(
        "name = \"finish\"\nkind = \"entity\"",
        "name = \"finish\"\nkind = \"entity\"\nprincipal = \"wasm-runtime\"",
    );
    let policy = r#"
permit(principal, action == Action::"Run", resource is Round);
permit(principal, action == Action::"File", resource is Round);
permit(principal == Agent::"service:wasm-runtime", action == Action::"Finish", resource is Round);
"#;
    let found = report(&spec, policy);
    assert!(
        codes(&found.errors, REFUSED_FOLLOW_UP).is_empty(),
        "{:?}",
        found.errors
    );
    assert!(
        codes(&found.errors, DEAD_SERVICE_PERMIT).is_empty(),
        "{:?}",
        found.errors
    );
}

#[test]
fn a_permit_for_a_service_that_never_sends_stays_an_error_beside_a_member_permit() {
    let policy = r#"
permit(principal, action == Action::"Run", resource is Round);
permit(principal, action == Action::"File", resource is Round);
permit(principal is Customer, action == Action::"Finish", resource is Round);
permit(principal == Agent::"service:wasm-runtime", action == Action::"Finish", resource is Round);
"#;
    let found = report(ROUND, policy);
    assert!(
        codes(&found.errors, REFUSED_FOLLOW_UP).is_empty(),
        "{:?}",
        found.errors
    );
    assert_eq!(codes(&found.errors, DEAD_SERVICE_PERMIT).len(), 1);
}

#[test]
fn a_record_condition_warns_and_does_not_refuse() {
    let policy = r#"
permit(principal, action == Action::"Run", resource is Round);
permit(principal, action == Action::"File", resource is Round);
permit(principal is Customer, action == Action::"Finish", resource is Round)
when { resource.owner == "someone" };
"#;
    let found = report(ROUND, policy);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert!(
        found
            .warnings
            .iter()
            .any(|finding| { finding.code == CONDITIONAL_FOLLOW_UP && finding.action == "Finish" }),
        "{:?}",
        found.warnings
    );
}

#[test]
fn a_state_timer_fires_as_whoever_armed_it() {
    let spec = r#"
[automaton]
name = "Round"
states = ["Open", "Waiting", "Told"]
initial = "Open"

[[action]]
name = "Start"
kind = "input"
from = ["Open"]
to = "Waiting"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "Fire"

[[action]]
name = "Fire"
from = ["Waiting"]
to = "Waiting"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Round"
target_action = "Tell"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Tell"
from = ["Waiting"]
to = "Told"
"#;
    let only_the_fallback = r#"
permit(principal, action == Action::"Start", resource is Round);
permit(principal == Agent::"service:state-timer", action == Action::"Tell", resource is Round);
"#;
    let found = report(spec, only_the_fallback);
    assert!(
        codes(&found.errors, REFUSED_FOLLOW_UP)
            .iter()
            .any(|finding| finding.action == "Tell" && finding.principal == "outside"),
        "{:?}",
        found.errors
    );
    let member = r#"
permit(principal, action == Action::"Start", resource is Round);
permit(principal is Customer, action == Action::"Tell", resource is Round);
"#;
    let found = report(spec, member);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
}

#[test]
fn a_scheduled_follow_up_keeps_the_callers_principal() {
    let spec = r#"
[automaton]
name = "Round"
states = ["Open", "Waiting", "Told"]
initial = "Open"

[[action]]
name = "Start"
kind = "input"
from = ["Open"]
to = "Waiting"
effect = ["schedule('Ping', 5)"]

[[action]]
name = "Ping"
from = ["Waiting"]
to = "Waiting"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Round"
target_action = "Tell"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Tell"
from = ["Waiting"]
to = "Told"
"#;
    let policy = r#"
permit(principal, action == Action::"Start", resource is Round);
permit(principal == Agent::"service:wasm-runtime", action == Action::"Tell", resource is Round);
"#;
    let found = report(spec, policy);
    assert!(
        codes(&found.errors, REFUSED_FOLLOW_UP)
            .iter()
            .any(|finding| finding.action == "Tell" && finding.principal == "outside"),
        "{:?}",
        found.errors
    );
}
