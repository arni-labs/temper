//! A record taking an action: a dispatch reaching its actor, which takes
//! or refuses it, the state timer that arms, and what the record does next.

use std::collections::BTreeMap;

use temper_spec::predicate::{Truth, eval};

use super::super::env::Blind;
use super::super::{Dispatch, JointState};
use super::JointModel;
use crate::model::semantics::{step, successors_among};
use crate::model::{
    ParamValue, ResolvedTransition, StringParam, TemperModelAction, TemperModelState,
};

impl JointModel {
    /// The kernel sends `name` to `entity`'s record with no parameters, as
    /// `by` (a timer, or a module's `on_failure`): it goes through the
    /// dispatch path like any dispatch.
    pub(super) fn fire(
        &self,
        state: JointState,
        entity: &str,
        name: &str,
        text: &str,
        by: u32,
    ) -> Vec<(JointState, String, bool)> {
        let dispatch = Dispatch {
            to: entity.to_string(),
            action: name.to_string(),
            target_state: None,
            params: BTreeMap::new(),
            read: !self.action_reads_related(entity, name),
            depth: 0,
            by,
            inline: false,
        };
        if !dispatch.read {
            let mut next = state;
            add(&mut next, dispatch);
            return vec![(next, format!("{text} sent"), true)];
        }
        self.deliver(state, &dispatch, text)
    }

    /// `dispatch` reaches its record's actor in `state`, which takes or
    /// refuses it; what the record does next follows.
    pub(super) fn deliver(
        &self,
        mut state: JointState,
        dispatch: &Dispatch,
        text: &str,
    ) -> Vec<(JointState, String, bool)> {
        match self.arrive(&state, dispatch) {
            Some((moved, name, certain)) => {
                let text = format!("{text} arrives -> {}", moved.status);
                self.place(&mut state, &dispatch.to, moved, &name, dispatch.by);
                self.after_commit(
                    state,
                    &dispatch.to,
                    &name,
                    dispatch.depth,
                    dispatch.by,
                    dispatch.inline,
                )
                .into_iter()
                .map(|(outcome, sure)| (outcome, text.clone(), certain && sure))
                .collect()
            }
            None => vec![(state, format!("{text} refused by its record"), true)],
        }
    }

    /// `entity`'s record takes `action` from `by` now; what it does next
    /// follows. No state when its effects cannot apply.
    pub(super) fn commit(
        &self,
        state: JointState,
        entity: &str,
        action: &TemperModelAction,
        depth: u32,
        by: u32,
        inline: bool,
    ) -> Vec<(JointState, bool)> {
        let Some(moved) = step(
            &self.models[entity],
            &action.name,
            action.target_state.as_deref(),
            &state.records[entity],
            &action.params,
        ) else {
            return Vec::new();
        };
        let mut next = state;
        self.place(&mut next, entity, moved, &action.name, by);
        self.after_commit(next, entity, &action.name, depth, by, inline)
    }

    /// Put `entity`'s record, after it took `action` from `by`, into
    /// `state`. Its state timer is armed by `by` when the record entered a
    /// state with one, or `action` resets it (`arm_state_timeouts_if_needed`).
    pub(super) fn place(
        &self,
        state: &mut JointState,
        entity: &str,
        moved: TemperModelState,
        action: &str,
        by: u32,
    ) {
        let entered = state.records[entity].status != moved.status;
        let timeout = self
            .timeout(entity, &moved.status)
            .filter(|_| self.sender_matters.contains(entity));
        match timeout {
            Some(timeout) if entered || timeout.reset_on.iter().any(|a| a == action) => {
                state.armed.insert(entity.to_string(), by);
            }
            Some(_) => {}
            None => {
                state.armed.remove(entity);
            }
        }
        state.records.insert(entity.to_string(), moved);
    }

    /// The target's record after its actor takes `dispatch`, the action it
    /// took, and whether the model knows the guard allowed it; `None` when it
    /// refuses. The actor checks its own state; the related records were read
    /// on the dispatch path.
    pub(super) fn arrive(
        &self,
        state: &JointState,
        dispatch: &Dispatch,
    ) -> Option<(TemperModelState, String, bool)> {
        let model = &self.models[&dispatch.to];
        let target = &state.records[&dispatch.to];
        let t = self
            .transitions_for(dispatch)
            .find(|t| self.enabled(&dispatch.to, t, state, Blind::Related))?;
        let certain = eval(&t.guard, &self.env(&dispatch.to, state)) == Truth::True;
        // Counts and booleans an effect reads and the dispatch does not
        // carry: the first choice, as the composite check takes. A string
        // parameter it does not carry leaves the field as it is.
        let values = self.values(state);
        let mut params: BTreeMap<String, ParamValue> = successors_among(model, t, target, &values)
            .into_iter()
            .next()
            .map(|(params, _)| params)
            .unwrap_or_default();
        for param in &t.string_params {
            params.remove(&param.name);
        }
        params.extend(dispatch.params.clone());
        if !constraints_hold(&t.string_params, target, &params) {
            return None;
        }
        let moved = step(model, &t.name, t.to_state.as_deref(), target, &params)?;
        Some((moved, t.name.clone(), certain))
    }

    /// The transitions `dispatch` may take on its record.
    pub(super) fn transitions_for<'a>(
        &'a self,
        dispatch: &'a Dispatch,
    ) -> impl Iterator<Item = &'a ResolvedTransition> + 'a {
        self.models[&dispatch.to]
            .transitions
            .iter()
            .filter(move |t| {
                t.name == dispatch.action
                    && dispatch
                        .target_state
                        .as_ref()
                        .is_none_or(|target| t.to_state.as_ref() == Some(target))
            })
    }
}

/// Put `dispatch` in flight, kept in order.
pub(super) fn add(state: &mut JointState, dispatch: Dispatch) {
    let at = state.inflight.partition_point(|d| d < &dispatch);
    state.inflight.insert(at, dispatch);
}

/// Whether the string parameters `params` passes satisfy the action's
/// constraints on its own record, which its actor checks as it writes.
fn constraints_hold(
    string_params: &[StringParam],
    target: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    string_params.iter().all(|param| {
        // A string parameter nobody passed is absent, which only a
        // non-empty constraint refuses.
        let Some(ParamValue::Str(value)) = params.get(&param.name) else {
            return !param.nonempty;
        };
        let tracked = |var: &String| target.strings.get(var);
        !(param.nonempty && value.is_empty())
            && param
                .equals
                .as_ref()
                .and_then(tracked)
                .is_none_or(|v| v == value)
            && param
                .not_equals
                .iter()
                .filter_map(tracked)
                .all(|v| v != value)
    })
}
