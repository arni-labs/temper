//! On a server with no journal, a trigger whose target is in the wrong state
//! is told so, and one whose target does not exist is told that.
//!
//! Its own test binary, so the meter is set before anything in this process
//! records a metric (`trigger_missing_record` is the same app on a journal).

#![cfg(feature = "observe")]

#[path = "trigger_missing_record/fixture.rs"]
mod fixture;

use axum::http::StatusCode;
use fixture::{act, read, refused, server};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::json;
use temper_server::build_router;

/// A server with no journal never advances a record's sequence, so "never
/// written" cannot be read from the sequence alone: there, a shipment that
/// exists and is in the wrong state is said and counted as that, and one that
/// does not exist as missing.
#[tokio::test]
async fn without_a_journal_a_target_in_the_wrong_state_is_not_called_missing() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    let temp = tempfile::tempdir().expect("temporary test directory");
    let app = build_router(server(&temp, None).await);

    let (status, made) = act(&app, "Shipments", "made", "Add", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let (status, reached) = act(&app, "Lines", "line-1", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{reached}");
    assert!(
        reached.get("@temper.triggerFailures").is_none(),
        "{reached}"
    );
    let (status, shipment) = read(&app, "Shipments", "made").await;
    assert_eq!(status, StatusCode::OK, "{shipment}");
    assert_eq!(shipment["status"], "Fixing", "{shipment}");

    let (status, again) = act(&app, "Lines", "line-2", "Fail", json!({"shipment": "made"})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["@temper.triggerFailures"],
        json!([
            "Line:Fail:fix: its Shipment did not take Fix: not possible from the state it is in"
        ]),
        "{again}"
    );
    let (status, lost) = act(
        &app,
        "Lines",
        "line-3",
        "Fail",
        json!({"shipment": "nowhere"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{lost}");
    assert_eq!(
        lost["@temper.triggerFailures"],
        json!(["Line:Fail:fix: the Shipment it names does not exist, and Fix does not make one"]),
        "{lost}"
    );
    let counted: std::collections::BTreeMap<String, u64> =
        [("missing".to_string(), 1), ("state".to_string(), 1)].into();
    assert_eq!(refused(&provider, &exporter), counted);
}
