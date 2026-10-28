//! A query store the server writes after each commit, as one kept apart from
//! the journal is, whose row writes can be refused, as a store that is down.
//! Reads answer from what it holds.
//!
//! When its writes never land, a batch still answers as committed, and each
//! row it answered without is logged with its record and sequence and counted.
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_runtime::persistence::{
    PersistenceError, ProjectionCheckpoint, ProjectionRepairCandidate,
};

use super::visible_test::{Rows, lagging_state, ref_id, sha};
use super::*;
use crate::storage::{
    CatalogGroup, CatalogRowFreshness, EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow, QueryProjectionUpsert,
};

/// `inner`, except that its row writes are refused while refusals are left.
pub(super) struct RowsRefused {
    inner: Arc<dyn QueryPlaneStore>,
    /// How many more row writes to refuse; [`u32::MAX`] refuses every one.
    refusals: AtomicU32,
}

impl RowsRefused {
    pub(super) fn new(inner: Arc<dyn QueryPlaneStore>) -> Self {
        Self {
            inner,
            refusals: AtomicU32::new(0),
        }
    }

    /// Refuse the next `count` row writes; [`u32::MAX`] refuses them all.
    pub(super) fn refuse(&self, count: u32) {
        self.refusals.store(count, Ordering::SeqCst);
    }

    fn refused(&self) -> Result<(), PersistenceError> {
        let refused = self
            .refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| match left {
                0 => None,
                u32::MAX => Some(u32::MAX),
                left => Some(left - 1),
            })
            .is_ok();
        if refused {
            return Err(PersistenceError::Storage(
                "injected query row write failure".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl QueryPlaneStore for RowsRefused {
    async fn lagging_projections(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<ProjectionRepairCandidate>>, PersistenceError> {
        self.inner.lagging_projections(tenant).await
    }

    async fn projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<ProjectionCheckpoint>, PersistenceError> {
        self.inner.projection_checkpoints(tenant).await
    }

    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.refused()?;
        self.inner
            .upsert_projection(
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                state,
                sequence_nr,
            )
            .await
    }

    async fn upsert_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        self.refused()?;
        self.inner.upsert_projections(tenant, projections).await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner
            .remove_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        self.inner
            .query_field_index(tenant, entity_type, where_clause, params)
            .await
    }

    async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order_by: &[QueryFieldIndexOrder],
        skip: usize,
        top: usize,
        include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        self.inner
            .query_field_index_page(
                tenant,
                entity_type,
                where_clause,
                params,
                order_by,
                skip,
                top,
                include_count,
            )
            .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        self.inner
            .load_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        self.inner
            .load_entity_catalog_rows(tenant, entity_type, entity_ids)
            .await
    }

    async fn load_selected_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        self.inner
            .load_selected_entity_catalog_rows(tenant, entity_type, entity_ids, selected_fields)
            .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        self.inner.projected_entity_counts_by_tenant().await
    }

    async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        self.inner
            .projection_catalog_freshness(tenant, entity_type)
            .await
    }

    async fn catalog_groups(
        &self,
        tenant: &str,
        entity_type: &str,
        fields: &[String],
        statuses: &[String],
    ) -> Result<Option<Vec<CatalogGroup>>, PersistenceError> {
        self.inner
            .catalog_groups(tenant, entity_type, fields, statuses)
            .await
    }
}

/// Every line logged on this thread while the guard lives.
#[derive(Clone, Default)]
struct LogLines(Arc<Mutex<Vec<u8>>>);

impl Write for LogLines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogLines {
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log lock").clone()).expect("log is UTF-8")
    }
}

/// `temper_query_projection_update_error_total` with source
/// `batch_unprojected`, by record type.
fn unprojected_counts(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> BTreeMap<String, u64> {
    let value_of = |attributes: &[KeyValue], key: &str| {
        attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
            .unwrap_or_default()
    };
    provider.force_flush().expect("flush metrics");
    let mut counted = BTreeMap::new();
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope
                .metrics
                .iter()
                .filter(|m| m.name == "temper_query_projection_update_error_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    if value_of(&point.attributes, "source") == "batch_unprojected" {
                        counted.insert(value_of(&point.attributes, "entity_type"), point.value);
                    }
                }
            }
        }
    }
    counted
}

/// The query store refuses every row write after the refs are made. A push's
/// batch moves two of them: it answers as committed, the records hold the
/// new heads, and each of the two rows is logged with its record and
/// sequence and counted once.
#[tokio::test]
async fn a_batch_whose_rows_never_land_answers_and_counts_each_row() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    let log = LogLines::default();
    let _log = log.capture();

    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(0);
    let (state, refused, db_path) = lagging_state(0, Rows::AfterCommit).await;
    let refused = refused.expect("rows written after commit");
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("batch-unprojected");
    for n in 0..2 {
        let params = json!({
            "RepositoryId": "repo-test",
            "Name": format!("refs/heads/b{n}"),
            "TargetCommitSha": sha(0, n),
            "Kind": "branch"
        });
        let created = state
            .dispatch_tenant_action(&tenant, "Ref", &ref_id(n), "Create", params, &agent)
            .await
            .expect("ref create should run");
        assert!(created.success);
    }

    refused.refuse(u32::MAX);
    let sub_writes: Vec<Value> = (0..2)
        .map(|n| {
            json!({
                "entity_type": "Ref",
                "entity_id": ref_id(n),
                "action": "Update",
                "params": {
                    "PreviousCommitSha": sha(0, n),
                    "NewCommitSha": sha(1, n),
                    "TargetCommitSha": sha(1, n)
                }
            })
        })
        .collect();
    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({ "sub_writes": sub_writes }),
            &agent,
        )
        .await;
    assert!(
        matches!(applied, Ok(true)),
        "a committed batch answers as committed even when its rows never land: {applied:?}"
    );

    let mut sequences = BTreeMap::new();
    for n in 0..2 {
        let record = state
            .get_tenant_entity_state(&tenant, "Ref", &ref_id(n))
            .await
            .expect("the ref reads back");
        assert_eq!(
            record.state.fields["TargetCommitSha"],
            json!(sha(1, n)),
            "the batch committed"
        );
        sequences.insert(ref_id(n), record.state.sequence_nr);
    }

    let text = log.text();
    for (id, sequence_nr) in &sequences {
        assert!(
            text.lines()
                .any(|line| line.contains(&format!("entity_id={id}"))
                    && line.contains(&format!("sequence_nr={sequence_nr}"))
                    && line.contains("answered before its query row was written")),
            "each row the batch answered without is logged with its record and sequence; log:\n{text}"
        );
    }
    assert_eq!(
        unprojected_counts(&provider, &exporter),
        BTreeMap::from([("Ref".to_string(), 2)]),
        "each row the batch answered without is counted once"
    );
    let _ = std::fs::remove_file(db_path);
}
