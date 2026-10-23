use super::*;

fn artifact_vault(endpoint: &str) -> SecretsVault {
    let vault = SecretsVault::new(&[11_u8; 32]);
    for (key, value) in [
        ("published_blob_public_base_url", "https://public.example"),
        ("published_blob_endpoint", endpoint),
        ("published_blob_bucket", "published-bucket"),
    ] {
        vault.cache_secret(TENANT, key, value.to_string());
    }
    vault
}

fn artifact_body(namespace: &str, source_version: &str) -> serde_json::Value {
    serde_json::json!({
        "file_id": "file-a",
        "label": "latest",
        "owner_ref_type": "Document",
        "owner_ref_id": "doc-a",
        "source_file_version_id": source_version,
        "namespace": namespace,
    })
}

#[tokio::test]
async fn artifact_publish_requires_dedicated_source_permission_and_safe_segments() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;
    let (mut state, store, _temp) = state_with_turso("artifact-resource-auth").await;
    state.secrets_vault = Some(std::sync::Arc::new(artifact_vault(&mock.uri())));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == File::"file-a"
);
permit(
  principal == Customer::"read-only",
  action == Action::"read",
  resource == File::"file-a"
);
permit(
  principal == Customer::"version-only",
  action == Action::"publish_artifact",
  resource == FileVersion::"version-a"
);
permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == FileVersion::"version-mismatch"
);
permit(
  principal == Customer::"forged-metadata",
  action == Action::"publish_artifact",
  resource == File::"file-a"
)
when {
  context.owner_ref_id == "doc-a"
};
"#,
        )
        .expect("artifact policy should parse");
    state
        .authz
        .reload_tenant_policies("other-tenant", "")
        .expect("other tenant should default-deny");
    seed_file(
        &store,
        "File",
        "file-a",
        "sha256:9938be87d35f2a7a2b80237e8dc71806b209aaea8252f12c1b12949f61d40476",
        b"artifact body",
        None,
    )
    .await;
    seed_file(
        &store,
        "FileVersion",
        "version-a",
        "sha256:6c8c7d46f81e53838444f74d1783a3082161068740dcdabe0ac2d578556bb469",
        b"artifact version",
        Some("file-a"),
    )
    .await;
    seed_file(
        &store,
        "FileVersion",
        "version-mismatch",
        "sha256:8b2a120ba4238c34e511f5b84f4d5085265ef08ddc033936fc067738de99ae0c",
        b"artifact mismatch",
        Some("file-b"),
    )
    .await;
    let app = build_router(state);

    for (tenant, principal, body, expected) in [
        (
            TENANT,
            "intruder",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "read-only",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "forged-metadata",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            "other-tenant",
            "publisher",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("artifacts", "version-a"),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "version-only",
            artifact_body("artifacts", "version-a"),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("artifacts", "version-mismatch"),
            StatusCode::BAD_REQUEST,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("../escape", ""),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/files/publish-artifact",
                body,
                tenant,
                principal,
            ))
            .await
            .expect("rejected publish request should run");
        assert_eq!(
            response.status(),
            expected,
            "unexpected publish status for tenant {tenant} principal {principal}"
        );
    }

    let allowed = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/publish-artifact",
            artifact_body("artifacts", ""),
            TENANT,
            "publisher",
        ))
        .await
        .expect("authorized publish request should run");
    assert_eq!(allowed.status(), StatusCode::OK);
    mock.verify().await;
}

#[tokio::test]
async fn local_artifact_publish_uses_direct_tenant_blob_store() {
    let (mut state, store, _temp) = state_with_turso("artifact-local-write").await;
    state.secrets_vault = Some(std::sync::Arc::new(artifact_vault(
        "http://127.0.0.1:9/_internal/blobs",
    )));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == File::"file-a"
);"#,
        )
        .expect("artifact policy should parse");
    seed_file(
        &store,
        "File",
        "file-a",
        "sha256:a50e31e40db113009ef1b378f2f9e129cf2a085b595d7bb03a2d8f9b7096d58e",
        b"local artifact body",
        None,
    )
    .await;
    let state_for_read = state.clone();
    let app = build_router(state);

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/publish-artifact",
            artifact_body("artifacts", ""),
            TENANT,
            "publisher",
        ))
        .await
        .expect("local publish request should run");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read publish response");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("publish JSON");
    let storage_key = json["artifact"]["public_storage_key"]
        .as_str()
        .expect("public storage key");
    let stored = state_for_read
        .get_blob_with_legacy_fallback(
            &TenantId::default(),
            &format!("published-bucket/{storage_key}"),
        )
        .await
        .expect("read local artifact blob");
    assert_eq!(stored.as_deref(), Some(b"local artifact body".as_slice()));
}

#[tokio::test]
async fn publishing_bytes_that_differ_from_their_recorded_hash_is_the_callers_request_to_fix() {
    let (mut state, store, _temp) = state_with_turso("artifact-hash-mismatch").await;
    state.secrets_vault = Some(std::sync::Arc::new(artifact_vault(
        "http://127.0.0.1:9/_internal/blobs",
    )));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == File::"file-a"
);"#,
        )
        .expect("artifact policy should parse");
    // The recorded hash is that of "local artifact body"; the stored bytes are not.
    seed_file(
        &store,
        "File",
        "file-a",
        "sha256:a50e31e40db113009ef1b378f2f9e129cf2a085b595d7bb03a2d8f9b7096d58e",
        b"other bytes",
        None,
    )
    .await;
    let app = build_router(state);

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/publish-artifact",
            artifact_body("artifacts", ""),
            TENANT,
            "publisher",
        ))
        .await
        .expect("publish request should run");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read publish response");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("publish JSON");
    assert_eq!(json["error"]["code"], "PublishArtifactRejected");
    assert!(
        json["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("do not match")),
        "{json}"
    );
}
