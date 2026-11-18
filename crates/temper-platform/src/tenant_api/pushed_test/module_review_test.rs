//! A module whose code is as installed is reviewed as advice: an install answers without waiting for it, what the
//! review found stays readable, and a caller that answers with all of it still can. A new or changed module with a
//! finding is refused.

use std::time::Duration;

use base64::Engine as _;
use tokio::sync::Semaphore;

use super::*;
use crate::genesis_install::{
    GenesisRegistryInstallRequest, PushMode, PushedFile, install_genesis_app_from_registry,
    install_pushed_app,
};
use crate::module_review::{
    Answer, Answers, ITEMS, Judge, JudgeUnavailable, ModuleReviewer, Question,
};
use crate::os_apps::InstallResult;

const MODULE: &str = "answer";

/// Jev, who answers once the test lets him. Says every module shows one item (the first, unless told), at its first
/// line.
struct HeldJudge {
    gate: Semaphore,
    down: bool,
    item: usize,
}

impl HeldJudge {
    fn held() -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(0),
            down: false,
            item: 0,
        })
    }

    /// Answers at once, finding `ITEMS[item]`.
    fn finding(item: usize) -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(1),
            down: false,
            item,
        })
    }

    fn open() -> Arc<Self> {
        let judge = Self::held();
        judge.let_answer();
        judge
    }

    fn down() -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(1),
            down: true,
            item: 0,
        })
    }

    fn let_answer(&self) {
        self.gate.add_permits(1);
    }
}

#[async_trait::async_trait]
impl Judge for HeldJudge {
    async fn ask(
        &self,
        _module: &str,
        _state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeUnavailable> {
        let _turn = self.gate.acquire().await.expect("the gate stays open");
        if self.down {
            return Err(JudgeUnavailable::OutOfCredit);
        }
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let answer = match question {
                    Question::Noul { instructions } => Answer {
                        noul: Some(if *instructions == ITEMS[self.item].shows {
                            0.85
                        } else {
                            0.1
                        }),
                        choice: None,
                    },
                    Question::Choice { criteria, .. } => Answer {
                        noul: None,
                        choice: criteria.keys().next().cloned(),
                    },
                };
                (key.clone(), answer)
            })
            .collect();
        Ok(Answers {
            answers,
            input_tokens: 100,
        })
    }
}

/// A smallest module, whose bytes differ with `version`.
fn wasm(version: u8) -> String {
    let bytes = [0, b'a', b's', b'm', 1, 0, 0, 0, 0, 3, 1, b'v', version];
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The workspace's app with one module, as a push sends it, naming its commit when it has one. Each commit's spec
/// differs by a comment, so a module whose code is as installed is still read again, as advice.
fn app_with_module(commit: Option<&str>, version: u8) -> serde_json::Value {
    let mut files: serde_json::Map<String, serde_json::Value> = app_files()
        .into_iter()
        .map(|(path, text)| (path, serde_json::Value::String(text)))
        .collect();
    let spec = files[SPEC].as_str().expect("text").to_string()
        + &format!("\n# {}\n", commit.unwrap_or("no commit"));
    files.insert(SPEC.into(), spec.into());
    let manifest = files["app.toml"].as_str().expect("text").to_string()
        + &format!(
            "\n[[wasm_modules]]\nname = {MODULE:?}\ntarget = \"wasm32-unknown-unknown\"\n\
             criticality = \"app-required\"\nstartup_loading = \"lazy\"\n\
             provenance = \"bundled-artifact\"\nimport_class = \"temper-host\"\n"
        );
    files.insert("app.toml".into(), manifest.into());
    files.insert(
        format!("wasm/{MODULE}/{MODULE}.wasm"),
        serde_json::json!({ "base64": wasm(version) }),
    );
    files.insert(
        format!("wasm/{MODULE}/src/lib.rs"),
        format!("pub fn step(x: u32) -> u32 {{\n    x + {version}\n}}\n").into(),
    );
    if let Some(commit) = commit {
        files.insert("COMMIT".into(), format!("{commit}\n").into());
    }
    serde_json::Value::Object(files)
}

/// A server whose tenant has the app's module installed at version 1, by an install with no Jev, its note kept.
async fn seeded(tenant: &str) -> Server {
    let server = server(tenant);
    install_over_http(&server.state, tenant, app_with_module(Some("seed"), 1)).await;
    assert_eq!(
        finished_review(&server.state, tenant, "seed").await["state"],
        "done"
    );
    server
}

fn as_operator(tenant: &str, mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        ));
    request
}

