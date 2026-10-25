use super::*;
use crate::entity_actor::{EntityEvent, EntityState};
use temper_authz::{MoveCause, TimerArm};

#[test]
fn previous_release_timer_row_recovers_its_cause() {
    let identity = serde_json::json!({
        "security_ctx": SecurityContext::from_verified_jwt(
            "hook-1", temper_authz::PrincipalKind::Agent,
            Some("katagami-hook"), Some("owner-1"), None, None),
        "agent_id": "hook-1"
    });
    let mut identity = identity;
    identity["security_ctx"]
        .as_object_mut()
        .unwrap()
        .remove("cause");
    let row = StateTimerRecord {
        tenant: "t".into(),
        entity_type: "Ticket".into(),
        entity_id: "e1".into(),
        state: "Open".into(),
        arm_id: "old-arm".into(),
        sequence_nr: 1,
        started_at: "2026-10-24T00:00:00Z".into(),
        params: "{}".into(),
        armed_by: identity.to_string(),
    };
    let mut state: EntityState = serde_json::from_value(serde_json::json!({
        "entity_type": "Ticket", "entity_id": "e1", "status": "Open",
        "item_count": 0, "fields": {}
    }))
    .unwrap();
    state.sequence_nr = 1;
    state.events.push_back(
        serde_json::from_value::<EntityEvent>(serde_json::json!({
            "action": "Park", "from_status": "New", "to_status": "Open",
            "timestamp": row.started_at, "params": {}
        }))
        .unwrap(),
    );
    // Later moves must not replace the action at the row's arm sequence.
    state.events.push_back(
        serde_json::from_value::<EntityEvent>(serde_json::json!({
            "action": "Later", "from_status": "Open", "to_status": "Open",
            "timestamp": row.started_at, "params": {}
        }))
        .unwrap(),
    );
    state.sequence_nr = 2;
    for retained in [true, false] {
        if !retained {
            state.events.clear();
        }
        let mut timer = armed_timer(&TenantId::new("t"), &row, "Lapse", 1).unwrap();
        timer.normalize_cause(&state);
        let security = timer.agent_ctx.security_ctx.as_ref().unwrap();
        assert_eq!(security.principal.id, "hook-1");
        assert_eq!(
            security.cause,
            Some(MoveCause::timer_fire(TimerArm {
                armed_by: "hook-1".into(),
                armed_action: retained.then(|| "Park".into()),
            }))
        );
        assert_eq!(timer.arm_id, "old-arm");
        // A saved arm survives normalization even with a different retained move.
        let saved = MoveCause::timer_fire(TimerArm {
            armed_by: "original-hook".into(),
            armed_action: Some("Original".into()),
        });
        timer.agent_ctx.security_ctx.as_mut().unwrap().cause = Some(saved.clone());
        timer.normalize_cause(&state);
        assert_eq!(timer.agent_ctx.security_ctx.unwrap().cause, Some(saved));
    }
}

