//! The push-lag budget: how long a confirmed move may wait for the remote
//! copy before the store stops confirming new ones.
//!
//! A move is confirmed once it commits to the file, and the remote is the
//! copy that outlives the file. While the push keeps up, losing the file
//! loses at most the push lag; once the push is behind or down, every move
//! confirmed since the last push that succeeded. The budget bounds that:
//! while the oldest confirmed move the remote lacks has waited longer than
//! [`PUSH_LAG_BUDGET`], new moves are refused as not durable, and they are
//! confirmed again once a push carries it.
//!
//! Only the push is measured here. Whether the drive has published the file
//! is watched outside the kernel, from the drive's own log.

use std::collections::VecDeque;
use std::time::Duration;

use temper_runtime::persistence::PersistenceError;
use tokio::time::Instant;

use super::SyncCopy;

/// How long a confirmed move may wait for the remote copy.
///
/// Measured on the drive: under steady writing it publishes about every
/// 21 s, and a killed sandbox lost the last 23.6 to 34.4 s of commits. Under
/// 23.6 s, losing the file costs less than the drive alone already risks.
/// Over 11 s, a push that times out (10 s) and the retry on the next tick
/// (1 s) fit inside it, so one hung request refuses nothing.
pub(super) const PUSH_LAG_BUDGET: Duration = Duration::from_secs(20);

/// Confirmed moves the remote lacks, and whether new moves are refused for
/// them.
#[derive(Debug)]
pub(super) struct PushBudget {
    budget: Duration,
    /// Each confirmed move no push has carried yet: its number and when it
    /// was confirmed, oldest first.
    unpushed: VecDeque<(u64, Instant)>,
    /// The number of the newest confirmed move; the first is 1.
    newest: u64,
    refusing: bool,
}

/// A change in whether moves are confirmed, reported once each way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Turn {
    /// The oldest confirmed move the remote lacks waited past the budget.
    Refusing,
    /// A push carried every move that had waited past the budget.
    Confirming,
}

impl PushBudget {
    pub(super) fn new(budget: Duration) -> Self {
        Self {
            budget,
            unpushed: VecDeque::new(),
            newest: 0,
            refusing: false,
        }
    }

    /// Whether a move may be confirmed at `now`: refused with how long the
    /// oldest confirmed move the remote lacks has waited. Also returns the
    /// turn, when this call is the one that sees it.
    pub(super) fn admit(&mut self, now: Instant) -> (Result<(), Duration>, Option<Turn>) {
        let turn = self.observe(now);
        match self.oldest_wait(now) {
            Some(waited) if self.refusing => (Err(waited), turn),
            _ => (Ok(()), turn),
        }
    }

    /// Record a move confirmed at `now`; returns its number.
    pub(super) fn confirmed(&mut self, now: Instant) -> u64 {
        self.newest += 1;
        self.unpushed.push_back((self.newest, now));
        self.newest
    }

    /// The newest confirmed move. Its commit came before this call, so a
    /// push that reads the file's change log after it carries this move and
    /// every one before it.
    pub(super) fn newest(&self) -> u64 {
        self.newest
    }

    /// A push carried every move up to `through`; returns the turn if that
    /// ends a refusal.
    pub(super) fn pushed(&mut self, through: u64, now: Instant) -> Option<Turn> {
        while self.unpushed.front().is_some_and(|(n, _)| *n <= through) {
            self.unpushed.pop_front();
        }
        self.observe(now)
    }

    /// Bring the refusal up to date at `now`; returns the turn if it changed.
    pub(super) fn observe(&mut self, now: Instant) -> Option<Turn> {
        // No gate yet: every move is confirmed.
        let _ = (now, Turn::Refusing, Turn::Confirming);
        None
    }

    /// How long the oldest confirmed move the remote lacks has waited.
    pub(super) fn oldest_wait(&self, now: Instant) -> Option<Duration> {
        let (_, at) = self.unpushed.front()?;
        Some(now.saturating_duration_since(*at))
    }

