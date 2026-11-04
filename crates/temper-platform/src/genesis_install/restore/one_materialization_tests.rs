//! One app version installed into many tenants is read from its source
//! tenant's records once per boot. The first tenant restored materializes it
//! from the records; every other tenant gets a copy in its own cache
//! directory, as boot restores them one at a time.
//!
//! A server with many tenants materialized the same few versions again for
//! each tenant installing them, reading every commit, tree and blob each
//! time, on every boot.

use base64::Engine as _;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_spec::csdl::parse_csdl;

use super::super::object_lookup::genesis_object_entity_id;
use super::*;

/// Where `App.Install` reads its records from when none is configured.
const SOURCE: &str = "default";
const REPOSITORY: &str = "rp-acme-notes";
const COMMIT: &str = "4444444444444444444444444444444444444444";
const TREE: [u8; 20] = [0x33; 20];
const MANIFEST: [u8; 20] = [0x11; 20];
const GUIDE: [u8; 20] = [0x22; 20];
const MANIFEST_TEXT: &str = "name = \"notes\"\ndependencies = []\n";
const GUIDE_TEXT: &str = "# notes\n\nversion from the source records\n";
const TYPES: [&str; 5] = ["App", "AppInstallation", "Commit", "Tree", "Blob"];

fn hex(sha: &[u8; 20]) -> String {
    sha.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A record type that stays in `state` once created.
fn ioa(name: &str, state: &str) -> String {
    format!(
        "[automaton]\nname = \"{name}\"\nstates = [\"{state}\"]\ninitial = \"{state}\"\n\
         allow_indefinite_states = [\"{state}\"]\n\n[[action]]\nname = \"Touch\"\n\
         kind = \"input\"\nfrom = [\"{state}\"]\nto = \"{state}\"\n"
    )
}

fn csdl() -> String {
    let types: String = TYPES
        .iter()
        .map(|name| {
            format!(
                r#"<EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>"#
            )
        })
        .collect();
    let sets: String = TYPES
        .iter()
        .map(|name| format!(r#"<EntitySet Name="{name}s" EntityType="Temper.Source.{name}"/>"#))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Source" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      {types}
      <EntityContainer Name="Source">{sets}</EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#
    )
}

/// A server whose source tenant holds one version of app `notes`, installed
/// into each of `targets`.
async fn server_with_installs(data_dir: &Path, targets: &[&str]) -> PlatformState {
    let csdl_xml = csdl();
    let sources: Vec<(String, String)> = TYPES
        .iter()
        .map(|name| {
            let state = if *name == "AppInstallation" {
                "Installed"
            } else {
                "Stored"
            };
            (name.to_string(), ioa(name, state))
        })
        .collect();
    let specs: Vec<(&str, &str)> = sources
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect();
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        SOURCE,
        parse_csdl(&csdl_xml).expect("source CSDL"),
        csdl_xml.clone(),
        &specs,
        None,
    );
    for name in TYPES {
        registry.set_verification_status(
            &TenantId::new(SOURCE),
            name,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-11-04T00:00:00Z".to_string(),
            }),
        );
    }
    let mut platform = PlatformState::with_registry(registry, None);
    platform.server.data_dir = data_dir.to_path_buf();

    let mut tree = Vec::new();
    for (name, sha) in [("APP.md", GUIDE), ("app.toml", MANIFEST)] {
        tree.extend_from_slice(format!("100644 {name}\0").as_bytes());
        tree.extend_from_slice(&sha);
    }
    let mut canonical = format!("tree {}\0", tree.len()).into_bytes();
    canonical.extend_from_slice(&tree);
    let base64 = &base64::engine::general_purpose::STANDARD;
    let mut records = vec![
        (
            "App",
            "app-notes".to_string(),
            serde_json::json!({"Name": "notes", "OwnerId": "acme", "RepositoryId": REPOSITORY}),
        ),
        (
            "Commit",
            genesis_object_entity_id(REPOSITORY, COMMIT),
            serde_json::json!({"RepositoryId": REPOSITORY, "TreeSha": hex(&TREE)}),
        ),
        (
            "Tree",
            genesis_object_entity_id(REPOSITORY, &hex(&TREE)),
            serde_json::json!({"RepositoryId": REPOSITORY, "CanonicalBytes": base64.encode(&canonical)}),
        ),
    ];
    for (sha, text) in [(MANIFEST, MANIFEST_TEXT), (GUIDE, GUIDE_TEXT)] {
        records.push((
            "Blob",
            genesis_object_entity_id(REPOSITORY, &hex(&sha)),
            serde_json::json!({
                "RepositoryId": REPOSITORY,
                "Size": text.len(),
                "Content": base64.encode(text),
            }),
        ));
    }
    for target in targets {
        records.push((
            "AppInstallation",
            format!("install-{target}"),
            serde_json::json!({
                "AppId": "app-notes",
                "VersionHash": COMMIT,
                "AppRef": format!("acme/notes@{COMMIT}"),
                "TargetTenant": target,
            }),
        ));
    }
    let source = TenantId::new(SOURCE);
    for (entity_type, id, fields) in records {
        platform
            .server
            .get_or_create_tenant_entity(&source, entity_type, &id, fields)
            .await
            .unwrap_or_else(|error| panic!("create {entity_type} {id}: {error:?}"));
    }
    platform
}

#[tokio::test]
async fn a_version_installed_into_many_tenants_is_read_from_its_records_once() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let id = uuid::Uuid::new_v4().simple().to_string();
    let first = format!("first-{id}");
    let second = format!("second-{id}");
    let platform = server_with_installs(data_dir.path(), &[&first, &second]).await;

    let mut planned = plan_app_cache_roots(&platform).await;
    assert_eq!(planned.len(), 2, "one planned root for each target tenant");
    let first_roots = planned.remove(&first).expect("the first tenant's root");
    let second_roots = planned.remove(&second).expect("the second tenant's root");

    // A boot restores one tenant, then the next.
    assert_eq!(restore_planned_cache_roots(&platform, first_roots).await, 1);
    let guide = crate::os_apps::tenant_app_guide(&first, "notes").expect("first resolves notes");
    assert!(guide.contains("version from the source records"), "{guide}");

    // The source's records are no longer there to read: the second tenant's
    // restore comes back only if it does not read them again.
    let source = TenantId::new(SOURCE);
    platform.server.remove_entity(
        &source,
        "Commit",
        &genesis_object_entity_id(REPOSITORY, COMMIT),
    );
    assert_eq!(
        restore_planned_cache_roots(&platform, second_roots).await,
        1,
        "the second tenant's install of the same version read the source tenant's \
         records again: each tenant's install was materialized from them on its own"
    );
    let guide = crate::os_apps::tenant_app_guide(&second, "notes").expect("second resolves notes");
    assert!(guide.contains("version from the source records"), "{guide}");
    let first_dir = crate::os_apps::tenant_os_app_dir(&first, "notes").expect("first's notes");
    let second_dir = crate::os_apps::tenant_os_app_dir(&second, "notes").expect("second's notes");
    assert_ne!(
        first_dir, second_dir,
        "each tenant's install keeps its own cache directory"
    );
}
