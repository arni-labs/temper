//! Facet reads: `GET /tdata/<Set>?$apply=groupby((<field>),aggregate($count
//! as count))`, the records a read would list counted by the value of one
//! declared scalar property or their status (ADR-0188).
//!
//! When every row is allowed and SQL holds the filter exactly, the store
//! groups and counts the rows in one statement. Otherwise the records are
//! walked as a list read walks them, a page at a time with each row
//! authorized, and counted in memory: what the walk keeps is one count per
//! distinct value and one page of records, and what bounds it is time, as a
//! count under per-row authorization (ADR-0188). A row the reader may not
//! read is never counted.

mod groups;

use std::collections::BTreeMap;

use temper_odata::query::GroupCount;
use temper_odata::query::types::{OrderDirection, QueryOptions};
use temper_runtime::persistence::{CatalogFacetQuery, FacetKey};

use self::groups::{Group, GroupKey};
use super::pagination::read_entity_set_from_query_plane;
use super::scan::{
    RowAuthz, decide_row_authz, declared_scalar, field_order_class, fields_typed,
    filter_only_query_options, native_candidate_page_plan_after,
};
use super::types::{QueryPlaneReadError, QueryPlaneReadRequest};
use crate::odata::authz::{LIST_ACTION, authorize_read};
use crate::query_eval::resolve_property;
use crate::storage::QueryFieldIndexPageQuery;

const SHAPE: &str = "$apply is read with $filter, $orderby, $top and $skiptoken only";
const ORDER: &str =
    "a facet read is ordered by `<property> asc` or by `<count> desc, <property> asc`";
const PROPERTY: &str = "a facet groups by Status or a declared scalar property";

/// One page of groups.
pub(in crate::odata) struct FacetPage {
    /// Each group as `{"<property>": value, "<alias>": count}`, in order.
    pub(in crate::odata) groups: Vec<serde_json::Value>,
    /// The cursor of the next page, when there are more groups.
    pub(in crate::odata) next_skiptoken: Option<String>,
    /// Whether the store grouped the rows, rather than a walk of them.
    pub(in crate::odata) grouped_in_sql: bool,
}

/// One page of the groups `group` asks for, over the records `request`'s
/// filter selects and its reader may read.
///
/// # Errors
///
/// [`QueryPlaneReadError::Unsupported`] for a shape this read does not
/// answer, [`QueryPlaneReadError::InvalidContinuation`] for a cursor that is
/// not one of this facet's, and the list read's errors.
pub(in crate::odata) async fn read_facets(
    request: QueryPlaneReadRequest<'_>,
    group: &GroupCount,
) -> Result<FacetPage, QueryPlaneReadError> {
    let by_count = facet_order(request.query_options, group)?;
    let property = group.property.as_str();
    if property != "Status" && declared_scalar(&request, property).is_none() {
        return Err(QueryPlaneReadError::Unsupported(PROPERTY));
    }
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
    let after = match &request.query_options.skiptoken {
        Some(token) => Some(
            Group::from_cursor(token, property).ok_or(QueryPlaneReadError::InvalidContinuation)?,
        ),
        None => None,
    };
    let top = request.budget.requested_top(request.query_options);
    let filter_only = filter_only_query_options(request.query_options);
    let rows = QueryPlaneReadRequest {
        query_options: &filter_only,
        ..request
    };
    let (mut page, grouped_in_sql) =
        match sql_groups(&rows, property, by_count, after.as_ref(), top).await {
            Some(page) => (page, true),
            None => (
                walked_groups(&rows, property, by_count, after.as_ref()).await?,
                false,
            ),
        };
    let next_skiptoken = if page.len() > top {
        page.truncate(top);
        page.last().map(|last| last.cursor(property))
    } else {
        None
    };
    let groups = page
        .iter()
        .map(|group_row| {
            let mut object = serde_json::Map::new();
            object.insert(property.to_string(), group_row.key.to_json());
            object.insert(group.alias.clone(), serde_json::json!(group_row.count));
            serde_json::Value::Object(object)
        })
        .collect();
    Ok(FacetPage {
        groups,
        next_skiptoken,
        grouped_in_sql,
    })
}

/// Whether the groups are in count order, after checking that the read
/// asks for nothing a facet does not answer.
fn facet_order(options: &QueryOptions, group: &GroupCount) -> Result<bool, QueryPlaneReadError> {
    if options.expand.is_some()
        || options.select.is_some()
        || options.skip.is_some()
        || options.search.is_some()
        || options.count == Some(true)
        || group.alias == group.property
    {
        return Err(QueryPlaneReadError::Unsupported(SHAPE));
    }
    let clauses: Vec<(&str, OrderDirection)> = options
        .orderby
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|clause| (clause.property.as_str(), clause.direction))
        .collect();
    let (property, alias) = (group.property.as_str(), group.alias.as_str());
    match clauses.as_slice() {
        [] => Ok(false),
        [(by, OrderDirection::Asc)] if *by == property => Ok(false),
        [(by, OrderDirection::Desc)] if *by == alias => Ok(true),
        [(by, OrderDirection::Desc), (then, OrderDirection::Asc)]
            if *by == alias && *then == property =>
        {
            Ok(true)
        }
        _ => Err(QueryPlaneReadError::Unsupported(ORDER)),
    }
}

