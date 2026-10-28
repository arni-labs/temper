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
//! A verdict is kept by the digest of what its walk read: which proof, every
//! spec and, for liveness, the Cedar. A proven one is kept in the platform's
//! blob table too, so a start that installs again what an earlier install
//! proved (a Katagami app's new Temper must serve within 20 s of the old
//! one's TERM) reads it instead of walking again, whatever checker made it:
//! those specs already run. A refusal is kept only while this process runs,
//! so a later checker decides it again. A walk that did not fit in its
//! memory says nothing of the specs: the install that waited for it reads it
//! throughout its retries ([`Attempt`]), and the next install walks again.

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

/// What a proof found, in words: the results that held, those the check
/// could not prove (warnings: the install goes ahead), and why the install
/// is refused when the walk found a rule broken.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Verdict {
    pub(super) held: Vec<String>,
    pub(super) unproved: Vec<String>,
    pub(super) refused: Option<String>,
}

/// What a walk returns: its verdict, and whether the walk fit in its memory
/// (a verdict about the specs, not about this host).
pub(super) struct Walked {
    pub(super) verdict: Verdict,
    pub(super) fit: bool,
}

/// The verdicts one install has waited for, by digest, kept across its
/// retries: a walk that did not fit is kept nowhere else, and the retry must
/// read it rather than walk again. A later install walks again.
#[derive(Default)]
pub(super) struct Attempt(BTreeMap<String, Verdict>);

/// A proof's verdict, or the walk an install must wait for.
pub(super) enum Proof {
    Done(Verdict),
    Running(Wait),
}

/// A walk that is running: the install waits for it without its lock.
pub(super) struct Wait {
    digest: String,
    ended: watch::Receiver<Option<Verdict>>,
}

impl Wait {
    /// Wait for the walk to end, and keep its verdict in `attempt`, where
    /// the install's next try finds it. Every install waiting on the walk
    /// gets it, even one of a walk that did not fit.
    pub(super) async fn ended(mut self, attempt: &mut Attempt) -> Result<(), String> {
        let verdict = self
            .ended
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "the proof job stopped without a verdict".to_string())?
            .clone()
            .expect("an ended proof has a verdict");
        attempt.0.insert(self.digest, verdict);
        Ok(())
    }
}

/// The digest of what a walk reads: which proof, every type's spec and, for
/// liveness, the Cedar. Each part is length-prefixed so no two inputs run
/// together into the same bytes.
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

/// The verdict of the walk `digest` names: one this install already waited
/// for, kept, or reached by a walk that ended, or the walk to wait for,
/// started here when none is running. The walk is `walk`, given this host's
/// memory to hold.
pub(super) async fn proof(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    proof: &'static str,
    digest: String,
    attempt: &Attempt,
    walk: impl FnOnce(Bound<'_>) -> Walked + Send + 'static,
) -> Proof {
    if let Some(verdict) = attempt.0.get(&digest) {
        return Proof::Done(verdict.clone());
    }
    let stored = match state.server.platform_metadata_store() {
        Some(store) => match store.get_blob(&blob_key(&digest)).await {
            Ok(Some(bytes)) => serde_json::from_slice::<Verdict>(&bytes).ok(),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(tenant, app = %app_name, %error, "a kept proof verdict could not be read");
                None
            }
        },
        None => None,
    };
    let mut proofs = proofs().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(verdict) = stored {
        proofs.reached.insert(digest, verdict.clone());
        return Proof::Done(verdict);
    }
    if let Some(verdict) = proofs.reached.get(&digest).cloned() {
        return Proof::Done(verdict);
    }
    if let Some(running) = proofs.running.get(&digest) {
        return Proof::Running(Wait {
            digest,
            ended: running.clone(),
        });
    }
    let (ended, wait) = watch::channel(None);
    proofs.running.insert(digest.clone(), wait.clone());
    drop(proofs);
    let waiting = Wait {
        digest: digest.clone(),
        ended: wait,
    };
    tracing::info!(tenant, app = %app_name, proof, %digest, "a proof starts");
    tokio::spawn(run(
        state.server.clone(),
        tenant.to_string(),
        app_name.to_string(),
        proof,
        digest,
        walk,
        ended,
    ));
    Proof::Running(waiting)
}

