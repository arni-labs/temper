//! Pushing committed changes to the remote, and what a refused push means.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{
    CopyState, DriverError, PUSH_ATTEMPT_TIMEOUT, REMOTE_STATEMENT_FAILED, SyncCopy, files,
};

/// How long the remote may take to say how far it got, under the 10 s its
/// HTTP client allows, so a hung remote is told from a refusing one.
const REMOTE_READ_TIMEOUT: Duration = Duration::from_secs(8);

impl SyncCopy {
    /// Change log rows one trim deletes at most. A trim holds the database's
    /// write lock and writes the pages it frees to the WAL, so a log that
    /// built up before trimming existed (1.7M rows on one app) is cleared
    /// over many pushes, not in one long transaction.
    pub const TRIM_ROWS_PER_PUSH: i64 = 10_000;

    /// Copy every change committed before this call to the remote database.
    ///
    /// Returns without a network request when another push already carried
    /// those changes, so writers that commit together share one push. An
    /// error means some of them may be only local; they stay in the local
    /// change log and the next successful push sends them. Nothing is pushed
    /// unless the copy is [`CopyState::Copying`].
    pub async fn push_committed(&self) -> Result<(), PersistenceError> {
        // Changes are numbered in commit order (one writer at a time), so
        // everything this caller committed is at or below `wanted`.
        let wanted = self.last_change().await?;
        let _pushing = self.inner.pushing.lock().await;
        self.require_copying()?;
        if self.inner.pushed.load(Ordering::Acquire) >= wanted {
            return Ok(());
        }
        // The push reads the change log after this, so it carries `covered`.
        let covered = self.last_change().await?;
        let remote_had = self.remote_had().await?;
        // Until it is known to have succeeded, a push may have committed on
        // the remote past the engine's own record: set before it starts, so
        // a caller that gives up on it (an answer's time limit) leaves it set.
        self.inner.unsure.store(true, Ordering::Release);
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
            Ok(Ok(())) => {
                self.inner.unsure.store(false, Ordering::Release);
                self.inner.timeouts.store(0, Ordering::Release);
            }
            Ok(Err(error)) => {
                self.inner.timeouts.store(0, Ordering::Release);
                let message = error.to_string();
                if message.contains(REMOTE_STATEMENT_FAILED) {
                    self.after_refusal(remote_had, &message).await;
                }
                return Err(storage_error(error));
            }
            Err(_) => {
                self.inner.timeouts.fetch_add(1, Ordering::AcqRel);
                return Err(PersistenceError::Storage(format!(
                    "turso.sync push timed out after {} s",
                    PUSH_ATTEMPT_TIMEOUT.as_secs()
                )));
            }
        }
        // The engine sends only changes numbered above the remote's record of
        // this file's last push. A record above everything the file holds
        // means the remote silently skipped what was just written: a second
        // server is pushing as this file.
        let remote_has = self.last_pushed_hint().await.map_err(storage_error)?;
        let local_has = self.last_change().await?;
        if remote_has > local_has {
            self.enter(CopyState::Behind, &format!(
                "after a push the remote has changes up to {remote_has}, the file only up to {local_has}"
            ));
            return self.require_copying();
        }
        self.inner.pushed.fetch_max(covered, Ordering::AcqRel);
        self.trim_confirmed(remote_has).await;
        Ok(())
    }

    /// Delete the change log rows the remote has confirmed, keeping the
    /// newest row. Nothing trims the log otherwise, and in 'full' mode each
    /// row holds whole row images, so it grows with every write.
    ///
    /// The engine reads the log only above the remote's record of this
    /// file's last push: a push scans from that record plus one
    /// (turso_sync_engine 0.7.2 `database_sync_operations.rs` 2853,
    /// `database_tape.rs` 416-432), a pull replays only rows above it
    /// (`database_sync_engine.rs` 864-887), and its stats count rows above
    /// it (1405-1427). `confirmed` is that record as the engine saved it
    /// after the remote committed, so no row at or below it is read again.
    ///
    /// The newest row stays because in WAL mode a change is numbered one
    /// above the largest in the log (turso_core 0.7.2
    /// `translate/emitter/mod.rs` 1223-1239, `vdbe/execute.rs` 10823-10841).
    /// An empty log would number the next change 1, at or below what the
    /// remote holds, and the next push would skip it. [`Self::last_change`],
    /// the start's check against the remote and the check after each push
    /// read that same newest row.
    ///
    /// A failed trim leaves the rows for the next push to delete.
    async fn trim_confirmed(&self, confirmed: i64) {
        match self.trim_through(confirmed).await {
            Ok(0) => {}
            Ok(trimmed) => tracing::debug!(trimmed, confirmed, "turso.sync trimmed the change log"),
            Err(error) => {
                tracing::warn!(%error, "turso.sync could not trim the change log; the next push will")
            }
        }
    }

    async fn trim_through(&self, confirmed: i64) -> Result<u64, DriverError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query("SELECT MIN(change_id), MAX(change_id) FROM turso_cdc", ())
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        let (Some(oldest), Some(newest)) = (row.get::<Option<i64>>(0)?, row.get::<Option<i64>>(1)?)
        else {
            return Ok(0);
        };
        drop(rows);
        let through = confirmed
            .min(newest - 1)
            .min(oldest.saturating_add(Self::TRIM_ROWS_PER_PUSH - 1));
        if through < oldest {
            return Ok(0);
        }
        // Changes to the change log itself are not captured, so this adds no
        // row of its own (turso_core 0.7.2 `translate/emitter/mod.rs` 1083-1091).
        let trimmed = conn
            .execute(
                "DELETE FROM turso_cdc WHERE change_id <= ?1",
                crate::driver::params![through],
            )
            .await?;
        Ok(trimmed)
    }

    /// The remote's record of this file's last push before the next one.
    /// The engine's own record is behind it when a push it never heard back
    /// from committed (it timed out, its answer was lost, or this server
    /// restarted after one), so then, and first after every start, the
    /// remote is asked.
    async fn remote_had(&self) -> Result<i64, PersistenceError> {
        let hint = self.last_pushed_hint().await.map_err(storage_error)?;
        if !self.inner.unsure.load(Ordering::Acquire) {
            return Ok(hint);
        }
        // A remote that does not answer this counts like a push that timed
        // out, so a hung remote still ends in the restart.
        match tokio::time::timeout(REMOTE_READ_TIMEOUT, self.inner.remote.last_change()).await {
            Ok(read) => Ok(hint.max(read.map_err(storage_error)?)),
            Err(_) => {
                self.inner.timeouts.fetch_add(1, Ordering::AcqRel);
                Err(PersistenceError::Storage(format!(
                    "turso.sync the remote did not say how far it got within {} s",
                    REMOTE_READ_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Whether the next push first asks the remote how far it got, because
    /// a push since this start may have committed there unheard.
    pub fn remote_record_unsure(&self) -> bool {
        self.inner.unsure.load(Ordering::Acquire)
    }

    /// A push the remote answered with a statement error. The remote skips a
    /// statement that fails and commits the rest of the push together with
    /// its record of the push, so a retry would succeed with that change
    /// missing. Its record moving past where it was shows that happened; a
    /// refusal that committed nothing (a busy remote) leaves it in place.
    async fn after_refusal(&self, remote_had: i64, message: &str) {
        match self.inner.remote.last_change().await {
            Ok(remote_has) if remote_has <= remote_had => {
                tracing::warn!(
                    cause = message,
                    "turso.sync push refused without committing; will retry"
                );
            }
            Ok(_) => self.enter(
                CopyState::Stopped(message.to_string()),
                "the remote refused a change and committed the rest",
            ),
            Err(error) => self.enter(
                CopyState::Stopped(format!(
                    "{message} (and its progress could not be read: {error})"
                )),
                "the remote refused a change and its progress could not be read",
            ),
        }
    }

    pub(super) fn require_copying(&self) -> Result<(), PersistenceError> {
        match self.state() {
            CopyState::Copying => Ok(()),
            CopyState::Unchecked => Err(PersistenceError::Storage(
                "turso.sync the remote copy has not been reachable since this start".to_string(),
            )),
            CopyState::Behind => Err(PersistenceError::Storage(
                "turso.sync the file is behind its remote copy; it is restored at the next start"
                    .to_string(),
            )),
            CopyState::Stopped(cause) => Err(PersistenceError::Storage(format!(
                "turso.sync the remote copy is missing a change ({cause})"
            ))),
        }
    }

    /// The remote's record of this file's last pushed change, as the engine
    /// saved it after its last successful push.
    async fn last_pushed_hint(&self) -> Result<i64, DriverError> {
        let info = files::read_sync_metadata(&self.inner.path).await?;
        Ok(info["last_pushed_change_id_hint"].as_i64().unwrap_or(0))
    }

    /// How many rows the local change log holds.
    pub async fn change_log_rows(&self) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM turso_cdc", ())
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        row.get::<i64>(0).map_err(storage_error)
    }

    /// Bytes of row images the local change log holds for changes numbered
    /// above `after`.
    pub async fn change_log_bytes_after(&self, after: i64) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(COALESCE(LENGTH(before), 0) + COALESCE(LENGTH(after), 0) \
                 + COALESCE(LENGTH(updates), 0)), 0) FROM turso_cdc WHERE change_id > ?1",
                crate::driver::params![after],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        row.get::<i64>(0).map_err(storage_error)
    }

    /// The number of the newest committed change in the local change log.
    pub async fn last_change(&self) -> Result<i64, PersistenceError> {
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

    pub(super) async fn query_text(&self, sql: &str) -> Result<String, DriverError> {
        let conn = self.connect().await?;
        let mut rows = conn.query(sql, ()).await?;
        let Some(row) = rows.next().await? else {
            return Ok(String::new());
        };
        Ok(row.get::<String>(0).unwrap_or_default())
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
                    "turso.sync push not done within {} ms",
                    budget.as_millis()
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
                    if started.elapsed() + delay < budget && self.state() == CopyState::Copying =>
                {
                    tracing::warn!(%error, delay_ms = delay.as_millis() as u64, "turso.sync push failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
