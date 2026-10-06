//! Policy and compiled module checks for `temper verify`.
use anyhow::{Context, Result};
use std::path::Path;

/// Require each IOA policy and referenced compiled module, then parse all policies.
pub(super) fn validate(specs: &Path) -> Result<()> {
    let mut entities = 0;
    for entry in std::fs::read_dir(specs)? {
        let path = entry?.path();
        let Some(stem) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".ioa.toml"))
        else {
            continue;
        };
        let automaton = temper_spec::automaton::parse_automaton(&std::fs::read_to_string(&path)?)?;
        for module in automaton
            .actions
            .iter()
            .flat_map(|a| &a.triggers)
            .filter_map(|t| t.module.as_deref())
            .chain(
                automaton
                    .integrations
                    .iter()
                    .filter_map(|i| i.module.as_deref()),
            )
        {
            anyhow::ensure!(
                !module.is_empty()
                    && module
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "invalid module name"
            );
            anyhow::ensure!(
                specs
                    .join("modules")
                    .join(format!("{module}.wasm"))
                    .is_file(),
                "missing compiled WASM module {module}"
            );
        }
        anyhow::ensure!(
            specs
                .join("policies")
                .join(format!("{stem}.cedar"))
                .is_file(),
            "missing Cedar policy for {stem}"
        );
        entities += 1;
    }
    anyhow::ensure!(entities > 0, "source contains no IOA specifications");
    validate_policy_files(specs)
}

/// Parse every policy the server loads, in the same lexical filename order.
/// Source collections may omit the policy directory before application packaging.
pub(super) fn validate_policy_files(specs: &Path) -> Result<()> {
    let directory = specs.join("policies");
    if !directory.exists() {
        return Ok(());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&directory)? {
        let path = entry?.path();
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cedar"))
        {
            files.push(path);
        }
    }
    files.sort();
    let mut policies = Vec::new();
    for path in files {
        policies.push(
            std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read Cedar policy {}", path.display()))?,
        );
    }
    temper_authz::AuthzEngine::new(&policies.join("\n\n")).context("invalid Cedar policy")?;
    Ok(())
}
