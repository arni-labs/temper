//! Persistence bootstrap — restoring a [`SpecRegistry`] from storage backends.
//!
//! Centralizes the logic for reading persisted specs from Postgres or Turso and
//! populating a `SpecRegistry` with tenant registrations and verification status.
//! This keeps storage-specific row translation out of the CLI layer.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};
use temper_store_turso::TursoEventStore;

use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};

/// Common accessors for spec rows from different storage backends.
trait SpecRowLike {
    fn verification_status(&self) -> &str;
    fn verified(&self) -> bool;
    fn levels_passed(&self) -> Option<i32>;
    fn levels_total(&self) -> Option<i32>;
    fn updated_at_rfc3339(&self) -> String;
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult>;
}

/// Postgres-backed spec row.
#[derive(sqlx::FromRow)]
pub struct PersistedSpecRow {
    pub tenant: String,
    pub entity_type: String,
    pub ioa_source: String,
    pub csdl_xml: Option<String>,
    pub verification_status: String,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result: Option<serde_json::Value>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl SpecRowLike for PersistedSpecRow {
    fn verification_status(&self) -> &str {
        &self.verification_status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        self.levels_passed
    }
    fn levels_total(&self) -> Option<i32> {
        self.levels_total
    }
    fn updated_at_rfc3339(&self) -> String {
        self.updated_at.to_rfc3339()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
    }
}

impl SpecRowLike for temper_store_turso::TursoSpecRow {
    fn verification_status(&self) -> &str {
        &self.verification_status
    }
    fn verified(&self) -> bool {
        self.verified
    }
    fn levels_passed(&self) -> Option<i32> {
        self.levels_passed
    }
    fn levels_total(&self) -> Option<i32> {
        self.levels_total
    }
    fn updated_at_rfc3339(&self) -> String {
        self.updated_at.clone()
    }
    fn try_parse_verification_result(&self) -> Option<EntityVerificationResult> {
        self.verification_result
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
    }
}

fn row_to_registry_status(row: &impl SpecRowLike) -> VerificationStatus {
    let status = row.verification_status().to_lowercase();
    match status.as_str() {
        "pending" => VerificationStatus::Pending,
        "running" => VerificationStatus::Running,
        _ => {
            // Full verification_result JSON → Completed (authoritative).
            if let Some(result) = row.try_parse_verification_result() {
                return VerificationStatus::Completed(result);
            }

            // No full result — build a synthetic summary and mark as Restored.
            let all_passed = status == "passed" || row.verified();
            let levels_passed = row
                .levels_passed()
                .unwrap_or(if all_passed { 1 } else { 0 })
                .max(0) as usize;
            let levels_total = row.levels_total().unwrap_or(levels_passed as i32).max(0) as usize;
            let levels = if levels_total > 0 {
                (0..levels_total)
                    .map(|idx| EntityLevelSummary {
                        level: format!("L{idx}"),
                        passed: idx < levels_passed,
                        summary: if idx < levels_passed {
                            "Restored from verification summary".to_string()
                        } else {
                            "Restored failed verification level".to_string()
                        },
                        details: None,
                    })
                    .collect()
            } else {
                vec![EntityLevelSummary {
                    level: "Persisted".to_string(),
                    passed: all_passed,
                    summary: format!("Restored status '{}'", row.verification_status()),
                    details: None,
                }]
            };
            VerificationStatus::Restored(EntityVerificationResult {
                all_passed,
                levels,
                verified_at: row.updated_at_rfc3339(),
            })
        }
    }
}

/// Helper: populate registry from grouped spec rows.
fn restored_csdl_for_rows<R>(
    tenant: &str,
    rows: &[R],
    get_csdl: &impl Fn(&R) -> Option<String>,
) -> Result<Option<(CsdlDocument, String)>, String> {
    let mut seen = BTreeSet::new();
    let mut merged: Option<CsdlDocument> = None;

    for csdl_xml in rows.iter().filter_map(get_csdl) {
        let csdl_xml = csdl_xml.trim();
        if csdl_xml.is_empty() || !seen.insert(csdl_xml.to_string()) {
            continue;
        }

        let parsed = parse_csdl(csdl_xml)
            .map_err(|e| format!("Failed to parse restored CSDL for tenant '{tenant}': {e}"))?;
        merged = Some(match merged {
            Some(existing) => merge_csdl(&existing, &parsed),
            None => parsed,
        });
    }

    Ok(merged.map(|csdl| {
        let csdl_xml = emit_csdl_xml(&csdl);
        (csdl, csdl_xml)
    }))
}

fn populate_registry<R: SpecRowLike>(
    registry: &mut SpecRegistry,
    grouped: BTreeMap<String, Vec<R>>,
    constraints_by_tenant: &mut BTreeMap<String, String>,
    get_csdl: impl Fn(&R) -> Option<String>,
    get_ioa: impl Fn(&R) -> (String, String),
) -> Result<usize, String> {
    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let Some((csdl, csdl_xml)) = restored_csdl_for_rows(&tenant, &tenant_rows, &get_csdl)?
        else {
            tracing::warn!(tenant = %tenant, "skipping restored tenant due to missing CSDL");
            continue;
        };

        // A stored spec in the old predicate syntax is converted in memory; one
        // that still cannot be read disables only its own entity type, not
        // the tenant or the server.
        let ioa_owned: Vec<(String, String)> = tenant_rows
            .iter()
            .map(&get_ioa)
            .filter_map(
                |(entity_type, ioa)| match read_stored_spec(&tenant, &entity_type, &ioa) {
                    StoredSpec::Current => Some((entity_type, ioa)),
                    StoredSpec::Converted(source) => Some((entity_type, source)),
                    StoredSpec::Unreadable => None,
                },
            )
            .collect();
        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        let cross_invariants_toml = constraints_by_tenant.remove(&tenant);
        registry
            .try_register_tenant_with_constraints(
                tenant.as_str(),
                csdl,
                csdl_xml,
                &ioa_pairs,
                cross_invariants_toml,
                false,
            )
            .map_err(|e| format!("Failed to restore tenant '{tenant}' into registry: {e}"))?;
        let tenant_id = TenantId::new(&tenant);
        for row in &tenant_rows {
            let entity_type = get_ioa(row).0;
            if !ioa_owned
                .iter()
                .any(|(restored, _)| *restored == entity_type)
            {
                continue;
            }
            registry.set_verification_status(&tenant_id, &entity_type, row_to_registry_status(row));
            restored_specs += 1;
        }
    }
    Ok(restored_specs)
}