async fn ask(
    state: &PlatformState,
    tenant: &str,
    version: &str,
) -> (StatusCode, serde_json::Value) {
    let request = Request::get(format!("/os-apps/{APP}/review/{version}"))
        .body(Body::empty())
        .expect("request should build");
    send(state, as_operator(tenant, request)).await
}

async fn review_of(state: &PlatformState, tenant: &str, version: &str) -> serde_json::Value {
    let (status, body) = ask(state, tenant, version).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The route's answer once the review has stopped reading.
async fn finished_review(state: &PlatformState, tenant: &str, version: &str) -> serde_json::Value {
    for _ in 0..400 {
        let body = review_of(state, tenant, version).await;
        if body["state"] != "reading" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(25)).await; // determinism-ok: the review runs on its own task
    }
    panic!("the review of {version} kept reading");
}

async fn install_over_http(
    state: &PlatformState,
    tenant: &str,
    files: serde_json::Value,
) -> serde_json::Value {
    let (status, body) = send(state, push(tenant, serde_json::json!({ "files": files }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn install_directly(
    state: &PlatformState,
    tenant: &str,
    files: serde_json::Value,
) -> InstallResult {
    let files: BTreeMap<String, PushedFile> =
        serde_json::from_value(files).expect("files should read");
    let (_, pushed) = install_pushed_app(state, tenant, APP, files, PushMode::Install).await;
    pushed.expect("the push should install").result
}

fn found_first_item(warnings: &serde_json::Value) -> bool {
    warnings
        .as_array()
        .expect("warnings")
        .iter()
        .any(|warning| {
            warning["check"] == "module_review"
                && warning["detail"]["module"] == MODULE
                && warning["detail"]["kind"] == ITEMS[0].kind
        })
}

#[tokio::test]
async fn the_answer_does_not_wait_for_the_review_and_what_it_found_is_kept() {
    let tenant = unique_tenant("review-kept");
    let server = seeded(&tenant).await;
    let judge = HeldJudge::held();
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(judge.clone())));

    // Jev has not answered, so a push that waited for him would not come back with the review still to come.
    let body = install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    assert_eq!(
        body["result"]["reviewing"],
        serde_json::json!([{ "app": APP, "version": "c1", "modules": [MODULE] }]),
        "{body}"
    );
    assert!(
        body["result"]["modules_reviewed"].is_null(),
        "nothing is reviewed yet: {body}"
    );
    let reading = review_of(&server.state, &tenant, "c1").await;
    assert_eq!(
        reading,
        serde_json::json!({ "app": APP, "version": "c1", "state": "reading" })
    );

    judge.let_answer();
    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["modules_reviewed"], serde_json::json!([MODULE]));
    assert!(found_first_item(&done["warnings"]), "{done}");

    // Kept with the store, so a restart still answers with it.
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert_eq!(review_of(&restarted, &tenant, "c1").await, done);
    // And it is for that version only.
    assert_eq!(review_of(&restarted, &tenant, "c2").await["state"], "none");
}

#[tokio::test]
async fn a_version_with_no_commit_is_asked_about_by_what_the_answer_names() {
    let tenant = unique_tenant("review-digest");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));

    let body = install_over_http(&server.state, &tenant, app_with_module(None, 1)).await;
    let version = body["result"]["reviewing"][0]["version"]
        .as_str()
        .expect("the answer names the version");
    assert!(!version.is_empty(), "{body}");

    let done = finished_review(&server.state, &tenant, version).await;
    assert_eq!(done["state"], "done", "{done}");
}

