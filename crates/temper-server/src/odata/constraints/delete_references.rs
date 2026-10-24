//! The lookup behind a `DeletePolicy::Restrict` delete: is there a row of the
//! referencing type whose foreign-key field holds the id being deleted?
//!
//! The query projection's field index answers that the way it answers
//! `$filter=<field> eq '<id>'`: one indexed probe, with no referencing entity
//! read from its actor. A type with thousands of referencing rows costs the
//! same as one with none.
//!
//! The index is written after the journal (ADR-0148), and a committed change
//! stays committed when its projection fails, so the index can only vouch for
//! rows whose catalog entry is current: no journal event newer than the one it
//! was projected from. Before probing, the lookup reads which catalog rows are
//! current. A probe hit counts only on a current row, and the live rows the
//! index cannot vouch for (never projected, or projected before a later
//! change) are read from their actors. When the index cannot answer at all (no
//! query plane, a field declared `query_indexed = false`, an id too long to
//! index, a failed read), every live row is in that set. The set is read only
//! while it fits [`UNINDEXED_REFERENCE_READ_BUDGET`]; past it the delete is
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
/// Reading a row the index cannot vouch for can restore its actor from
/// storage, and production restores about 40 actors a second, so this keeps a
/// delete to a few seconds of reads. Projection lag or a failed projection
/// leaves a handful of rows unvouched; a larger set means the type has no
/// usable projection, and reading all of it is the request that ran for
/// minutes before this lookup.
const UNINDEXED_REFERENCE_READ_BUDGET: usize = 100;

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

/// The field index's answer, before any unvouched rows are read.
enum IndexAnswer {
    /// A row the index vouches for holds the id.
    Referenced(String),
    /// No row the index vouches for holds the id.
    NoVouchedReference,
    /// The index cannot answer for this relation.
    Unavailable,
}

/// Which live rows of a type the field index can vouch for, read before it
/// is probed.
///
/// A row that is current when this is read holds, by the time of the probe,
/// either those fields or ones a later commit projected; a change committed
/// after this read races the delete as any concurrent write does. A row
/// projected after this read is in neither set: a hit on it is a committed
/// state that held the id, and it is in `unvouched` when live.
struct IndexCoverage {
    /// Catalog rows whose journal has moved past them. Their index entries
    /// may hold earlier fields, so a probe hit on one proves nothing.
    stale: BTreeSet<String>,
    /// Live rows the index cannot vouch for: never projected, or stale.
    unvouched: Vec<String>,
}

/// Check incoming relation policy before deleting an entity.
///
/// Each `Restrict` relation is answered by [`find_restricting_reference`], so
/// the check does not read every referencing entity. Returns a
/// `RelationUnverified` violation when there are too many rows the index
/// cannot vouch for to read one by one.
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
    unvouched = tracing::field::Empty,
))]
async fn find_restricting_reference(
    state: &ServerState,
    tenant: &TenantId,
    edge: &RelationEdge,
    entity_id: &str,
) -> ReferenceLookup {
    debug_assert!(!edge.from_entity.is_empty());
    debug_assert!(!edge.source_field.is_empty());

    let live_ids = state.list_entity_ids_lazy(tenant, &edge.from_entity).await;
    let unvouched = match index_coverage(state, tenant, &edge.from_entity, &live_ids).await {
        Some(coverage) => {
            match probe_field_index(state, tenant, edge, entity_id, &coverage).await {
                IndexAnswer::Referenced(source_id) => {
                    return ReferenceLookup::Referenced { source_id };
                }
                IndexAnswer::NoVouchedReference => coverage.unvouched,
                IndexAnswer::Unavailable => live_ids,
            }
        }
        None => live_ids,
    };
    tracing::Span::current().record("unvouched", unvouched.len());
    read_unindexed_rows(state, tenant, edge, entity_id, &unvouched).await
}

/// Read which catalog rows of `entity_type` are current, and from that which
/// of `live_ids` the index cannot vouch for. `None` when the store cannot say.
async fn index_coverage(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    live_ids: &[String],
) -> Option<IndexCoverage> {
    let query_plane = state.query_plane_store()?;
    let catalog = match query_plane
        .projection_catalog_freshness(tenant.as_str(), entity_type)
        .await
    {
        Ok(catalog) => catalog?,
        Err(error) => {
            tracing::warn!(
                tenant = %tenant, entity_type, error = %error,
                "projection catalog freshness read failed; treating every live row as unindexed"
            );
            return None;
        }
    };
    let mut current = BTreeSet::new();
    let mut stale = BTreeSet::new();
    for row in catalog {
        if row.current {
            current.insert(row.entity_id);
        } else {
            stale.insert(row.entity_id);
        }
    }
    let unvouched = live_ids
        .iter()
        .filter(|id| !current.contains(*id))
        .cloned()
        .collect();
    Some(IndexCoverage { stale, unvouched })
}

/// Ask the field index for a row the index vouches for whose `source_field`
/// is `entity_id`, using the same predicate an OData `$filter` equality
/// pushes down.
async fn probe_field_index(
    state: &ServerState,
    tenant: &TenantId,
    edge: &RelationEdge,
    entity_id: &str,
    coverage: &IndexCoverage,
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
    // Stale rows can fill the page; one more than there are of them reaches a
    // current row whenever one matches.
    let top = coverage.stale.len().saturating_add(1);
    match query_plane
        .query_field_index_page(
            tenant.as_str(),
            &edge.from_entity,
            &predicate.where_clause,
            predicate.params,
            &[],
            0,
            top,
            false,
        )
        .await
    {
        Ok(Some(page)) => {
            debug_assert!(page.entity_ids.len() <= top);
            match page
                .entity_ids
                .into_iter()
                .find(|id| !coverage.stale.contains(id))
            {
                Some(source_id) => IndexAnswer::Referenced(source_id),
                None => IndexAnswer::NoVouchedReference,
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
                "{} {} rows are not current in the query index, more than the {} a \
                 delete reads one by one; retry once the index has caught up",
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
                        "{}('{}') is not current in the query index and could not be read: {error}",
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
