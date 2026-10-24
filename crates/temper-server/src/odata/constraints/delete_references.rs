//! Finding the rows that reference an entity being deleted: the rows of a
//! relation's referencing type whose foreign-key field holds the entity's id.
//!
//! The query projection's field index answers that the way it answers
//! `$filter=<field> eq '<id>'`: one indexed probe, with no referencing entity
//! read from its actor. A type with thousands of rows costs the same as one
//! with none.
//!
//! The index is written after the journal (ADR-0148), so it can only vouch for
//! rows it has projected. Live rows it has no projection for yet are read from
//! their actors, which covers projection lag. When the index cannot answer at
//! all (no query plane, a field declared `query_indexed = false`, an id too
//! long to index, a failed probe), every live row of the type is in that set.
//! The set is read once per type per delete, and only while it fits
//! [`UNINDEXED_REFERENCE_READ_BUDGET`]; past it the lookup reports that it
//! could not check the rows instead of reading the whole type.

use std::collections::{BTreeMap, BTreeSet};

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::tenant::TenantId;

use crate::registry::RelationEdge;
use crate::state::ServerState;

/// Most referencing rows one delete reads from their actors, per type.
///
/// Reading a row the index has not projected can restore its actor from
/// storage, and production restores about 40 actors a second, so this keeps a
/// delete to a few seconds of reads. Projection lag leaves a handful of rows
/// unprojected; a larger set means the type has no usable projection, and
/// reading all of it is the request that ran for minutes before this lookup.
const UNINDEXED_REFERENCE_READ_BUDGET: usize = 100;

/// A clause every projected row passes: lists the type's projection catalog.
const EVERY_PROJECTED_ROW: &str = "entity_id IS NOT NULL";

/// Which live rows of a type the field index cannot vouch for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unvouched {
    /// The index answered: only rows with no projection yet.
    Unprojected,
    /// The index could not answer: every live row.
    Every,
}

/// Rows read from their actors because the index could not vouch for them.
type ReadRows = Vec<(String, serde_json::Value)>;

/// Answers "which rows reference this id" for every relation one delete
/// looks at, reading each type's unvouched rows at most once.
pub(super) struct ReferenceFinder<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    read_rows: BTreeMap<(String, Unvouched), Result<ReadRows, String>>,
}

impl<'a> ReferenceFinder<'a> {
    pub(super) fn new(state: &'a ServerState, tenant: &'a TenantId) -> Self {
        Self {
            state,
            tenant,
            read_rows: BTreeMap::new(),
        }
    }

    /// Ids of `edge.from_entity` rows whose `edge.source_field` holds
    /// `entity_id`, leaving out the ids in `skip`, at most `limit` of them.
    /// `Err` says which rows could not be checked and why.
    pub(super) async fn referencing_ids(
        &mut self,
        edge: &RelationEdge,
        entity_id: &str,
        skip: &BTreeSet<String>,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        debug_assert!(!edge.from_entity.is_empty());
        debug_assert!(!edge.source_field.is_empty());
        // Skipped ids can fill the first page; ask for enough to see past them.
        let top = limit.saturating_add(skip.len());
        let (mut found, unvouched) = match self.probe_field_index(edge, entity_id, top).await {
            Some(ids) => (ids, Unvouched::Unprojected),
            None => (Vec::new(), Unvouched::Every),
        };
        found.retain(|id| !skip.contains(id));
        if found.len() >= limit {
            found.truncate(limit);
            return Ok(found);
        }

        let rows = self.unvouched_rows(&edge.from_entity, unvouched).await?;
        for (source_id, fields) in rows {
            if found.len() >= limit {
                break;
            }
            if holds_id(fields, &edge.source_field, entity_id)
                && !skip.contains(source_id)
                && !found.contains(source_id)
            {
                found.push(source_id.clone());
            }
        }
        Ok(found)
    }

