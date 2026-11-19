//! One SQL page, and its count, when every row is allowed and the SQL
//! predicate is exactly the filter: no per-row loop and no scan budget.

use super::super::types::{QueryPlaneCoverageReport, QueryPlaneReadError};
use super::native::try_query_native_page;
use super::{
    CandidateScanResult, NativeCandidatePagePlan, RowAuthz, ScanCounters, TelemetryInput,
    apply_select_only, base_telemetry, materialize_filter_and_authorize_ids,
};
use crate::odata::query_plane_read::types::{
    QueryPlaneFallbackReason, QueryPlaneReadRequest, QueryPlaneReadStrategy,
};

/// Suppress the uniform page when any candidate needs its effective values
/// rechecked. The proof and window count run in the same SQL statement, so a
/// write cannot invalidate a separate completeness probe before counting.
fn uniform_page_plan(plan: &NativeCandidatePagePlan) -> NativeCandidatePagePlan {
    let mut uniform = plan.clone();
    if plan.filter_missing_fields.is_empty() {
        return uniform;
    }
    let gaps = plan
        .filter_missing_fields
        .iter()
        .map(|field| {
            let field_param = uniform.params.len() + 3;
            uniform.params.push(field.clone());
            format!(
                "entity_id NOT IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?{field_param})"
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    uniform.where_clause = format!(
        "({}) AND NOT EXISTS (SELECT 1 FROM entity_catalog \
         WHERE tenant = ?1 AND entity_type = ?2 AND ({}) AND ({gaps}))",
        plan.where_clause, plan.where_clause,
    );
    uniform
}

fn result(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
    counters: ScanCounters,
    entities: Vec<serde_json::Value>,
    count: Option<usize>,
) -> CandidateScanResult {
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::NativePagePushdown,
            fallback_reason: QueryPlaneFallbackReason::None,
            filter_pushdown: plan.filter_pushdown,
            catalog_materialization: true,
            coverage,
            counters,
            returned_count: entities.len(),
        },
    );
    CandidateScanResult {
        entities,
        count,
        telemetry,
    }
}

/// `$count` alone is the store's count of the filter: one SQL `COUNT`.
/// A filter whose stored values may be missing from the index cannot be
/// counted in SQL, so that read keeps the per-row path.
async fn try_uniform_count(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    if !plan.filter_missing_fields.is_empty() {
        return None;
    }
    let page = try_query_native_page(request, plan, 0, 0, true)
        .await
        .ok()??;
    let count = page.total_count?;
    let mut counters = ScanCounters::empty();
    counters.pushdown_sparse_probe_count = 1;
    Some(Ok(result(
        request,
        plan,
        coverage,
        counters,
        Vec::new(),
        Some(count),
    )))
}

/// One SQL page when every row is allowed and the filter, if any, is exactly
/// the query's predicate. `$count` then comes from the store's count instead
/// of a second pass over every document.
pub(super) async fn try_uniform_native_page(
    request: &QueryPlaneReadRequest<'_>,
    plan: &NativeCandidatePagePlan,
    coverage: QueryPlaneCoverageReport,
    authz: &RowAuthz,
    requested_top: usize,
    skip: usize,
    include_count: bool,
) -> Option<Result<CandidateScanResult, QueryPlaneReadError>> {
    if matches!(authz, RowAuthz::DenyAll) {
        let count = include_count.then_some(0);
        let counters = ScanCounters::empty();
        return Some(Ok(result(
            request,
            plan,
            coverage,
            counters,
            Vec::new(),
            count,
        )));
    }
    if !matches!(authz, RowAuthz::AllowAll) || !plan.filter_exact {
        return None;
    }
    if requested_top == 0 {
        return try_uniform_count(request, plan, coverage).await;
    }

    let uniform_plan = uniform_page_plan(plan);
    let mut page =
        match try_query_native_page(request, &uniform_plan, skip, requested_top, include_count)
            .await
        {
            Ok(Some(page)) => page,
            _ => return None,
        };
    if include_count && page.total_count.is_none() {
        return None;
    }
    // A page in an event-time order is counted by a second statement, so a
    // write between the two can leave the count short of the page.
    if let Some(count) = page.total_count
        && !page.entity_ids.is_empty()
        && count < skip.saturating_add(page.entity_ids.len())
    {
        return None;
    }
    let mut page_queries = 1;
    let mut page_ids = page.entity_ids.len();
    // An empty guarded page may mean the proof failed or skip passed the
    // last match. A guarded first page can prove the latter in its own
    // snapshot, retaining bounded counts without materializing documents.
    if !plan.filter_missing_fields.is_empty() && page.entity_ids.is_empty() {
        if skip == 0 {
            return None;
        }
        let first_page = try_query_native_page(request, &uniform_plan, 0, 1, true)
            .await
            .ok()??;
        let count = first_page.total_count?;
        if first_page.entity_ids.is_empty() || count > skip {
            return None;
        }
        page.total_count = Some(count);
        page_queries += 1;
        page_ids += first_page.entity_ids.len();
    }

    let mut counters = ScanCounters::empty();
    counters.pushdown_sparse_probe_count = page_queries;
    counters.pushdown_page_count = page_ids;
    let authorized =
        materialize_filter_and_authorize_ids(request, &page.entity_ids, true, authz, &mut counters)
            .await;
    if authorized.len() != page.entity_ids.len() {
        return None;
    }

    let count = include_count.then_some(page.total_count.unwrap_or(authorized.len()));
    let entities = apply_select_only(authorized, request.query_options);
    Some(Ok(result(
        request, plan, coverage, counters, entities, count,
    )))
}
