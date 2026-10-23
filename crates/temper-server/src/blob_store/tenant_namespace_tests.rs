//! The default tenant's blob namespace and every other tenant's stay apart on one shared local root.

use sha2::{Digest as _, Sha256};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::CsdlDocument;

use super::{BlobStreamRead, TENANT_NAMESPACES_DIR, hex_lower};
use crate::state::ServerState;

fn state_on(data_dir: &std::path::Path) -> ServerState {
    let mut state = ServerState::new(
        ActorSystem::new("tenant-blob-namespaces"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.data_dir = data_dir.to_path_buf();
    state
}

#[tokio::test]
async fn the_default_tenant_cannot_reach_another_tenants_objects_by_key() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let state = state_on(data_dir.path());
    let tenant_b = TenantId::new("tenant-b");
    let key = "records/sha256/value.json";
    state
        .put_blob_object(&tenant_b, key, b"tenant-b bytes", None)
        .await
        .expect("tenant B write");

    let namespace = hex_lower(&Sha256::digest(b"tenant-b"));
    let default = TenantId::default();
    for dir in [TENANT_NAMESPACES_DIR, "Tenants", "TENANTS"] {
        let nested = format!("{dir}/{namespace}/{key}");
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&default, &nested)
                .await
                .expect("default read"),
            None,
            "{nested}"
        );
        let streamed = state.stream_blob_object(&default, &nested, 1024).await;
        assert!(
            !matches!(streamed, Ok(BlobStreamRead::Found(_))),
            "{nested}"
        );
        assert!(
            state
                .put_blob_object(&default, &format!("{nested}.new"), b"default bytes", None)
                .await
                .is_err(),
            "{nested}"
        );
    }

    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant_b, key)
            .await
            .expect("tenant B read"),
        Some(b"tenant-b bytes".to_vec())
    );
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&tenant_b, &format!("{key}.new"))
            .await
            .expect("tenant B read"),
        None,
        "the default tenant added nothing to tenant B's namespace"
    );

    // The default tenant's own objects stay at the shared root, where its older objects already are.
    state
        .put_blob_object(&default, key, b"default bytes", None)
        .await
        .expect("default write");
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&default, key)
            .await
            .expect("default read"),
        Some(b"default bytes".to_vec())
    );
    assert!(data_dir.path().join("blobs").join(key).exists());
}
