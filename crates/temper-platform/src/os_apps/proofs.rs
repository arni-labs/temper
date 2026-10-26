//! Joint proofs an install waits for, run apart from it and kept by what
//! they read.
//!
//! A joint walk of a large app can run for many minutes; what bounds it is
//! the memory this host leaves it ([`HOST_ROOM`]), not a clock. It runs as
//! its own task, so an install whose client gives up does not stop it, and
//! without the tenant's install lock, so the tenant's other installs go on
//! meanwhile: each attached install receives its verdict. On success the
//! install starts again under the lock and checks its current inputs.
//!
//! A verdict is kept by two digests ([`Keys`]): of what its walk read, and of
//! the specs and Cedar it was read from. An install that changes a spec or
//! the Cedar only where a rule's cut does not reach finds the first; a start
//! on a new kernel, whose models may read differently, finds the second.
//! Only a change to both walks again. A proven verdict is kept in the
//! platform's blob table, so a start that installs again what an earlier
//! install proved (a Katagami app's new Temper must serve within 20 s of the
//! old one's TERM) reads it instead of walking again, whatever checker made
//! it: those specs already run. A refusal is kept only while this process runs,
//! so a later checker decides it again. A walk that did not fit in its
//! memory says nothing of the specs: the install that waited for it reads it
//! once, and the next walks again.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_server::ServerState;
use temper_server::host_room::HOST_ROOM;
use temper_server::state::DesignTimeEvent;
use temper_verify::composite::{Bound, Progress};
use tokio::sync::watch;

use crate::state::PlatformState;

/// How often a running proof says how far it got: often enough that someone
/// watching an install sees it move, a few rows a minute in the event log.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

const MB: usize = 1 << 20;

/// What a proof found: the results that held, in words, and why the
/// install is refused when one did not.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Verdict {
    pub(super) held: Vec<String>,
    pub(super) refused: Option<String>,
}

/// What a walk returns: its verdict, and whether the walk fit in its memory
/// (a verdict about the specs, not about this host).
pub(super) struct Walked {
    pub(super) verdict: Verdict,
    pub(super) fit: bool,
}

/// The digests a proof's verdict is kept by.
pub(super) struct Keys {
    /// Of what its walk reads
    /// ([`JointProof::digest`](temper_verify::composite::JointProof::digest)):
    /// the rules and their joint model cut to what they read, with what
    /// Cedar lets its senders send. It names the walk while this process
    /// runs.
    pub(super) model: String,
    /// Of the proof's name, every type's spec and the Cedar ([`digest`]),
    /// which a new kernel reads the same.
    pub(super) source: String,
}

/// The digest of a proof's sources: its name, every type's spec and the
/// Cedar. Each part is length-prefixed so no two inputs run together into
/// the same bytes.
pub(super) fn digest(
    proof: &str,
    sources: &BTreeMap<String, String>,
    policy: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    let parts = std::iter::once(proof)
        .chain(sources.iter().flat_map(|(t, s)| [t.as_str(), s.as_str()]))
        .chain(policy);
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// A proof's verdict, or the walk an install must wait for.
pub(super) enum Proof {
    Done(Verdict),
    Running(Wait),
}

/// A walk that is running: the install waits for it without its lock.
pub(super) struct Wait(watch::Receiver<Option<Verdict>>);

impl Wait {
    /// Receive this walk's verdict, including a memory refusal shared by
    /// every attached install. Later installs may retry that walk.
    pub(super) async fn ended(mut self) -> Result<(), String> {
        let verdict = self
            .0
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "the proof job stopped without a verdict".to_string())?;
        verdict
            .as_ref()
            .expect("completed proof has a verdict")
            .refused
            .clone()
            .map_or(Ok(()), Err)
    }
}

/// Verdicts about specs this process reached, and walks
/// running now, by digest.
#[derive(Default)]
struct Proofs {
    reached: BTreeMap<String, Verdict>,
    running: BTreeMap<String, watch::Receiver<Option<Verdict>>>,
}

