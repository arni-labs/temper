//! A raw upload's record is checked against its type's rules, read from the
//! staged bytes, before either of its blobs is stored.

use base64::Engine as _;

use super::*;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Git" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Blob">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="Size" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Content" Type="Edm.Binary" Nullable="false"/>
        <Property Name="CanonicalBytes" Type="Edm.Binary" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Blobs" EntityType="Temper.Git.Blob"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A Blob type whose rule refuses the content "refused".
fn state_with_content_rule() -> ServerState {
    let refused = base64::engine::general_purpose::STANDARD.encode(b"refused");
    let blob_ioa = format!(
        r#"
[automaton]
name = "Blob"
states = ["Durable"]
initial = "Durable"

[[action]]
name = "Create"
kind = "input"
from = ["Durable"]
to = "Durable"
params = ["RepositoryId", "Size", "Content", "CanonicalBytes", "CreatedAt"]

[[field_invariant]]
name = "NotRefused"
assert = "Content != '{refused}'"
"#
    );
    let csdl = parse_csdl(CSDL).unwrap();
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Blob".to_string(), blob_ioa);
    let mut state = ServerState::with_specs(
        ActorSystem::new("raw-upload-rules"),
        csdl,
        CSDL.into(),
        specs,
    )
    .unwrap();
    state.data_dir = tempfile::tempdir().expect("dir").keep();
    state
}

/// The keys the upload of `body` stores its two representations under.
fn stored_keys(body: &[u8]) -> [String; 2] {
    let prefix = format!("blob {}\0", body.len());
    let canonical = [prefix.as_bytes(), body].concat();
    [body.to_vec(), canonical].map(|bytes| {
        let json = format!(
            "\"{}\"",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        format!(
            "{}{}.json",
            crate::blobs::FIELD_OVERFLOW_BLOB_PREFIX,
            crate::aws_sigv4::sha256_hex(json.as_bytes())
        )
    })
}

async fn upload(state: &ServerState, body: &'static [u8]) -> StatusCode {
    authenticated_router(state.clone())
        .oneshot(
            Request::post("/tdata/Blobs/Temper.IngestRaw")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", body.len().to_string())
                .header("X-Expected-Object-Id", git_blob_id(body))
                .header("X-Repository-Id", "rp-acme-demo")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn a_raw_upload_a_rule_refuses_stores_no_blob() {
    let state = state_with_content_rule();
    let tenant = TenantId::default();
    let store = state.blob_store_for_tenant(&tenant).expect("store");

    assert_eq!(upload(&state, b"refused").await, StatusCode::CONFLICT);
    assert!(state.list_entity_ids(&tenant, "Blob").is_empty());
    for key in stored_keys(b"refused") {
        assert_eq!(store.get(&key).await.expect("read"), None, "{key} stored");
    }

    // The rule reads the staged content: an upload it allows is stored.
    assert_eq!(upload(&state, b"kept").await, StatusCode::CREATED);
    for key in stored_keys(b"kept") {
        assert!(
            store.get(&key).await.expect("read").is_some(),
            "{key} missing"
        );
    }
}
