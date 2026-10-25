//! Removing a deleted tenant's own database, so an id created again starts empty.

use std::path::{Path, PathBuf};

use temper_runtime::persistence::PersistenceError;
use tracing::info;

use temper_runtime::tenant::TenantId;

use super::TenantStoreRouter;

/// Files SQLite and Turso keep beside a local database: the database itself, its write-ahead log and its shared
/// memory index.
const LOCAL_DATABASE_FILE_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

impl TenantStoreRouter {
    /// Remove the database a tenant's registry row names. Removing one that is already gone succeeds, so a delete
    /// that is retried converges. A local database file the platform or another registered tenant also opens is
    /// never removed with this tenant.
    pub(super) async fn drop_tenant_database(
        &self,
        tenant: &TenantId,
    ) -> Result<(), PersistenceError> {
        let tenant_id = tenant.as_str();
        let registry = self.load_tenant_registry().await?;
        let Some(entry) = registry.iter().find(|row| row.tenant_id == tenant_id) else {
            return Ok(());
        };
        if entry.turso_db_url.starts_with("libsql://") {
            return self.drop_cloud_database(tenant_id).await;
        }

        let path = local_database_path(&entry.turso_db_url);
        let shared_with = std::iter::once(("the platform", self.platform_url.as_str()))
            .chain(
                registry
                    .iter()
                    .filter(|row| row.tenant_id != tenant_id)
                    .map(|row| (row.tenant_id.as_str(), row.turso_db_url.as_str())),
            )
            .find(|(_, url)| {
                !url.starts_with("libsql://") && same_file(&local_database_path(url), &path)
            });
        if let Some((owner, _)) = shared_with {
            return Err(PersistenceError::Storage(format!(
                "tenant '{tenant_id}' database {} is also used by {owner}; it was not removed",
                path.display()
            )));
        }

        for suffix in LOCAL_DATABASE_FILE_SUFFIXES {
            let mut file = path.clone().into_os_string();
            file.push(suffix);
            match tokio::fs::remove_file(&file).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(PersistenceError::Storage(format!(
                        "failed to remove tenant '{tenant_id}' database file {}: {error}",
                        PathBuf::from(file).display()
                    )));
                }
            }
        }
        info!(tenant_id, path = %path.display(), "Removed local tenant database");
        Ok(())
    }

    /// Delete the tenant's Turso Cloud database, `temper-{tenant_id}`, the name provisioning gives it.
    #[cfg(feature = "cloud")]
    async fn drop_cloud_database(&self, tenant_id: &str) -> Result<(), PersistenceError> {
        let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) else {
            return Err(cloud_credentials_missing(tenant_id));
        };
        let db_name = format!("temper-{tenant_id}");
        let response = reqwest::Client::new()
            .delete(format!(
                "{}/v1/organizations/{org}/databases/{db_name}",
                self.turso_api_base_url
            ))
            .bearer_auth(api_token)
            .send()
            .await
            .map_err(|e| {
                PersistenceError::Storage(format!("Turso API delete request failed: {e}"))
            })?;
        let status = response.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(PersistenceError::Storage(format!(
                "Turso API database delete returned {status}: {body}"
            )));
        }
        info!(tenant_id, db_name, "Removed Turso Cloud tenant database");
        Ok(())
    }

    #[cfg(not(feature = "cloud"))]
    async fn drop_cloud_database(&self, tenant_id: &str) -> Result<(), PersistenceError> {
        Err(cloud_credentials_missing(tenant_id))
    }
}

fn cloud_credentials_missing(tenant_id: &str) -> PersistenceError {
    PersistenceError::Storage(format!(
        "tenant '{tenant_id}' database is in Turso Cloud and this server has no Turso Cloud API credentials to remove it"
    ))
}

fn local_database_path(url: &str) -> PathBuf {
    PathBuf::from(url.strip_prefix("file:").unwrap_or(url))
}

