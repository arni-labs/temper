//! Native query-plane page reads over the durable Turso catalog.
//!
//! A page ordered by an event time names the index that holds that order
//! (`schema/event_time_order.rs`), so it reads its rows in order and stops
//! after `LIMIT`: a keyset continuation seeks to the cursor in that index
//! instead of passing over the earlier rows (ADR-0187).

use temper_runtime::persistence::{
    CatalogOrderKey, PersistenceError, QueryFieldIndexPageQuery, storage_error,
};
use tracing::instrument;

use super::TursoEventStore;
use super::instrumentation::WriteConnection;
use crate::schema::{create_event_time_indexes as event_time_indexes, event_time_key_sql};

/// Create the catalog's event-time indexes. Building one evaluates its key
/// for every existing row; a failure leaves reads of that order to sort in
/// SQL, so it is reported and startup goes on.
pub(super) async fn create_event_time_indexes(conn: &WriteConnection) {
    for statement in event_time_indexes() {
        if let Err(error) = conn.execute(&statement, ()).await {
            tracing::error!(%error, statement, "event-time catalog index was not created");
        }
    }
}

impl TursoEventStore {
    /// Return a bounded page of entity IDs matching a translated query-plane filter.
    ///
    /// The returned count is the storage candidate count, before Cedar row
    /// authorization. Server read code must still materialize and authorize
    /// rows before using the count as an OData result count.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the SQL fails, or when `page.after` does
    /// not fit the page's order.
    #[instrument(skip_all, fields(
        otel.name = "turso.query_field_index_page",
        tenant, entity_type,
    ))]
    pub async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        page: &QueryFieldIndexPageQuery,
    ) -> Result<(Vec<String>, Option<usize>), PersistenceError> {
        let (where_clause, params) = page_where(page)?;
        if page.top == 0 {
            let count = if page.include_count {
                Some(
                    self.query_field_index_count(tenant, entity_type, &where_clause, params)
                        .await?,
                )
            } else {
                None
            };
            return Ok((Vec::new(), count));
        }

        // A window count over a page in an index's order would sort every
        // match by its key; a separate count reads only the filter.
        let window_count = page.include_count && !page.uses_event_time();
        let sql = turso_query_field_page_sql(page, &where_clause, params.len(), window_count);
        let mut all_params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.iter().cloned().map(crate::driver::Value::from));
        all_params.push(crate::driver::Value::from(
            page.top.min(i64::MAX as usize) as i64
        ));
        if page.skip > 0 {
            all_params.push(crate::driver::Value::from(
                page.skip.min(i64::MAX as usize) as i64
            ));
        }

        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(&sql, crate::driver::params_from_iter(all_params))
            .await
            .map_err(storage_error)?;

        let mut ids = Vec::new();
        let mut total_count = None;
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            if window_count && total_count.is_none() {
                let count: i64 = row.get(1).map_err(storage_error)?;
                total_count = Some(count.max(0) as usize);
            }
            ids.push(entity_id);
        }

        if page.include_count && total_count.is_none() {
            total_count = Some(
                self.query_field_index_count(tenant, entity_type, &where_clause, params)
                    .await?,
            );
        }

        Ok((ids, total_count))
    }

    async fn query_field_index_count(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<usize, PersistenceError> {
        let conn = self.configured_connection().await?;
        let index = super::field_index::catalog_filter_index(where_clause);
        let sql = format!(
            "SELECT COUNT(*) \
             FROM entity_catalog {index}\
             WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause})"
        );
        let mut all_params: Vec<crate::driver::Value> = vec![
            crate::driver::Value::from(tenant.to_string()),
            crate::driver::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(crate::driver::Value::from));
        let count: i64 = conn
            .query(&sql, crate::driver::params_from_iter(all_params))
            .await
            .map_err(storage_error)?
            .next()
            .await
            .map_err(storage_error)?
            .and_then(|row| row.get(0).ok())
            .unwrap_or(0);
        Ok(count.max(0) as usize)
    }
}

/// The page's filter with its event-time bounds and keyset, and every
/// parameter after the tenant and type (`?3` on).
pub(super) fn page_where(
    page: &QueryFieldIndexPageQuery,
) -> Result<(String, Vec<String>), PersistenceError> {
    let (conditions, condition_params) =
        page.catalog_conditions(page.params.len() + 3, &event_time_key_sql)?;
    let mut params = page.params.clone();
    params.extend(condition_params);
    Ok((format!("({}) AND {conditions}", page.where_clause), params))
}

/// The page's statement. `params` is how many filter, bound and keyset
/// parameters follow the tenant and type; `LIMIT` comes next, then
/// `OFFSET` only when the page skips rows.
pub(super) fn turso_query_field_page_sql(
    page: &QueryFieldIndexPageQuery,
    where_clause: &str,
    params: usize,
    window_count: bool,
) -> String {
    let select = if window_count {
        "entity_id, COUNT(*) OVER() AS total_count"
    } else {
        "entity_id"
    };
    let index = match page.leading_event_time() {
        Some((column, descending)) => format!(
            "INDEXED BY {} ",
            crate::schema::event_time_index_name(column, descending)
        ),
        None => super::field_index::catalog_filter_index(where_clause).to_string(),
    };
    let order_sql = turso_query_field_order_sql(page);
    let limit_param = params + 3;
    let offset = if page.skip > 0 {
        format!(" OFFSET ?{}", limit_param + 1)
    } else {
        String::new()
    };
    format!(
        "SELECT {select} \
         FROM entity_catalog {index}\
         WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause}) \
         ORDER BY {order_sql} \
         LIMIT ?{limit_param}{offset}"
    )
}

fn turso_query_field_order_sql(page: &QueryFieldIndexPageQuery) -> String {
    let mut clauses = Vec::new();
    for (key, descending) in page.total_order() {
        let direction = if descending { "DESC" } else { "ASC" };
        match key {
            CatalogOrderKey::EntityId => clauses.push(format!("entity_id {direction}")),
            CatalogOrderKey::Status => {
                clauses.push(format!("status IS NULL {direction}"));
                clauses.push(format!("status {direction}"));
            }
            CatalogOrderKey::EventTime(column) => {
                clauses.push(format!("{} {direction}", event_time_key_sql(column)));
            }
            CatalogOrderKey::Field(field_name) => {
                let path = turso_json_path_literal(field_name);
                clauses.push(format!("json_extract(fields, {path}) IS NULL {direction}"));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) IN ('integer', 'real') \
                     THEN CAST(json_extract(fields, {path}) AS REAL) END IS NULL ASC"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) IN ('integer', 'real') \
                     THEN CAST(json_extract(fields, {path}) AS REAL) END {direction}"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) NOT IN ('integer', 'real') \
                     THEN json_extract(fields, {path}) END IS NULL ASC"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) NOT IN ('integer', 'real') \
                     THEN json_extract(fields, {path}) END {direction}"
                ));
            }
        }
    }
    clauses.join(", ")
}

pub(super) fn turso_json_path_literal(field_name: &str) -> String {
    let escaped = field_name
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\'', "''");
    format!("'$.\"{escaped}\"'")
}
