//! Native bounded query-page reads for the Postgres query projection.
//!
//! A page ordered by an event time orders by the expression
//! [`event_time_key_sql`] gives, the one its index (migration 0029) is
//! built on, and resumes a keyset continuation with a range on it instead
//! of an `OFFSET` (ADR-0187).

use temper_runtime::persistence::{
    CatalogOrderKey, EventTimeColumn, MISSING_EVENT_TIME_KEY, PersistenceError,
    QueryFieldIndexPageQuery,
};

use crate::catalog_sql::{PostgresCatalogSql, postgres_string_literal};
use crate::platform::{postgres_placeholders, storage_error};
use crate::store::PostgresEventStore;

/// The sort key of `column` on an `entity_catalog` row, compared byte by
/// byte: the event time's text as the fixed-width
/// `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`, or `MISSING_EVENT_TIME_KEY` without
/// one. `first_event_at` is read from its column, which is what a catalog
/// read shows (`selected_catalog.rs`).
pub(crate) fn event_time_key_sql(column: EventTimeColumn) -> String {
    let text = match column {
        EventTimeColumn::First => "first_event_at",
        EventTimeColumn::Last => "(state ->> 'last_event_at')",
    };
    format!(
        "(COALESCE(substr({text}, 1, 19) || '.' || \
         substr(rtrim(substr({text}, 21), 'Z') || '000000000', 1, 9), \
         '{MISSING_EVENT_TIME_KEY}') COLLATE \"C\")"
    )
}

