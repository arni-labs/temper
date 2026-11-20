//! A reaction a new version adds is never authorized by the policy it
//! replaces: while the install waits on storage, a record moved meets either
//! the old spec or the new spec under the new policy, never the new spec
//! under the old policy.
use super::*;

const POLICY_CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Order" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Order.Job"/><EntitySet Name="Tallies" EntityType="Order.Tally"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// A Job that a Poke moves on; `reacts` adds the reaction that counts the
/// Poke on the Tally of the same id.
fn job_spec(reacts: bool) -> String {
    let mut spec = String::from(
        r#"
[automaton]
name = "Job"
states = ["Waiting", "Poked"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Poked"]
[[action]]
name = "Poke"
kind = "input"
from = ["Waiting"]
to = "Poked"
"#,
    );
    if reacts {
        spec.push_str(
            r#"
[[action.triggers]]
name = "poke_counts_on_tally"
kind = "entity"
target_entity = "Tally"
target_action = "Count"
resolve_target = { kind = "same_id" }
"#,
        );
    }
    spec
}

const TALLY_SPEC: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Counted"]
initial = "Open"
allow_indefinite_states = ["Open", "Counted"]
[[action]]
name = "Count"
kind = "input"
from = ["Open"]
to = "Counted"
"#;

/// The version before allows everything; the version after adds the
/// reaction and forbids the move it makes.
const ALLOW_ALL: &str = "permit(principal, action, resource);";
const FORBID_COUNT: &str = "permit(principal, action, resource);\nforbid(principal, action == Action::\"Count\", resource);";

/// Write one version of the app.
fn write_policy_version(dir: &std::path::Path, app: &str, reacts: bool) {
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::create_dir_all(dir.join("policies")).expect("create policies dir");
    let policy = if reacts { FORBID_COUNT } else { ALLOW_ALL };
    fs::write(dir.join("policies/policy.cedar"), policy).expect("write policy");
    fs::write(
        dir.join("app.toml"),
        format!("name = \"{app}\"\nversion = \"0.1.0\"\n"),
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Policy first").expect("write APP.md");
    fs::write(dir.join("specs/model.csdl.xml"), POLICY_CSDL).expect("write csdl");
    fs::write(dir.join("specs/job.ioa.toml"), job_spec(reacts)).expect("write job spec");
    fs::write(dir.join("specs/tally.ioa.toml"), TALLY_SPEC).expect("write tally spec");
}

/// Poke a new Job and say where the Tally of the same id ends: Counted only
/// when a reaction counted the Poke on it.
async fn poke_and_read_tally(state: &PlatformState, tenant: &TenantId, id: &str) -> String {
    for entity_type in ["Job", "Tally"] {
        state
            .server
            .get_or_create_tenant_entity(tenant, entity_type, id, serde_json::json!({}))
            .await
            .expect("create record");
    }
    let response = state
        .server
        .dispatch_tenant_action(
            tenant,
            "Job",
            id,
            "Poke",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch Poke");
    assert!(response.success, "Poke failed: {:?}", response.error);
    state
        .server
        .get_tenant_entity_state(tenant, "Tally", id)
        .await
        .expect("read tally")
        .state
        .status
}

/// Install the version that adds the reaction and forbids its move over the
/// one that allows everything, poking a new Job at every storage call the
/// install waits on: no Poke may count on its Tally.
#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_a_new_version_adds_never_runs_under_the_policy_it_replaces() {
    let name = "install-order-policy";
    let plan = OsAppInstallPlan {
        specs: true,
        policies: true,
        wasm: false,
        content: false,
        seed: false,
    };
    let root = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
    let dir = root.join(name);
    write_policy_version(&dir, name, false);
    let store = TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
        .await
        .expect("open turso store");
    let mut state = PlatformState::new(None);
    state.server.data_dir = root.join("data");
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(store.clone()));
    let tenant = TenantId::new(name);
    state
        .server
        .start_runtime()
        .await
        .expect("first start")
        .await
        .expect("boot walk");
    add_os_apps_dir(root.clone());
    install_os_app_with_plan(&state, tenant.as_str(), name, plan, InstallOrigin::Request)
        .await
        .expect("install the version before");
    assert_eq!(poke_and_read_tally(&state, &tenant, "before").await, "Open");

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut stack = state
        .server
        .storage_stack
        .as_ref()
        .expect("storage stack")
        .as_ref()
        .clone();
    let paused = Arc::new(PauseInstallStorage {
        inner: stack.turso.take().expect("turso provider"),
        platform: stack.platform.take().expect("platform store"),
        tenant: tenant.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });
    stack.turso = Some(paused.clone());
    stack.platform = Some(paused);
    state.server.set_storage_stack(stack);
    write_policy_version(&dir, name, true);
    let install = INSTALLER.scope(
        true,
        install_os_app_with_plan(&state, tenant.as_str(), name, plan, InstallOrigin::Request),
    );
    tokio::pin!(install);
    let mut waits = 0;
    loop {
        tokio::select! {
            _ = entered.notified() => {},
            result = &mut install => {
                result.expect("install the version after");
                break;
            }
        }
        waits += 1;
        let tally = poke_and_read_tally(&state, &tenant, &format!("during-{waits}")).await;
        assert_eq!(
            tally, "Open",
            "a Poke while the install waited on storage ({waits}) was counted by the reaction \
             the new version adds, under the policy that version replaces"
        );
        release.add_permits(1);
    }
    assert!(
        waits > 0,
        "the install never waited on storage, so nothing was tested"
    );
    assert_eq!(
        poke_and_read_tally(&state, &tenant, "after").await,
        "Open",
        "the new version's policy forbids the reaction's move"
    );
    fs::remove_dir_all(root).expect("remove temp dir");
}
