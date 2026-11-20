//! One page of a catalog read, as every query-plane store runs it (ADR-0187).
//!
//! A page is a translated `$filter` (`where_clause` and its `params`), bounds
//! on the kernel's event times, an order, and where it starts: `skip` rows
//! in, or after a keyset cursor (`after`). Each store renders an event time's
//! sort key in its own SQL; the bounds and the keyset around it come from
//! [`QueryFieldIndexPageQuery::catalog_conditions`], so every store compares
//! the same way.
//!
//! An event time is RFC 3339 UTC text with 0, 3, 6 or 9 fraction digits, so
//! its text does not sort by time (`…:00Z` sorts after `…:00.5Z`). Its sort
//! key is the fixed-width `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`, and a record
//! without one sorts as [`MISSING_EVENT_TIME_KEY`], after every time: last
//! ascending, first descending, where the in-memory sort puts it.

use super::PersistenceError;
use super::catalog_field::{
    CatalogSql, FieldClass, FieldComparison, FieldLiteral, field_comparison_sql,
};

/// The length of an event-time sort key, `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`.
pub const EVENT_TIME_KEY_WIDTH: usize = 29;

/// The sort key of a record with no event time. Every character is the
/// highest a real key can hold at that place or above it, so it sorts after
/// every real key in any collation, and a keyset or an order never meets a
/// SQL NULL.
pub const MISSING_EVENT_TIME_KEY: &str = "9999-99-99T99:99:99.999999999";

/// The sort key of an event time, or `None` when `text` is not an RFC 3339
/// time a key can hold.
///
/// The stores compute the same key in SQL from the text the kernel writes
/// (UTC, ending in `Z`): the first 19 characters, a point, and the fraction
/// right-padded with zeros to nine digits.
pub fn event_time_key(text: &str) -> Option<String> {
    let time = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    let key = time
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%S%.9f")
        .to_string();
    (key.len() == EVENT_TIME_KEY_WIDTH).then_some(key)
}

/// An event time the kernel keeps on every record, which a page can order
/// and bound by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTimeColumn {
    /// `first_event_at`: when the record was created.
    First,
    /// `last_event_at`: when the record last moved.
    Last,
}

impl EventTimeColumn {
    /// The column a read property names, if it is an event time.
    pub fn of(property: &str) -> Option<Self> {
        match property {
            "first_event_at" => Some(Self::First),
            "last_event_at" => Some(Self::Last),
            _ => None,
        }
    }

    /// The property this column is read as.
    pub fn property(self) -> &'static str {
        match self {
            Self::First => "first_event_at",
            Self::Last => "last_event_at",
        }
    }
}

/// What one order clause sorts a catalog row by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogOrderKey<'a> {
    /// The record id.
    EntityId,
    /// The catalog status.
    Status,
    /// An event time's sort key.
    EventTime(EventTimeColumn),
    /// An app field.
    Field(&'a str),
}

impl<'a> CatalogOrderKey<'a> {
    /// The key an order clause's field name sorts by.
    pub fn of(field_name: &'a str) -> Self {
        match field_name {
            "entity_id" | "Id" | "id" => Self::EntityId,
            "status" | "Status" => Self::Status,
            _ => EventTimeColumn::of(field_name).map_or(Self::Field(field_name), Self::EventTime),
        }
    }
}

/// One key of a keyset cursor: the boundary row's value for one clause of
/// the page's order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeysetValue {
    /// An id, a status, or an event time's key ([`event_time_key`]).
    Key(String),
    /// An app field's value, or `None` when the row holds none. A page
    /// resumed by one only holds rows whose value for that field is absent
    /// or of the cursor value's JSON type; the reader checks that first
    /// (ADR-0188).
    Field(Option<FieldLiteral>),
}

impl From<&str> for KeysetValue {
    fn from(key: &str) -> Self {
        Self::Key(key.to_string())
    }
}

impl From<String> for KeysetValue {
    fn from(key: String) -> Self {
        Self::Key(key)
    }
}

/// Sort direction for a storage-pushed query-field page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryFieldIndexOrderDirection {
    /// Ascending order.
    Asc,
    /// Descending order.
    Desc,
}

/// One storage-pushed query-field page order clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryFieldIndexOrder {
    /// Projection field, `status`, `entity_id`, or an event time to order by.
    pub field_name: String,
    /// Sort direction.
    pub direction: QueryFieldIndexOrderDirection,
}

impl QueryFieldIndexOrder {
    /// Whether this clause sorts descending.
    pub fn descending(&self) -> bool {
        self.direction == QueryFieldIndexOrderDirection::Desc
    }
}

