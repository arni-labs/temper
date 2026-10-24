//! A guest's loopback `/tdata` path is percent-decoded once, as the HTTP route
//! decodes it, so an encoded key names the same record either way.

use super::*;

#[test]
fn a_local_tdata_path_is_percent_decoded_like_the_http_route() {
    let request = LocalTDataRequest::parse(
        "http://127.0.0.1:8787/tdata/Files('doc%207')/$value",
        &BTreeSet::new(),
    )
    .expect("loopback TData URL should parse");

    assert_eq!(request.path, "Files('doc 7')/$value");
}

#[tokio::test]
async fn an_encoded_key_reaches_the_record_it_names() {
    let state = test_state();
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
        "test-module",
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    for (id, customer) in [("order 1", "Ada"), ("order%201", "Lin")] {
        let body = serde_json::json!({ "id": id, "Customer": customer }).to_string();
        let (status, _) = host
            .http_call(
                "POST",
                "http://127.0.0.1:8787/tdata/Orders",
                &headers,
                &body,
            )
            .await
            .expect("local create should dispatch");
        assert_eq!(status, StatusCode::CREATED.as_u16(), "create {id}");
    }

    let (status, body) = host
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('order%201')",
            &headers,
            "",
        )
        .await
        .expect("local read should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["entity_id"], "order 1");
    assert_eq!(fetched["fields"]["Customer"], "Ada");

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders('order%201')/Temper.Local.SubmitOrder",
            &headers,
            "{}",
        )
        .await
        .expect("local action should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let submitted: serde_json::Value = serde_json::from_str(&body).expect("action JSON");
    assert_eq!(submitted["status"], "Submitted");

    // The record whose id is literally `order%201` is reached by encoding its
    // `%`, and the action above left it as it was.
    let (status, body) = host
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('order%25201')",
            &headers,
            "",
        )
        .await
        .expect("local read should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let other: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(other["entity_id"], "order%201");
    assert_eq!(other["fields"]["Customer"], "Lin");
    assert_eq!(other["status"], "Draft");
}
