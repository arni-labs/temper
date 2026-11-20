use serde_json::json;
use sha2::{Digest as _, Sha256};

use temper_wasm::FieldBlobReader as _;

use super::BlobReadSource;
use super::hydration::hydrate_blob_refs_with_source;
use super::{
    BlobHydrationBudget, FIELD_OVERFLOW_REF_KEY, WASM_FIELD_READ_BUDGET_BYTES, WasmFieldBlobReader,
    blob_ref_value, field_overflow_descriptor,
};
use crate::blob_store::BlobStore;

async fn store_json(store: &BlobStore, value: &serde_json::Value) -> (String, usize) {
    let bytes = serde_json::to_vec(value).expect("serialize overflow JSON");
    let key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(&bytes));
    store
        .put_if_absent(&key, &bytes, None)
        .await
        .expect("store overflow JSON");
    (key, bytes.len())
}

/// The largest field a record write can carry is as large as the largest `/tdata` body, which is
/// `WASM_FIELD_READ_BUDGET_BYTES` (router.rs; a body one byte past it is refused there, see
/// `router/body_limit_test.rs`). A field that large reaches a module whole: it stays a reference through the call's
/// hydration, and the call's field reader, which `host_read_field` asks, reads it within the WASM dispatch budget.
#[tokio::test]
async fn a_field_as_large_as_the_largest_record_write_reaches_a_module_whole() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    // A JSON string serializes as its text between two quotes.
    let text = "a".repeat(WASM_FIELD_READ_BUDGET_BYTES - 2);
    let (key, len) = store_json(&store, &json!(text)).await;
    assert_eq!(len, WASM_FIELD_READ_BUDGET_BYTES);
    let mut entity_state = json!({
        "status": "Ready",
        "fields": { "text": blob_ref_value(&key, len) },
    });

    let budget = BlobHydrationBudget::wasm_dispatch();
    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut entity_state, &budget).await;
    let reader = WasmFieldBlobReader::for_store(store, budget);

    let read = reader
        .read_field_blob(&entity_state["fields"]["text"])
        .await
        .expect("a field the router accepts reaches the module");
    assert_eq!(read.len(), WASM_FIELD_READ_BUDGET_BYTES);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&read).expect("the field's JSON"),
        json!(text),
        "the module reads the whole value"
    );
}

#[test]
fn descriptor_requires_the_canonical_json_encoding() {
    let key = format!("field-overflow/sha256/{}.json", "0".repeat(64));
    let missing_encoding = json!({
        "__temper_blob_ref": key,
        "__temper_blob_size": 4,
    });
    let wrong_encoding = json!({
        "__temper_blob_ref": key,
        "__temper_blob_size": 4,
        "__temper_blob_encoding": "raw",
    });

    assert!(field_overflow_descriptor(&missing_encoding).is_none());
    assert!(field_overflow_descriptor(&wrong_encoding).is_none());
}

#[tokio::test]
async fn aggregate_inline_budget_is_shared_across_all_refs() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (first_key, first_len) = store_json(&store, &json!("a".repeat(400))).await;
    let (second_key, second_len) = store_json(&store, &json!("b".repeat(400))).await;
    assert_eq!(first_len, second_len);
    let mut value = json!({
        "a": blob_ref_value(&first_key, first_len),
        "b": blob_ref_value(&second_key, second_len),
    });
    let budget = BlobHydrationBudget::new(700, 500, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value["a"].as_str().map(str::len), Some(400));
    assert!(value["b"].get(FIELD_OVERFLOW_REF_KEY).is_some());
    assert_eq!(budget.remaining(), (700 - first_len, 0));
}

#[tokio::test]
async fn lying_small_descriptor_cannot_trigger_oversized_buffered_read() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, _) = store_json(&store, &json!("x".repeat(64 * 1024))).await;
    let mut value = blob_ref_value(&key, 4);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(
        budget.remaining(),
        (1024, 0),
        "failed bounded reads refund admission"
    );
}

#[tokio::test]
async fn noncanonical_overflow_key_is_never_read() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let key = "wasm-modules/not-a-field-overflow-object";
    store
        .put_if_absent(key, br#"{"secret":true}"#, None)
        .await
        .expect("store noncanonical object");
    let mut value = blob_ref_value(key, 15);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn descriptor_length_must_match_the_stored_object() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, actual) = store_json(&store, &json!("short")).await;
    let mut value = blob_ref_value(&key, actual + 10);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn missing_blob_reads_have_an_aggregate_attempt_budget() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let mut values = serde_json::Map::new();
    for index in 0..100u64 {
        let key = format!("field-overflow/sha256/{index:064x}.json");
        values.insert(index.to_string(), blob_ref_value(&key, 4));
    }
    let mut value = serde_json::Value::Object(values);
    let budget = BlobHydrationBudget::new(1024, 16, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(budget.read_attempts_remaining(), 0);
    assert!(
        value
            .as_object()
            .expect("object")
            .values()
            .all(|value| value.get(FIELD_OVERFLOW_REF_KEY).is_some())
    );
}

#[tokio::test]
async fn a_repeated_missing_key_is_read_only_once() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let key = format!("field-overflow/sha256/{}.json", "5".repeat(64));
    let mut value = serde_json::Value::Array((0..10).map(|_| blob_ref_value(&key, 4)).collect());
    let budget = BlobHydrationBudget::new(1024, 16, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(budget.read_attempts_remaining(), 63);
}

