//! Concrete strings and fresh equality symbols.

use std::fmt;

/// A concrete spec string or a fresh equality symbol. Symbols never equal
/// literals, including literals whose spelling is `#1`.
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ModelString {
    /// A string from a spec or a concrete input.
    Literal(String),
    /// An arbitrary nonempty value absent from the spec's literals.
    Symbol(usize),
}

impl ModelString {
    /// Whether this is the empty concrete string.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Literal(s) if s.is_empty())
    }
    /// Its value for predicate evaluation.
    pub fn val(&self) -> temper_spec::predicate::Val<'_> {
        match self {
            Self::Literal(s) => temper_spec::predicate::Val::Str(s),
            Self::Symbol(n) => temper_spec::predicate::Val::Symbol(*n),
        }
    }
}
impl From<String> for ModelString {
    fn from(s: String) -> Self {
        Self::Literal(s)
    }
}
impl From<&str> for ModelString {
    fn from(s: &str) -> Self {
        Self::Literal(s.into())
    }
}
impl fmt::Display for ModelString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(s) => s.fmt(f),
            Self::Symbol(n) => write!(f, "#{n}"),
        }
    }
}
