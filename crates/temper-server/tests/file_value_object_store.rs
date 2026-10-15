//! A File's bytes in a remote object store (R2/S3) under temporary credentials: streamed uploads past the buffered
//! body limit, a deployment key prefix, no copy in the database, and short-lived read links handed out only after
//! Cedar has let the reader read.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::secrets::vault::SecretsVault;
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;
use wiremock::matchers::{header_exists, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PREFIX: &str = "people/t0123456789ab/a1fileprobe01";

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FileObjectStoreTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="content_hash" Type="Edm.String"/>
        <Property Name="mime_type" Type="Edm.String"/>
        <Property Name="has_content" Type="Edm.Boolean"/>
        <Property Name="size_bytes" Type="Edm.Int64"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Files" EntityType="Temper.FileObjectStoreTest.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A File written once: its first `$value` makes it Ready, and nothing writes it again.
const WRITE_ONCE_FILE: &str = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[state]]
name = "content_hash"
type = "string"
initial = ""

[[state]]
name = "mime_type"
type = "string"
initial = ""

[[state]]
name = "has_content"
type = "bool"
initial = false

[[state]]
name = "size_bytes"
type = "counter"
initial = 0

[[state]]
name = "version_count"
type = "counter"
initial = 0

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type", "version_number", "previous_version_id", "created_by"]
effect = ["version_count += 1", "size_bytes = params.size_bytes", "has_content = true"]
"#;

async fn authenticate(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let principal = request
        .headers()
        .get("x-test-principal")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("member")
        .to_string();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(&principal, "test-agent", None),
        ));
    next.run(request).await
}

/// A Temper whose default tenant keeps File bytes in `mock`'s bucket under [`PREFIX`], with temporary credentials,
/// and whose Cedar lets `member` write and read Files and `reader` only read them.
async fn state_with_object_store(test: &str, mock: &MockServer) -> ServerState {
    let db = std::env::temp_dir().join(format!(
        "temper-file-object-store-{test}-{}.db",
        uuid::Uuid::new_v4()
    ));
    let store = TursoEventStore::new(&format!("file:{}", db.display()), None)
        .await
        .expect("turso");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("csdl"),
        CSDL.to_string(),
        &[("File", WRITE_ONCE_FILE)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "File",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0".to_string(),
                passed: true,
                summary: "test fixture verified".to_string(),
                details: None,
            }],
            verified_at: "2026-10-15T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(test), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state.data_dir = tempfile::tempdir().expect("data dir").keep();
    let vault = SecretsVault::new(&[7_u8; 32]);
    for (key, value) in [
        ("blob_endpoint", mock.uri()),
        ("blob_bucket", "bucket".to_string()),
        ("blob_prefix", PREFIX.to_string()),
        ("blob_access_key", "AKID".to_string()),
        ("blob_secret_key", "temporary-secret".to_string()),
        ("blob_session_token", "dGVtcG9yYXJ5".to_string()),
    ] {
        vault
            .cache_secret("default", key, value)
            .expect("cache secret");
    }
    state.secrets_vault = Some(std::sync::Arc::new(vault));
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action in [Action::"update", Action::"read"], resource is File) when { principal.id == "member" };
permit(principal, action == Action::"read", resource is File) when { principal.id == "reader" };"#,
        )
        .expect("policy");
    state
}

fn router(state: &ServerState) -> axum::Router {
    build_router(state.clone()).layer(axum::middleware::from_fn(authenticate))
}

fn put_file(id: &str, body: Vec<u8>, who: &str) -> Request<Body> {
    Request::put(format!("/tdata/Files('{id}')/$value"))
        .header("content-type", "video/mp4")
        .header("content-length", body.len().to_string())
        .header("x-test-principal", who)
        .body(Body::from(body))
        .expect("request")
}

fn content_hash(body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(body))
}

