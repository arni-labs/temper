//! Explicit PostgreSQL schema selection, independent of connection session state.
use std::borrow::Cow;
use temper_runtime::persistence::PersistenceError;

/// Optional schema for all tables accessed by a PostgreSQL store.
///
/// The default preserves PostgreSQL's existing `search_path` behavior. An explicit
/// name is quoted as one SQL identifier, never interpreted as SQL or a search path.
#[derive(Clone, Debug, Default)]
pub struct PostgresSchema {
    name: Option<String>,
    prefix: String,
}

impl PostgresSchema {
    /// Select an existing or migration-managed schema. PostgreSQL identifiers
    /// must be nonempty, contain no NUL, and fit its standard 63-byte limit.
    pub fn new(name: impl Into<String>) -> Result<Self, PersistenceError> {
        let name = name.into();
        if name.is_empty() || name.len() > 63 || name.contains('\0') {
            return Err(PersistenceError::Storage(
                "PostgreSQL schema must contain 1 to 63 bytes and no NUL".into(),
            ));
        }
        let prefix = format!("\"{}\".", name.replace('"', "\"\""));
        Ok(Self {
            name: Some(name),
            prefix,
        })
    }

    /// The configured schema, or `None` for the connection's existing search path.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Expand explicit `{schema}` markers in a static SQL template to the quoted
    /// schema prefix (or an empty prefix for the default). Markers belong before
    /// table names. Bind data separately; this does not parse or rewrite SQL.
    pub fn qualify_sql(&self, template: &'static str) -> Cow<'static, str> {
        if template.contains("{schema}") {
            Cow::Owned(template.replace("{schema}", &self.prefix))
        } else {
            Cow::Borrowed(template)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_keeps_unqualified_queries() {
        assert_eq!(
            PostgresSchema::default().qualify_sql("SELECT * FROM {schema}events"),
            "SELECT * FROM events"
        );
    }
    #[test]
    fn quotes_one_identifier_and_preserves_bound_parameters() {
        let schema = PostgresSchema::new("Odd.\"Schema;--").unwrap();
        assert_eq!(
            schema.qualify_sql("SELECT * FROM {schema}events WHERE tenant=$1"),
            "SELECT * FROM \"Odd.\"\"Schema;--\".events WHERE tenant=$1"
        );
    }
    #[test]
    fn rejects_invalid_names_without_truncating() {
        for name in [String::new(), "a".repeat(64), "a\0b".into(), "é".repeat(32)] {
            assert!(PostgresSchema::new(name).is_err());
        }
        assert!(PostgresSchema::new("a".repeat(63)).is_ok());
    }
}
