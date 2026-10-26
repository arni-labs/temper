//! `[[carry]]`: where a new version of a spec moves the stored records the
//! version it replaces would leave with no way forward.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Automaton;
use crate::predicate::Expr;

/// The prefix of the name of every move that applies a `[[carry]]`.
pub const CARRY_PREFIX: &str = "carry:";

/// Where this version moves the stored records in `from` that it would
/// otherwise leave with no way forward: `from` is a state it drops, or one
/// where none of its moves is possible for them. Installing the version
/// applies the carry to each such record as one move made by the platform
/// and journaled like any other. Records with a way forward are not moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Carry {
    /// The state the records are in. This version may no longer have it.
    pub from: String,
    /// Which of those records; all of them when absent.
    #[serde(default = "Expr::always", skip_serializing_if = "Expr::is_always")]
    pub guard: Expr,
    /// The state they go to; `from` when absent.
    pub to: Option<String>,
    /// Values the move writes: a counter, a flag, or a string or number
    /// field, each a state variable this version declares.
    #[serde(default)]
    pub set: BTreeMap<String, serde_json::Value>,
}

impl Carry {
    /// The state a carried record ends in.
    pub fn target(&self) -> &str {
        self.to.as_deref().unwrap_or(&self.from)
    }
}

/// Whether `action` names the move of a `[[carry]]`, which only the platform
/// makes.
pub fn is_carry_move(action: &str) -> bool {
    action.starts_with(CARRY_PREFIX)
}

impl Automaton {
    /// Each `[[carry]]` with the name of the move that applies it,
    /// `carry:<from>:<n>` for the n-th carry from that state, in spec order.
    pub fn carry_moves(&self) -> Vec<(String, &Carry)> {
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        self.carries
            .iter()
            .map(|carry| {
                let n = seen.entry(carry.from.as_str()).or_default();
                *n += 1;
                (format!("{CARRY_PREFIX}{}:{n}", carry.from), carry)
            })
            .collect()
    }
}
