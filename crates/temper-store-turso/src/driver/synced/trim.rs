//! Trimming the change log the remote has confirmed, off every answer's path.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{Connection, DriverError, SyncCopy};

/// How long one trim batch should hold the database's write lock. Writers
/// wait behind a batch, and a request arriving mid-batch is noticed only when
/// it ends. A batch is sized from the last one's rate, and may double it, so
/// one fast batch followed by the usual rate runs twice this long: on CI, a
/// 100 ms target ran a 170 ms batch. Half of 100 ms keeps that case near
/// 100 ms, the most a write should wait on a trim.
const BATCH_TARGET: Duration = Duration::from_millis(50);

/// Rows the first batch deletes. At the slowest rate measured, a cold log on
/// a Tensorlake drive (10,000 rows in 34.2 s, about 290 rows a second), 12
/// rows take about 41 ms, within [`BATCH_TARGET`].
pub(super) const FIRST_BATCH_ROWS: i64 = 12;

impl SyncCopy {
    /// Delete change log rows the remote has confirmed, for about `budget`,
    /// keeping the newest row. Returns the rows deleted.
    /// After a quarter of the budget, stop when `requests_in_flight` is true.
    /// The floor prevents starvation under constant load: the Factory grows
    /// about 56 rows/s; even 250 ms at the old cold 290 rows/s nets only
    /// about 19 rows/s, roughly 10 h to clear 667k rows. Idle ticks use the
    /// whole budget. The caller supplies request pressure, without HTTP here.
    ///
    /// In 'full' mode each row holds whole row images, and nothing else
    /// trims the log, so it grows with every write. Only the background loop
    /// calls this: an answer's push never starts a trim.
    ///
    /// The engine reads the log only above the remote's record of this
    /// file's last push: a push scans from that record plus one
    /// (turso_sync_engine 0.7.2 `database_sync_operations.rs` 2853,
    /// `database_tape.rs` 416-432), a pull replays only rows above it
    /// (`database_sync_engine.rs` 864-887), and its stats count rows above
    /// it (1405-1427). The record trimmed to is the one the engine saved
    /// after the remote committed, so no row at or below it is read again.
    ///
    /// No push lock is held. A push reads only rows above the remote's
    /// record, which is at or past the saved one, and a commit adds only
    /// rows numbered above the newest, so a push in flight and writes that
    /// land meanwhile touch none of the rows a batch deletes.
    ///
    /// The newest row stays because in WAL mode a change is numbered one
    /// above the largest in the log (turso_core 0.7.2
    /// `translate/emitter/mod.rs` 1223-1239, `vdbe/execute.rs` 10823-10841).
    /// An empty log would number the next change 1, at or below what the
    /// remote holds, and the next push would skip it. [`Self::last_change`],
    /// the start's check against the remote and the check after each push
    /// read that same newest row.
    ///
    /// Each batch is its own short DELETE, so the write lock is free between
    /// batches. Batches are sized by time: each deletes as many rows as the
    /// last one's rate fits in [`BATCH_TARGET`], at most twice the rows the
    /// last one deleted, so a cold stretch of the log shrinks them at once
    /// and a warm one grows them step by step. A batch starts only within
    /// `budget` and always runs to its end: one cut off mid-statement would
    /// be rolled back and its work lost. A failed call leaves the rows for
    /// the next.
    pub async fn trim_pushed(
        &self,
        budget: Duration,
        requests_in_flight: impl Fn() -> bool,
    ) -> Result<u64, PersistenceError> {
        self.require_copying()?;
        let started = Instant::now();
        let confirmed = self.last_pushed_hint().await.map_err(storage_error)?;
        let conn = self.connect().await.map_err(storage_error)?;
        let mut trimmed = 0u64;
        let mut batches = 0u32;
        let (through, oldest, newest) = loop {
            let Some((oldest, newest)) = change_ids(&conn).await.map_err(storage_error)? else {
                return Ok(trimmed);
            };
            let through = confirmed.min(newest - 1);
            if through < oldest
                || started.elapsed() >= budget
                || (started.elapsed() >= budget / 4 && requests_in_flight())
            {
                break (through, oldest, newest);
            }
            let rows = self.inner.trim_batch.load(Ordering::Acquire);
            let upper = through.min(oldest.saturating_add(rows - 1));
            let batch_started = Instant::now();
            // Changes to the change log itself are not captured, so this adds no
            // row of its own (turso_core 0.7.2 `translate/emitter/mod.rs` 1083-1091).
            let deleted = conn
                .execute(
                    "DELETE FROM turso_cdc WHERE change_id <= ?1",
                    crate::driver::params![upper],
                )
                .await
                .map_err(storage_error)?;
            self.inner.trim_batch.store(
                next_batch(deleted, batch_started.elapsed()),
                Ordering::Release,
            );
            trimmed += deleted;
            batches += 1;
            // DELETE can complete without yielding on a warm disk. Always let
            // waiting requests run before deciding whether to start another.
            // A yielded task is woken only after the scheduler turns the
            // driver without blocking, which fires due timers and reads ready
            // sockets (tokio 1.50 `scheduler/current_thread/mod.rs` 794-798
            // and 421-434, `multi_thread/worker.rs` 586-590,
            // `time/mod.rs` 254-255), so no pause is needed for that.
            tokio::task::yield_now().await;
        };
        // Change numbers are consecutive (each one above the newest), and a
        // trim deletes from the oldest, so this is about the rows left.
        let rows_left = newest - oldest + 1;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if through >= oldest {
            tracing::info!(
                trimmed,
                batches,
                elapsed_ms,
                rows_left,
                confirmed,
                "turso.sync trimmed the change log; confirmed rows are left for the next tick"
            );
        } else if trimmed > 0 {
            tracing::debug!(
                trimmed,
                batches,
                elapsed_ms,
                rows_left,
                confirmed,
                "turso.sync trimmed the change log"
            );
        }
        Ok(trimmed)
    }
}

