//! What a start does with the file on the drive before it serves.
//!
//! - **Whole** (it opens, and `PRAGMA quick_check` finds nothing wrong): it is
//!   served as it is, and whatever it has beyond the remote is pushed.
//! - **Missing or empty**: the engine restores it from the remote.
//! - **Damaged** (`PRAGMA quick_check` reports a problem, or reading it fails
//!   as corrupt or not a database): it is moved aside, kept, and the database
//!   restored from the remote.
//! - **Behind** (the remote has changes from its history that it lacks, and
//!   it has nothing of its own since): the remote holds everything it does, so
//!   it is brought level. The engine pulls the remote's newer changes into it;
//!   the result is checked against the remote, table by table. When the pull
//!   fails or the result differs, the remote is restored beside the file and
//!   swapped in, the file kept aside. When that fails too, the file serves as
//!   it is and the backup stops until an operator decides.
//! - **Diverged** (each has changes the other lacks): the file serves as it
//!   is and the backup stops (see [`CopyState::Stopped`]).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64};
use std::time::Instant;

use super::files::{self, DIVERGED_SUFFIX, INCOMING_SUFFIX, check_local_files, sidecar};
use super::remote::{self, Mark, Remote};
use super::{ConnectionPool, CopyState, DriverError, Inner, SyncCopy, WARM_CONNECTIONS, restore};

