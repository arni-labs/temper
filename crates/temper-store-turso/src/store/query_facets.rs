//! The groups of a facet read, counted by the database (ADR-0188).

use temper_runtime::persistence::{CatalogFacetQuery, PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use super::catalog_sql::TursoCatalogSql;
use super::field_index::catalog_filter_index;

impl TursoEventStore {
    /// One page of the groups `facets` asks for: each group's value (a
    /// number, a string, or null for the rows without one) and its count.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the SQL fails, or when the cursor's
    /// value is not of the key's type.
    #[instrument(skip_all, fields(otel.name = "turso.query_catalog_facets", tenant, entity_type))]
    pub async fn query_catalog_facets(
        &self,
        tenant: &str,
        entity_type: &str,
        facets: &CatalogFacetQuery,
    ) -> Result<Vec<(serde_json::Value, usize)>, PersistenceError> {
        let from = format!(
            "entity_catalog {}",
            catalog_filter_index(&facets.filter.where_clause)
        );
        let (sql, params) = facets.statement(from.trim_end(), &TursoCatalogSql)?;
        let mut all_params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(crate::driver::Value::from));
        all_params.push(crate::driver::Value::from(
            facets.top.min(i64::MAX as usize) as i64,
        ));
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(all_params))
            .await
            .map_err(storage_error)?;
        let mut groups = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let value = match row.get_value(0).map_err(storage_error)? {
                crate::driver::Value::Null => serde_json::Value::Null,
                crate::driver::Value::Integer(number) => serde_json::Value::from(number),
                crate::driver::Value::Real(number) => serde_json::Number::from_f64(number)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                crate::driver::Value::Text(text) => serde_json::Value::String(text),
                crate::driver::Value::Blob(_) => {
                    return Err(PersistenceError::Storage(
                        "a facet group's value is a blob".to_string(),
                    ));
                }
            };
            let count: i64 = row.get(1).map_err(storage_error)?;
            groups.push((value, count.max(0) as usize));
        }
        Ok(groups)
    }
}
