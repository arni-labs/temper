//! DST: an entity actor marks a reply `replayed` exactly when an earlier
//! request with the same idempotency key already made the move, and a replay
//! writes nothing. The workload repeats keys from a small pool and crashes
//! the actor between requests: before a crash the actor's reply cache answers
//! a repeat, after it the durable key set rebuilt from the journal does.
//!
//! Real EntityActor + SimEventStore, across many seeds.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::idempotency::IdempotencyCache;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::SimEventStore;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const NUM_SEEDS: u64 = 100;
const STEPS: usize = 24;
const KEY_POOL: u64 = 4;
/// One step in this many crashes the actor instead of sending a request.
const CRASH_ONE_IN: u64 = 5;

/// Seeded xorshift64: the workload is a function of the seed alone.
fn next(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

/// A fresh actor over the journal, with an empty reply cache of its own:
/// what a restarted server runs.
fn spawn(
    store: &BoxedEventStore,
    table: &Arc<RwLock<TransitionTable>>,
    entity_id: &str,
    generation: usize,
) -> (ActorSystem, ActorRef<EntityMsg>) {
    let system = ActorSystem::new("dst-idempotent-replay");
    let actor = EntityActor::with_persistence(
        "Order",
        entity_id,
        table.clone(),
        serde_json::json!({}),
        store.clone(),
        BackendLabel::Sim,
    )
    .with_tenant("default")
    .with_idempotency_cache(Arc::new(IdempotencyCache::new()));
    let actor_ref = system.spawn(actor, format!("{entity_id}-{generation}"));
    (system, actor_ref)
}

async fn add_item(actor_ref: &ActorRef<EntityMsg>, key: &str) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "AddItem".to_string(),
                params: serde_json::json!({}),
                related: BTreeMap::new(),
                idempotency_key: Some(key.to_string()),
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

async fn items(actor_ref: &ActorRef<EntityMsg>) -> usize {
    let response: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("actor should respond");
    response.state.counters.get("items").copied().unwrap_or(0)
}

#[tokio::test]
async fn dst_replayed_iff_the_key_already_moved() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA)));
        let entity_id = format!("ord-{seed}");
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut generation = 0;
        let (mut _system, mut actor_ref) = spawn(&store, &table, &entity_id, generation);
        let mut moved = BTreeSet::new();
        let mut crashes = 0;

        for step in 0..STEPS {
            let roll = next(&mut rng);
            if roll % CRASH_ONE_IN == 0 {
                generation += 1;
                crashes += 1;
                (_system, actor_ref) = spawn(&store, &table, &entity_id, generation);
                continue;
            }
            let key = format!("add-{}", (roll / CRASH_ONE_IN) % KEY_POOL);
            let already_moved = moved.contains(&key);
            let reply = add_item(&actor_ref, &key).await;
            assert!(
                reply.success,
                "seed {seed} step {step}: AddItem {key} failed: {:?}",
                reply.error
            );
            assert_eq!(
                reply.replayed, already_moved,
                "seed {seed} step {step} (after {crashes} crashes): key {key} \
                 replayed must be {already_moved}"
            );
            moved.insert(key);
            assert_eq!(
                items(&actor_ref).await,
                moved.len(),
                "seed {seed} step {step}: each key moves once, a replay writes nothing"
            );
        }
    }
}
