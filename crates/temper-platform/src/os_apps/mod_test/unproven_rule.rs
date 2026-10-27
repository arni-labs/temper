//! An install refuses a rule that breaks, unless the tenant runs that rule
//! now as it is: then the rule keeps running, the install says so among its
//! unproved rules, and only a change to it is refused.

use super::*;
use std::path::PathBuf;

/// A counter whose rule `rule` reads its count. It counts by one from zero,
/// so a rule that bounds it below two breaks.
fn counter(rule: &str) -> String {
    format!(
        r#"[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["count += 1"]

[[invariant]]
name = "Small"
assert = "{rule}"
"#
    )
}

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.UnprovenRule" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Counter">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="count" Type="Edm.Int64" Nullable="false" DefaultValue="0"/>
      </EntityType>
      <Action Name="Increment" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.UnprovenRule.Counter"/>
        <ReturnType Type="Temper.UnprovenRule.Counter"/>
      </Action>
      <EntityContainer Name="UnprovenRuleService">
        <EntitySet Name="Counters" EntityType="Temper.UnprovenRule.Counter"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

/// Lets anyone count. Without it nobody may send `Increment`, the count
/// stays at zero, and every rule below holds.
const PERMIT: &str = "permit(principal, action == Action::\"Increment\", resource is Counter);";

/// Breaks at a count of two.
const BELOW_TWO: &str = "count < 2";
/// Breaks at a count of one: the same rule, changed.
const BELOW_ONE: &str = "count < 1";

struct Fixture {
    state: PlatformState,
    apps_root: PathBuf,
    app: String,
}

impl Fixture {
    /// An app whose Counter's rule is `rule`, which lets anyone count.
    fn new(label: &str, rule: &str) -> Self {
        let id = uuid::Uuid::new_v4().simple();
        let app = format!("unproven-{label}-{id}");
        let apps_root = std::env::temp_dir().join(format!("temper-unproven-{label}-{id}"));
        let app_dir = apps_root.join(&app);
        fs::create_dir_all(app_dir.join("specs")).expect("create specs dir");
        fs::create_dir_all(app_dir.join("policies")).expect("create policies dir");
        fs::write(
            app_dir.join("app.toml"),
            format!(
                "name = \"{app}\"\ndescription = \"Unproven rule test app\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("write app.toml");
        fs::write(app_dir.join("APP.md"), format!("# {app}\n\nTest app.\n")).expect("write APP.md");
        fs::write(app_dir.join("specs/counter.ioa.toml"), counter(rule)).expect("write spec");
        fs::write(app_dir.join("specs/model.csdl.xml"), CSDL).expect("write CSDL");
        fs::write(app_dir.join("policies/app.cedar"), PERMIT).expect("write Cedar");
        Self {
            state: PlatformState::new(None),
            apps_root,
            app,
        }
    }

    /// `tenant` runs a Counter whose rule is `rule`, with `cedar` as its
    /// Cedar, as a start restores them.
    fn runs(&self, tenant: &str, rule: &str, cedar: &str) {
        self.state
            .server
            .authz
            .reload_tenant_policies(tenant, cedar)
            .expect("load the running Cedar");
        let source = counter(rule);
        let mut registry = self.state.registry.write().expect("the spec registry lock");
        registry
            .try_register_tenant_with_constraints(
                tenant,
                parse_csdl(CSDL).expect("CSDL parses"),
                CSDL.to_string(),
                &[("Counter", source.as_str())],
                None,
                false,
            )
            .expect("register the running Counter");
    }

    async fn install(&self, tenant: &str) -> Result<InstallResult, String> {
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, tenant, &self.app).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.apps_root).ok();
    }
}

#[tokio::test]
async fn a_new_type_whose_rule_breaks_is_refused() {
    let fixture = Fixture::new("new", BELOW_TWO);
    let error = fixture
        .install("unproven-new")
        .await
        .expect_err("a rule that breaks refuses the install of a type the tenant does not run");
    assert!(error.contains("would break"), "{error}");
    assert!(error.contains("Counter.Small"), "{error}");
}

#[tokio::test]
async fn a_rule_the_tenant_runs_as_it_is_keeps_running() {
    let fixture = Fixture::new("running", BELOW_TWO);
    let tenant = "unproven-running";
    fixture.runs(tenant, BELOW_TWO, PERMIT);
    let result = fixture
        .install(tenant)
        .await
        .expect("a rule the tenant already runs as it is does not take the app down");
    assert!(
        result
            .unproved_rules
            .iter()
            .any(|rule| rule.contains("Counter.Small")),
        "{:?}",
        result.unproved_rules
    );
}

#[tokio::test]
async fn a_change_to_a_rule_that_breaks_is_refused() {
    let fixture = Fixture::new("changed", BELOW_ONE);
    let tenant = "unproven-changed";
    fixture.runs(tenant, BELOW_TWO, PERMIT);
    let error = fixture
        .install(tenant)
        .await
        .expect_err("a changed rule that breaks refuses the install");
    assert!(error.contains("Counter.Small"), "{error}");
}

#[tokio::test]
async fn cedar_that_breaks_a_running_rule_is_refused() {
    let fixture = Fixture::new("cedar", BELOW_TWO);
    let tenant = "unproven-cedar";
    // Nobody may count, so the rule holds as the tenant runs it; the app's
    // Cedar lets anyone count, and the same rule breaks.
    fixture.runs(tenant, BELOW_TWO, "");
    let error = fixture
        .install(tenant)
        .await
        .expect_err("Cedar that breaks a rule refuses the install");
    assert!(error.contains("Counter.Small"), "{error}");
}