/// The page of groups, and one more when there is one, from one SQL
/// statement: when every row is allowed, the store holds the filter exactly
/// and the key is one SQL compares as the evaluator does. `None` otherwise.
async fn sql_groups(
    rows: &QueryPlaneReadRequest<'_>,
    property: &str,
    by_count: bool,
    after: Option<&Group>,
    top: usize,
) -> Option<Vec<Group>> {
    let store = rows.state.query_plane_store()?;
    if !matches!(decide_row_authz(rows), RowAuthz::AllowAll) {
        return None;
    }
    // An equality read answers from its own reconciled path, which a
    // projection that lags a write can make differ from the catalog's rows.
    if let Some(filter) = &rows.query_options.filter
        && crate::odata::filter_sql::equality_field_predicates(filter).is_some()
    {
        return None;
    }
    let key = sql_key(rows, property)?;
    let plan = native_candidate_page_plan_after(rows, None)?;
    if !plan.filter_exact
        || !plan.filter_missing_fields.is_empty()
        || !(plan.filter_pushdown || plan.unfiltered)
    {
        return None;
    }
    if let FacetKey::Field { name, class } = &key
        && !fields_typed(rows, &plan, &[(name.clone(), *class)]).await?
    {
        return None;
    }
    let after = match after {
        Some(group) => Some(group.facet_after(key.class()).ok()?),
        None => None,
    };
    let query = CatalogFacetQuery {
        filter: QueryFieldIndexPageQuery {
            where_clause: plan.where_clause,
            params: plan.params,
            event_time_bounds: plan.event_time_bounds,
            field_comparisons: plan.field_comparisons,
            text_matches: plan.text_matches,
            ..QueryFieldIndexPageQuery::default()
        },
        key,
        by_count,
        after,
        top: top.saturating_add(1),
    };
    let found = store
        .query_catalog_facets(rows.tenant.as_str(), rows.entity_type, &query)
        .await
        .inspect_err(|error| {
            tracing::warn!(
                error = %error,
                tenant = %rows.tenant,
                entity_type = %rows.entity_type,
                "facet groups in SQL failed; walking the records"
            );
        })
        .ok()??;
    Some(
        found
            .into_iter()
            .map(|(value, count)| Group {
                key: GroupKey::of(Some(&value)),
                count,
            })
            .collect(),
    )
}

/// The key SQL groups by: the status, or an app field that is a declared
/// non-nullable number or string the projection holds, without a declared
/// default (a record without the key reads as the default, which its
/// catalog row does not hold).
fn sql_key(rows: &QueryPlaneReadRequest<'_>, property: &str) -> Option<FacetKey> {
    if property == "Status" {
        return Some(FacetKey::Status);
    }
    let class = field_order_class(rows, property)?;
    let defaults = crate::odata::read_defaults::ReadDefaults::for_entity(
        rows.state,
        rows.tenant,
        rows.entity_type,
    )
    .field_values(&serde_json::Value::Null);
    defaults.get(property).is_none().then(|| FacetKey::Field {
        name: property.to_string(),
        class,
    })
}

/// Every group, after `after`, in order: the records walked as a list read
/// walks them and counted by their value.
async fn walked_groups(
    rows: &QueryPlaneReadRequest<'_>,
    property: &str,
    by_count: bool,
    after: Option<&Group>,
) -> Result<Vec<Group>, QueryPlaneReadError> {
    let mut counts: BTreeMap<GroupKey, usize> = BTreeMap::new();
    let mut token: Option<String> = None;
    loop {
        let options = QueryOptions {
            filter: rows.query_options.filter.clone(),
            top: Some(rows.budget.max_entities),
            skiptoken: token.take(),
            ..QueryOptions::default()
        };
        let page = read_entity_set_from_query_plane(QueryPlaneReadRequest {
            query_options: &options,
            ..*rows
        })
        .await?;
        for entity in &page.entities {
            let key = GroupKey::of(resolve_property(entity, property).as_ref());
            *counts.entry(key).or_insert(0) += 1;
        }
        match page.next_skiptoken {
            Some(next) => token = Some(next),
            None => break,
        }
        // Let the connection see a client that left (`scan::walk_yield`).
        super::scan::walk_yield().await;
    }
    let mut groups: Vec<Group> = counts
        .into_iter()
        .map(|(key, count)| Group { key, count })
        .filter(|group| after.is_none_or(|after| group.order(after, by_count).is_gt()))
        .collect();
    groups.sort_by(|a, b| a.order(b, by_count));
    Ok(groups)
}
