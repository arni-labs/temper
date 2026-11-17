//! Installing an app installs that app, and a dependency only when no version
//! of it is installed.

use std::sync::Arc;

use temper_server::platform_store::{PlatformStore, SimPlatformFaultConfig, SimPlatformStore};

use super::*;

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";

fn server_on(data_dir: &Path, ps: &Arc<SimPlatformStore>) -> PlatformState {
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.to_path_buf();
    platform
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(
            temper_store_sim::SimEventStore::no_faults(0),
            Some(ps.clone()),
        ));
    platform
}

/// `app` at `version` in `cache_root`, needing `dependencies`, as a source
/// writes it: its guide names the version.
fn written_app(
    cache_root: &Path,
    app: &str,
    version: &str,
    dependencies: &[&str],
) -> MaterializedApp {
    let dir = cache_root.join(app);
    std::fs::create_dir_all(&dir).expect("app dir");
    let dependencies: Vec<String> = dependencies
        .iter()
        .map(|name| format!("{name:?}"))
        .collect();
    std::fs::write(
        dir.join("app.toml"),
        format!(
            "name = {app:?}\ndependencies = [{}]\n",
            dependencies.join(", ")
        ),
    )
    .expect("app.toml");
    std::fs::write(
        dir.join("APP.md"),
        format!("# {app}\n\n{app} at {version}\n"),
    )
    .expect("APP.md");
    MaterializedApp {
        version: RegistryAppRef {
            owner: "acme".to_string(),
            name: app.to_string(),
            version_hash: Some(version.to_string()),
        },
        files: AppFileSource::written(dir, ["app.toml".to_string(), "APP.md".to_string()]),
    }
}

/// Install the closure `apps` from `cache_root`, the first app its root, as a
/// registry install does.
async fn install(
    platform: &PlatformState,
    tenant: &str,
    cache_root: &Path,
    apps: Vec<MaterializedApp>,
) -> ClosureInstalled {
    let root = apps[0].version.clone();
    let app_ref = format!(
        "acme/{}@{}",
        root.name,
        root.version_hash.as_deref().unwrap()
    );
    let installed = install_materialized_closure(
        platform,
        tenant,
        cache_root,
        MaterializedClosure {
            app_ref: &app_ref,
            root,
            apps,
            source: ClosureSource {
                source_kind: "genesis",
                registry_url: "http://127.0.0.1:9",
                registry_tenant: "default",
                follow_policy: "pinned",
            },
        },
    )
    .await;
    match installed {
        Ok(installed) => installed,
        Err(
            ClosureRefused::Unparsed(error)
            | ClosureRefused::Failed(error)
            | ClosureRefused::Inactive(error),
        ) => panic!("install of {app_ref} refused: {error}"),
    }
}

async fn recorded_ref(ps: &SimPlatformStore, tenant: &str, app: &str) -> String {
    ps.get_installed_app(tenant, app)
        .await
        .expect("read the install record")
        .unwrap_or_else(|| panic!("{app} has an install record"))
        .app_ref
}

fn guide(tenant: &str, app: &str) -> String {
    crate::os_apps::tenant_app_guide(tenant, app)
        .unwrap_or_else(|| panic!("the tenant resolves {app}"))
}

/// The 17 Nov 2026 shape: `tags` is installed at a newer version, then `notes`
/// is installed with a closure that pins `tags` at an older one. `notes`
/// installs and runs on the installed `tags`, whose version and record stay.
#[tokio::test]
async fn installing_an_app_keeps_an_installed_dependency_at_its_version() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(SimPlatformStore::new(0, SimPlatformFaultConfig::none()));
    let platform = server_on(data_dir.path(), &ps);
    let tenant = "closure-keeps-installed-dependency";

    let tags_cache = data_dir.path().join("tags-new");
    install(
        &platform,
        tenant,
        &tags_cache,
        vec![written_app(&tags_cache, "tags", NEW, &[])],
    )
    .await;
    let tags_before = ps
        .get_installed_app(tenant, "tags")
        .await
        .expect("read the install record")
        .expect("tags is installed");

    let notes_cache = data_dir.path().join("notes-old");
    let installed = install(
        &platform,
        tenant,
        &notes_cache,
        vec![
            written_app(&notes_cache, "notes", OLD, &["tags"]),
            written_app(&notes_cache, "tags", OLD, &[]),
        ],
    )
    .await;

    assert_eq!(
        recorded_ref(&ps, tenant, "notes").await,
        format!("acme/notes@{OLD}")
    );
    assert!(guide(tenant, "notes").contains(&format!("notes at {OLD}")));
    let tags_after = ps
        .get_installed_app(tenant, "tags")
        .await
        .expect("read the install record")
        .expect("tags is still installed");
    assert_eq!(tags_after.app_ref, format!("acme/tags@{NEW}"));
    assert_eq!(tags_after.bundle_digest, tags_before.bundle_digest);
    assert!(
        guide(tenant, "tags").contains(&format!("tags at {NEW}")),
        "the tenant still resolves the installed tags: {}",
        guide(tenant, "tags")
    );
    assert!(
        installed.materialized.contains(&"tags".to_string()),
        "notes still runs on its dependency: {:?}",
        installed.materialized
    );
}

/// A dependency no version of which is installed comes with the app, at the
/// version its closure pins.
#[tokio::test]
async fn installing_an_app_brings_a_dependency_that_is_not_installed() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(SimPlatformStore::new(0, SimPlatformFaultConfig::none()));
    let platform = server_on(data_dir.path(), &ps);
    let tenant = "closure-brings-missing-dependency";

    let notes_cache = data_dir.path().join("notes-old");
    install(
        &platform,
        tenant,
        &notes_cache,
        vec![
            written_app(&notes_cache, "notes", OLD, &["tags"]),
            written_app(&notes_cache, "tags", OLD, &[]),
        ],
    )
    .await;

    assert_eq!(
        recorded_ref(&ps, tenant, "notes").await,
        format!("acme/notes@{OLD}")
    );
    assert_eq!(
        recorded_ref(&ps, tenant, "tags").await,
        format!("acme/tags@{OLD}")
    );
    assert!(guide(tenant, "tags").contains(&format!("tags at {OLD}")));
}