impl SyncCopy {
    /// Open `path` as a synced database, restoring it from `remote_url` when
    /// there is no local database yet or it is damaged, and bringing it level
    /// with the remote when it is behind (see the module docs).
    ///
    /// A remote that cannot be reached leaves the copy [`CopyState::Unchecked`]
    /// until [`Self::recheck`] passes. A file that a stopped restore left part
    /// way is moved aside and restored again.
    pub(crate) async fn open(
        path: &str,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<Self, DriverError> {
        let path = Path::new(path);
        check_local_files(path)?;
        let copy = match Self::build(path, remote_url, auth_token).await {
            Ok(copy) => match copy.damage().await? {
                None => copy,
                Some(damage) => {
                    drop(copy);
                    Self::restore_damaged(path, remote_url, auth_token, &damage).await?
                }
            },
            Err(error) => match corrupt(&error) {
                Some(damage) => {
                    Self::restore_damaged(path, remote_url, auth_token, &damage).await?
                }
                None => return Err(error),
            },
        };
        match copy.recheck_inner().await {
            Ok(CopyState::Behind) => copy.catch_up(remote_url, auth_token).await,
            Ok(_) => Ok(copy),
            Err(error) => {
                tracing::warn!(%error, "turso.sync remote unreachable at start; serving the file and backing it up once the remote answers");
                files::mark_unchecked(path, copy.opened_at())?;
                Ok(copy)
            }
        }
    }

    /// Move a damaged file aside, kept, and restore the database from the remote.
    async fn restore_damaged(
        path: &Path,
        remote_url: &str,
        auth_token: &str,
        damage: &str,
    ) -> Result<Self, DriverError> {
        let moved = files::move_aside(path)?;
        tracing::error!(
            moved = %moved.display(),
            damage,
            "turso.sync the local file is damaged; restoring it from the remote"
        );
        Self::build(path, remote_url, auth_token).await
    }

    async fn build(path: &Path, remote_url: &str, auth_token: &str) -> Result<Self, DriverError> {
        // The engine's HTTPS client takes the process's rustls crypto provider,
        // and rustls cannot choose one when the build carries two (the whole
        // workspace does): the engine's IO thread then panics and every sync
        // call waits forever. An error here means one is already installed.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Without sync metadata the engine restores the file from the remote,
        // and writes the metadata only once every page is on disk. Until then
        // the file is marked, so a start that stops part way is not taken
        // for a database that was never synced.
        if !sidecar(path, "-info").exists() {
            files::mark_restoring(path)?;
        }
        let local = path.to_string_lossy();
        let opening = turso::sync::Builder::new_remote(&local)
            .with_remote_url(remote_url)
            .with_auth_token(auth_token)
            .with_client_name("temper")
            .bootstrap_if_empty(true)
            .build();
        let db = restore::while_progressing(opening, || restore::footprint(path))
            .await
            .ok_or_else(|| {
                DriverError::SyncSetup(format!(
                    "opening {} with its remote copy made no progress for {} s",
                    path.display(),
                    restore::RESTORE_STALL_BUDGET.as_secs()
                ))
            })??;
        files::restored(path);
        let state = match std::fs::read_to_string(sidecar(path, DIVERGED_SUFFIX)) {
            Ok(cause) => CopyState::Stopped(cause),
            Err(_) => CopyState::Unchecked,
        };
        let client_id = remote::client_id(&files::sync_metadata(path)?);
        let copy = Self {
            inner: Arc::new(Inner {
                db,
                path: path.to_path_buf(),
                remote: Remote::new(remote_url, auth_token, client_id),
                engine: tokio::sync::Mutex::new(()),
                pushing: tokio::sync::Mutex::new(()),
                pushed: AtomicI64::new(0),
                opened_at: std::sync::Mutex::new(Mark::default()),
                pool: ConnectionPool::default(),
                in_use: Arc::new(tokio::sync::RwLock::new(())),
                state: std::sync::Mutex::new(state),
                timeouts: AtomicU32::new(0),
                unsure: AtomicBool::new(true),
                in_flight: AtomicBool::new(false),
                failures: AtomicU64::new(0),
                lag: std::sync::Mutex::default(),
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
        let opened_at = match files::unchecked_since(path) {
            Some(opened_at) => opened_at,
            None => copy
                .local_mark()
                .await
                .map_err(|error| DriverError::SyncSetup(error.to_string()))?,
        };
        *copy.inner.opened_at.lock().unwrap() = opened_at;
        let mut warm = Vec::with_capacity(WARM_CONNECTIONS);
        for _ in 0..WARM_CONNECTIONS {
            warm.push(copy.connect().await?);
        }
        drop(warm);
        Ok(copy)
    }

    /// What `PRAGMA quick_check` finds wrong with the file, if anything. It
    /// reads the whole file once.
    async fn damage(&self) -> Result<Option<String>, DriverError> {
        let started = Instant::now();
        let checked = async {
            let conn = self.connect().await?;
            let mut rows = conn.query("PRAGMA quick_check", ()).await?;
            let mut problems = Vec::new();
            while let Some(row) = rows.next().await? {
                let line = row.get::<String>(0).unwrap_or_default();
                if line != "ok" {
                    problems.push(line);
                }
            }
            Ok::<_, DriverError>(problems)
        }
        .await;
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "turso.sync checked the local file"
        );
        match checked {
            Ok(problems) if problems.is_empty() => Ok(None),
            Ok(problems) => Ok(Some(problems.join("; "))),
            Err(error) => corrupt(&error).map(Some).ok_or(error),
        }
    }

    /// Bring a file that is behind its remote, with nothing of its own since,
    /// level with it (see the module docs). The file holds nothing the remote
    /// lacks, so each way of doing it keeps every change.
    async fn catch_up(self, remote_url: &str, auth_token: &str) -> Result<Self, DriverError> {
        let path = self.inner.path.clone();
        let pulled = self.pull_remote().await;
        drop(self);
        let problem = match pulled {
            Err(error) => format!("the pull failed: {error}"),
            Ok(()) => {
                // The number at open belongs to the file before the pull.
                files::mark_checked(&path);
                let copy = Self::build(&path, remote_url, auth_token).await?;
                match copy.level_with_remote().await {
                    Ok(()) => {
                        tracing::info!(
                            "turso.sync pulled the remote's newer changes into the file"
                        );
                        return Ok(copy);
                    }
                    Err(difference) => format!("after the pull, {difference}"),
                }
            }
        };
        tracing::warn!(
            problem,
            "turso.sync restoring the remote beside the file to swap it in"
        );
        match Self::restore_beside(&path, remote_url, auth_token).await {
            Ok(copy) => Ok(copy),
            Err(error) => {
                tracing::error!(%error, problem, "turso.sync could not bring the file level with its remote; serving it as it is");
                // Stopped, not left to the next check: that check would find
                // the file behind again and restart the server into this same
                // failure. Pushing is no answer either, since the remote holds
                // change numbers the file's next changes would reuse.
                let copy = Self::build(&path, remote_url, auth_token).await?;
                copy.enter(
                    CopyState::Stopped(format!(
                        "the file is behind its remote copy and could not be brought level: \
                         {problem}, and restoring the remote beside it failed: {error}"
                    )),
                    "the file could not be brought level with its remote copy",
                );
                Ok(copy)
            }
        }
    }

    /// Pull the remote's changes since the file's last pull into it.
    async fn pull_remote(&self) -> Result<(), DriverError> {
        let _engine = self.inner.engine.lock().await;
        let path = &self.inner.path;
        let _changed =
            restore::while_progressing(self.inner.db.pull(), || restore::footprint(path))
                .await
                .ok_or_else(|| {
                    DriverError::SyncSetup(format!(
                        "pulling into {} made no progress for {} s",
                        path.display(),
                        restore::RESTORE_STALL_BUDGET.as_secs()
                    ))
                })??;
        Ok(())
    }

    /// Check a file just brought level: each table holds as many rows as on
    /// the remote, and the remote then checks the file out.
    async fn level_with_remote(&self) -> Result<(), String> {
        let tables = self
            .user_tables()
            .await
            .map_err(|error| error.to_string())?;
        let mut here = Vec::with_capacity(tables.len());
        for table in &tables {
            here.push(
                self.count_rows(table)
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }
        let there = self
            .inner
            .remote
            .row_counts(&tables)
            .await
            .map_err(|error| error.to_string())?;
        let differ: Vec<String> = tables
            .iter()
            .zip(here.iter().zip(&there))
            .filter(|(_, (here, there))| here != there)
            .map(|(table, (here, there))| {
                format!("{table}: {here} rows here, {there} on the remote")
            })
            .collect();
        if !differ.is_empty() {
            return Err(format!(
                "the file differs from the remote: {}",
                differ.join(", ")
            ));
        }
        match self.recheck_inner().await {
            Ok(CopyState::Copying) => Ok(()),
            Ok(state) => Err(format!("the check against the remote says {state:?}")),
            Err(error) => Err(format!("the check against the remote failed: {error}")),
        }
    }

    /// Restore the remote into a new file beside `path`, then swap it in,
    /// keeping `path`'s files aside.
    async fn restore_beside(
        path: &Path,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<Self, DriverError> {
        let incoming = sidecar(path, INCOMING_SUFFIX);
        // An earlier attempt cut short: only the remote's pages, kept anyway.
        if files::refuse_existing(&incoming).is_err() {
            files::move_aside(&incoming)?;
        }
        drop(Self::build(&incoming, remote_url, auth_token).await?);
        let aside = files::swap_in(&incoming, path)?;
        tracing::warn!(
            aside = %aside.display(),
            "turso.sync swapped in a fresh restore of the remote; the file it replaced is kept aside"
        );
        let copy = Self::build(path, remote_url, auth_token).await?;
        if let Err(error) = copy.recheck_inner().await {
            tracing::warn!(%error, "turso.sync check after the restore failed");
        }
        Ok(copy)
    }

    async fn user_tables(&self) -> Result<Vec<String>, DriverError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT GLOB 'sqlite_*' \
                 AND name NOT GLOB 'turso_*' AND name NOT GLOB '__turso_*' ORDER BY name",
                (),
            )
            .await?;
        let mut tables = Vec::new();
        while let Some(row) = rows.next().await? {
            tables.push(row.get::<String>(0).unwrap_or_default());
        }
        Ok(tables)
    }

    async fn count_rows(&self, table: &str) -> Result<i64, DriverError> {
        let conn = self.connect().await?;
        let sql = format!("SELECT COUNT(*) FROM {}", remote::quote(table));
        let mut rows = conn.query(&sql, ()).await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<i64>(0).unwrap_or_default()),
            None => Ok(0),
        }
    }
}

/// The damage an error reports, when it says the file is corrupt or not a
/// database at all.
fn corrupt(error: &DriverError) -> Option<String> {
    match error {
        DriverError::Local(turso::Error::Corrupt(_) | turso::Error::NotAdb(_)) => {
            Some(error.to_string())
        }
        _ => None,
    }
}
