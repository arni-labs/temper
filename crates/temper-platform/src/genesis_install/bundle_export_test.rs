//! Exports of one bundle running at once, as their callers see them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::super::GenesisAppBundle;
use super::super::bundles::GenesisBundleBudget;
use super::super::cache_paths::{replace_directory, stage_beside};
use super::{MaterializeTree, export_closure_files};

const EXPORTS_AT_ONCE: usize = 8;
const ROUNDS: usize = 20;
const FILES_PER_APP: usize = 16;

fn closure() -> Vec<GenesisAppBundle> {
    ["notes", "tasks"]
        .into_iter()
        .map(|name| GenesisAppBundle {
            owner: "acme".to_string(),
            name: name.to_string(),
            repository_id: format!("repo-{name}"),
            version_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
        })
        .collect()
}

/// Fixed files standing in for an app's Genesis rows, published the way
/// `materialize_commit_tree` publishes a tree: staged beside the app's
/// directory, then moved into place. Yields between files so exports running
/// at once interleave.
struct FixedTrees;

impl MaterializeTree for FixedTrees {
    async fn materialize(
        &self,
        app: &GenesisAppBundle,
        app_dir: &Path,
        _budget: &mut GenesisBundleBudget,
    ) -> Result<BTreeSet<String>, String> {
        let staged = stage_beside(app_dir)?;
        let mut paths = BTreeSet::new();
        for index in 0..FILES_PER_APP {
            let rel = format!("specs/file-{index}.ioa.toml");
            let path = staged.path().join(&rel);
            std::fs::create_dir_all(path.parent().expect("parent")).map_err(|e| e.to_string())?;
            std::fs::write(&path, format!("{} {index}\n", app.name)).map_err(|e| e.to_string())?;
            paths.insert(rel);
            tokio::task::yield_now().await;
        }
        replace_directory(staged.keep(), app_dir)?;
        Ok(paths)
    }
}

/// A TemperPaw boot fetches many bundles at once, and two boots or two tenants
/// can ask for the same one. Every export of it must answer with its files.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn exports_of_one_bundle_at_once_all_return_the_same_files() {
    let base = tempfile::tempdir().expect("cache base");
    for round in 0..ROUNDS {
        let exports: Vec<_> = (0..EXPORTS_AT_ONCE)
            .map(|_| {
                let base: PathBuf = base.path().to_path_buf();
                tokio::spawn(async move {
                    export_closure_files(&base, "default", closure(), &FixedTrees).await
                })
            })
            .collect();
        let mut answers = Vec::new();
        for export in exports {
            let apps = export
                .await
                .expect("export task")
                .unwrap_or_else(|error| panic!("round {round}: an export failed: {error}"));
            answers.push(serde_json::to_vec(&apps).expect("serialize"));
        }
        let first = &answers[0];
        let files: usize =
            serde_json::from_slice::<Vec<super::super::GenesisRegistryBundleApp>>(first)
                .expect("bundle apps")
                .iter()
                .map(|app| app.files.len())
                .sum();
        assert_eq!(
            files,
            2 * FILES_PER_APP,
            "round {round}: every file came back"
        );
        assert!(
            answers.iter().all(|answer| answer == first),
            "round {round}: every export returned the same bytes"
        );
    }
    let left: Vec<_> = std::fs::read_dir(base.path())
        .expect("cache base")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(left.is_empty(), "an export leaves nothing behind: {left:?}");
}