#[tokio::test(start_paused = true)]
async fn a_previous_release_row_fires_with_timer_provenance() {
    use crate::registry::SpecRegistry;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    let csdl = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");
    let ioa = r#"
[automaton]
name = "Ticket"
states = ["New", "Open", "InProgress", "Followed"]
initial = "New"
allow_indefinite_states = ["New", "InProgress", "Followed"]
[[action]]
name = "Park"
kind = "input"
from = ["New"]
to = "Open"
[[action]]
name = "AssignAgent"
kind = "internal"
from = ["Open"]
to = "InProgress"
[[action.triggers]]
name = "follows"
kind = "entity"
target_entity = "Ticket"
target_action = "FollowUp"
resolve_target = { kind = "same_id" }
[[action]]
name = "FollowUp"
kind = "input"
from = ["InProgress"]
to = "Followed"
[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "AssignAgent"
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "t",
        parse_csdl(csdl).unwrap(),
        csdl.into(),
        &[("Ticket", ioa)],
    );
    let mut server = ServerState::from_registry(ActorSystem::new("old-row-fire"), registry);
    server
        .authz
        .reload_tenant_policies(
            "t",
            r#"
permit(principal, action, resource);
forbid(principal, action == Action::"FollowUp", resource)
unless {
context has via && context.via == "timer" &&
context has armedBy && context.armedBy == "hook-1" &&
context has armedAction && context.armedAction == "Park" &&
context has rule && context.rule == "Ticket:AssignAgent:follows" &&
context has causeAction && context.causeAction == "AssignAgent" &&
context has depth && context.depth == 1
};
"#,
        )
        .unwrap();
    server.rebuild_reaction_dispatcher();
    let tenant = TenantId::new("t");
    server
        .get_or_create_tenant_entity(&tenant, "Ticket", "e1", serde_json::json!({}))
        .await
        .unwrap();
    let mut security = serde_json::to_value(SecurityContext::from_verified_jwt(
        "hook-1",
        temper_authz::PrincipalKind::Agent,
        Some("katagami-hook"),
        Some("owner-1"),
        None,
        None,
    ))
    .unwrap();
    security.as_object_mut().unwrap().remove("cause");
    let agent = AgentContext {
        security_ctx: Some(serde_json::from_value(security.clone()).unwrap()),
        agent_id: Some("hook-1".into()),
        ..AgentContext::default()
    };
    let current = server
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "e1",
            "Park",
            serde_json::json!({}),
            &agent,
        )
        .await
        .unwrap();
    let row = StateTimerRecord {
        tenant: "t".into(),
        entity_type: "Ticket".into(),
        entity_id: "e1".into(),
        state: "Open".into(),
        arm_id: "previous-release".into(),
        sequence_nr: current.state.sequence_nr,
        started_at: sim_now().to_rfc3339(),
        params: "{}".into(),
        armed_by: serde_json::json!({"security_ctx": security, "agent_id": "hook-1"}).to_string(),
    };
    struct Rows(std::sync::Mutex<Vec<StateTimerRecord>>);
    #[async_trait::async_trait]
    impl StateTimerStore for Rows {
        async fn upsert_state_timer(
            &self,
            row: &StateTimerRecord,
        ) -> Result<(), temper_runtime::persistence::PersistenceError> {
            self.0.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn delete_state_timer(
            &self,
            _: &str,
            _: &str,
            _: &str,
            arm: &str,
        ) -> Result<(), temper_runtime::persistence::PersistenceError> {
            self.0.lock().unwrap().retain(|row| row.arm_id != arm);
            Ok(())
        }
        async fn list_state_timers(
            &self,
            _: &str,
        ) -> Result<Vec<StateTimerRecord>, temper_runtime::persistence::PersistenceError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }
    server.state_timer_rows = Some(Arc::new(Rows(std::sync::Mutex::new(vec![row]))));
    server.rearm_state_timers().await.await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        let result = server
            .get_tenant_entity_state(&tenant, "Ticket", "e1")
            .await
            .unwrap();
        if result.state.status == "Followed" {
            let event = result
                .state
                .events
                .iter()
                .find(|e| e.action == "AssignAgent")
                .unwrap();
            assert_eq!(event.made_by.as_ref().unwrap().id, "hook-1");
            let cause = event
                .cause
                .as_ref()
                .expect("recovered timer must journal its cause");
            assert_eq!(cause.via(), "timer");
            assert_eq!(cause.depth, 0);
            assert_eq!(cause.timer.as_ref().unwrap().armed_by, "hook-1");
            assert_eq!(
                cause.timer.as_ref().unwrap().armed_action.as_deref(),
                Some("Park")
            );
            let follow = result.state.events.back().unwrap();
            assert_eq!(follow.action, "FollowUp");
            assert_eq!(follow.made_by.as_ref().unwrap().id, "hook-1");
            assert_eq!(follow.cause.as_ref().unwrap().via(), "timer");
            return;
        }
    }
    panic!("recovered timer or its timer-only reaction never landed");
}
