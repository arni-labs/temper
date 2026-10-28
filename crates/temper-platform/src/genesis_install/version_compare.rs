//! How two versions of one Genesis app relate in its commit history.
//!
//! An installer that already runs a dependency at one version, and is asked
//! to install it at another, needs to know which of the two is newer before it
//! moves anything. Genesis keeps every commit's parents (`Commit.ParentShas`),
//! so the registry can answer that from its own rows. This module is that
//! answer: a bounded walk over the parent graph, behind
//! `GET /api/genesis/apps/{owner}/{name}/compare/{base}...{head}`.

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use temper_runtime::tenant::TenantId;
use temper_server::state::ServerState;

use super::cache_paths::validate_git_object_id;
use super::object_lookup::load_genesis_object;
use super::{
    BundleAudience, refuse_unless_repository_is_public, resolve_genesis_app_by_ref, string_field,
};

/// The most commits one direction of a comparison reads before it gives up.
///
/// Each step loads one `Commit` row, and the compare route serves anonymous
/// callers, so without a bound one request could make the registry read a
/// repository's whole history. 4096 matches Genesis's own merge-base walk
/// (`MERGE_BASE_WALK_MAX_COMMITS` in genesis `wasm/scm_merge_pr`), which bounds
/// the same graph for the same reason. When it runs out the answer is
/// [`VersionComparison::Unknown`], never a guess.
pub const MAX_VERSION_COMPARE_WALK_COMMITS: usize = 4096;

/// How `head` relates to `base` in an app's commit history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionComparison {
    /// `base` and `head` are the same commit.
    Identical,
    /// `head` descends from `base`.
    Ahead,
    /// `base` descends from `head`.
    Behind,
    /// Neither descends from the other.
    Diverged,
    /// The walk ran out of budget, or met a parent the registry has no row
    /// for, before it could tell.
    Unknown,
}

/// Body of the compare route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisVersionCompareResponse {
    /// How `head` relates to `base`.
    pub status: VersionComparison,
}

/// Where a comparison reads a commit's parents.
pub(super) trait CommitHistory {
    /// The parents of `sha`, or `None` when the repository has no such commit.
    async fn parents(&self, sha: &str) -> Result<Option<Vec<String>>, String>;
}

/// What one directed walk found.
enum Reach {
    Found,
    Absent,
    Undecided,
}

/// Compare two commits of one history. Both must exist in it.
pub(super) async fn compare_in_history<H: CommitHistory>(
    history: &H,
    base: &str,
    head: &str,
) -> Result<VersionComparison, String> {
    for sha in [base, head] {
        if history.parents(sha).await?.is_none() {
            return Err(format!("Genesis commit {sha} not found"));
        }
    }
    if base == head {
        return Ok(VersionComparison::Identical);
    }
    // Each direction is walked on its own budget. A walk that cannot finish
    // (a long history, or a missing row) does not settle the question: the
    // other direction may still find its target in a few steps, which is the
    // common case when the installed version is a recent child of the pin.
    let forward = reaches(history, head, base).await?;
    if matches!(forward, Reach::Found) {
        return Ok(VersionComparison::Ahead);
    }
    let backward = reaches(history, base, head).await?;
    Ok(match (forward, backward) {
        (_, Reach::Found) => VersionComparison::Behind,
        (Reach::Absent, Reach::Absent) => VersionComparison::Diverged,
        _ => VersionComparison::Unknown,
    })
}

/// Whether `target` is an ancestor of `from`, walking parents breadth-first.
///
/// A parent the registry has no row for is stepped over, so another branch
/// can still reach the target, but the walk can then no longer claim the
/// target is absent.
async fn reaches<H: CommitHistory>(history: &H, from: &str, target: &str) -> Result<Reach, String> {
    let mut queue = VecDeque::from([from.to_string()]);
    let mut seen = BTreeSet::from([from.to_string()]);
    let mut complete = true;
    while let Some(sha) = queue.pop_front() {
        let Some(parents) = history.parents(&sha).await? else {
            complete = false;
            continue;
        };
        for parent in parents {
            if parent == target {
                return Ok(Reach::Found);
            }
            if seen.insert(parent.clone()) {
                if seen.len() > MAX_VERSION_COMPARE_WALK_COMMITS {
                    return Ok(Reach::Undecided);
                }
                queue.push_back(parent);
            }
        }
    }
    Ok(if complete {
        Reach::Absent
    } else {
        Reach::Undecided
    })
}

/// A repository's commit rows as Genesis stored them.
struct StoredCommits<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    repository_id: &'a str,
}

impl CommitHistory for StoredCommits<'_> {
    async fn parents(&self, sha: &str) -> Result<Option<Vec<String>>, String> {
        let Ok(sha) = validate_git_object_id(sha) else {
            return Ok(None);
        };
        let commit =
            load_genesis_object(self.state, self.tenant, "Commit", self.repository_id, sha).await?;
        Ok(commit.map(|commit| {
            // Genesis writes ParentShas as a comma-joined list (scm_ingest_pack).
            string_field(&commit.state.fields, "ParentShas")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|parent| !parent.is_empty())
                .map(str::to_string)
                .collect()
        }))
    }
}

/// Split a `{base}...{head}` path segment into two validated commit ids.
pub fn parse_compare_range(range: &str) -> Result<(String, String), String> {
    let (base, head) = range
        .split_once("...")
        .ok_or_else(|| "compare range must be {base}...{head}".to_string())?;
    Ok((
        validate_git_object_id(base)?.to_string(),
        validate_git_object_id(head)?.to_string(),
    ))
}

/// Compare two versions of one Genesis app, bounded by who is asking.
///
/// An anonymous caller gets an answer only when the app's repository is
/// public, the same rule the bundle route applies; the comparison reads no
/// other repository.
pub async fn compare_genesis_app_versions(
    state: &ServerState,
    registry_tenant: &str,
    owner: &str,
    name: &str,
    base: &str,
    head: &str,
    audience: BundleAudience,
) -> Result<VersionComparison, String> {
    let tenant = TenantId::new(registry_tenant);
    let app = resolve_genesis_app_by_ref(state, &tenant, owner, name, head).await?;
    if audience == BundleAudience::Anonymous {
        refuse_unless_repository_is_public(state, &tenant, &app).await?;
    }
    let history = StoredCommits {
        state,
        tenant: &tenant,
        repository_id: &app.repository_id,
    };
    compare_in_history(&history, base, head).await
}

#[cfg(test)]
#[path = "version_compare_test.rs"]
mod tests;
