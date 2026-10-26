//! A new version of a spec may change anything, so long as it leaves no
//! stored record with no way forward: in a state it drops, or in a state
//! where no move is possible for the record any more. A `[[carry]]` says where
//! such records go; the install tries each carry before writing anything and
//! makes it after the swap. The records are the type's real stored ones.

use super::*;
use std::path::PathBuf;

use temper_store_turso::TursoEventStore;

/// A place an app's Temper is brought to, after `KatagamiAppPlace` before
/// katagami #1130.
const PLACE: &str = r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Placed"]
initial = "Unplaced"
allow_indefinite_states = ["Unplaced", "Bringing", "Placed"]

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced", "Bringing", "Placed"]
to = "Bringing"
params = [{ name = "commit", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "commit"

[[action]]
name = "Placed"
kind = "input"
from = ["Bringing"]
to = "Placed"
"#;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Places" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Place"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Places" EntityType="Places.Place"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// The place whose last state is called Settled, not Placed.
fn settles() -> String {
    PLACE
        .replace("\"Bringing\", \"Placed\"]", "\"Bringing\", \"Settled\"]")
        .replace("to = \"Placed\"", "to = \"Settled\"")
}

/// Settled, for a place brought at least once, counted.
fn settles_once_brought() -> String {
    settles()
        .replace(
            "[[action]]\nname = \"Bring\"",
            "[[state]]\nname = \"brought\"\ntype = \"counter\"\ninitial = 0\n\n\
             [[invariant]]\nname = \"SettledWasBrought\"\nassert = \"status == 'Settled' => brought > 0\"\n\n\
             [[action]]\nname = \"Bring\"",
        )
        .replace(
            "to = \"Bringing\"\n",
            "to = \"Bringing\"\neffect = [\"brought += 1\"]\n",
        )
}

/// The place with a flag set before it may be brought: `ready`, false for a
/// place stored before it. With `ready_move`, a `Ready` move sets it.
fn needs_ready(ready_move: bool) -> String {
    let ready = if ready_move {
        "[[action]]\nname = \"Ready\"\nkind = \"input\"\nfrom = [\"Unplaced\", \"Placed\"]\neffect = [\"ready = true\"]\n\n"
    } else {
        ""
    };
    PLACE
        .replace(
            "[[action]]\nname = \"Bring\"",
            &format!(
                "[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = false\n\n{ready}\
                 [[action]]\nname = \"Bring\""
            ),
        )
        .replace(
            "to = \"Bringing\"\n",
            "to = \"Bringing\"\nguard = \"ready\"\n",
        )
}

/// katagami #1130's change: each Bring carries a count higher than the last.
fn counts_brings() -> String {
    PLACE
        .replace(
            "[[action]]\nname = \"Bring\"",
            "[[state]]\nname = \"brought\"\ntype = \"counter\"\ninitial = 0\n\n[[action]]\nname = \"Bring\"",
        )
        .replace(
            "params = [{ name = \"commit\", type = \"string\" }]",
            "params = [{ name = \"commit\", type = \"string\" }, { name = \"brought\", type = \"counter\" }]\n\
             effect = [\"brought = params.brought\"]\n\n\
             [[action.constraints]]\nkind = \"param_greater_than_field\"\nparam = \"brought\"\nfield = \"brought\"",
        )
}

struct Fixture {
    state: PlatformState,
    tenant: TenantId,
    apps_root: PathBuf,
}

const APP: &str = "spec-change-places";

impl Fixture {
    async fn new(label: &str) -> Self {
        let id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("temper-spec-change-{label}-{id}"));
        fs::create_dir_all(&root).expect("create the test root");
        let store =
            TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
                .await
                .expect("open the store");
        let mut state = PlatformState::new(None);
        state.server.data_dir = root.join("data");
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_turso(store));
        Self {
            state,
            tenant: TenantId::new(format!("spec-change-{label}")),
            apps_root: root.join("apps"),
        }
    }

    /// Install the app with `spec` as its Place.
    async fn install(&self, spec: &str) -> Result<InstallResult, String> {
        let dir = self.apps_root.join(APP);
        fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        fs::create_dir_all(dir.join("policies")).expect("create policies dir");
        fs::write(
            dir.join("app.toml"),
            format!("name = \"{APP}\"\ndescription = \"Places\"\nversion = \"0.1.0\"\n"),
        )
        .expect("write app.toml");
        fs::write(dir.join("APP.md"), "# Places\n").expect("write APP.md");
        fs::write(
            dir.join("policies/allow.cedar"),
            "permit(principal, action, resource);",
        )
        .expect("write policy");
        fs::write(dir.join("specs/model.csdl.xml"), CSDL).expect("write CSDL");
        fs::write(dir.join("specs/place.ioa.toml"), spec).expect("write spec");
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, self.tenant.as_str(), APP).await
    }

    async fn act(&self, id: &str, action: &str, params: serde_json::Value) -> (bool, String) {
        let response = self
            .state
            .server
            .dispatch_tenant_action(
                &self.tenant,
                "Place",
                id,
                action,
                params,
                &AgentContext::system(),
            )
            .await
            .unwrap_or_else(|error| panic!("dispatch {action} on {id}: {error}"));
        (response.success, response.state.status)
    }

    async fn status(&self, id: &str) -> String {
        self.state
            .server
            .get_tenant_entity_state(&self.tenant, "Place", id)
            .await
            .unwrap_or_else(|error| panic!("read {id}: {error}"))
            .state
            .status
    }

    async fn create(&self, id: &str) {
        self.state
            .server
            .get_or_create_tenant_entity(&self.tenant, "Place", id, serde_json::json!({}))
            .await
            .expect("create the place");
    }

    /// A place stored under the first version, brought to a commit and placed.
    async fn placed(&self, id: &str) {
        self.create(id).await;
        let brought = self
            .act(id, "Bring", serde_json::json!({ "commit": "c1" }))
            .await;
        assert_eq!(brought, (true, "Bringing".to_string()));
        let placed = self.act(id, "Placed", serde_json::json!({})).await;
        assert_eq!(placed, (true, "Placed".to_string()));
    }
}

