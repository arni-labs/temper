//! An install never moves a shared dependency backwards unless asked.
//!
//! Each scenario installs app A, which brings a shared dependency B, then app
//! C, which pins B at another version, through the production registry
//! installer (`install_genesis_app_from_registry`) against a fake registry
//! that serves the bundles and the compare route over HTTP:
//!
//! - C pins a newer B (an upgrade): B moves to C's pin.
//! - C pins an older B: B stays where A left it, in its record and in the
//!   spec the tenant runs, and C still installs.
//! - B is installed at a version unrelated to C's pin: C's install is refused
//!   before anything changes, naming B and both versions.
//! - The same, with B named in `downgrade_dependencies`: B moves to C's pin.
//!
//! Before the rule, C's install wrote B at C's pin in every case: the second
//! and third scenarios fail on that code.

mod common;

use base64::Engine as _;
use common::platform_harness::SimPlatformHarness;
use serde_json::{Value, json};
use temper_platform::genesis_install::{
    GenesisRegistryInstallRequest, install_genesis_app_from_registry,
};
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::PlatformStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OWNER: &str = "acme";

fn sha(pair: &str) -> String {
    pair.repeat(20)
}

/// The spec of the shared dependency at one version. Each version adds a
/// differently named action, so the spec the tenant runs shows its version.
fn shared_spec(marker: &str) -> String {
    format!(
        r#"[automaton]
name = "SharedThing"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
from = ["Open"]
to = "Closed"

[[action]]
name = "Mark{marker}"
from = ["Open"]
to = "Open"
"#
    )
}

fn plain_spec(entity: &str) -> String {
    format!(
        r#"[automaton]
name = "{entity}"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
from = ["Open"]
to = "Closed"
"#
    )
}

fn csdl(entity: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.{entity}" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="{entity}">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="{entity}s" EntityType="Test.{entity}.{entity}"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#
    )
}

/// One app of a bundle, with its manifest, guide, spec and CSDL.
fn bundle_app(name: &str, version: &str, deps: &[&str], entity: &str, spec: &str) -> Value {
    let deps = deps
        .iter()
        .map(|dep| format!("\"{dep}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let file = |path: &str, text: &str| {
        json!({
            "path": path,
            "content_base64": base64::engine::general_purpose::STANDARD.encode(text),
        })
    };
    json!({
        "owner": OWNER,
        "name": name,
        "version_hash": version,
        "files": [
            file(
                "app.toml",
                &format!("name = \"{name}\"\nversion = \"0.1.0\"\ndependencies = [{deps}]\n"),
            ),
            file("APP.md", &format!("# {name}\n\nTest app.\n")),
            file(&format!("specs/{}.ioa.toml", entity.to_lowercase()), spec),
            file("specs/model.csdl.xml", &csdl(entity)),
        ],
    })
}

/// One scenario's apps: a root `{prefix}-a` and `{prefix}-c`, each depending on
/// `{prefix}-shared`.
struct Scenario {
    prefix: &'static str,
    tenant: String,
}

impl Scenario {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            tenant: format!("deps-{prefix}"),
        }
    }

    fn name(&self, app: &str) -> String {
        format!("{}-{app}", self.prefix)
    }

    /// Serve the bundle of root `app` at `version`, pinning shared at `shared`.
    async fn serve_root(&self, registry: &MockServer, app: &str, version: &str, shared: &str) {
        let root = self.name(app);
        let shared_name = self.name("shared");
        let entity = format!("{}Thing", app.to_uppercase());
        let bundle = json!({
            "app_ref": format!("{OWNER}/{root}@{version}"),
            "registry_tenant": "default",
            "apps": [
                bundle_app(&root, version, &[&shared_name], &entity, &plain_spec(&entity)),
                bundle_app(&shared_name, shared, &[], "SharedThing", &shared_spec(&shared[..4])),
            ],
        });
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/{OWNER}/{root}/versions/{version}/bundle"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(bundle))
            .mount(registry)
            .await;
    }

    /// Answer the compare route for shared `{base}...{head}`.
    async fn serve_compare(&self, registry: &MockServer, base: &str, head: &str, status: &str) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/{OWNER}/{}/compare/{base}...{head}",
                self.name("shared")
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": status })))
            .mount(registry)
            .await;
    }

    async fn install(
        &self,
        harness: &SimPlatformHarness,
        registry: &MockServer,
        app: &str,
        version: &str,
        downgrade: &[String],
    ) -> Result<Value, String> {
        let request: GenesisRegistryInstallRequest = serde_json::from_value(json!({
            "tenant": self.tenant,
            "app_ref": format!("{OWNER}/{}@{version}", self.name(app)),
            "registry_url": registry.uri(),
            "downgrade_dependencies": downgrade,
        }))
        .expect("install request");
        install_genesis_app_from_registry(&harness.platform_state, request)
            .await
            .map(|result| serde_json::to_value(result).expect("result serializes"))
    }

    /// The shared dependency's install record version and the spec it runs.
    async fn shared_state(&self, harness: &SimPlatformHarness) -> (String, String) {
        let record = harness
            .sim_platform_store
            .get_installed_app(&self.tenant, &self.name("shared"))
            .await
            .expect("read shared record")
            .expect("shared is installed");
        let spec = harness
            .platform_state
            .server
            .registry
            .read()
            .expect("registry lock") // ci-ok: infallible lock
            .get_spec(&TenantId::new(&self.tenant), "SharedThing")
            .map(|spec| spec.ioa_source.clone())
            .expect("shared spec registered");
        (record.version_hash, spec)
    }

    async fn installed(&self, harness: &SimPlatformHarness, app: &str) -> bool {
        harness
            .sim_platform_store
            .get_installed_app(&self.tenant, &self.name(app))
            .await
            .expect("read record")
            .is_some()
    }
}

