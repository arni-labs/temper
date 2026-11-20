//! `$apply`, as far as Temper reads it: one `groupby` of one property with a
//! count of each group (ADR-0188).
//!
//! `groupby((kind),aggregate($count as count))` groups a set's records by
//! `kind` and counts each group. Every other transformation, a second
//! property, or another aggregate is refused, so a caller never gets an
//! answer to a question it did not ask.

use crate::error::ODataError;

/// `groupby((property),aggregate($count as alias))`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCount {
    /// The property the records are grouped by.
    pub property: String,
    /// The name each group's count is given in the answer.
    pub alias: String,
}

/// Parse a `$apply` value.
///
/// # Errors
///
/// Returns [`ODataError::InvalidQueryOption`] for anything other than one
/// `groupby` of one property with `aggregate($count as <alias>)`.
pub fn parse_apply(value: &str) -> Result<GroupCount, ODataError> {
    group_count(value).ok_or_else(|| ODataError::InvalidQueryOption {
        option: "$apply".to_string(),
        message: "only groupby((<property>),aggregate($count as <name>)) is read".to_string(),
    })
}

fn group_count(value: &str) -> Option<GroupCount> {
    let inner = value
        .trim()
        .strip_prefix("groupby")?
        .trim_start()
        .strip_prefix('(')?
        .trim_end()
        .strip_suffix(')')?
        .trim();
    let (property, rest) = inner.strip_prefix('(')?.split_once(')')?;
    let property = property.trim();
    let aggregate = rest
        .trim_start()
        .strip_prefix(',')?
        .trim_start()
        .strip_prefix("aggregate")?
        .trim_start()
        .strip_prefix('(')?
        .trim_end()
        .strip_suffix(')')?;
    let words: Vec<&str> = aggregate.split_whitespace().collect();
    match words.as_slice() {
        ["$count", "as", alias] if is_identifier(property) && is_identifier(alias) => {
            Some(GroupCount {
                property: property.to_string(),
                alias: (*alias).to_string(),
            })
        }
        _ => None,
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
#[path = "apply_test.rs"]
mod tests;
