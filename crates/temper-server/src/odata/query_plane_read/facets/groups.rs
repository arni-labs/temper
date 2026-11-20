//! A facet's groups: their values, their order, and the cursor a page
//! resumes after (ADR-0188).

use std::cmp::Ordering;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use temper_runtime::persistence::{FacetAfter, FieldClass, FieldLiteral};

/// The largest integer a double holds exactly; an integral value up to it
/// is written as an integer.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// A group's value as the evaluator compares it: a number as a double, a
/// string by its bytes. A record without a value, or with a JSON null, is in
/// the null group, which sorts last; values of other types sort before it,
/// booleans, then numbers, then strings, then anything else.
#[derive(Clone, Debug)]
pub(super) enum GroupKey {
    Bool(bool),
    Number(f64),
    Text(String),
    Other(String),
    Null,
}

impl GroupKey {
    pub(super) fn of(value: Option<&serde_json::Value>) -> Self {
        match value {
            None | Some(serde_json::Value::Null) => Self::Null,
            Some(serde_json::Value::Bool(value)) => Self::Bool(*value),
            Some(serde_json::Value::Number(number)) => match number.as_f64() {
                // One zero: the pattern matches -0 as well as 0.
                Some(0.0) => Self::Number(0.0),
                Some(value) => Self::Number(value),
                None => Self::Other(number.to_string()),
            },
            Some(serde_json::Value::String(text)) => Self::Text(text.clone()),
            Some(other) => Self::Other(other.to_string()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Bool(_) => 0,
            Self::Number(_) => 1,
            Self::Text(_) => 2,
            Self::Other(_) => 3,
            Self::Null => 4,
        }
    }

    /// The value as the answer writes it.
    pub(super) fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::Number(value) if value.fract() == 0.0 && value.abs() <= MAX_EXACT_INTEGER => {
                serde_json::Value::from(*value as i64)
            }
            Self::Number(value) => serde_json::Number::from_f64(*value)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Self::Text(text) => serde_json::Value::String(text.clone()),
            Self::Other(json) => serde_json::from_str(json).unwrap_or(serde_json::Value::Null),
            Self::Null => serde_json::Value::Null,
        }
    }

    /// The value as a store compares it for a key of `class`, or `Err` when
    /// it is of another type. The null group is `Ok(None)`.
    pub(super) fn literal(&self, class: FieldClass) -> Result<Option<FieldLiteral>, ()> {
        match (self, class) {
            (Self::Null, _) => Ok(None),
            (Self::Number(_), FieldClass::Number) => {
                Ok(Some(FieldLiteral::Number(self.to_json().to_string())))
            }
            (Self::Text(text), FieldClass::Text) => Ok(Some(FieldLiteral::Text(text.clone()))),
            _ => Err(()),
        }
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            (Self::Text(a), Self::Text(b)) | (Self::Other(a), Self::Other(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GroupKey {}

/// One group: its value and how many records hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Group {
    pub(super) key: GroupKey,
    pub(super) count: usize,
}

impl Group {
    /// Where `self` sorts against `other`: by count, largest first, then by
    /// value, when `by_count`; else by value.
    pub(super) fn order(&self, other: &Self, by_count: bool) -> Ordering {
        let by_value = self.key.cmp(&other.key);
        if by_count {
            other.count.cmp(&self.count).then(by_value)
        } else {
            by_value
        }
    }

    /// The cursor a page that ends with this group gives: the facet's
    /// property, the group's count and value.
    pub(super) fn cursor(&self, property: &str) -> String {
        let cursor = serde_json::json!({"p": property, "n": self.count, "v": self.key.to_json()});
        URL_SAFE_NO_PAD.encode(cursor.to_string())
    }

    /// The group a cursor for `property` ends after, or `None` when the
    /// token is not one.
    pub(super) fn from_cursor(token: &str, property: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(token.as_bytes()).ok()?;
        let cursor = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
        if cursor.get("p")?.as_str()? != property {
            return None;
        }
        let count = usize::try_from(cursor.get("n")?.as_u64()?).ok()?;
        Some(Self {
            key: GroupKey::of(cursor.get("v")),
            count,
        })
    }

    /// The group as the stores resume after it, for a key of `class`.
    pub(super) fn facet_after(&self, class: FieldClass) -> Result<FacetAfter, ()> {
        Ok(FacetAfter {
            count: self.count,
            value: self.key.literal(class)?,
        })
    }
}

#[cfg(test)]
#[path = "groups_test.rs"]
mod tests;