/// Restore a [`SpecRegistry`] from Postgres.
pub async fn restore_registry_from_postgres(
    registry: &mut SpecRegistry,
    pool: &sqlx::PgPool,
) -> Result<usize, String> {
    let rows: Vec<PersistedSpecRow> = sqlx::query_as(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM specs \
         ORDER BY tenant, entity_type",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to read specs from Postgres: {e}"))?;

    #[derive(sqlx::FromRow)]
    struct ConstraintRow {
        tenant: String,
        cross_invariants_toml: String,
    }

    let constraints_rows: Vec<ConstraintRow> = sqlx::query_as(
        "SELECT tenant, cross_invariants_toml \
         FROM tenant_constraints \
         ORDER BY tenant",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to read tenant constraints from Postgres: {e}"))?;

    let mut constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut grouped: BTreeMap<String, Vec<PersistedSpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    populate_registry(
        registry,
        grouped,
        &mut constraints_by_tenant,
        |rows| rows.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

/// Restore a [`SpecRegistry`] from Turso.
pub async fn restore_registry_from_turso(
    registry: &mut SpecRegistry,
    turso: &TursoEventStore,
) -> Result<usize, String> {
    // GC uncommitted specs left behind by interrupted install_os_app writes.
    match turso.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("deleted {n} uncommitted specs during startup recovery"),
        Err(e) => tracing::warn!("failed to delete uncommitted specs: {e}"),
    }
    let rows = turso
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from Turso: {e}"))?;
    let constraints_rows = turso
        .load_tenant_constraints()
        .await
        .map_err(|e| format!("Failed to read tenant constraints from Turso: {e}"))?;

    let mut constraints_by_tenant: BTreeMap<String, String> = constraints_rows
        .into_iter()
        .map(|row| (row.tenant, row.cross_invariants_toml))
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut grouped: BTreeMap<String, Vec<temper_store_turso::TursoSpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    populate_registry(
        registry,
        grouped,
        &mut constraints_by_tenant,
        |rows| rows.csdl_xml.clone(),
        |row| (row.entity_type.clone(), row.ioa_source.clone()),
    )
}

/// Restore a [`SpecRegistry`] from a [`PlatformStore`] (trait-based).
///
/// This is the production code path for restoring specs from any platform
/// store backend (Turso, Sim, etc.). Used by both the CLI bootstrap and
/// the DST harness — ensuring simulation runs identical code.
///
/// Unlike [`restore_registry_from_turso`], this does not restore verification
/// status or cross-entity constraints (the `PlatformStore` trait returns simpler
/// `SpecRow` types). Specs are registered with `Pending` verification status.
pub async fn restore_registry_from_platform_store(
    registry: &mut SpecRegistry,
    store: &dyn crate::platform_store::PlatformStore,
) -> Result<usize, String> {
    match store.delete_uncommitted_specs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("deleted {n} uncommitted specs during startup recovery"),
        Err(e) => tracing::warn!("failed to delete uncommitted specs: {e}"),
    }
    let rows = store
        .load_specs()
        .await
        .map_err(|e| format!("Failed to read specs from platform store: {e}"))?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Group by tenant.
    let mut grouped: BTreeMap<String, Vec<crate::platform_store::SpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    let mut restored_specs = 0usize;
    // Track specs that failed to register — these are orphans to reconcile.
    let mut orphaned_specs: Vec<(String, String)> = Vec::new();

    for (tenant, tenant_rows) in &grouped {
        let (csdl, csdl_xml) = match restored_csdl_for_rows(tenant, tenant_rows, &|r| {
            r.csdl_xml.clone()
        }) {
            Ok(Some(restored)) => restored,
            Ok(None) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant with missing CSDL");
                for row in tenant_rows {
                    orphaned_specs.push((row.tenant.clone(), row.entity_type.clone()));
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant with invalid CSDL: {e}");
                for row in tenant_rows {
                    orphaned_specs.push((row.tenant.clone(), row.entity_type.clone()));
                }
                continue;
            }
        };

        // Specs stored by an earlier kernel in the old predicate syntax are
        // converted here, once, and written back. One that still cannot be
        // read disables only its own entity type and stays in the store.
        let mut readable: Vec<(String, String)> = Vec::with_capacity(tenant_rows.len());
        let mut converted: Vec<(&crate::platform_store::SpecRow, String)> = Vec::new();
        for row in tenant_rows {
            match read_stored_spec(tenant, &row.entity_type, &row.ioa_source) {
                StoredSpec::Current => {
                    readable.push((row.entity_type.clone(), row.ioa_source.clone()));
                }
                StoredSpec::Converted(source) => {
                    readable.push((row.entity_type.clone(), source.clone()));
                    converted.push((row, source));
                }
                StoredSpec::Unreadable => {}
            }
        }
        let ioa_pairs: Vec<(&str, &str)> = readable
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        match registry.try_register_tenant_with_constraints(
            tenant.as_str(),
            csdl,
            csdl_xml,
            &ioa_pairs,
            None,
            false,
        ) {
            Ok(()) => {
                restored_specs += readable.len();
                persist_converted_specs(store, tenant, &converted).await;
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, "reconciling orphaned specs for tenant that failed registration: {e}");
                for (entity_type, _) in &readable {
                    orphaned_specs.push((tenant.clone(), entity_type.clone()));
                }
            }
        }
    }

    // Reconciliation: delete orphaned specs from the store so P1 holds
    // (every spec in store has a matching registry entry).
    for (tenant, entity_type) in &orphaned_specs {
        if let Err(e) = store.delete_spec(tenant, entity_type).await {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                "best-effort orphan cleanup failed: {e}"
            );
        }
    }

    Ok(restored_specs)
}

