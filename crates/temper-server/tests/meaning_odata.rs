//! Integration test for search by meaning (ADR-0186), driven end to end through the
//! real axum OData router: `$search` on an entity set, the `Temper.Nearest` bound
//! function, and the backfill of records saved before meaning rows existed. The
//! deterministic `WordCountModel` stands in for the kernel's model, and the worker's
//! pass (`fill_meaning`) runs after writes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::TEXT_RULE;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::meaning::rank::{rank_nearest, rank_search, search_words};
use temper_server::meaning::worker::fill_meaning;
use temper_server::meaning::{MeaningKind, MeaningModel, WordCountModel};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::BoxedEventStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const NOTE_IOA: &str = include_str!("../../../test-fixtures/specs/noted_item.ioa.toml");
const TYPE: &str = "NotedItem";

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

const READ_TEST_POLICY: &str = r#"
    permit(
        principal,
        action in [Action::"list", Action::"read"],
        resource is NotedItem
    );
"#;

/// The notes most tests write: id, title, body, category. Against the word-count
/// model, "ferry coast" holds in e, b and a (closest first); c, d and f hold neither
/// word. Nearest to note-c is d, then e, then a, b and f (no overlap, so by id).
const NOTES: [(&str, &str, &str, &str); 6] = [
    ("note-a", "ferry to the coast", "morning crossing", ""),
    ("note-b", "ferry timetable", "coast times", ""),
    ("note-c", "grocery list", "milk butter", ""),
    ("note-d", "flour butter recipe", "milk flour", ""),
    ("note-e", "coast walk", "ferry", ""),
    ("note-f", "jacket and boots", "rain", ""),
];

/// A server over a sim store, with the word-count model installed or not, and a
/// handle on the same store.
fn build_state(with_model: bool) -> (ServerState, BoxedEventStore) {
    let sim = SimEventStore::no_faults(7);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant("default", csdl, CSDL_XML.to_string(), &[(TYPE, NOTE_IOA)]);
    let system = ActorSystem::new("meaning-odata");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), READ_TEST_POLICY)
        .expect("install NotedItem read/list test policy");
    if with_model {
        assert!(state.install_meaning_model(Arc::new(WordCountModel)));
    }
    (state, BoxedEventStore::new(sim))
}

async fn create_note(state: &ServerState, id: &str, title: &str, body: &str, category: &str) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            TYPE,
            id,
            "Create",
            serde_json::json!({ "Title": title, "Body": body, "Category": category }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Create");
    assert!(response.success, "Create {id} failed: {:?}", response.error);
}

async fn create_notes(state: &ServerState) {
    for (id, title, body, category) in NOTES {
        create_note(state, id, title, body, category).await;
    }
}

async fn delete_note(state: &ServerState, id: &str) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            TYPE,
            id,
            "Delete",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Delete");
    assert!(response.success, "Delete {id} failed: {:?}", response.error);
}

/// Run the worker's pass: every waiting row gets its vector.
async fn fill(state: &ServerState) {
    fill_meaning(state, &WordCountModel)
        .await
        .expect("fill meaning");
}

/// A server with the model, the notes written and their vectors made.
async fn ready_state() -> (ServerState, BoxedEventStore) {
    let (state, store) = build_state(true);
    create_notes(&state).await;
    fill(&state).await;
    (state, store)
}

async fn get_json(state: &ServerState, path: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "meaning-odata-test",
                "test-customer",
                None,
            ),
        ));
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn entity_id(entity: &serde_json::Value) -> &str {
    entity["entity_id"]
        .as_str()
        .or_else(|| entity["Id"].as_str())
        .unwrap_or("")
}

/// The kernel's own ranking for a search, read straight from the store: what the
/// HTTP read must return before its filter, read gate and page apply.
async fn expected_search(store: &BoxedEventStore, search: &str) -> Vec<String> {
    let mut query = WordCountModel
        .embed(&[search.to_string()], MeaningKind::Query)
        .await
        .expect("embed");
    let query = query.swap_remove(0);
    rank_search(
        store,
        TenantId::default().as_str(),
        TYPE,
        &search_words(search),
        Some((query.as_slice(), WordCountModel::TAG)),
    )
    .await
    .expect("rank")
    .into_iter()
    .map(|r| r.entity_id)
    .collect()
}