/// What the check holds, from the refusing side: a version that would leave a
/// stored record in a state it drops is refused, nothing is swapped, and the
/// answer opens with the `[[carry]]` that would fix it.
#[tokio::test]
async fn a_dropped_state_that_holds_records_is_refused_with_the_carry_that_fixes_it() {
    let places = Fixture::new("dropped").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&settles())
        .await
        .expect_err("a version that drops Placed, where p1 is, with no carry is refused");
    let carry = refused
        .find("[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"")
        .unwrap_or_else(|| panic!("the refusal gives the carry to Settled: {refused}"));
    let record = refused
        .find("p1")
        .unwrap_or_else(|| panic!("the refusal names p1: {refused}"));
    assert!(carry < record, "the carry comes first: {refused}");

    // Nothing was swapped: p1 moves under the version it was stored with.
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_carry_moves_stored_records_out_of_a_dropped_state() {
    let places = Fixture::new("carried").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places.create("p2").await;

    let installed = places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
            settles()
        ))
        .await
        .expect("a version that says where Placed records go is installed");
    assert!(installed.warnings.is_empty(), "{:?}", installed.warnings);
    assert_eq!(places.status("p1").await, "Settled");
    assert_eq!(places.status("p2").await, "Unplaced", "p2 was never Placed");
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_guard_that_leaves_records_no_move_is_refused_with_the_values_to_set() {
    let places = Fixture::new("guard").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&needs_ready(false))
        .await
        .expect_err("a version under which p1, Placed, can make no move is refused");
    for words in [
        "[[carry]]\nfrom = \"Placed\"\nset = { ready = true }",
        "p1",
        "Bring",
    ] {
        assert!(
            refused.contains(words),
            "the refusal has {words}: {refused}"
        );
    }
}

