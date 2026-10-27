//! Reaction cascades and resolver-dependent identity for repeat checks.

use stateright::Model;
use temper_spec::automaton::{TargetResolver, TriggerEdge};

use super::{CompositeState, CompositeTemperModel, DroppedReaction};

// Sizes for growing the stack a cascade recurses on. They refuse nothing:
// they say when a level moves to a new segment and how big a new one is. A
// level evaluates the target's model and guards before it recurses, and
// 128 KiB is ample room for that, so a checker thread's own stack is used
// until a cascade is deep and `next_state` allocates nothing for a shallow
// one.

/// Stack that must be left before a cascade level is entered; with less, it
/// runs on a new segment.
const CASCADE_RED_ZONE: usize = 128 * 1024;
/// Size of each new segment.
const CASCADE_SEGMENT_BYTES: usize = 1024 * 1024;

/// A step of a cascade. The joint state holds one entity per type, but an
/// entity the chain found and one it made are distinct, as in the runtime's
/// `InlineChain`. A create admits the first made step; another create of the
/// same type and action repeats it even though production mints a new id.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct CascadeStep {
    pub(super) entity_type: String,
    pub(super) entity: CascadeEntity,
    pub(super) action: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CascadeEntity {
    Found,
    Made,
}

impl CompositeTemperModel {
    /// Apply trigger cascades starting from a post-commit source action.
    ///
    /// `chain` holds the steps the cascade is inside of. A trigger whose
    /// target step is already there is not applied: the runtime stops that
    /// reaction as a loop. Each triggered transition applies the target
    /// entity's matching action by name if enabled in the target's current
    /// state.
    ///
    /// When a reaction's target action is NOT enabled from the target's
    /// current state (from-state mismatch or false guard) it is a *dropped
    /// reaction* (ADR-0150). The first such non-exempt drop is recorded into
    /// `state.dropped` so the `no_dropped_reaction` property can flag it.
    /// `create`-resolver reactions and `drop_ok = true` triggers are exempt
    /// and never recorded.
    pub(super) fn cascade(
        &self,
        state: &mut CompositeState,
        source_entity: &str,
        source_action: &str,
        source_to_state: &str,
        chain: &mut Vec<CascadeStep>,
    ) {
        let edges: Vec<&TriggerEdge> = self
            .edges
            .iter()
            .filter(|e| e.from == source_entity && e.source_action == source_action)
            .collect();
        for edge in edges {
            // The trigger's guard pins the source statuses it fires in.
            if let Some(statuses) = &edge.status_filter
                && !statuses.iter().any(|status| status == source_to_state)
            {
                continue;
            }
            let entity = self.target_identity(edge, chain);
            let step = CascadeStep {
                entity_type: edge.to.clone(),
                entity,
                action: edge.target_action.clone(),
            };
            if chain.contains(&step) {
                continue;
            }
            // Find target entity + model
            let Some(target_model) = self.models.get(&edge.to) else {
                continue;
            };
            let Some(target_state) = state.entities.get(&edge.to) else {
                continue;
            };

            // Is the target action enabled on the target's current state?
            // The per-entity model offers cross-entity-guarded actions
            // abstractly; resolve any such guard concretely against the joint
            // state so a guarded target action only counts as enabled when its
            // own cross-entity precondition holds.
            let mut enabled = Vec::new();
            target_model.actions(target_state, &mut enabled);
            let Some(target_action) = enabled
                .iter()
                .find(|a| {
                    a.name == edge.target_action
                        && self.cross_entity_guards_satisfied(&edge.to, &a.name, state)
                })
                .cloned()
            else {
                // Target action not enabled — the reaction is dropped.
                // `create` resolvers spawn a fresh, always-enabled target, so
                // they can never drop; `drop_ok` marks an intentional drop.
                // Anything else is a no_dropped_reaction violation: record the
                // first one (and keep cascading; later drops don't overwrite
                // the witnessed counterexample).
                if !matches!(edge.resolve_target, TargetResolver::Create)
                    && !edge.drop_ok
                    && state.dropped.is_none()
                {
                    state.dropped = Some(DroppedReaction {
                        source_entity: source_entity.to_string(),
                        source_action: source_action.to_string(),
                        trigger_name: edge.trigger_name.clone(),
                        target_entity: edge.to.clone(),
                        target_action: edge.target_action.clone(),
                        target_state: target_state.status.clone(),
                    });
                }
                continue;
            };

            // Apply target action
            let Some(new_target_state) =
                target_model.next_state(target_state, target_action.clone())
            else {
                continue;
            };

            let new_target_status = new_target_state.status.clone();
            state.entities.insert(edge.to.clone(), new_target_state);

            // Recurse: the target action itself may trigger further cascades.
            // A cascade is as deep as the spec's distinct steps, so each level
            // runs on a stack that grows.
            chain.push(step);
            stacker::maybe_grow(CASCADE_RED_ZONE, CASCADE_SEGMENT_BYTES, || {
                self.cascade(
                    state,
                    &edge.to,
                    &target_action.name,
                    &new_target_status,
                    chain,
                );
            });
            chain.pop();
        }
    }

    /// Resolve the identity production uses for the repeat check. A supplied
    /// id addresses the found slice; only a generated id makes a new one.
    /// `same_id` on the source type keeps the current source's identity.
    fn target_identity(&self, edge: &TriggerEdge, chain: &[CascadeStep]) -> CascadeEntity {
        match &edge.resolve_target {
            TargetResolver::Create => CascadeEntity::Made,
            TargetResolver::CreateIfMissing { id_field } => {
                let supplied = self.models[&edge.from]
                    .initial_strings
                    .get(id_field)
                    .is_some_and(|id| !id.is_empty());
                if supplied {
                    CascadeEntity::Found
                } else {
                    CascadeEntity::Made
                }
            }
            TargetResolver::SameId if edge.from == edge.to => chain
                .last()
                .map_or(CascadeEntity::Found, |step| step.entity),
            TargetResolver::SameId
            | TargetResolver::Field { .. }
            | TargetResolver::Static { .. } => CascadeEntity::Found,
        }
    }
}
