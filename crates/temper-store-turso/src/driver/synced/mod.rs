//! A local database file whose committed changes are backed up to its own
//! Turso Cloud database (Turso Sync).
//!
//! The file is the database: reads and writes go to it, and a change is
//! confirmed once it commits there. [`SyncCopy::push_committed`] sends every
//! change committed before the call to the remote in one request; the server
//! calls it in the background, so no answer waits for the remote and none is
//! refused because of it. [`SyncCopy::backup_status`] says how far the remote
//! is behind.
//!
//! A start opens the file as it is when it is whole (see `start`). It
//! restores from the remote only a file that is missing or damaged, brings a
//! file that is only behind the remote level with it, and never replaces
//! changes the file has that the remote lacks.
//!
//! Pushes run only in [`CopyState::Copying`]. A start that cannot reach the
//! remote serves as usual and pushes once the remote has checked the file
//! ([`CopyState::Unchecked`]); a file found behind the remote, with nothing of
//! its own since, is brought level at the next start ([`CopyState::Behind`]);
//! a remote that refused a change, or a file and remote that each have
//! changes the other lacks, stop the backup until an operator decides
//! ([`CopyState::Stopped`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{Connection, DriverError, DriverHandle, Lease};

mod backup;
mod files;
mod pool;
mod push;
mod remote;
mod restore;
mod start;
pub use backup::BackupStatus;
use files::{DIVERGED_SUFFIX, sidecar};
pub(crate) use pool::ConnectionPool;
use remote::{Mark, Remote};

/// What the backup of the file can do now. The file serves and confirms
/// writes in every state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyState {
    /// Every committed change is pushed in the background.
    Copying,
    /// The remote could not be reached to check the file since this start.
    /// Nothing is pushed until the check passes; what the file commits
    /// meanwhile is pushed after it.
    Unchecked,
    /// The remote holds changes from this file's history that the file lacks
    /// (it came back from an older copy), and the file has nothing of its own
    /// since. The next start brings the file level with the remote.
    Behind,
    /// The backup is stopped, kept across restarts until an operator decides
    /// ([`SyncCopy::clear_stop`], `temper sync-copy`): the remote refused a
    /// change and committed the rest of its push, so it is missing one, or
    /// the file and the remote each have changes the other lacks.
    Stopped(String),
}

/// The remote copy of one local database, shared by every connection to it.
#[derive(Clone)]
pub struct SyncCopy {
    inner: Arc<Inner>,
}

struct Inner {
    db: turso::sync::Database,
    path: PathBuf,
    remote: Remote,
    /// Every call into the sync engine (connect, push, pull, stats,
    /// checkpoint) goes through this lock. The engine guards itself with a
    /// blocking mutex that a push holds across its network wait, so an
    /// unguarded `connect` blocks a runtime thread until the push ends, and
    /// on a one-thread runtime never returns.
    engine: tokio::sync::Mutex<()>,
    /// One push at a time. A caller that asks while a push is in flight waits
    /// here, not on `engine`, so a connection being opened waits for at most
    /// the push in flight, and the next push carries all of their changes.
    pushing: tokio::sync::Mutex<()>,
    /// Connections are reused, so store calls (reads above all) do not wait
    /// on that lock behind a push. The engine opens a connection by setting
    /// change capture on it, which also fails while another connection
    /// writes; reuse makes that rare and `connect` retries it.
    pool: ConnectionPool,
    /// Every connection in use holds this shared. While the engine
    /// checkpoints, every statement on any connection fails at once with
    /// "database is locked", so a checkpoint takes it exclusively, and only
    /// when it is free right away: waiting for it could deadlock a store call
    /// that holds one connection while opening another.
    in_use: Arc<tokio::sync::RwLock<()>>,
    /// The newest change a successful push carried (from `last_change`).
    pushed: AtomicI64,
    /// The newest change in the file when it was opened, before this start
    /// committed anything. A file is behind when the remote has more.
    opened_at: std::sync::Mutex<Mark>,
    state: std::sync::Mutex<CopyState>,
    /// Push attempts that ran out of time in a row.
    timeouts: AtomicU32,
    /// Whether the remote may hold a push the engine never heard back from.
    unsure: AtomicBool,
    /// Whether a push is running now.
    in_flight: AtomicBool,
    /// Push attempts that failed since this start.
    failures: AtomicU64,
    /// Since when the remote has lacked a committed change.
    lag: std::sync::Mutex<backup::LagClock>,
}

