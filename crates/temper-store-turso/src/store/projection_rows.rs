//! An entity's query-plane rows, written inside a transaction the caller
//! holds: the journal append's, so a move's projection commits with its
//! event, or the projection writer's own.

use std::collections::BTreeMap;

use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;

use super::field_index::{canonical_projection_status, indexed_projection_fields};
use super::unchanged_projections::{
    ExistingProjection, drop_event_counters, projection_hash, store_event_counters,
};
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
    let fields_json = serde_json::to_string(fields)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let new_hash = projection_hash(status, &fields_json, state);
    let state_json = serde_json::to_string(state)
        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
    let updated_at = sim_now().to_rfc3339();
    let sequence_nr = i64::try_from(row.sequence_nr).unwrap_or(i64::MAX);

    let existing = {
        let mut rows = tx
            .query(
                "SELECT c.sequence_nr, c.projection_hash, k.sequence_nr \
                 FROM entity_catalog c LEFT JOIN entity_catalog_counters k \
                   ON k.tenant = c.tenant AND k.entity_type = c.entity_type \
                  AND k.entity_id = c.entity_id \
                 WHERE c.tenant = ?1 AND c.entity_type = ?2 AND c.entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(found) => Some(ExistingProjection::from_row(&found, 0)?),
            None => None,
        }
    };
    if existing
        .as_ref()
        .is_some_and(|existing| existing.sequence_nr() > sequence_nr)
    {
        return Ok(());
    }

    if let Some(existing) = existing
        .as_ref()
        .filter(|existing| existing.projection_hash == new_hash)
    {
        if sequence_nr > existing.sequence_nr() {
            store_event_counters(tx, [tenant, entity_type, entity_id], sequence_nr, state).await?;
        }
        return Ok(());
    }
    if existing
        .as_ref()
        .is_some_and(|existing| existing.counters_sequence_nr.is_some())
    {
        drop_event_counters(tx, [tenant, entity_type, entity_id]).await?;
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

    let indexed = indexed_projection_fields(status, fields);
    sync_field_index(tx, [tenant, entity_type, entity_id], status, &indexed).await
}

/// Bring an entity's field-index rows to `indexed` in `tx`. Only a row whose
/// value or status changed is written, and only a name the entity no longer
/// has is deleted, so a move that sets three fields writes three rows.
pub(super) async fn sync_field_index(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
    status: &str,
    indexed: &[(String, Option<String>)],
) -> Result<(), PersistenceError> {
    let mut stored = BTreeMap::new();
    {
        let mut rows = tx
            .query(
                "SELECT field_name, field_value, status FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let field_name: String = row.get(0).map_err(storage_error)?;
            let field_value: Option<String> = row.get(1).map_err(storage_error)?;
            let row_status: Option<String> = row.get(2).map_err(storage_error)?;
            stored.insert(field_name, (field_value, row_status));
        }
    }

    let changed = indexed
        .iter()
        .filter(|(field_name, field_value)| {
            stored.remove(field_name).is_none_or(|(value, row_status)| {
                value != *field_value || row_status.as_deref() != Some(status)
            })
        })
        .collect::<Vec<_>>();
    // What is left in `stored` names fields the entity no longer indexes.
    if !stored.is_empty() {
        let mut sql = String::from(
            "DELETE FROM entity_field_index \
             WHERE tenant = ? AND entity_type = ? AND entity_id = ? AND field_name IN (",
        );
        let mut values = vec![
            Value::from(tenant.to_string()),
            Value::from(entity_type.to_string()),
            Value::from(entity_id.to_string()),
        ];
        for (index, field_name) in stored.into_keys().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
            values.push(Value::from(field_name));
        }
        sql.push(')');
        tx.execute(&sql, params_from_iter(values))
            .await
            .map_err(storage_error)?;
    }
    if changed.is_empty() {
        return Ok(());
    }
    let mut sql = String::from(
        "INSERT INTO entity_field_index \
         (tenant, entity_type, entity_id, field_name, field_value, status) VALUES ",
    );
    let mut values = Vec::with_capacity(changed.len() * 6);
    for (index, (field_name, field_value)) in changed.into_iter().enumerate() {
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
    sql.push_str(
        " ON CONFLICT(tenant, entity_type, entity_id, field_name) DO UPDATE SET \
         field_value = excluded.field_value, status = excluded.status",
    );
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
    drop_event_counters(tx, [tenant, entity_type, entity_id]).await?;
    Ok(())
}