#[tokio::test]
async fn a_jev_that_cannot_answer_neither_holds_nor_fails_the_install_and_is_said() {
    let tenant = unique_tenant("review-down");
    let server = server(&tenant);
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::down())));

    install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;

    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["modules_reviewed"], serde_json::json!([]));
    let note = &done["warnings"][0];
    assert_eq!(note["check"], "module_review", "{done}");
    assert_eq!(note["detail"]["modules"], serde_json::json!([MODULE]));
}

#[tokio::test]
async fn a_server_with_no_jev_says_so_in_the_same_place() {
    let tenant = unique_tenant("review-no-jev");
    let server = server(&tenant);

    let body = install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    assert_eq!(body["result"]["reviewing"][0]["version"], "c1", "{body}");

    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");
    let message = done["warnings"][0]["message"].as_str().expect("a note");
    assert!(message.contains("no Jev"), "{done}");
}

#[tokio::test]
async fn a_restart_during_a_review_answers_none() {
    let tenant = unique_tenant("review-restart");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::held())));

    install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    assert_eq!(
        review_of(&server.state, &tenant, "c1").await["state"],
        "reading"
    );

    // A new process has no review under way and nothing kept for this version.
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert_eq!(review_of(&restarted, &tenant, "c1").await["state"], "none");
}

#[tokio::test]
async fn a_newer_review_stops_the_older_and_a_caller_waiting_on_the_older_is_told() {
    let tenant = unique_tenant("review-newer");
    let server = seeded(&tenant).await;
    let judge = HeldJudge::held();
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(judge.clone())));

    let older = install_directly(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    install_over_http(&server.state, &tenant, app_with_module(Some("c2"), 1)).await;

    assert_eq!(
        review_of(&server.state, &tenant, "c1").await["state"],
        "none"
    );
    assert_eq!(
        review_of(&server.state, &tenant, "c2").await["state"],
        "reading"
    );
    let older = older.with_reviews_read().await;
    assert!(older.reviewing.is_empty());
    assert!(older.modules_reviewed.is_empty());
    assert!(
        older
            .warnings
            .iter()
            .any(|warning| warning.check == "module_review"
                && warning.message.contains("the review stopped")),
        "{:?}",
        older.warnings
    );

    judge.let_answer();
    let done = finished_review(&server.state, &tenant, "c2").await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(
        review_of(&server.state, &tenant, "c1").await["state"],
        "none"
    );
}

#[tokio::test]
async fn a_caller_that_answers_with_all_of_it_can_wait_for_it() {
    let tenant = unique_tenant("review-read");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));

    let result = install_directly(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    let result = result.with_reviews_read().await;

    assert_eq!(result.modules_reviewed, vec![MODULE.to_string()]);
    assert!(result.reviewing.is_empty());
    let warnings = serde_json::to_value(&result.warnings).expect("warnings serialize");
    assert!(found_first_item(&warnings), "{warnings}");
}

