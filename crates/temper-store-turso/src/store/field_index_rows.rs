//! Writes one entity's `entity_field_index` rows, touching only the rows a
//! move changed.
//!
//! A synced database's change log records every row a write inserts, updates
//! or deletes. Rewriting all of an entity's index rows on every move put two
//! log rows per indexed field into each move; writing only what changed puts
//! one per changed field.

use std::collections::BTreeMap;

use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::driver::{Transaction, Value, params, params_from_iter};

/// Bring the entity's field-index rows to `indexed` in `tx`. Only a row whose
/// value or status changed is written, and only a field the entity no longer
/// indexes is deleted, so a move that sets three fields writes three rows.
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
        let mut values = Vec::with_capacity(3 + stored.len());
        values.push(Value::from(tenant.to_string()));
        values.push(Value::from(entity_type.to_string()));
        values.push(Value::from(entity_id.to_string()));
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
