//! Making a tenant's database. The registry row is written first, marked
//! `provisioning`, then the database is made and the row becomes `active`, so
//! a create that stops part way is finished by creating the id again.
//!
//! A create takes over no storage it did not make: a database already at the
//! tenant's place while no registry row names it was left by an earlier
//! tenant with the id, and it refuses the create. Nothing is removed.

use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::{info, instrument};

use super::TenantStoreRouter;
use crate::TursoEventStore;
use crate::store::WritePriority;

/// The registry status of a tenant whose create began and has not ended.
/// Only a create of the same id resumes it, and only such a create takes over
/// a database already at the tenant's place: its own earlier attempt made it.
pub(super) const PROVISIONING: &str = "provisioning";

/// What provisioning a tenant's database found.
enum Provisioned {
    /// The database to connect to.
    Made {
        db_url: String,
        auth_token: Option<String>,
    },
    /// A database this create did not make is at the tenant's place.
    LeftOver(String),
}

impl TenantStoreRouter {
    /// Register and connect a new tenant.
    ///
    /// In local mode, creates a new SQLite file in `local_base_dir`.
    /// In cloud mode (with `cloud` feature), provisions via Turso Cloud API.
    /// A database left at the tenant's place by an earlier tenant with the
    /// id refuses the create and is named in the error; see
    /// [`Self::left_over_database`].
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.register_tenant"))]
    pub async fn register_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<TursoEventStore, PersistenceError> {
        let status = if self.tenants.read().await.contains_key(tenant_id) {
            Some("active".to_string())
        } else {
            self.load_tenant_registry()
                .await?
                .into_iter()
                .find(|row| row.tenant_id == tenant_id)
                .map(|row| row.status)
        };
        // In any other status the id is taken: a tenant still being removed
        // keeps its id until its database is gone.
        let resuming = match status.as_deref() {
            None => {
                if let Some(left_over) = self.left_over_database(tenant_id).await? {
                    return Err(left_over_refusal(tenant_id, &left_over));
                }
                self.platform
                    .write_connection("router.register_tenant", WritePriority::High)
                    .await?
                    .execute(
                        "INSERT INTO tenant_registry (tenant_id, turso_db_url, status)
                         VALUES (?1, '', ?2)",
                        crate::driver::params![tenant_id, PROVISIONING],
                    )
                    .await
                    .map_err(storage_error)?;
                false
            }
            Some(PROVISIONING) => true,
            Some(_) => {
                return Err(PersistenceError::Storage(format!(
                    "tenant '{tenant_id}' already exists"
                )));
            }
        };

        let (db_url, auth_token) = match self.provision_database(tenant_id, resuming).await? {
            Provisioned::Made { db_url, auth_token } => (db_url, auth_token),
            Provisioned::LeftOver(left_over) => {
                // Made by someone else since the check above, while this
                // create made nothing: its row goes, so the next create
                // checks again rather than taking it over.
                self.platform
                    .write_connection("router.register_tenant", WritePriority::High)
                    .await?
                    .execute(
                        "DELETE FROM tenant_registry WHERE tenant_id = ?1 AND status = ?2",
                        crate::driver::params![tenant_id, PROVISIONING],
                    )
                    .await
                    .map_err(storage_error)?;
                return Err(left_over_refusal(tenant_id, &left_over));
            }
        };
        let store = TursoEventStore::new(&db_url, auth_token.as_deref()).await?;
        self.platform
            .write_connection("router.register_tenant", WritePriority::High)
            .await?
            .execute(
                "UPDATE tenant_registry
                 SET turso_db_url = ?2, turso_auth_token = ?3, status = 'active'
                 WHERE tenant_id = ?1 AND status = ?4",
                crate::driver::params![tenant_id, db_url, auth_token, PROVISIONING],
            )
            .await
            .map_err(storage_error)?;
        self.tenants
            .write()
            .await
            .insert(tenant_id.to_string(), store.clone());

        info!(tenant_id, "Registered and connected new tenant");
        Ok(store)
    }