    /// Ask the field index for the first `top` projected rows whose
    /// `source_field` is `entity_id`, using the same predicate an OData
    /// `$filter` equality pushes down. `None` when the index cannot answer.
    async fn probe_field_index(
        &self,
        edge: &RelationEdge,
        entity_id: &str,
        top: usize,
    ) -> Option<Vec<String>> {
        let query_plane = self.state.query_plane_store()?;
        if !self.state.query_projection_indexes_field(
            self.tenant,
            &edge.from_entity,
            &edge.source_field,
        ) {
            return None;
        }
        let equality = FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property(edge.source_field.clone())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                entity_id.to_string(),
            ))),
        };
        // `None` when the id is longer than the index stores a value.
        let predicate = crate::odata::filter_sql::try_translate_candidate_filter(&equality)?;
        match query_plane
            .query_field_index_page(
                self.tenant.as_str(),
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
                Some(page.entity_ids)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    tenant = %self.tenant, from_entity = %edge.from_entity, error = %error,
                    "delete reference probe failed; checking unindexed rows instead"
                );
                None
            }
        }
    }

    /// The rows of `entity_type` the index cannot vouch for, read from their
    /// actors once per delete while they fit the budget.
    async fn unvouched_rows(
        &mut self,
        entity_type: &str,
        unvouched: Unvouched,
    ) -> Result<&ReadRows, String> {
        let key = (entity_type.to_string(), unvouched);
        if !self.read_rows.contains_key(&key) {
            let ids = match unvouched {
                Unvouched::Unprojected => self.unprojected_live_ids(entity_type).await,
                Unvouched::Every => {
                    self.state
                        .list_entity_ids_lazy(self.tenant, entity_type)
                        .await
                }
            };
            let rows = self.read_within_budget(entity_type, &ids).await;
            self.read_rows.insert(key.clone(), rows);
        }
        match &self.read_rows[&key] {
            Ok(rows) => Ok(rows),
            Err(reason) => Err(reason.clone()),
        }
    }

    /// Live ids of `entity_type` that have no row in the projection catalog.
    ///
    /// A projected row's field index rows are written in the same transaction
    /// as its catalog row, so a projected row the probe did not return does not
    /// hold the id. Only the ids returned here are unknown to the index.
    async fn unprojected_live_ids(&self, entity_type: &str) -> Vec<String> {
        let live_ids = self
            .state
            .list_entity_ids_lazy(self.tenant, entity_type)
            .await;
        if live_ids.is_empty() {
            return live_ids;
        }
        let Some(query_plane) = self.state.query_plane_store() else {
            return live_ids;
        };
        // The catalog lists the type's projected rows: about as many ids as the
        // live listing above already holds in memory.
        let catalog = query_plane
            .query_field_index_page(
                self.tenant.as_str(),
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
                    tenant = %self.tenant, entity_type, error = %error,
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
    async fn read_within_budget(
        &self,
        entity_type: &str,
        ids: &[String],
    ) -> Result<ReadRows, String> {
        tracing::debug!(
            entity_type,
            unindexed = ids.len(),
            "delete reference rows to read"
        );
        if ids.len() > UNINDEXED_REFERENCE_READ_BUDGET {
            return Err(format!(
                "{} {} rows are not in the query index, more than the {} a delete reads \
                 one by one; retry once the index has caught up",
                ids.len(),
                entity_type,
                UNINDEXED_REFERENCE_READ_BUDGET
            ));
        }
        debug_assert!(ids.len() <= UNINDEXED_REFERENCE_READ_BUDGET);

        let mut rows = Vec::with_capacity(ids.len());
        for source_id in ids {
            match self
                .state
                .get_tenant_entity_state(self.tenant, entity_type, source_id)
                .await
            {
                Ok(response) => rows.push((source_id.clone(), response.state.fields)),
                Err(error) => {
                    return Err(format!(
                        "{entity_type}('{source_id}') is not in the query index and could not \
                         be read: {error}"
                    ));
                }
            }
        }
        Ok(rows)
    }
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
