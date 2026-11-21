use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::source::{self, lines_of, without_comments};
use super::*;

/// A judge that rates one item's statement high and points at the first line holding `marker`, and keeps each state
/// it was asked about.
#[derive(Default)]
struct Stub {
    found: Option<&'static str>,
    marker: &'static str,
    unavailable: Option<JudgeUnavailable>,
    calls: AtomicUsize,
    states: std::sync::Mutex<Vec<String>>,
}

impl Stub {
    fn finding(kind: &'static str, marker: &'static str) -> Self {
        Self {
            found: Some(kind),
            marker,
            ..Self::default()
        }
    }

    fn clean() -> Self {
        Self::default()
    }

    fn down(reason: JudgeUnavailable) -> Self {
        Self {
            unavailable: Some(reason),
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl Judge for Stub {
    async fn ask(
        &self,
        _module: &str,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeUnavailable> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(state.to_string());
        if let Some(reason) = &self.unavailable {
            return Err(reason.clone());
        }
        let found = self
            .found
            .and_then(|kind| ITEMS.iter().find(|item| item.kind == kind));
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let answer = match question {
                    Question::Noul { instructions } => {
                        let hit = found.is_some_and(|item| item.shows == instructions);
                        Answer {
                            noul: Some(if hit { 0.85 } else { 0.1 }),
                            choice: None,
                        }
                    }
                    Question::Choice { criteria, .. } => Answer {
                        noul: None,
                        choice: criteria
                            .iter()
                            .find(|(_, text)| text.contains(self.marker))
                            .map(|(key, _)| key.clone()),
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

fn specs(module: &str) -> Vec<SpecSource> {
    let spec = format!(
        r#"
[automaton]
name = "Order"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Advance"
from = ["Open"]

[[action.triggers]]
kind = "wasm"
module = "{module}"
on_success = "Advanced"
"#
    );
    vec![SpecSource::new("Order", &spec)]
}

#[test]
fn test_spec_source_reads_which_action_runs_which_module() {
    let spec = &specs("steps")[0];
    assert_eq!(
        spec.module_triggers,
        vec![("Advance".to_string(), "steps".to_string())]
    );
    assert_eq!(
        SpecSource::new("Broken", "not = [toml").module_triggers,
        Vec::new()
    );
}

/// No module renamed: each is read by the name its specs run it by.
fn names() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn module(name: &str, source: &str) -> ModuleSource {
    ModuleSource {
        module: name.to_string(),
        path: format!("wasm/{name}/src/lib.rs"),
        source: source.to_string(),
        does: None,
    }
}

const PHASED: &str = r#"fn run(ctx: &Ctx) -> Value {
    // walks its own steps
    let phase = ctx.field("phase");
    let total = ctx.field("total");
    let label = format!("{total} items");
    let note = "// not a comment";
    if phase == "fetch" {
        return json!({ "phase": "parse", "label": label, "note": note });
    }
    json!({ "phase": "done" })
}
"#;

#[test]
fn test_without_comments_keeps_lines_and_strings() {
    let source = "let a = 1; // gone\n/* gone\n   too */ let b = \"// kept\";\nlet c = '/';\n";
    let stripped = without_comments(source);
    assert_eq!(stripped.matches('\n').count(), source.matches('\n').count());
    assert!(!stripped.contains("gone"));
    assert!(stripped.contains("\"// kept\""));
    assert!(stripped.contains("'/'"));
    let lines = lines_of(source);
    assert_eq!(
        (lines[3].number, lines[3].text.as_str()),
        (4, "let c = '/';")
    );
}

#[tokio::test]
async fn test_review_names_item_line_and_runner() {
    let judge = Stub::finding("hidden_states", "ctx.field(\"phase\")");
    let review = review_module(&judge, &module("steps", PHASED), &specs("steps"), &names()).await;
    let review = review.expect("the stub answers");
    assert_eq!(review.findings.len(), 1);
    let finding = &review.findings[0];
    assert_eq!(finding.item, 2);
    assert_eq!(finding.location, "wasm/steps/src/lib.rs:3");
    assert_eq!(
        finding.line.as_deref(),
        Some("let phase = ctx.field(\"phase\");")
    );
    assert!(
        finding
            .why
            .starts_with("In this app the module runs from Order's Advance; ")
    );
    assert!(
        finding
            .instead
            .starts_with("Instead: make each phase a state")
    );
}

#[tokio::test]
async fn test_clean_module_has_no_findings() {
    let judge = Stub::clean();
    let review = review_module(&judge, &module("label", PHASED), &specs("label"), &names()).await;
    assert_eq!(review.expect("the stub answers").findings, Vec::new());
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        1,
        "a clean module is one question set"
    );
}

#[tokio::test]
async fn test_large_module_is_flagged_by_size() {
    let body: String = (0..LARGE_LINES + 10)
        .map(|n| format!("    let v{n} = if n > {n} {{ 1 }} else {{ 0 }};\n"))
        .collect();
    let source = format!("fn run(n: i64) -> i64 {{\n{body}    0\n}}\n");
    let judge = Stub::clean();
    let review = review_module(&judge, &module("big", &source), &specs("big"), &names()).await;
    let findings = review.expect("the stub answers").findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, "large_module");
    assert_eq!(findings[0].item, 7);
    assert!(
        findings[0]
            .why
            .contains(&format!("with {} branches", LARGE_LINES + 10))
    );
    assert!(
        judge.calls.load(Ordering::SeqCst) >= 2,
        "a large module is read in windows"
    );
}

#[tokio::test]
async fn test_unavailable_judge_is_said_once_and_reviews_nothing() {
    let reviewer = ModuleReviewer::new(Arc::new(Stub::down(JudgeUnavailable::OutOfCredit)));
    let modules = [module("a", PHASED), module("b", PHASED)];
    let review = reviewer.review(&modules, &specs("a"), &names()).await;
    assert!(review.reviews.is_empty());
    assert_eq!(review.unavailable, Some(JudgeUnavailable::OutOfCredit));
}

#[tokio::test]
async fn test_reviewer_reuses_a_review_of_the_same_module_and_specs() {
    let judge = Arc::new(Stub::finding("hidden_states", "phase"));
    let reviewer = ModuleReviewer::new(judge.clone());
    let modules = [module("steps", PHASED)];
    let first = reviewer.review(&modules, &specs("steps"), &names()).await;
    let calls = judge.calls.load(Ordering::SeqCst);
    let again = reviewer.review(&modules, &specs("steps"), &names()).await;
    assert_eq!(first, again);
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        calls,
        "a cached review asks nothing"
    );
    let changed = [module("steps", &PHASED.replace("total", "count"))];
    reviewer.review(&changed, &specs("steps"), &names()).await;
    assert!(
        judge.calls.load(Ordering::SeqCst) > calls,
        "changed source is read again"
    );
}

fn questions() -> BTreeMap<String, Question> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "leaf".to_string(),
        Question::Noul {
            instructions: "x".to_string(),
        },
    );
    questions
}

#[tokio::test]
async fn test_jev_without_a_key_sends_nothing() {
    let server = MockServer::start().await;
    let judge = JevJudge::at(&server.uri(), None);
    assert_eq!(
        judge.ask("steps", "state", &questions()).await.unwrap_err(),
        JudgeUnavailable::NoKey
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn test_jev_out_of_credit_is_asked_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(402))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    assert_eq!(
        judge.ask("steps", "state", &questions()).await.unwrap_err(),
        JudgeUnavailable::OutOfCredit
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        1
    );
}

#[tokio::test]
async fn test_jev_answer_is_read_with_its_tokens() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "answers": { "leaf": { "noul": 0.9 } },
        "model": JEV_MODEL,
        "usage": { "input_tokens": 1234 }
    });
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer k"))
        .and(header("temper-review-module", "steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    let answers = judge
        .ask("steps", "state", &questions())
        .await
        .expect("Jev answers");
    assert_eq!(answers.rating("leaf"), Some(0.9));
    assert_eq!(answers.input_tokens, 1234);
}

#[tokio::test]
async fn test_jev_down_is_tried_at_most_twice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    let error = judge.ask("steps", "state", &questions()).await.unwrap_err();
    assert_eq!(
        error,
        JudgeUnavailable::Down("HTTP 503, HTTP 503".to_string())
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        2
    );
}