/// Each holder of the lock inserts or removes whole entries, so one that
/// panicked left the maps whole.
fn proofs() -> &'static Mutex<Proofs> {
    static PROOFS: OnceLock<Mutex<Proofs>> = OnceLock::new();
    PROOFS.get_or_init(Default::default)
}

fn blob_key(digest: &str) -> String {
    format!("joint-verdict/{digest}")
}

/// The verdict of the proof `keys` name: kept under either key, or reached
/// by a walk that ended, or the walk to wait for, started here when none is
/// running. The walk is `walk`, given this host's memory to hold.
pub(super) async fn proof(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    proof: String,
    keys: Keys,
    walk: impl FnOnce(Bound<'_>) -> Walked + Send + 'static,
) -> Proof {
    let mut stored = None;
    if let Some(store) = state.server.platform_metadata_store() {
        let mut missing = Vec::new();
        for key in [&keys.model, &keys.source] {
            match store.get_blob(&blob_key(key)).await {
                Ok(Some(bytes)) if stored.is_none() => {
                    stored = serde_json::from_slice::<Verdict>(&bytes).ok();
                }
                Ok(Some(_)) => {}
                Ok(None) => missing.push(key),
                Err(error) => {
                    tracing::warn!(tenant, app = %app_name, %error, "a kept proof verdict could not be read");
                }
            }
        }
        // Kept under the key that missed too, so the next change to either
        // side alone still finds it.
        if let Some(bytes) = stored.as_ref().and_then(|v| serde_json::to_vec(v).ok()) {
            for key in missing {
                if let Err(error) = store.put_blob(&blob_key(key), &bytes).await {
                    tracing::warn!(tenant, app = %app_name, %error, "a proof verdict could not be kept");
                }
            }
        }
    }
    let digest = keys.model;
    let mut proofs = proofs().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(verdict) = stored {
        proofs.reached.insert(digest, verdict.clone());
        return Proof::Done(verdict);
    }
    if let Some(verdict) = proofs.reached.get(&digest).cloned() {
        return Proof::Done(verdict);
    }
    if let Some(running) = proofs.running.get(&digest) {
        return Proof::Running(Wait(running.clone()));
    }
    let (ended, wait) = watch::channel(None);
    proofs.running.insert(digest.clone(), wait.clone());
    drop(proofs);
    tracing::info!(tenant, app = %app_name, %proof, %digest, "a proof starts");
    tokio::spawn(run(
        state.server.clone(),
        tenant.to_string(),
        app_name.to_string(),
        proof,
        [digest, keys.source],
        walk,
        ended,
    ));
    Proof::Running(Wait(wait))
}

/// Walk on a blocking thread, saying how far it got as it goes, then keep
/// the verdict (in the blob table under both keys when proven) and tell
/// whoever waits.
async fn run(
    server: ServerState,
    tenant: String,
    app_name: String,
    proof: String,
    [digest, source]: [String; 2],
    walk: impl FnOnce(Bound<'_>) -> Walked + Send + 'static,
    ended: watch::Sender<Option<Verdict>>,
) {
    let progress = Arc::new(Progress::default());
    let reports = tokio::spawn(report(
        server.clone(),
        tenant.clone(),
        app_name.clone(),
        proof.clone(),
        Arc::clone(&progress),
    ));
    let walking = Arc::clone(&progress);
    let walked = tokio::task::spawn_blocking(move || {
        walk(Bound {
            room: &HOST_ROOM,
            progress: &walking,
        })
    })
    .await
    .unwrap_or_else(|error| Walked {
        verdict: Verdict {
            held: Vec::new(),
            refused: Some(format!(
                "os-app '{app_name}': the check of {proof} stopped: {error}"
            )),
        },
        fit: false,
    });
    reports.abort();
    if walked.fit
        && walked.verdict.refused.is_none()
        && let Some(store) = server.platform_metadata_store()
        && let Ok(bytes) = serde_json::to_vec(&walked.verdict)
    {
        for key in [&digest, &source] {
            if let Err(error) = store.put_blob(&blob_key(key), &bytes).await {
                tracing::warn!(%tenant, app = %app_name, %error, "a proof verdict could not be kept");
            }
        }
    }
    let states = progress.states.load(Ordering::Relaxed);
    let summary = match &walked.verdict.refused {
        None => format!("os-app '{app_name}': {proof}: proven ({states} joint states)"),
        Some(why) => format!("os-app '{app_name}': {proof}: not proven: {why}"),
    };
    tracing::info!(%tenant, app = %app_name, %proof, %digest, states, "{summary}");
    emit(
        &server,
        &tenant,
        &app_name,
        "proof_done",
        summary,
        Some(walked.verdict.refused.is_none()),
    )
    .await;
    let mut proofs = proofs().lock().unwrap_or_else(PoisonError::into_inner);
    proofs.running.remove(&digest);
    if walked.fit {
        proofs.reached.insert(digest, walked.verdict.clone());
    }
    drop(proofs);
    let _ = ended.send(Some(walked.verdict));
}

/// Say every [`PROGRESS_EVERY`] how many joint states the walk has and the
/// memory it holds, in the server's log and its design-time events.
async fn report(
    server: ServerState,
    tenant: String,
    app_name: String,
    proof: String,
    progress: Arc<Progress>,
) {
    let mut every = tokio::time::interval(PROGRESS_EVERY); // determinism-ok: progress reports of a production proof
    every.tick().await;
    loop {
        every.tick().await;
        let states = progress.states.load(Ordering::Relaxed);
        let mb = progress.bytes.load(Ordering::Relaxed) / MB;
        let summary = format!(
            "os-app '{app_name}': proving {proof}: {states} joint states so far, holding {mb} MB"
        );
        tracing::info!(%tenant, app = %app_name, %proof, states, mb, "{summary}");
        emit(&server, &tenant, &app_name, "proof_progress", summary, None).await;
    }
}

/// A design-time event about the proof of `app_name` (its entity type is
/// the app's name: the proof is of the app's types together).
async fn emit(
    server: &ServerState,
    tenant: &str,
    app_name: &str,
    kind: &str,
    summary: String,
    passed: Option<bool>,
) {
    let event = DesignTimeEvent {
        kind: kind.to_string(),
        entity_type: app_name.to_string(),
        tenant: tenant.to_string(),
        summary,
        level: None,
        passed,
        timestamp: temper_runtime::scheduler::sim_now().to_rfc3339(),
        step_number: None,
        total_steps: None,
    };
    if let Err(error) = server.emit_design_time_event(event).await {
        tracing::warn!(tenant, app = %app_name, %error, "a proof event could not be kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(key: &str) -> Keys {
        Keys {
            model: key.to_string(),
            source: key.to_string(),
        }
    }

    #[tokio::test]
    async fn every_waiter_gets_memory_refusal_and_later_install_retries() {
        let state = PlatformState::new(None);
        let (release, blocked) = std::sync::mpsc::channel();
        let key = "memory-refusal-all-waiters".to_string();
        let first = proof(
            &state,
            "default",
            "test",
            "test".into(),
            keys(&key),
            move |_| {
                blocked.recv().unwrap();
                Walked {
                    verdict: Verdict {
                        held: vec![],
                        refused: Some("no room".into()),
                    },
                    fit: false,
                }
            },
        )
        .await;
        let second = proof(&state, "default", "test", "test".into(), keys(&key), |_| {
            panic!("an attached install must not start another walk")
        })
        .await;
        let Proof::Running(first) = first else {
            panic!("expected a job")
        };
        let Proof::Running(second) = second else {
            panic!("expected shared job")
        };
        release.send(()).unwrap();
        assert_eq!(first.ended().await, Err("no room".into()));
        // Read the second verdict after the first install has consumed its own.
        assert_eq!(second.ended().await, Err("no room".into()));
        let later = proof(&state, "default", "test", "test".into(), keys(&key), |_| {
            Walked {
                verdict: Verdict {
                    held: vec![],
                    refused: None,
                },
                fit: true,
            }
        })
        .await;
        let Proof::Running(later) = later else {
            panic!("later install must retry")
        };
        assert_eq!(later.ended().await, Ok(()));
    }
}
