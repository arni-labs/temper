//! Keeping a state's sequence numbers as how far behind the newest they are.

use std::collections::BTreeSet;

use super::super::super::{Dispatch, JointState};
use super::{BEHIND, Family, Order};
use crate::model::{ParamValue, TemperModelState};

impl Order {
    /// Whether `target` refuses `dispatch` now and whenever it lands: it
    /// carries a count not greater than a field that never goes down, and
    /// the model knows that (not both [`BEHIND`] or further behind).
    fn doomed(&self, dispatch: &Dispatch, target: &TemperModelState) -> bool {
        if !self.compares.contains(&(dispatch.to, dispatch.action)) {
            return false;
        }
        self.compared
            .get(pair!(&dispatch.to, &dispatch.action))
            .is_some_and(|pairs| {
                pairs.iter().any(|(param, field)| {
                    let rising = self.rising.contains(pair!(&dispatch.to, field));
                    let held = target.counters.get(field).copied();
                    match (held, dispatch.params.get(param)) {
                        (Some(held), Some(ParamValue::Count(sent))) => {
                            rising && *sent <= held && (*sent, held) != (0, 0)
                        }
                        _ => false,
                    }
                })
            })
    }

    /// Keep each family's values in `state` as how far behind its newest
    /// they are: the newest is [`BEHIND`], and 0 is that far behind or more.
    /// A dispatch its record will refuse whenever it lands (it carries a
    /// count not greater than a field that never goes down) is first dropped:
    /// a refused dispatch changes nothing, so it is the same as delivered.
    /// Says whether it rewrote a dispatch still in flight.
    pub(in crate::composite::joint::model) fn normalize(&self, state: &mut JointState) -> bool {
        let mut rewrote = false;
        let records = &state.records;
        state
            .inflight
            .retain(|dispatch| !self.doomed(dispatch, &records[dispatch.to.as_str()]));
        for family in &self.families {
            let newest = values(family, state).last().copied().unwrap_or(0);
            let keep = |value: usize| BEHIND - (newest - value).min(BEHIND);
            for (entity, field) in &family.fields {
                if let Some(record) = state.records.get_mut(entity.as_str())
                    && let Some(&value) = record.counters.get(field)
                    && keep(value) != value
                {
                    std::sync::Arc::make_mut(record)
                        .counters
                        .insert(field.clone(), keep(value));
                }
            }
            for dispatch in &mut state.inflight {
                for (entity, action, param) in &family.params {
                    if &dispatch.to == entity
                        && &dispatch.action == action
                        && let Some(&ParamValue::Count(value)) = dispatch.params.get(param)
                        && keep(value) != value
                    {
                        std::sync::Arc::make_mut(dispatch)
                            .params
                            .insert(param.clone(), ParamValue::Count(keep(value)));
                        rewrote = true;
                    }
                }
            }
        }
        rewrote
    }
}

/// The values `family` holds in `state`, in records and in flight.
pub(in crate::composite::joint::model) fn values(
    family: &Family,
    state: &JointState,
) -> BTreeSet<usize> {
    let mut values = BTreeSet::new();
    for (entity, field) in &family.fields {
        if let Some(value) = state
            .records
            .get(entity.as_str())
            .and_then(|r| r.counters.get(field))
        {
            values.insert(*value);
        }
    }
    for dispatch in &state.inflight {
        for (entity, action, param) in &family.params {
            if &dispatch.to == entity
                && &dispatch.action == action
                && let Some(ParamValue::Count(value)) = dispatch.params.get(param)
            {
                values.insert(*value);
            }
        }
    }
    values
}
