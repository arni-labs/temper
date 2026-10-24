//! What one delete removes, and whether it may go ahead.
//!
//! Deleting an entity follows every relation that points at its type, by that
//! relation's delete policy:
//! - `cascade`: the rows that reference it are removed with it, and the
//!   relations pointing at them are followed the same way;
//! - `restrict`: a row outside that set which references any entity in it
//!   refuses the whole delete;
//! - `none`: nothing happens, and the reference may dangle.
//!
//! The plan is made before anything is removed, so a refusal removes nothing.
//! Its order puts every dependent before the entity it depends on, so a delete
//! that stops part way leaves no row pointing at a removed one, and retrying it
//! plans what is left.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant::DeletePolicy;
use tracing::instrument;

use super::ConstraintViolation;
use super::delete_references::ReferenceFinder;
use crate::registry::RelationEdge;
use crate::state::ServerState;

/// One entity a delete removes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeleteTarget {
    pub entity_type: String,
    pub entity_id: String,
}

/// The entities one delete removes, the requested entity last.
#[derive(Debug)]
pub struct DeletePlan {
    /// Removal order: each dependent before the entity it depends on.
    order: Vec<DeleteTarget>,
}

impl DeletePlan {
    /// The entities removed with the requested one, in removal order.
    pub fn dependents(&self) -> &[DeleteTarget] {
        debug_assert!(!self.order.is_empty());
        &self.order[..self.order.len() - 1]
    }
}

/// Plan the delete of `entity_type('entity_id')` under its relations' delete
/// policies, or say why it may not go ahead.
///
/// A `restrict` reference is refused as `RelationIntegrity` (409). Rows that
/// could not be checked refuse the delete as `RelationUnverified` (503).
#[instrument(skip_all, fields(otel.name = "constraint.plan_delete", tenant = %tenant, entity_type, entity_id, operation, cascaded = tracing::field::Empty))]
pub async fn plan_delete(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
) -> Result<DeletePlan, ConstraintViolation> {
    let root = DeleteTarget {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
    };
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(DeletePlan { order: vec![root] });
    }
    let incoming: BTreeMap<String, Vec<RelationEdge>> = {
        let registry = state.registry.read().unwrap();
        match registry.get_tenant(tenant) {
            Some(tc) => tc.relation_graph.incoming.clone(),
            None => return Ok(DeletePlan { order: vec![root] }),
        }
    };
    let refusal = Refusal {
        state,
        tenant,
        root: &root,
        operation,
    };
    let mut finder = ReferenceFinder::new(state, tenant);

    let none_removed = BTreeSet::new();

    // Everything the delete removes, found by following cascade relations.
    let mut discovered = vec![root.clone()];
    let mut removed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    removed
        .entry(root.entity_type.clone())
        .or_default()
        .insert(root.entity_id.clone());
    let mut next = 0;
    while next < discovered.len() {
        let target = discovered[next].clone();
        next += 1;
        for edge in edges_with(&incoming, &target.entity_type, DeletePolicy::Cascade) {
            let already = removed.get(&edge.from_entity).unwrap_or(&none_removed);
            let ids = finder
                .referencing_ids(edge, &target.entity_id, already, usize::MAX)
                .await
                .map_err(|reason| refusal.unverified(&target, edge, &reason))?;
            for source_id in ids {
                removed
                    .entry(edge.from_entity.clone())
                    .or_default()
                    .insert(source_id.clone());
                discovered.push(DeleteTarget {
                    entity_type: edge.from_entity.clone(),
                    entity_id: source_id,
                });
            }
        }
    }
    tracing::Span::current().record("cascaded", discovered.len() - 1);

    // No row the delete keeps may hold a restrict reference to one it removes.
    for target in &discovered {
        for edge in incoming.get(&target.entity_type).into_iter().flatten() {
            match edge.delete_policy {
                DeletePolicy::Cascade | DeletePolicy::None => {}
                DeletePolicy::SetNull => {
                    return Err(refusal.unverified(
                        target,
                        edge,
                        "set-null deletes are not implemented",
                    ));
                }
                DeletePolicy::Restrict => {
                    let skip = removed.get(&edge.from_entity).unwrap_or(&none_removed);
                    let found = finder
                        .referencing_ids(edge, &target.entity_id, skip, 1)
                        .await
                        .map_err(|reason| refusal.unverified(target, edge, &reason))?;
                    if let Some(source_id) = found.into_iter().next() {
                        return Err(refusal.referenced(target, edge, &source_id));
                    }
                }
            }
        }
    }

    discovered.reverse();
    debug_assert_eq!(discovered.last(), Some(&root));
    Ok(DeletePlan { order: discovered })
}

/// The relations pointing at `entity_type` with `policy`.
fn edges_with<'a>(
    incoming: &'a BTreeMap<String, Vec<RelationEdge>>,
    entity_type: &str,
    policy: DeletePolicy,
) -> impl Iterator<Item = &'a RelationEdge> {
    incoming
        .get(entity_type)
        .into_iter()
        .flatten()
        .filter(move |edge| edge.delete_policy == policy)
}

/// Builds the violation that refuses one planned delete.
struct Refusal<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    root: &'a DeleteTarget,
    operation: &'a str,
}

impl Refusal<'_> {
    /// How the refusal names what it refuses: the requested entity, or the
    /// dependent of it that the delete would also remove.
    fn subject(&self, target: &DeleteTarget) -> String {
        let root = format!("{}('{}')", self.root.entity_type, self.root.entity_id);
        if target == self.root {
            root
        } else {
            format!(
                "{root}: it would delete {}('{}'), which",
                target.entity_type, target.entity_id
            )
        }
    }

    fn referenced(
        &self,
        target: &DeleteTarget,
        edge: &RelationEdge,
        source_id: &str,
    ) -> ConstraintViolation {
        tracing::warn!(
            tenant = %self.tenant, entity_type = %self.root.entity_type,
            entity_id = %self.root.entity_id, operation = self.operation,
            referenced_type = %target.entity_type, referenced_id = %target.entity_id,
            from_entity = %edge.from_entity, source_id,
            "constraint violation: cannot delete entity referenced by another"
        );
        self.state.metrics.record_relation_integrity_violation(
            self.tenant.as_str(),
            &self.root.entity_type,
            self.operation,
        );
        let verb = if target == self.root { ":" } else { " is" };
        ConstraintViolation::relation(
            format!(
                "cannot delete {}{verb} referenced by {}('{}') via {}",
                self.subject(target),
                edge.from_entity,
                source_id,
                edge.source_field
            ),
            &self.root.entity_type,
            &self.root.entity_id,
            self.operation,
        )
    }

    fn unverified(
        &self,
        target: &DeleteTarget,
        edge: &RelationEdge,
        reason: &str,
    ) -> ConstraintViolation {
        tracing::warn!(
            tenant = %self.tenant, entity_type = %self.root.entity_type,
            entity_id = %self.root.entity_id, operation = self.operation,
            from_entity = %edge.from_entity, reason,
            "constraint check unavailable: cannot confirm what references the entity"
        );
        let verb = if target == self.root {
            ": could not"
        } else {
            " could not"
        };
        ConstraintViolation::relation_unverified(
            format!(
                "cannot delete {}{verb} check the {} rows that reference it via {}: {}",
                self.subject(target),
                edge.from_entity,
                edge.source_field,
                reason
            ),
            &self.root.entity_type,
            &self.root.entity_id,
            self.operation,
        )
    }
}