impl PostgresEventStore {
    /// Return a bounded page of entity IDs matching a translated query-plane
    /// filter, and their count when asked.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the SQL fails, or when `page.after` does
    /// not fit the page's order.
    pub async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        page: &QueryFieldIndexPageQuery,
    ) -> Result<(Vec<String>, Option<usize>), PersistenceError> {
        let (conditions, condition_params) =
            page.catalog_conditions(page.params.len() + 3, &PostgresCatalogSql)?;
        let mut params = page.params.clone();
        params.extend(condition_params);
        let filter = format!("({}) AND {conditions}", page.where_clause);
        let clause = postgres_placeholders(&filter, params.len() + 2);
        if page.top == 0 {
            let count = if page.include_count {
                Some(
                    self.query_field_index_count(tenant, entity_type, &clause, params)
                        .await?,
                )
            } else {
                None
            };
            return Ok((Vec::new(), count));
        }

        let window_count = page.include_count && !page.uses_event_time();
        let sql = postgres_query_field_page_sql(
            self.schema.prefix(),
            &clause,
            &postgres_query_field_order_sql(page),
            params.len(),
            page.skip > 0,
            window_count,
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let limit = page.top.min(i64::MAX as usize) as i64;
        let skip = page.skip.min(i64::MAX as usize) as i64;
        if !window_count {
            let mut query = sqlx::query_scalar::<_, String>(tagged_sql.as_ref())
                .bind(tenant)
                .bind(entity_type);
            for param in params.iter().cloned() {
                query = query.bind(param);
            }
            query = query.bind(limit);
            if page.skip > 0 {
                query = query.bind(skip);
            }
            let entity_ids = query.fetch_all(self.pool()).await.map_err(storage_error)?;
            let total_count = if page.include_count {
                Some(
                    self.query_field_index_count(tenant, entity_type, &clause, params)
                        .await?,
                )
            } else {
                None
            };
            return Ok((entity_ids, total_count));
        }

        let mut query = sqlx::query_as::<_, (String, i64)>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params.iter().cloned() {
            query = query.bind(param);
        }
        query = query.bind(limit);
        if page.skip > 0 {
            query = query.bind(skip);
        }

        let rows = query.fetch_all(self.pool()).await.map_err(storage_error)?;
        let total_count = if let Some((_, count)) = rows.first() {
            Some((*count).max(0) as usize)
        } else {
            Some(
                self.query_field_index_count(tenant, entity_type, &clause, params)
                    .await?,
            )
        };
        let entity_ids = rows
            .into_iter()
            .map(|(entity_id, _)| entity_id)
            .collect::<Vec<_>>();
        Ok((entity_ids, total_count))
    }

    async fn query_field_index_count(
        &self,
        tenant: &str,
        entity_type: &str,
        clause: &str,
        params: Vec<String>,
    ) -> Result<usize, PersistenceError> {
        let schema = self.schema.prefix();
        let index_cte = field_index_cte(schema);
        let sql = format!(
            "{index_cte}SELECT COUNT(*) \
             FROM {schema}entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND ({clause})"
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query_scalar::<_, i64>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        let count = query.fetch_one(self.pool()).await.map_err(storage_error)?;
        Ok(count.max(0) as usize)
    }
}

/// The page's statement. `params` is how many filter, bound and keyset
/// parameters follow the tenant and type; `LIMIT` comes next, then `OFFSET`
/// only when the page skips rows.
fn postgres_query_field_page_sql(
    schema: &str,
    clause: &str,
    order_sql: &str,
    params: usize,
    skips: bool,
    window_count: bool,
) -> String {
    let select = if window_count {
        "entity_id, COUNT(*) OVER() AS total_count"
    } else {
        "entity_id"
    };
    let index_cte = field_index_cte(schema);
    let limit_param = params + 3;
    let offset = if skips {
        format!(" OFFSET ${}", limit_param + 1)
    } else {
        String::new()
    };
    format!(
        "{index_cte}SELECT {select} \
         FROM {schema}entity_catalog \
         WHERE tenant = $1 AND entity_type = $2 AND ({clause}) \
         ORDER BY {order_sql} \
         LIMIT ${limit_param}{offset}"
    )
}

// Generated filters and their completeness guards refer to these query
// relations. Bind both to the selected physical tables without rewriting the
// fragment or its literals. With no schema selected, retain the original SQL.
pub(crate) fn field_index_cte(schema: &str) -> String {
    if schema.is_empty() {
        return String::new();
    }
    format!(
        "WITH entity_field_index AS (SELECT * FROM {schema}entity_field_index WHERE tenant = $1 AND entity_type = $2), \
         entity_catalog AS (SELECT * FROM {schema}entity_catalog WHERE tenant = $1 AND entity_type = $2) "
    )
}

fn postgres_query_field_order_sql(page: &QueryFieldIndexPageQuery) -> String {
    let mut clauses = Vec::new();
    for (key, descending) in page.total_order() {
        let direction = if descending { "DESC" } else { "ASC" };
        let nulls = if descending {
            "NULLS FIRST"
        } else {
            "NULLS LAST"
        };
        match key {
            CatalogOrderKey::EntityId => clauses.push(format!("entity_id {direction}")),
            CatalogOrderKey::Status => clauses.push(format!("status {direction} {nulls}")),
            // Never null, so the index's own null order is never asked for.
            CatalogOrderKey::EventTime(column) => {
                clauses.push(format!("{} {direction}", event_time_key_sql(column)));
            }
            // As the in-memory sort compares (ADR-0188): a number as an IEEE
            // double, a string byte by byte. A row without a value sorts
            // last ascending and first descending.
            CatalogOrderKey::Field(field_name) => {
                let field = postgres_string_literal(field_name);
                clauses.push(format!(
                    "CASE WHEN jsonb_typeof(fields -> {field}) = 'number' \
                     THEN CAST(fields ->> {field} AS double precision) END {direction} {nulls}"
                ));
                clauses.push(format!(
                    "CASE WHEN jsonb_typeof(fields -> {field}) <> 'number' \
                     THEN (fields ->> {field}) COLLATE \"C\" END {direction} {nulls}"
                ));
            }
        }
    }
    clauses.join(", ")
}

#[cfg(test)]
#[path = "query_page_test.rs"]
mod tests;
