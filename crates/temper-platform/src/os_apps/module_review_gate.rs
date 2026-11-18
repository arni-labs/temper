//! The refusal half of the install's module review ([`super::module_review_step`]): a push whose new or changed
//! module gets a finding is not let in.
//!
//! Only a push is refused, its check too: its files are its author's change. A Genesis registry install, a start's
//! `--app` from the catalog and a restore install a version already published, and a refusal there would fall on
//! whoever installs it, such as a fresh tenant taking an app as shipped, never on the author. Those are advised on.

use std::path::Path;

use super::file_source::AppFileSource;
use super::module_review_step::{changed_modules, read_source, specs_of};
use super::types::AppBundle;
use crate::module_review::{Finding, ModuleSource};
use crate::state::PlatformState;

/// Refuse the install when Jev finds, in a module of `bundle` whose code is new or differs from what `tenant` has,
/// something the spec should do: each finding is named with its line and what to move into the spec. It uses the
/// review's own rules (an item rated at [`crate::module_review::FOUND_AT`] or more, or a module past its size), and its
/// cache, so the advisory review after it reads nothing twice.
///
/// Rita, 17 Nov 2026: every Jev module check belongs to Temper, and a module that decides is refused. Untouched code is
/// never refused for it, so the apps that run now are not blocked until someone changes that module.
/// Refuse a pushed version of an app (`files`, as its author sent them) as [`refuse_findings`] does. Files that do not
/// read as an app are left to the install, which says why.
pub(crate) async fn refuse_pushed_findings(
    state: &PlatformState,
    tenant: &str,
    files: &AppFileSource,
) -> Result<(), String> {
    let Ok(bundle) = super::load_app_bundle(files) else {
        return Ok(());
    };
    refuse_findings(state, tenant, files.dir(), &bundle, true).await
}

async fn refuse_findings(
    state: &PlatformState,
    tenant: &str,
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
    let modules: Vec<ModuleSource> = changed_modules(state, tenant, bundle, &specs)
        .await
        .into_iter()
        .filter(|(_, code_changed)| *code_changed)
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
                .map(|finding| refusal_line(&module.module, finding))
        })
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Refused: Jev's review found work in a new or changed module that belongs in the spec. A module computes a \
         value or calls one outside service; the spec's states, guards, timers and policies decide the move, where the \
         checker proves it. Move each into the spec and install again.\n{}",
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
