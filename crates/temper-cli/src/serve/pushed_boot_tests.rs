//! Boot and pushed app versions: an `--app` with no files waits for its first
//! install, and an `--app` served from the boot catalog with no pushed version
//! is adopted as its first one, which every later boot serves.

use std::path::Path;

use temper_platform::os_apps::{add_os_apps_dir, remove_tenant_os_apps_dir};
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;

use super::bootstrap::{bootstrap_installed_apps, missing_app_line};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const TENANT: &str = "default";

fn write_app(apps_root: &Path, app: &str, spec: &str) {
    let app_dir = apps_root.join(app);
    std::fs::create_dir_all(app_dir.join("specs")).expect("create specs dir"); // determinism-ok: test fixture
    let files = [
        (
            "app.toml",
            format!("name = \"{app}\"\ndescription = \"Boot test app\"\nversion = \"0.1.0\"\n"),
        ),
        ("APP.md", format!("# {app}\n\nBoot test app.\n")),
        ("specs/order.ioa.toml", spec.to_string()),
        ("specs/model.csdl.xml", ORDER_CSDL.to_string()),
    ];
    for (path, text) in files {
        std::fs::write(app_dir.join(path), text).expect("write app file"); // determinism-ok: test fixture
    }
}

fn server(data_dir: &Path) -> PlatformState {
    let mut state = PlatformState::new(None);
    state.server.data_dir = data_dir.to_path_buf();
    state
}

fn registered_order(state: &PlatformState) -> Option<String> {
    state
        .registry
        .read()
        .expect("registry")
        .get_spec(&TenantId::new(TENANT), "Order")
        .map(|spec| spec.ioa_source.clone())
}

#[tokio::test]
async fn an_app_with_no_files_anywhere_waits_and_boot_goes_on() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let app = format!("waits-{}", temper_runtime::scheduler::sim_uuid().simple());
    let state = server(data_dir.path());

    bootstrap_installed_apps(&state, std::slice::from_ref(&app))
        .await
        .expect("an app with no files does not stop the boot");

    assert!(registered_order(&state).is_none());
    assert!(
        !data_dir
            .path()
            .join("pushed-apps")
            .join(TENANT)
            .join(&app)
            .exists(),
        "nothing is adopted for an app with no files"
    );
}

#[tokio::test]
async fn a_catalog_app_is_adopted_and_served_on_later_boots_whatever_the_catalog_holds() {
    let id = temper_runtime::scheduler::sim_uuid();
    let app = format!("adopted-{}", id.simple());
    let apps_root = tempfile::tempdir().expect("apps root");
    let data_dir = tempfile::tempdir().expect("data dir");
    let served = format!("{ORDER_IOA}\n# the version served before pushes\n");
    write_app(apps_root.path(), &app, &served);
    let commit = "0123456789abcdef0123456789abcdef01234567\n";
    std::fs::write(apps_root.path().join(&app).join("COMMIT"), commit).expect("write COMMIT"); // determinism-ok: test fixture
    add_os_apps_dir(apps_root.path().to_path_buf());

    let first = server(data_dir.path());
    bootstrap_installed_apps(&first, std::slice::from_ref(&app))
        .await
        .expect("first boot");
    assert_eq!(registered_order(&first).as_deref(), Some(served.as_str()));
    let home = data_dir.path().join("pushed-apps").join(TENANT).join(&app);
    let adopted =
        std::fs::read_to_string(home.join("current")).expect("the served version is adopted"); // determinism-ok: test reads the server's files
    let root = home.join(&adopted);
    assert_eq!(
        std::fs::read_to_string(root.join(&app).join("specs/order.ioa.toml"))
            .expect("adopted spec"), // determinism-ok: test reads the server's files
        served
    );
    assert_eq!(
        std::fs::read_to_string(root.join(&app).join("COMMIT")).expect("adopted COMMIT"), // determinism-ok: test reads the server's files
        commit,
        "the adopted version names the catalog's commit"
    );

    // The catalog moves on; a restart (a new process, so no tenant installs
    // until boot restores them) still serves the adopted version, and does
    // not adopt again.
    write_app(
        apps_root.path(),
        &app,
        &format!("{ORDER_IOA}\n# a later catalog\n"),
    );
    remove_tenant_os_apps_dir(TENANT, &root);
    let second = server(data_dir.path());
    bootstrap_installed_apps(&second, std::slice::from_ref(&app))
        .await
        .expect("second boot");
    assert_eq!(registered_order(&second).as_deref(), Some(served.as_str()));
    assert_eq!(
        std::fs::read_to_string(home.join("current")).expect("current"), // determinism-ok: test reads the server's files
        adopted
    );

    remove_tenant_os_apps_dir(TENANT, &root);
}

/// With no files for an `--app`, boot says it waits only when it serves
/// nothing for the app; an app whose install it stored serves those specs.
#[test]
fn a_missing_app_waits_only_when_nothing_was_stored_for_it() {
    assert_eq!(
        missing_app_line("shop", TENANT, false),
        "  App 'shop' waits for its first install for 'default'"
    );
    assert_eq!(
        missing_app_line("shop", TENANT, true),
        "  App 'shop' serves its stored specs for 'default'; no files to install"
    );
}
