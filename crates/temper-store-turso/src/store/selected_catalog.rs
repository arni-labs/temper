//! Catalog reads for a `$select` list page: only the selected values, not
//! each record's whole `fields` and `state` documents.

use temper_runtime::persistence::{PersistenceError, catalog_state_metadata_key, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::field_index::EntityCatalogRow;
use super::query_page::turso_json_path_literal;

/// Properties a row answers from its own columns, so a selected read loads
/// nothing for them.
const ROW_COLUMNS: [&str; 5] = [
    "entity_type",
    "entity_id",
    "status",
    "events",
    "sequence_nr",
];

impl TursoEventStore {
    /// Batch-load catalog rows holding only `selected_fields`, each read
    /// where [`Self::load_entity_catalog_rows`] would find it.
    ///
    /// Selected kernel event metadata comes back in the row's `state`, read
    /// from the stored state alone and `first_event_at` from its column, so
    /// an app field of the same name never supplies it. Every other selected
    /// property comes back in `fields`: a newer unchanged projection's event
    /// counters first, then the state's top level, the state's `fields`, and
    /// the `fields` column. A property no row holds is left out.
    ///
    /// Returns `None` for a selection of `fields` or `state`, which are whole
    /// documents; the caller loads whole rows instead.
    #[instrument(skip_all, fields(
        otel.name = "turso.load_selected_entity_catalog_rows",
        tenant, entity_type,
        entity_count = entity_ids.len(),
    ))]
    pub async fn load_selected_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        if selected_fields
            .iter()
            .any(|field| matches!(field.as_str(), "fields" | "state"))
        {
            return Ok(None);
        }
        if entity_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut selected: Vec<&str> = Vec::new();
        for field in selected_fields {
            if !ROW_COLUMNS.contains(&field.as_str()) && !selected.contains(&field.as_str()) {
                selected.push(field);
            }
        }
        let columns: String = selected
            .iter()
            .map(|field| format!(", {}", selected_value_sql(field)))
            .collect();
        let placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.entity_id, c.status, COALESCE(k.sequence_nr, c.sequence_nr){columns} \
             FROM entity_catalog c LEFT JOIN entity_catalog_counters k \
               ON k.tenant = c.tenant AND k.entity_type = c.entity_type \
              AND k.entity_id = c.entity_id \
             WHERE c.tenant = ? AND c.entity_type = ? AND c.entity_id IN ({placeholders}) \
             ORDER BY c.entity_id"
        );
        let mut params: Vec<crate::driver::Value> = Vec::with_capacity(entity_ids.len() + 2);
        params.push(crate::driver::Value::Text(tenant.to_string()));
        params.push(crate::driver::Value::Text(entity_type.to_string()));
        for id in entity_ids {
            params.push(crate::driver::Value::Text(id.clone()));
        }

        let conn = self.configured_connection().await?;
        let mut rows = conn.query(&sql, params).await.map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(2).map_err(storage_error)?;
            let mut fields = serde_json::Map::new();
            let mut metadata = serde_json::Map::new();
            for (offset, field) in selected.iter().enumerate() {
                let text: Option<String> = row.get(3 + offset).map_err(storage_error)?;
                let Some(text) = text else { continue };
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                let into = if catalog_state_metadata_key(field) {
                    &mut metadata
                } else {
                    &mut fields
                };
                into.insert((*field).to_string(), value);
            }
            let any_metadata = selected
                .iter()
                .any(|field| catalog_state_metadata_key(field));
            out.push(EntityCatalogRow {
                entity_id,
                status,
                fields: serde_json::Value::Object(fields),
                state: any_metadata.then_some(serde_json::Value::Object(metadata)),
                sequence_nr: sequence_nr.max(0) as u64,
            });
        }
        Ok(Some(out))
    }
}

/// SQL for one selected property's JSON text, NULL when the row lacks it.
fn selected_value_sql(field: &str) -> String {
    let path = turso_json_path_literal(field);
    if field == "first_event_at" {
        return "CASE WHEN c.first_event_at IS NULL THEN NULL \
                ELSE json_quote(c.first_event_at) END"
            .to_string();
    }
    if catalog_state_metadata_key(field) {
        return format!("c.state -> {path}");
    }
    format!(
        "COALESCE(k.counters -> {path}, c.state -> {path}, \
         (c.state -> '$.fields') -> {path}, c.fields -> {path})"
    )
}

#[cfg(test)]
#[path = "selected_catalog_test.rs"]
mod tests;