/// The kernel's own nearest ranking to `reference`, read straight from the store.
async fn expected_nearest(store: &BoxedEventStore, reference: &str) -> Vec<String> {
    let tenant = TenantId::default();
    let row = store
        .meaning_row(tenant.as_str(), TYPE, reference)
        .await
        .expect("row")
        .expect("reference row");
    rank_nearest(
        store,
        tenant.as_str(),
        TYPE,
        &row.vector.expect("reference vector"),
        &row.model_tag,
        reference,
    )
    .await
    .expect("rank")
    .into_iter()
    .map(|r| r.entity_id)
    .collect()
}

fn without<'a>(ids: &'a [String], left_out: &[&str]) -> Vec<&'a str> {
    ids.iter()
        .map(String::as_str)
        .filter(|id| !left_out.contains(id))
        .collect()
}

fn ids_of(body: &serde_json::Value) -> Vec<&str> {
    body["value"]
        .as_array()
        .expect("value array")
        .iter()
        .map(entity_id)
        .collect()
}

#[tokio::test]
async fn search_ranks_word_matches_first_then_meaning() {
    let (state, store) = ready_state().await;

    let (status, body) = get_json(&state, "/tdata/NotedItems?$search=ferry%20coast").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let expected = expected_search(&store, "ferry coast").await;
    assert_eq!(expected.len(), NOTES.len(), "every note ranks");
    assert_eq!(
        ids_of(&body),
        without(&expected, &[]),
        "the kernel's ranking"
    );
    let mut holding: Vec<&str> = ids_of(&body)[..3].to_vec();
    holding.sort();
    assert_eq!(
        holding,
        ["note-a", "note-b", "note-e"],
        "the notes holding both words come first"
    );

    let value = body["value"].as_array().unwrap();
    let words: Vec<bool> = value
        .iter()
        .map(|e| e["@temper.words"].as_bool().expect("@temper.words"))
        .collect();
    assert_eq!(words, [true, true, true, false, false, false]);
    let scores: Vec<f64> = value
        .iter()
        .map(|e| e["@temper.score"].as_f64().expect("@temper.score"))
        .collect();
    assert!(
        scores[..3].windows(2).all(|w| w[0] > w[1]),
        "word matches are ordered by closeness: {scores:?}"
    );
}

