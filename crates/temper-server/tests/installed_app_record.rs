//! Recording that an app is installed in a tenant adds the app to the
//! tenant's installs and never changes a record already stored for it.
//!
//! Every install, a boot's replay of a stored one included, records the app
//! as installed before it writes the app's digests. On Postgres that record
//! replaced the stored one with a blank local record, so a replay of an app
//! installed from a registry lost where it came from: its source, ref,
//! version, registry and digests.
//!
//! The Postgres case needs `TEMPER_TEST_POSTGRES_URL` naming a disposable
//! database; the Turso case runs anywhere.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_server::platform_store::{InstalledAppRecord, PlatformStore};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;

const TENANT: &str = "installs";

/// A store an install writes its record to.
enum Store {
    Turso {
        store: TursoEventStore,
        _dir: tempfile::TempDir,
    },
    Postgres(PostgresEventStore),
}

impl Store {
    async fn turso() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let url = format!("file:{}", dir.path().join("installs.db").display());
        let store = TursoEventStore::new(&url, None)
            .await
            .expect("open the Turso store");
        Self::Turso { store, _dir: dir }
    }

    async fn postgres() -> Self {
        let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
        let options: PgConnectOptions = url.parse().expect("a Postgres URL");
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", "pg_catalog")]))
            .await
            .expect("connect to Postgres");
        let name = format!(
            "installed_app_record_{}",
            temper_runtime::scheduler::sim_uuid().simple()
        );
        let schema = PostgresSchema::new(name).expect("a schema name");
        migration::run_migrations_in_schema(&pool, &schema)
            .await
            .expect("migrate the schema");
        Self::Postgres(PostgresEventStore::with_schema(pool, schema))
    }

    fn platform(&self) -> &dyn PlatformStore {
        match self {
            Self::Turso { store, .. } => store,
            Self::Postgres(store) => store,
        }
    }

    async fn record(&self, app: &str) -> Option<InstalledAppRecord> {
        self.platform()
            .get_installed_app(TENANT, app)
            .await
            .expect("read an install record")
    }
}

/// `notes`, installed from a registry at a pinned version.
fn registry_install() -> InstalledAppRecord {
    let version = "5".repeat(40);
    InstalledAppRecord {
        tenant: TENANT.to_string(),
        app_name: "notes".to_string(),
        source_kind: "genesis".to_string(),
        app_ref: format!("acme/notes@{version}"),
        version_hash: version.clone(),
        pinned_version_hash: version.clone(),
        current_version_hash: version,
        follow_policy: "pinned".to_string(),
        closure_id: "closure-notes".to_string(),
        registry_url: "https://registry.example".to_string(),
        registry_tenant: "default".to_string(),
        app_version: "1.2.0".to_string(),
        bundle_digest: "sha256:bundle".to_string(),
        spec_digest: "sha256:spec".to_string(),
        policy_digest: "sha256:policy".to_string(),
        wasm_digest: "sha256:wasm".to_string(),
        content_digest: "sha256:content".to_string(),
        seed_digest: "sha256:seed".to_string(),
        installed_at: None,
        last_reconciled_at: None,
        status: "installed".to_string(),
    }
}

async fn recording_an_install_keeps_its_stored_record(store: Store) {
    store
        .platform()
        .record_installed_app_metadata(&registry_install())
        .await
        .expect("store the registry install's record");
    let stored = store.record("notes").await.expect("the record is stored");
    assert_eq!(stored.source_kind, "genesis");

    // What every install, a replay included, does before it writes digests.
    store
        .platform()
        .record_installed_app(TENANT, "notes")
        .await
        .expect("record the install");
    assert_eq!(
        store.record("notes").await,
        Some(stored),
        "recording an install replaced the app's stored record: a replay loses \
         where the app came from"
    );

    // An app with no record yet gets one, and is among the tenant's installs.
    store
        .platform()
        .record_installed_app(TENANT, "drafts")
        .await
        .expect("record a first install");
    let first = store
        .record("drafts")
        .await
        .expect("a first install is recorded");
    assert_eq!(first.source_kind, "local");
    assert_eq!(first.status, "installed");
    let installed = store
        .platform()
        .list_all_installed_apps()
        .await
        .expect("list installs");
    for app in ["notes", "drafts"] {
        assert!(
            installed.contains(&(TENANT.to_string(), app.to_string())),
            "{app} is among the installs: {installed:?}"
        );
    }
}

#[tokio::test]
async fn a_turso_install_record_keeps_its_stored_record() {
    recording_an_install_keeps_its_stored_record(Store::turso().await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn a_postgres_install_record_keeps_its_stored_record() {
    recording_an_install_keeps_its_stored_record(Store::postgres().await).await;
}