/// Every scenario runs in one test: the app catalog is process-wide, so
/// scenarios use their own app names and run one after another.
#[tokio::test]
async fn an_install_keeps_a_newer_dependency_and_refuses_an_unrelated_one() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(7);
    let mut harness = SimPlatformHarness::no_faults(7);
    let cache = tempfile::tempdir().expect("cache dir");
    harness.platform_state.server.data_dir = cache.path().to_path_buf();
    let registry = MockServer::start().await;
    let (a1, c1) = (sha("a1"), sha("c1"));
    let (b1, b2, bx) = (sha("b1"), sha("b2"), sha("be"));

    // 1. Upgrade: A pins B1, C pins B2, which descends from B1.
    let up = Scenario::new("up");
    up.serve_root(&registry, "a", &a1, &b1).await;
    up.serve_root(&registry, "c", &c1, &b2).await;
    up.serve_compare(&registry, &b1, &b2, "ahead").await;
    up.install(&harness, &registry, "a", &a1, &[])
        .await
        .expect("install A");
    up.install(&harness, &registry, "c", &c1, &[])
        .await
        .expect("install C");
    let (version, spec) = up.shared_state(&harness).await;
    assert_eq!(
        version, b2,
        "an upgrade moves the dependency to the newer pin"
    );
    assert!(
        spec.contains("Markb2b2"),
        "the tenant runs B2's spec:\n{spec}"
    );

    // 2. Keep newer: A pins B2, C pins B1, an ancestor of B2.
    let keep = Scenario::new("keep");
    keep.serve_root(&registry, "a", &a1, &b2).await;
    keep.serve_root(&registry, "c", &c1, &b1).await;
    keep.serve_compare(&registry, &b2, &b1, "behind").await;
    keep.install(&harness, &registry, "a", &a1, &[])
        .await
        .expect("install A");
    let installed_c = keep
        .install(&harness, &registry, "c", &c1, &[])
        .await
        .expect("C installs; only the downgrade is skipped");
    let (version, spec) = keep.shared_state(&harness).await;
    assert_eq!(version, b2, "the newer installed dependency is kept");
    assert!(
        spec.contains("Markb2b2"),
        "the tenant still runs B2's spec:\n{spec}"
    );
    assert!(
        !spec.contains("Markb1b1"),
        "B1's spec was not reconciled:\n{spec}"
    );
    assert!(keep.installed(&harness, "c").await, "C itself installed");
    assert_eq!(
        installed_c["kept_dependencies"],
        json!([{
            "app": format!("{OWNER}/keep-shared"),
            "installed_version": b2,
            "pinned_version": b1,
        }]),
        "the install says what it kept"
    );

    // 3. Unrelated: A pins BX, C pins B1, and neither descends from the other.
    let unrelated = Scenario::new("unrel");
    unrelated.serve_root(&registry, "a", &a1, &bx).await;
    unrelated.serve_root(&registry, "c", &c1, &b1).await;
    unrelated
        .serve_compare(&registry, &bx, &b1, "diverged")
        .await;
    unrelated
        .install(&harness, &registry, "a", &a1, &[])
        .await
        .expect("install A");
    let before = unrelated.shared_state(&harness).await;
    let refusal = unrelated
        .install(&harness, &registry, "c", &c1, &[])
        .await
        .expect_err("an unrelated installed dependency refuses the install");
    assert_eq!(
        refusal,
        format!(
            "Genesis install of {OWNER}/unrel-c@{c1} refused before any change: dependency \
             {OWNER}/unrel-shared is installed at {bx} and the closure pins {b1}: neither \
             version descends from the other. Name the dependency in downgrade_dependencies \
             to move it to the pinned version."
        )
    );
    assert_eq!(
        unrelated.shared_state(&harness).await,
        before,
        "the refused install changed neither B's record nor its spec"
    );
    assert!(
        !unrelated.installed(&harness, "c").await,
        "the refused app was not installed"
    );

    // 4. Explicit downgrade: as 3, with B named.
    let named = Scenario::new("named");
    named.serve_root(&registry, "a", &a1, &bx).await;
    named.serve_root(&registry, "c", &c1, &b1).await;
    named
        .install(&harness, &registry, "a", &a1, &[])
        .await
        .expect("install A");
    named
        .install(
            &harness,
            &registry,
            "c",
            &c1,
            &[format!("{OWNER}/named-shared")],
        )
        .await
        .expect("a named dependency moves to the pin");
    let (version, spec) = named.shared_state(&harness).await;
    assert_eq!(version, b1, "the named dependency moved to C's pin");
    assert!(
        spec.contains("Markb1b1"),
        "the tenant runs B1's spec:\n{spec}"
    );
}
