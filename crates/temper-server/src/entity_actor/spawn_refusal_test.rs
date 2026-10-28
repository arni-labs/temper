//! Reservation ownership through snapshots and optimistic-concurrency replay.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;

use super::{ChildLookup, SpawnRefusal};
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::storage::{BackendLabel, BoxedEventStore};

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Active"]
initial = "Active"

[[state]]
name = "last_run_id"
type = "string"
initial = ""

[[action]]
name = "Fire"
kind = "input"
from = ["Active"]
params = ["run_id"]
effect = ["spawn('Run', 'Configure', last_run_id, params.run_id)"]

[[action]]
name = "Ping"
kind = "input"
from = ["Active"]
"#;

async fn action(actor: &ActorRef<EntityMsg>, name: &str) -> EntityResponse {
    let reply: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                name: name.to_owned(),
                params: serde_json::json!({"run_id": "reused"}),
                related: Default::default(),
                idempotency_key: None,
                expected_authorization_precondition: None,
                caller: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("action reply");
    assert!(reply.success, "{name}: {:?}", reply.error);
    reply
}

#[tokio::test(start_paused = true)]
async fn dst_spawn_refusal_rechecks_reservation_ownership_after_a_conflict() {
    for seed in 0..8 {
        for reserve_again in [false, true] {
            let (_guard, _clock, _ids) = install_deterministic_context(seed);
            let store = SimEventStore::no_faults(seed);
            let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(JOB)));
            let system = ActorSystem::new("spawn-refusal-conflict");
            let spawn = |name| {
                system.spawn(
                    EntityActor::with_persistence(
                        "Job",
                        "job",
                        table.clone(),
                        serde_json::json!({}),
                        BoxedEventStore::new(store.clone()),
                        BackendLabel::Sim,
                    ),
                    name,
                )
            };
            let stale = spawn("stale");
            // No dispatcher runs here: the actor returns its spawn request,
            // letting us deliver that request's refusal after another writer.
            let first = action(&stale, "Fire").await;
            let request = &first.spawn_requests[0];
            let mut snapshot = serde_json::to_value(&first.state).unwrap();
            snapshot["events"] = serde_json::json!([]);
            snapshot["events_since_snapshot"] = serde_json::json!(0);
            snapshot["last_snapshot_sequence_nr"] = first.state.sequence_nr.into();
            store
                .save_snapshot(
                    "default:Job:job",
                    first.state.sequence_nr,
                    &serde_json::to_vec(&snapshot).unwrap(),
                )
                .await
                .unwrap();

            let newer = spawn("newer");
            if reserve_again {
                let next = action(&newer, "Fire").await;
                assert_ne!(
                    request.reservation_id,
                    next.spawn_requests[0].reservation_id
                );
            }
            // Advancing the parent alone must not invalidate a reservation.
            action(&newer, "Ping").await;
            let refused: EntityResponse = stale
                .ask(
                    EntityMsg::RecordSpawnRefusal {
                        refusal: SpawnRefusal {
                            reservation_id: request.reservation_id.clone(),
                            child_type: request.entity_type.clone(),
                            child_id: request.entity_id.clone(),
                            field: request.store_id_in.clone(),
                            child: ChildLookup::Absent,
                            error: "initializer refused before the newer writer".to_owned(),
                        },
                    },
                    Duration::from_secs(5),
                )
                .await
                .expect("refusal reply");
            assert!(refused.success, "seed {seed}: {:?}", refused.error);
            let cleared = refused.state.events.back().unwrap().params["cleared_field"].is_string();
            assert_eq!(
                cleared, !reserve_again,
                "seed {seed}: reservation ownership after conflict"
            );
            assert_eq!(
                refused.state.spawn_reservations.contains_key("last_run_id"),
                reserve_again
            );
            if reserve_again {
                assert_eq!(refused.state.fields["last_run_id"], "reused", "seed {seed}");
            }
            let replayed: EntityResponse = spawn("replayed")
                .ask(EntityMsg::GetState, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(
                replayed.state.fields, refused.state.fields,
                "seed {seed}: snapshot plus tail"
            );
            assert_eq!(
                replayed.state.spawn_reservations,
                refused.state.spawn_reservations
            );
        }
    }
}
