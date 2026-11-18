//! DST: search by meaning (ADR-0186).
//!
//! Records are written through the server's real write path, so the store's
//! commit writes each record's meaning row. Then the meaning worker's pass makes
//! the vectors, and rankings read them back. Invariants, under every seed:
//!
//! - a ranking is identical under every seed, and no live record is left waiting;
//! - a vector made from text a record no longer holds is never kept;
//! - a deleted record has no meaning row and is never ranked.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{meaning_input, text_hash};
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::meaning::rank::{rank_nearest, rank_search, search_words};
use temper_server::meaning::worker::fill_meaning;
use temper_server::meaning::{MeaningKind, MeaningModel, WordCountModel};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::BoxedEventStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const NOTE_IOA: &str = include_str!("../../../test-fixtures/specs/noted_item.ioa.toml");
const NUM_SEEDS: u64 = 100;
const TYPE: &str = "NotedItem";
const TENANT: &str = "default";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="NotedItem">
        <Key>
          <PropertyRef Name="Id"/>
        </Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Title" Type="Edm.String"/>
        <Property Name="Body" Type="Edm.String"/>
        <Property Name="Category" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="NotedItems" EntityType="Temper.Test.NotedItem"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

/// The fixed corpus every seed writes: id, title, body.
const CORPUS: [(&str, &str, &str); 6] = [
    ("note-a", "ferry to the coast", "morning crossing"),
    ("note-b", "ferry timetable", "coast times"),
    ("note-c", "grocery list", "milk butter"),
    ("note-d", "flour butter recipe", "milk flour"),
    ("note-e", "coast walk", "ferry"),
    ("note-f", "jacket and boots", "rain"),
];

/// A server over a sim store, and a handle on the same store.
fn build(seed: u64) -> (ServerState, BoxedEventStore) {
    let sim = SimEventStore::no_faults(seed);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(TENANT, csdl, CSDL_XML.to_string(), &[(TYPE, NOTE_IOA)]);
    let mut state = ServerState::from_registry(ActorSystem::new("dst-meaning"), registry);
    state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
    (state, BoxedEventStore::new(sim))
}

async fn act(state: &ServerState, id: &str, action: &str, params: serde_json::Value) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::from(TENANT),
            TYPE,
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .expect("dispatch");
    assert!(
        response.success,
        "{action} {id} failed: {:?}",
        response.error
    );
}

async fn create(state: &ServerState, id: &str, title: &str, body: &str) {
    act(
        state,
        id,
        "Create",
        serde_json::json!({ "Title": title, "Body": body }),
    )
    .await;
}

async fn create_corpus(state: &ServerState) {
    for (id, title, body) in CORPUS {
        create(state, id, title, body).await;
    }
}

async fn query_vector(model: &WordCountModel, search: &str) -> Vec<f32> {
    let mut vectors = model
        .embed(&[search.to_string()], MeaningKind::Query)
        .await
        .expect("embed query");
    vectors.swap_remove(0)
}

fn ids(ranked: &[temper_server::meaning::rank::Ranked]) -> Vec<&str> {
    ranked.iter().map(|r| r.entity_id.as_str()).collect()
}

/// Whether `ranked` is in rank order: records holding every word first, then by
/// score descending, then by id.
fn assert_rank_order(ranked: &[temper_server::meaning::rank::Ranked], seed: u64) {
    for pair in ranked.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let in_order = (a.words && !b.words)
            || (a.words == b.words
                && match (a.score, b.score) {
                    (Some(x), Some(y)) => x > y || (x == y && a.entity_id < b.entity_id),
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => a.entity_id < b.entity_id,
                });
        assert!(
            in_order,
            "seed {seed}: {} before {} is out of rank order",
            a.entity_id, b.entity_id
        );
    }
}

