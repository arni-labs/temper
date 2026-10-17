//! A local database file whose committed changes are copied to its own
//! Turso Cloud database (Turso Sync).
//!
//! Reads and writes stay on the local file. [`SyncCopy::push_committed`]
//! sends every change committed before the call to the remote database in one
//! request, and returns only once the remote has committed it. Callers that
//! answer a request after a write await it first, so a confirmed change is
//! never only on this machine. A fresh machine with no local file starts from
//! the remote copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{Connection, DriverError, DriverHandle, Lease};

mod files;
mod pool;
use files::{DIVERGED_SUFFIX, check_local_files, move_aside, sidecar};
pub(crate) use pool::ConnectionPool;

/// The remote copy of one local database, shared by every connection to it.
#[derive(Clone)]
pub struct SyncCopy {
    inner: Arc<Inner>,
}

struct Inner {
    db: turso::sync::Database,
    path: PathBuf,
    /// Every call into the sync engine (connect, push, stats, checkpoint) goes
    /// through this lock. The engine guards itself with a blocking mutex that
    /// a push holds across its network wait, so an unguarded `connect` blocks
    /// a runtime thread until the push ends, and on a one-thread runtime
    /// never returns.
    engine: tokio::sync::Mutex<()>,
    /// One push at a time. Writers that finish while a push is in flight wait
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
    /// Set once the remote copy cannot be trusted to hold every confirmed
    /// change; see [`SyncCopy::push_committed`]. Every later push fails.
    diverged: std::sync::OnceLock<String>,
    /// The remote holds changes from this file's history that the file does
    /// not: it was put back from an older copy (a snapshot or backup), or a
    /// second server shares it.
    behind: AtomicBool,
}

/// The sync engine's text for a statement the remote refused.
const REMOTE_STATEMENT_FAILED: &str = "failed to execute sql";

/// One push attempt. The engine's HTTP client has no timeout of its own.
const PUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long opening the engine (a restore included) may take.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a start keeps trying to check the file against the remote.
const OPEN_PUSH_BUDGET: Duration = Duration::from_secs(30);

impl std::fmt::Debug for SyncCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncCopy").finish_non_exhaustive()
    }
}