#[tokio::test]
async fn a_carry_that_sets_what_a_guard_wants_is_installed_and_the_record_moves() {
    let places = Fixture::new("set").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nset = {{ ready = true }}\n",
            needs_ready(false)
        ))
        .await
        .expect("a version that carries Placed records with ready set is installed");
    assert_eq!(places.status("p1").await, "Placed");
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// A version may narrow a move, so long as the records keep another: it is
/// installed and the install reports it.
#[tokio::test]
async fn a_narrowed_move_with_another_way_out_is_installed_and_reported() {
    let places = Fixture::new("narrowed").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let installed = places
        .install(&needs_ready(true))
        .await
        .expect("p1 can still be made Ready, so the version is installed");
    assert!(
        installed
            .warnings
            .iter()
            .any(|warning| warning.check == "spec_change"
                && warning.message.contains("Bring")
                && warning.message.contains("p1")),
        "{:?}",
        installed.warnings
    );
    let refused = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(refused, (false, "Placed".to_string()), "p1 waits for Ready");
    assert_eq!(
        places.act("p1", "Ready", serde_json::json!({})).await,
        (true, "Placed".to_string())
    );
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_carry_that_would_break_an_invariant_is_refused_and_nothing_is_written() {
    let places = Fixture::new("invariant").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
            settles_once_brought()
        ))
        .await
        .expect_err("p1 was brought before brought was counted, so Settled would break the rule");
    for words in ["SettledWasBrought", "p1"] {
        assert!(
            refused.contains(words),
            "the refusal has {words}: {refused}"
        );
    }
    assert_eq!(places.status("p1").await, "Placed");
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// katagami #1130's shape: a new counter, and a constraint on an existing
/// action that reads it. A missing declared counter reads as its initial
/// value (temper #167), so the stored place can still Bring.
#[tokio::test]
async fn a_new_counter_with_a_constraint_is_installed_and_stored_records_still_move() {
    let places = Fixture::new("counter").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    places
        .install(&counts_brings())
        .await
        .expect("the counter strands nothing");
    let brought = places
        .act(
            "p1",
            "Bring",
            serde_json::json!({ "commit": "c2", "brought": 1 }),
        )
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// Count-based: records the installed version already leaves with no way
/// forward are reported, and do not stop a change that strands no others.
#[tokio::test]
async fn records_already_stuck_are_reported_and_do_not_stop_an_install() {
    let places = Fixture::new("already").await;
    places
        .install(&needs_ready(false))
        .await
        .expect("first version");
    places.create("p1").await;
    assert_eq!(
        places
            .act("p1", "Bring", serde_json::json!({ "commit": "c1" }))
            .await,
        (false, "Unplaced".to_string()),
        "p1 is not ready, and nothing makes it so"
    );

    let installed = places
        .install(&needs_ready(false).replace(
            "from = [\"Unplaced\", \"Bringing\", \"Placed\"]",
            "from = [\"Unplaced\", \"Placed\"]",
        ))
        .await
        .expect("p1 had no way forward before this version either");
    assert!(
        installed
            .warnings
            .iter()
            .any(|warning| warning.message.contains("p1")),
        "{:?}",
        installed.warnings
    );
}

#[tokio::test]
async fn callers_cannot_make_a_carry_move() {
    let places = Fixture::new("caller").await;
    places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nset = {{ ready = true }}\n",
            needs_ready(false)
        ))
        .await
        .expect("first version");
    places.create("p1").await;
    let refused = places
        .state
        .server
        .dispatch_tenant_action(
            &places.tenant,
            "Place",
            "p1",
            "carry:Placed:1",
            serde_json::json!({ "ready": true }),
            &AgentContext::system(),
        )
        .await;
    match refused {
        Err(error) => assert!(error.contains("[[carry]]"), "{error}"),
        Ok(response) => panic!("a caller made a carry move: {response:?}"),
    }
}
