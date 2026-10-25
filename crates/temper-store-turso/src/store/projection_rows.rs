//! An entity's query-plane rows, written inside a transaction the caller
//! holds: the journal append's, so a move's projection commits with its
//! event, or the projection writer's own.

use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;

use super::field_index::{canonical_projection_status, indexed_projection_fields, projection_hash};
use crate::driver::{Transaction, Value, params, params_from_iter};

/// One entity's projection, as its events at `sequence_nr` leave it.
pub(super) struct ProjectionWrite<'a> {
    pub(super) tenant: &'a str,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) status: &'a str,
    pub(super) fields: &'a serde_json::Value,
    pub(super) state: &'a serde_json::Value,
    pub(super) sequence_nr: u64,
}

/// Write `row` in `tx`: its catalog row, and its field-index rows when its
/// indexed values changed. A row already at a later sequence is kept.
pub(super) async fn write_projection(
    tx: &Transaction<'_>,
    row: &ProjectionWrite<'_>,
) -> Result<(), PersistenceError> {
    let ProjectionWrite {
        tenant,
        entity_type,
        entity_id,
        fields,
        state,
        ..
    } = *row;
    let status = canonical_projection_status(row.status, state);
    let new_hash = projection_hash(status, fields);
    let fields_json = serde_json::to_string(fields)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let state_json = serde_json::to_string(state)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let updated_at = sim_now().to_rfc3339();
    let sequence_nr = i64::try_from(row.sequence_nr).unwrap_or(i64::MAX);

    let existing = {
        let mut rows = tx
            .query(
                "SELECT sequence_nr, projection_hash FROM entity_catalog \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(found) => Some((
                found.get::<i64>(0).map_err(storage_error)?,
                found.get::<Option<String>>(1).map_err(storage_error)?,
            )),
            None => None,
        }
    };
    if existing
        .as_ref()
        .is_some_and(|(existing, _)| *existing > sequence_nr)
    {
        return Ok(());
    }

    tx.execute(
        "INSERT INTO entity_catalog \
         (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 2, ?9) \
         ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
             status = excluded.status, \
             fields = excluded.fields, \
             state = excluded.state, \
             updated_at = excluded.updated_at, \
             sequence_nr = excluded.sequence_nr, \
             projection_version = excluded.projection_version, \
             projection_hash = excluded.projection_hash",
        params![
            tenant,
            entity_type,
            entity_id,
            status,
            fields_json.as_str(),
            state_json.as_str(),
            updated_at.as_str(),
            sequence_nr,
            new_hash.as_str(),
        ],
    )
    .await
    .map_err(storage_error)?;

    // The field-index rows follow the indexed values; unchanged, they stay.
    let unchanged = existing.is_some_and(|(_, hash)| hash.as_deref() == Some(new_hash.as_str()));
    if unchanged {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM entity_field_index WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
        params![tenant, entity_type, entity_id],
    )
    .await
    .map_err(storage_error)?;
    let indexed = indexed_projection_fields(status, fields);
    if indexed.is_empty() {
        return Ok(());
    }
    let mut sql = String::from(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) VALUES ",
    );
    let mut values = Vec::with_capacity(indexed.len() * 6);
    for (index, (field_name, field_value)) in indexed.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?, ?, ?, ?)");
        values.push(Value::from(tenant.to_string()));
        values.push(Value::from(entity_type.to_string()));
        values.push(Value::from(entity_id.to_string()));
        values.push(Value::from(field_name.clone()));
        values.push(
            field_value
                .as_ref()
                .map(|value| Value::from(value.clone()))
                .unwrap_or(Value::Null),
        );
        values.push(Value::from(status.to_string()));
    }
    tx.execute(&sql, params_from_iter(values))
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Remove an entity's query-plane rows in `tx`.
pub(super) async fn remove_projection(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<(), PersistenceError> {
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
    Ok(())
}
