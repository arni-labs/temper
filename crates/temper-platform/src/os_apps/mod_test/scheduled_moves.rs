//! A move that schedules two moves to fire together, whose guards no record
//! state meets at once, has one of them refused every time it runs; on a
//! repeating state timer that is a refusal every round. An install of a new
//! spec version refuses it. A spec installed before keeps loading.

use super::*;

/// Waiting's timer makes A every minute, and A schedules B and C to fire
/// together. B needs `ready && approved`, C needs `!ready`: no record meets
/// both, so one of them is refused every time.
const ITEM: &str = r#"
[automaton]
name = "Item"
states = ["Waiting", "Done"]
initial = "Waiting"
allow_indefinite_states = ["Done"]

[[state]]
name = "ready"
type = "bool"
initial = false

[[state]]
name = "approved"
type = "bool"
initial = false

[[state]]
name = "tries"
type = "counter"
initial = 0

[[state]]
name = "limit"
type = "counter"
initial = 0

[[action]]
name = "MarkReady"
kind = "input"
from = ["Waiting"]
effect = ["ready = true"]

[[action]]
name = "Approve"
kind = "input"
from = ["Waiting"]
effect = ["approved = true"]

[[action]]
name = "A"
from = ["Waiting"]
effect = ["schedule('B', 10)", "schedule('C', 10)"]

[[action]]
name = "B"
from = ["Waiting"]
to = "Done"
guard = "ready && approved"

[[action]]
name = "C"
from = ["Waiting"]
guard = "!ready"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "A"
"#;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Items" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Item"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Items" EntityType="Items.Item"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// The item with C's guard replaced by `guard`.
fn item_with_c_guard(guard: &str) -> String {
    ITEM.replace(
        "name = \"C\"\nfrom = [\"Waiting\"]\nguard = \"!ready\"",
        &format!("name = \"C\"\nfrom = [\"Waiting\"]\nguard = \"{guard}\""),
    )
}

/// Write an app of `item` under a root of its own; returns the root and the
/// app's name.
fn write_app(label: &str, item: &str) -> (std::path::PathBuf, String) {
    let id = uuid::Uuid::new_v4().simple();
    let app = format!("scheduled-moves-{label}-{id}");
    let apps_root = std::env::temp_dir().join(format!("temper-{app}"));
    let dir = apps_root.join(&app);
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::write(
        dir.join("app.toml"),
        format!("name = \"{app}\"\ndescription = \"Items\"\nversion = \"0.1.0\"\n"),
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Items\n").expect("write APP.md");
    fs::write(dir.join("specs/model.csdl.xml"), CSDL).expect("write CSDL");
    fs::write(dir.join("specs/item.ioa.toml"), item).expect("write the item spec");
    (apps_root, app)
}

/// Install an app of `item` into a fresh tenant.
async fn install(
    label: &str,
    item: &str,
) -> (PlatformState, String, Result<InstallResult, String>) {
    let (apps_root, app) = write_app(label, item);
    let state = PlatformState::new(None);
    let tenant = format!("items-{label}-{}", uuid::Uuid::new_v4().simple());
    // Other tests reload the global catalog; register right before use.
    add_os_apps_dir(apps_root);
    let result = install_os_app(&state, &tenant, &app).await;
    (state, tenant, result)
}

fn scheduled_warnings(installed: &InstallResult) -> Vec<&InstallWarning> {
    installed
        .warnings
        .iter()
        .filter(|warning| warning.check == "scheduled_moves")
        .collect()
}

#[tokio::test]
async fn a_new_spec_whose_move_always_schedules_a_refused_move_is_refused() {
    let (state, tenant, result) = install("refused", ITEM).await;
    let err = result.expect_err("one of B and C is refused every time A runs");
    assert!(
        ["Item", "A", "B", "C"]
            .iter()
            .all(|named| err.contains(named)),
        "{err}"
    );
    let registry = state.registry.read().unwrap();
    assert!(
        registry.get_spec(&TenantId::new(&tenant), "Item").is_none(),
        "a refused install registers nothing"
    );
}

#[tokio::test]
async fn two_scheduled_moves_that_can_both_be_made_install() {
    let (_, _, result) = install("both", &item_with_c_guard("approved")).await;
    let installed = result.expect("a record ready and approved may be made both B and C");
    assert_eq!(installed.added, ["Item"]);
    assert!(
        scheduled_warnings(&installed).is_empty(),
        "{:?}",
        installed.warnings
    );
}

#[tokio::test]
async fn one_scheduled_move_with_a_guard_installs() {
    // A schedules only B: a record not yet ready waits for the next round.
    let one = ITEM.replace(
        "effect = [\"schedule('B', 10)\", \"schedule('C', 10)\"]",
        "effect = [\"schedule('B', 10)\"]",
    );
    let (_, _, result) = install("one", &one).await;
    let installed = result.expect("a retry-later wait is not refused");
    assert!(
        scheduled_warnings(&installed).is_empty(),
        "{:?}",
        installed.warnings
    );
}

#[tokio::test]
async fn guards_the_check_cannot_decide_warn_and_install() {
    // Whether `tries < limit` can hold beside `ready && approved` turns on
    // two counters compared with each other, which the check does not read.
    let (_, _, result) = install("undecided", &item_with_c_guard("tries < limit")).await;
    let installed = result.expect("a guard the check cannot decide does not refuse");
    let found = scheduled_warnings(&installed);
    let [warning] = found.as_slice() else {
        panic!("one pair is undecided: {:?}", installed.warnings);
    };
    assert!(
        ["Item", "A", "B", "C", "tries < limit"]
            .iter()
            .all(|named| warning.message.contains(named)),
        "{}",
        warning.message
    );
}

#[tokio::test]
async fn a_spec_installed_before_the_check_still_loads() {
    use temper_server::registry::SpecRegistry;
    use temper_server::registry_bootstrap::restore_registry_from_turso;
    use temper_store_turso::TursoEventStore;

    let db_url = format!(
        "file:/tmp/temper-scheduled-moves-{}.db",
        uuid::Uuid::new_v4()
    );
    let tenant = format!("items-reload-{}", uuid::Uuid::new_v4().simple());
    let (apps_root, app) = write_app("reload", ITEM);

    // The rows an install from before this check left: the Item spec,
    // committed, and the app recorded as installed.
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let hash = temper_store_turso::spec_content_hash(ITEM);
    store
        .upsert_spec(&tenant, "Item", ITEM, CSDL, &hash)
        .await
        .unwrap();
    store.commit_specs(&tenant).await.unwrap();
    store.record_installed_app(&tenant, &app).await.unwrap();

    // Boot: the registry is restored from the store, then the installed app
    // is replayed through the installer.
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let mut registry = SpecRegistry::new();
    let restored = restore_registry_from_turso(&mut registry, &store)
        .await
        .unwrap();
    assert!(restored > 0, "the stored Item spec restores");
    let mut state = PlatformState::with_registry(registry, None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store));
    add_os_apps_dir(apps_root);
    let installed = install_os_app(&state, &tenant, &app)
        .await
        .expect("an installed spec keeps loading");
    assert!(
        installed
            .skipped
            .iter()
            .chain(&installed.updated)
            .any(|entity_type| entity_type == "Item"),
        "{installed:?}"
    );
    let registry = state.registry.read().unwrap();
    let spec = registry
        .get_spec(&TenantId::new(&tenant), "Item")
        .expect("Item stays registered");
    assert_eq!(spec.ioa_source, ITEM);
}
