//! The groups of a facet read, counted by the database (ADR-0188).

use sqlx::Row;
use temper_runtime::persistence::{CatalogFacetQuery, FieldClass, PersistenceError};

use crate::catalog_sql::PostgresCatalogSql;
use crate::platform::{postgres_placeholders, storage_error};
use crate::query_page::field_index_cte;
use crate::store::PostgresEventStore;

impl PostgresEventStore {
    /// One page of the groups `facets` asks for: each group's value (a
    /// number, a string, or null for the rows without one) and its count.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the SQL fails, or when the cursor's
    /// value is not of the key's type.
    pub async fn query_catalog_facets(
        &self,
        tenant: &str,
        entity_type: &str,
        facets: &CatalogFacetQuery,
    ) -> Result<Vec<(serde_json::Value, usize)>, PersistenceError> {
        let schema = self.schema.prefix();
        let from = format!("{schema}entity_catalog");
        let (statement, params) = facets.statement(&from, &PostgresCatalogSql)?;
        let sql = format!(
            "{}{}",
            field_index_cte(schema),
            postgres_placeholders(&statement, params.len() + 3)
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        query = query.bind(facets.top.min(i64::MAX as usize) as i64);
        let rows = query.fetch_all(self.pool()).await.map_err(storage_error)?;
        let mut groups = Vec::with_capacity(rows.len());
        for row in rows {
            let value = match facets.key.class() {
                FieldClass::Number => row
                    .try_get::<Option<f64>, _>("g")
                    .map_err(storage_error)?
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number),
                FieldClass::Text | FieldClass::Boolean => row
                    .try_get::<Option<String>, _>("g")
                    .map_err(storage_error)?
                    .map(serde_json::Value::String),
            };
            let count: i64 = row.try_get("n").map_err(storage_error)?;
            groups.push((
                value.unwrap_or(serde_json::Value::Null),
                count.max(0) as usize,
            ));
        }
        Ok(groups)
    }
}
