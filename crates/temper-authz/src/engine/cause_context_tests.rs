use super::*;
use crate::cause::{MOVE_CAUSE_CONTEXT_KEYS, MoveCause, ReactionHop, TimerArm};

#[test]
fn cause_keys_cannot_be_supplied_by_context_or_resource_attributes() {
    let reaction = MoveCause::reaction(
        ReactionHop {
            rule: "Direction:AutoGo:goes".into(),
            action: "FindFix".into(),
            entity_type: "Direction".into(),
            entity_id: "d1".into(),
        },
        None,
    );
    let timer = MoveCause::timer_fire(TimerArm {
        armed_action: None,
        armed_by: "hook-1".into(),
    });
    let cases = [
        (None, vec![("via", "\"request\""), ("depth", "0")]),
        (
            Some(reaction),
            vec![
                ("via", "\"reaction\""),
                ("depth", "1"),
                ("rule", "\"Direction:AutoGo:goes\""),
                ("causeAction", "\"FindFix\""),
                ("causeEntityType", "\"Direction\""),
                ("causeEntityId", "\"d1\""),
            ],
        ),
        (
            Some(timer),
            vec![
                ("via", "\"timer\""),
                ("depth", "0"),
                ("armedBy", "\"hook-1\""),
            ],
        ),
    ];
    for (cause, expected) in cases {
        let mut ctx = SecurityContext::anonymous();
        ctx.cause = cause;
        for key in MOVE_CAUSE_CONTEXT_KEYS {
            ctx.context_attrs
                .insert(key.into(), serde_json::json!("spoofed"));
        }
        ctx.context_attrs
            .insert("custom".into(), serde_json::json!("kept"));
        let mut conditions = vec!["context.custom == \"kept\"".to_string()];
        for key in MOVE_CAUSE_CONTEXT_KEYS {
            conditions.push(match expected.iter().find(|(name, _)| *name == key) {
                Some((_, value)) => format!("context.{key} == {value}"),
                None => format!("!(context has {key})"),
            });
        }
        let engine = AuthzEngine::new(&format!(
            "permit(principal, action, resource) when {{ {} }};",
            conditions.join(" && ")
        ))
        .unwrap();
        let attrs = ctx.context_attrs.clone();
        let decision = engine.authorize(&ctx, "AutoGo", "Direction", &attrs);
        assert!(decision.is_allowed(), "cause {:?}: {decision:?}", ctx.cause);
    }
}
