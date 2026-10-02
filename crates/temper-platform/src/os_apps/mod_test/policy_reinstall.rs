//! Reinstalling an app replaces its previous Cedar (a stale forbid must not
//! outlive its replacement), and leaves every other owner's policy alone.

use super::*;
use std::path::PathBuf;
use std::sync::Arc;

use temper_server::storage::{PolicyStore, PolicyStoreRow};
use temper_store_turso::TursoEventStore;

const PERMIT_THINGS: &str = "permit(principal, action, resource is Thing);";
const FORBID_WITHDRAW: &str =
    "forbid(principal, action == Action::\"Withdraw\", resource is Thing);";
const FORBID_DELETE: &str = "forbid(principal, action == Action::\"Delete\", resource is Thing);";
const APPROVED_PING: &str = "permit(principal, action == Action::\"Ping\", resource is Gadget);";
const HAND_ADDED_POKE: &str = "permit(principal, action == Action::\"Poke\", resource is Gadget);";
/// `FORBID_WITHDRAW` as someone might have saved it by hand.
const FORBID_WITHDRAW_REFORMATTED: &str = "// frozen items\r\nforbid(\r\n    principal,\r\n    action == Action::\"Withdraw\",\r\n    resource is Thing\r\n);";

struct Fixture {
    state: PlatformState,
    db_url: String,
    apps_root: PathBuf,
    tenant: String,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        Self::with_policy_store(label, |store| store).await
    }

    /// A fixture whose policy rows go through `wrap(real store)`.
    async fn with_policy_store(
        label: &str,
        wrap: impl FnOnce(Arc<dyn PolicyStore>) -> Arc<dyn PolicyStore>,
    ) -> Self {
        let id = uuid::Uuid::new_v4();
        let db_url = format!("file:/tmp/temper-policy-reinstall-{label}-{id}.db");
        let apps_root = std::env::temp_dir().join(format!("temper-policy-reinstall-{label}-{id}"));
        let turso = TursoEventStore::new(&db_url, None).await.unwrap();
        let mut stack = temper_server::StorageStack::from_turso(turso);
        stack.policies = stack.policies.map(wrap);
        let mut state = PlatformState::new(None);
        state.server.set_storage_stack(stack);
        Self {
            state,
            db_url,
            apps_root,
            tenant: format!("test-policy-reinstall-{label}"),
        }
    }

    /// Write `app` with exactly these policy files, replacing earlier ones.
    fn write_app(&self, app: &str, policies: &[(&str, &str)]) {
        let app_dir = self.apps_root.join(app);
        let policy_dir = app_dir.join("policies");
        let _ = fs::remove_dir_all(&policy_dir);
        fs::create_dir_all(&policy_dir).unwrap();
        fs::write(
            app_dir.join("app.toml"),
            format!("name = \"{app}\"\ndescription = \"Policy reinstall test app\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        fs::write(
            app_dir.join("APP.md"),
            format!("# {app}\n\nPolicy reinstall test app.\n"),
        )
        .unwrap();
        for (file, text) in policies {
            fs::write(policy_dir.join(file), text).unwrap();
        }
    }

    async fn install(&self, app: &str) {
        self.try_install(app)
            .await
            .unwrap_or_else(|error| panic!("install {app}: {error}"));
    }

    async fn try_install(&self, app: &str) -> Result<InstallResult, String> {
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, &self.tenant, app).await
    }

    fn allowed(&self, state: &PlatformState, action: &str, resource_type: &str) -> bool {
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), serde_json::json!("item-1"));
        state
            .server
            .authz
            .authorize_for_tenant(
                &self.tenant,
                &test_admin_security_context("admin-1"),
                action,
                resource_type,
                &attrs,
            )
            .is_allowed()
    }

    fn live_text(&self) -> String {
        self.state
            .server
            .authz
            .get_tenant_policy_text(&self.tenant)
            .unwrap_or_default()
    }

    async fn rows(&self) -> Vec<temper_server::storage::PolicyStoreRow> {
        self.state
            .server
            .policy_store()
            .expect("policy store")
            .load_policies_for_tenant(&self.tenant)
            .await
            .expect("load policy rows")
    }

    async fn legacy_text(&self) -> String {
        let store = self.state.server.platform_turso_store().unwrap();
        store
            .load_tenant_policies()
            .await
            .unwrap()
            .into_iter()
            .find(|(tenant, _)| tenant == &self.tenant)
            .map(|(_, text)| text)
            .unwrap_or_default()
    }

    /// Store a row and activate it the way the approval and PUT routes do.
    async fn add_active_row(&self, policy_id: &str, cedar_text: &str, live_text: &str) {
        self.state
            .server
            .policy_store()
            .expect("policy store")
            .save_policy(&self.tenant, policy_id, cedar_text, "rita")
            .await
            .expect("save row");
        self.state
            .server
            .authz
            .reload_tenant_policies(&self.tenant, live_text)
            .expect("reload live text");
        self.state
            .server
            .tenant_policies
            .write()
            .unwrap()
            .insert(self.tenant.clone(), live_text.to_string());
    }

    /// A fresh process on the same database, booted the way TemperPaw boots.
    async fn restart(&self) -> PlatformState {
        let store = TursoEventStore::new(&self.db_url, None).await.unwrap();
        let state = PlatformState::new(None);
        crate::recovery::recover_cedar_policies(&state, &store).await;
        state
    }
}

