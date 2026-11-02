//! The install's review of its app's modules ([`crate::module_review`]). It starts when the install starts, so Jev
//! reads beside the install's own proofs and writes, and it goes on after the install answers, since Jev takes seconds
//! and an app's change may not. Advisory: it never refuses or holds an install. Only an install that succeeds lets it
//! take the place of the app's review before it and keep what it says (`PlatformStore::save_module_review`), so a failed
//! push costs neither. A caller that answers with all of it waits (`InstallResult::with_reviews_read`).
//!
//! A module is read again when its code changed or when a spec that runs it changed, since Jev reads a module against
//! the specs that run it: a new guard or a split state can answer a finding, or make one.

use std::path::{Component, Path};
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::PlatformStore;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};

use super::reconcile;
use super::types::{AppBundle, InstallWarning, WasmModuleManifest};
use crate::module_review::{
    BundleReview, Finding, ModuleSource, ReviewsUnderWay, SPEC_BYTES, SpecSource, said_left_out,
};
use crate::state::PlatformState;

/// Source past this is not read. The largest app module seen was 1,175 lines, about 60 KB (census of Rita's apps, 26
/// Oct 2026); each 160 lines is a paid call, so a file many times that is not a module to read line by line.
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

/// The file in an app's folder that says which commit it is, when whoever made it says one.
const COMMIT_FILE: &str = "COMMIT";

/// What the review said, and which modules Jev read (with or without findings).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ReviewOutcome {
    pub(super) warnings: Vec<InstallWarning>,
    pub(super) reviewed: Vec<String>,
}

/// A review an install left running: `GET /api/os-apps/{app}/review/{version}` says what became of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewAsked {
    /// The app reviewed.
    pub app: String,
    /// The version to ask about: the app's commit, else its bundle's digest.
    pub version: String,
    /// The modules that changed.
    pub modules: Vec<String>,
}

/// What a review left running comes to say, for a caller that waits for it.
#[derive(Debug, Clone)]
pub(crate) struct ReviewAnswer {
    said: watch::Receiver<Option<ReviewOutcome>>,
    asked: Vec<String>,
}

impl ReviewAnswer {
    /// Never fails: a review that stopped before it answered is said as one note.
    pub(super) async fn read(mut self) -> ReviewOutcome {
        match self.said.wait_for(Option::is_some).await {
            Ok(said) => said.clone().unwrap_or_default(),
            Err(_) => ReviewOutcome {
                warnings: vec![stopped_note(&self.asked)],
                reviewed: Vec::new(),
            },
        }
    }
}

/// Jev reading. An install that fails before it answers leaves him reading nothing, and paying for nothing, behind it.
struct JevReading(JoinHandle<BundleReview>);