/// Every PUT the object store received, as (path, body length, whether it carried the session token).
async fn object_puts(mock: &MockServer) -> Vec<(String, usize, bool)> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| {
            (
                r.url.path().to_string(),
                r.body.len(),
                r.headers.get("x-amz-security-token").is_some(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_file_past_the_buffered_limit_streams_to_the_object_store_and_never_into_the_database() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex("^/bucket/.*"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let state = state_with_object_store("streamed-put", &mock).await;
    // Three times axum's 2 MiB default: the buffered path refuses it.
    let body: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let hash = content_hash(&body);

    let response = router(&state)
        .oneshot(put_file("fl-big", body.clone(), "member"))
        .await
        .expect("respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let key = format!("/bucket/{PREFIX}/temper-fs/{hash}");
    let puts: Vec<_> = object_puts(&mock)
        .await
        .into_iter()
        .map(|(path, n, token)| (path.replace("%3A", ":"), n, token))
        .collect();
    assert_eq!(
        puts,
        vec![(key, body.len(), true)],
        "one PUT of the whole body, under the prefix, with the session token"
    );
    let file = state
        .get_tenant_entity_state(&TenantId::default(), "File", "fl-big")
        .await
        .expect("file");
    assert_eq!(file.state.status, "Ready");
    assert_eq!(file.state.fields["content_hash"], hash);
    let metadata = state
        .metadata_store_for_tenant("default")
        .await
        .expect("metadata store");
    assert_eq!(
        metadata
            .get_blob(&format!("temper-fs/{hash}"))
            .await
            .expect("read"),
        None,
        "a File's bytes in the object store are never copied into the database"
    );
}

#[tokio::test]
async fn a_buffered_file_write_keeps_no_database_copy_either() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex("^/bucket/.*"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let state = state_with_object_store("buffered-put", &mock).await;
    let body = b"a small picture".to_vec();
    let hash = content_hash(&body);
    // No Content-Length: the buffered path.
    let request = Request::put("/tdata/Files('fl-small')/$value")
        .header("content-type", "image/png")
        .header("x-test-principal", "member")
        .body(Body::from(body.clone()))
        .expect("request");
    assert_eq!(
        router(&state)
            .oneshot(request)
            .await
            .expect("respond")
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(object_puts(&mock).await.len(), 1);
    let metadata = state
        .metadata_store_for_tenant("default")
        .await
        .expect("metadata store");
    assert_eq!(
        metadata
            .get_blob(&format!("temper-fs/{hash}"))
            .await
            .expect("read"),
        None
    );
}

#[tokio::test]
async fn a_refused_writer_and_a_file_written_once_store_nothing() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex("^/bucket/.*"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let state = state_with_object_store("refused-put", &mock).await;

    let refused = router(&state)
        .oneshot(put_file("fl-x", vec![1; 4096], "reader"))
        .await
        .expect("respond");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(
        object_puts(&mock).await.is_empty(),
        "Cedar refuses before a byte is stored"
    );

    let first = router(&state)
        .oneshot(put_file("fl-once", vec![2; 4096], "member"))
        .await
        .expect("respond");
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    let again = router(&state)
        .oneshot(put_file("fl-once", vec![3; 4096], "member"))
        .await
        .expect("respond");
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(
        object_puts(&mock).await.len(),
        1,
        "a File that takes no more content is refused before its body is stored"
    );
}

#[tokio::test]
async fn a_reader_gets_a_short_link_to_the_bytes_and_a_stranger_gets_nothing() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex("^/bucket/.*"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/bucket/.*"))
        .and(header_exists("x-amz-security-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"film".to_vec()))
        .mount(&mock)
        .await;
    let state = state_with_object_store("read-link", &mock).await;
    let body = b"film".to_vec();
    let hash = content_hash(&body);
    assert_eq!(
        router(&state)
            .oneshot(put_file("fl-film", body, "member"))
            .await
            .expect("put")
            .status(),
        StatusCode::NO_CONTENT
    );

    let read = |who: &str| {
        Request::get("/tdata/Files('fl-film')/$value?redirect=1")
            .header("x-test-principal", who)
            .body(Body::empty())
            .expect("request")
    };
    let linked = router(&state)
        .oneshot(read("reader"))
        .await
        .expect("respond");
    assert_eq!(linked.status(), StatusCode::FOUND);
    let location = linked
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        location.starts_with(&format!(
            "{}/bucket/{PREFIX}/temper-fs/{}",
            mock.uri(),
            hash.replace(':', "%3A")
        )),
        "{location}"
    );
    assert!(
        location.contains("X-Amz-Security-Token=dGVtcG9yYXJ5&"),
        "{location}"
    );
    assert!(
        location.contains("response-content-type=video%2Fmp4"),
        "{location}"
    );
    assert!(
        !location.contains("response-content-disposition"),
        "a film opens in place: {location}"
    );
    assert_eq!(
        linked
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    let refused = router(&state)
        .oneshot(read("stranger"))
        .await
        .expect("respond");
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(refused.headers().get("location").is_none());

    // Without ?redirect=1 the bytes come through the server, read with the session token.
    let direct = router(&state)
        .oneshot(
            Request::get("/tdata/Files('fl-film')/$value")
                .header("x-test-principal", "reader")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("respond");
    assert_eq!(direct.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(direct.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(&bytes[..], b"film");
}
