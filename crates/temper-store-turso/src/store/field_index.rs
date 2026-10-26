//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use crate::driver::params;
use std::collections::{BTreeMap, BTreeSet};
use temper_runtime::persistence::{PersistenceError, catalog_state_from_storage, storage_error};
use tracing::instrument;

use super::write_gate::WritePriority;
use super::{TursoEventStore, TursoQueryProjectionRow};

/// Match the Postgres query-plane limit: large scalar values stay in
/// `entity_catalog.fields` but are not copied into the filter index.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;

/// Sparse projected field values loaded from the durable query plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedEntityFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// One row from `entity_catalog`, with the full JSON fields blob preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityCatalogRow {
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: Option<serde_json::Value>,
    pub sequence_nr: u64,
}

/// One durable projection upsert in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryProjectionUpsert {
    pub entity_type: String,
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: serde_json::Value,
    pub indexed_fields: serde_json::Value,
    pub sequence_nr: u64,
    pub known_new: bool,
}

impl TursoEventStore {
    /// Upsert the durable query-plane projection for a single entity.
    ///
    /// Maintains both:
    /// - `entity_catalog`: one live row per entity
    /// - `entity_field_index`: EAV rows for OData filter push-down
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_query_projection",
        tenant, entity_type, entity_id,
    ))]
    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": status,
            "item_count": 0,
            "counters": {},
            "booleans": {},
            "lists": {},
            "fields": fields,
            "events": [],
            "total_event_count": sequence_nr,
            "sequence_nr": sequence_nr
        });
        self.upsert_query_projection_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            sequence_nr,
        )
        .await
    }

    /// Backwards-compatible alias for the old name.
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_field_index",
        tenant, entity_type, entity_id,
    ))]
    pub async fn upsert_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        let state = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": status,
            "item_count": 0,
            "counters": {},
            "booleans": {},
            "lists": {},
            "fields": fields,
            "events": [],
            "total_event_count": 0,
            "sequence_nr": 0
        });
        self.upsert_query_projection_with_state(
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            &state,
            0,
        )
        .await
    }

    /// Remove the durable query-plane projection for a single entity.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_query_projection",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.remove_query_projection", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_catalog WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Backwards-compatible alias for the old name.
    #[instrument(skip_all, fields(
        otel.name = "turso.remove_field_index",
        tenant, entity_type, entity_id,
    ))]
    pub async fn remove_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    /// Query the field index with a pre-built SQL WHERE clause.
    ///
    /// Returns distinct entity IDs matching the filter. The `where_clause`
    /// and `params` are generated by the OData-to-SQL translator
    /// (`filter_sql::try_translate_filter`).
    ///
    /// Parameters use `String` (not `crate::driver::Value`) so the interface is
    /// usable from crates that don't depend on the database driver directly.
    #[instrument(skip_all, fields(
        otel.name = "turso.query_field_index",
        tenant, entity_type,
    ))]
    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let sql = format!(
            "SELECT DISTINCT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause})"
        );

        // Prepend the tenant/entity_type params. The translator's params
        // start at ?3 so there's no collision.
        let mut all_params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(crate::driver::Value::from));

        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(all_params))
            .await
            .map_err(storage_error)?;

        let mut ids = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            if let Ok(id) = row.get::<String>(0) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Load a sparse set of projected fields for many entities in one query.
    ///
    /// Returns one row per projected entity that exists in the durable query
    /// plane. Missing entity ids are omitted from the result.
    #[instrument(skip_all, fields(
        otel.name = "turso.load_query_projection_fields_many",
        tenant, entity_type,
        entity_count = entity_ids.len(),
        field_count = field_names.len(),
    ))]
    pub async fn load_query_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Vec<ProjectedEntityFieldsRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.configured_connection().await?;
        let requested_fields: BTreeSet<&str> = field_names.iter().copied().collect();

        let entity_placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let field_placeholders = std::iter::repeat_n("?", requested_fields.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.entity_id, c.status, f.field_name, f.field_value \
             FROM entity_catalog c \
             LEFT JOIN entity_field_index f \
               ON c.tenant = f.tenant \
              AND c.entity_type = f.entity_type \
              AND c.entity_id = f.entity_id \
              AND f.field_name IN ({field_placeholders}) \
             WHERE c.tenant = ? \
               AND c.entity_type = ? \
               AND c.entity_id IN ({entity_placeholders}) \
             ORDER BY c.entity_id, f.field_name"
        );

        let mut params: Vec<crate::driver::Value> =
            Vec::with_capacity(2 + requested_fields.len() + entity_ids.len());
        for field_name in &requested_fields {
            params.push(crate::driver::Value::from((*field_name).to_string()));
        }
        params.push(crate::driver::Value::from(tenant.to_string()));
        params.push(crate::driver::Value::from(entity_type.to_string()));
        for entity_id in entity_ids {
            params.push(crate::driver::Value::from(entity_id.clone()));
        }

        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(params))
            .await
            .map_err(storage_error)?;

        let mut by_entity: BTreeMap<String, ProjectedEntityFieldsRow> = BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let field_name: Option<String> = row.get(2).map_err(storage_error)?;
            let field_value: Option<String> = row.get(3).map_err(storage_error)?;

            let entry =
                by_entity
                    .entry(entity_id.clone())
                    .or_insert_with(|| ProjectedEntityFieldsRow {
                        entity_id: entity_id.clone(),
                        status,
                        fields: requested_fields
                            .iter()
                            .map(|field| ((*field).to_string(), None))
                            .collect(),
                    });

            if let Some(field_name) = field_name
                && requested_fields.contains(field_name.as_str())
            {
                entry.fields.insert(field_name, field_value);
            }
        }

        Ok(entity_ids
            .iter()
            .filter_map(|entity_id| by_entity.remove(entity_id))
            .collect())
    }

    /// Export every durable query-plane projection, optionally scoped to one tenant.
    ///
    /// Turso keeps projected fields in `entity_field_index`, so this reconstructs
    /// the full projected fields JSON preserved in the catalog.
    #[instrument(skip_all, fields(
        otel.name = "turso.export_query_projections",
        tenant = tenant.unwrap_or("*"),
    ))]
    pub async fn export_query_projections(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<TursoQueryProjectionRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, status, fields, sequence_nr \
                 FROM entity_catalog \
                 WHERE (?1 IS NULL OR tenant = ?1) \
                 ORDER BY tenant, entity_type, entity_id",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant: String = row.get(0).map_err(storage_error)?;
            let entity_type: String = row.get(1).map_err(storage_error)?;
            let entity_id: String = row.get(2).map_err(storage_error)?;
            let status: String = row.get(3).map_err(storage_error)?;
            let fields_text: String = row.get(4).map_err(storage_error)?;
            let sequence_nr = row.get::<i64>(5).map_err(storage_error)?.max(0) as u64;
            let fields = serde_json::from_str(&fields_text)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            out.push(TursoQueryProjectionRow {
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                sequence_nr,
            });
        }

        Ok(out)
    }

    /// Return projected entity counts grouped by tenant.
    #[instrument(skip_all, fields(otel.name = "turso.projected_entity_counts_by_tenant"))]
    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Vec<(String, u64)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, COUNT(*) AS projected_count \
                 FROM entity_catalog \
                 GROUP BY tenant \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut counts = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant: String = row.get(0).map_err(storage_error)?;
            let count: i64 = row.get(1).map_err(storage_error)?;
            counts.push((tenant, count.max(0) as u64));
        }
        Ok(counts)
    }

    /// Batch-load full entity catalog rows for a list of entity IDs.
    ///
    /// Returns only rows that exist in the catalog; missing IDs are silently
    /// omitted. Mirrors the Postgres impl so the OData read path can stay
    /// backend-agnostic. Each row's `state` comes back whole, its fields put
    /// back from the `fields` column ([`catalog_state_from_storage`]).
    #[instrument(skip_all, fields(
        otel.name = "turso.load_entity_catalog_rows",
        tenant, entity_type,
        entity_count = entity_ids.len(),
    ))]
    pub async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Vec<EntityCatalogRow>, PersistenceError> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let placeholders = std::iter::repeat_n("?", entity_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT entity_id, status, fields, state, sequence_nr \
             FROM entity_catalog \
             WHERE tenant = ? AND entity_type = ? AND entity_id IN ({placeholders}) \
             ORDER BY entity_id"
        );
        let mut params: Vec<crate::driver::Value> = Vec::with_capacity(entity_ids.len() + 2);
        params.push(crate::driver::Value::Text(tenant.to_string()));
        params.push(crate::driver::Value::Text(entity_type.to_string()));
        for id in entity_ids {
            params.push(crate::driver::Value::Text(id.clone()));
        }
        let mut rows = conn.query(&sql, params).await.map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let status: String = row.get(1).map_err(storage_error)?;
            let fields_text: String = row.get(2).map_err(storage_error)?;
            let state_text: Option<String> = row.get(3).map_err(storage_error)?;
            let sequence_nr: i64 = row.get(4).map_err(storage_error)?;
            let fields: serde_json::Value = serde_json::from_str(&fields_text)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let state = state_text
                .map(|state_text| serde_json::from_str(&state_text))
                .transpose()
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?
                .map(|stored| catalog_state_from_storage(&fields, stored));
            out.push(EntityCatalogRow {
                entity_id,
                status,
                fields,
                state,
                sequence_nr: sequence_nr.max(0) as u64,
            });
        }
        Ok(out)
    }
}