/// The sync engine's text for a statement the remote refused.
const REMOTE_STATEMENT_FAILED: &str = "failed to execute sql";

/// One push attempt. The engine's HTTP client has no timeout of its own.
const PUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

impl std::fmt::Debug for SyncCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncCopy").finish_non_exhaustive()
    }
}

impl SyncCopy {
    /// What the backup can do now, and if it cannot push, why.
    pub fn state(&self) -> CopyState {
        self.inner.state.lock().unwrap().clone()
    }

    /// Push attempts that ran out of time in a row. The engine sends its
    /// requests one at a time, so a request the remote never answers holds
    /// every later push until its connection fails; a restart clears it.
    pub fn pushes_timed_out_in_a_row(&self) -> u32 {
        self.inner.timeouts.load(Ordering::Acquire)
    }

    fn opened_at(&self) -> Mark {
        *self.inner.opened_at.lock().unwrap()
    }

    /// Check an [`CopyState::Unchecked`] file against the remote and return
    /// the state after it. Other states are returned as they are.
    pub async fn recheck(&self) -> Result<CopyState, PersistenceError> {
        self.recheck_inner().await.map_err(storage_error)
    }

    /// The remote at or before the file's place at open: the file holds all
    /// of its history the remote does, and what it has beyond is pushed. The
    /// remote past it: the file is behind when it has committed nothing since
    /// it opened, and the two have diverged when it has.
    async fn recheck_inner(&self) -> Result<CopyState, DriverError> {
        if self.state() != CopyState::Unchecked {
            return Ok(self.state());
        }
        let remote_has = self.inner.remote.last_change().await?;
        let opened_at = self.opened_at();
        if remote_has <= opened_at {
            self.enter(
                CopyState::Copying,
                "the file checks out against its remote copy",
            );
            return Ok(self.state());
        }
        let local_has = self
            .local_mark()
            .await
            .map_err(|error| DriverError::SyncSetup(error.to_string()))?;
        if local_has == opened_at {
            self.enter(CopyState::Behind, &format!(
                "the remote has changes up to {remote_has} from this file's history, the file only \
                 up to {opened_at} and nothing of its own since"
            ));
        } else {
            self.enter(
                CopyState::Stopped(format!(
                    "diverged: the remote has changes up to {remote_has} from this file's history \
                     that the file lacks, and the file has changes of its own past {opened_at}, \
                     up to {local_has}, that the remote lacks"
                )),
                "the file and its remote copy each have changes the other lacks",
            );
        }
        Ok(self.state())
    }

    /// Move to `next` unless the copy is already stopped or behind, which only
    /// a restart or an operator clears.
    pub(super) fn enter(&self, next: CopyState, why: &str) {
        let mut state = self.inner.state.lock().unwrap();
        if matches!(*state, CopyState::Stopped(_) | CopyState::Behind) || *state == next {
            return;
        }
        *state = next.clone();
        drop(state);
        match &next {
            CopyState::Copying => {
                files::mark_checked(&self.inner.path);
                tracing::info!(why, "turso.sync the backup is pushing");
            }
            CopyState::Unchecked => {}
            CopyState::Behind => tracing::warn!(
                why,
                "turso.sync the file is behind its remote copy; the next start brings it level"
            ),
            CopyState::Stopped(_) => tracing::error!(
                event = "turso.sync.backup_stopped",
                state = ?next,
                why,
                "turso.sync the backup stopped; the file keeps serving and confirming writes"
            ),
        }
        if let CopyState::Stopped(cause) = &next
            && let Err(error) = std::fs::write(sidecar(&self.inner.path, DIVERGED_SUFFIX), cause)
        {
            tracing::error!(%error, "turso.sync could not record the stop");
        }
    }