    /// A database at `tenant_id`'s place that no registry row names, left by
    /// an earlier tenant with the id: its local file (with its `-wal` or
    /// `-shm`), or in cloud mode its Turso Cloud database. `None` when there
    /// is none.
    pub async fn left_over_database(
        &self,
        tenant_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let registered = self.tenants.read().await.contains_key(tenant_id)
            || self
                .load_tenant_registry()
                .await?
                .iter()
                .any(|row| row.tenant_id == tenant_id);
        if registered {
            return Ok(None);
        }
        #[cfg(feature = "cloud")]
        if let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) {
            let db_name = format!("temper-{tenant_id}");
            let response = reqwest::Client::new()
                .get(format!(
                    "{}/v1/organizations/{org}/databases/{db_name}",
                    self.turso_api_base_url
                ))
                .bearer_auth(api_token)
                .send()
                .await
                .map_err(|e| {
                    PersistenceError::Storage(format!("Turso API lookup request failed: {e}"))
                })?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "<no body>".to_string());
                return Err(PersistenceError::Storage(format!(
                    "Turso API database lookup for tenant '{tenant_id}' returned {status}: {body}"
                )));
            }
            return Ok(Some(cloud_database(&db_name)));
        }
        self.left_over_local_database(tenant_id)
    }

    /// The tenant's local database file, or its `-wal` or `-shm`, if any is
    /// there.
    fn left_over_local_database(
        &self,
        tenant_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let path = self.local_database_path(tenant_id)?;
        Ok(["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| format!("{}{suffix}", path.display()))
            .find(|file| std::path::Path::new(file).exists())
            .map(|file| format!("database file {file}")))
    }

    /// Provision a new database for a tenant.
    ///
    /// In local mode: creates a `file:{base_dir}/{tenant_id}.db` SQLite file.
    /// In cloud mode: calls the Turso Cloud API to create a database.
    async fn provision_database(
        &self,
        tenant_id: &str,
        adopt: bool,
    ) -> Result<Provisioned, PersistenceError> {
        #[cfg(feature = "cloud")]
        if let (Some(api_token), Some(org)) = (&self.turso_api_token, &self.turso_org) {
            return self
                .provision_cloud_database(tenant_id, api_token, org, adopt)
                .await;
        }
        // Local mode: the file is made on connecting, so one already there
        // is this create's own only when it resumes.
        if !adopt && let Some(left_over) = self.left_over_local_database(tenant_id)? {
            return Ok(Provisioned::LeftOver(left_over));
        }
        let path = self.local_database_path(tenant_id)?;
        let base_dir = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(base_dir).map_err(|e| {
            PersistenceError::Storage(format!(
                "failed to create tenant directory {}: {e}",
                base_dir.display()
            ))
        })?;

        let db_url = format!("file:{}", path.display());
        info!(tenant_id, db_url, "Provisioned local tenant database");
        Ok(Provisioned::Made {
            db_url,
            auth_token: None,
        })
    }

    /// Provision a database via the Turso Cloud Platform API. One that is
    /// already there (409) is taken over only when `adopt`: this create's
    /// own earlier attempt made it.
    #[cfg(feature = "cloud")]
    async fn provision_cloud_database(
        &self,
        tenant_id: &str,
        api_token: &str,
        org: &str,
        adopt: bool,
    ) -> Result<Provisioned, PersistenceError> {
        let client = reqwest::Client::new();

        // Sanitize tenant ID for use as a database name (alphanumeric + hyphens).
        let db_name = format!("temper-{tenant_id}");

        let mut body = serde_json::json!({
            "name": db_name,
        });
        if let Some(group) = &self.turso_group {
            body["group"] = serde_json::Value::String(group.clone());
        }

        // Create the database.
        let resp = client
            .post(format!(
                "{}/v1/organizations/{org}/databases",
                self.turso_api_base_url
            ))
            .bearer_auth(api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso API request failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::CONFLICT && !adopt {
            return Ok(Provisioned::LeftOver(cloud_database(&db_name)));
        }
        let hostname = if resp.status() == reqwest::StatusCode::CONFLICT {
            // This create's earlier attempt made it: read its hostname and
            // create a fresh token.
            let lookup_resp = client
                .get(format!(
                    "{}/v1/organizations/{org}/databases/{db_name}",
                    self.turso_api_base_url
                ))
                .bearer_auth(api_token)
                .send()
                .await
                .map_err(|e| {
                    PersistenceError::Storage(format!("Turso API lookup request failed: {e}"))
                })?;

            if !lookup_resp.status().is_success() {
                let status = lookup_resp.status();
                let body_text = lookup_resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<no body>".to_string());
                return Err(PersistenceError::Storage(format!(
                    "Turso API database lookup returned {status}: {body_text}"
                )));
            }

            let lookup_json: serde_json::Value = lookup_resp.json().await.map_err(|e| {
                PersistenceError::Storage(format!("Turso API database lookup parse: {e}"))
            })?;

            lookup_json["database"]["Hostname"]
                .as_str()
                .or_else(|| lookup_json["database"]["hostname"].as_str())
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "Turso API missing hostname in lookup response: {lookup_json}"
                    ))
                })?
                .to_string()
        } else {
            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<no body>".to_string());
                return Err(PersistenceError::Storage(format!(
                    "Turso API returned {status}: {body_text}"
                )));
            }

            let create_resp: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| PersistenceError::Storage(format!("Turso API response parse: {e}")))?;

            create_resp["database"]["Hostname"]
                .as_str()
                .or_else(|| create_resp["database"]["hostname"].as_str())
                .ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "Turso API missing hostname in response: {create_resp}"
                    ))
                })?
                .to_string()
        };

        let db_url = format!("libsql://{hostname}");

        // Create an auth token for the database (new or existing).
        let token_resp = client
            .post(format!(
                "{}/v1/organizations/{org}/databases/{db_name}/auth/tokens",
                self.turso_api_base_url
            ))
            .bearer_auth(api_token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| {
                PersistenceError::Storage(format!("Turso API token request failed: {e}"))
            })?;

        if !token_resp.status().is_success() {
            let status = token_resp.status();
            let body_text = token_resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(PersistenceError::Storage(format!(
                "Turso API token creation returned {status}: {body_text}"
            )));
        }

        let token_json: serde_json::Value = token_resp
            .json()
            .await
            .map_err(|e| PersistenceError::Storage(format!("Turso token parse: {e}")))?;

        let auth_token = token_json["jwt"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "Turso API missing jwt in token response: {token_json}"
                ))
            })?;

        info!(tenant_id, db_url, "Provisioned Turso Cloud tenant database");
        Ok(Provisioned::Made {
            db_url,
            auth_token: Some(auth_token),
        })
    }
}

#[cfg(feature = "cloud")]
fn cloud_database(db_name: &str) -> String {
    format!("Turso Cloud database '{db_name}'")
}

/// The error for a create refused by storage an earlier tenant with the id
/// left behind.
fn left_over_refusal(tenant_id: &str, left_over: &str) -> PersistenceError {
    PersistenceError::Storage(format!(
        "tenant '{tenant_id}' cannot be created: an earlier tenant with this id left its {left_over}, \
         which a new tenant does not take over; nothing was removed"
    ))
}

#[cfg(test)]
#[path = "provision_test.rs"]
mod tests;
