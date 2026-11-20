use temper_odata::query::types::{OrderByClause, OrderDirection};

use super::{FinishedPage, WalkPage};
use crate::query_eval::apply_query_options;
use temper_odata::query::types::QueryOptions;

fn card(id: &str, v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(ref s) if s == "<missing>" => {
            serde_json::json!({"entity_id": id, "fields": {}})
        }
        v => serde_json::json!({"entity_id": id, "fields": {"v": v}}),
    }
}

fn order(direction: OrderDirection) -> Vec<OrderByClause> {
    vec![
        OrderByClause {
            property: "v".into(),
            direction,
        },
        OrderByClause {
            property: "entity_id".into(),
            direction: OrderDirection::Asc,
        },
    ]
}

fn ids(rows: &[serde_json::Value]) -> Vec<String> {
    rows.iter()
        .map(|row| row["entity_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The page a walk keeps, fed `batch` rows at a time.
fn walked(
    rows: &[serde_json::Value],
    order: Option<&[OrderByClause]>,
    skip: usize,
    top: usize,
    batch: usize,
) -> (Vec<String>, usize) {
    let mut page = WalkPage::new(order, skip, top);
    let mut held = 0;
    for chunk in rows.chunks(batch) {
        page.add(chunk.to_vec());
        held = held.max(page.documents_held());
    }
    let ids = match page.finish() {
        FinishedPage::Documents(documents) => ids(&documents),
        FinishedPage::Ids(ids) => ids,
    };
    (ids, held)
}

/// The rows a whole sort gives, `skip..skip + top`.
fn sorted_whole(
    rows: &[serde_json::Value],
    order: &[OrderByClause],
    skip: usize,
    top: usize,
) -> Vec<String> {
    let options = QueryOptions {
        orderby: Some(order.to_vec()),
        skip: Some(skip),
        top: Some(top),
        ..QueryOptions::default()
    };
    ids(&apply_query_options(rows.to_vec(), &options).0)
}

/// Explicit nulls, missing values, numbers and a string, in id order. A JSON
/// null used to be equal to every value, so where it fell decided the
/// answer: values 5, null, 1 with `$top=1` gave 5 sorted whole and 1 sorted
/// a row at a time.
fn mixed() -> Vec<serde_json::Value> {
    let values = [
        serde_json::json!(5),
        serde_json::Value::Null,
        serde_json::json!(1),
        serde_json::json!("<missing>"),
        serde_json::json!(3),
        serde_json::Value::Null,
        serde_json::json!("x"),
        serde_json::json!(1),
        serde_json::json!(true),
        serde_json::json!(-2.5),
        serde_json::Value::Null,
        serde_json::json!(5),
    ];
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| card(&format!("r-{index:02}"), value))
        .collect()
}

#[test]
fn a_sorted_walk_keeps_the_page_a_whole_sort_gives_for_any_batch() {
    let rows = mixed();
    for direction in [OrderDirection::Asc, OrderDirection::Desc] {
        let order = order(direction);
        for (skip, top) in [(0, 1), (0, 5), (2, 3), (7, 4), (11, 5), (30, 5)] {
            let want = sorted_whole(&rows, &order, skip, top);
            for batch in [1, 2, 3, 5, 12] {
                let (got, _) = walked(&rows, Some(&order), skip, top, batch);
                assert_eq!(
                    got, want,
                    "{direction:?} skip {skip} top {top} batch {batch}"
                );
            }
        }
    }
}

#[test]
fn a_null_sorts_with_the_missing_values_last_ascending_and_first_descending() {
    let rows = vec![
        card("a", serde_json::json!(5)),
        card("b", serde_json::Value::Null),
        card("c", serde_json::json!(1)),
    ];
    let asc = order(OrderDirection::Asc);
    assert_eq!(sorted_whole(&rows, &asc, 0, 3), ["c", "a", "b"]);
    assert_eq!(walked(&rows, Some(&asc), 0, 1, 1).0, ["c"]);
    let desc = order(OrderDirection::Desc);
    assert_eq!(sorted_whole(&rows, &desc, 0, 3), ["b", "a", "c"]);
    assert_eq!(walked(&rows, Some(&desc), 0, 1, 1).0, ["b"]);
}

#[test]
fn a_walk_past_a_skip_holds_no_skipped_document() {
    let rows = mixed();
    let order = order(OrderDirection::Asc);
    let (page, held) = walked(&rows, Some(&order), 6, 2, 3);
    assert_eq!(page.len(), 2);
    assert_eq!(
        held, 0,
        "a sorted walk with a skip holds keys, not documents"
    );
    let (page, held) = walked(&rows, None, 6, 2, 3);
    assert_eq!(page, ["r-06", "r-07"]);
    assert!(held <= 2, "in id order only the page is held, not {held}");
}
