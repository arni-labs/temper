//! `$search`: reading an entity set ranked by its records' words and meaning
//! (ADR-0186).
//!
//! ```text
//! GET /tdata/Trips?$search=ferry to the coast&$filter=Status eq 'Open'&$top=10
//! ```
//!
//! Records whose text holds every searched word come first, then every other record
//! by closeness in meaning to the words, then by id. `$filter`, `$select`, `$top`,
//! `$skip`, `$count` and `$expand` apply as on any read, under the same Cedar list
//! and read gates. Each record carries `@temper.words` (it holds every word) and,
//! when it has a meaning, `@temper.score`.

use std::collections::BTreeMap;

use temper_odata::query::types::QueryOptions;

use super::super::authz::{LIST_ACTION, authorize_read, entity_id_from_body};
use super::scan::{
    RowAuthz, ScanCounters, TelemetryInput, apply_select_only, base_telemetry, decide_row_authz,
    entity_id_from_odata_id, materialize_filter_and_authorize_ids,
};
use super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadError, QueryPlaneReadRequest,
    QueryPlaneReadResult, QueryPlaneReadStrategy,
};
use crate::meaning::MeaningKind;
use crate::meaning::rank::{Ranked, rank_search, search_words};

/// A continuation names how many accepted records the next page skips.
const CONTINUATION_PREFIX: char = 's';

/// Read `request`'s entity set ranked for `search`.
pub(super) async fn read_by_search(
    request: QueryPlaneReadRequest<'_>,
    search: &str,
) -> Result<QueryPlaneReadResult, QueryPlaneReadError> {
    if let Err(response) = authorize_read(
        request.state,
        request.tenant,
        request.security_ctx,
        LIST_ACTION,
        request.entity_type,
        "",
        &serde_json::json!({}),
    ) {
        return Err(QueryPlaneReadError::AuthorizationDenied(response));
    }
    if request.query_options.orderby.is_some() {
        return Err(QueryPlaneReadError::Unsupported(
            "$search orders records by their words and meaning; drop $orderby, or drop $search to order by a field",
        ));
    }
    let Some((store, _)) = request.state.event_journal() else {
        return Err(QueryPlaneReadError::ReadUnavailable);
    };
    let skip = match request.query_options.skiptoken.as_deref() {
        Some(token) => token
            .strip_prefix(CONTINUATION_PREFIX)
            .and_then(|offset| offset.parse::<usize>().ok())
            .ok_or(QueryPlaneReadError::InvalidContinuation)?,
        None => request.query_options.skip.unwrap_or(0),
    };

    let words = search_words(search);
    let query = query_vector(&request, search).await;
    let ranked = rank_search(
        &store,
        request.tenant.as_str(),
        request.entity_type,
        &words,
        query.as_ref().map(|(vector, tag)| (vector.as_slice(), tag.as_str())),
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, entity_type = request.entity_type, "search: meaning rows unreadable");
        QueryPlaneReadError::ReadUnavailable
    })?;

    let take = request.budget.requested_top(request.query_options);
    let walk = walk_ranked(
        &request,
        &ranked,
        skip,
        take,
        request.query_options.count == Some(true),
        true,
    )
    .await;
    let telemetry = base_telemetry(
        &request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::MeaningSearch,
            fallback_reason: QueryPlaneFallbackReason::None,
            filter_pushdown: false,
            catalog_materialization: true,
            coverage: QueryPlaneCoverageReport::default(),
            counters: walk.counters,
            returned_count: walk.entities.len(),
        },
    );
    Ok(QueryPlaneReadResult {
        next_skiptoken: walk
            .more
            .then(|| format!("{CONTINUATION_PREFIX}{}", skip + walk.entities.len())),
        entities: walk.entities,
        count: walk.count,
        telemetry,
    })
}

