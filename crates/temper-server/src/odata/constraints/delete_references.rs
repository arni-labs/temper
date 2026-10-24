//! The lookup behind a `DeletePolicy::Restrict` delete: is there a row of the
//! referencing type whose foreign-key field holds the id being deleted?
//!
//! The query projection's field index answers that the way it answers
//! `$filter=<field> eq '<id>'`: one indexed probe, stopping at the first match,
//! with no referencing entity read from its actor. A type with thousands of
//! referencing rows costs the same as one with none.
//!
//! The index is written after the journal (ADR-0148), so a miss is only proof
//! for rows it has projected. Live rows it has no projection for yet are read
//! from their actors, which covers projection lag. When the index cannot answer
//! at all (no query plane, a field declared `query_indexed = false`, an id too
//! long to index, a failed probe), every live row is in that set. The set is read
//! only while it fits [`UNINDEXED_REFERENCE_READ_BUDGET`]; past it the delete is
//! refused as unverified rather than reading the whole type.

use std::collections::BTreeSet;

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant::DeletePolicy;
use tracing::instrument;

use super::ConstraintViolation;
use crate::registry::RelationEdge;
use crate::state::ServerState;

/// Most referencing rows one delete reads from their actors.
///
/// Reading a row the index has not projected can restore its actor from
/// storage, and production restores about 40 actors a second, so this keeps a
/// delete to a few seconds of reads. Projection lag leaves a handful of rows
/// unprojected; a larger set means the type has no usable projection, and
/// reading all of it is the request that ran for minutes before this lookup.
const UNINDEXED_REFERENCE_READ_BUDGET: usize = 100;

/// A clause every projected row passes: lists the type's projection catalog.
const EVERY_PROJECTED_ROW: &str = "entity_id IS NOT NULL";

/// What one Restrict relation says about deleting an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceLookup {
    /// `source_id` holds the entity's id in the relation's field.
    Referenced { source_id: String },
    /// No live row of the referencing type holds the entity's id.
    Unreferenced,
    /// Some rows could not be checked; `reason` says which and why.
    Unverified { reason: String },
}

/// The field index's answer, before any unprojected rows are read.
enum IndexAnswer {
    /// A projected row holds the id.
    Referenced(String),
    /// No projected row holds the id.
    NoProjectedReference,
    /// The index cannot answer for this relation.
    Unavailable,
}

/// Check incoming relation policy before deleting an entity.
///
/// Each `Restrict` relation is answered by [`find_restricting_reference`], so
/// the check does not read every referencing entity. Returns a
/// `RelationUnverified` violation when the index cannot answer and there are
/// too many unprojected rows to read one by one.
#[instrument(skip_all, fields(otel.name = "constraint.pre_delete_relation_checks", tenant = %tenant, entity_type, entity_id, operation))]
pub async fn pre_delete_relation_checks(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
) -> Result<(), ConstraintViolation> {
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(());
    }

    let (tenant_name, edges): (String, Vec<RelationEdge>) = {
        let registry = state.registry.read().unwrap();
        let Some(tc) = registry.get_tenant(tenant) else {
            return Ok(());
        };
        (
            tenant.to_string(),
            tc.relation_graph
                .incoming
                .get(entity_type)
                .cloned()
                .unwrap_or_default(),
        )
    };

    for edge in edges {
        if edge.delete_policy != DeletePolicy::Restrict {
            continue;
        }
        match find_restricting_reference(state, tenant, &edge, entity_id).await {
            ReferenceLookup::Unreferenced => {}
            ReferenceLookup::Referenced { source_id } => {
                tracing::warn!(
                    tenant = %tenant_name, entity_type, entity_id, operation,
                    from_entity = %edge.from_entity, source_id = %source_id,
                    "constraint violation: cannot delete entity referenced by another"
                );
                state.metrics.record_relation_integrity_violation(
                    &tenant_name,
                    entity_type,
                    operation,
                );
                return Err(ConstraintViolation::relation(
                    format!(
                        "cannot delete {}('{}'): referenced by {}('{}') via {}",
                        entity_type, entity_id, edge.from_entity, source_id, edge.source_field
                    ),
                    entity_type,
                    entity_id,
                    operation,
                ));
            }
            ReferenceLookup::Unverified { reason } => {
                tracing::warn!(
                    tenant = %tenant_name, entity_type, entity_id, operation,
                    from_entity = %edge.from_entity, reason = %reason,
                    "constraint check unavailable: cannot confirm entity is unreferenced"
                );
                return Err(ConstraintViolation::relation_unverified(
                    format!(
                        "cannot delete {}('{}'): could not confirm that no {} references it via {}: {}",
                        entity_type, entity_id, edge.from_entity, edge.source_field, reason
                    ),
                    entity_type,
                    entity_id,
                    operation,
                ));
            }
        }
    }

    Ok(())
}

/// Find a row that keeps `entity_id` from being deleted through `edge`.
#[instrument(skip_all, fields(
    otel.name = "constraint.delete_reference_lookup",
    tenant = %tenant,
    from_entity = %edge.from_entity,
    source_field = %edge.source_field,
    unindexed = tracing::field::Empty,
))]
async fn find_restricting_reference(
    state: &ServerState,
    tenant: &TenantId,
    edge: &RelationEdge,
    entity_id: &str,
) -> ReferenceLookup {
    debug_assert!(!edge.from_entity.is_empty());
    debug_assert!(!edge.source_field.is_empty());

    let unindexed = match probe_field_index(state, tenant, edge, entity_id).await {
        IndexAnswer::Referenced(source_id) => return ReferenceLookup::Referenced { source_id },
        IndexAnswer::NoProjectedReference => {
            unprojected_live_ids(state, tenant, &edge.from_entity).await
        }
        IndexAnswer::Unavailable => state.list_entity_ids_lazy(tenant, &edge.from_entity).await,
    };
    tracing::Span::current().record("unindexed", unindexed.len());
    read_unindexed_rows(state, tenant, edge, entity_id, &unindexed).await
}

