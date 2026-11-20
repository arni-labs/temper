//! The SQL page a read asks the query plane for: its predicate, event-time
//! bounds, order, and where a continuation starts (ADR-0187).

use temper_odata::query::types::{FilterExpr, OrderDirection};
use temper_runtime::persistence::{CatalogOrderKey, EventTimeColumn, FieldClass, KeysetValue};

use super::super::super::filter_sql;
use super::NativeCandidatePagePlan;
use crate::odata::query_plane_read::types::QueryPlaneReadRequest;
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexOrderDirection};

/// A continuation the store resumes in SQL: the caller's own filter, without
/// the keyset predicate the read's `$filter` also carries, and the cursor's
/// key for each clause of the page's total order.
pub(in crate::odata::query_plane_read) struct SqlKeyset {
    pub(in crate::odata::query_plane_read) filter: Option<FilterExpr>,
    pub(in crate::odata::query_plane_read) after: Vec<KeysetValue>,
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
    field_order_class(request, property).is_some()
}

/// The JSON type an order by app field `property` compares: a number or a
/// string, for a declared, non-nullable scalar the query projection holds
/// and the record does not also hold at its top. A boolean is never
/// ordered: the in-memory sort does not compare booleans.
pub(in crate::odata::query_plane_read) fn field_order_class(
    request: &QueryPlaneReadRequest<'_>,
    property: &str,
) -> Option<FieldClass> {
    if filter_sql::is_record_property(property)
        || !request.state.query_projection_indexes_field(
            request.tenant,
            request.entity_type,
            property,
        )
    {
        return None;
    }
    let (class, nullable) = declared_scalar(request, property)?;
    (!nullable && class != FieldClass::Boolean).then_some(class)
}

/// The JSON type of `property` and whether it is nullable, when the entity
/// type declares it as a scalar a comparison reads.
pub(in crate::odata::query_plane_read) fn declared_scalar(
    request: &QueryPlaneReadRequest<'_>,
    property: &str,
) -> Option<(FieldClass, bool)> {
    let registry = request
        .state
        .registry
        .read()
        .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
    let declared = registry
        .get_tenant(request.tenant)?
        .csdl
        .schemas
        .iter()
        .flat_map(|schema| &schema.entity_types)
        .find(|entity_type| entity_type.name == request.entity_type)?
        .properties
        .iter()
        .find(|candidate| candidate.name == property)?;
    filter_sql::edm_class(&declared.type_name).map(|class| (class, declared.nullable))
}

#[cfg(test)]
pub(in crate::odata::query_plane_read) fn native_candidate_page_plan(
    request: &QueryPlaneReadRequest<'_>,
) -> Option<NativeCandidatePagePlan> {
    native_candidate_page_plan_after(request, None)
}

/// The page plan, resuming `keyset` in SQL when there is one. An order by an
/// app field names the field's JSON type, which the reader checks every
/// candidate holds before it trusts SQL's order (ADR-0188).
pub(in crate::odata::query_plane_read) fn native_candidate_page_plan_after(
    request: &QueryPlaneReadRequest<'_>,
    keyset: Option<&SqlKeyset>,
) -> Option<NativeCandidatePagePlan> {
    let order_by = native_order_by(request)?;
    let field_orders = order_by
        .iter()
        .filter(|clause| {
            matches!(
                CatalogOrderKey::of(&clause.field_name),
                CatalogOrderKey::Field(_)
            )
        })
        .map(|clause| {
            field_order_class(request, &clause.field_name)
                .map(|class| (clause.field_name.clone(), class))
        })
        .collect::<Option<Vec<_>>>()?;
    let filter = match keyset {
        Some(keyset) => keyset.filter.as_ref(),
        None => request.query_options.filter.as_ref(),
    };
    let mut plan = filter_plan(request, filter);
    plan.order_by = order_by;
    plan.field_orders = field_orders;
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
        field_comparisons: Vec::new(),
        order_by: Vec::new(),
        field_orders: Vec::new(),
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
    let fields = filter_sql::split_field_comparisons(&rest, &declared_fields(request, &defaults));
    plan.filter_pushdown |= !fields.comparisons.is_empty();
    plan.field_comparisons = fields.comparisons;
    let mut missing_fields = fields.missing_default_fields;
    let Some(rest) = fields.rest else {
        plan.filter_missing_fields = missing_fields.into_iter().collect();
        return plan;
    };
    match filter_sql::try_translate_candidate_filter_with_defaults(&rest, &defaults) {
        Some(translated) => {
            // Missing index entries include omitted/oversized stored fields.
            // Recheck candidates before counting or applying skip/top.
            plan.where_clause = translated.where_clause;
            plan.params = translated.params;
            plan.filter_pushdown = true;
            plan.filter_exact = filter_sql::candidate_filter_is_lossless(&rest);
            missing_fields.extend(translated.missing_default_fields);
        }
        None => plan.filter_exact = false,
    }
    plan.filter_missing_fields = missing_fields.into_iter().collect();
    plan
}

/// The app fields this type's reads compare on typed values: its declared,
/// non-nullable scalar properties that the query projection holds, with the
/// ones a record without the key reads as a declared default.
fn declared_fields(
    request: &QueryPlaneReadRequest<'_>,
    defaults: &serde_json::Value,
) -> filter_sql::DeclaredFields {
    let mut declared = filter_sql::DeclaredFields::default();
    let properties: Vec<(String, String)> = {
        let registry = request
            .state
            .registry
            .read()
            .expect("registry lock should not be poisoned"); // ci-ok: infallible lock
        registry
            .get_tenant(request.tenant)
            .into_iter()
            .flat_map(|tenant| &tenant.csdl.schemas)
            .flat_map(|schema| &schema.entity_types)
            .filter(|entity_type| entity_type.name == request.entity_type)
            .flat_map(|entity_type| &entity_type.properties)
            .filter(|property| !property.nullable)
            .map(|property| (property.name.clone(), property.type_name.clone()))
            .collect()
    };
    for (name, type_name) in properties {
        let indexed = request.state.query_projection_indexes_field(
            request.tenant,
            request.entity_type,
            &name,
        );
        if let Some(class) = filter_sql::edm_class(&type_name)
            && indexed
        {
            declared.declare(&name, class);
        }
    }
    for name in defaults
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
    {
        declared.default_for(name);
    }
    declared
}
