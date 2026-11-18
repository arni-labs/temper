//! Installs of one app into one tenant at once, from a registry.

use base64::Engine as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::source_and_cache::{DEP, dependency_files};
use super::*;
use crate::genesis_install::{GenesisRegistryInstallRequest, install_genesis_app_from_registry};
use crate::os_apps::tenant_os_app_dir;

const HASH: &str = "89abcdef0123456789abcdef0123456789abcdef";
const INSTALLS_AT_ONCE: usize = 4;
const ROUNDS: usize = 3;

/// Two agents, or an agent and a retry, can install the same app into the
/// same tenant at once. Both installs share the tenant's directory for that
/// app, so each must publish its files there without moving them out from
/// under the other: every install succeeds and the app's files are there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn installs_of_one_app_into_one_tenant_at_once_all_install_it() {
    let tenant = unique_tenant("same-app-at-once");
    let server = server(&tenant);
    let registry = MockServer::start().await;
    let files: Vec<_> = dependency_files()
        .into_iter()
        .map(|(path, text)| {
            serde_json::json!({
                "path": path,
                "content_base64": base64::engine::general_purpose::STANDARD.encode(text),
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/genesis/apps/acme/{DEP}/versions/{HASH}/bundle"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "app_ref": format!("acme/{DEP}@{HASH}"), "registry_tenant": "default",
            "apps": [{"owner": "acme", "name": DEP, "version_hash": HASH, "files": files}]
        })))
        .mount(&registry)
        .await;
    // SAFETY: these HTTP regressions run with nextest's process isolation,
    // as required by the existing registry-install tests.
    unsafe { std::env::set_var("TEMPER_GENESIS_REGISTRY_URL", registry.uri()) };

    for round in 0..ROUNDS {
        let installs: Vec<_> = (0..INSTALLS_AT_ONCE)
            .map(|_| {
                let state = server.state.clone();
                let request = GenesisRegistryInstallRequest {
                    tenant: tenant.clone(),
                    app_ref: format!("acme/{DEP}@{HASH}"),
                    registry_url: registry.uri(),
                    registry_tenant: "default".into(),
                    follow_policy: "pinned".into(),
                };
                tokio::spawn(
                    async move { install_genesis_app_from_registry(&state, request).await },
                )
            })
            .collect();
        for install in installs {
            if let Err(error) = install.await.expect("install task") {
                panic!("round {round}: an install failed: {error}");
            }
        }
        let dir = tenant_os_app_dir(&tenant, DEP).expect("the app is in the tenant's catalog");
        let manifest = std::fs::read_to_string(dir.join("app.toml")).expect("its manifest");
        assert_eq!(manifest, dependency_files()["app.toml"], "round {round}");
        let cache_base = dir
            .parent()
            .and_then(Path::parent)
            .expect("the cache base holds the install's directory");
        let left: Vec<_> = std::fs::read_dir(cache_base)
            .expect("cache base")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(
            left.is_empty(),
            "round {round}: an install leaves nothing beside its directory: {left:?}"
        );
    }
}