    pub(super) async fn connect(&self) -> Result<Connection, DriverError> {
        let in_use = Some(self.inner.in_use.clone().read_owned().await);
        let pool = Some(self.inner.pool.clone());
        if let Some(conn) = self.inner.pool.take() {
            let conn = Lease::new(conn, in_use);
            return Ok(Connection::Local { conn, pool });
        }
        let started = Instant::now();
        let mut delay = Duration::from_millis(2);
        loop {
            let opened = {
                let _engine = self.inner.engine.lock().await;
                self.inner.db.connect().await
            };
            match opened {
                Ok(conn) => {
                    conn.busy_timeout(Duration::from_secs(5))?;
                    let conn = Lease::new(Arc::new(DriverHandle::new(conn)), in_use);
                    return Ok(Connection::Local { conn, pool });
                }
                Err(error) if is_busy(&error) && started.elapsed() < CONNECT_BUSY_BUDGET => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Move the local write-ahead log into the database file once it is larger
    /// than `max_wal_bytes`. Turso Sync turns automatic checkpoints off, so
    /// without this the log grows with every write.
    pub async fn checkpoint_if_wal_over(
        &self,
        max_wal_bytes: u64,
    ) -> Result<bool, PersistenceError> {
        let _engine = self.inner.engine.lock().await;
        let Ok(_idle) = self.inner.in_use.try_write() else {
            return Ok(false);
        };
        let stats = self.inner.db.stats().await.map_err(storage_error)?;
        if stats.main_wal_size < max_wal_bytes {
            return Ok(false);
        }
        self.inner.db.checkpoint().await.map_err(storage_error)?;
        Ok(true)
    }

    /// With the app stopped: clear a [`CopyState::Stopped`] copy once its
    /// remote has been rebuilt from the file. Returns the cause it cleared.
    pub fn clear_stop(path: &str) -> Result<Option<String>, PersistenceError> {
        files::clear_stop(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }

    /// With the app stopped: move the file and its sync files aside, so the
    /// next start restores the database from the remote. Returns where they went.
    pub fn set_aside(path: &str) -> Result<PathBuf, PersistenceError> {
        files::move_aside(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }

    /// With the app stopped: download the remote copy into `path`, which must
    /// not exist yet, the way a start restores it, without serving, pushing or
    /// changing the remote. Returns [`Self::describe`] of the result.
    pub async fn fetch(
        path: &str,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<String, PersistenceError> {
        let local = path.strip_prefix("file:").unwrap_or(path);
        files::refuse_existing(Path::new(local)).map_err(storage_error)?;
        let copy = Self::open(local, remote_url, auth_token)
            .await
            .map_err(storage_error)?;
        drop(copy);
        Self::describe(local)
    }

    /// How far a restore into `path` has got: the bytes written so far, and
    /// the database's whole size once the remote has sent it.
    pub fn restored_bytes(path: &str) -> (u64, u64) {
        restore::written(Path::new(path.strip_prefix("file:").unwrap_or(path)))
    }

    /// What is on disk for `path`: the stop, if any, and how far the file and
    /// its last push got. For an operator; the app may be running.
    pub fn describe(path: &str) -> Result<String, PersistenceError> {
        files::describe(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }
}

/// How long opening a connection retries while other connections write; as
/// long as the store's statements wait for a busy database.
const CONNECT_BUSY_BUDGET: Duration = Duration::from_secs(30);

/// Connections opened at start and kept idle, so a burst of requests finds
/// them instead of opening new ones while writes hold the database.
const WARM_CONNECTIONS: usize = 16;

fn is_busy(error: &turso::Error) -> bool {
    matches!(error, turso::Error::Busy(_))
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("database is busy")
}