#[test]
fn test_jev_debug_never_shows_the_key() {
    let judge = JevJudge::at(
        "https://judge.example.test",
        Some("secret-key-value".to_string()),
    );
    assert!(!format!("{judge:?}").contains("secret-key-value"));
}

/// Serialises the tests that change the Jev settings in the process environment.
static JEV_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `check` with the Jev settings set to `url` and `key` (`None` removes one), then removes both.
fn with_jev_env(url: Option<&str>, key: Option<&str>, check: impl FnOnce()) {
    let _guard = JEV_ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, value) in [(JEV_URL_ENV, url), (JEV_KEY_ENV, key)] {
        // SAFETY: the lock above keeps the tests that touch these settings from running at once.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
    check();
    // SAFETY: as above.
    unsafe {
        std::env::remove_var(JEV_URL_ENV);
        std::env::remove_var(JEV_KEY_ENV);
    }
}

#[test]
fn test_jev_from_env_needs_both_url_and_key() {
    with_jev_env(
        Some("https://judge.example.test"),
        Some("secret-key-value"),
        || {
            let judge = JevJudge::from_env().expect("both settings name a judge");
            let shown = format!("{judge:?}");
            assert!(shown.contains("https://judge.example.test"), "{shown}");
            assert!(!shown.contains("secret-key-value"), "{shown}");
        },
    );
    // Without both settings there is no judge: the kernel names no endpoint of its own.
    let missing = [
        (None, None),
        (None, Some("secret-key-value")),
        (Some("https://judge.example.test"), None),
        (Some(""), Some("secret-key-value")),
        (Some("https://judge.example.test"), Some("")),
        (Some("  "), Some("secret-key-value")),
        (Some("https://judge.example.test"), Some(" \t")),
    ];
    for (url, key) in missing {
        with_jev_env(url, key, || {
            assert!(
                JevJudge::from_env().is_none(),
                "url {url:?} and key {key:?} must give no judge"
            )
        });
    }
}

