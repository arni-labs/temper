//! A relation's declared delete policy decides what deleting its target does.
//!
//! The model mirrors a parent that owns its children and also points at one of
//! them: `Track.album_id -> Album` is `cascade` (an album owns its tracks),
//! `Album.latest_track_id -> Track` is `none` (a pointer, not ownership), and
//! `PlaylistEntry.track_id -> Track` keeps the default `restrict`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Music" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Album">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="latest_track_id" Type="Edm.String"/>
        <NavigationProperty Name="LatestTrack" Type="Test.Music.Track">
          <ReferentialConstraint Property="latest_track_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="none"/>
        </NavigationProperty>
      </EntityType>
      <EntityType Name="Track">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="album_id" Type="Edm.String" Nullable="false"/>
        <NavigationProperty Name="Album" Type="Test.Music.Album">
          <ReferentialConstraint Property="album_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>
        </NavigationProperty>
      </EntityType>
      <EntityType Name="PlaylistEntry">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="track_id" Type="Edm.String" Nullable="false"/>
        <NavigationProperty Name="Track" Type="Test.Music.Track">
          <ReferentialConstraint Property="track_id" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="MusicService">
        <EntitySet Name="Albums" EntityType="Test.Music.Album"/>
        <EntitySet Name="Tracks" EntityType="Test.Music.Track"/>
        <EntitySet Name="PlaylistEntries" EntityType="Test.Music.PlaylistEntry"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Each type only needs to exist and keep the fields it was created with.
fn ioa(name: &str) -> String {
    format!("[automaton]\nname = \"{name}\"\nstates = [\"Active\"]\ninitial = \"Active\"\n")
}

struct Store {
    db_path: std::path::PathBuf,
    db_url: String,
}

impl Store {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "temper-delete-cascade-{name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        let db_url = format!("file:{}", db_path.display());
        Self { db_path, db_url }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
    }
}

async fn server(name: &str, store: &Store) -> ServerState {
    let (album, track, entry) = (ioa("Album"), ioa("Track"), ioa("PlaylistEntry"));
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[
            ("Album", album.as_str()),
            ("Track", track.as_str()),
            ("PlaylistEntry", entry.as_str()),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    let store = TursoEventStore::new(&store.db_url, None)
        .await
        .expect("open local turso db");
    state.set_storage_stack(StorageStack::from_turso(store));
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Album", "Track", "PlaylistEntry"] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "L0 SMT".to_string(),
                        passed: true,
                        summary: "OK".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-10-24T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy parses");
    state
}

async fn create(state: &ServerState, entity_type: &str, id: &str, fields: serde_json::Value) {
    state
        .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, fields)
        .await
        .unwrap_or_else(|error| panic!("create {entity_type}('{id}'): {error}"));
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "delete-cascade-test",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete(state: &ServerState, set: &str, id: &str) -> (StatusCode, serde_json::Value) {
    let path = format!("/tdata/{set}('{id}')");
    send(state, Request::delete(path).body(Body::empty()).unwrap()).await
}

async fn get(state: &ServerState, set: &str, id: &str) -> StatusCode {
    let path = format!("/tdata/{set}('{id}')");
    send(state, Request::get(path).body(Body::empty()).unwrap())
        .await
        .0
}

async fn post(
    state: &ServerState,
    set: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::post(format!("/tdata/{set}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, request).await
}

/// An album that owns `tracks` and points at the last of them.
async fn album_with_tracks(state: &ServerState, album: &str, tracks: &[&str]) {
    let latest = tracks.last().copied().unwrap_or_default();
    create(
        state,
        "Album",
        album,
        serde_json::json!({ "latest_track_id": latest }),
    )
    .await;
    for track in tracks {
        create(
            state,
            "Track",
            track,
            serde_json::json!({ "album_id": album }),
        )
        .await;
    }
}

#[tokio::test]
async fn deleting_an_owner_deletes_what_it_owns() {
    let store = Store::new("owner");
    let state = server("delete-cascade-owner", &store).await;
    let owned = ["t-1", "t-2", "t-3", "t-4", "t-5"];
    album_with_tracks(&state, "a-owner", &owned).await;
    album_with_tracks(&state, "a-other", &["t-other"]).await;

    let (status, body) = delete(&state, "Albums", "a-owner").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "owner delete: {body}");

    assert_eq!(
        get(&state, "Albums", "a-owner").await,
        StatusCode::NOT_FOUND
    );
    for track in owned {
        assert_eq!(
            get(&state, "Tracks", track).await,
            StatusCode::NOT_FOUND,
            "{track} outlived its album"
        );
    }
    assert_eq!(get(&state, "Albums", "a-other").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-other").await, StatusCode::OK);
}

#[tokio::test]
async fn a_restrict_reference_into_what_would_be_deleted_refuses_all_of_it() {
    let store = Store::new("restricted");
    let state = server("delete-cascade-restricted", &store).await;
    album_with_tracks(&state, "a-kept", &["t-kept-1", "t-kept-2"]).await;
    create(
        &state,
        "PlaylistEntry",
        "e-1",
        serde_json::json!({ "track_id": "t-kept-2" }),
    )
    .await;

    let (status, body) = delete(&state, "Albums", "a-kept").await;
    assert_eq!(status, StatusCode::CONFLICT, "restricted delete: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Track('t-kept-2')") && message.contains("PlaylistEntry('e-1')"),
        "the refusal names the owned row and what references it: {message}"
    );

    assert_eq!(get(&state, "Albums", "a-kept").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-kept-1").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-kept-2").await, StatusCode::OK);
}

#[tokio::test]
async fn a_none_relation_does_not_block_deleting_its_target() {
    let store = Store::new("pointer");
    let state = server("delete-cascade-pointer", &store).await;
    album_with_tracks(&state, "a-owning", &["t-pointed"]).await;
    create(
        &state,
        "Album",
        "a-pointer",
        serde_json::json!({ "latest_track_id": "t-pointed" }),
    )
    .await;

    let (status, body) = delete(&state, "Tracks", "t-pointed").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "pointed-at delete: {body}");
    assert_eq!(
        get(&state, "Tracks", "t-pointed").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&state, "Albums", "a-pointer").await, StatusCode::OK);
    assert_eq!(get(&state, "Albums", "a-owning").await, StatusCode::OK);
}

#[tokio::test]
async fn an_owned_row_cannot_be_created_for_a_missing_owner() {
    let store = Store::new("create");
    let state = server("delete-cascade-create", &store).await;
    album_with_tracks(&state, "a-present", &[]).await;

    let (status, body) = post(
        &state,
        "Tracks",
        serde_json::json!({ "Id": "t-orphan", "album_id": "a-missing" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "orphan create: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    assert_eq!(
        get(&state, "Tracks", "t-orphan").await,
        StatusCode::NOT_FOUND
    );

    let (status, body) = post(
        &state,
        "Tracks",
        serde_json::json!({ "Id": "t-owned", "album_id": "a-present" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "owned create: {body}");
}
