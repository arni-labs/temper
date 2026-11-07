//! `"dry_run": true`: a push that only checks. It answers what the install of
//! the same files would answer, refusing with the same words, and leaves the
//! tenant as it found it.

use super::*;

/// What the tenant serves and keeps for the app, as a caller and the server see it.
#[derive(Debug, PartialEq)]
struct Kept {
    record: Option<String>,
    listed: Option<Listed>,
    versions: Vec<String>,
    registered: Option<String>,
    resolved: Option<String>,
    policy: Option<String>,
    csdl: Option<String>,
}

async fn kept(server: &Server, tenant: &str) -> Kept {
    let id = TenantId::new(tenant);
    let (registered, csdl) = {
        let registry = server.state.registry.read().expect("registry");
        (
            registry
                .get_spec(&id, "SentinelMonitor")
                .map(|spec| spec.ioa_source.clone()),
            registry
                .get_tenant(&id)
                .map(|config| config.csdl_xml.to_string()),
        )
    };
    Kept {
        record: record(&server.ps, tenant)
            .await
            .map(|record| format!("{record:?}")),
        listed: listed_for(&server.state, tenant).await,
        versions: pushed_versions(server.data_dir.path()),
        registered,
        resolved: tenant_os_app(tenant, APP).map(|bundle| {
            bundle
                .specs
                .into_iter()
                .map(|(entity_type, source)| format!("{entity_type}\n{source}"))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        policy: server.state.server.authz.get_tenant_policy_text(tenant),
        csdl,
    }
}

fn dry(files: &BTreeMap<String, String>) -> serde_json::Value {
    serde_json::json!({ "files": files, "dry_run": true })
}

/// The app's files changed one way each, every one a version the install
/// refuses, by what it checks.
fn refused_versions() -> Vec<(&'static str, BTreeMap<String, String>)> {
    let mut cases = Vec::new();

    let mut cedar = with_comment("cedar");
    cedar.insert(
        "policies/evolution.cedar".to_string(),
        "permit(principal, action, resource) when { this is not cedar };".to_string(),
    );
    cases.push(("Cedar that does not compile", cedar));

    // NoAlerts takes has_alerts, a flag the move itself writes, and reads it nowhere.
    let mut lost = with_comment("lost param");
    let spec = lost.get_mut(SPEC).expect("sentinel spec");
    let unread = "effect = [\"has_alerts = false\"]\n";
    assert_eq!(spec.matches(unread).count(), 1, "NoAlerts' effect");
    *spec = spec.replace(unread, &format!("{unread}params = [\"has_alerts\"]\n"));
    cases.push(("a param the move never stores", lost));

    let mut unparsed = with_comment("unparsed");
    unparsed.insert(SPEC.to_string(), "[automaton]\nname = \n".to_string());
    cases.push(("a spec that does not parse", unparsed));

    let mut csdl = with_comment("csdl");
    csdl.insert("model.csdl.xml".to_string(), "<edmx:Edmx".to_string());
    cases.push(("a CSDL that does not parse", csdl));

    let mut stray = with_comment("stray");
    stray.insert(
        "stray.ioa.toml".to_string(),
        app_files()[SPEC].replace("SentinelMonitor", "StrayMonitor"),
    );
    cases.push(("a spec with no entity type in the CSDL", stray));

    let mut cross = with_comment("cross");
    cross.insert(
        "specs/cross-invariants.toml".to_string(),
        "this is not toml =".to_string(),
    );
    cases.push(("cross-invariants that do not parse", cross));

    let mut reactions = with_comment("reactions");
    reactions.insert(
        "specs/reactions.toml".to_string(),
        "[[reaction]]\n".to_string(),
    );
    cases.push(("a reactions.toml", reactions));

    let mut module = with_comment("module");
    module.get_mut("app.toml").expect("app.toml").push_str(
        "\n[[wasm_modules]]\nname = \"absent\"\ntarget = \"wasm32-unknown-unknown\"\ncriticality = \"app-required\"\nstartup_loading = \"lazy\"\nprovenance = \"bundled-artifact\"\nimport_class = \"temper-host\"\n",
    );
    cases.push(("a required module with no binary", module));

    let mut unnamed = with_comment("unnamed");
    unnamed.remove("APP.md");
    cases.push(("files that are not the app", unnamed));

    cases
}

/// Every version the install refuses, checked dry on a tenant with nothing
/// installed and on one serving the app: the check refuses it with the words
/// the install then refuses it with, and the tenant is as it was.
#[tokio::test]
async fn a_dry_run_refuses_what_the_install_refuses_and_changes_nothing() {
    let mut wrong = Vec::new();
    for installed_before in [false, true] {
        for (case, files) in refused_versions() {
            let case = format!("{case} (installed before: {installed_before})");
            let tenant = unique_tenant("dry-refused");
            let server = server(&tenant);
            if installed_before {
                let (status, body) = send(
                    &server.state,
                    push(&tenant, serde_json::json!({ "files": app_files() })),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
            let before = kept(&server, &tenant).await;

            let (checked, check) = send(&server.state, push(&tenant, dry(&files))).await;
            if checked == StatusCode::OK {
                wrong.push(format!("{case}: the check took it: {check}"));
            }
            if kept(&server, &tenant).await != before {
                wrong.push(format!("{case}: the check changed the tenant"));
            }

            let (installed, install) = send(
                &server.state,
                push(&tenant, serde_json::json!({ "files": files })),
            )
            .await;
            let said = check["error"].as_str().unwrap_or("(no error)");
            let agrees = installed == checked
                && install["error"]
                    .as_str()
                    .is_some_and(|error| error.starts_with(said));
            if !agrees {
                wrong.push(format!(
                    "{case}: the install answered {installed} {install}, the check {checked} {check}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A version the check passes is the version the install then installs, by
/// the same digest, and the check before it left nothing behind.
#[tokio::test]
async fn a_dry_run_passes_what_the_install_takes_and_changes_nothing() {
    let tenant = unique_tenant("dry-passed");
    let server = server(&tenant);
    for files in [app_files(), with_comment("a second version")] {
        let before = kept(&server, &tenant).await;
        let (status, check) = send(&server.state, push(&tenant, dry(&files))).await;
        assert_eq!(status, StatusCode::OK, "{check}");
        assert_eq!(check["checked"], APP, "{check}");
        assert!(check.get("installed").is_none(), "{check}");
        assert_eq!(
            kept(&server, &tenant).await,
            before,
            "the check changed the tenant"
        );

        let (status, install) = send(
            &server.state,
            push(&tenant, serde_json::json!({ "files": files })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{install}");
        assert_eq!(install["digest"], check["digest"]);
        assert_eq!(
            install["result"]["added"], check["result"]["added"],
            "{install} vs {check}"
        );
        assert_eq!(install["result"]["updated"], check["result"]["updated"]);
    }
}