/// Convert a JSON scalar to a TEXT representation for the index.
///
/// Returns `None` for non-scalar types (objects, arrays) — these are not indexed.
/// `null` values return `None` (stored as SQL NULL in field_value).
fn scalar_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

pub(super) fn indexed_projection_fields(
    status: &str,
    fields: &serde_json::Value,
) -> Vec<(String, Option<String>)> {
    let mut indexed_fields = Vec::new();

    if let Some(obj) = fields.as_object() {
        for (field_name, value) in obj {
            if field_name == "Status" {
                continue;
            }

            let field_value = scalar_to_text(value);
            if field_value.is_none() && !value.is_null() {
                continue;
            }
            if field_value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_INDEXABLE_FIELD_VALUE_BYTES)
            {
                continue;
            }

            indexed_fields.push((field_name.clone(), field_value));
        }
    }

    // Also index the status as a pseudo-field so `$filter=Status eq 'Active'` works.
    indexed_fields.push(("Status".to_string(), Some(status.to_string())));
    indexed_fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_to_text_converts_primitives() {
        assert_eq!(
            scalar_to_text(&serde_json::json!("hello")),
            Some("hello".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(42)),
            Some("42".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(true)),
            Some("true".to_string())
        );
        assert_eq!(scalar_to_text(&serde_json::Value::Null), None);
    }

    #[test]
    fn scalar_to_text_skips_complex_types() {
        assert_eq!(scalar_to_text(&serde_json::json!({"a": 1})), None);
        assert_eq!(scalar_to_text(&serde_json::json!([1, 2, 3])), None);
    }

    #[test]
    fn indexed_projection_fields_skips_oversized_scalars() {
        let long = "x".repeat(MAX_INDEXABLE_FIELD_VALUE_BYTES + 1);
        let fields = serde_json::json!({
            "Title": "short",
            "Payload": long,
        });

        let indexed = indexed_projection_fields("Active", &fields);

        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Title" && value.as_deref() == Some("short"))
        );
        assert!(indexed.iter().all(|(name, _)| name != "Payload"));
        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Status" && value.as_deref() == Some("Active"))
        );
    }
}
