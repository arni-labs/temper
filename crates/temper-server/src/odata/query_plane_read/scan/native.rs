//! Native query-plane pages: the store orders, bounds and pages the
//! candidates in SQL, and rows are materialized and authorized a page at a
//! time.

use super::super::types::{QueryPlaneCoverageReport, QueryPlaneReadError};
use super::uniform::try_uniform_native_page;
use super::{
    CandidateScanResult, NativeCandidatePagePlan, RowAuthz, ScanCounters, TelemetryInput,
    apply_select_only, base_telemetry, budget_rejection, decide_row_authz,
    materialize_filter_and_authorize_ids,
};
use crate::odata::query_plane_read::types::{
    QueryPlaneFallbackReason, QueryPlaneReadRequest, QueryPlaneReadStrategy,
};
use crate::storage::{QueryFieldIndexPage, QueryFieldIndexPageQuery};

/// One store page of `plan`: `top` ids from `skip` rows past its start (the
/// first row, or the row after its continuation's cursor).
pub(super) async fn try_query_native_page(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    skip: usize,
    top: usize,
    include_count: bool,
) -> Result<Option<QueryFieldIndexPage>, ()> {
    let Some(query_plane) = request.state.query_plane_store() else {
        return Ok(None);
    };
    let page = QueryFieldIndexPageQuery {
        where_clause: plan.where_clause.clone(),
        params: plan.params.clone(),
        event_time_bounds: plan.event_time_bounds.clone(),
        field_comparisons: plan.field_comparisons.clone(),
        order_by: plan.order_by.clone(),
        after: plan.after.clone(),
        skip,
        top,
        include_count,
    };
    query_plane
        .query_field_index_page(request.tenant.as_str(), request.entity_type, &page)
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
        if !native_values_present(request, &plan, &sort_fields).await? {
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

    let scan_budget = request.budget.scan_candidate_budget();
    let page_size = request.budget.scan_page_size(request.query_options);
    let mut storage_offset = 0;
    let mut authorized_seen = 0usize;
    let mut returned_before_select = Vec::new();
    let mut counters = ScanCounters::empty();

    loop {
        let remaining_budget = scan_budget.saturating_sub(counters.candidate_count);
        if remaining_budget == 0 {
            return Some(Err(budget_rejection(
                request,
                QueryPlaneReadStrategy::NativePagePushdown,
                plan.filter_pushdown,
                true,
                coverage,
                counters,
            )));
        }
        let storage_top = page_size.min(remaining_budget);
        // The loop counts the rows it authorizes, so no page asks the store
        // for a count. `storage_offset` counts from the continuation's
        // cursor, which the store seeks to, not from the first row.
        let page =
            match try_query_native_page(request, &plan, storage_offset, storage_top, false).await {
                Ok(Some(page)) => page,
                Ok(None) => return None,
                Err(()) => return None,
            };
        counters.pushdown_sparse_probe_count += 1;
        counters.pushdown_page_count += page.entity_ids.len();

        if page.entity_ids.is_empty() {
            break;
        }

        let page_len = page.entity_ids.len();
        let authorized = materialize_filter_and_authorize_ids(
            request,
            &page.entity_ids,
            true,
            &authz,
            &mut counters,
        )
        .await;
        for entity in authorized {
            let index = authorized_seen;
            authorized_seen += 1;
            if index >= skip && returned_before_select.len() < requested_top {
                returned_before_select.push(entity);
            }
        }

        storage_offset += page_len;
        if !include_count && returned_before_select.len() >= requested_top {
            break;
        }
        if page_len < storage_top {
            break;
        }
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
