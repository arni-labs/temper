use serde_json::json;

use super::*;

fn row(id: &str, url: &str) -> serde_json::Value {
    json!({
        "id": id,
        "method": "POST",
        "url": url,
        "headers": { "Idempotency-Key": format!("key-{id}") },
        "body": { "n": 1 },
    })
}

fn read(requests: serde_json::Value) -> Result<Vec<BatchRow>, String> {
    read_rows(json!({ "requests": requests }).to_string().as_bytes())
}

#[test]
fn rows_keep_their_order_path_query_key_and_body() {
    let rows = read(json!([
        row("a", "Items('one')/Shop.Bump?await_integration=true"),
        row("b", "/tdata/Items('two%20x')/Shop.Bump"),
        row("c", "Items"),
    ]))
    .expect("a valid batch");
    let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(rows[0].path, "Items('one')/Shop.Bump");
    assert_eq!(
        rows[0].query.get("await_integration").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        rows[1].path, "Items('two x')/Shop.Bump",
        "the path is percent-decoded"
    );
    assert_eq!(rows[1].key, "key-b");
    assert_eq!(rows[2].body.as_ref(), br#"{"n":1}"#);
}

#[test]
fn two_rows_on_one_record_are_refused() {
    let error = read(json!([
        row("a", "Items('one')/Shop.Bump"),
        row("b", "Items('two')/Shop.Bump"),
        row("c", "Items('one')/Shop.Close"),
    ]))
    .unwrap_err();
    assert!(error.contains("'a' and 'c'"), "{error}");
}

#[test]
fn creates_on_the_entity_set_name_no_record() {
    let rows = read(json!([row("a", "Items"), row("b", "Items")])).expect("two creates");
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_row_without_a_key_is_refused() {
    let mut keyless = row("a", "Items('one')/Shop.Bump");
    keyless["headers"] = json!({});
    let error = read(json!([keyless])).unwrap_err();
    assert!(error.contains("Idempotency-Key"), "{error}");
}

#[test]
fn caller_authority_headers_are_dropped() {
    let mut forged = row("a", "Items('one')/Shop.Bump");
    forged["headers"]["x-temper-principal-id"] = json!("mallory");
    let rows = read(json!([forged])).expect("a valid batch");
    assert!(rows[0].headers.get("x-temper-principal-id").is_none());
    assert_eq!(rows[0].key, "key-a");
}

#[test]
fn groupings_other_methods_and_foreign_urls_are_refused() {
    let mut grouped = row("a", "Items('one')/Shop.Bump");
    grouped["atomicityGroup"] = json!("g1");
    let mut read_row = row("b", "Items('one')");
    read_row["method"] = json!("GET");
    for (requests, says) in [
        (json!([grouped]), "atomicityGroup"),
        (json!([read_row]), "POST rows"),
        (
            json!([row("c", "https://elsewhere.example/tdata/Items")]),
            "under /tdata",
        ),
        (json!([row("d", "/observe/entities")]), "under /tdata"),
        (json!([row("e", "$batch")]), "create or an action"),
        (
            json!([row("f", "Items"), row("f", "Items")]),
            "two rows have the id",
        ),
    ] {
        let error = read(requests).unwrap_err();
        assert!(error.contains(says), "{says}: {error}");
    }
    assert!(read_rows(b"[]").unwrap_err().contains("requests"));
}

#[test]
fn a_row_starts_only_while_the_answer_keeps_its_push_time() {
    use std::time::Duration;

    use super::super::run::may_start;

    assert!(may_start(Duration::ZERO));
    assert!(may_start(Duration::from_millis(8_999)));
    assert!(!may_start(Duration::from_secs(9)));
    assert!(!may_start(Duration::from_secs(60)));
}
