//! ADR-0187 through the `/tdata` router: a list ordered by an event time
//! follows `@odata.nextLink` to every record once, in the order of the
//! instants the records report, and its count is the whole set's.

use super::*;
use temper_runtime::persistence::{MISSING_EVENT_TIME_KEY, event_time_key};

const RECORDS: usize = 25;

/// Every record's id and the key of `property`, read in one page.
async fn keys(state: &ServerState, property: &str) -> Vec<(String, String)> {
    let (status, body) = get_json(state, "/tdata/Orders?$top=1000").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["value"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            let key = row[property]
                .as_str()
                .and_then(event_time_key)
                .unwrap_or_else(|| MISSING_EVENT_TIME_KEY.to_string());
            (row["entity_id"].as_str().expect("id").to_string(), key)
        })
        .collect()
}

/// Follow the nextLink from `path` to the last page.
async fn walk(state: &ServerState, path: &str) -> (Vec<String>, Option<u64>) {
    let mut path = path.to_string();
    let mut ids = Vec::new();
    let mut count = None;
    loop {
        let (status, body) = get_json(state, &path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        count = count.or(body["@odata.count"].as_u64());
        ids.extend(
            body["value"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| row["entity_id"].as_str().expect("id").to_string()),
        );
        match body["@odata.nextLink"].as_str() {
            // nextLink is relative to the entity set's /tdata/ base URL.
            Some(next) => path = format!("/tdata/{next}"),
            None => break,
        }
        assert!(
            ids.len() <= RECORDS,
            "{path}: the continuation did not move on"
        );
    }
    (ids, count)
}

#[tokio::test]
async fn an_event_time_list_follows_next_links_in_instant_order() {
    let db_dir = tempfile::tempdir().expect("create isolated query-plane db dir");
    let db_url = format!("file:{}", db_dir.path().join("event-time.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let state = build_turso_state("odata-read-event-time-pages", store);
    for index in 0..RECORDS {
        let (status, body) = post_json(
            &state,
            "/tdata/Orders",
            serde_json::json!({ "id": format!("ord-{index:02}"), "Currency": "USD" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create {index}: {body:?}");
    }
    // Move some early records again, so the last move's order is not the
    // order they were made in.
    for index in (0..RECORDS).step_by(4) {
        let (status, body) = patch_json(
            &state,
            &format!("/tdata/Orders('ord-{index:02}')"),
            serde_json::json!({ "Currency": "EUR" }),
        )
        .await;
        assert!(status.is_success(), "patch {index}: {body:?}");
    }

    for (property, descending) in [("last_event_at", true), ("first_event_at", false)] {
        let mut want = keys(&state, property).await;
        want.sort_by(|a, b| {
            let by_key = if descending {
                b.1.cmp(&a.1)
            } else {
                a.1.cmp(&b.1)
            };
            by_key.then_with(|| a.0.cmp(&b.0))
        });
        let want: Vec<String> = want.into_iter().map(|(id, _)| id).collect();
        let direction = if descending { "desc" } else { "asc" };
        let (ids, count) = walk(
            &state,
            &format!("/tdata/Orders?$orderby={property}%20{direction}&$top=4&$count=true"),
        )
        .await;
        assert_eq!(ids, want, "{property} {direction}");
        assert_eq!(count, Some(RECORDS as u64), "{property} {direction}");
    }

    let (eur, count) = walk(
        &state,
        "/tdata/Orders?$filter=Currency%20eq%20'EUR'&$orderby=last_event_at%20desc&$top=2&$count=true",
    )
    .await;
    assert_eq!(count, Some(RECORDS.div_ceil(4) as u64));
    assert_eq!(eur.len(), RECORDS.div_ceil(4));
}