#[tokio::test]
async fn search_with_select_keeps_the_ranking() {
    let (state, store) = ready_state().await;
    let (status, body) = get_json(
        &state,
        "/tdata/NotedItems?$search=ferry%20coast&$select=Title",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let by_odata_id: Vec<String> = body["value"]
        .as_array()
        .expect("value array")
        .iter()
        .map(|entity| match entity["@odata.id"].as_str() {
            Some(odata_id) => odata_id
                .trim_start_matches("NotedItems('")
                .trim_end_matches("')")
                .to_string(),
            None => entity_id(entity).to_string(),
        })
        .collect();
    let expected = expected_search(&store, "ferry coast").await;
    assert_eq!(
        by_odata_id, expected,
        "a $select read keeps every ranked record: {body}"
    );
    for entity in body["value"].as_array().unwrap() {
        assert!(
            entity.get("Title").is_some(),
            "the selected field: {entity}"
        );
        assert!(
            entity.get("Body").is_none(),
            "only the selected field: {entity}"
        );
    }
}

#[tokio::test]
async fn search_folds_accents_and_case() {
    let (state, _store) = build_state(true);
    create_note(&state, "cafe-1", "Meet at the Café", "Espresso", "").await;
    create_note(&state, "note-c", "grocery list", "milk butter", "").await;
    fill(&state).await;

    for search in ["cafe", "CAFE", "caf%C3%A9"] {
        let (status, body) = get_json(&state, &format!("/tdata/NotedItems?$search={search}")).await;
        assert_eq!(status, StatusCode::OK, "{search}: {body}");
        let first = &body["value"].as_array().expect("value")[0];
        assert_eq!(entity_id(first), "cafe-1", "{search} finds Café first");
        assert_eq!(first["@temper.words"], true, "{search}");
    }
}

#[tokio::test]
async fn search_applies_filter_and_continues_without_repeats() {
    let (state, _store) = build_state(true);
    for (id, category) in [
        ("trip-1", "red"),
        ("trip-2", "blue"),
        ("trip-3", "red"),
        ("trip-4", "blue"),
        ("trip-5", "red"),
    ] {
        create_note(&state, id, "trip", "plan", category).await;
    }
    fill(&state).await;

    let (status, page1) = get_json(
        &state,
        "/tdata/NotedItems?$search=trip&$filter=Category%20eq%20'red'&$top=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {page1}");
    assert_eq!(ids_of(&page1), ["trip-1", "trip-3"]);
    let next = page1["@odata.nextLink"]
        .as_str()
        .expect("a full page carries @odata.nextLink");

    let (status, page2) = get_json(&state, &format!("/tdata/{next}")).await;
    assert_eq!(status, StatusCode::OK, "body: {page2}");
    assert_eq!(ids_of(&page2), ["trip-5"], "the next page, no repeats");
    assert!(
        page2.get("@odata.nextLink").is_none(),
        "the last page has no nextLink: {page2}"
    );
}

#[tokio::test]
async fn search_with_orderby_is_rejected() {
    let (state, _store) = ready_state().await;
    let (status, body) = get_json(&state, "/tdata/NotedItems?$search=ferry&$orderby=Title").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn search_without_a_model_finds_only_word_matches() {
    let (state, _store) = build_state(false);
    create_notes(&state).await;
    assert!(state.meaning_model().is_none());

    let (status, body) = get_json(&state, "/tdata/NotedItems?$search=ferry").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        ids_of(&body),
        ["note-a", "note-b", "note-e"],
        "only records holding the word, in id order"
    );
    for entity in body["value"].as_array().unwrap() {
        assert_eq!(entity["@temper.words"], true);
        assert!(
            entity.get("@temper.score").is_none(),
            "no model, no score: {entity}"
        );
    }
}

#[tokio::test]
async fn nearest_ranks_scores_and_excludes_the_reference() {
    let (state, store) = ready_state().await;
    let expected = expected_nearest(&store, "note-c").await;
    assert_eq!(expected.len(), NOTES.len() - 1, "every other note ranks");
    assert!(!expected.iter().any(|id| id == "note-c"));

    let (status, body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-c',k=10)").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        ids_of(&body),
        without(&expected, &[]),
        "nearest first, the reference left out"
    );
    let scores: Vec<f64> = body["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["@temper.score"].as_f64().expect("score"))
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");

    // k bounds the list; decl and model are accepted and ignored.
    let (status, body) = get_json(
        &state,
        "/tdata/NotedItems/Temper.Nearest(decl='anything',model='m1',to='note-c',k=2)",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(ids_of(&body), without(&expected, &[])[..2]);
}

#[tokio::test]
async fn nearest_excludes_deleted_and_404s_on_deleted_reference() {
    let (state, store) = ready_state().await;
    let expected = expected_nearest(&store, "note-c").await;
    delete_note(&state, "note-d").await;

    let (status, body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-c',k=10)").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        ids_of(&body),
        without(&expected, &["note-d"]),
        "a deleted record is never ranked"
    );

    let (status, _body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-d',k=5)").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "to='<deleted>' must 404");
}

#[tokio::test]
async fn nearest_applies_filter_before_taking_k() {
    let (state, store) = build_state(true);
    for (id, title, body, _) in NOTES {
        let category = if ["note-a", "note-b", "note-f"].contains(&id) {
            "red"
        } else {
            "blue"
        };
        create_note(&state, id, title, body, category).await;
    }
    fill(&state).await;

    let red = ["note-a", "note-b", "note-f"];
    let expected: Vec<String> = expected_nearest(&store, "note-c")
        .await
        .into_iter()
        .filter(|id| red.contains(&id.as_str()))
        .take(2)
        .collect();
    let (status, body) = get_json(
        &state,
        "/tdata/NotedItems/Temper.Nearest(to='note-c',k=2,filter='Category%20eq%20''red''')",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        ids_of(&body),
        without(&expected, &[]),
        "only red records, ranked, k of them"
    );
}

#[tokio::test]
async fn nearest_authorizes_reference_and_ranked_rows() {
    let (state, store) = ready_state().await;
    let expected = expected_nearest(&store, "note-c").await;

    // Permit list + read, but forbid read on note-d specifically.
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"
                permit(principal, action in [Action::"list", Action::"read"], resource is NotedItem);
                forbid(principal, action == Action::"read", resource == NotedItem::"note-d");
            "#,
        )
        .expect("install Cedar policy");

    // A row the caller may not read is skipped, not leaked.
    let (status, body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-c',k=10)").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(ids_of(&body), without(&expected, &["note-d"]));

    // Reading a forbidden record as the reference is denied, not disclosed.
    let (status, _body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-d',k=5)").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "reading a forbidden reference record must be denied"
    );
}

#[tokio::test]
async fn nearest_rejects_system_query_options_and_vector() {
    let (state, _store) = ready_state().await;

    for option in ["$top=1", "$filter=Category%20eq%20'red'", "$search=ferry"] {
        let (status, body) = get_json(
            &state,
            &format!("/tdata/NotedItems/Temper.Nearest(to='note-c',k=5)?{option}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{option}: {body}");
    }

    // The caller no longer supplies a vector; search by words is $search.
    let (status, body) = get_json(
        &state,
        "/tdata/NotedItems/Temper.Nearest(vector='%5B1,0,0,0%5D',k=1)",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn nearest_rejects_duplicate_named_argument() {
    let (state, _store) = ready_state().await;
    let (status, _body) = get_json(
        &state,
        "/tdata/NotedItems/Temper.Nearest(to='note-c',k=5,k=10)",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "duplicate 'k' must be rejected"
    );
}

#[tokio::test]
async fn nearest_to_a_reference_without_a_vector_is_409() {
    let (state, _store) = build_state(true);
    create_notes(&state).await;
    // No pass has run: no record has a vector yet.
    let (status, body) =
        get_json(&state, "/tdata/NotedItems/Temper.Nearest(to='note-c',k=5)").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn backfill_restores_meaning_rows_of_records_saved_before_them() {
    let (state, store) = ready_state().await;
    let tenant = TenantId::default();

    let before = store
        .meaning_rows(tenant.as_str(), TYPE, None, 100)
        .await
        .expect("rows");
    assert_eq!(before.len(), NOTES.len());

    // A store from before ADR-0186 has the records and no meaning rows.
    for (id, ..) in NOTES {
        store
            .write_meaning(tenant.as_str(), TYPE, id, None)
            .await
            .expect("remove row");
    }
    assert!(
        store
            .meaning_rows(tenant.as_str(), TYPE, None, 100)
            .await
            .expect("rows")
            .is_empty()
    );
    let marked = store
        .meaning_backfilled_types(tenant.as_str())
        .await
        .expect("marks");
    assert!(!marked.contains(&(TYPE.to_string(), TEXT_RULE.to_string())));
    let (_, body) = get_json(&state, "/tdata/NotedItems?$search=ferry").await;
    assert!(
        ids_of(&body).is_empty(),
        "without rows nothing is found: {body}"
    );

    state.populate_meaning_from_snapshots(&tenant).await;

    let after = store
        .meaning_rows(tenant.as_str(), TYPE, None, 100)
        .await
        .expect("rows");
    let texts = |rows: &[temper_runtime::persistence::MeaningRow]| -> Vec<(String, String)> {
        rows.iter()
            .map(|r| (r.entity_id.clone(), r.text.clone()))
            .collect()
    };
    assert_eq!(
        texts(&after),
        texts(&before),
        "the same text as the write path made"
    );
    assert!(
        after.iter().all(|r| r.vector.is_none()),
        "restored rows wait for the model"
    );
    let marked = store
        .meaning_backfilled_types(tenant.as_str())
        .await
        .expect("marks");
    assert!(
        marked.contains(&(TYPE.to_string(), TEXT_RULE.to_string())),
        "the type is marked: {marked:?}"
    );

    // The pass then makes their vectors, and search finds them again.
    fill(&state).await;
    let (status, body) = get_json(&state, "/tdata/NotedItems?$search=ferry%20coast").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let expected = expected_search(&store, "ferry coast").await;
    assert_eq!(expected.len(), NOTES.len());
    assert_eq!(ids_of(&body), without(&expected, &[]));
}