impl Drop for JevReading {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A review under way beside the install. Nothing depends on it until the install has succeeded.
#[derive(Default)]
pub(super) struct PendingReview(Option<Reading>);

struct Reading {
    jev: Option<JevReading>,
    said: Vec<InstallWarning>,
    asked: Vec<String>,
    changed: Vec<String>,
    tenant: String,
    app: String,
    version: String,
    store: Option<Arc<dyn PlatformStore>>,
    under_way: Arc<ReviewsUnderWay>,
}

impl PendingReview {
    /// The install succeeded and answers now. The review goes on without it, in place of the app's review before it.
    pub(super) fn left_running(self) -> (Vec<ReviewAsked>, Vec<ReviewAnswer>) {
        let Some(reading) = self.0 else {
            return (Vec::new(), Vec::new());
        };
        let (tell, said) = watch::channel(None);
        let asked = ReviewAsked {
            app: reading.app.clone(),
            version: reading.version.clone(),
            modules: reading.changed.clone(),
        };
        let answer = ReviewAnswer {
            said,
            asked: reading.asked.clone(),
        };
        let under_way = Arc::clone(&reading.under_way);
        let tenant = reading.tenant.clone();
        under_way.start(&tenant, &asked.app, &asked.version, reading.finish(tell));
        (vec![asked], vec![answer])
    }
}

impl Reading {
    async fn finish(mut self, tell: watch::Sender<Option<ReviewOutcome>>) {
        let review: Result<BundleReview, JoinError> = match self.jev.as_mut() {
            Some(jev) => (&mut jev.0).await,
            None => Ok(BundleReview::default()),
        };
        let outcome = match review {
            Ok(review) => outcome_of(review, self.said, &self.asked),
            Err(error) => {
                tracing::warn!(error = %error, "module review stopped");
                self.said.push(stopped_note(&self.asked));
                ReviewOutcome {
                    warnings: self.said,
                    reviewed: Vec::new(),
                }
            }
        };
        let kept = json!({ "warnings": outcome.warnings, "modules_reviewed": outcome.reviewed });
        // Before the save: a write takes its turn behind every other, and a start or a Genesis install has what it
        // waits for by now. The review stays on the list until the save is done, so the route misses nothing.
        let _ = tell.send(Some(outcome));
        if let Some(store) = self.store
            && let Err(error) = store
                .save_module_review(&self.tenant, &self.app, &self.version, &kept.to_string())
                .await
        {
            tracing::warn!(error = %error, app = %self.app, "module review was not kept");
        }
    }
}

/// Start reviewing the modules of `bundle` whose code or running specs differ from what `tenant` has, when the
/// install puts modules (`wasm`).
pub(super) async fn start(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    app_dir: &Path,
    bundle: &AppBundle,
    wasm: bool,
) -> PendingReview {
    if !wasm || bundle.wasm_modules.is_empty() {
        return PendingReview::default();
    }
    let specs: Vec<SpecSource> = bundle
        .specs
        .iter()
        .map(|(entity_type, source)| SpecSource::new(entity_type, source))
        .collect();
    let changed = changed_modules(state, tenant, bundle, &specs).await;
    if changed.is_empty() {
        return PendingReview::default();
    }
    let reviewer = state.module_reviewer.get().cloned();
    let mut said = Vec::new();
    let mut modules = Vec::with_capacity(changed.len());
    if reviewer.is_none() {
        said.push(note(
            "Modules were not reviewed: no Jev is set up on this server.".to_string(),
            json!({ "modules": changed }),
        ));
    } else {
        for module in &changed {
            match read_source(app_dir, module, bundle.wasm_module_configs.get(module)) {
                Ok(Some(source)) => modules.push(source),
                // A built module with no source beside it (one the platform adds to every app): nothing to read.
                Ok(None) => {
                    tracing::info!(module = %module, "module review: no source in the bundle")
                }
                Err(why) => said.push(note(
                    format!("Module {module} was not reviewed: {why}."),
                    json!({ "module": module }),
                )),
            }
        }
    }
    if modules.is_empty() && said.is_empty() {
        return PendingReview::default();
    }
    let asked: Vec<String> = modules.iter().map(|module| module.module.clone()).collect();
    let version = version_of(app_dir, tenant, app_name, bundle);
    let store = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone());
    let jev = reviewer.filter(|_| !modules.is_empty()).map(|reviewer| {
        JevReading(tokio::spawn(async move {
            reviewer.review(&modules, &specs).await
        }))
    });
    PendingReview(Some(Reading {
        jev,
        said,
        asked,
        changed,
        tenant: tenant.to_string(),
        app: app_name.to_string(),
        version,
        store,
        under_way: Arc::clone(&state.module_reviews),
    }))
}

/// What names a version for its review: the commit its files say, else its bundle's digest.
fn version_of(app_dir: &Path, tenant: &str, app_name: &str, bundle: &AppBundle) -> String {
    std::fs::read_to_string(app_dir.join(COMMIT_FILE))
        .ok()
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| {
            reconcile::digest_app_bundle(Some(tenant), app_name, bundle).bundle_digest
        })
}

/// The modules whose code differs from the stored module, or that a changed spec runs (every module, when a changed
/// spec runs none by name).
async fn changed_modules(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
    specs: &[SpecSource],
) -> Vec<String> {
    let stored = state
        .server
        .load_wasm_module_sources(tenant)
        .await
        .unwrap_or_default();
    let tenant_id = TenantId::new(tenant);
    let changed_specs: Vec<&SpecSource> = {
        let registry = state
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        specs
            .iter()
            .filter(|spec| {
                registry
                    .get_spec(&tenant_id, &spec.entity_type)
                    .is_none_or(|existing| {
                        temper_store_turso::spec_content_hash(&existing.ioa_source)
                            != temper_store_turso::spec_content_hash(&spec.source)
                    })
            })
            .collect()
    };
    bundle
        .wasm_modules
        .iter()
        .filter(|(module, bytes)| {
            let hash = temper_wasm::WasmEngine::hash_module(bytes);
            let code_changed = stored
                .get(*module)
                .is_none_or(|existing| existing.sha256_hash != hash);
            let runs_it =
                |spec: &&SpecSource| spec.module_triggers.iter().any(|(_, runs)| runs == *module);
            let named = specs.iter().any(|spec| runs_it(&spec));
            code_changed
                || changed_specs
                    .iter()
                    .any(|spec| runs_it(spec) || (!named && spec.module_triggers.is_empty()))
        })
        .map(|(module, _)| module.clone())
        .collect()
}