/// The oldest and newest change numbers in the log, if it has any rows.
async fn change_ids(conn: &Connection) -> Result<Option<(i64, i64)>, DriverError> {
    // turso 0.7.2 reads one end of the log for a lone MIN or MAX, but scans every
    // row for both in one query (turso_core `translate/optimizer/mod.rs` 662-718).
    let oldest = one_id(conn, "SELECT MIN(change_id) FROM turso_cdc").await?;
    let newest = one_id(conn, "SELECT MAX(change_id) FROM turso_cdc").await?;
    Ok(oldest.zip(newest))
}

async fn one_id(conn: &Connection, sql: &str) -> Result<Option<i64>, DriverError> {
    let mut rows = conn.query(sql, ()).await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let id = row.get::<Option<i64>>(0)?;
    drop(rows);
    Ok(id)
}

/// Rows for the batch after one that deleted `deleted` rows in `took`: what
/// that rate fits in [`BATCH_TARGET`], between one row and twice `deleted`.
fn next_batch(deleted: u64, took: Duration) -> i64 {
    let deleted = i64::try_from(deleted).unwrap_or(i64::MAX).max(1);
    let took_us = i128::try_from(took.as_micros()).unwrap_or(i128::MAX).max(1);
    let fits = i128::from(deleted) * BATCH_TARGET.as_micros() as i128 / took_us;
    let fits = i64::try_from(fits).unwrap_or(i64::MAX);
    fits.clamp(1, deleted.saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_batch_shrinks_the_next_and_a_fast_one_at_most_doubles_it() {
        // 25 rows in 1 s: about 1 fits in 50 ms.
        assert_eq!(next_batch(25, Duration::from_secs(1)), 1);
        // 25 rows in 1 ms: 1,250 would fit, but growth is at most double.
        assert_eq!(next_batch(25, Duration::from_millis(1)), 50);
        // Exactly on target keeps the size.
        assert_eq!(next_batch(400, BATCH_TARGET), 400);
        // Never below one row, however slow.
        assert_eq!(next_batch(1, Duration::from_secs(60)), 1);
    }
}