/// An Order whose `Advance` runs `steps`: `Advanced` takes its answer, `Ship` leaves the state it reaches, `Archive`
/// is elsewhere, and `total` is a field none of those use.
fn order_spec() -> SpecSource {
    let spec = r#"
# The order's lifecycle.
[automaton]
name = "Order"
states = ["Open", "Waiting", "Ready", "Shipped", "Old"]
initial = "Open"

[[state]]
name = "attempts"
type = "counter"
initial = 0

[[state]]
name = "total"
type = "counter"
initial = 0

# Asks the module which way to go.
[[action]]
name = "Advance"
from = ["Open"]
to = "Waiting"
effect = ["attempts += 1"]

[[action.triggers]]
kind = "wasm"
module = "steps"
on_success = "Advanced"

[[action]]
name = "Advanced"
kind = "internal"
from = ["Waiting"]
to = "Ready"

[[action]]
name = "Ship"
from = ["Ready"]
to = "Shipped"

[[action]]
name = "Archive"
from = ["Shipped"]
to = "Old"
"#;
    SpecSource::new("Order", spec)
}

#[test]
fn test_excerpt_reads_what_runs_the_module_and_around_it_without_comments() {
    let read = excerpt("steps", &[order_spec()]);
    for kept in [
        "name = \"Advance\"",
        "name = \"Advanced\"",
        "name = \"Ship\"",
        "name = \"attempts\"",
        "states = [",
    ] {
        assert!(read.text.contains(kept), "{kept} is read");
    }
    for left in [
        "name = \"Archive\"",
        "name = \"total\"",
        "# Asks the module",
        "# The order's",
    ] {
        assert!(!read.text.contains(left), "{left} is not read");
    }
    assert!(read.left_out.is_empty(), "nothing it needs is left out");
    assert!(read.heading.contains("actions that run it"));
    // A module no action runs by name reads the whole spec.
    let whole = excerpt("other", &[order_spec()]);
    assert!(
        whole.text.contains("name = \"Archive\"")
            && whole.heading.contains("no action runs this module")
    );
}

/// The order spec with `Ship`, an action around the module, larger than all the room an excerpt has.
fn order_with_a_large_ship() -> SpecSource {
    let notes: String = (0..SPEC_BYTES / 150)
        .map(|n| format!("note{n} = \"{}\"\n", "x".repeat(150)))
        .collect();
    let spec = order_spec().source.replace(
        "[[action]]\nname = \"Ship\"\n",
        &format!("[[action]]\nname = \"Ship\"\n{notes}"),
    );
    SpecSource::new("Order", &spec)
}

