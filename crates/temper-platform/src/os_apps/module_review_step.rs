//! The install's review of its app's modules ([`crate::module_review`]). It starts when the install starts, so Jev
//! reads beside the install's own proofs and writes, and is joined when the install answers. Advisory: it adds
//! `module_review` warnings and never refuses an install.
//!
//! A module is read again when its code changed or when a spec that runs it changed, since Jev reads a module against
//! the specs that run it: a new guard or a split state can answer a finding, or make one.

use std::path::{Component, Path};
use std::sync::Arc;

use serde_json::json;
use temper_runtime::tenant::TenantId;
use tokio::task::JoinHandle;

use super::types::{AppBundle, InstallWarning, WasmModuleManifest};
use crate::module_review::{
    BundleReview, Finding, ModuleSource, SPEC_BYTES, SpecSource, said_left_out,
};
use crate::state::PlatformState;

/// Source past this is not read. The largest app module seen was 1,175 lines, about 60 KB (census of Rita's apps, 26
/// Oct 2026); each 160 lines is a paid call, so a file many times that is not a module to read line by line.
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

/// What the review said, and which modules Jev read (with or without findings).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ReviewOutcome {
    pub(super) warnings: Vec<InstallWarning>,
    pub(super) reviewed: Vec<String>,
}

/// A review under way beside the install.
#[derive(Default)]
pub(super) struct PendingReview {
    task: Option<JoinHandle<BundleReview>>,
    asked: Vec<String>,
    said: Vec<InstallWarning>,
}

/// An install that fails before it answers leaves no review running behind it.
impl Drop for PendingReview {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start reviewing the modules of `bundle` whose code or running specs differ from what `tenant` has.
pub(super) async fn start(
    state: &PlatformState,
    tenant: &str,
    app_dir: &Path,
    bundle: &AppBundle,
) -> PendingReview {
    if bundle.wasm_modules.is_empty() {
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
    let Some(reviewer) = state.module_reviewer.get().cloned() else {
        let said = vec![note(
            "Modules were not reviewed: no Jev is set up on this server.".to_string(),
            json!({ "modules": changed }),
        )];
        return PendingReview {
            task: None,
            asked: Vec::new(),
            said,
        };
    };
    let mut said = Vec::new();
    let mut modules = Vec::with_capacity(changed.len());
    for module in &changed {
        match read_source(app_dir, module, bundle.wasm_module_configs.get(module)) {
            Ok(Some(source)) => modules.push(source),
            // A built module with no source beside it (one the platform adds to every app): nothing to read.
            Ok(None) => tracing::info!(module = %module, "module review: no source in the bundle"),
            Err(why) => said.push(note(
                format!("Module {module} was not reviewed: {why}."),
                json!({ "module": module }),
            )),
        }
    }
    let asked: Vec<String> = modules.iter().map(|module| module.module.clone()).collect();
    let task = (!modules.is_empty()).then(|| {
        let reviewer = Arc::clone(&reviewer);
        tokio::spawn(async move { reviewer.review(&modules, &specs).await })
    });
    debug_assert!(asked.len() + said.len() <= changed.len());
    PendingReview { task, asked, said }
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

impl PendingReview {
    /// Wait for the review and say what it found. Never fails: a review that did not finish is said as one note.
    pub(super) async fn finish(mut self) -> ReviewOutcome {
        let mut outcome = ReviewOutcome {
            warnings: std::mem::take(&mut self.said),
            reviewed: Vec::new(),
        };
        let Some(task) = self.task.take() else {
            return outcome;
        };
        let review = match task.await {
            Ok(review) => review,
            Err(error) => {
                tracing::warn!(error = %error, "module review stopped");
                outcome.warnings.push(note(
                    "Modules were not reviewed: the review stopped.".to_string(),
                    json!({ "modules": self.asked }),
                ));
                return outcome;
            }
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
            let unreviewed: Vec<&String> = self
                .asked
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
}

fn note(message: String, detail: serde_json::Value) -> InstallWarning {
    InstallWarning::module_review(message, detail)
}

fn finding_note(module: &str, finding: &Finding) -> InstallWarning {
    note(
        format!(
            "{} in module {} ({}): {}",
            finding.title, module, finding.location, finding.why
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
