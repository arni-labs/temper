use temper_odata::query::types::{FilterExpr, QueryOptions};

use crate::storage::EntityCatalogRow;

/// The properties a `$select` read loads from the catalog, or `None` when it
/// loads whole records.
///
/// A projected page still filters, sorts and continues in the server, so it
/// loads what those read as well as what the caller selected: every property
/// the filter and `$orderby` name (the catalog `status` for `Status`), and
/// `entity_id`, which paging's cursor ends with. The page is cut
/// back to the caller's `$select` after the cursor is taken. `$expand` reads
/// whole related records, so it loads whole records too.
pub(in crate::odata) fn catalog_select_projection_fields(
    query_options: &QueryOptions,
) -> Option<Vec<String>> {
    let select = query_options.select.as_deref()?;
    if select.is_empty() || query_options.expand.is_some() {
        return None;
    }
    let mut fields = select.to_vec();
    let mut read = |property: &str| {
        // A filter or sort on `Status` compares the catalog status, as a
        // whole record resolves it.
        let property = if property == "Status" {
            "status"
        } else {
            property
        };
        add_field(&mut fields, property);
    };
    if let Some(filter) = &query_options.filter {
        for_each_property(filter, &mut read);
    }
    for clause in query_options.orderby.iter().flatten() {
        read(&clause.property);
    }
    add_field(&mut fields, "entity_id");
    Some(fields)
}

fn add_field(fields: &mut Vec<String>, property: &str) {
    if !fields.iter().any(|field| field == property) {
        fields.push(property.to_string());
    }
}

fn for_each_property(expr: &FilterExpr, read: &mut impl FnMut(&str)) {
    match expr {
        FilterExpr::BinaryOp { left, right, .. } => {
            for_each_property(left, read);
            for_each_property(right, read);
        }
        FilterExpr::UnaryOp { operand, .. } => for_each_property(operand, read),
        FilterExpr::FunctionCall { args, .. } => {
            for arg in args {
                for_each_property(arg, read);
            }
        }
        FilterExpr::Property(property) => read(property),
        FilterExpr::Literal(_) => {}
    }
}

fn selected_catalog_property(
    entity_type: &str,
    row: &EntityCatalogRow,
    prop: &str,
) -> Option<serde_json::Value> {
    // Kernel event metadata comes from the record's state alone: an app field
    // of the same name never supplies it. A sparse row's `state` holds the
    // selected metadata its store read from the state.
    if temper_spec::automaton::is_event_metadata_field(prop) {
        return Some(
            row.state
                .as_ref()
                .and_then(|state| state.get(prop))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
    }
    match prop {
        "entity_type" => return Some(serde_json::json!(entity_type)),
        "entity_id" => return Some(serde_json::json!(row.entity_id)),
        "status" => return Some(serde_json::json!(row.status)),
        "events" => return Some(serde_json::json!([])),
        "sequence_nr" => return Some(serde_json::json!(row.sequence_nr)),
        _ => {}
    }

    if let Some(state) = row.state.as_ref() {
        if let Some(value) = state.get(prop) {
            return Some(value.clone());
        }
        if let Some(value) = state.get("fields").and_then(|fields| fields.get(prop)) {
            return Some(value.clone());
        }
    }

    if let Some(value) = row.fields.get(prop) {
        return Some(value.clone());
    }

    match prop {
        "fields" => Some(row.fields.clone()),
        "item_count" => Some(serde_json::json!(0)),
        "counters" => Some(serde_json::json!({})),
        "booleans" => Some(serde_json::json!({})),
        "lists" => Some(serde_json::json!({})),
        "total_event_count" => Some(serde_json::json!(row.sequence_nr)),
        _ => None,
    }
}

pub(in crate::odata) fn catalog_row_to_selected_entity_body(
    entity_type: &str,
    entity_set_name: &str,
    row: EntityCatalogRow,
    select: &[String],
) -> serde_json::Value {
    let mut selected = serde_json::Map::new();
    for prop in select {
        if let Some(value) = selected_catalog_property(entity_type, &row, prop) {
            selected.insert(prop.clone(), value);
        }
    }
    selected.insert(
        "@odata.id".to_string(),
        serde_json::json!(format!("{}('{}')", entity_set_name, row.entity_id)),
    );
    serde_json::Value::Object(selected)
}