fn thing_policies(forbid: &str) -> String {
    format!("{PERMIT_THINGS}\n{forbid}\n")
}

#[tokio::test]
async fn reinstall_replaces_a_changed_forbid() {
    let fx = Fixture::new("changed-forbid").await;
    let app = format!("reinstall-forbid-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    assert!(!fx.allowed(&fx.state, "Withdraw", "Thing"));

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    assert!(
        fx.allowed(&fx.state, "Withdraw", "Thing"),
        "old forbid still live"
    );
    assert!(
        !fx.allowed(&fx.state, "Delete", "Thing"),
        "new forbid not live"
    );
    assert!(!fx.live_text().contains(FORBID_WITHDRAW));
    assert!(!fx.legacy_text().await.contains(FORBID_WITHDRAW));
    let rows = fx.rows().await;
    assert_eq!(rows.len(), 1, "one row per policy file: {rows:?}");
    assert!(rows[0].cedar_text.contains(FORBID_DELETE));

    let restarted = fx.restart().await;
    assert!(
        fx.allowed(&restarted, "Withdraw", "Thing"),
        "old forbid back after restart"
    );
    assert!(!fx.allowed(&restarted, "Delete", "Thing"));
}

#[tokio::test]
async fn approvals_and_primary_survive_reinstall_and_restart() {
    let fx = Fixture::new("other-owners").await;
    let app = format!("reinstall-owners-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;

    let with_approval = format!("{}\n{APPROVED_PING}", fx.live_text());
    fx.add_active_row("decision:d-1", APPROVED_PING, &with_approval)
        .await;
    // A PUT snapshots the whole live text, the app's current forbid included.
    let snapshot = format!("{with_approval}\n{HAND_ADDED_POKE}");
    fx.add_active_row("primary", &snapshot, &snapshot).await;

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(
            fx.allowed(state, "Withdraw", "Thing"),
            "old forbid survived"
        );
        assert!(!fx.allowed(state, "Delete", "Thing"));
        assert!(fx.allowed(state, "Ping", "Gadget"), "approval lost");
        assert!(
            fx.allowed(state, "Poke", "Gadget"),
            "hand-added policy lost"
        );
    }
    let rows = fx.rows().await;
    let primary = rows.iter().find(|row| row.policy_id == "primary").unwrap();
    assert!(!primary.cedar_text.contains(FORBID_WITHDRAW));
    assert!(primary.cedar_text.contains(HAND_ADDED_POKE));
    assert!(primary.cedar_text.contains(APPROVED_PING));
    assert!(rows.iter().any(|row| row.policy_id == "decision:d-1"));
}

#[tokio::test]
async fn unchanged_reinstall_is_a_noop() {
    let fx = Fixture::new("unchanged").await;
    let app = format!("reinstall-same-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    let live = fx.live_text();
    let rows: Vec<_> = fx
        .rows()
        .await
        .into_iter()
        .map(|row| (row.policy_id, row.policy_hash))
        .collect();

    fx.install(&app).await;

    assert_eq!(fx.live_text(), live);
    let after: Vec<_> = fx
        .rows()
        .await
        .into_iter()
        .map(|row| (row.policy_id, row.policy_hash))
        .collect();
    assert_eq!(after, rows);
    assert!(!fx.allowed(&fx.state, "Withdraw", "Thing"));
}

#[tokio::test]
async fn reinstalling_one_app_keeps_another_apps_policies() {
    let fx = Fixture::new("two-apps").await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let first = format!("reinstall-first-{suffix}");
    let second = format!("reinstall-second-{suffix}");
    let widget_policies = "permit(principal, action, resource is Widget);\n\
        forbid(principal, action == Action::\"Delete\", resource is Widget);\n";
    fx.write_app(&first, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.write_app(&second, &[("widget.cedar", widget_policies)]);
    fx.install(&first).await;
    fx.install(&second).await;

    fx.write_app(&first, &[("thing.cedar", PERMIT_THINGS)]);
    fx.install(&first).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(fx.allowed(state, "Withdraw", "Thing"));
        assert!(fx.allowed(state, "Rename", "Widget"));
        assert!(
            !fx.allowed(state, "Delete", "Widget"),
            "other app's forbid lost"
        );
    }
}

#[tokio::test]
async fn reinstall_drops_a_removed_policy_file() {
    let fx = Fixture::new("removed-file").await;
    let app = format!("reinstall-removed-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(
        &app,
        &[
            ("thing.cedar", PERMIT_THINGS),
            ("freeze.cedar", FORBID_WITHDRAW),
        ],
    );
    fx.install(&app).await;
    assert!(!fx.allowed(&fx.state, "Withdraw", "Thing"));

    fx.write_app(&app, &[("thing.cedar", PERMIT_THINGS)]);
    fx.install(&app).await;

    let rows = fx.rows().await;
    assert!(
        rows.iter().all(|row| !row.policy_id.ends_with("-freeze")),
        "removed file's row kept: {rows:?}"
    );
    for state in [&fx.state, &fx.restart().await] {
        assert!(
            fx.allowed(state, "Withdraw", "Thing"),
            "removed file's forbid kept"
        );
    }
}

// SECTION: statement-level ownership, layout and write order

/// Forwards to the real store but refuses to rewrite the `primary` row.
struct PrimaryRewriteFails(Arc<dyn PolicyStore>);

#[async_trait::async_trait]
impl PolicyStore for PrimaryRewriteFails {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.0
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.0.load_policies_for_tenant(tenant).await
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.0.load_all_policies().await
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.0
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        if policy_id == "primary" {
            return Err("injected failure rewriting primary".to_string());
        }
        self.0
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        policy_id: &str,
        expected_hash: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.0
            .replace_policy_if_hash(tenant, policy_id, expected_hash, cedar_text, created_by)
            .await
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.0.delete_policy(tenant, policy_id).await
    }
}

#[tokio::test]
async fn another_owners_multi_statement_row_keeps_its_forbid() {
    let fx = Fixture::new("shared-statement").await;
    let app = format!("reinstall-shared-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    // A hand-added row that carries the app's old file, including its forbid,
    // among other statements of its own.
    let guard_row = format!(
        "{APPROVED_PING}\n{}",
        thing_policies(FORBID_WITHDRAW).trim()
    );
    let live = format!("{}\n{guard_row}", fx.live_text());
    fx.add_active_row("manual-withdraw-guard", &guard_row, &live)
        .await;

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(
            !fx.allowed(state, "Withdraw", "Thing"),
            "another owner's forbid was dropped"
        );
        assert!(fx.allowed(state, "Ping", "Gadget"));
        assert!(!fx.allowed(state, "Delete", "Thing"));
    }
}

#[tokio::test]
async fn reformatted_primary_copy_is_removed() {
    let fx = Fixture::new("reformatted-primary").await;
    let app = format!("reinstall-reformatted-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    let snapshot =
        format!("{PERMIT_THINGS}\r\n{FORBID_WITHDRAW_REFORMATTED}\r\n{HAND_ADDED_POKE}\r\n");
    fx.add_active_row("primary", &snapshot, &snapshot).await;
    assert!(!fx.allowed(&fx.state, "Withdraw", "Thing"));

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(
            fx.allowed(state, "Withdraw", "Thing"),
            "reformatted old forbid survived"
        );
        assert!(!fx.allowed(state, "Delete", "Thing"));
        assert!(
            fx.allowed(state, "Poke", "Gadget"),
            "hand-added policy lost"
        );
    }
    let rows = fx.rows().await;
    let primary = rows.iter().find(|row| row.policy_id == "primary").unwrap();
    assert!(
        primary.cedar_text.contains("// frozen items"),
        "layout around it kept"
    );
    assert!(primary.cedar_text.contains(HAND_ADDED_POKE));
}

#[tokio::test]
async fn failed_primary_rewrite_leaves_the_previous_install() {
    let fx = Fixture::with_policy_store("primary-fails", |store| {
        Arc::new(PrimaryRewriteFails(store))
    })
    .await;
    let app = format!("reinstall-primary-fails-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    let snapshot = format!("{}\n{HAND_ADDED_POKE}", fx.live_text());
    fx.add_active_row("primary", &snapshot, &snapshot).await;
    let rows_before: Vec<_> = fx
        .rows()
        .await
        .into_iter()
        .map(|row| (row.policy_id, row.policy_hash))
        .collect();
    let legacy_before = fx.legacy_text().await;

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    let error = fx
        .try_install(&app)
        .await
        .expect_err("primary rewrite must fail");
    assert!(error.contains("primary"), "{error}");

    let rows_after: Vec<_> = fx
        .rows()
        .await
        .into_iter()
        .map(|row| (row.policy_id, row.policy_hash))
        .collect();
    assert_eq!(rows_after, rows_before, "rows changed by a failed install");
    assert_eq!(fx.legacy_text().await, legacy_before);
    assert_eq!(fx.live_text(), snapshot);
    for state in [&fx.state, &fx.restart().await] {
        assert!(
            !fx.allowed(state, "Withdraw", "Thing"),
            "previous install not intact"
        );
        assert!(fx.allowed(state, "Delete", "Thing"));
    }
}