/// A module's source, read only from inside the app's folder: a bundle comes from anyone who may publish one, and
/// whatever file it names is sent to Jev, so a path out of the folder (`..`, an absolute path, a link) would send
/// the server's own files to an outside service. `None` when the manifest names no source and none is at
/// `wasm/<module>/src/lib.rs`.
fn read_source(
    app_dir: &Path,
    module: &str,
    config: Option<&WasmModuleManifest>,
) -> Result<Option<ModuleSource>, String> {
    let named = config.and_then(|config| config.source.clone());
    let path = named
        .clone()
        .unwrap_or_else(|| format!("wasm/{module}/src/lib.rs"));
    if named.is_none() && !app_dir.join(&path).exists() {
        return Ok(None);
    }
    let relative = Path::new(&path);
    let inside = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(format!("its source {path} is not a path inside the app"));
    }
    let (Ok(file), Ok(root)) = (
        app_dir.join(relative).canonicalize(),
        app_dir.canonicalize(),
    ) else {
        return Err(format!("its source {path} is not in the bundle"));
    };
    if !file.starts_with(&root) {
        return Err(format!("its source {path} leads out of the app"));
    }
    let size = std::fs::metadata(&file)
        .map_err(|_| format!("its source {path} could not be read"))?
        .len();
    if size > MAX_SOURCE_BYTES {
        return Err(format!(
            "its source {path} is {} KB, past the {} KB read",
            size / 1024,
            MAX_SOURCE_BYTES / 1024
        ));
    }
    let source =
        std::fs::read_to_string(&file).map_err(|_| format!("its source {path} is not text"))?;
    Ok(Some(ModuleSource {
        module: module.to_string(),
        path,
        source,
    }))
}

/// What a finished review says, after what was known before it asked (`said`).
fn outcome_of(review: BundleReview, said: Vec<InstallWarning>, asked: &[String]) -> ReviewOutcome {
    let mut outcome = ReviewOutcome {
        warnings: said,
        reviewed: Vec::new(),
    };
    let mut findings = 0;
    let mut input_tokens = 0;
    for module in &review.reviews {
        outcome.reviewed.push(module.module.clone());
        input_tokens += module.input_tokens;
        for finding in &module.findings {
            findings += 1;
            // A warning, as it is to the install: telemetry that keeps only warnings counts findings by item.
            tracing::warn!(module = %module.module, item = finding.item, kind = %finding.kind, rating = finding.rating, "module review finding");
            outcome.warnings.push(finding_note(&module.module, finding));
        }
        if !module.left_out.is_empty() {
            tracing::warn!(module = %module.module, left_out = module.left_out.len(), "module review read part of its spec");
            outcome.warnings.push(note(
                format!(
                    "Module {} was reviewed against part of its spec, the {} KB a review reads: {} left out. What it \
                     found stands; what it did not find may be in the part left out.",
                    module.module,
                    SPEC_BYTES / 1024,
                    said_left_out(&module.left_out)
                ),
                json!({ "module": module.module, "partial": true, "left_out": module.left_out }),
            ));
        }
    }
    if let Some(reason) = &review.unavailable {
        let unreviewed: Vec<&String> = asked
            .iter()
            .filter(|module| !outcome.reviewed.contains(module))
            .collect();
        tracing::warn!(reason = %reason, modules = unreviewed.len(), "module review unavailable");
        outcome.warnings.push(note(
            reason.words().to_string(),
            json!({ "modules": unreviewed, "reason": reason.to_string() }),
        ));
    }
    tracing::info!(
        reviewed = outcome.reviewed.len(),
        findings,
        input_tokens,
        "module review"
    );
    outcome
}

fn stopped_note(asked: &[String]) -> InstallWarning {
    note(
        "Modules were not reviewed: the review stopped.".to_string(),
        json!({ "modules": asked }),
    )
}

fn note(message: String, detail: serde_json::Value) -> InstallWarning {
    InstallWarning::module_review(message, detail)
}

fn finding_note(module: &str, finding: &Finding) -> InstallWarning {
    note(
        format!(
            "{} in module {} ({}): {} {}",
            finding.title, module, finding.location, finding.why, finding.instead
        ),
        json!({
            "module": module,
            "item": finding.item,
            "kind": finding.kind,
            "title": finding.title,
            "why": finding.why,
            "where": finding.location,
            "line": finding.line,
            "instead": finding.instead,
            "rating": finding.rating,
        }),
    )
}
