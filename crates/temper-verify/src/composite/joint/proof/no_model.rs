//! Why a proof has no model to walk, and what of it the proof's digest reads.

use std::fmt;

/// Why a proof has no model to walk.
pub(super) enum NoModel {
    /// Its rules read a type the app does not declare: a fact of the specs.
    Undeclared(String),
    /// Building it stopped short, nearly always because it did not fit in
    /// the room: a fact of the host at that moment. Its text names the memory the host had free, which changes
    /// from one try to the next, so it prints as no part of the digest: a
    /// try that finds the room as short again reads the verdict kept for
    /// this one, instead of walking again for ever.
    DidNotFit(String),
}

impl NoModel {
    /// The proof's rules read the `missing` types.
    pub(super) fn undeclared(missing: &[String]) -> Self {
        Self::Undeclared(format!(
            "it reads {}, which this app does not declare",
            missing.join(", ")
        ))
    }

    /// Why, in words.
    pub(super) fn reason(&self) -> &str {
        match self {
            Self::Undeclared(reason) | Self::DidNotFit(reason) => reason,
        }
    }
}

impl fmt::Debug for NoModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared(reason) => f.debug_tuple("Undeclared").field(reason).finish(),
            Self::DidNotFit(_) => f.write_str("DidNotFit"),
        }
    }
}
