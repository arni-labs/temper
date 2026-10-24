//! An overdue install timer cannot dispatch while module storage is awaiting.
use super::*;
use std::sync::Arc;
use temper_runtime::persistence::{EventStore, PersistenceError};
use temper_server::storage::TursoStoreProvider;
use temper_store_turso::{TenantUserRow, TursoEventStore};

tokio::task_local! { static INSTALLER: bool; }

struct ObserveModuleInstall {
    inner: Arc<dyn TursoStoreProvider>,
    modules: temper_server::ServerState,
    tenant: TenantId,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl TursoStoreProvider for ObserveModuleInstall {
    fn supports_tenant_admin(&self) -> bool {
        self.inner.supports_tenant_admin()
    }
    fn platform_store(&self) -> Option<TursoEventStore> {
        self.inner.platform_store()
    }
    async fn store_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        let spec_changed = self
            .modules
            .registry
            .read()
            .expect("registry lock")
            .get_spec(&self.tenant, "Job")
            .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty());
        if tenant == self.tenant.as_str()
            && spec_changed
            && INSTALLER.try_with(|installer| *installer).unwrap_or(false)
        {
            self.entered.notify_one();
            let _ = self.release.acquire().await;
        }
        self.inner.store_for_tenant(tenant).await
    }
    async fn all_stores(&self) -> Vec<TursoEventStore> {
        self.inner.all_stores().await
    }
    async fn connected_tenants(&self) -> Vec<String> {
        self.inner.connected_tenants().await
    }
    async fn tenants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.tenants_for_user(user_id).await
    }
    async fn register_tenant(&self, tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        self.inner.register_tenant(tenant_id).await
    }
    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        self.inner.list_tenants().await
    }
    async fn remove_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.remove_tenant(tenant_id).await
    }
    async fn add_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.add_tenant_user(tenant_id, user_id, role).await
    }
    async fn list_tenant_users(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        self.inner.list_tenant_users(tenant_id).await
    }
    async fn remove_tenant_user(
        &self,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner.remove_tenant_user(tenant_id, user_id).await
    }
    async fn ensure_tenant(&self, tenant_id: &str) -> Result<bool, PersistenceError> {
        self.inner.ensure_tenant(tenant_id).await
    }
}

#[tokio::test]
async fn overdue_install_timer_waits_for_module_registration() {
    let root = std::env::temp_dir().join(format!("timer-install-{}", uuid::Uuid::new_v4()));
    let app = "timer-install";
    let dir = root.join(app);
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::create_dir_all(dir.join("wasm/echo")).expect("create the echo module dir");
    fs::create_dir_all(dir.join("policies")).expect("create policies dir");
    fs::write(
        dir.join("policies/allow.cedar"),
        "permit(principal, action, resource);",
    )
    .expect("write policy");
    fs::write(
        dir.join("app.toml"),
        r#"name = "timer-install"
version = "0.1.0"
[[wasm_modules]]
name = "echo"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Timer install").expect("write APP.md");
    let wasm = fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../temper-wasm/tests/fixtures/echo_integration.wasm"),
    )
    .expect("read echo fixture");
    fs::write(dir.join("wasm/echo/echo.wasm"), &wasm).expect("write echo module");
    let spec = r#"
[automaton]
name = "Job"
states = ["Waiting", "Done"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Done"]
[[action]]
name = "Poke"
from = ["Waiting"]
[[action]]
name = "Expire"
from = ["Waiting"]
to = "Done"
"#;
    let csdl = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Timers" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Timers.Job"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
    fs::write(dir.join("specs/model.csdl.xml"), csdl).expect("write csdl");
    fs::write(dir.join("specs/job.ioa.toml"), spec).expect("write spec");
    let store = TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
        .await
        .expect("open turso store");
    let mut state = PlatformState::new(None);
    state.server.data_dir = root.join("data");
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
    let tenant = TenantId::new("timer-install");
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            tenant.as_str(),
            parse_csdl(csdl).expect("parse csdl"),
            csdl.to_string(),
            &[("Job", spec)],
        );
    state
        .registry
        .write()
        .expect("registry lock")
        .set_verification_status(
            &tenant,
            "Job",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-10-24T00:00:00Z".to_string(),
            }),
        );
    state
        .server
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("load policies");
    state
        .server
        .start_runtime()
        .await
        .expect("first start")
        .await
        .expect("boot walk");
    state
        .server
        .get_or_create_tenant_entity(&tenant, "Job", "r", serde_json::json!({}))
        .await
        .expect("create record");
    let response = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Job",
            "r",
            "Poke",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch Poke");
    assert!(
        response.success,
        "initial dispatch failed: {:?}",
        response.error
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut stack = state
        .server
        .storage_stack
        .as_ref()
        .expect("storage stack")
        .as_ref()
        .clone();
    stack.turso = Some(Arc::new(ObserveModuleInstall {
        inner: stack.turso.take().expect("turso provider"),
        modules: state.server.clone(),
        tenant: tenant.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    state.server.set_storage_stack(stack);
    // The record predates the newly installed one-second budget.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fs::write(
        dir.join("specs/job.ioa.toml"),
        format!(
            r#"{spec}
[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Expire"
"#
        ),
    )
    .expect("write timed spec");
    add_os_apps_dir(root.clone());
    let install = INSTALLER.scope(
        true,
        install_os_app_with_plan(
            &state,
            tenant.as_str(),
            app,
            OsAppInstallPlan {
                specs: true,
                policies: true,
                wasm: true,
                content: false,
                seed: false,
            },
        ),
    );
    tokio::pin!(install);
    tokio::select! {
        _ = entered.notified() => {},
        result = &mut install => panic!("installation missed the storage pause: {result:?}"),
    }
    assert!(
        state
            .server
            .wasm_module_registry
            .read()
            .expect("module registry lock")
            .get_hash(&tenant, "echo")
            .is_none()
    );
    // Hold module storage long enough for an erroneously early timer to fire.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        store
            .read_events("timer-install:Job:r", 0)
            .await
            .expect("read events")
            .len(),
        2,
        "overdue timeout dispatched while module installation was blocked"
    );
    release.close();
    install.await.expect("install the app");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .read_events("timer-install:Job:r", 0)
                .await
                .expect("read events")
                .len()
                == 3
            {
                let record = state
                    .server
                    .get_tenant_entity_state(&tenant, "Job", "r")
                    .await
                    .expect("read record");
                assert_eq!(record.state.status, "Done");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the overdue record must leave its timed state after installation");
    fs::remove_dir_all(root).expect("remove temp dir");
}
