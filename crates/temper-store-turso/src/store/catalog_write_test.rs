use super::*;

fn stored(sequence_nr: i64, status: &str, hash: &str, state: &str) -> StoredCatalogRow {
    StoredCatalogRow {
        sequence_nr,
        status: status.to_string(),
        projection_hash: hash.to_string(),
        state: Some(state.to_string()),
    }
}

fn prepared(sequence_nr: u64, status: &str, title: &str) -> PreparedProjection {
    let fields = serde_json::json!({"Title": title});
    let state = serde_json::json!({
        "status": status,
        "fields": fields.clone(),
        "sequence_nr": sequence_nr,
    });
    let projection = QueryProjectionUpsert {
        entity_type: "Note".to_string(),
        entity_id: "n-1".to_string(),
        status: status.to_string(),
        fields: fields.clone(),
        state,
        indexed_fields: fields,
        sequence_nr,
        known_new: false,
    };
    PreparedProjection::new(&projection, status).expect("prepare")
}

#[test]
fn a_write_names_only_what_changes() {
    let first = prepared(1, "Draft", "Plan");
    let row = first.as_stored();
    assert_eq!(first.write_against(None), CatalogRowWrite::Insert);
    assert_eq!(first.write_against(Some(&row)), CatalogRowWrite::Unchanged);

    let moved = prepared(2, "Active", "Plan");
    assert_eq!(
        moved.write_against(Some(&row)),
        CatalogRowWrite::Update {
            fields: false,
            state: true,
            index: true
        }
    );
    let edited = prepared(2, "Draft", "Plan v2");
    assert_eq!(
        edited.write_against(Some(&row)),
        CatalogRowWrite::Update {
            fields: true,
            state: true,
            index: true
        }
    );
    assert_eq!(
        first.write_against(Some(&stored(5, "Draft", "", ""))),
        CatalogRowWrite::Stale
    );
}
