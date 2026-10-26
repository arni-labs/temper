//! A joint state: the records in scope, and the dispatches and module calls
//! on their way.

use std::collections::BTreeMap;
use std::fmt;

use super::senders;
use crate::model::semantics::canonical_strings;
use crate::model::{ParamValue, TemperModelAction, TemperModelState};

/// The records in scope, one per type, and the dispatches and module calls
/// on their way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JointState {
    /// Each type's record.
    pub records: BTreeMap<String, TemperModelState>,
    /// Dispatches sent and not yet taken, sorted; the same dispatch may be
    /// in flight twice.
    pub inflight: Vec<Dispatch>,
    /// Module calls started and not yet answered, sorted. Tracked only when
    /// the model makes the kernel's moves itself.
    pub calls: Vec<Call>,
    /// Who armed each record's state timer, by sender number, for the
    /// records in a state that has one: its timer fires as them.
    pub armed: BTreeMap<String, u32>,
}

impl JointState {
    /// Nothing is on its way: no dispatch and no module call.
    pub fn settled(&self) -> bool {
        self.inflight.is_empty() && self.calls.is_empty()
    }
}

/// A module call a record's trigger started, which the module answers later.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Call {
    /// The entity type whose record's trigger started it; the answer goes
    /// back to that record.
    pub to: String,
    /// The module.
    pub module: String,
    /// The trigger's declared answer, if any.
    pub on_success: Option<String>,
    /// The action sent when the call fails, if any.
    pub on_failure: Option<String>,
    /// Who its answer runs as, by sender number: the runtime when the call
    /// runs in the background, the caller when it runs inline.
    pub by: u32,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} call", self.to, self.module)?;
        if self.by != senders::RUNTIME && self.by != senders::PLATFORM_DISPATCH {
            f.write_str(" (inline)")?;
        }
        Ok(())
    }
}

/// A dispatch on its way to a record's actor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dispatch {
    /// The target entity type.
    pub to: String,
    /// The action.
    pub action: String,
    /// The transition's target status, when the sender chose one.
    pub target_state: Option<String>,
    /// The parameters sent; a reaction's are its `args` as the source
    /// committed them.
    pub params: BTreeMap<String, ParamValue>,
    /// Whether the dispatch path has read the related records it needs.
    pub read: bool,
    /// The reaction depth that sent it (0 for a request from outside).
    pub depth: u32,
    /// The reaction that sent it, by its place among the model's reactions;
    /// `None` for a request from outside.
    pub via: Option<usize>,
    /// Who sends it, by sender number; Cedar decides it as them.
    pub by: u32,
    /// Its sender waits for the module calls it starts, so they run inline
    /// and answer as the sender: an outside request may ask for that.
    pub inline: bool,
}

impl fmt::Display for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let action = TemperModelAction {
            name: self.action.clone(),
            target_state: None,
            params: self.params.clone(),
        };
        write!(f, "{}.{action}", self.to)?;
        if !self.read {
            f.write_str(" (unread)")?;
        }
        Ok(())
    }
}

/// A joint state in words: each record, then what is on its way.
pub(super) fn render(state: &JointState) -> String {
    let mut text = state
        .records
        .iter()
        .map(|(entity, s)| format!("{entity}={s}"))
        .collect::<Vec<_>>()
        .join(" | ");
    if !state.inflight.is_empty() {
        let inflight: Vec<String> = state.inflight.iter().map(ToString::to_string).collect();
        text.push_str(&format!(" | in flight: {}", inflight.join(", ")));
    }
    if !state.calls.is_empty() {
        let calls: Vec<String> = state.calls.iter().map(ToString::to_string).collect();
        text.push_str(&format!(" | calls: {}", calls.join(", ")));
    }
    text
}

/// Renumber new strings over the whole joint state, records first, then
/// the dispatches in flight; returns each value's new name.
pub(super) fn canonical(state: &mut JointState) -> BTreeMap<String, String> {
    let mut dispatches = std::mem::take(&mut state.inflight);
    let mut strings: Vec<BTreeMap<String, String>> = dispatches
        .iter()
        .map(|d| {
            d.params
                .iter()
                .filter_map(|(name, value)| match value {
                    ParamValue::Str(text) => Some((name.clone(), text.clone())),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let renamed = {
        let mut maps: Vec<&mut BTreeMap<String, String>> = state
            .records
            .values_mut()
            .map(|s| &mut s.strings)
            .chain(strings.iter_mut())
            .collect();
        canonical_strings(&mut maps)
    };
    for (dispatch, renamed_strings) in dispatches.iter_mut().zip(strings) {
        for (name, text) in renamed_strings {
            dispatch.params.insert(name, ParamValue::Str(text));
        }
    }
    dispatches.sort();
    state.inflight = dispatches;
    state.calls.sort();
    renamed
}
