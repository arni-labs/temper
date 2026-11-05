//! A walk that held a move back walks again with more in flight only when
//! that move can change what the rule reads (`model/cone.rs`).

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// An action of `kind` from `from` to `to` that sends each of `sends`, a
/// (type, action), to the record with its own id.
fn action(name: &str, kind: &str, (from, to): (&str, &str), sends: &[(&str, &str)]) -> String {
    let mut text = format!(
        "\n[[action]]\nname = \"{name}\"\nkind = \"{kind}\"\nfrom = [\"{from}\"]\nto = \"{to}\"\n"
    );
    for (at, (entity, target)) in sends.iter().enumerate() {
        text += &format!(
            "\n[[action.triggers]]\nname = \"{name}_{at}\"\nkind = \"entity\"\n\
             target_entity = \"{entity}\"\ntarget_action = \"{target}\"\n\
             resolve_target = {{ kind = \"same_id\" }}\n"
        );
    }
    text
}

/// The type `name`, starting in the first of `states`, with `body`.
fn automaton(name: &str, states: &[&str], body: &str) -> Automaton {
    let listed: Vec<String> = states.iter().map(|s| format!("\"{s}\"")).collect();
    let source = format!(
        "[automaton]\nname = \"{name}\"\nstates = [{}]\ninitial = \"{}\"\n{body}",
        listed.join(", "),
        states[0]
    );
    parse_automaton(&source).expect("fixture parses")
}

/// `B` steps from S0 to All by P1 to P4, each from the one before, and
/// holds `rules`.
fn steps(rules: &str) -> Automaton {
    let ways = [("S0", "S1"), ("S1", "S2"), ("S2", "S3"), ("S3", "All")];
    let mut body: String = ways
        .iter()
        .enumerate()
        .map(|(at, way)| action(&format!("P{}", at + 1), "internal", *way, &[]))
        .collect();
    body += rules;
    automaton("B", &["S0", "S1", "S2", "S3", "All"], &body)
}

/// Only `starts` may be sent from outside; everything else only as a reaction.
fn policies(starts: &str) -> SendAnalysis {
    let text = format!(
        "permit(principal, action in [{starts}], resource);\n\
         permit(principal, action, resource) when {{ context has via && context.via != \"request\" }};"
    );
    SendAnalysis::new(&text).expect("fixture policies parse")
}