/// Ask the field index for the first projected row whose `source_field` is
/// `entity_id`, using the same predicate an OData `$filter` equality pushes down.
async fn probe_field_index(
    state: &ServerState,
    tenant: &TenantId,
    edge: &RelationEdge,
    entity_id: &str,
) -> IndexAnswer {
    let Some(query_plane) = state.query_plane_store() else {
        return IndexAnswer::Unavailable;
    };
    if !state.query_projection_indexes_field(tenant, &edge.from_entity, &edge.source_field) {
        return IndexAnswer::Unavailable;
    }
    let equality = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(edge.source_field.clone())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(
            entity_id.to_string(),
        ))),
    };
    // `None` when the id is longer than the index stores a value.
    let Some(predicate) = crate::odata::filter_sql::try_translate_candidate_filter(&equality)
    else {
        return IndexAnswer::Unavailable;
    };
    match query_plane
        .query_field_index_page(
            tenant.as_str(),
            &edge.from_entity,
            &predicate.where_clause,
            predicate.params,
            &[],
            0,
            1,
            false,
        )
        .await
    {
        Ok(Some(page)) => {
            debug_assert!(page.entity_ids.len() <= 1);
            match page.entity_ids.into_iter().next() {
                Some(source_id) => IndexAnswer::Referenced(source_id),
                None => IndexAnswer::NoProjectedReference,
            }
        }
        Ok(None) => IndexAnswer::Unavailable,
        Err(error) => {
            tracing::warn!(
                tenant = %tenant, from_entity = %edge.from_entity, error = %error,
                "delete reference probe failed; checking unindexed rows instead"
            );
            IndexAnswer::Unavailable
        }
    }
}

/// Live ids of `entity_type` that have no row in the projection catalog.
///
/// A projected row's field index rows are written in the same transaction as
/// its catalog row, so a projected row the probe did not return does not hold
/// the id. Only the ids returned here are unknown to the index.
async fn unprojected_live_ids(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Vec<String> {
    let live_ids = state.list_entity_ids_lazy(tenant, entity_type).await;
    if live_ids.is_empty() {
        return live_ids;
    }
    let Some(query_plane) = state.query_plane_store() else {
        return live_ids;
    };
    // The catalog lists the type's projected rows: about as many ids as the
    // live listing above already holds in memory.
    let catalog = query_plane
        .query_field_index_page(
            tenant.as_str(),
            entity_type,
            EVERY_PROJECTED_ROW,
            Vec::new(),
            &[],
            0,
            usize::MAX,
            false,
        )
        .await;
    let projected: BTreeSet<String> = match catalog {
        Ok(Some(page)) => page.entity_ids.into_iter().collect(),
        Ok(None) => return live_ids,
        Err(error) => {
            tracing::warn!(
                tenant = %tenant, entity_type, error = %error,
                "projection catalog listing failed; treating every live row as unindexed"
            );
            return live_ids;
        }
    };
    live_ids
        .into_iter()
        .filter(|id| !projected.contains(id))
        .collect()
}

/// Read the rows the index could not vouch for, while they fit the budget.
async fn read_unindexed_rows(
    state: &ServerState,
    tenant: &TenantId,
    edge: &RelationEdge,
    entity_id: &str,
    unindexed: &[String],
) -> ReferenceLookup {
    if unindexed.len() > UNINDEXED_REFERENCE_READ_BUDGET {
        return ReferenceLookup::Unverified {
            reason: format!(
                "{} {} rows are not in the query index, more than the {} a delete reads \
                 one by one; retry once the index has caught up",
                unindexed.len(),
                edge.from_entity,
                UNINDEXED_REFERENCE_READ_BUDGET
            ),
        };
    }
    debug_assert!(unindexed.len() <= UNINDEXED_REFERENCE_READ_BUDGET);

    for source_id in unindexed {
        match state
            .get_tenant_entity_state(tenant, &edge.from_entity, source_id)
            .await
        {
            Ok(response) => {
                if holds_id(&response.state.fields, &edge.source_field, entity_id) {
                    return ReferenceLookup::Referenced {
                        source_id: source_id.clone(),
                    };
                }
            }
            Err(error) => {
                return ReferenceLookup::Unverified {
                    reason: format!(
                        "{}('{}') is not in the query index and could not be read: {error}",
                        edge.from_entity, source_id
                    ),
                };
            }
        }
    }
    ReferenceLookup::Unreferenced
}

/// Whether `fields[field]` holds `entity_id`, compared as the field index
/// stores a scalar: its text.
fn holds_id(fields: &serde_json::Value, field: &str, entity_id: &str) -> bool {
    match fields.get(field) {
        Some(serde_json::Value::String(value)) => value == entity_id,
        Some(serde_json::Value::Number(value)) => value.to_string() == entity_id,
        Some(serde_json::Value::Bool(value)) => value.to_string() == entity_id,
        _ => false,
    }
}
