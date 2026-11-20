//! A trigger aimed at a record that does not exist fails where its sender
//! sees it, makes no record, and is counted.
//!
//! Before this, the kernel made an empty record for the id, refused the move
//! on it, and threw the refusal away: the sender got a clean answer and the
//! work the trigger was to start never started (Arni Ops, 19 Nov 2026: a
//! line's `Fix` sent to a shipment id that was a bare commit).
//!
//! Its own test binary, so the meter is set before anything in this process
//! records a metric.

#![cfg(feature = "observe")]

#[path = "trigger_missing_record/fixture.rs"]
mod fixture;

use axum::http::{Method, StatusCode};
use fixture::{act, read, refused, request, send, server};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::{Value, json};
use temper_server::build_router;
use temper_store_turso::TursoEventStore;

/// [`act`], sent under an idempotency key.
async fn act_keyed(
    app: &axum::Router,
    set: &str,
    id: &str,
    action: &str,
    body: Value,
    key: &str,
) -> (StatusCode, Value) {
    let uri = format!("/tdata/{set}('{id}')/Temper.{action}");
    let mut request = request(Method::POST, &uri, body);
    request
        .headers_mut()
        .insert("idempotency-key", key.parse().unwrap());
    send(app, request).await
}

#[tokio::test]
async fn a_trigger_aimed_at_a_record_that_does_not_exist_fails_where_its_sender_sees_it() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let temp = tempfile::tempdir().expect("temporary test directory");
    let url = format!("file:{}", temp.path().join("metadata.db").display());
    let store = TursoEventStore::new(&url, None).await.expect("store opens");
    let app = build_router(server(&temp, Some(store)).await);

    // The line fails and names a shipment that was never made. Its own move
    // is committed; the trigger's failure is said beside it.
    let (status, failed) = act(
        &app,
        "Lines",
        "line-1",
        "Fail",
        json!({"shipment": "abc123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(failed["status"], "Failed", "{failed}");
    let failures = failed["@temper.triggerFailures"]
        .as_array()
        .unwrap_or_else(|| panic!("the sender is told its trigger failed: {failed}"));
    assert_eq!(failures.len(), 1, "{failed}");
    let failure = failures[0].as_str().unwrap();
    assert!(failure.contains("fix"), "it names the trigger: {failure}");
    assert_eq!(
        failure, "Line:Fail:fix: the Shipment it names does not exist, and Fix does not make one",
        "it says the record is missing, and not which id: the id may come from a record the sender cannot read"
    );
    // No empty record was made for the id.
    assert_eq!(
        read(&app, "Shipments", "abc123").await.0,
        StatusCode::NOT_FOUND
    );
    let counted = |reasons: &[(&str, u64)]| -> std::collections::BTreeMap<String, u64> {
        reasons
            .iter()
            .map(|(reason, count)| (reason.to_string(), *count))
            .collect()
    };
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1)]),
        "and it is counted as a missing record"
    );

    // A request's own move on a record that does not exist is refused the
    // same way, and makes none either.
    let (status, body) = act(&app, "Shipments", "nowhere", "Fix", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "InvalidState", "{body}");
    assert_eq!(body["error"]["state"], "Unmade", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Shipment 'nowhere' does not exist")),
        "{body}"
    );
    assert_eq!(
        read(&app, "Shipments", "nowhere").await.0,
        StatusCode::NOT_FOUND
    );

    // A move that starts from the initial state still makes its record, and a
    // trigger that names a record that exists reaches it and says nothing.
    let (status, made) = act(&app, "Shipments", "made", "Add", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, reached) = act(&app, "Lines", "line-2", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{reached}");
    assert!(
        reached.get("@temper.triggerFailures").is_none(),
        "{reached}"
    );
    let (status, shipment) = read(&app, "Shipments", "made").await;
    assert_eq!(status, StatusCode::OK, "{shipment}");
    assert_eq!(shipment["status"], "Fixing", "{shipment}");
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1)]),
        "nothing more was refused"
    );

    // A record that exists and is not where the move starts from refuses as
    // it always did, is said to the sender too, and is counted apart.
    let (status, again) = act(&app, "Lines", "line-3", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    let failure = again["@temper.triggerFailures"][0]
        .as_str()
        .unwrap_or_else(|| panic!("the sender is told: {again}"));
    assert_eq!(
        failure,
        "Line:Fail:fix: its Shipment did not take Fix: not possible from the state it is in",
        "the sender may not be allowed to read the shipment, so its state is not named"
    );
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 1), ("state", 1)])
    );

    // A retry under the same key gets the reply the first try got, with what
    // it said about the trigger.
    let body = json!({"shipment": "nowhere"});
    let (status, first) = act_keyed(&app, "Lines", "line-4", "Fail", body.clone(), "k1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let said = first["@temper.triggerFailures"].clone();
    assert_eq!(
        said,
        json!(["Line:Fail:fix: the Shipment it names does not exist, and Fix does not make one"]),
        "{first}"
    );
    let (status, retried) = act_keyed(&app, "Lines", "line-4", "Fail", body, "k1").await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["@temper.triggerFailures"], said, "{retried}");
    assert_eq!(
        refused(&provider, &exporter),
        counted(&[("missing", 2), ("state", 1)]),
        "the retry sent no second reaction"
    );
}
