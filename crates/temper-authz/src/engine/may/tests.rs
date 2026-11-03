use super::*;
use crate::{AuthzEngine, SecurityContext};
use std::collections::HashMap;

#[test]
fn cedar_set_equality_denies_the_same_reaction_as_the_real_engine() {
    for (expression, denied) in [
        ("[1, 2] == [2, 1]", true),
        ("[1, 2, 1] == [2, 1]", true),
        ("[[1, 2], [3]] == [[3, 3], [2, 1, 1]]", true),
        ("[1, 2] != [2, 1]", false),
        ("[1, 2] == [1, 3]", false),
        ("[] == []", true),
    ] {
        let policies = format!(
            r#"permit(principal, action, resource);
            forbid(principal, action == Action::"Bring", resource is Place)
            when {{ {expression} }};"#
        );
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        let asker = Asker::Service {
            name: "place".into(),
            context: BTreeMap::new(),
        };
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        let context = SecurityContext::from_resolved_identity("service:place", "place", None);
        let decision = engine.authorize(&context, "Bring", "Place", &HashMap::new());
        assert_eq!(decision.is_allowed(), !denied, "{expression}: {decision:?}");
        assert_eq!(
            analysis.may(&asker, "Bring", "Place", &BTreeMap::new(), None),
            if denied { May::No } else { May::Yes },
            "{expression}"
        );
    }
}

#[test]
fn a_known_cause_is_read_as_the_engine_reads_it() {
    use crate::{MoveCause, ReactionHop, TimerArm};
    let hop = || ReactionHop {
        rule: "Place:Ask:bring".into(),
        action: "Ask".into(),
        entity_type: "Place".into(),
        entity_id: "p1".into(),
    };
    let timer = MoveCause::timer_fire(TimerArm {
        armed_by: "someone".into(),
        state_timer_of: None,
    });
    let causes = [
        (None, Cause::request()),
        (
            Some(MoveCause::reaction(hop(), None)),
            Cause::reaction(false, Hop::unknown()),
        ),
        (
            Some(MoveCause::reaction(hop(), Some(&timer))),
            Cause::reaction(true, Hop::unknown()),
        ),
    ];
    for when in [
        r#"context.via != "request""#,
        r#"context has via && context.via == "reaction""#,
        r#"context.via == "timer" && context has armedBy"#,
        r#"context has rule"#,
    ] {
        let policies =
            format!(r#"permit(principal, action == Action::"Bring", resource) when {{ {when} }};"#);
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        for (move_cause, cause) in &causes {
            let mut context = SecurityContext::from_resolved_identity("someone", "agent", None);
            context.cause = move_cause.clone();
            let allowed = engine
                .authorize(&context, "Bring", "Place", &HashMap::new())
                .is_allowed();
            let may = analysis.may(
                &Asker::Outside,
                "Bring",
                "Place",
                &BTreeMap::new(),
                Some(cause),
            );
            assert_eq!(
                may,
                if allowed { May::Yes } else { May::No },
                "{when}, {cause:?}"
            );
        }
    }
}

#[test]
fn every_outside_principal_is_of_one_kind_that_answers_as_the_engine_does() {
    use crate::{MoveCause, ReactionHop};
    let policies = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && (principal.role == "member" || principal.role like "guest*") };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role like "guest*" };
"#;
    let analysis = SendAnalysis::new(policies).expect("policies parse");
    let engine = AuthzEngine::new(policies).expect("policies parse");
    let questions = [
        Question {
            action: "Add",
            resource_type: "Note",
            cause: Cause::request(),
        },
        Question {
            action: "Address",
            resource_type: "Note",
            cause: Cause::reaction(false, Hop::unknown()),
        },
    ];
    let kinds = analysis.outside_kinds(&questions);
    let answers = |kind: &Kind| -> Vec<May> {
        let asker = Asker::OutsideAs(kind.clone());
        questions
            .iter()
            .map(|q| {
                analysis.may(
                    &asker,
                    q.action,
                    q.resource_type,
                    &BTreeMap::new(),
                    Some(&q.cause),
                )
            })
            .collect()
    };
    // The policies decide both moves for each kind.
    for kind in &kinds {
        assert!(
            !answers(kind).contains(&May::Maybe),
            "{kind}: {:?}",
            answers(kind)
        );
    }
    let hop = ReactionHop {
        rule: "Note:Add:address".into(),
        action: "Add".into(),
        entity_type: "Note".into(),
        entity_id: "n1".into(),
    };
    for role in [
        None,
        Some("member"),
        Some("guest:link"),
        Some("guestbook"),
        Some("visitor"),
    ] {
        let mut context = SecurityContext::from_resolved_identity("someone", "agent", None);
        context.principal.role = role.map(str::to_string);
        let request = engine
            .authorize(&context, "Add", "Note", &HashMap::new())
            .is_allowed();
        context.cause = Some(MoveCause::reaction(hop.clone(), None));
        let reaction = engine
            .authorize(&context, "Address", "Note", &HashMap::new())
            .is_allowed();
        let engine_answers =
            [request, reaction].map(|allowed| if allowed { May::Yes } else { May::No });
        // Its Add and its Address are answered for the same kind.
        assert!(
            kinds.iter().any(|kind| answers(kind) == engine_answers),
            "{role:?}: {engine_answers:?} in none of {:?}",
            kinds.iter().map(answers).collect::<Vec<_>>()
        );
    }
}
