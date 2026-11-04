//! Tenant-aware store router for database-per-tenant isolation.
//!
//! [`TenantStoreRouter`] manages a platform database plus per-tenant databases.
//! The platform DB holds shared state (tenant registry, user mappings, system
//! packages). Each tenant gets an isolated database with the full entity schema.
//!
//! In local/dev mode, tenant databases are `file:`-based SQLite files.
//! In cloud mode (with Turso Cloud API credentials), new tenant databases
//! are provisioned on demand.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, instrument, warn};

use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    storage_error,
};
use temper_runtime::tenant::{KEPT_TENANTS, parse_persistence_id_parts};

use crate::TursoEventStore;
use crate::schema;
use crate::store::WritePriority;

mod provision;
mod remove;

/// Registry statuses whose tenants are connected: `active` ones, and those
/// being removed (`deleting`), which stay reachable until their removal ends
/// so a removal that failed part way can be run again.
const REACHABLE: [&str; 2] = ["active", "deleting"];

/// Routes storage operations to per-tenant Turso databases.
///
/// Holds a platform database (for tenant registry, user access, and shared
/// system packages) plus a lazily-populated map of tenant → `TursoEventStore`.
#[derive(Clone)]
pub struct TenantStoreRouter {
    /// Platform database — tenant registry, user mappings, system integrations.
    platform: TursoEventStore,
    /// Per-tenant database connections, keyed by tenant ID.
    tenants: Arc<RwLock<BTreeMap<String, TursoEventStore>>>,
    /// Turso Cloud API token for dynamic provisioning (optional).
    #[cfg(feature = "cloud")]
    turso_api_token: Option<String>,
    /// Turso Cloud organization slug.
    #[cfg(feature = "cloud")]
    turso_org: Option<String>,
    /// Turso Cloud database group for new databases.
    #[cfg(feature = "cloud")]
    turso_group: Option<String>,
    /// Turso Cloud API base URL.
    #[cfg(feature = "cloud")]
    turso_api_base_url: String,
    /// Base directory for local file-based tenant databases (dev mode).
    local_base_dir: Option<String>,
}

impl std::fmt::Debug for TenantStoreRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantStoreRouter")
            .field("platform", &"<TursoEventStore>")
            .field("tenants", &"<RwLock<BTreeMap>>")
            .finish()
    }
}

/// A row from the `tenant_registry` table.
#[derive(Debug, Clone)]
pub struct TenantRegistryRow {
    pub tenant_id: String,
    pub turso_db_url: String,
    pub turso_auth_token: Option<String>,
    pub status: String,
}

/// A row from the `tenant_users` table.
#[derive(Debug, Clone)]
pub struct TenantUserRow {
    pub tenant_id: String,
    pub user_id: String,
    pub role: String,
}