/// How a persisted spec reads under this kernel.
enum StoredSpec {
    /// Parses as stored.
    Current,
    /// Was in the old predicate syntax; this is the converted source.
    Converted(String),
    /// Neither parses nor converts.
    Unreadable,
}

fn read_stored_spec(tenant: &str, entity_type: &str, ioa_source: &str) -> StoredSpec {
    let Err(parse_error) = temper_spec::automaton::parse_automaton(ioa_source) else {
        return StoredSpec::Current;
    };
    match temper_spec::automaton::legacy::migrate_source(ioa_source) {
        Ok(migration) => {
            tracing::info!(
                tenant,
                entity_type,
                notes = migration.notes.len(),
                "converted stored spec to the current syntax"
            );
            StoredSpec::Converted(migration.source)
        }
        Err(convert_error) => {
            tracing::error!(
                tenant,
                entity_type,
                %parse_error,
                %convert_error,
                "stored spec does not parse and cannot be converted; its entity type stays unavailable until the spec is fixed"
            );
            StoredSpec::Unreadable
        }
    }
}

/// Write converted specs back so the conversion happens once. A failed write
/// only means the next boot converts the stored source again.
async fn persist_converted_specs(
    store: &dyn crate::platform_store::PlatformStore,
    tenant: &str,
    converted: &[(&crate::platform_store::SpecRow, String)],
) {
    if converted.is_empty() {
        return;
    }
    for (row, source) in converted {
        let hash = temper_store_turso::spec_content_hash(source);
        let csdl_xml = row.csdl_xml.as_deref().unwrap_or_default();
        if let Err(error) = store
            .upsert_spec(tenant, &row.entity_type, source, csdl_xml, &hash)
            .await
        {
            tracing::warn!(tenant, entity_type = %row.entity_type, %error, "failed to store converted spec");
        }
    }
    // upsert_spec marks a changed row uncommitted, and uncommitted rows are
    // deleted at the next startup.
    if let Err(error) = store.commit_specs(tenant).await {
        tracing::warn!(tenant, %error, "failed to commit converted specs");
    }
}

#[cfg(test)]
#[path = "registry_bootstrap_test.rs"]
mod tests;