/// A comparison a bound makes against an event time's key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyComparison {
    /// Equal to.
    Eq,
    /// Before.
    Lt,
    /// At or before.
    Le,
    /// After.
    Gt,
    /// At or after.
    Ge,
}

impl KeyComparison {
    fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// `column comparison key`: a record whose event time compares so with a
/// time. A record with no event time meets no bound, as an OData comparison
/// with null is false.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventTimeBound {
    /// The event time compared.
    pub column: EventTimeColumn,
    /// How it compares.
    pub comparison: KeyComparison,
    /// The time's sort key ([`event_time_key`]).
    pub key: String,
}

/// One bounded page of catalog entity ids.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryFieldIndexPageQuery {
    /// Translated filter, `?1` = tenant and `?2` = entity type, then `params`.
    pub where_clause: String,
    /// The filter's parameters, `?3` on.
    pub params: Vec<String>,
    /// Event-time bounds every row also meets.
    pub event_time_bounds: Vec<EventTimeBound>,
    /// App-field comparisons every row also meets, on its typed value
    /// (ADR-0188).
    pub field_comparisons: Vec<FieldComparison>,
    /// The order; `entity_id` ascending follows unless it ends with `entity_id`.
    pub order_by: Vec<QueryFieldIndexOrder>,
    /// Rows that also hold, for one of these fields, a value that is not of
    /// its class (a JSON null included): what a reader asks for to learn
    /// whether an order by those fields compares one JSON type only.
    pub mistyped: Vec<(String, FieldClass)>,
    /// Start after the row with these keys, one for each clause of
    /// [`Self::total_order`]: a [`KeysetValue::Field`] for an app field and
    /// a [`KeysetValue::Key`] for every other clause.
    pub after: Option<Vec<KeysetValue>>,
    /// Rows to pass over, after `after` when both are set.
    pub skip: usize,
    /// Most ids to return. With `top == 0` and `include_count`, only the count.
    pub top: usize,
    /// Also count every row the filter, bounds and keyset select.
    pub include_count: bool,
}

