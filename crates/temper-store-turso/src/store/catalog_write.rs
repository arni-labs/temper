//! Writes of the durable query-plane catalog: one `entity_catalog` row per
//! record and its `entity_field_index` rows.
//!
//! A row holds the record's fields once: in its `fields` column, and not again
//! inside its `state` column (see [`catalog_state_for_storage`]). A write
//! names only the columns it changes. Turso's change log keeps the whole row
//! before and after every UPDATE, plus each column the UPDATE sets, so a write
//! that changes nothing is skipped, a move that changes no field leaves
//! `fields` out of its UPDATE, and the field index is rebuilt only when the
//! fields or the status change.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, catalog_state_for_storage, storage_error};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::TursoEventStore;
use super::field_index::{QueryProjectionUpsert, indexed_projection_fields};
use super::write_gate::WritePriority;
use crate::driver::{Value, params_from_iter};

/// The status a projection is stored under: the projected state's own status
/// when it names one, else the caller's.
fn canonical_projection_status<'a>(fallback: &'a str, state: &'a serde_json::Value) -> &'a str {
    state
        .get("status")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
        .unwrap_or(fallback)
}

/// `projection_hash`: the SHA-256 of the stored `fields` text, as the Postgres
/// store keeps it. Equal hashes mean the `fields` column needs no write.
fn fields_hash(fields_json: &str) -> String {
    format!("{:x}", Sha256::digest(fields_json.as_bytes()))
}

fn serialize(value: &serde_json::Value) -> Result<String, PersistenceError> {
    serde_json::to_string(value).map_err(|error| PersistenceError::Serialization(error.to_string()))
}

/// The parts of a stored catalog row that decide what a write changes.
struct StoredCatalogRow {
    sequence_nr: i64,
    status: String,
    projection_hash: String,
    state: Option<String>,
}

/// What one projection write does to the stored row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogRowWrite {
    /// A newer move already wrote the row.
    Stale,
    /// The row already holds this projection.
    Unchanged,
    /// There is no row yet.
    Insert,
    /// The row changes. Each flag says whether that part changes with it.
    Update {
        fields: bool,
        state: bool,
        index: bool,
    },
}

/// One projection, serialized as it is stored.
struct PreparedProjection {
    entity_type: String,
    entity_id: String,
    status: String,
    fields_json: String,
    state_json: String,
    projection_hash: String,
    sequence_nr: i64,
    indexed_fields: Vec<(String, Option<String>)>,
    known_new: bool,
}

impl PreparedProjection {
    fn new(projection: &QueryProjectionUpsert, status: &str) -> Result<Self, PersistenceError> {
        let fields_json = serialize(&projection.fields)?;
        let stored_state = catalog_state_for_storage(&projection.fields, &projection.state);
        Ok(Self {
            entity_type: projection.entity_type.clone(),
            entity_id: projection.entity_id.clone(),
            status: status.to_string(),
            projection_hash: fields_hash(&fields_json),
            fields_json,
            state_json: serialize(&stored_state)?,
            sequence_nr: i64::try_from(projection.sequence_nr).unwrap_or(i64::MAX),
            indexed_fields: indexed_projection_fields(status, &projection.indexed_fields),
            known_new: projection.known_new,
        })
    }

    fn key(&self) -> (String, String) {
        (self.entity_type.clone(), self.entity_id.clone())
    }

    fn write_against(&self, stored: Option<&StoredCatalogRow>) -> CatalogRowWrite {
        let Some(stored) = stored else {
            return CatalogRowWrite::Insert;
        };
        if stored.sequence_nr > self.sequence_nr {
            return CatalogRowWrite::Stale;
        }
        let fields = stored.projection_hash != self.projection_hash;
        let state = stored.state.as_deref() != Some(self.state_json.as_str());
        let status = stored.status != self.status;
        if !fields && !state && !status && stored.sequence_nr == self.sequence_nr {
            return CatalogRowWrite::Unchanged;
        }
        CatalogRowWrite::Update {
            fields,
            state,
            index: fields || status,
        }
    }

    fn as_stored(&self) -> StoredCatalogRow {
        StoredCatalogRow {
            sequence_nr: self.sequence_nr,
            status: self.status.clone(),
            projection_hash: self.projection_hash.clone(),
            state: Some(self.state_json.clone()),
        }
    }
}