/// The search's vector and the model that made it, when the server has a model that
/// can make one. Without it the search ranks by words alone.
async fn query_vector(
    request: &QueryPlaneReadRequest<'_>,
    search: &str,
) -> Option<(Vec<f32>, String)> {
    let model = request.state.meaning_model()?;
    let texts = [search.to_string()];
    match model.embed(&texts, MeaningKind::Query).await {
        Ok(mut vectors) if vectors.len() == 1 => {
            Some((vectors.swap_remove(0), model.tag().to_string()))
        }
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, model = model.tag(), "search: no vector for the words; ranking by words");
            None
        }
    }
}

/// One page of a ranked walk.
pub(in crate::odata) struct RankedPage {
    /// The page's records, in rank order, annotated.
    pub(in crate::odata) entities: Vec<serde_json::Value>,
    /// How many records pass the filter and read gate, when asked.
    pub(in crate::odata) count: Option<usize>,
    /// Whether a record passes after the page.
    pub(in crate::odata) more: bool,
    pub(in crate::odata::query_plane_read) counters: ScanCounters,
}

/// Walk `ranked` in order, reading records a batch at a time and keeping those that
/// pass `request`'s `$filter` and the Cedar read gate: skip the first `skip` that
/// pass and keep the next `take`. With `count_all`, walk on to count every one that
/// passes. `$select` applies to the kept records, then the rank annotations.
pub(in crate::odata) async fn walk_ranked(
    request: &QueryPlaneReadRequest<'_>,
    ranked: &[Ranked],
    skip: usize,
    take: usize,
    count_all: bool,
    words_annotation: bool,
) -> RankedPage {
    let authz = decide_row_authz(request);
    let mut counters = ScanCounters::empty();
    let mut entities: Vec<(serde_json::Value, &Ranked)> = Vec::new();
    let mut passed = 0usize;
    let mut more = false;
    // Without a filter and with every row readable, every ranked record passes: the
    // count is the ranking's length, and the walk stops at the page.
    let every_record_passes =
        request.query_options.filter.is_none() && matches!(authz, RowAuthz::AllowAll);
    let walk_all = count_all && !every_record_passes;
    for batch in ranked.chunks(request.budget.scan_page_size(request.query_options)) {
        if more && !walk_all {
            break;
        }
        let ids: Vec<String> = batch.iter().map(|r| r.entity_id.clone()).collect();
        let mut bodies: BTreeMap<String, serde_json::Value> =
            materialize_filter_and_authorize_ids(request, &ids, true, &authz, &mut counters)
                .await
                .into_iter()
                .filter(|body| body.get("status").and_then(|s| s.as_str()) != Some("Deleted"))
                // A `$select` read comes back with only the chosen fields and its
                // `@odata.id`, which then names the record.
                .filter_map(|body| {
                    let id = entity_id_from_body(&body)
                        .map(str::to_string)
                        .or_else(|| entity_id_from_odata_id(&body))?;
                    Some((id, body))
                })
                .collect();
        for rank in batch {
            let Some(body) = bodies.remove(&rank.entity_id) else {
                continue;
            };
            passed += 1;
            if passed <= skip {
                continue;
            }
            if entities.len() < take {
                entities.push((body, rank));
            } else {
                more = true;
                if !walk_all {
                    break;
                }
            }
        }
    }
    let count = count_all.then_some(if every_record_passes {
        ranked.len()
    } else {
        passed
    });
    let (bodies, ranks): (Vec<_>, Vec<_>) = entities.into_iter().unzip();
    let entities = apply_select_only(bodies, &select_only(request.query_options))
        .into_iter()
        .zip(ranks)
        .map(|(mut body, rank)| {
            if let Some(object) = body.as_object_mut() {
                if words_annotation {
                    object.insert("@temper.words".to_string(), rank.words.into());
                }
                if let Some(score) = rank.score
                    && let Some(number) = serde_json::Number::from_f64(f64::from(score))
                {
                    object.insert("@temper.score".to_string(), number.into());
                }
            }
            body
        })
        .collect();
    RankedPage {
        entities,
        count,
        more,
        counters,
    }
}

fn select_only(query_options: &QueryOptions) -> QueryOptions {
    QueryOptions {
        select: query_options.select.clone(),
        ..QueryOptions::default()
    }
}
