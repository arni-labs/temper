//! Native query-plane pages: the store orders, bounds and pages the
//! candidates in SQL, and rows are materialized and authorized a page at a
//! time.

use super::super::types::{QueryPlaneCoverageReport, QueryPlaneReadError};
use super::uniform::try_uniform_native_page;
use super::{
    CandidateScanResult, NativeCandidatePagePlan, RowAuthz, ScanCounters, TelemetryInput,
    apply_select_only, base_telemetry, decide_row_authz, filter_and_authorize, materialize_ids,
};
use crate::odata::authz::entity_id_from_body;
use crate::odata::query_plane_read::pagination::{canonical_pagination_order, sql_keyset_for_row};
use crate::odata::query_plane_read::types::{
    QueryPlaneFallbackReason, QueryPlaneReadRequest, QueryPlaneReadStrategy,
};
use crate::storage::{
    QueryFieldIndexOrderDirection, QueryFieldIndexPage, QueryFieldIndexPageQuery,
};
use temper_odata::query::types::{OrderByClause, OrderDirection};
use temper_runtime::persistence::{FieldClass, KeysetValue};

/// The keyset of a page's last row, `last_id`, under `plan`'s order: where
/// the next page starts. `None` when the row is not in `page` (it left
/// between the page and its documents) or a clause of the order is not a
/// key the store can seek by.
fn resume_after(
    plan: &NativeCandidatePagePlan,
    last_id: &str,
    page: &[serde_json::Value],
) -> Option<Vec<KeysetValue>> {
    if plan.order_by.is_empty() {
        return Some(vec![KeysetValue::Key(last_id.to_string())]);
    }
    let row = page
        .iter()
        .find(|entity| entity_id_from_body(entity) == Some(last_id))?;
    let order: Vec<OrderByClause> = plan
        .order_by
        .iter()
        .map(|clause| OrderByClause {
            property: clause.field_name.clone(),
            direction: match clause.direction {
                QueryFieldIndexOrderDirection::Asc => OrderDirection::Asc,
                QueryFieldIndexOrderDirection::Desc => OrderDirection::Desc,
            },
        })
        .collect();
    sql_keyset_for_row(row, &canonical_pagination_order(Some(&order)))
}

/// One store page of `plan`: `top` ids from `skip` rows past its start (the
/// first row, or the row after its continuation's cursor).
pub(super) async fn try_query_native_page(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    skip: usize,
    top: usize,
    include_count: bool,
) -> Result<Option<QueryFieldIndexPage>, ()> {
    run_page(request, &page_query(plan, skip, top, include_count)).await
}

fn page_query(
    plan: &NativeCandidatePagePlan,
    skip: usize,
    top: usize,
    include_count: bool,
) -> QueryFieldIndexPageQuery {
    QueryFieldIndexPageQuery {
        where_clause: plan.where_clause.clone(),
        params: plan.params.clone(),
        event_time_bounds: plan.event_time_bounds.clone(),
        field_comparisons: plan.field_comparisons.clone(),
        text_matches: plan.text_matches.clone(),
        mistyped: Vec::new(),
        order_by: plan.order_by.clone(),
        after: plan.after.clone(),
        skip,
        top,
        include_count,
    }
}

/// Whether no candidate holds, for an app field the order sorts by, a value
/// of another JSON type than the order compares (a JSON null included).
/// The in-memory sort finds such values equal to everything, so only then is
/// SQL's order its order (ADR-0188). One probe of the filter's rows, which
/// stops at the first such row.
async fn field_orders_typed(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
) -> Option<bool> {
    fields_typed(request, plan, &plan.field_orders).await
}

/// Whether no row `plan`'s filter selects holds, for one of `fields`, a
/// value of another JSON type than its class (a JSON null included). `None`
/// when the store cannot say.
pub(in crate::odata::query_plane_read) async fn fields_typed(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    fields: &[(String, FieldClass)],
) -> Option<bool> {
    if fields.is_empty() {
        return Some(true);
    }
    let mut probe = page_query(plan, 0, 1, false);
    probe.order_by.clear();
    probe.after = None;
    probe.mistyped = fields.to_vec();
    let page = run_page(request, &probe).await.ok()??;
    Some(page.entity_ids.is_empty())
}

async fn run_page(
    request: &QueryPlaneReadRequest<'_>,
    page: &QueryFieldIndexPageQuery,
) -> Result<Option<QueryFieldIndexPage>, ()> {
    let Some(query_plane) = request.state.query_plane_store() else {
        return Ok(None);
    };
    query_plane
        .query_field_index_page(request.tenant.as_str(), request.entity_type, page)
        .await
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                tenant = %request.tenant,
                entity_type = %request.entity_type,
                "native OData query-plane page failed; falling back"
            );
        })
}

