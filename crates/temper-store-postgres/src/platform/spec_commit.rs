//! Committing verified spec versions.

use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

impl PostgresEventStore {
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
            committed += result.rows_affected() as usize;
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(committed)
    }
}
