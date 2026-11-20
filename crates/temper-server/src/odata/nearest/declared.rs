//! Compatibility for callers with vectors from their own embedding model.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use super::super::authz::entity_id_from_body;
use super::super::read_support::materialize_entity_set_entities;
use super::{QueryPlaneReadRequest, arg, bad_request, nearest_store, no_store};
use crate::meaning::rank::{Ranked, closeness};
use crate::response::odata_error;

#[derive(Deserialize)]
struct DeclaredPaths {
    #[serde(default, rename = "vector")]
    paths: Vec<VectorPath>,
}

#[derive(Deserialize)]
struct VectorPath {
    name: String,
    property: String,
    model_property: String,
    dims: usize,
    metric: Metric,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Metric {
    Cosine,
    Dot,
    L2,
}

pub(super) async fn rank(
    request: &QueryPlaneReadRequest<'_>,
    params: &[(String, String)],
    raw_vector: &str,
) -> Result<Vec<Ranked>, Response> {
    let name = arg(params, "decl")
        .ok_or_else(|| bad_request("'decl' is required when querying by raw 'vector'"))?;
    let model = arg(params, "model")
        .filter(|model| !model.is_empty())
        .ok_or_else(|| bad_request("'model' is required when querying by raw 'vector'"))?;

    // Meaning search ignores [[vector]], but the registry still holds the installed
    // spec. Only an existing declaration can select the fields to rank.
    let path = {
        let registry = request.state.registry.read().unwrap();
        registry
            .get_spec(request.tenant, request.entity_type)
            .and_then(|spec| toml::from_str::<DeclaredPaths>(&spec.ioa_source).ok())
            .and_then(|decls| decls.paths.into_iter().find(|path| path.name == name))
    }
    .ok_or_else(|| bad_request(&format!("unknown vector path '{name}'")))?;
    let query =
        parse_vector(&Value::String(raw_vector.to_owned()), path.dims).ok_or_else(|| {
            bad_request(&format!(
                "'vector' must be a JSON array of exactly {} finite numbers",
                path.dims
            ))
        })?;

    // The retired index is no longer updated. Read the current record fields
    // within the normal query-plane scan budget.
    let budget = request.budget.scan_candidate_budget();
    let ids: Vec<String> = nearest_store(request.state)
        .ok_or_else(no_store)?
        .list_entity_ids_limited(
            request.tenant.as_str(),
            Some(request.entity_type),
            budget.saturating_add(1),
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, "could not list candidates for declared vector search");
            odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ReadUnavailable",
                "vector search candidates could not be read",
            )
            .into_response()
        })?
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    if ids.len() > budget {
        return Err(odata_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "QueryTooLarge",
            "vector search exceeds the candidate read budget",
        )
        .into_response());
    }

    let mut select = vec!["entity_id".to_owned()];
    for field in [&path.property, &path.model_property] {
        select.extend([
            field.clone(),
            temper_spec::to_snake_case(field),
            temper_spec::to_pascal_case(field),
        ]);
    }
    select.sort();
    select.dedup();
    let mut ranked = Vec::new();
    for batch in ids.chunks(request.budget.scan_page_size(request.query_options)) {
        let records = materialize_entity_set_entities(
            request.state,
            request.tenant,
            request.entity_type,
            request.entity_set_name,
            batch,
            true,
            Some(&select),
        )
        .await;
        ranked.extend(records.entities.iter().filter_map(|body| {
            if field(body, &path.model_property)?.as_str()? != model {
                return None;
            }
            let vector = parse_vector(field(body, &path.property)?, path.dims)?;
            Some(Ranked {
                entity_id: entity_id_from_body(body)?.to_owned(),
                words: false,
                score: Some(path.metric.score(&query, &vector)?),
            })
        }));
    }
    ranked.sort_by(|a, b| {
        b.score
            .unwrap_or_default()
            .total_cmp(&a.score.unwrap_or_default())
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    Ok(ranked)
}

fn field<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    let snake = temper_spec::to_snake_case(name);
    let pascal = temper_spec::to_pascal_case(name);
    let fields = body.get("fields").unwrap_or(&Value::Null);
    [fields, body].into_iter().find_map(|record| {
        record
            .get(name)
            .or_else(|| record.get(&snake))
            .or_else(|| record.get(&pascal))
    })
}

fn parse_vector(value: &Value, dims: usize) -> Option<Vec<f32>> {
    let decoded;
    let items = match value {
        Value::String(text) => {
            decoded = serde_json::from_str::<Vec<Value>>(text).ok()?;
            &decoded
        }
        _ => value.as_array()?,
    };
    if dims == 0 || items.len() != dims {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let component = item.as_f64()? as f32;
            component.is_finite().then_some(component)
        })
        .collect()
}

impl Metric {
    fn score(&self, query: &[f32], candidate: &[f32]) -> Option<f32> {
        let pairs = query
            .iter()
            .zip(candidate)
            .map(|(&q, &c)| (f64::from(q), f64::from(c)));
        let score = match self {
            Self::Cosine => closeness(query, candidate).unwrap_or_default(),
            Self::Dot => pairs.map(|(q, c)| q * c).sum::<f64>() as f32,
            Self::L2 => -pairs.map(|(q, c)| (q - c).powi(2)).sum::<f64>().sqrt() as f32,
        };
        score.is_finite().then_some(score)
    }
}