#[tokio::test]
async fn dst_ranking_is_reproducible_and_nothing_waits() {
    let model = WordCountModel;
    let mut first: Option<(Vec<String>, Vec<String>)> = None;
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let (state, store) = build(seed);
        create_corpus(&state).await;

        // Every record's row waits until the worker's pass.
        let waiting = store
            .meaning_pending(TENANT, WordCountModel::TAG, 100)
            .await
            .expect("pending");
        assert_eq!(
            waiting.len(),
            CORPUS.len(),
            "seed {seed}: rows wait for the model"
        );
        let settled = fill_meaning(&state, &model).await.expect("fill");
        assert_eq!(
            settled,
            CORPUS.len(),
            "seed {seed}: the pass settles every row"
        );

        let rows = store
            .meaning_rows(TENANT, TYPE, None, 100)
            .await
            .expect("rows");
        assert_eq!(
            rows.len(),
            CORPUS.len(),
            "seed {seed}: one row per live record"
        );
        for row in &rows {
            assert_eq!(
                row.model_tag,
                WordCountModel::TAG,
                "seed {seed}: {}",
                row.entity_id
            );
            assert!(
                row.vector.is_some(),
                "seed {seed}: {} has no vector",
                row.entity_id
            );
        }
        assert!(
            store
                .meaning_pending(TENANT, WordCountModel::TAG, 100)
                .await
                .expect("pending")
                .is_empty(),
            "seed {seed}: no row is left pending"
        );

        // Search: records holding both words first (closest meaning first), then the
        // rest by closeness, then id.
        let words = search_words("ferry coast");
        let query = query_vector(&model, "ferry coast").await;
        let ranked = rank_search(
            &store,
            TENANT,
            TYPE,
            &words,
            Some((query.as_slice(), WordCountModel::TAG)),
        )
        .await
        .expect("search");
        assert_eq!(
            ranked.len(),
            CORPUS.len(),
            "seed {seed}: every record ranks"
        );
        assert_rank_order(&ranked, seed);
        let mut holding: Vec<&str> = ranked
            .iter()
            .filter(|r| r.words)
            .map(|r| r.entity_id.as_str())
            .collect();
        holding.sort();
        assert_eq!(
            holding,
            ["note-a", "note-b", "note-e"],
            "seed {seed}: word holders"
        );

        // Nearest: the reference's neighbours, never the reference.
        let reference = store
            .meaning_row(TENANT, TYPE, "note-c")
            .await
            .expect("row")
            .expect("note-c has a row");
        let vector = reference.vector.expect("note-c vector");
        let nearest = rank_nearest(
            &store,
            TENANT,
            TYPE,
            &vector,
            &reference.model_tag,
            "note-c",
        )
        .await
        .expect("nearest");
        assert_eq!(
            nearest.len(),
            CORPUS.len() - 1,
            "seed {seed}: every other record"
        );
        assert!(
            !ids(&nearest).contains(&"note-c"),
            "seed {seed}: the reference ranked"
        );
        assert_rank_order(&nearest, seed);

        // The same order under every seed.
        let orders = (
            ranked
                .iter()
                .map(|r| r.entity_id.clone())
                .collect::<Vec<_>>(),
            nearest
                .iter()
                .map(|r| r.entity_id.clone())
                .collect::<Vec<_>>(),
        );
        match &first {
            None => first = Some(orders),
            Some(expected) => assert_eq!(&orders, expected, "seed {seed}: the ranking moved"),
        }
    }
}