#[tokio::test]
async fn object_bytes_must_match_the_content_addressed_key() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let bytes = serde_json::to_vec(&json!("tampered")).expect("serialize value");
    let key = format!("field-overflow/sha256/{}.json", "6".repeat(64));
    store
        .put_if_absent(&key, &bytes, None)
        .await
        .expect("store corrupted content-addressed object");
    let mut value = blob_ref_value(&key, bytes.len());
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn reads_on_request_share_one_aggregate_budget() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (first_key, first_len) = store_json(&store, &json!("a".repeat(300))).await;
    let (second_key, second_len) = store_json(&store, &json!("b".repeat(300))).await;
    let budget = BlobHydrationBudget::new(0, 0, 500, 400);
    let reader = WasmFieldBlobReader::for_store(store, budget.clone());

    let first = reader
        .read_field_blob(&blob_ref_value(&first_key, first_len))
        .await;
    let second = reader
        .read_field_blob(&blob_ref_value(&second_key, second_len))
        .await;

    assert_eq!(first.map(|bytes| bytes.len()), Some(first_len));
    assert!(
        second.is_none(),
        "the second read does not fit what is left"
    );
    assert_eq!(budget.remaining(), (0, 500 - first_len));
}

/// Record checks must read all required values, regardless of response budgets.
#[tokio::test]
async fn record_validation_resolves_more_than_response_read_and_pointer_limits() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, size) = store_json(&store, &json!(["item"])).await;
    for count in [65, 1025] {
        let fields = serde_json::Value::Object(
            (0..count)
                .map(|i| (format!("Items{i:04}"), blob_ref_value(&key, size)))
                .collect(),
        );
        let names = fields.as_object().unwrap().keys().cloned().collect();
        let resolved =
            super::resolve_stored_fields(&BlobReadSource::Store(&store), &fields, &names)
                .await
                .expect("every healthy required value resolves")
                .unwrap();
        for i in 0..count {
            assert_eq!(
                resolved[format!("Items{i:04}")],
                json!(["item"]),
                "field {i} of {count}"
            );
        }
    }
}

#[tokio::test]
async fn record_validation_is_not_limited_to_the_wasm_byte_budget() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let value = json!(["x".repeat(16 * 1024 * 1024)]);
    let (key, size) = store_json(&store, &value).await;
    let fields = json!({"Items": blob_ref_value(&key, size)});
    let names = ["Items".to_string()].into_iter().collect();
    let resolved = super::resolve_stored_fields(&BlobReadSource::Store(&store), &fields, &names)
        .await
        .expect("healthy required value resolves")
        .unwrap();
    assert_eq!(resolved["Items"], value);
}

#[tokio::test]
async fn record_validation_rejects_missing_corrupt_wrong_size_and_invalid_json() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, size) = store_json(&store, &json!(["valid"])).await;
    let malformed = b"not json";
    let malformed_key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(malformed));
    store
        .put_if_absent(&malformed_key, malformed, None)
        .await
        .unwrap();
    let corrupt_key = format!(
        "field-overflow/sha256/{:x}.json",
        Sha256::digest(b"different")
    );
    store
        .put_if_absent(&corrupt_key, b"[]", None)
        .await
        .unwrap();
    let missing_key = format!("field-overflow/sha256/{}.json", "0".repeat(64));
    let names = ["Items".to_string()].into_iter().collect();
    for reference in [
        blob_ref_value(&missing_key, 2),
        blob_ref_value(&corrupt_key, 2),
        blob_ref_value(&key, size + 1),
        blob_ref_value(&malformed_key, malformed.len()),
    ] {
        let fields = json!({"Items": reference});
        assert!(
            super::resolve_stored_fields(&BlobReadSource::Store(&store), &fields, &names)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn record_validation_resolves_nested_selected_fields_and_ignores_others() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, size) = store_json(&store, &json!(["valid"])).await;
    let fields = json!({
        "Items~/": [{"nested~/": blob_ref_value(&key, size)}],
        "Unselected": blob_ref_value(&format!("field-overflow/sha256/{}.json", "0".repeat(64)), 2),
    });
    let names = ["Items~/".to_string()].into_iter().collect();
    let resolved = super::resolve_stored_fields(&BlobReadSource::Store(&store), &fields, &names)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved["Items~/"][0]["nested~/"], json!(["valid"]));
    assert_eq!(resolved["Unselected"], fields["Unselected"]);
}

/// A module reads a 2 MiB field of a record that also holds a 15 MiB one it never reads. The 2 MiB read must
/// succeed: a field the module does not ask for takes nothing from the call's 16 MiB. When the module then asks for
/// the 15 MiB field too, that read is charged to the same budget and does not fit.
#[tokio::test]
async fn an_unread_large_field_leaves_the_budget_to_the_field_a_module_reads() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (large_key, large_len) = store_json(&store, &json!("l".repeat(15 * 1024 * 1024))).await;
    let (small_key, small_len) = store_json(&store, &json!("s".repeat(2 * 1024 * 1024))).await;
    // "a_large" sorts before "b_small", so a pass in field order meets the large field first.
    let mut entity_state = json!({
        "fields": {
            "a_large": blob_ref_value(&large_key, large_len),
            "b_small": blob_ref_value(&small_key, small_len),
        },
    });
    let budget = BlobHydrationBudget::wasm_dispatch();
    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut entity_state, &budget).await;
    let reader = WasmFieldBlobReader::for_store(store, budget.clone());

    let small = reader
        .read_field_blob(&entity_state["fields"]["b_small"])
        .await
        .expect("the field the module reads is readable");
    assert_eq!(small.len(), small_len);
    assert_eq!(
        budget.remaining().1,
        WASM_FIELD_READ_BUDGET_BYTES - small_len,
        "only the field read is charged"
    );

    assert!(
        reader
            .read_field_blob(&entity_state["fields"]["a_large"])
            .await
            .is_none(),
        "the large field, read after, is charged to the same 16 MiB and does not fit"
    );
}