#[test]
fn test_excerpt_past_its_bytes_leaves_out_the_farthest_and_names_it() {
    let read = excerpt("steps", &[order_with_a_large_ship()]);
    assert_eq!(read.left_out, vec!["Order's action Ship".to_string()]);
    assert!(read.text.contains("name = \"Advance\"") && read.text.contains("name = \"Advanced\""));
    assert!(read.text.len() <= SPEC_BYTES);
    assert_eq!(said_left_out(&read.left_out), "Order's action Ship");
    let many: Vec<String> = (1..=7).map(|n| format!("Order's action A{n}")).collect();
    assert_eq!(
        said_left_out(&many),
        "Order's action A1, Order's action A2, Order's action A3, Order's action A4, Order's action A5 and 2 more"
    );
}

#[tokio::test]
async fn test_review_says_what_it_left_out() {
    let judge = Stub::clean();
    let review = review_module(
        &judge,
        &module("steps", "pub fn run() {}\n"),
        &[order_with_a_large_ship()],
        &names(),
    )
    .await
    .expect("reviewed");
    assert!(review.findings.is_empty());
    assert_eq!(
        review.left_out,
        vec!["Order's action Ship".to_string()],
        "a clean review that read part of its spec says so"
    );
}

#[tokio::test]
async fn test_jev_route_answer_gives_its_tokens_at_the_top() {
    let server = MockServer::start().await;
    let body = serde_json::json!({ "answers": { "leaf": { "noul": 0.2 } }, "model": JEV_MODEL, "input_tokens": 77, "via": "jev" });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let answers = JevJudge::at(&server.uri(), Some("pass".to_string()))
        .ask("steps", "state", &questions())
        .await
        .expect("the route answers");
    assert_eq!(answers.input_tokens, 77);
}

#[test]
fn test_a_long_line_is_cut() {
    let line = format!("let x = \"{}\";", "y".repeat(400));
    let read = lines_of(&line);
    assert_eq!(read[0].text.chars().count(), source::LINE_CHARS + 1);
    assert!(read[0].text.ends_with('…'));
}

#[test]
fn test_changed_lines_are_the_lines_the_change_added_or_altered() {
    let installed = "fn a() {\n    let x = 1;\n    x\n}\n";
    let now = "// new\nfn a() {\n    let x = 2;\n    let y = 3;\n    x\n}\n";
    assert_eq!(
        changed_lines(installed, now),
        [1, 3, 4].into_iter().collect(),
        "the new comment's line, the altered line and the added one"
    );
    // A comment, or the spaces around a line, is not code.
    assert!(changed_lines("let a = 1; // x\n", "  let a = 1; // y\n").is_empty());
    assert_eq!(changed_lines("", "let a = 1;\n"), [1].into_iter().collect());
}

/// A module of two windows, `fn first` and `fn second`, whose first window gains `extra` lines.
fn two_windows(extra: usize) -> String {
    let first: String = (0..100 + extra)
        .map(|n| format!("    let a{n} = {n};\n"))
        .collect();
    let second: String = (0..100).map(|n| format!("    let b{n} = {n};\n")).collect();
    format!("fn first() {{\n{first}}}\n\nfn second() {{\n{second}}}\n")
}

/// An edit in one window, and a module name that changes with the code (as an app may name it by a hash of its
/// source), leave the other window's state as it was, so Jev's record of that ask answers it the same; its finding
/// still names the line where it now is.
#[tokio::test]
async fn test_an_unchanged_window_is_asked_in_the_same_words() {
    let path = "wasm/calc/src/lib.rs";
    let mut asked = Vec::new();
    for (name, extra) in [("app_calc_1111111111", 0), ("app_calc_2222222222", 3)] {
        let judge = Stub::finding("rules", "let b50 =");
        let source = ModuleSource {
            path: path.to_string(),
            ..module(name, &two_windows(extra))
        };
        let names = [(name.to_string(), path.to_string())].into_iter().collect();
        let review = review_module(&judge, &source, &specs(name), &names)
            .await
            .expect("the stub answers");
        let at = two_windows(extra)
            .lines()
            .position(|line| line.contains("let b50 ="))
            .expect("the line is there")
            + 1;
        assert!(
            review
                .findings
                .iter()
                .any(|finding| finding.number == Some(at)
                    && finding.location == format!("{path}:{at}")),
            "{:?}",
            review.findings
        );
        let states = judge
            .states
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            states.iter().all(|state| !state.contains(name)),
            "the name that changes with the code is not read"
        );
        let second: std::collections::BTreeSet<String> = states
            .into_iter()
            .filter(|state| state.contains("fn second"))
            .collect();
        assert_eq!(second.len(), 1, "one state for the second window");
        asked.push(second);
    }
    assert_eq!(
        asked[0], asked[1],
        "the second window is asked in the same words"
    );
}