#[tokio::test]
async fn dst_a_stale_vector_never_survives() {
    let model = WordCountModel;
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let (state, store) = build(seed);
        create(&state, "by-action", "alpha beta", "gamma").await;
        create(&state, "by-field", "alpha beta", "gamma").await;

        // The model is mid-call with these texts...
        let pending = store
            .meaning_pending(TENANT, WordCountModel::TAG, 100)
            .await
            .expect("pending");
        assert_eq!(pending.len(), 2, "seed {seed}");
        let old_vector = query_vector(&model, "alpha beta gamma").await;

        // ...when both records change their text: one by an action, one by a field update.
        act(
            &state,
            "by-action",
            "Edit",
            serde_json::json!({ "Title": "delta epsilon", "Body": "zeta" }),
        )
        .await;
        state
            .update_tenant_entity_fields(
                &TenantId::from(TENANT),
                TYPE,
                "by-field",
                serde_json::json!({ "Title": "delta epsilon", "Body": "zeta" }),
                false,
            )
            .await
            .expect("field update");

        for row in &pending {
            let kept = store
                .set_meaning_vector(
                    TENANT,
                    &row.entity_type,
                    &row.entity_id,
                    &row.text_hash,
                    WordCountModel::TAG,
                    &old_vector,
                )
                .await
                .expect("set");
            assert!(
                !kept,
                "seed {seed}: {} kept a vector of its old text",
                row.entity_id
            );
        }

        // Both wait again, under their new text's hash.
        let again = store
            .meaning_pending(TENANT, WordCountModel::TAG, 100)
            .await
            .expect("pending");
        assert_eq!(again.len(), 2, "seed {seed}: both wait again");
        for row in &again {
            assert!(!row.text.contains("alpha"), "seed {seed}: {}", row.text);
            assert!(row.text.contains("delta"), "seed {seed}: {}", row.text);
            assert_eq!(row.text_hash, text_hash(&row.text), "seed {seed}");
            assert!(
                pending.iter().all(|old| old.text_hash != row.text_hash),
                "seed {seed}: the hash follows the text"
            );
        }

        // The next pass gives each the vector of its new text.
        fill_meaning(&state, &model).await.expect("fill");
        for id in ["by-action", "by-field"] {
            let row = store
                .meaning_row(TENANT, TYPE, id)
                .await
                .expect("row")
                .expect("row exists");
            let expected = model
                .embed(&[meaning_input(&row.text)], MeaningKind::Passage)
                .await
                .expect("embed")
                .swap_remove(0);
            assert_eq!(
                row.vector,
                Some(expected),
                "seed {seed}: {id} holds its new text's vector"
            );
            assert_ne!(row.vector, Some(old_vector.clone()), "seed {seed}: {id}");
        }
    }
}

#[tokio::test]
async fn dst_a_deleted_record_is_never_ranked() {
    let model = WordCountModel;
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let (state, store) = build(seed);
        create_corpus(&state).await;
        fill_meaning(&state, &model).await.expect("fill");

        act(&state, "note-e", "Delete", serde_json::json!({})).await;
        // Delete a record that was never made into a vector, too.
        create(&state, "note-g", "ferry coast", "").await;
        act(&state, "note-g", "Delete", serde_json::json!({})).await;

        for id in ["note-e", "note-g"] {
            assert!(
                store
                    .meaning_row(TENANT, TYPE, id)
                    .await
                    .expect("row")
                    .is_none(),
                "seed {seed}: {id} keeps no meaning row"
            );
        }
        let rows = store
            .meaning_rows(TENANT, TYPE, None, 100)
            .await
            .expect("rows");
        assert_eq!(rows.len(), CORPUS.len() - 1, "seed {seed}");
        assert!(
            store
                .meaning_pending(TENANT, WordCountModel::TAG, 100)
                .await
                .expect("pending")
                .is_empty(),
            "seed {seed}: a deleted record does not wait for the model"
        );

        let query = query_vector(&model, "ferry coast").await;
        let ranked = rank_search(
            &store,
            TENANT,
            TYPE,
            &search_words("ferry coast"),
            Some((query.as_slice(), WordCountModel::TAG)),
        )
        .await
        .expect("search");
        assert_eq!(
            ranked.len(),
            CORPUS.len() - 1,
            "seed {seed}: the live records rank"
        );
        assert!(
            !ids(&ranked).contains(&"note-e"),
            "seed {seed}: the deleted record ranked"
        );
        assert_rank_order(&ranked, seed);

        let reference = store
            .meaning_row(TENANT, TYPE, "note-c")
            .await
            .expect("row")
            .expect("note-c has a row");
        let nearest = rank_nearest(
            &store,
            TENANT,
            TYPE,
            &reference.vector.expect("vector"),
            &reference.model_tag,
            "note-c",
        )
        .await
        .expect("nearest");
        assert!(
            ids(&nearest)
                .iter()
                .all(|id| *id != "note-e" && *id != "note-g"),
            "seed {seed}: nearest ranked a deleted record: {:?}",
            ids(&nearest)
        );
    }
}