fn invariant(automatons: &[Automaton], policies: &SendAnalysis) -> JointInvariantResult {
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(policies), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn liveness(automatons: &[Automaton], policies: &SendAnalysis) -> JointLivenessResult {
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, Some(policies), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

const SENDS: [(&str, &str); 4] = [("B", "P1"), ("B", "P2"), ("B", "P3"), ("B", "P4")];

#[test]
fn a_held_move_that_writes_what_the_rule_reads_still_grows() {
    // Go moves A, which the rule reads, and sends all four steps at once:
    // with three in flight it is held back, and it matters.
    let a = automaton(
        "A",
        &["Idle", "Went"],
        &action("Go", "input", ("Idle", "Went"), &SENDS),
    );
    let rule = "\n[[invariant]]\nname = \"never_all\"\n\
                assert = \"status != 'All' || A[Id].status == 'Idle'\"\n";
    let result = invariant(&[a, steps(rule)], &policies("Action::\"Go\""));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert!(!result.held_aside, "{result}");
}

/// `A` goes once, which finishes `B`, and shouts as often as anyone likes,
/// each time sending `B` four ticks that change nothing it reads.
fn shouting() -> Automaton {
    let body = action("Go", "input", ("Idle", "Went"), &[("B", "Finish")])
        + &action("Shout", "input", ("Idle", "Idle"), &[("B", "Tick"); 4]);
    automaton("A", &["Idle", "Went"], &body)
}

fn ticking(rules: &str) -> Automaton {
    let body = action("Finish", "internal", ("Open", "Done"), &[])
        + &action("Tick", "internal", ("Open", "Open"), &[])
        + rules;
    automaton("B", &["Open", "Done"], &body)
}

#[test]
fn a_rule_whose_held_moves_cannot_reach_it_does_not_grow() {
    // Shout never fits three in flight. Before, the walk grew with each
    // Shout until its room ran out; the ticks change nothing the rule reads.
    let rule = "\n[[invariant]]\nname = \"done_after_go\"\n\
                assert = \"status != 'Done' || A[Id].status == 'Went'\"\n";
    let starts = "Action::\"Go\", Action::\"Shout\"";
    let result = invariant(&[shouting(), ticking(rule)], &policies(starts));
    assert!(result.holds(), "{result}");
    assert_eq!(
        result.in_flight_bound,
        Some(model::FIRST_IN_FLIGHT),
        "{result}"
    );
    assert!(result.held_aside, "{result}");
    assert!(
        result
            .to_string()
            .contains("cannot change what the rule reads"),
        "{result}"
    );
}

#[test]
fn a_condition_whose_held_moves_cannot_reach_it_does_not_grow() {
    // The same for a liveness rule: the ticks in flight do not keep the
    // condition from being reached, and the walk does not grow for them.
    let rule = "\n[[liveness]]\nname = \"open_or_went\"\n\
                eventually = \"status == 'Open' || A[Id].status == 'Went'\"\n";
    let starts = "Action::\"Go\", Action::\"Shout\"";
    let result = liveness(&[shouting(), ticking(rule)], &policies(starts));
    assert!(result.holds(), "{result}");
    assert_eq!(
        result.in_flight_bound,
        Some(model::FIRST_IN_FLIGHT),
        "{result}"
    );
    assert!(result.held_aside, "{result}");
}

#[test]
fn a_held_move_two_reactions_from_what_the_rule_reads_still_grows() {
    // Go and M's moves leave every status as it is, but each of M's moves
    // sends one of B's steps: Go, held back with three in flight, reaches
    // the rule through two reactions.
    let relays = [("M", "Q1"), ("M", "Q2"), ("M", "Q3"), ("M", "Q4")];
    let a = automaton(
        "A",
        &["Idle", "Gone"],
        &action("Go", "input", ("Idle", "Idle"), &relays),
    );
    let m_body: String = SENDS
        .iter()
        .enumerate()
        .map(|(at, send)| {
            let name = format!("Q{}", at + 1);
            action(&name, "internal", ("Calm", "Calm"), &[*send])
        })
        .collect();
    let m = automaton("M", &["Calm", "Upset"], &m_body);
    let rule = "\n[[invariant]]\nname = \"never_all\"\n\
                assert = \"status != 'All' || A[Id].status == 'Gone' || M[Id].status == 'Upset'\"\n";
    let result = invariant(&[a, m, steps(rule)], &policies("Action::\"Go\""));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn a_module_answer_nothing_else_sends_counts_as_reaching_the_rule() {
    // B's call names no answer, so the module may answer with Halt, which
    // nothing else may send. Go, held back with three Hits in flight, starts
    // that call: it can reach the rule, and the walk may not hold it aside.
    let a = automaton(
        "A",
        &["Idle"],
        "\n[[state]]\nname = \"sent\"\ntype = \"counter\"\ninitial = 0\n\
         \n[[action]]\nname = \"Send\"\nkind = \"input\"\nfrom = [\"Idle\"]\nto = \"Idle\"\n\
         guard = \"sent < 3\"\neffect = [\"sent += 1\"]\n\
         \n[[action.triggers]]\nname = \"hit\"\nkind = \"entity\"\ntarget_entity = \"B\"\n\
         target_action = \"Hit\"\nresolve_target = { kind = \"same_id\" }\n\
         \n[[action]]\nname = \"Go\"\nkind = \"input\"\nfrom = [\"Idle\"]\nto = \"Idle\"\n\
         guard = \"sent == 3\"\n\
         \n[[action.triggers]]\nname = \"start\"\nkind = \"entity\"\ntarget_entity = \"B\"\n\
         target_action = \"Start\"\nresolve_target = { kind = \"same_id\" }\n\
         \n[[invariant]]\nname = \"not_all_refused\"\n\
         assert = \"sent != 3 || B[Id].hits != 0 || B[Id].status != 'Stop'\"\n",
    );
    let b = automaton(
        "B",
        &["Run", "Stop"],
        "\n[[state]]\nname = \"hits\"\ntype = \"counter\"\ninitial = 0\n\
         \n[[action]]\nname = \"Hit\"\nkind = \"input\"\nfrom = [\"Run\"]\nto = \"Run\"\n\
         effect = [\"hits += 1\"]\n\
         \n[[action]]\nname = \"Start\"\nkind = \"input\"\nfrom = [\"Run\"]\nto = \"Run\"\n\
         \n[[action.triggers]]\nname = \"ask\"\nkind = \"wasm\"\nmodule = \"decider\"\n\
         \n[[action]]\nname = \"Halt\"\nkind = \"input\"\nfrom = [\"Run\"]\nto = \"Stop\"\n",
    );
    let starts = "Action::\"Send\", Action::\"Go\"";
    let result = invariant(&[a, b], &policies(starts));
    assert!(!result.holds(), "{result}");
    assert!(!result.held_aside, "{result}");
}

#[test]
fn a_fair_cycle_grows_on_every_move_held_back() {
    // Each Kick sends four Ticks, whose module answers change nothing the
    // condition reads. A fair cycle between A and B with Kicks in flight
    // stays broken: the check counts a delivery held back as one the
    // runtime keeps able to make, so it may not stop at three in flight.
    let kick = action("Kick", "internal", ("A", "A"), &[("Loop", "Tick"); 4])
        .replace("from = [\"A\"]\nto = \"A\"\n", "from = [\"A\", \"B\"]\n");
    let tick = "\n[[action]]\nname = \"Tick\"\nkind = \"internal\"\nfrom = [\"A\", \"B\"]\n\
                \n[[action.triggers]]\nname = \"ask\"\nkind = \"wasm\"\nmodule = \"noter\"\n\
                on_success = \"Noop\"\n\
                \n[[action]]\nname = \"Noop\"\nkind = \"internal\"\nfrom = [\"A\", \"B\"]\n";
    let body = action("Begin", "input", ("Idle", "A"), &[("Loop", "Kick")])
        + &kick
        + tick
        + &action("Go", "internal", ("A", "B"), &[])
        + &action("Back", "internal", ("B", "A"), &[])
        + &action("Exit", "input", ("A", "Out"), &[])
        + "\n[[state_timeout]]\nstate = \"A\"\nafter_seconds = 60\non_timeout = \"Go\"\n\
           \n[[state_timeout]]\nstate = \"B\"\nafter_seconds = 60\non_timeout = \"Back\"\n\
           \n[[liveness]]\nname = \"GetsOut\"\neventually = \"status == 'Idle' || status == 'Out'\"\n\
           relies_on = [\"Exit\"]\nfairness = { weak = [\"Exit\"] }\n";
    let looping = automaton("Loop", &["Idle", "A", "B", "Out"], &body);
    let starts = "Action::\"Begin\", Action::\"Exit\"";
    let result = liveness(&[looping], &policies(starts));
    let fair = result.fair.as_ref().expect("a fairness verdict");
    assert!(
        matches!(fair.outcome, FairOutcome::Broken { .. }),
        "{result}"
    );
}