/// Walk on a blocking thread, saying how far it got as it goes, then keep
/// the verdict (in the blob table when proven) and tell whoever waits.
async fn run(
    server: ServerState,
    tenant: String,
    app_name: String,
    proof: &'static str,
    digest: String,
    walk: impl FnOnce(Bound<'_>) -> Walked + Send + 'static,
    ended: watch::Sender<Option<Verdict>>,
) {
    let progress = Arc::new(Progress::default());
    let reports = tokio::spawn(report(
        server.clone(),
        tenant.clone(),
        app_name.clone(),
        proof,
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
            unproved: Vec::new(),
            refused: Some(format!(
                "os-app '{app_name}': the {proof} check stopped: {error}"
            )),
        },
        fit: false,
    });
    reports.abort();
    if walked.fit
        && walked.verdict.refused.is_none()
        && let Some(store) = server.platform_metadata_store()
        && let Ok(bytes) = serde_json::to_vec(&walked.verdict)
        && let Err(error) = store.put_blob(&blob_key(&digest), &bytes).await
    {
        tracing::warn!(%tenant, app = %app_name, %error, "a proof verdict could not be kept");
    }
    let states = progress.states.load(Ordering::Relaxed);
    let verdict = &walked.verdict;
    let proven = verdict.refused.is_none() && verdict.unproved.is_empty();
    let summary = match &verdict.refused {
        Some(why) => format!("os-app '{app_name}': its {proof} do not hold: {why}"),
        None if proven => format!("os-app '{app_name}': its {proof} hold ({states} joint states)"),
        None => format!(
            "os-app '{app_name}': its {proof} are not all proven ({states} joint states):\n{}",
            verdict.unproved.join("\n")
        ),
    };
    tracing::info!(%tenant, app = %app_name, proof, %digest, states, "{summary}");
    emit(
        &server,
        &tenant,
        &app_name,
        "proof_done",
        summary,
        Some(proven),
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
    proof: &'static str,
    progress: Arc<Progress>,
) {
    let mut every = tokio::time::interval(PROGRESS_EVERY); // determinism-ok: progress reports of a production proof
    every.tick().await;
    loop {
        every.tick().await;
        let states = progress.states.load(Ordering::Relaxed);
        let mb = progress.bytes.load(Ordering::Relaxed) / MB;
        let summary = format!(
            "os-app '{app_name}': proving its {proof}: {states} joint states so far, holding {mb} MB"
        );
        tracing::info!(%tenant, app = %app_name, proof, states, mb, "{summary}");
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

    /// The verdict of a walk that did not fit in its memory.
    fn no_room() -> Walked {
        Walked {
            verdict: Verdict {
                held: vec![],
                unproved: vec!["no room".into()],
                refused: None,
            },
            fit: false,
        }
    }

    #[tokio::test]
    async fn every_waiter_reads_a_walk_that_did_not_fit_and_a_later_install_walks_again() {
        let state = PlatformState::new(None);
        let (release, blocked) = std::sync::mpsc::channel();
        let key = "memory-refusal-all-waiters".to_string();
        let (mut one, mut two) = (Attempt::default(), Attempt::default());
        let first = proof(
            &state,
            "default",
            "test",
            "test",
            key.clone(),
            &one,
            move |_| {
                blocked.recv().unwrap();
                no_room()
            },
        )
        .await;
        let second = proof(&state, "default", "test", "test", key.clone(), &two, |_| {
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
        assert_eq!(first.ended(&mut one).await, Ok(()));
        assert_eq!(second.ended(&mut two).await, Ok(()));
        // Each install's retry reads the verdict it waited for: it goes ahead
        // with the rule unproved instead of walking again, and again.
        for attempt in [&one, &two] {
            let retry = proof(
                &state,
                "default",
                "test",
                "test",
                key.clone(),
                attempt,
                |_| panic!("a retry must read the verdict it waited for"),
            )
            .await;
            let Proof::Done(verdict) = retry else {
                panic!("expected the verdict the install waited for")
            };
            assert_eq!(verdict.unproved, vec!["no room".to_string()]);
            assert_eq!(verdict.refused, None);
        }
        // A walk that did not fit says nothing of the specs: a later install
        // walks again.
        let mut three = Attempt::default();
        let later = proof(&state, "default", "test", "test", key, &three, |_| Walked {
            verdict: Verdict {
                held: vec![],
                unproved: vec![],
                refused: None,
            },
            fit: true,
        })
        .await;
        let Proof::Running(later) = later else {
            panic!("later install must retry")
        };
        assert_eq!(later.ended(&mut three).await, Ok(()));
    }
}
