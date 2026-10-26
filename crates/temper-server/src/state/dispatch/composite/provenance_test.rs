//! A composite's parent audit record says who made the move and the dispatch
//! path that carried it, as its sub-write events do, and the parent's history
//! returns it with both.
use temper_authz::SecurityContext;

use super::*;
use crate::entity_actor::MoveSource;
use crate::entity_history::{HistoryPageRequest, read_history_page};

const PARENT: &str = "parent-provenance";
const CHILD: &str = "child-provenance";

async fn history(state: &ServerState, entity_type: &str, entity_id: &str) -> Vec<Value> {
    let request = HistoryPageRequest {
        after: 0,
        limit: 10,
        requested_limit: 10,
    };
    let page = read_history_page(state, &TenantId::default(), entity_type, entity_id, request)
        .await
        .unwrap_or_else(|error| panic!("history of {entity_type}:{entity_id}: {error:?}"));
    page["events"].as_array().expect("history events").clone()
}

fn event<'a>(events: &'a [Value], action: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["action"] == action)
        .unwrap_or_else(|| panic!("no {action} event in {events:?}"))
}

#[tokio::test]
async fn composite_parent_event_records_its_maker_and_history_returns_it() {
    let store = SimEventStore::no_faults(41);
    let state = composite_test_state_with_store(store);
    let agent = AgentContext {
        security_ctx: Some(SecurityContext::from_resolved_identity(
            "agent-1", "operator", None,
        )),
        agent_id: Some("agent-1".to_string()),
        ..Default::default()
    }
    .with_source(MoveSource::Http);
    let agent = super::super::wasm::agent_ctx_for_composite_wasm_result(&agent, None);

    let applied = state
        .apply_composite_integration_result(
            &TenantId::default(),
            "Parent",
            PARENT,
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": CHILD,
                    "action": "Create",
                    "params": { "Name": "made by agent-1" }
                }]
            }),
            &agent,
        )
        .await
        .expect("composite result should apply");
    assert!(applied);

    let child_events = history(&state, "Child", CHILD).await;
    let child = event(&child_events, "Create");
    assert_eq!(child["made_by"]["id"], "agent-1", "{child:?}");
    assert_eq!(child["source"], "module", "{child:?}");

    let parent_events = history(&state, "Parent", PARENT).await;
    let composite = event(&parent_events, "CreateChild");
    assert_eq!(composite["made_by"], child["made_by"], "{composite:?}");
    assert_eq!(composite["source"], child["source"], "{composite:?}");
    assert_eq!(
        composite["sub_writes"][0]["entity_id"], CHILD,
        "{composite:?}"
    );
}