impl SyncCopy {
    /// Open `path` as a synced database, restoring it from `remote_url` when
    /// there is no local database yet.
    ///
    /// The open pushes what the last run left behind, before anything new can
    /// commit. If that shows the file is behind the remote (it came back from
    /// an older copy), the file is moved aside and the database restored from
    /// the remote, which holds everything the older copy did. The check needs
    /// the remote: once new changes commit, a stale file's reused change
    /// numbers can no longer be told apart, so a start that cannot reach the
    /// remote fails rather than serve an unchecked file.
    pub(crate) async fn open(
        path: &str,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<Self, DriverError> {
        let path = Path::new(path);
        check_local_files(path)?;
        let copy = Self::build(path, remote_url, auth_token).await?;
        // A copy already known to be incomplete confirms nothing; it opens for
        // reads without the check.
        if copy.inner.diverged.get().is_none()
            && let Err(error) = copy.push_committed_within(OPEN_PUSH_BUDGET).await
            && !copy.inner.behind.load(Ordering::Acquire)
        {
            return Err(DriverError::SyncSetup(format!(
                "could not check {} against its remote copy: {error}",
                path.display()
            )));
        }
        if !copy.inner.behind.load(Ordering::Acquire) {
            return Ok(copy);
        }
        drop(copy);
        let moved = move_aside(path)?;
        tracing::warn!(
            moved = %moved.display(),
            "turso.sync local file is behind its remote copy; restoring from the remote"
        );
        Self::build(path, remote_url, auth_token).await
    }

    async fn build(path: &Path, remote_url: &str, auth_token: &str) -> Result<Self, DriverError> {
        // The engine's HTTPS client takes the process's rustls crypto provider,
        // and rustls cannot choose one when the build carries two (the whole
        // workspace does): the engine's IO thread then panics and every sync
        // call waits forever. An error here means one is already installed.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let local = path.to_string_lossy();
        let opening = turso::sync::Builder::new_remote(&local)
            .with_remote_url(remote_url)
            .with_auth_token(auth_token)
            .with_client_name("temper")
            .bootstrap_if_empty(true)
            .build();
        // A restore downloads the whole database; the engine sets no timeout.
        let db = tokio::time::timeout(BUILD_TIMEOUT, opening)
            .await
            .map_err(|_| {
                DriverError::SyncSetup(format!(
                    "opening {} with its remote copy took over {} s",
                    path.display(),
                    BUILD_TIMEOUT.as_secs()
                ))
            })??;
        let diverged = std::sync::OnceLock::new();
        if let Ok(cause) = std::fs::read_to_string(sidecar(path, DIVERGED_SUFFIX)) {
            let _ = diverged.set(cause);
        }
        let copy = Self {
            inner: Arc::new(Inner {
                db,
                path: path.to_path_buf(),
                engine: tokio::sync::Mutex::new(()),
                pushing: tokio::sync::Mutex::new(()),
                pushed: AtomicI64::new(0),
                pool: ConnectionPool::default(),
                in_use: Arc::new(tokio::sync::RwLock::new(())),
                diverged,
                behind: AtomicBool::new(false),
            }),
        };
        // Change numbers follow commit order only with one writer at a time
        // (WAL). The engine can switch a file to MVCC for an MVCC remote.
        let mode = copy.query_text("PRAGMA journal_mode").await?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(DriverError::SyncSetup(format!(
                "{} is in journal mode {mode}; Temper's Turso Sync copy needs WAL",
                path.display()
            )));
        }
        Ok(copy)
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

    /// Copy every change committed before this call to the remote database.
    ///
    /// Returns without a network request when another push already carried
    /// those changes, so writers that commit together share one push. An
    /// error means some of them may be only local; they stay in the local
    /// change log and the next successful push sends them.
    ///
    /// A push is one remote transaction, but the remote skips a statement that
    /// fails and commits the rest together with the push's progress marker, so
    /// a retry would succeed without the skipped change. Once that happens
    /// every later push fails too: nothing is confirmed against a copy that is
    /// missing a change until the remote is rebuilt from the local file.
    pub async fn push_committed(&self) -> Result<(), PersistenceError> {
        // Changes are numbered in commit order (one writer at a time), so
        // everything this caller committed is at or below `wanted`.
        let wanted = self.last_change().await?;
        let _pushing = self.inner.pushing.lock().await;
        self.fail_if_diverged()?;
        if self.inner.pushed.load(Ordering::Acquire) >= wanted {
            return Ok(());
        }
        // The push reads the change log after this, so it carries `covered`.
        let covered = self.last_change().await?;
        let started = Instant::now();
        let pushed = {
            let _engine = self.inner.engine.lock().await;
            tokio::time::timeout(PUSH_ATTEMPT_TIMEOUT, self.inner.db.push()).await
        };
        tracing::debug!(
            changes = covered - self.inner.pushed.load(Ordering::Acquire),
            elapsed_ms = started.elapsed().as_millis() as u64,
            ok = matches!(pushed, Ok(Ok(()))),
            "turso.sync push"
        );
        match pushed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let message = error.to_string();
                let transient = ["busy", "locked"]
                    .iter()
                    .any(|word| message.to_ascii_lowercase().contains(word));
                if message.contains(REMOTE_STATEMENT_FAILED) && !transient {
                    self.set_diverged(&message, true);
                }
                return Err(storage_error(error));
            }
            Err(_) => {
                return Err(PersistenceError::Storage(format!(
                    "turso.sync push timed out after {} s",
                    PUSH_ATTEMPT_TIMEOUT.as_secs()
                )));
            }
        }
        // The engine sends only changes numbered above the remote's record of
        // this file's last push. A remote record above everything the file
        // holds means the remote silently skipped what was just written.
        let remote_has = self.last_pushed_hint().map_err(storage_error)?;
        let local_has = self.last_change().await?;
        if remote_has > local_has {
            self.inner.behind.store(true, Ordering::Release);
            self.set_diverged(
                &format!(
                    "the remote has changes up to {remote_has} from this file's history, the file \
                     only up to {local_has}; restart to restore it from the remote"
                ),
                false,
            );
            return self.fail_if_diverged();
        }
        self.inner.pushed.fetch_max(covered, Ordering::AcqRel);
        Ok(())
    }

    /// Stop confirming writes. A refused change is kept across restarts, so
    /// writes stay unconfirmed until the remote is rebuilt and the file
    /// `<db>-sync-diverged` removed; a file that is behind is restored at the
    /// next start instead.
    fn set_diverged(&self, cause: &str, keep: bool) {
        if self.inner.diverged.set(cause.to_string()).is_err() {
            return;
        }
        tracing::error!(
            cause,
            "turso.sync remote copy is incomplete; no further writes will be confirmed"
        );
        if keep
            && let Err(error) = std::fs::write(sidecar(&self.inner.path, DIVERGED_SUFFIX), cause)
        {
            tracing::error!(%error, "turso.sync could not record the incomplete remote copy");
        }
    }

    /// The remote's record of this file's last pushed change, as the engine
    /// saved it after the push.
    fn last_pushed_hint(&self) -> Result<i64, DriverError> {
        let info = std::fs::read(sidecar(&self.inner.path, "-info"))
            .map_err(|error| DriverError::SyncSetup(format!("read sync metadata: {error}")))?;
        let info: serde_json::Value = serde_json::from_slice(&info)
            .map_err(|error| DriverError::SyncSetup(format!("parse sync metadata: {error}")))?;
        Ok(info["last_pushed_change_id_hint"].as_i64().unwrap_or(0))
    }

    /// The number of the newest committed change in the local change log.
    async fn last_change(&self) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query("SELECT MAX(change_id) FROM turso_cdc", ())
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        Ok(row
            .get::<Option<i64>>(0)
            .map_err(storage_error)?
            .unwrap_or(0))
    }

    async fn query_text(&self, sql: &str) -> Result<String, DriverError> {
        let conn = self.connect().await?;
        let mut rows = conn.query(sql, ()).await?;
        let Some(row) = rows.next().await? else {
            return Ok(String::new());
        };
        Ok(row.get::<String>(0).unwrap_or_default())
    }

    fn fail_if_diverged(&self) -> Result<(), PersistenceError> {
        match self.inner.diverged.get() {
            Some(cause) => Err(PersistenceError::Storage(format!(
                "the remote copy is missing a change ({cause})"
            ))),
            None => Ok(()),
        }
    }

    /// [`Self::push_committed`], retried with backoff until it succeeds or
    /// `budget` has passed. The remote records the last change it applied in
    /// the same transaction as the changes, so a retry after a push whose
    /// answer was lost does not apply anything twice.
    ///
    /// The budget covers the whole call, waiting behind other pushes included.
    pub async fn push_committed_within(&self, budget: Duration) -> Result<(), PersistenceError> {
        tokio::time::timeout(budget, self.push_committed_retrying(budget))
            .await
            .unwrap_or_else(|_| {
                Err(PersistenceError::Storage(format!(
                    "turso.sync push not done within {} s",
                    budget.as_secs()
                )))
            })
    }

    async fn push_committed_retrying(&self, budget: Duration) -> Result<(), PersistenceError> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            match self.push_committed().await {
                Ok(()) => return Ok(()),
                Err(error)
                    if started.elapsed() + delay < budget
                        && self.inner.diverged.get().is_none() =>
                {
                    tracing::warn!(%error, delay_ms = delay.as_millis() as u64, "turso.sync push failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
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
}

/// How long opening a connection retries while another connection writes.
const CONNECT_BUSY_BUDGET: Duration = Duration::from_secs(5);

fn is_busy(error: &turso::Error) -> bool {
    matches!(error, turso::Error::Busy(_))
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("database is busy")
}
