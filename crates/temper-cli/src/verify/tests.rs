use super::*;

#[test]
fn test_verify_reference_specs() {
    let fixtures = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/specs"
    ));
    let app = tempfile::tempdir().unwrap();
    fs::create_dir(app.path().join("policies")).unwrap();
    // This CSDL accompanies Order. Other files in the fixture collection are
    // independent examples, including mutually exclusive Process definitions.
    for file in [
        "model.csdl.xml",
        "order.ioa.toml",
        "order.tla",
        "policies/order.cedar",
    ] {
        fs::copy(fixtures.join(file), app.path().join(file)).unwrap();
    }
    run(app.path().to_str().unwrap()).expect("complete reference application should pass");
}

#[test]
fn test_verify_fails_on_broken_spawn_contract_with_exact_lint_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs_dir = tmp.path();

    let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Broken" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Plan">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityType Name="Task">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Plans" EntityType="Temper.Broken.Plan" />
        <EntitySet Name="Tasks" EntityType="Temper.Broken.Task" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let plan = r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
kind = "input"
from = ["Active"]
params = ["title"]
effect = ["spawn('Task', 'Create')"]
"#;
    let task = r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#;

    fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
    fs::write(specs_dir.join("plan.ioa.toml"), plan).expect("write plan");
    fs::write(specs_dir.join("task.ioa.toml"), task).expect("write task");

    fs::create_dir(specs_dir.join("policies")).unwrap();
    for path in fs::read_dir(specs_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
        if let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".ioa.toml"))
        {
            // These tests exercise IOA failures, with explicit deny-all policy fixtures.
            fs::write(
                specs_dir.join("policies").join(format!("{stem}.cedar")),
                "forbid(principal, action, resource);",
            )
            .unwrap();
        }
    }
    let result = run(specs_dir.to_str().expect("tmp path utf-8"));
    let err = result.expect_err("verify should fail on broken spawn contract");
    let msg = err.to_string();
    assert!(
        msg.contains("spawn_initial_action_params_unmapped"),
        "expected exact lint code in error, got: {msg}"
    );
}

#[test]
fn test_multi_entity_dir_runs_composite_as_gating_step() {
    // A two-entity directory whose cross-entity reaction can be dropped:
    // Workspace.Freeze moves Workspace out of Active before File.Touch
    // fires Workspace.IncrementUsage (enabled only from Active). The
    // dropped reaction must FAIL the command (composite is gating).
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs_dir = tmp.path();

    let csdl = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Fs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
        <Property Name="workspace_id" Type="Edm.String" />
      </EntityType>
      <EntityType Name="Workspace">
        <Key><PropertyRef Name="Id" /></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false" />
        <Property Name="status" Type="Edm.String" />
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Files" EntityType="Temper.Fs.File" />
        <EntitySet Name="Workspaces" EntityType="Temper.Fs.Workspace" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"

[[action]]
name = "Touch"
kind = "input"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
kind = "field"
id_field = "workspace_id"
"#;
    let workspace = r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"

[[action]]
name = "IncrementUsage"
kind = "input"
from = ["Active"]
to = "Active"

[[action]]
name = "Freeze"
kind = "internal"
from = ["Active"]
to = "Frozen"
"#;

    fs::write(specs_dir.join("model.csdl.xml"), csdl).expect("write csdl");
    fs::write(specs_dir.join("file.ioa.toml"), file).expect("write file");
    fs::write(specs_dir.join("workspace.ioa.toml"), workspace).expect("write workspace");

    fs::create_dir(specs_dir.join("policies")).unwrap();
    for path in fs::read_dir(specs_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
        if let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".ioa.toml"))
        {
            // These tests exercise IOA failures, with explicit deny-all policy fixtures.
            fs::write(
                specs_dir.join("policies").join(format!("{stem}.cedar")),
                "forbid(principal, action, resource);",
            )
            .unwrap();
        }
    }
    let result = run(specs_dir.to_str().expect("tmp path utf-8"));
    let err = result.expect_err("composite gating step must fail on a dropped reaction");
    let msg = err.to_string();
    assert!(
        msg.contains("composite cross-entity verification failed"),
        "expected composite gating failure, got: {msg}"
    );
    assert!(
        msg.contains("IncrementUsage") && msg.contains("Frozen"),
        "failure should name the dropped reaction + wrong state, got: {msg}"
    );
}
