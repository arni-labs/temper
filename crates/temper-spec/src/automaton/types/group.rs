//! State groups: how a statechart of the spec boxes its states.

use serde::{Deserialize, Serialize};

/// A `[[group]]`: a named box of states for drawing the spec as a statechart.
///
/// Groups only label; nothing at runtime or in verification reads them, and
/// a group name is not a state (`from` and `to` cannot name one). `states`
/// lists declared states and other groups' names, so groups nest. Every state
/// and group sits in at most one group, and groups never contain themselves,
/// so the boxes form a tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StateGroup {
    /// The group's name, unique among groups and distinct from every state.
    pub name: String,
    /// What the box says in plain words: one non-empty line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub says: Option<String>,
    /// The declared states and groups this group holds, in drawing order.
    #[serde(default)]
    pub states: Vec<String>,
}