impl QueryFieldIndexPageQuery {
    /// The page's order: `order_by`, then `entity_id` ascending unless the
    /// last clause already orders by `entity_id`, so it is a total order.
    pub fn total_order(&self) -> Vec<(CatalogOrderKey<'_>, bool)> {
        let mut order: Vec<(CatalogOrderKey<'_>, bool)> = self
            .order_by
            .iter()
            .map(|clause| (CatalogOrderKey::of(&clause.field_name), clause.descending()))
            .collect();
        if self
            .order_by
            .last()
            .is_none_or(|clause| clause.field_name != "entity_id")
        {
            order.push((CatalogOrderKey::EntityId, false));
        }
        order
    }

    /// The event time the page's order leads with, if any: the order a
    /// store's event-time index gives.
    pub fn leading_event_time(&self) -> Option<(EventTimeColumn, bool)> {
        match self.total_order().first() {
            Some((CatalogOrderKey::EventTime(column), descending)) => Some((*column, *descending)),
            _ => None,
        }
    }

    /// Whether the page orders or bounds by an event time.
    pub fn uses_event_time(&self) -> bool {
        !self.event_time_bounds.is_empty()
            || self
                .total_order()
                .iter()
                .any(|(key, _)| matches!(key, CatalogOrderKey::EventTime(_)))
    }

    /// The bounds, the field comparisons and the keyset as one SQL
    /// condition, with parameters numbered `?first_param` on, or `TRUE` when
    /// the page has none. `sql` writes the store's expressions.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Storage`] when `after` does not hold one
    /// key of the right kind for each clause of the order.
    pub fn catalog_conditions(
        &self,
        first_param: usize,
        sql: &dyn CatalogSql,
    ) -> Result<(String, Vec<String>), PersistenceError> {
        let event_time_sql = |column| sql.event_time_key(column);
        let mut conditions = Vec::new();
        let mut params = Vec::new();
        let next = |value: String, params: &mut Vec<String>| {
            params.push(value);
            format!("?{}", first_param + params.len() - 1)
        };
        for bound in &self.event_time_bounds {
            let key = event_time_sql(bound.column);
            let placeholder = next(bound.key.clone(), &mut params);
            conditions.push(format!("{key} {} {placeholder}", bound.comparison.sql()));
            if matches!(bound.comparison, KeyComparison::Gt | KeyComparison::Ge) {
                // A record with no event time sorts after every time; it is
                // not after one.
                conditions.push(format!("{key} < '{MISSING_EVENT_TIME_KEY}'"));
            }
        }
        for comparison in &self.field_comparisons {
            let placeholder = next(comparison.literal.param(), &mut params);
            conditions.push(field_comparison_sql(sql, comparison, &placeholder));
        }
        if !self.mistyped.is_empty() {
            let mistyped: Vec<String> = self
                .mistyped
                .iter()
                .map(|(field, class)| {
                    format!(
                        "({} AND NOT {})",
                        sql.field_has_key(field),
                        sql.field_is(field, *class)
                    )
                })
                .collect();
            conditions.push(format!("({})", mistyped.join(" OR ")));
        }
        if let Some(after) = &self.after {
            let order = self.total_order();
            if after.len() != order.len() {
                return Err(PersistenceError::Storage(format!(
                    "keyset has {} keys for an order of {}",
                    after.len(),
                    order.len()
                )));
            }
            let mut clauses = Vec::with_capacity(order.len());
            for ((key, descending), value) in order.iter().zip(after) {
                let clause = match (key, value) {
                    (CatalogOrderKey::Field(name), KeysetValue::Field(literal)) => {
                        KeysetClause::Field {
                            present: sql.field_present(name),
                            value: literal.as_ref().map(|literal| {
                                let placeholder = next(literal.param(), &mut params);
                                (
                                    sql.field_value(name, literal.class()),
                                    sql.literal(&placeholder, literal.class()),
                                )
                            }),
                        }
                    }
                    (CatalogOrderKey::Field(name), KeysetValue::Key(_)) => {
                        return Err(PersistenceError::Storage(format!(
                            "a keyset resumes field {name} by a field value"
                        )));
                    }
                    (_, KeysetValue::Field(_)) => {
                        return Err(PersistenceError::Storage(
                            "a keyset resumes a catalog key by its text".to_string(),
                        ));
                    }
                    (key, KeysetValue::Key(text)) => KeysetClause::Total {
                        sql: match key {
                            CatalogOrderKey::Status => "status".to_string(),
                            CatalogOrderKey::EventTime(column) => event_time_sql(*column),
                            _ => "entity_id".to_string(),
                        },
                        value: next(text.clone(), &mut params),
                    },
                };
                clauses.push((clause, *descending));
            }
            conditions.push(keyset_sql(&clauses));
        }
        if conditions.is_empty() {
            return Ok(("TRUE".to_string(), params));
        }
        Ok((conditions.join(" AND "), params))
    }
}

/// One clause of a keyset as SQL.
enum KeysetClause {
    /// A key every row holds: `sql` against the cursor's `value`.
    Total { sql: String, value: String },
    /// An app field: `present` is true when the row holds a value, and
    /// `value` is the row's value and the cursor's, when the cursor holds
    /// one. A row without a value sorts last ascending and first descending,
    /// where the in-memory sort puts it.
    Field {
        present: String,
        value: Option<(String, String)>,
    },
}

/// Rows strictly after the cursor under `clauses`. When the first key is
/// one every row holds, the rows at or past its value come first, which an
/// index on the order seeks to.
fn keyset_sql(clauses: &[(KeysetClause, bool)]) -> String {
    debug_assert!(!clauses.is_empty());
    match &clauses[0] {
        (KeysetClause::Total { sql, value }, descending) => {
            let at_or_past = if *descending { "<=" } else { ">=" };
            format!("{sql} {at_or_past} {value} AND {}", strictly_after(clauses))
        }
        (KeysetClause::Field { .. }, _) => strictly_after(clauses),
    }
}

fn strictly_after(clauses: &[(KeysetClause, bool)]) -> String {
    let (clause, descending) = &clauses[0];
    let past = if *descending { "<" } else { ">" };
    let rest = match &clauses[1..] {
        [] => None,
        rest => Some(strictly_after(rest)),
    };
    match clause {
        KeysetClause::Total { sql, value } => match rest {
            None => format!("{sql} {past} {value}"),
            Some(rest) => format!("({sql} {past} {value} OR ({sql} = {value} AND {rest}))"),
        },
        KeysetClause::Field { present, value } => {
            // The order always ends with the id, so a field is never last.
            let rest = rest.unwrap_or_else(|| "FALSE".to_string());
            match (value, descending) {
                (Some((row, cursor)), false) => format!(
                    "(({present} AND ({row} > {cursor} OR ({row} = {cursor} AND {rest}))) \
                     OR NOT {present})"
                ),
                (None, false) => format!("(NOT {present} AND {rest})"),
                (Some((row, cursor)), true) => {
                    format!("({present} AND ({row} < {cursor} OR ({row} = {cursor} AND {rest})))")
                }
                (None, true) => format!("({present} OR (NOT {present} AND {rest}))"),
            }
        }
    }
}

#[cfg(test)]
#[path = "catalog_page_test.rs"]
mod tests;
