//! The refusal half of the install's module review ([`super::module_review_step`]): a module whose code is new, or
//! whose sentence changed, and that gets a finding is not let in.
//!
//! Rita, 18 Nov 2026: refuse new modules and rewritten ones, on every path; an app already there is fine. So every
//! install a request makes is held to it: a push and its check, a start's `--app`, a Genesis registry install and a
//! first install. A boot replay or a rollback brings back an install already made and is never refused. Code is new
//! when this server runs it nowhere ([`super::module_review_step::changed_modules`]): a restart, a version whose
//! module is as the app had it, and a tenant taking an app another tenant here already runs install as they are. A
//! module whose code is as installed keeps any finding as advice.

use std::collections::BTreeMap;
use std::path::Path;

use super::file_source::AppFileSource;
use super::module_review_step::{changed_modules, read_source, specs_of};
use super::types::AppBundle;
use crate::module_review::{Finding, ModuleSource};
use crate::state::PlatformState;

/// Refuse a pushed version of an app (`files`, as its author sent them) before its files are kept, as the install
/// that follows would ([`refuse_findings`]). Files that do not read as an app are left to the install, which says why.
pub(crate) async fn refuse_pushed_findings(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    files: &AppFileSource,
) -> Result<(), String> {
    let Ok(bundle) = super::load_app_bundle(files) else {
        return Ok(());
    };
    refuse_findings(state, tenant, app_name, files.dir(), &bundle, true).await
}

/// Refuse the install when Jev finds, in a module of `bundle` whose code is new to this server or whose sentence
/// changed, something the spec should do, or a sentence its code does not do: each finding is named with its line and
/// what to do instead. It uses the review's own rules (an item rated at [`crate::module_review::FOUND_AT`] or more, or
/// a `does_mismatch`) and its cache, so the advisory review after it reads nothing twice.
pub(super) async fn refuse_findings(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    app_dir: &Path,
    bundle: &AppBundle,
    wasm: bool,
) -> Result<(), String> {
    let Some(reviewer) = state.module_reviewer.get().cloned() else {
        return Ok(());
    };
    if !wasm || bundle.wasm_modules.is_empty() {
        return Ok(());
    }
    let specs = specs_of(bundle);
    let modules: Vec<ModuleSource> = changed_modules(state, tenant, app_name, bundle, &specs)
        .await
        .into_iter()
        .filter(|(_, held)| *held)
        // A module with no source, or one that cannot be read, is said by the advisory review.
        .filter_map(|(module, _)| {
            read_source(app_dir, &module, bundle.wasm_module_configs.get(&module))
                .ok()
                .flatten()
        })
        .collect();
    if modules.is_empty() {
        return Ok(());
    }
    // A Jev that does not answer refuses nothing: the module is left out of the review, the advisory review says so
    // once, and nothing retries. A Jev outage must not stop every install.
    let review = reviewer.review(&modules, &specs).await;
    let found: Vec<String> = review
        .reviews
        .iter()
        .flat_map(|module| {
            module
                .findings
                .iter()
                // Size alone is advice, never a refusal: it is a count, not something the spec should do, and a long
                // module that only computes is allowed. The questions Jev answers are what refuse.
                .filter(|finding| finding.kind != "large_module")
                .map(|finding| refusal_line(&module.module, finding))
        })
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Refused: Jev's review found work in a new or changed module that belongs in the spec, or a sentence on a \
         module that is not what it does. A module computes a value or calls one outside service; the spec's states, \
         guards, timers and policies decide the move, where the checker proves it. Move each into the spec, or say \
         what the module does, and install again.\n{}",
        found.join("\n")
    ))
}

/// One finding as a refusal says it: what, in which module, at which line, and what to move into the spec.
fn refusal_line(module: &str, finding: &Finding) -> String {
    let line = finding
        .line
        .as_deref()
        .map_or_else(String::new, |line| format!(" `{line}`"));
    format!(
        "- {} ({}) in module {module} at {}{line}: {} {}",
        finding.title, finding.kind, finding.location, finding.why, finding.instead
    )
}

/// Where a kept review keeps the sentences of the version it read ([`sentences_of`]).
pub(super) const SENTENCES: &str = "does";

/// Each module's sentence on what it computes (`does` in the app's manifest), by module, as a review keeps it.
pub(super) fn sentences_of(bundle: &AppBundle) -> BTreeMap<String, String> {
    bundle
        .wasm_modules
        .keys()
        .filter_map(|module| {
            let does = bundle
                .wasm_module_configs
                .get(module)?
                .does
                .as_deref()?
                .trim();
            (!does.is_empty()).then(|| (module.clone(), does.to_string()))
        })
        .collect()
}

/// The sentences the app's last kept review was of ([`sentences_of`]), or None when no review kept them.
pub(super) async fn kept_sentences(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Option<BTreeMap<String, String>> {
    let store = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())?;
    let row = store.load_module_review(tenant, app_name).await.ok()??;
    let kept: serde_json::Value = serde_json::from_str(&row.result).ok()?;
    serde_json::from_value(kept.get(SENTENCES)?.clone()).ok()
}
