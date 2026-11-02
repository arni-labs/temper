//! A chain names whoever started it, so a lost row's timer fires as them.

use super::*;

/// A record made and moved only by a chain, and who the chain says it ran as.
struct Shipped {
    /// Who the shipment's newest move says it ran as, before the restart.
    moved_by: Value,
    /// Who its timer fired as after losing its row.
    fired_by: Value,
}

/// `by`'s request sets off a reaction whose effect makes the Shipment, its timer
/// row is lost while the server is down, and the server restarts. `owner`, when
/// given, moved the Line first.
async fn ship_and_lose_the_row(owner: Option<AgentContext>, by: AgentContext) -> Shipped {
    let store = SimEventStore::no_faults(7);
    let (shipment, moved_by) = on_a_server_that_stops(&store, 3600, |state| async move {
        if let Some(owner) = owner {
            act(&state, "Line", "line", "Tidy", &owner).await;
        }
        act(&state, "Line", "line", "Merge", &by).await;
        let line = until(&state, "Lines", "line", "shipped", |line| {
            line["fields"]["rollout"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        })
        .await;
        let shipment = line["fields"]["rollout"].as_str().unwrap().to_string();
        let record = until(&state, "Shipments", &shipment, "started", |record| {
            record["status"] == "Staging"
        })
        .await;
        (shipment, record["last_event_by"].clone())
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == shipment)
        .expect("the shipment's timer left a row");
    store.delete_state_timer("default", "Shipment", &shipment, &row.arm_id);

    let state = server("after", &store, 1).await;
    let record = until(&state, "Shipments", &shipment, "fired", |record| {
        record["status"] == "Diagnosed"
    })
    .await;
    Shipped {
        moved_by,
        fired_by: record["fields"]["fired_by"].clone(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_person_started_keeps_a_lost_rows_timer_as_that_person() {
    let shipped = ship_and_lose_the_row(None, person("ann")).await;
    assert_eq!(shipped.moved_by, "ann");
    assert_eq!(shipped.fired_by, "ann");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_an_agent_started_keeps_a_lost_rows_timer_as_that_agent() {
    let shipped = ship_and_lose_the_row(None, agent_for("bea")).await;
    assert_eq!(shipped.moved_by, "agent-1");
    assert_eq!(
        shipped.fired_by, "bea",
        "the action binds whom the agent acts for"
    );
}

/// A guest's chain on a record its owner moved first names the guest, never
/// the owner.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_non_owner_started_names_them_not_the_owner() {
    let shipped = ship_and_lose_the_row(Some(person("owner")), person("guest")).await;
    assert_eq!(shipped.moved_by, "guest");
    assert_eq!(shipped.fired_by, "guest");
}