impl TenantStoreRouter {
    /// Connect to the platform database and load existing tenant connections.
    ///
    /// # Arguments
    ///
    /// * `platform_url` — URL for the platform database (e.g., `libsql://...` or `file:platform.db`)
    /// * `platform_token` — Auth token for remote platform DB (None for local)
    /// * `local_base_dir` — Base directory for local tenant DBs (dev mode, e.g., `.temper/tenants/`)
    #[instrument(skip_all, fields(otel.name = "router.new"))]
    pub async fn new(
        platform_url: &str,
        platform_token: Option<&str>,
        local_base_dir: Option<String>,
    ) -> Result<Self, PersistenceError> {
        let platform = TursoEventStore::new(platform_url, platform_token).await?;

        // Run platform-specific migrations (tenant registry + user tables).
        Self::migrate_platform(&platform).await?;

        let router = Self {
            platform,
            tenants: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "cloud")]
            turso_api_token: None,
            #[cfg(feature = "cloud")]
            turso_org: None,
            #[cfg(feature = "cloud")]
            turso_group: None,
            #[cfg(feature = "cloud")]
            turso_api_base_url: "https://api.turso.tech".to_string(),
            local_base_dir,
        };

        // Pre-connect to all registered tenants.
        router.connect_registered_tenants().await?;

        Ok(router)
    }

    /// Configure Turso Cloud API credentials for dynamic provisioning.
    #[cfg(feature = "cloud")]
    pub fn with_cloud_config(
        mut self,
        api_token: String,
        org: String,
        group: Option<String>,
    ) -> Self {
        self.turso_api_token = Some(api_token);
        self.turso_org = Some(org);
        self.turso_group = group;
        self
    }

    /// Access the platform store directly (for shared system packages, user lookups).
    pub fn platform_store(&self) -> &TursoEventStore {
        &self.platform
    }

    /// Get the store for a specific tenant.
    ///
    /// Returns the tenant-specific store if connected, or attempts to connect
    /// from the registry. For the special `temper-system` tenant, returns the
    /// platform store.
    #[instrument(skip_all, fields(tenant, otel.name = "router.store_for_tenant"))]
    pub async fn store_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<TursoEventStore, PersistenceError> {
        // System tenant uses the platform DB.
        if tenant == "temper-system" {
            return Ok(self.platform.clone());
        }

        // Check cache first (read lock).
        {
            let tenants = self.tenants.read().await;
            if let Some(store) = tenants.get(tenant) {
                return Ok(store.clone());
            }
        }

        // Not cached — try to connect from registry.
        self.connect_tenant(tenant).await
    }

    /// List all registered tenant IDs.
    #[instrument(skip_all, fields(otel.name = "router.list_tenants"))]
    pub async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let rows = self.load_tenant_registry().await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.status == "active")
            .map(|r| r.tenant_id)
            .collect())
    }

    /// List tenants accessible by a given user ID (e.g., `github:username`).
    #[instrument(skip_all, fields(user_id, otel.name = "router.tenants_for_user"))]
    pub async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        let conn = self.platform.connection().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, user_id, role FROM tenant_users WHERE user_id = ?1",
                crate::driver::params![user_id],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantUserRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                user_id: row.get::<String>(1).map_err(storage_error)?,
                role: row.get::<String>(2).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Add a user to a tenant.
    #[instrument(skip_all, fields(tenant_id, user_id, otel.name = "router.add_tenant_user"))]
    pub async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self
            .platform
            .write_connection("router.add_tenant_user", WritePriority::High)
            .await?;
        conn.execute(
            "INSERT OR REPLACE INTO tenant_users (tenant_id, user_id, role) VALUES (?1, ?2, ?3)",
            crate::driver::params![tenant_id, user_id, role],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// List all users for a specific tenant.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.list_tenant_users"))]
    pub async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        let conn = self.platform.connection().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, user_id, role FROM tenant_users WHERE tenant_id = ?1",
                crate::driver::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantUserRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                user_id: row.get::<String>(1).map_err(storage_error)?,
                role: row.get::<String>(2).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Mark a tenant as being removed: its registry row becomes `deleting`,
    /// a tombstone that keeps its id from being registered again until
    /// [`Self::remove_tenant`] has removed everything it had. The tenant
    /// stays reachable meanwhile, so a removal that fails part way can be
    /// run again. Returns false when no such tenant is registered, or its
    /// create has not ended (creating it again finishes that).
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.begin_tenant_removal"))]
    pub async fn begin_tenant_removal(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        if KEPT_TENANTS.contains(&tenant_id) {
            return Err(PersistenceError::Storage(format!(
                "tenant '{tenant_id}' holds the server's own data and is never removed"
            )));
        }
        let marked = self
            .platform
            .write_connection("router.begin_tenant_removal", WritePriority::High)
            .await?
            .execute(
                "UPDATE tenant_registry SET status = 'deleting' WHERE tenant_id = ?1 AND status != ?2",
                crate::driver::params![tenant_id, provision::PROVISIONING],
            )
            .await
            .map_err(storage_error)?;
        Ok(marked > 0)
    }

    /// Remove a tenant entirely: its database and then its registry rows.
    ///
    /// The tenant is marked first ([`Self::begin_tenant_removal`]), then made
    /// unreachable; the database is removed ([`Self::remove_database`]), and
    /// only then are the rows deleted, the tombstone last. A removal that fails leaves the
    /// tombstone, and removing the tenant again finishes it. Returns false
    /// when no such tenant is registered.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.remove_tenant"))]
    pub async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        if !self.begin_tenant_removal(tenant_id).await? {
            return Ok(false);
        }
        // From here the tenant is no longer reachable (`removing` is not in
        // `REACHABLE`), so no connection reopens its database while it goes.
        self.platform
            .write_connection("router.remove_tenant", WritePriority::High)
            .await?
            .execute(
                "UPDATE tenant_registry SET status = 'removing' WHERE tenant_id = ?1",
                crate::driver::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;
        self.finish_removal(tenant_id).await?;
        Ok(true)
    }

    /// Remove an unreachable (`removing`) tenant's database, then its rows.
    /// Boot runs this for every tenant whose removal stopped here.
    async fn finish_removal(&self, tenant_id: &str) -> Result<(), PersistenceError> {
        self.tenants.write().await.remove(tenant_id);
        let entry = self
            .load_tenant_registry()
            .await?
            .into_iter()
            .find(|row| row.tenant_id == tenant_id)
            .ok_or_else(|| {
                PersistenceError::Storage(format!("tenant '{tenant_id}' left the registry"))
            })?;
        self.remove_database(tenant_id, &entry.turso_db_url).await?;

        let conn = self
            .platform
            .write_connection("router.remove_tenant", WritePriority::High)
            .await?;
        for table in ["tenant_users", "tenant_installed_apps", "tenant_registry"] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE tenant_id = ?1"),
                crate::driver::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;
        }
        info!(tenant_id, "Tenant and its database removed");
        Ok(())
    }

    /// Remove a user from a tenant.
    #[instrument(skip_all, fields(tenant_id, user_id, otel.name = "router.remove_tenant_user"))]
    pub async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self
            .platform
            .write_connection("router.remove_tenant_user", WritePriority::High)
            .await?;
        conn.execute(
            "DELETE FROM tenant_users WHERE tenant_id = ?1 AND user_id = ?2",
            crate::driver::params![tenant_id, user_id],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Ensure a tenant exists in the persistence layer.
    ///
    /// If the tenant is already registered, returns `Ok(true)` (already existed).
    /// If not, provisions a new database and registers it, returning `Ok(false)`.
    #[instrument(skip_all, fields(tenant_id, otel.name = "router.ensure_tenant"))]
    pub async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        {
            let tenants = self.tenants.read().await;
            if tenants.contains_key(tenant_id) {
                return Ok(true);
            }
        }
        // Not in memory — check if in DB but not yet connected.
        let conn = self.platform.connection().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT turso_db_url, turso_auth_token, status FROM tenant_registry WHERE tenant_id = ?1",
                crate::driver::params![tenant_id],
            )
            .await
            .map_err(storage_error)?;
        if let Some(row) = rows.next().await.map_err(storage_error)? {
            let status: String = row.get::<String>(2).map_err(storage_error)?;
            if status == provision::PROVISIONING {
                // A create that stopped part way: finish it.
                self.register_tenant(tenant_id).await?;
                return Ok(false);
            }
            if status != "active" {
                return Err(PersistenceError::Storage(format!(
                    "tenant '{tenant_id}' is {status}"
                )));
            }
            // Exists in DB but not connected — reconnect.
            let db_url: String = row.get::<String>(0).map_err(storage_error)?;
            let auth_token: Option<String> = row.get::<Option<String>>(1).ok().flatten();
            let store = TursoEventStore::new(&db_url, auth_token.as_deref()).await?;
            self.tenants
                .write()
                .await
                .insert(tenant_id.to_string(), store);
            return Ok(true);
        }
        // Not in DB either — provision new.
        self.register_tenant(tenant_id).await?;
        Ok(false)
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Run platform-specific schema migrations.
    async fn migrate_platform(store: &TursoEventStore) -> Result<(), PersistenceError> {
        let conn = store
            .write_connection("router.migrate_platform", WritePriority::High)
            .await?;
        conn.execute(schema::CREATE_TENANT_REGISTRY_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_USERS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_USERS_USER_INDEX, ())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    /// Load all tenant registry rows from the platform DB.
    async fn load_tenant_registry(&self) -> Result<Vec<TenantRegistryRow>, PersistenceError> {
        let conn = self.platform.connection().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT tenant_id, turso_db_url, turso_auth_token, status
                 FROM tenant_registry ORDER BY tenant_id",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TenantRegistryRow {
                tenant_id: row.get::<String>(0).map_err(storage_error)?,
                turso_db_url: row.get::<String>(1).map_err(storage_error)?,
                turso_auth_token: row.get::<Option<String>>(2).ok().flatten(),
                status: row.get::<String>(3).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Pre-connect to all active tenants in the registry.
    async fn connect_registered_tenants(&self) -> Result<(), PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        for entry in registry.iter().filter(|entry| entry.status == "removing") {
            if let Err(e) = self.finish_removal(&entry.tenant_id).await {
                warn!(tenant = %entry.tenant_id, error = %e, "Tenant removal still unfinished");
            }
        }
        let mut tenants = self.tenants.write().await;

        for entry in &registry {
            if !REACHABLE.contains(&entry.status.as_str()) {
                continue;
            }
            match TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await
            {
                Ok(store) => {
                    tenants.insert(entry.tenant_id.clone(), store);
                    info!(tenant = %entry.tenant_id, "Connected to tenant database");
                }
                Err(e) => {
                    warn!(
                        tenant = %entry.tenant_id,
                        error = %e,
                        "Failed to connect to tenant database, skipping"
                    );
                }
            }
        }
        Ok(())
    }

    /// Connect to a tenant from the registry (not cached yet).
    async fn connect_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        let registry = self.load_tenant_registry().await?;
        let entry = registry
            .iter()
            .find(|r| r.tenant_id == tenant_id && REACHABLE.contains(&r.status.as_str()))
            .ok_or_else(|| {
                PersistenceError::Storage(format!("tenant '{tenant_id}' not found in registry"))
            })?;

        let store =
            TursoEventStore::new(&entry.turso_db_url, entry.turso_auth_token.as_deref()).await?;
        self.tenants
            .write()
            .await
            .insert(tenant_id.to_string(), store.clone());

        info!(tenant = tenant_id, "Connected to tenant database on demand");
        Ok(store)
    }

    /// List all connected tenant IDs (cached connections only).
    pub async fn connected_tenants(&self) -> Vec<String> {
        self.tenants.read().await.keys().cloned().collect()
    }
}