impl TursoEventStore {
    /// Upsert the durable query-plane projection for one entity: its
    /// `entity_catalog` row and, when its fields or status change, its
    /// `entity_field_index` rows.
    ///
    /// `fields` is the query projection of `state.fields`; the row stores
    /// each of the record's fields once. A projection older than the stored
    /// row is ignored, and one the row already holds writes nothing.
    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    pub async fn upsert_query_projection_with_state(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let projection = QueryProjectionUpsert {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: status.to_string(),
            fields: fields.clone(),
            state: state.clone(),
            indexed_fields: fields.clone(),
            sequence_nr,
            known_new: false,
        };
        let status = canonical_projection_status(status, state);
        let prepared = PreparedProjection::new(&projection, status)?;
        self.write_projections("turso.upsert_query_projection", tenant, vec![prepared])
            .await
    }

    /// Upsert several durable query-plane projections in one transaction.
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_query_projections",
        tenant,
        projection_count = projections.len(),
    ))]
    pub async fn upsert_query_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        if projections.is_empty() {
            return Ok(());
        }
        let prepared = projections
            .iter()
            .map(|projection| PreparedProjection::new(projection, &projection.status))
            .collect::<Result<Vec<_>, _>>()?;
        self.write_projections("turso.upsert_query_projections", tenant, prepared)
            .await
    }

    /// Write each projection against the row it finds, in order, in one
    /// transaction.
    ///
    /// Boxed where it is built: a projection write runs at the bottom of
    /// nested dispatches, and the boxed future keeps its state off their stack.
    fn write_projections<'a>(
        &'a self,
        operation: &'static str,
        tenant: &'a str,
        projections: Vec<PreparedProjection>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>> {
        Box::pin(async move {
            let _write_permit = self
                .acquire_write_permit(operation, WritePriority::Low)
                .await?;
            let conn = self.configured_connection().await?;
            let tx = conn.begin_immediate().await.map_err(storage_error)?;
            let mut stored = BTreeMap::new();
            if let Some(select) = stored_rows_statement(tenant, &projections) {
                let mut rows = tx
                    .query(&select.sql, params_from_iter(select.values))
                    .await
                    .map_err(storage_error)?;
                while let Some(row) = rows.next().await.map_err(storage_error)? {
                    let entity_type: String = row.get(0).map_err(storage_error)?;
                    let entity_id: String = row.get(1).map_err(storage_error)?;
                    let row = StoredCatalogRow {
                        sequence_nr: row.get(2).map_err(storage_error)?,
                        status: row.get(3).map_err(storage_error)?,
                        projection_hash: row.get(4).map_err(storage_error)?,
                        state: row.get(5).map_err(storage_error)?,
                    };
                    stored.insert((entity_type, entity_id), row);
                }
            }
            let updated_at = sim_now().to_rfc3339();

            // New rows go in one statement and the field index in two. Each
            // update sets its own columns, so updates run one per row, after
            // the inserts they may follow within the batch.
            let mut inserts = Vec::new();
            let mut updates = Vec::new();
            let mut reindexed = BTreeMap::new();
            for projection in &projections {
                let reindex = match projection.write_against(stored.get(&projection.key())) {
                    CatalogRowWrite::Stale | CatalogRowWrite::Unchanged => continue,
                    CatalogRowWrite::Insert => {
                        inserts.push(projection);
                        true
                    }
                    CatalogRowWrite::Update {
                        fields,
                        state,
                        index,
                    } => {
                        updates.push(update_statement(
                            tenant,
                            projection,
                            &updated_at,
                            fields,
                            state,
                        ));
                        index
                    }
                };
                if reindex {
                    reindexed.insert(projection.key(), projection);
                }
                stored.insert(projection.key(), projection.as_stored());
            }

            let statements = insert_statement(tenant, &inserts, &updated_at)
                .into_iter()
                .chain(updates)
                .chain(field_index_statements(
                    tenant,
                    &reindexed.into_values().collect::<Vec<_>>(),
                ));
            for statement in statements {
                tx.execute(&statement.sql, params_from_iter(statement.values))
                    .await
                    .map_err(storage_error)?;
            }

            tx.commit().await.map_err(storage_error)?;
            Ok(())
        })
    }
}

/// One SQL statement and its positional parameters.
struct Statement {
    sql: String,
    values: Vec<Value>,
}

/// The read of the stored rows of every projection not known to be new, or
/// `None` when every projection is new.
fn stored_rows_statement(tenant: &str, projections: &[PreparedProjection]) -> Option<Statement> {
    let candidates = projections
        .iter()
        .filter(|projection| !projection.known_new)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    let mut sql = String::from(
        "SELECT entity_type, entity_id, sequence_nr, status, projection_hash, state \
         FROM entity_catalog WHERE tenant = ? AND (",
    );
    let mut values = Vec::with_capacity(1 + candidates.len() * 2);
    values.push(Value::from(tenant.to_string()));
    for (index, projection) in candidates.iter().enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str("(entity_type = ? AND entity_id = ?)");
        values.push(Value::from(projection.entity_type.clone()));
        values.push(Value::from(projection.entity_id.clone()));
    }
    sql.push(')');
    Some(Statement { sql, values })
}