#[tokio::test]
async fn a_genesis_install_still_answers_with_what_the_review_found() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    // The first version installs the module with no Jev; the second changes its code. A Genesis install takes a
    // published version, so its review is advice and never refuses it.
    const FIRST: &str = "0123456789abcdef0123456789abcdef01234567";
    const HASH: &str = "89abcdef0123456789abcdef0123456789abcdef";
    let tenant = unique_tenant("review-genesis");
    let server = server(&tenant);
    let registry = MockServer::start().await;
    for (hash, commit, version) in [(FIRST, "seed", 1), (HASH, "c1", 2)] {
        let files: Vec<serde_json::Value> = app_with_module(Some(commit), version)
            .as_object()
            .expect("files")
            .iter()
            .filter(|(path, _)| path.as_str() != "COMMIT")
            .map(|(path, file)| {
                let bytes = match file {
                    serde_json::Value::String(text) => text.clone().into_bytes(),
                    file => base64::engine::general_purpose::STANDARD
                        .decode(file["base64"].as_str().expect("base64"))
                        .expect("base64 should read"),
                };
                serde_json::json!({
                    "path": path,
                    "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/acme/{APP}/versions/{hash}/bundle"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "app_ref": format!("acme/{APP}@{hash}"), "registry_tenant": "default",
                "apps": [{"owner": "acme", "name": APP, "version_hash": hash, "files": files}]
            })))
            .mount(&registry)
            .await;
    }
    // SAFETY: these HTTP regressions run with nextest's process isolation,
    // as required by the existing registry-install tests.
    unsafe { std::env::set_var("TEMPER_GENESIS_REGISTRY_URL", registry.uri()) };
    let install = |hash: &str| {
        install_genesis_app_from_registry(
            &server.state,
            GenesisRegistryInstallRequest {
                tenant: tenant.clone(),
                app_ref: format!("acme/{APP}@{hash}"),
                registry_url: registry.uri(),
                registry_tenant: "default".into(),
                follow_policy: "pinned".into(),
            },
        )
    };
    install(FIRST)
        .await
        .expect("the first Genesis install should succeed");
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));

    let installed = install(HASH)
        .await
        .expect("the Genesis install should succeed");

    assert_eq!(installed.modules_reviewed, vec![MODULE.to_string()]);
    let warnings = serde_json::to_value(&installed.warnings).expect("warnings serialize");
    assert!(found_first_item(&warnings), "{warnings}");
}

/// A new or changed module with a finding is refused, naming the finding, its line and what to move into the spec;
/// the same finding in a module whose code is as installed is advice, and the install goes on.
#[tokio::test]
async fn a_changed_module_with_a_finding_is_refused_and_an_unchanged_one_is_not() {
    let several_concerns = ITEMS
        .iter()
        .position(|item| item.kind == "several_concerns")
        .expect("an item for several concerns");
    let tenant = unique_tenant("review-refuses");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::finding(
            several_concerns,
        ))));

    // The module's code is as installed; only a spec changed.
    install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    let done = finished_review(&server.state, &tenant, "c1").await;
    assert!(
        done["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| {
                warning["detail"]["module"] == MODULE
                    && warning["detail"]["kind"] == "several_concerns"
            }),
        "{done}"
    );

    // Its code changed: refused, with nothing written.
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({ "files": app_with_module(Some("c2"), 2) }),
        ),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let said = body.to_string();
    for words in [
        "Refused: Jev's review found work in a new or changed module",
        "(several_concerns) in module answer at wasm/answer/src/lib.rs",
        "Instead:",
    ] {
        assert!(said.contains(words), "{words}: {said}");
    }
    assert_eq!(
        review_of(&server.state, &tenant, "c2").await["state"],
        "none"
    );
}

/// A changed module Jev finds nothing in installs even when it is large: size is advice, not a refusal.
#[tokio::test]
async fn a_large_changed_module_with_no_finding_installs() {
    let tenant = unique_tenant("review-large");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));
    install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;

    let mut files = app_with_module(Some("c2"), 2);
    let body: String = (0..crate::module_review::LARGE_LINES + 10)
        .map(|n| format!("    let v{n} = if x > {n} {{ 1 }} else {{ 0 }};\n"))
        .collect();
    files[format!("wasm/{MODULE}/src/lib.rs")] =
        format!("pub fn step(x: u32) -> u32 {{\n{body}    x + 2\n}}\n").into();
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_route_answers_only_for_the_credentials_own_tenant() {
    let tenant = unique_tenant("review-owner");
    let server = seeded(&tenant).await;
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));
    install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");

    // A tenant whose policies do not let it install is refused.
    let refused = unique_tenant("review-refused");
    let (status, body) = ask(&server.state, &refused, "c1").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // One that may install its own apps has no sight of this tenant's.
    let other = unique_tenant("review-other");
    server
        .state
        .server
        .authz
        .reload_tenant_policies(
            &other,
            r#"permit(principal, action == Action::"install_app", resource is App);"#,
        )
        .expect("install policy should parse");
    assert_eq!(
        review_of(&server.state, &other, "c1").await,
        serde_json::json!({ "app": APP, "version": "c1", "state": "none" })
    );
}

#[path = "module_review_lifecycle_test.rs"]
mod lifecycle;
