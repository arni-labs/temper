//! Tenants that share one public bucket keep separate public objects.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::TursoEventStore;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{PublishArtifactError, PublishFileArtifactRequest};
use crate::aws_sigv4::sha256_hex;
use crate::secrets::vault::SecretsVault;
use crate::state::ServerState;
use crate::state::file_writes::native_file_blob_key;
use crate::storage::StorageStack;

struct Harness {
    state: ServerState,
    store: TursoEventStore,
    _dir: tempfile::TempDir,
}

/// One server whose public bucket is configured once, for every tenant, at the platform layer.
async fn harness(public_endpoint: &str) -> Harness {
    let dir = tempfile::tempdir().expect("data dir");
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("server.db").display()),
        None,
    )
    .await
    .expect("local database");
    let vault = SecretsVault::new(&[7_u8; 32]);
    for (key, value) in [
        ("published_blob_endpoint", public_endpoint.to_string()),
        ("published_blob_bucket", "public-artifacts".to_string()),
        (
            "published_blob_public_base_url",
            format!("{public_endpoint}/public-artifacts"),
        ),
    ] {
        vault.cache_platform_secret(key, value);
    }
    let mut state = ServerState::new(
        ActorSystem::new("published-artifact-tenant-keys"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    )
    .with_secrets_vault(vault);
    state.data_dir = dir.path().to_path_buf();
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    Harness {
        state,
        store,
        _dir: dir,
    }
}

/// A File in `tenant` that records `content_hash` and whose stored bytes are `bytes`.
async fn seed_file(harness: &Harness, tenant: &TenantId, content_hash: &str, bytes: &[u8]) {
    harness
        .store
        .upsert_query_projection(
            tenant.as_str(),
            "File",
            "file-1",
            "Ready",
            &serde_json::json!({
                "content_hash": content_hash,
                "mime_type": "text/markdown",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("File projection");
    harness
        .state
        .put_blob_object(tenant, &native_file_blob_key(content_hash), bytes, None)
        .await
        .expect("File bytes");
}

fn request() -> PublishFileArtifactRequest {
    PublishFileArtifactRequest {
        file_id: "file-1".to_string(),
        label: "latest".to_string(),
        owner_ref_type: "Doc".to_string(),
        owner_ref_id: "doc-1".to_string(),
        source_file_version_id: String::new(),
        namespace: None,
    }
}

#[tokio::test]
async fn two_tenants_publishing_the_same_name_keep_separate_public_objects() {
    let bucket = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&bucket)
        .await;
    let harness = harness(&bucket.uri()).await;
    let bytes = b"same public bytes\n";
    let hash = format!("sha256:{}", sha256_hex(bytes));
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");
    seed_file(&harness, &tenant_a, &hash, bytes).await;
    seed_file(&harness, &tenant_b, &hash, bytes).await;

    let a = harness
        .state
        .publish_file_artifact(&tenant_a, request())
        .await
        .expect("tenant A publishes");
    let b = harness
        .state
        .publish_file_artifact(&tenant_b, request())
        .await
        .expect("tenant B publishes");

    assert_ne!(a.public_storage_key, b.public_storage_key);
    assert_ne!(a.public_url, b.public_url);
    for (tenant, artifact) in [(&tenant_a, &a), (&tenant_b, &b)] {
        let segment = sha256_hex(tenant.as_str().as_bytes());
        assert!(
            artifact
                .public_storage_key
                .starts_with(&format!("published-artifacts/tenants/{segment}/Doc/doc-1/")),
            "{}",
            artifact.public_storage_key
        );
    }
    let paths: Vec<String> = bucket
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
}

#[tokio::test]
async fn bytes_that_do_not_match_their_content_hash_are_not_published() {
    let bucket = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&bucket)
        .await;
    let harness = harness(&bucket.uri()).await;
    let tenant = TenantId::new("tenant-a");
    let recorded = format!("sha256:{}", sha256_hex(b"the recorded bytes\n"));
    seed_file(&harness, &tenant, &recorded, b"other bytes\n").await;

    let error = harness
        .state
        .publish_file_artifact(&tenant, request())
        .await
        .expect_err("mismatched bytes are refused");
    assert!(
        matches!(&error, PublishArtifactError::Rejected(message) if message.contains("do not match")),
        "{error}"
    );
}
