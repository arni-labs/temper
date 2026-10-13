//! Spec rows and their commit protocol.
//!
//! A changed spec is written uncommitted and committed once its install
//! verifies. While a committed version is being replaced it is kept in
//! `spec_committed_versions`, so startup recovery puts it back instead of
//! losing the type when the replacement never commits.

use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Upsert a spec source with content-hash gating: an unchanged spec keeps
    /// its row as it is; a changed one is written uncommitted and pending.
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        crate::dbm::postgres_query!(&self.schema,
            "INSERT INTO {schema}spec_committed_versions \
             (tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
              verification_status, levels_passed, levels_total, verification_result, updated_at) \
             SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
                    verification_status, levels_passed, levels_total, verification_result, updated_at \
             FROM {schema}specs \
             WHERE tenant = $1 AND entity_type = $2 AND committed = true \
               AND (content_hash IS DISTINCT FROM $3 OR csdl_xml IS DISTINCT FROM $4) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, csdl_xml = EXCLUDED.csdl_xml, \
                 content_hash = EXCLUDED.content_hash, version = EXCLUDED.version, \
                 verified = EXCLUDED.verified, verification_status = EXCLUDED.verification_status, \
                 levels_passed = EXCLUDED.levels_passed, levels_total = EXCLUDED.levels_total, \
                 verification_result = EXCLUDED.verification_result, updated_at = EXCLUDED.updated_at",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(content_hash)
        .bind(csdl_xml)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!(&self.schema,
            "INSERT INTO {schema}specs \
             (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, $5, false, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.ioa_source ELSE specs.ioa_source END, \
                 csdl_xml = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.csdl_xml ELSE specs.csdl_xml END, \
                 content_hash = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.content_hash ELSE specs.content_hash END, \
                 committed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.committed END, \
                 version = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN specs.version + 1 ELSE specs.version END, \
                 verified = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.verified END, \
                 verification_status = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN 'pending' ELSE specs.verification_status END, \
                 levels_passed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_passed END, \
                 levels_total = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_total END, \
                 verification_result = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.verification_result END, \
                 updated_at = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN now() ELSE specs.updated_at END",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(csdl_xml)
        .bind(content_hash)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }

    /// Delete a spec and any committed version saved for it.
    pub async fn delete_spec(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        for sql in [
            "DELETE FROM {schema}specs WHERE tenant = $1 AND entity_type = $2",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1 AND entity_type = $2",
        ] {
            crate::dbm::postgres_query!(&self.schema, sql)
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    /// Commit every spec row of a tenant.
    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        for sql in [
            "UPDATE {schema}specs SET committed = true, updated_at = now() WHERE tenant = $1",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1",
        ] {
            crate::dbm::postgres_query!(&self.schema, sql)
                .bind(tenant)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    /// Commit exactly these spec versions, `(entity_type, content_hash)`, in
    /// one transaction, and return how many rows became committed.
    ///
    /// A row whose content changed after it was verified stays as it is, so
    /// another install of the same type is committed only by its own success.
    pub async fn commit_spec_versions(
        &self,
        tenant: &str,
        versions: &[(&str, &str)],
    ) -> Result<usize, PersistenceError> {
        if versions.is_empty() {
            return Ok(0);
        }
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        let mut committed = 0usize;
        for (entity_type, content_hash) in versions {
            let result = crate::dbm::postgres_query!(
                &self.schema,
                "UPDATE {schema}specs SET committed = true, updated_at = now() \
                 WHERE tenant = $1 AND entity_type = $2 AND content_hash = $3 AND committed = false",
            )
            .bind(tenant)
            .bind(*entity_type)
            .bind(*content_hash)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
            if result.rows_affected() > 0 {
                crate::dbm::postgres_query!(
                    &self.schema,
                    "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1 AND entity_type = $2",
                )
                .bind(tenant)
                .bind(*entity_type)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
            }
            committed += result.rows_affected() as usize;
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(committed)
    }

    /// Discard every uncommitted spec version (startup recovery), and return
    /// how many were discarded. A type that had a committed version gets it
    /// back; a type that never committed is removed.
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        let restored = crate::dbm::postgres_query!(&self.schema,
            "UPDATE {schema}specs AS s SET \
                 ioa_source = b.ioa_source, csdl_xml = b.csdl_xml, content_hash = b.content_hash, \
                 version = b.version, verified = b.verified, verification_status = b.verification_status, \
                 levels_passed = b.levels_passed, levels_total = b.levels_total, \
                 verification_result = b.verification_result, updated_at = b.updated_at, committed = true \
             FROM {schema}spec_committed_versions AS b \
             WHERE s.tenant = b.tenant AND s.entity_type = b.entity_type AND s.committed = false",
        )
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let deleted = crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}specs WHERE committed = false"
        )
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!(&self.schema, "DELETE FROM {schema}spec_committed_versions")
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok((restored.rows_affected() + deleted.rows_affected()) as usize)
    }
}
