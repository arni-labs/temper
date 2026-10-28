//! The Genesis compare route's answer, read from stored Commit rows.
//!
//! The registry keeps each commit's parents in `Commit.ParentShas`. These
//! tests store a small history the way Genesis does (rows keyed
//! `{repository}-{sha}`) and ask the production comparison how two versions
//! of an app relate, as an anonymous caller and as an authenticated one.

use temper_platform::genesis_install::BundleAudience;
use temper_platform::genesis_install::version_compare::{
    VersionComparison, compare_genesis_app_versions,
};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Genesis.CompareTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="App">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="Name" Type="Edm.String"/>
        <Property Name="OwnerId" Type="Edm.String"/>
        <Property Name="RepositoryId" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Repository">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="Visibility" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Commit">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="RepositoryId" Type="Edm.String"/>
        <Property Name="ParentShas" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Apps" EntityType="Genesis.CompareTest.App"/>
        <EntitySet Name="Repositories" EntityType="Genesis.CompareTest.Repository"/>
        <EntitySet Name="Commits" EntityType="Genesis.CompareTest.Commit"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const APP_IOA: &str = r#"
[automaton]
name = "App"
states = ["Active"]
initial = "Active"

[[state]]
name = "Name"
type = "string"
initial = ""

[[state]]
name = "OwnerId"
type = "string"
initial = ""

[[state]]
name = "RepositoryId"
type = "string"
initial = ""
"#;

const REPOSITORY_IOA: &str = r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[state]]
name = "Visibility"
type = "string"
initial = ""
"#;

const COMMIT_IOA: &str = r#"
[automaton]
name = "Commit"
states = ["Durable"]
initial = "Durable"

[[state]]
name = "RepositoryId"
type = "string"
initial = ""

[[state]]
name = "ParentShas"
type = "string"
initial = ""
"#;

const PUBLIC_REPO: &str = "rp-acme-widget";
const PRIVATE_REPO: &str = "rp-acme-secret";

fn sha(pair: &str) -> String {
    pair.repeat(20)
}

/// Store one app, its repository and its commits as Genesis rows.
async fn store_app(
    state: &ServerState,
    name: &str,
    repository_id: &str,
    visibility: &str,
    commits: &[(&str, &[&str])],
) {
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Repository",
            repository_id,
            serde_json::json!({ "Visibility": visibility }),
        )
        .await
        .expect("store Repository");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "App",
            &format!("app-acme-{name}"),
            serde_json::json!({ "Name": name, "OwnerId": "acme", "RepositoryId": repository_id }),
        )
        .await
        .expect("store App");
    for (commit, parents) in commits {
        let parents: Vec<String> = parents.iter().map(|parent| sha(parent)).collect();
        state
            .get_or_create_tenant_entity(
                &tenant,
                "Commit",
                // The key Genesis writes: `{repository}-{sha}`.
                &format!("{repository_id}-{}", sha(commit)),
                serde_json::json!({
                    "RepositoryId": repository_id,
                    "ParentShas": parents.join(","),
                }),
            )
            .await
            .expect("store Commit");
    }
}

/// widget (public): c1 <- c2 <- c3, and c4 branches from c1.
/// secret (private): a1 <- a2.
async fn registry() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[
            ("App", APP_IOA),
            ("Repository", REPOSITORY_IOA),
            ("Commit", COMMIT_IOA),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("genesis-compare"), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(0), None));
    store_app(
        &state,
        "widget",
        PUBLIC_REPO,
        "public",
        &[
            ("c1", &[]),
            ("c2", &["c1"]),
            ("c3", &["c2"]),
            ("c4", &["c1"]),
        ],
    )
    .await;
    store_app(
        &state,
        "secret",
        PRIVATE_REPO,
        "private",
        &[("a1", &[]), ("a2", &["a1"])],
    )
    .await;
    state
}

async fn compare(
    state: &ServerState,
    name: &str,
    base: &str,
    head: &str,
    audience: BundleAudience,
) -> Result<VersionComparison, String> {
    compare_genesis_app_versions(
        state,
        "default",
        "acme",
        name,
        &sha(base),
        &sha(head),
        audience,
    )
    .await
}

#[tokio::test]
async fn stored_parents_answer_how_two_public_versions_relate() {
    let state = registry().await;
    for (base, head, expected) in [
        ("c1", "c3", VersionComparison::Ahead),
        ("c3", "c1", VersionComparison::Behind),
        ("c3", "c3", VersionComparison::Identical),
        ("c3", "c4", VersionComparison::Diverged),
    ] {
        assert_eq!(
            compare(&state, "widget", base, head, BundleAudience::Anonymous).await,
            Ok(expected),
            "{base}...{head}"
        );
    }
}

#[tokio::test]
async fn a_version_outside_the_apps_repository_is_not_found() {
    let state = registry().await;
    // a2 is a commit, but of another repository.
    for (base, head) in [("c1", "ff"), ("a2", "c3")] {
        let error = compare(&state, "widget", base, head, BundleAudience::Anonymous)
            .await
            .expect_err("a commit of another repository is not this app's version");
        assert!(error.contains("not found"), "{base}...{head}: {error}");
    }
}

#[tokio::test]
async fn an_anonymous_caller_gets_no_answer_about_a_private_app() {
    let state = registry().await;
    let error = compare(&state, "secret", "a1", "a2", BundleAudience::Anonymous)
        .await
        .expect_err("a private repository's history is not readable anonymously");
    assert!(error.contains("not public"), "{error}");
    assert_eq!(
        compare(&state, "secret", "a1", "a2", BundleAudience::Authenticated).await,
        Ok(VersionComparison::Ahead),
        "an authenticated caller, already past Cedar, gets the answer"
    );
}