/// Insert new rows, or `None` when there are none. A row that appeared
/// meanwhile is overwritten only by a projection at least as new.
fn insert_statement(
    tenant: &str,
    projections: &[&PreparedProjection],
    updated_at: &str,
) -> Option<Statement> {
    if projections.is_empty() {
        return None;
    }
    let mut sql = String::from(
        "INSERT INTO entity_catalog \
         (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
         VALUES ",
    );
    let mut values = Vec::with_capacity(projections.len() * 9);
    for (index, projection) in projections.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, 2, ?)");
        values.extend([
            Value::from(tenant.to_string()),
            Value::from(projection.entity_type.clone()),
            Value::from(projection.entity_id.clone()),
            Value::from(projection.status.clone()),
            Value::from(projection.fields_json.clone()),
            Value::from(projection.state_json.clone()),
            Value::from(updated_at.to_string()),
            Value::from(projection.sequence_nr),
            Value::from(projection.projection_hash.clone()),
        ]);
    }
    sql.push_str(
        " ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
             status = excluded.status, \
             fields = excluded.fields, \
             state = excluded.state, \
             updated_at = excluded.updated_at, \
             sequence_nr = excluded.sequence_nr, \
             projection_version = excluded.projection_version, \
             projection_hash = excluded.projection_hash \
         WHERE entity_catalog.sequence_nr <= excluded.sequence_nr",
    );
    Some(Statement { sql, values })
}

/// Update an existing row, setting `fields` and `state` only when they change.
fn update_statement(
    tenant: &str,
    projection: &PreparedProjection,
    updated_at: &str,
    fields: bool,
    state: bool,
) -> Statement {
    let mut sql = String::from(
        "UPDATE entity_catalog SET status = ?, updated_at = ?, sequence_nr = ?, \
         projection_version = 2, projection_hash = ?",
    );
    let mut values = vec![
        Value::from(projection.status.clone()),
        Value::from(updated_at.to_string()),
        Value::from(projection.sequence_nr),
        Value::from(projection.projection_hash.clone()),
    ];
    if fields {
        sql.push_str(", fields = ?");
        values.push(Value::from(projection.fields_json.clone()));
    }
    if state {
        sql.push_str(", state = ?");
        values.push(Value::from(projection.state_json.clone()));
    }
    sql.push_str(" WHERE tenant = ? AND entity_type = ? AND entity_id = ?");
    values.push(Value::from(tenant.to_string()));
    values.push(Value::from(projection.entity_type.clone()));
    values.push(Value::from(projection.entity_id.clone()));
    Statement { sql, values }
}

/// Replace these entities' `entity_field_index` rows, each from its newest
/// projection. Deleting them all first handles fields that disappeared.
fn field_index_statements(tenant: &str, projections: &[&PreparedProjection]) -> Vec<Statement> {
    if projections.is_empty() {
        return Vec::new();
    }
    let mut delete = String::from("DELETE FROM entity_field_index WHERE tenant = ? AND (");
    let mut delete_values = Vec::with_capacity(1 + projections.len() * 2);
    delete_values.push(Value::from(tenant.to_string()));
    for (index, projection) in projections.iter().enumerate() {
        if index > 0 {
            delete.push_str(" OR ");
        }
        delete.push_str("(entity_type = ? AND entity_id = ?)");
        delete_values.push(Value::from(projection.entity_type.clone()));
        delete_values.push(Value::from(projection.entity_id.clone()));
    }
    delete.push(')');
    let mut statements = vec![Statement {
        sql: delete,
        values: delete_values,
    }];

    let mut insert = String::from(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) VALUES ",
    );
    let mut values = Vec::new();
    for projection in projections {
        for (field_name, field_value) in &projection.indexed_fields {
            if !values.is_empty() {
                insert.push_str(", ");
            }
            insert.push_str("(?, ?, ?, ?, ?, ?)");
            values.extend([
                Value::from(tenant.to_string()),
                Value::from(projection.entity_type.clone()),
                Value::from(projection.entity_id.clone()),
                Value::from(field_name.clone()),
                field_value
                    .as_ref()
                    .map(|value| Value::from(value.clone()))
                    .unwrap_or(Value::Null),
                Value::from(projection.status.clone()),
            ]);
        }
    }
    if !values.is_empty() {
        statements.push(Statement {
            sql: insert,
            values,
        });
    }
    statements
}

#[cfg(test)]
#[path = "catalog_write_test.rs"]
mod tests;