/// Axia's value module (21 Nov 2026): pure math, no outside call, refused for "several steps" on its bisection
/// closure and for "rules" on a Gordon clamp and a NaN check.
const PURE_MATH: &str = r#"pub fn run(ctx: &Ctx) -> Value {
    let m: Model = ctx.input();
    let price = m.price;
    let r = m.discount / 100.0;
    let terminal = (m.tg / 100.0).min(r - 0.005);
    let gap = |rate: f64| dcf(&m, rate, terminal).map(|d| d.per_share - price);
    let (mut lo, mut hi) = (-0.5, 1.0);
    for _ in 0..80 {
        let mid = (lo + hi) / 2.0;
        let (flo, fhi) = (gap(lo).unwrap_or(f64::NAN), gap(mid).unwrap_or(f64::NAN));
        if flo.is_nan() || fhi.is_nan() {
            break;
        }
        if flo * fhi <= 0.0 { hi = mid } else { lo = mid }
    }
    json!({ "implied_rate": (lo + hi) / 2.0 })
}
"#;

#[test]
fn test_calls_out_reads_the_sdk_calls_and_nothing_else() {
    assert!(!source::calls_out(PURE_MATH));
    assert!(!source::calls_out(
        "// calls http_get later\nfn run() { /* connect_call */ }\n"
    ));
    for code in [
        "fn run(ctx: &Ctx) { ctx.http_get(url); }",
        "fn run(ctx: &Ctx) { ctx.http_post(url, body); }",
        "fn run(ctx: &Ctx) { ctx.http_call_batch(&reqs); }",
        "fn run(ctx: &Ctx) { ctx.connect_call(\"gmail\", op); }",
        "fn run() { unsafe { host_http_call(0, 0) }; }",
        "fn run() { unsafe { host_connect_call(0, 0) }; }",
        "fn run(ctx: &Ctx) { let s = http_stream_begin(ctx); }",
        "mod client;\nfn run() {}",
        "pub(crate) mod client;\nfn run() {}",
    ] {
        assert!(source::calls_out(code), "{code}");
    }
    assert!(
        !source::calls_out("mod math {\n    pub fn f() {}\n}\nfn run() {}"),
        "a module written inline is read"
    );
}

#[tokio::test]
async fn test_pure_math_is_never_several_steps_or_waiting() {
    for kind in ["several_steps", "waits"] {
        let judge = Stub::finding(kind, "dcf(");
        let review = review_module(
            &judge,
            &module("value", PURE_MATH),
            &specs("value"),
            &names(),
        )
        .await
        .expect("the stub answers");
        assert_eq!(review.findings, Vec::new(), "{kind} on pure math");
    }
}

#[tokio::test]
async fn test_a_module_that_calls_out_can_still_be_several_steps() {
    let calls = PURE_MATH.replace(
        "let m: Model = ctx.input();",
        "let m: Model = ctx.http_get(&ctx.input::<String>())?.json();",
    );
    let judge = Stub::finding("several_steps", "dcf(");
    let review = review_module(&judge, &module("value", &calls), &specs("value"), &names())
        .await
        .expect("the stub answers");
    assert_eq!(review.findings.len(), 1);
    assert_eq!(review.findings[0].kind, "several_steps");
}

#[test]
fn test_rules_and_hidden_logic_say_a_numerical_guard_is_not_them() {
    for kind in ["rules", "hidden_logic"] {
        let item = ITEMS
            .iter()
            .find(|item| item.kind == kind)
            .expect("an item");
        for guard in ["clamp", "NaN", "infinity", "zero divisor"] {
            assert!(item.shows.contains(guard), "{kind} names {guard}");
        }
    }
}
