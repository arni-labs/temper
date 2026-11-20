//! Catalog rows counted by the value of one key: the groups of a facet read
//! (`$apply=groupby((key),aggregate($count as count))`, ADR-0188).
//!
//! One statement groups the rows a page query's filter selects and returns a
//! page of the groups, in order and after a keyset of the last group sent.
//! A group's value is the key as the stores compare it ([`CatalogSql`]), so
//! the rows without a value make one group whose value is NULL, which sorts
//! last.

use super::catalog_field::{CatalogSql, FieldClass, FieldLiteral};
use super::catalog_page::QueryFieldIndexPageQuery;
use crate::persistence::PersistenceError;

/// What the rows are grouped by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FacetKey {
    /// The catalog row's status.
    Status,
    /// An app field, read as `class`; a value of another type is NULL.
    Field {
        /// The field.
        name: String,
        /// The JSON type its values are read as.
        class: FieldClass,
    },
}

impl FacetKey {
    /// The JSON type the key's values are, and a cursor's value must be.
    pub fn class(&self) -> FieldClass {
        match self {
            Self::Status => FieldClass::Text,
            Self::Field { class, .. } => *class,
        }
    }
}

/// The last group a page sent: its count, and its value (`None` for the
/// group of rows without one).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacetAfter {
    /// The group's count.
    pub count: usize,
    /// The group's value.
    pub value: Option<FieldLiteral>,
}

/// One page of the groups of the rows `filter` selects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogFacetQuery {
    /// The rows: its filter, parameters, event-time bounds and field
    /// comparisons. Its order, keyset, skip, top and count are not read.
    pub filter: QueryFieldIndexPageQuery,
    /// What the rows are grouped by.
    pub key: FacetKey,
    /// Groups in count order, largest first, then by value; else by value.
    /// A value sorts as the key compares, the group without one last.
    pub by_count: bool,
    /// Start after this group.
    pub after: Option<FacetAfter>,
    /// Most groups to return.
    pub top: usize,
}

impl CatalogFacetQuery {
    /// The statement and its parameters after the tenant (`?1`) and type
    /// (`?2`); the last parameter is the `LIMIT`, which the store binds as
    /// an integer after them. `from` names the catalog table, with whatever
    /// index hint the store gives it. The statement returns the group's
    /// value as `g` and its count as `n`.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Storage`] when the cursor's value is not
    /// of the key's type.
    pub fn statement(
        &self,
        from: &str,
        sql: &dyn CatalogSql,
    ) -> Result<(String, Vec<String>), PersistenceError> {
        let mut filter = self.filter.clone();
        filter.order_by.clear();
        filter.after = None;
        let mut params = filter.params.clone();
        let (conditions, condition_params) = filter.catalog_conditions(params.len() + 3, sql)?;
        params.extend(condition_params);
        let value = match &self.key {
            FacetKey::Status => sql.status(),
            FacetKey::Field { name, class } => sql.field_value(name, *class),
        };
        let keyset = match &self.after {
            None => "TRUE".to_string(),
            Some(after) => self.keyset(after, sql, &mut params)?,
        };
        let by_count = if self.by_count { "n DESC, " } else { "" };
        let limit = params.len() + 3;
        let statement = format!(
            "SELECT g, n FROM (SELECT {value} AS g, COUNT(*) AS n FROM {from} \
             WHERE tenant = ?1 AND entity_type = ?2 AND ({}) AND {conditions} \
             GROUP BY {value}) facets \
             WHERE {keyset} ORDER BY {by_count}(g IS NULL), g LIMIT ?{limit}",
            filter.where_clause
        );
        Ok((statement, params))
    }

    /// The groups after `after`: a smaller count, or the same count and a
    /// later value, when in count order; a later value otherwise. No value
    /// follows the group without one.
    fn keyset(
        &self,
        after: &FacetAfter,
        sql: &dyn CatalogSql,
        params: &mut Vec<String>,
    ) -> Result<String, PersistenceError> {
        let later_value = match &after.value {
            None => "FALSE".to_string(),
            Some(literal) if literal.class() != self.key.class() => {
                return Err(PersistenceError::Storage(
                    "a facet cursor's value is not of its key's type".to_string(),
                ));
            }
            Some(literal) => {
                params.push(literal.param());
                let placeholder = format!("?{}", params.len() + 2);
                format!(
                    "(g IS NULL OR g > {})",
                    sql.literal(&placeholder, literal.class())
                )
            }
        };
        if !self.by_count {
            return Ok(later_value);
        }
        params.push(after.count.to_string());
        let count = format!("CAST(?{} AS BIGINT)", params.len() + 2);
        Ok(format!("(n < {count} OR (n = {count} AND {later_value}))"))
    }
}

#[cfg(test)]
#[path = "catalog_facet_test.rs"]
mod tests;
