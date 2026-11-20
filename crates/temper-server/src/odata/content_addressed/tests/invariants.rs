//! A Blob's rules read its content, which a raw ingest stores out of line:
//! the create is checked on the content, never on the reference to it.
use super::*;

/// Blobs whose content may not be "abc" (base64 "YWJj").
fn state_forbidding_abc() -> (ServerState, tempfile::TempDir) {
    let spec = format!(
        "{BLOB_IOA}\n[[field_invariant]]\nname = \"NotAbc\"\n\
         message = \"a blob may not hold abc\"\nassert = \"Content != 'YWJj'\"\n"
    );
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Blob".to_string(), spec);
    let mut state = ServerState::with_specs(
        ActorSystem::new("raw-blob-ingest-invariants"),
        parse_csdl(BLOB_CSDL).expect("Blob test CSDL"),
        BLOB_CSDL.to_string(),
        specs,
    )
    .expect("Blob test state");
    let data_dir = tempfile::tempdir().expect("raw Blob data dir");
    state.data_dir = data_dir.path().to_path_buf();
    allow_blob_create(&state);
    (state, data_dir)
}

async fn ingest(state: &ServerState, body: &'static [u8]) -> StatusCode {
    let object_id = git_blob_id(body);
    app(state.clone())
        .oneshot(ingest_request(Body::from(body), body.len(), &object_id))
        .await
        .expect("raw Blob response")
        .status()
}

#[tokio::test]
async fn a_raw_ingest_is_checked_on_its_content() {
    let (state, _data_dir) = state_forbidding_abc();
    assert_eq!(ingest(&state, b"abc").await, StatusCode::CONFLICT);
    assert_eq!(ingest(&state, b"abd").await, StatusCode::CREATED);
}