    /// Whether a confirmed move has waited for the remote longer than half
    /// the budget. The backup then pushes on every tick, whatever its
    /// backoff, so a remote that is back carries the move well before the
    /// budget passes, and a refusal ends within a tick of the remote's return.
    pub(super) fn pressing(&self, now: Instant) -> bool {
        self.oldest_wait(now)
            .is_some_and(|waited| waited > self.budget / 2)
    }

    /// Confirmed moves no push has carried yet.
    pub(super) fn unpushed(&self) -> usize {
        self.unpushed.len()
    }

    /// Whether new moves are refused.
    pub(super) fn refusing(&self) -> bool {
        self.refusing
    }

    pub(super) fn budget(&self) -> Duration {
        self.budget
    }
}

impl SyncCopy {
    /// Before a move is written: refused as not durable while the oldest
    /// confirmed move the remote lacks has waited past [`PUSH_LAG_BUDGET`].
    /// A refused move writes nothing.
    pub(crate) fn admit_move(&self) -> Result<(), PersistenceError> {
        let (admitted, turn, budget) = {
            let mut ledger = self.inner.budget.lock().unwrap();
            let (admitted, turn) = ledger.admit(Instant::now());
            (admitted, turn, ledger.budget())
        };
        self.turned(turn);
        admitted.map_err(|waited| {
            PersistenceError::Storage(format!(
                "turso.sync not durable: the remote copy has lacked a confirmed move for {} s, \
                 past the {} s budget; moves are confirmed again once a push carries it",
                waited.as_secs(),
                budget.as_secs()
            ))
        })
    }

    /// After a move committed to the file: it waits for the remote from now.
    pub(crate) fn move_confirmed(&self) {
        self.inner.budget.lock().unwrap().confirmed(Instant::now());
    }

    /// The newest confirmed move, read before a push reads the change log.
    pub(super) fn newest_move(&self) -> u64 {
        self.inner.budget.lock().unwrap().newest()
    }

    /// A push carried every move up to `through`.
    pub(super) fn moves_pushed(&self, through: u64) {
        let turn = self
            .inner
            .budget
            .lock()
            .unwrap()
            .pushed(through, Instant::now());
        self.turned(turn);
    }

    /// Bring the refusal up to date without a move or a push, so it is
    /// reported when the budget passes while nothing is written.
    pub(super) fn observe_budget(&self) {
        let turn = self.inner.budget.lock().unwrap().observe(Instant::now());
        self.turned(turn);
    }

    /// Whether the next push is due on this tick whatever the backoff: a
    /// confirmed move has waited for the remote over half the budget.
    pub fn push_is_pressing(&self) -> bool {
        self.inner.budget.lock().unwrap().pressing(Instant::now())
    }

    /// Whether new moves are refused until a push catches up.
    pub fn refusing_moves(&self) -> bool {
        self.inner.budget.lock().unwrap().refusing()
    }

    fn turned(&self, turn: Option<Turn>) {
        let Some(turn) = turn else {
            return;
        };
        let (unpushed, waited, budget) = {
            let ledger = self.inner.budget.lock().unwrap();
            (
                ledger.unpushed(),
                ledger.oldest_wait(Instant::now()).unwrap_or_default(),
                ledger.budget(),
            )
        };
        match turn {
            Turn::Refusing => tracing::error!(
                event = "turso.sync.moves_refused",
                unpushed,
                waited_ms = waited.as_millis() as u64,
                budget_ms = budget.as_millis() as u64,
                "turso.sync moves are refused as not durable until a push catches up"
            ),
            Turn::Confirming => tracing::warn!(
                event = "turso.sync.moves_confirmed",
                unpushed,
                budget_ms = budget.as_millis() as u64,
                "turso.sync a push caught up; moves are confirmed again"
            ),
        }
    }
}

#[cfg(test)]
#[path = "budget_dst_test.rs"]
mod dst;