/// A bounded probe detects sort values that need default materialization.
/// Filter completeness is checked inside the uniform page's SQL snapshot.
async fn native_values_present(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    fields: &[String],
) -> Option<bool> {
    for field in fields {
        let mut probe = plan.clone();
        let field_param = probe.params.len() + 3;
        probe.params.push(field.clone());
        probe.where_clause = format!(
            "({}) AND entity_id NOT IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?{field_param})",
            probe.where_clause,
        );
        probe.order_by.clear();
        probe.after = None;
        let page = try_query_native_page(request, &probe, 0, 1, false)
            .await
            .ok()??;
        if !page.entity_ids.is_empty() {
            return Some(false);
        }
    }
    Some(true)
}

pub(in crate::odata::query_plane_read) async fn try_native_page_read(
    request: &QueryPlaneReadRequest<'_>,
    plan: NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    let requested_top = request.budget.requested_top(request.query_options);
    let skip = request.query_options.skip.unwrap_or(0);
    let include_count = request.query_options.count == Some(true);
    if requested_top == 0 && !include_count {
        let counters = ScanCounters::empty();
        let telemetry = base_telemetry(
            request,
            TelemetryInput {
                strategy: QueryPlaneReadStrategy::NativePagePushdown,
                fallback_reason: QueryPlaneFallbackReason::None,
                filter_pushdown: plan.filter_pushdown,
                catalog_materialization: true,
                coverage,
                counters,
                returned_count: 0,
            },
        );
        return Some(Ok(CandidateScanResult {
            entities: Vec::new(),
            count: None,
            telemetry,
        }));
    }

    let authz = decide_row_authz(request);
    if !matches!(authz, RowAuthz::DenyAll) {
        let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
            request.state,
            request.tenant,
            request.entity_type,
        )
        .field_values(&serde_json::Value::Null);
        let sort_fields = plan
            .order_by
            .iter()
            .filter(|clause| defaults.get(&clause.field_name).is_some())
            .map(|clause| clause.field_name.clone())
            .collect::<Vec<_>>();
        if !native_values_present(request, &plan, &sort_fields).await?
            || (requested_top > 0 && !field_orders_typed(request, &plan).await?)
        {
            return None;
        }
    }
    if let Some(result) = try_uniform_native_page(
        request,
        &plan,
        coverage,
        &authz,
        requested_top,
        skip,
        include_count,
    )
    .await
    {
        return Some(result);
    }

    // ADR-0188: no scan budget. The rows are walked a store page at a time,
    // each page starting after the last row of the one before (a keyset the
    // store seeks to), keeping only the requested page and a running count,
    // until the page is full or, for `$count`, the last row is judged. What
    // bounds the walk is time: on 2026-11-19 an Apple M3 Max (16 cores,
    // 128 GB, macOS 26.5, not idle) running a release build on a Turso file
    // store judged about 4,350 rows a second, a `$count` of 12,000 records
    // whose policy hides 6,000 taking 2.76 s (median of 7) as a non-admin
    // reader. A million such rows is about four minutes (ADR-0188).
    let page_size = request.budget.scan_page_size(request.query_options);
    let mut walk = plan.clone();
    let mut storage_skip = 0;
    let mut authorized_seen = 0usize;
    let mut returned_before_select = Vec::new();
    let mut counters = ScanCounters::empty();

    loop {
        // The loop counts the rows it authorizes, so no page asks the store
        // for a count. A page whose last row has no keyset the store can
        // seek by moves on by `storage_skip` rows past the last cursor.
        let page = match try_query_native_page(request, &walk, storage_skip, page_size, false).await
        {
            Ok(Some(page)) => page,
            Ok(None) | Err(()) => return None,
        };
        counters.pushdown_sparse_probe_count += 1;
        counters.pushdown_page_count += page.entity_ids.len();
        let Some(last_id) = page.entity_ids.last().cloned() else {
            break;
        };
        let page_len = page.entity_ids.len();
        let materialized =
            materialize_ids(request, &page.entity_ids, true, &authz, &mut counters).await;
        let resume = resume_after(&walk, &last_id, &materialized);
        counters.hold(returned_before_select.len() + materialized.len());
        // Rows before `$skip` are counted, never kept.
        for entity in filter_and_authorize(request, materialized, &authz) {
            let index = authorized_seen;
            authorized_seen += 1;
            if index >= skip && returned_before_select.len() < requested_top {
                returned_before_select.push(entity);
            }
        }

        if !include_count && returned_before_select.len() >= requested_top {
            break;
        }
        if page_len < page_size {
            break;
        }
        match resume {
            Some(after) => {
                walk.after = Some(after);
                storage_skip = 0;
            }
            None => storage_skip += page_len,
        }
        super::walk_yield().await;
    }

    let count = include_count.then_some(authorized_seen);
    let entities = apply_select_only(returned_before_select, request.query_options);
    let returned_count = entities.len();
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::NativePagePushdown,
            fallback_reason: QueryPlaneFallbackReason::None,
            filter_pushdown: plan.filter_pushdown,
            catalog_materialization: true,
            coverage,
            counters,
            returned_count,
        },
    );
    Some(Ok(CandidateScanResult {
        entities,
        count,
        telemetry,
    }))
}