/// `EventStore` implementation that routes by tenant extracted from `persistence_id`.
impl EventStore for TenantStoreRouter {
    #[instrument(skip_all, fields(persistence_id, otel.name = "router.append"))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .append(persistence_id, expected_sequence, events)
            .await
    }

    #[instrument(skip_all, fields(otel.name = "router.append_batch"))]
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let Some(first) = appends.first() else {
            return Ok(Vec::new());
        };
        let (tenant, _, _) =
            parse_persistence_id_parts(&first.persistence_id).map_err(PersistenceError::Storage)?;
        for append in &appends[1..] {
            let (next_tenant, _, _) = parse_persistence_id_parts(&append.persistence_id)
                .map_err(PersistenceError::Storage)?;
            if next_tenant != tenant {
                return Err(PersistenceError::Storage(format!(
                    "append_batch cannot span routed tenant databases: '{tenant}' and '{next_tenant}'"
                )));
            }
        }
        let store = self.store_for_tenant(tenant).await?;
        store.append_batch(appends).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.read_events"))]
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.read_events(persistence_id, from_sequence).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.read_events_page"))]
    async fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .read_events_page(persistence_id, before_sequence, limit)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.first_event_at"))]
    async fn first_event_at(
        &self,
        persistence_id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        self.store_for_tenant(tenant)
            .await?
            .first_event_at(persistence_id)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.save_snapshot"))]
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.load_snapshot"))]
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.load_snapshot(persistence_id).await
    }

    async fn latest_sequence_nr(&self, persistence_id: &str) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        self.store_for_tenant(tenant)
            .await?
            .latest_sequence_nr(persistence_id)
            .await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.journal_heads"))]
    async fn journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::JournalHead>, PersistenceError> {
        self.store_for_tenant(tenant)
            .await?
            .journal_heads(tenant)
            .await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_entity_ids(tenant).await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.list_entity_ids_by_type"))]
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_entity_ids_by_type(tenant, entity_type).await
    }

    // ADR-0155: forward the vector-index surface to the per-tenant store so kNN works
    // on the routed Turso deployment. (Keys deliberately fall through to the no-op
    // defaults — Turso does not maintain entity_key_index live; see event_store.rs.)
    #[instrument(skip_all, fields(persistence_id, otel.name = "router.append_with_rows"))]
    async fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: temper_runtime::persistence::CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .append_with_rows(persistence_id, expected_sequence, events, rows)
            .await
    }

    /// Each tenant's store commits its entities' rows, and is also that
    /// tenant's query plane and timer-row store.
    fn commits_entity_rows(&self) -> bool {
        true
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.list_scheduled_actions"))]
    async fn list_scheduled_actions(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::ScheduledActionRecord>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_scheduled_actions(tenant).await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.list_entity_scheduled_actions"))]
    async fn list_entity_scheduled_actions(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Vec<temper_runtime::persistence::ScheduledActionRecord>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .list_entity_scheduled_actions(tenant, entity_type, entity_id)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.cancel_scheduled_action"))]
    async fn cancel_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .cancel_scheduled_action(tenant, entity_type, entity_id, arm_id)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.delete_scheduled_action"))]
    async fn delete_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .delete_scheduled_action(tenant, entity_type, entity_id, arm_id)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.backfill_entity_vectors"))]
    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.vector_candidates"))]
    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<temper_runtime::persistence::EntityVectorCandidate>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .vector_candidates(tenant, entity_type, decl_name, model_tag, limit)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.mark_vector_index_backfilled"))]
    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .mark_vector_index_backfilled(tenant, entity_type, vector_set)
            .await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.vector_index_backfilled_types"))]
    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.vector_index_backfilled_types(tenant).await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.vectored_entity_ids_for_type"))]
    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .vectored_entity_ids_for_type(tenant, entity_type)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::params;

    #[tokio::test]
    async fn test_router_local_dev() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let platform_url = format!("file:{}", platform_path.display());

        let router = TenantStoreRouter::new(
            &platform_url,
            None,
            Some(dir.path().join("tenants").to_string_lossy().to_string()),
        )
        .await
        .expect("router creation");

        // No tenants initially.
        let tenants = router.list_tenants().await.expect("list");
        assert!(tenants.is_empty());

        // Register a tenant.
        let _store = router.register_tenant("alpha").await.expect("register");

        // Verify it's registered.
        let tenants = router.list_tenants().await.expect("list");
        assert_eq!(tenants, vec!["alpha"]);

        // Write and read back through the router.
        let persistence_id = "alpha:Order:order-1";
        let events = vec![PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "OrderCreated".to_string(),
            payload: serde_json::json!({"status": "Draft"}),
            metadata: temper_runtime::persistence::EventMetadata {
                event_id: uuid::Uuid::nil(),
                causation_id: uuid::Uuid::nil(),
                correlation_id: uuid::Uuid::nil(),
                timestamp: chrono::Utc::now(),
                actor_id: "test".to_string(),
            },
        }];
        let seq = router
            .append(persistence_id, 0, &events)
            .await
            .expect("append");
        assert_eq!(seq, 1);

        let read_back = router.read_events(persistence_id, 0).await.expect("read");
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].event_type, "OrderCreated");

        // System tenant routes to platform DB.
        let sys_store = router
            .store_for_tenant("temper-system")
            .await
            .expect("system");
        // The platform store should work (it has the entity schema too).
        let entity_ids = sys_store
            .list_entity_ids("temper-system")
            .await
            .expect("list");
        assert!(entity_ids.is_empty());
    }

    #[tokio::test]
    async fn routed_creation_time_survives_compaction_and_keeps_tenants_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let router = TenantStoreRouter::new(
            &format!("file:{}", dir.path().join("platform.db").display()),
            None,
            Some(dir.path().join("tenants").to_string_lossy().into_owned()),
        )
        .await
        .unwrap();
        // The same entity key in separate tenant stores must retain each
        // catalog's known creation time even with only a later event left.
        for (tenant, first) in [
            ("alpha", "2026-01-01T00:00:00.100Z"),
            ("beta", "2026-02-01T00:00:00.200Z"),
        ] {
            let store = router.register_tenant(tenant).await.unwrap();
            store
                .upsert_query_projection_with_state(
                    tenant,
                    "Order",
                    "one",
                    "Draft",
                    &serde_json::json!({}),
                    &serde_json::json!({"first_event_at": first}),
                    2,
                )
                .await
                .unwrap();
            let conn = store
                .write_connection("test", WritePriority::High)
                .await
                .unwrap();
            conn.execute(
                "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, event_type, payload) VALUES (?1, 'Order', 'one', 2, 'Moved', ?2)",
                params![tenant, r#"{"timestamp":"2026-03-01T00:00:00.000Z"}"#],
            ).await.unwrap();
        }
        for (tenant, first) in [
            ("alpha", "2026-01-01T00:00:00.100Z"),
            ("beta", "2026-02-01T00:00:00.200Z"),
        ] {
            assert_eq!(
                router
                    .first_event_at(&format!("{tenant}:Order:one"))
                    .await
                    .unwrap(),
                Some(
                    chrono::DateTime::parse_from_rfc3339(first)
                        .unwrap()
                        .with_timezone(&chrono::Utc)
                ),
            );
            assert_eq!(
                router
                    .first_event_at(&format!("{tenant}:Order:missing"))
                    .await
                    .unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn test_tenant_user_management() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let platform_url = format!("file:{}", platform_path.display());

        let router = TenantStoreRouter::new(&platform_url, None, None)
            .await
            .expect("router");

        // Add users.
        router
            .add_tenant_user("alpha", "github:alice", "admin")
            .await
            .expect("add user");
        router
            .add_tenant_user("alpha", "github:bob", "member")
            .await
            .expect("add user");
        router
            .add_tenant_user("beta", "github:alice", "member")
            .await
            .expect("add user");

        // Query by user.
        let alice_tenants = router
            .tenants_for_user("github:alice")
            .await
            .expect("query");
        assert_eq!(alice_tenants.len(), 2);

        let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
        assert_eq!(bob_tenants.len(), 1);
        assert_eq!(bob_tenants[0].tenant_id, "alpha");

        // Remove user.
        router
            .remove_tenant_user("alpha", "github:bob")
            .await
            .expect("remove");
        let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
        assert!(bob_tenants.is_empty());
    }

    #[tokio::test]
    async fn test_ensure_tenant_reconnects_existing_registry_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let tenant_db_path = dir.path().join("preprovisioned.db");
        let platform_url = format!("file:{}", platform_path.display());
        let tenant_db_url = format!("file:{}", tenant_db_path.display());

        let router = TenantStoreRouter::new(&platform_url, None, None)
            .await
            .expect("router");

        let conn = router
            .platform_store()
            .write_connection("test", WritePriority::High)
            .await
            .expect("conn");
        conn.execute(
            "INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token)
             VALUES (?1, ?2, ?3)",
            params!["preprovisioned", tenant_db_url, Option::<String>::None],
        )
        .await
        .expect("insert registry row");

        let existed = router
            .ensure_tenant("preprovisioned")
            .await
            .expect("ensure existing tenant");
        assert!(existed, "existing tenant should report already existed");

        let connected = router.connected_tenants().await;
        assert!(
            connected.contains(&"preprovisioned".to_string()),
            "tenant should be connected after ensure_tenant"
        );
    }

    #[tokio::test]
    async fn test_ensure_tenant_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let platform_path = dir.path().join("platform.db");
        let platform_url = format!("file:{}", platform_path.display());

        let router = TenantStoreRouter::new(
            &platform_url,
            None,
            Some(dir.path().join("tenants").to_string_lossy().to_string()),
        )
        .await
        .expect("router creation");

        let first = router.ensure_tenant("repeat").await.expect("first ensure");
        let second = router.ensure_tenant("repeat").await.expect("second ensure");

        assert!(!first, "first ensure should provision the tenant");
        assert!(
            second,
            "second ensure should be idempotent and reuse tenant"
        );

        let tenants = router.list_tenants().await.expect("list tenants");
        assert_eq!(tenants, vec!["repeat".to_string()]);
    }
}
