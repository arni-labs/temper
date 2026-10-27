//! The admin and debug route that clones an app closure over the registry's
//! smart-HTTP surface instead of fetching its bundle.
//!
//! Used only when `TEMPER_GENESIS_INSTALL_GIT_FALLBACK` is set; every normal
//! install and recovery reads the bundle.

use std::collections::BTreeSet;
use std::path::Path;

use super::cache_paths::{app_cache_dir, validate_git_object_id, validate_identity_component};
use super::{RegistryAppRef, parse_dependency_ref, read_manifest_dependencies};

async fn materialize_git_registry_app(
    registry_url: &str,
    owner: &str,
    name: &str,
    version_hash: Option<&str>,
    app_dir: &Path,
) -> Result<String, String> {
    validate_identity_component("owner", owner)?;
    validate_identity_component("app name", name)?;
    let remote = registry_git_url(registry_url, owner, name);
    let git_dir = app_dir.join(".git");
    if app_dir.exists() && !git_dir.is_dir() {
        std::fs::remove_dir_all(app_dir).map_err(|error| {
            format!(
                "remove stale Genesis app cache '{}': {error}",
                app_dir.display()
            )
        })?;
    }
    if !git_dir.is_dir() {
        if let Some(parent) = app_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create Genesis app cache parent '{}': {error}",
                    parent.display()
                )
            })?;
        }
        let app_dir_arg = app_dir.display().to_string();
        run_git(None, &["clone", &remote, &app_dir_arg]).await?;
    } else {
        run_git(Some(app_dir), &["remote", "set-url", "origin", &remote]).await?;
        run_git(Some(app_dir), &["fetch", "origin", "--tags", "--prune"]).await?;
    }

    if let Some(hash) = version_hash {
        let hash = validate_git_object_id(hash)?;
        run_git(Some(app_dir), &["checkout", "--detach", hash]).await?;
    }

    let resolved = run_git(Some(app_dir), &["rev-parse", "HEAD"]).await?;
    let resolved = resolved.trim();
    if resolved.is_empty() {
        Err(format!(
            "git rev-parse returned an empty hash for {owner}/{name}"
        ))
    } else {
        Ok(resolved.to_string())
    }
}

pub(super) async fn materialize_registry_app_closure(
    registry_url: &str,
    root_ref: RegistryAppRef,
    cache_root: &Path,
) -> Result<Vec<RegistryAppRef>, String> {
    let mut stack = vec![root_ref];
    let mut seen = BTreeSet::new();
    let mut materialized_refs = Vec::new();

    while let Some(app_ref) = stack.pop() {
        let key = format!("{}/{}", app_ref.owner, app_ref.name);
        if !seen.insert(key) {
            continue;
        }

        let app_dir = app_cache_dir(cache_root, &app_ref.name)?;
        let resolved_hash = materialize_git_registry_app(
            registry_url,
            &app_ref.owner,
            &app_ref.name,
            app_ref.version_hash.as_deref(),
            &app_dir,
        )
        .await?;
        materialized_refs.push(RegistryAppRef {
            owner: app_ref.owner.clone(),
            name: app_ref.name.clone(),
            version_hash: Some(resolved_hash),
        });

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = parse_dependency_ref(&dependency, &app_ref.owner);
            stack.push(RegistryAppRef {
                owner: dependency.owner.unwrap_or_else(|| app_ref.owner.clone()),
                name: dependency.name,
                version_hash: dependency.version_hash,
            });
        }
    }

    Ok(materialized_refs)
}

pub(super) fn registry_git_url(registry_url: &str, owner: &str, name: &str) -> String {
    format!(
        "{}/{}/{}.git",
        registry_url.trim_end_matches('/'),
        owner,
        name
    )
}

async fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("git");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .await
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!(
        "git {} failed with status {}: {}{}{}",
        args.join(" "),
        output.status,
        stderr.trim(),
        if stderr.trim().is_empty() || stdout.trim().is_empty() {
            ""
        } else {
            "\n"
        },
        stdout.trim()
    ))
}
