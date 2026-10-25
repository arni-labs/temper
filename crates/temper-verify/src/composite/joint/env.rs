//! One entity of a joint state as a predicate environment.

use std::collections::BTreeMap;

use temper_spec::predicate::{Env, Val, VarKind};

use super::JointState;
use crate::model::{TemperModel, TemperModelState};

/// One entity of a joint state as a predicate environment: its own
/// variables as the per-entity model reads them, and the related entities'
/// statuses and tracked variables from the joint state.
pub(super) struct JointEnv<'a> {
    pub(super) models: &'a BTreeMap<String, TemperModel>,
    pub(super) state: &'a JointState,
    pub(super) owner: &'a str,
}

fn var_of<'a>(model: &TemperModel, state: &'a TemperModelState, name: &str) -> Val<'a> {
    match model.var_kinds.get(name) {
        Some(VarKind::Counter) => Val::Num(state.counters.get(name).copied().unwrap_or(0) as f64),
        Some(VarKind::Bool) => Val::Bool(state.booleans.get(name).copied().unwrap_or(false)),
        Some(VarKind::List) => state
            .lists
            .get(name)
            .map_or(Val::Strs(&[]), |items| Val::Strs(items)),
        Some(VarKind::Str) => state
            .strings
            .get(name)
            .map_or(Val::Unknown, |value| Val::Str(value)),
        Some(VarKind::Num) | None => Val::Unknown,
    }
}

impl Env for JointEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(&self.state[self.owner].status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        var_of(&self.models[self.owner], &self.state[self.owner], name)
    }

    fn cross_statuses(&self, entity_type: &str, _id_field: &str) -> Option<Vec<Val<'_>>> {
        self.state
            .get(entity_type)
            .map(|s| vec![Val::Str(&s.status)])
    }

    fn cross_fields(
        &self,
        entity_type: &str,
        _id_field: &str,
        field: &str,
    ) -> Option<Vec<Val<'_>>> {
        let state = self.state.get(entity_type)?;
        let model = self.models.get(entity_type)?;
        Some(vec![var_of(model, state, field)])
    }
}
