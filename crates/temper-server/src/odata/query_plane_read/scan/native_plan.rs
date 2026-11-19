//! The SQL page a read asks the query plane for: its predicate, event-time
//! bounds, order, and where a continuation starts (ADR-0187).

use temper_odata::query::types::{FilterExpr, OrderDirection};
use temper_runtime::persistence::{CatalogOrderKey, EventTimeColumn};

use super::super::super::filter_sql;
use super::NativeCandidatePagePlan;
use crate::odata::query_plane_read::types::QueryPlaneReadRequest;
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexOrderDirection};

/// A continuation the store resumes in SQL: the caller's own filter, without
/// the keyset predicate the read's `$filter` also carries, and the cursor's
/// key for each clause of the page's total order.
pub(in crate::odata::query_plane_read) struct SqlKeyset {
    pub(in crate::odata::query_plane_read) filter: Option<FilterExpr>,
    pub(in crate::odata::query_plane_read) after: Vec<String>,
}

fn native_order_by(request: &QueryPlaneReadRequest<'_>) -> Option<Vec<QueryFieldIndexOrder>> {
    let query_options = request.query_options;
    query_options
        .orderby
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|clause| {
            native_order_property_is_lossless(request, &clause.property).then(|| {
                QueryFieldIndexOrder {
                    field_name: clause.property.clone(),
                    direction: match clause.direction {
                        OrderDirection::Asc => QueryFieldIndexOrderDirection::Asc,
                        OrderDirection::Desc => QueryFieldIndexOrderDirection::Desc,
                    },
                }
            })
        })
        .collect()
}

fn native_order_property_is_lossless(request: &QueryPlaneReadRequest<'_>, property: &str) -> bool {
    // The stores order an event time by the key of its instant, as the
    // in-memory sort does. Other kernel event metadata is in the catalog
    // state, never the app field index.
    if EventTimeColumn::of(property).is_some() {
        return true;
    }
    if temper_spec::automaton::is_event_metadata_field(property) {
        return false;
    }
    if matches!(property, "entity_id" | "Id" | "id" | "Status" | "status") {
        return true;
    }

    let registry = request
        .state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
    registry
        .get_tenant(request.tenant)
        .and_then(|tenant_config| {
            tenant_config
                .csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.entity_types)
                .find(|entity_type| entity_type.name == request.entity_type)
        })
        .and_then(|entity_type| {
            entity_type
                .properties
                .iter()
                .find(|candidate| candidate.name == property)
        })
        .is_some_and(|property| {
            !property.nullable && native_order_type_is_lossless(&property.type_name)
        })
}

fn native_order_type_is_lossless(type_name: &str) -> bool {
    matches!(
        type_name,
        "Edm.String"
            | "Edm.Guid"
            | "Edm.Date"
            | "Edm.DateTimeOffset"
            | "Edm.TimeOfDay"
            | "Edm.Byte"
            | "Edm.SByte"
            | "Edm.Int16"
            | "Edm.Int32"
            | "Edm.Int64"
            | "Edm.Decimal"
            | "Edm.Double"
            | "Edm.Single"
    )
}

#[cfg(test)]
pub(in crate::odata::query_plane_read) fn native_candidate_page_plan(
    request: &QueryPlaneReadRequest<'_>,
) -> Option<NativeCandidatePagePlan> {
    native_candidate_page_plan_after(request, None)
}

/// The page plan, resuming `keyset` in SQL when every clause of the order is
/// a catalog key. An order with an app field keeps the keyset folded into
/// the filter, which narrows candidates only in memory.
pub(in crate::odata::query_plane_read) fn native_candidate_page_plan_after(
    request: &QueryPlaneReadRequest<'_>,
    keyset: Option<&SqlKeyset>,
) -> Option<NativeCandidatePagePlan> {
    let order_by = native_order_by(request)?;
    let keyset = keyset.filter(|_| {
        order_by
            .iter()
            .all(|clause| CatalogOrderKey::of(&clause.field_name).is_total())
    });
    let filter = match keyset {
        Some(keyset) => keyset.filter.as_ref(),
        None => request.query_options.filter.as_ref(),
    };
    let mut plan = filter_plan(request, filter);
    plan.order_by = order_by;
    plan.after = keyset.map(|keyset| keyset.after.clone());
    Some(plan)
}

/// The SQL form of `filter`: event-time conjuncts as bounds, and the rest
/// translated where it can be.
fn filter_plan(
    request: &QueryPlaneReadRequest<'_>,
    filter: Option<&FilterExpr>,
) -> NativeCandidatePagePlan {
    let mut plan = NativeCandidatePagePlan {
        where_clause: "TRUE".to_string(),
        params: Vec::new(),
        event_time_bounds: Vec::new(),
        order_by: Vec::new(),
        after: None,
        filter_pushdown: false,
        unfiltered: filter.is_none(),
        filter_exact: true,
        filter_missing_fields: Vec::new(),
    };
    let Some(filter) = filter else {
        return plan;
    };
    let split = filter_sql::split_event_time_bounds(filter);
    plan.filter_pushdown = !split.bounds.is_empty();
    plan.event_time_bounds = split.bounds;
    let Some(rest) = split.rest else {
        return plan;
    };
    let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
        request.state,
        request.tenant,
        request.entity_type,
    )
    .field_values(&serde_json::Value::Null);
    match filter_sql::try_translate_candidate_filter_with_defaults(&rest, &defaults) {
        Some(translated) => {
            // Missing index entries include omitted/oversized stored fields.
            // Recheck candidates before counting or applying skip/top.
            plan.where_clause = translated.where_clause;
            plan.params = translated.params;
            plan.filter_pushdown = true;
            plan.filter_exact = filter_sql::candidate_filter_is_lossless(&rest);
            plan.filter_missing_fields = translated.missing_default_fields.into_iter().collect();
        }
        None => plan.filter_exact = false,
    }
    plan
}