/// Whether two paths name one file, following `..` and links when both exist.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::persistence::{EventMetadata, EventStore as _, PersistenceEnvelope};
    use temper_runtime::tenant::TenantId;

    use super::super::TenantStoreRouter;
    use crate::driver::params;

    async fn router(dir: &std::path::Path) -> TenantStoreRouter {
        TenantStoreRouter::new(
            &format!("file:{}", dir.join("platform.db").display()),
            None,
            Some(dir.join("tenants").to_string_lossy().to_string()),
        )
        .await
        .expect("router")
    }

    fn event() -> Vec<PersistenceEnvelope> {
        vec![PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "Issued".to_string(),
            payload: serde_json::json!({"status": "Active"}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::nil(),
                causation_id: uuid::Uuid::nil(),
                correlation_id: uuid::Uuid::nil(),
                timestamp: chrono::Utc::now(),
                actor_id: "test".to_string(),
            },
        }]
    }

    #[tokio::test]
    async fn a_tenant_created_again_after_delete_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let acme = TenantId::new("acme");
        let first = router(dir.path()).await;
        first.register_tenant(&acme).await.expect("create");
        first
            .append("acme:AgentCredential:key-1", 0, &event())
            .await
            .expect("append");

        assert!(first.remove_tenant(&acme).await.expect("delete"));
        assert!(!dir.path().join("tenants").join("acme.db").exists());
        // A retried delete converges.
        assert!(!first.remove_tenant(&acme).await.expect("delete again"));
        drop(first);

        // A new router on the same files, as after a restart, then the same id again.
        let second = router(dir.path()).await;
        second.register_tenant(&acme).await.expect("create again");
        let events = second
            .read_events("acme:AgentCredential:key-1", 0)
            .await
            .expect("read");
        assert!(events.is_empty(), "the old tenant's journal came back");
    }

    #[tokio::test]
    async fn a_database_the_platform_uses_is_never_removed_with_a_tenant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = router(dir.path()).await;
        std::fs::create_dir_all(dir.path().join("tenants")).expect("tenants dir");
        let aliased_url = format!("file:{}/tenants/../platform.db", dir.path().display());
        let conn = router.platform_store().connection().await.expect("conn");
        conn.execute(
            "INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token)
             VALUES (?1, ?2, ?3)",
            params!["aliased", aliased_url, Option::<String>::None],
        )
        .await
        .expect("registry row");

        let error = router
            .remove_tenant(&TenantId::new("aliased"))
            .await
            .expect_err("the platform database stays");
        assert!(error.to_string().contains("also used by"), "{error}");
        assert!(dir.path().join("platform.db").exists());
        assert_eq!(router.list_tenants().await.expect("list"), vec!["aliased"]);
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn deleting_a_cloud_tenant_deletes_its_cloud_database() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/organizations/acme-org/databases/temper-alpha"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut router = router(dir.path()).await.with_cloud_config(
            "api-token".to_string(),
            "acme-org".to_string(),
            None,
        );
        router.turso_api_base_url = server.uri();
        let conn = router.platform_store().connection().await.expect("conn");
        conn.execute(
            "INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token)
             VALUES (?1, ?2, ?3)",
            params!["alpha", "libsql://alpha.db.turso.io", Some("token")],
        )
        .await
        .expect("registry row");

        assert!(
            router
                .remove_tenant(&TenantId::new("alpha"))
                .await
                .expect("delete")
        );
        server.verify().await;
    }
    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn cloud_deletion_retries_after_a_later_platform_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let cloud_path = "/v1/organizations/acme-org/databases/temper-alpha";
        Mock::given(method("DELETE"))
            .and(path(cloud_path))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut router =
            router(dir.path())
                .await
                .with_cloud_config("api-token".into(), "acme-org".into(), None);
        router.turso_api_base_url = server.uri();
        let conn = router.platform_store().connection().await.unwrap();
        conn.execute("INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token) VALUES (?1, ?2, ?3)",
            params!["alpha", "libsql://alpha.db.turso.io", Some("token")]).await.unwrap();
        conn.execute("DROP TABLE tenant_users", ()).await.unwrap();
        let tenant = TenantId::new("alpha");
        assert!(
            router.remove_tenant(&tenant).await.is_err(),
            "platform failure after successful cloud delete"
        );
        server.verify().await;
        server.reset().await;
        Mock::given(method("DELETE"))
            .and(path(cloud_path))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        conn.execute(
            "CREATE TABLE tenant_users (tenant_id TEXT, user_id TEXT, role TEXT)",
            (),
        )
        .await
        .unwrap();
        assert!(router.remove_tenant(&tenant).await.unwrap());
        assert!(router.load_tenant_registry().await.unwrap().is_empty());
        server.verify().await;
    }
}
