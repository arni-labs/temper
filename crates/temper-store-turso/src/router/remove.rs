//! Where a tenant's database is, and its removal.

use std::path::{Path, PathBuf};

use temper_runtime::persistence::PersistenceError;
use tracing::info;

use super::TenantStoreRouter;

impl TenantStoreRouter {
    /// The local database file the router keeps for `tenant_id`:
    /// `{base_dir}/{tenant_id}.db`. The id must be one file name, so the
    /// path never leaves `base_dir`.
    pub(super) fn local_database_path(&self, tenant_id: &str) -> Result<PathBuf, PersistenceError> {
        if tenant_id.is_empty()
            || tenant_id == "."
            || tenant_id == ".."
            || tenant_id.contains(['/', '\\'])
            || tenant_id.chars().any(char::is_control)
        {
            return Err(PersistenceError::Storage(format!(
                "tenant id '{tenant_id}' is not one file name, so it names no local database"
            )));
        }
        let base_dir = self.local_base_dir.as_deref().unwrap_or(".temper/tenants");
        Ok(Path::new(base_dir).join(format!("{tenant_id}.db")))
    }

    /// Remove the database the registry names for `tenant_id`.
    ///
    /// A local one (`file:`) is removed with its `-wal` and `-shm` files, and
    /// only when its file is the one the router makes for this id
    /// (`{tenant_id}.db`), so no other tenant's file is ever touched. A remote
    /// one is the Turso Cloud database the router made for this id, removed
    /// through the Cloud API; without Cloud credentials it cannot be, and the
    /// removal fails with nothing removed.
    pub(super) async fn remove_database(
        &self,
        tenant_id: &str,
        db_url: &str,
    ) -> Result<(), PersistenceError> {
        if let Some(path) = db_url.strip_prefix("file:") {
            let own_file = format!("{tenant_id}.db");
            if Path::new(path).file_name() != Some(own_file.as_ref()) {
                return Err(PersistenceError::Storage(format!(
                    "tenant '{tenant_id}' is registered with database '{db_url}', which is not the router's \
                     '{own_file}'; nothing was removed"
                )));
            }
            for file in [
                path.to_string(),
                format!("{path}-wal"),
                format!("{path}-shm"),
            ] {
                match std::fs::remove_file(&file) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(PersistenceError::Storage(format!(
                            "failed to remove tenant '{tenant_id}' database file {file}: {error}"
                        )));
                    }
                }
            }
            info!(tenant_id, db_url, "Removed local tenant database");
            return Ok(());
        }
        self.remove_cloud_database(tenant_id, db_url).await
    }

    #[cfg(feature = "cloud")]
    async fn remove_cloud_database(
        &self,
        tenant_id: &str,
        db_url: &str,
    ) -> Result<(), PersistenceError> {
        let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) else {
            return Err(no_cloud_credentials(tenant_id, db_url));
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
                "Turso API database delete for tenant '{tenant_id}' returned {status}: {body}"
            )));
        }
        info!(tenant_id, db_url, "Removed Turso Cloud tenant database");
        Ok(())
    }

    #[cfg(not(feature = "cloud"))]
    async fn remove_cloud_database(
        &self,
        tenant_id: &str,
        db_url: &str,
    ) -> Result<(), PersistenceError> {
        Err(no_cloud_credentials(tenant_id, db_url))
    }
}

fn no_cloud_credentials(tenant_id: &str, db_url: &str) -> PersistenceError {
    PersistenceError::Storage(format!(
        "tenant '{tenant_id}' keeps its database at '{db_url}', and this server has no Turso Cloud \
         credentials to remove it; nothing was removed"
    ))
}

#[cfg(test)]
mod tests {
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

    use super::TenantStoreRouter;

    async fn routed(dir: &std::path::Path) -> TenantStoreRouter {
        TenantStoreRouter::new(
            &format!("file:{}", dir.join("platform.db").display()),
            None,
            Some(dir.join("tenants").to_string_lossy().into_owned()),
        )
        .await
        .expect("router")
    }

    async fn write_one(router: &TenantStoreRouter, tenant: &str) {
        let event = PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "OrderCreated".to_string(),
            payload: serde_json::json!({"status": "Draft"}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::nil(),
                causation_id: uuid::Uuid::nil(),
                correlation_id: uuid::Uuid::nil(),
                timestamp: chrono::Utc::now(),
                actor_id: "test".to_string(),
            },
        };
        router
            .append(&format!("{tenant}:Order:one"), 0, &[event])
            .await
            .expect("append");
    }

    #[tokio::test]
    async fn removing_a_tenant_removes_only_its_own_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = routed(dir.path()).await;
        for tenant in ["alpha", "beta"] {
            router.register_tenant(tenant).await.expect("register");
            write_one(&router, tenant).await;
        }
        let alpha = router.local_database_path("alpha").expect("alpha path");
        for suffix in ["-wal", "-shm"] {
            std::fs::write(format!("{}{suffix}", alpha.display()), b"").expect("sibling");
        }

        assert!(router.remove_tenant("alpha").await.expect("remove"));

        for suffix in ["", "-wal", "-shm"] {
            assert!(
                !std::path::Path::new(&format!("{}{suffix}", alpha.display())).exists(),
                "alpha.db{suffix} is removed"
            );
        }
        assert!(router.local_database_path("beta").expect("beta").exists());
        assert!(dir.path().join("platform.db").exists());
        assert_eq!(router.list_tenants().await.expect("list"), vec!["beta"]);
        assert_eq!(
            router
                .read_events("beta:Order:one", 0)
                .await
                .expect("beta read")
                .len(),
            1
        );

        router
            .register_tenant("alpha")
            .await
            .expect("register again");
        assert!(
            router
                .read_events("alpha:Order:one", 0)
                .await
                .expect("alpha read")
                .is_empty(),
            "a tenant registered again starts empty"
        );
        assert!(!router.remove_tenant("gamma").await.expect("unknown"));
    }

    #[tokio::test]
    async fn the_server_s_own_tenants_and_paths_outside_the_base_are_never_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = routed(dir.path()).await;
        router.register_tenant("default").await.expect("default");
        for kept in ["default", "temper-system"] {
            assert!(router.remove_tenant(kept).await.is_err(), "{kept}");
        }
        assert!(
            router
                .local_database_path("default")
                .expect("path")
                .exists()
        );
        assert!(dir.path().join("platform.db").exists());
        for id in ["../platform", "a/b", "..", "."] {
            assert!(router.local_database_path(id).is_err(), "{id}");
            assert!(router.register_tenant(id).await.is_err(), "{id}");
        }
    }

    #[tokio::test]
    async fn a_database_the_router_did_not_make_for_the_id_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = routed(dir.path()).await;
        let other = dir.path().join("other.db");
        std::fs::write(&other, b"not this tenant's").expect("other file");
        let url = format!("file:{}", other.display());

        assert!(router.remove_database("alpha", &url).await.is_err());
        assert!(other.exists());
        assert!(
            router
                .remove_database("alpha", "libsql://temper-alpha-acme.turso.io")
                .await
                .is_err(),
            "without Cloud credentials a remote database is not removed"
        );
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn a_cloud_database_is_removed_through_the_cloud_api() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/organizations/acme/databases/temper-alpha"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut router = routed(dir.path()).await.with_cloud_config(
            "api-token".to_string(),
            "acme".to_string(),
            None,
        );
        router.turso_api_base_url = server.uri();

        router
            .remove_database("alpha", "libsql://temper-alpha-acme.turso.io")
            .await
            .expect("removed");
        server.verify().await;
    }
}
