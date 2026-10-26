//! Seeded simulation of the push-lag budget while the remote push stalls.
//!
//! Moves arrive at random. The backup pushes the way the server's loop does:
//! on a 1 s tick, one push at a time, waiting longer after each failure up to
//! 30 s, and on every tick while a confirmed move has waited over half the
//! budget. The remote answers,
//! fails fast, or hangs each request until the 10 s attempt timeout, in
//! stalls from 2 s to two minutes. An oracle keeps its own record of every
//! confirmed move and of which ones a push carried, and checks:
//!
//! - no move is confirmed while a confirmed move the remote lacks has waited
//!   longer than the budget;
//! - no move is refused otherwise;
//! - the refusal is reported once when it starts and once when it ends.

use std::time::Duration;

use temper_runtime::scheduler::DeterministicRng;
use tokio::time::Instant;

use super::{PUSH_LAG_BUDGET, PushBudget, Turn};

const SEEDS: u64 = 400;
/// About 17 simulated minutes a seed.
const STEPS: usize = 4_000;
const TICK_MS: u64 = 1_000;
const PUSH_TIMEOUT_MS: u64 = 10_000;
const MAX_BACKOFF_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remote {
    Up,
    /// Every request fails at once (refused connection, error answer).
    Failing,
    /// Every request hangs until the attempt times out.
    Hung,
}

struct Push {
    /// The ledger's newest move when the push started.
    through: u64,
    /// The oracle's count of confirmed moves when the push started; the push
    /// carries all of them.
    carries: usize,
    ends_ms: u64,
    up_at_start: bool,
}

struct Sim {
    seed: u64,
    rng: DeterministicRng,
    base: Instant,
    now_ms: u64,
    remote: Remote,
    remote_until_ms: u64,
    push: Option<Push>,
    next_tick_ms: u64,
    retry_at_ms: u64,
    retry_wait_ms: u64,
    move_chance: f64,
    ledger: PushBudget,
    /// The oracle: when each confirmed move was confirmed, in order, and how
    /// many of the first ones a push carried.
    confirmed_ms: Vec<u64>,
    carried: usize,
    reported_refusing: bool,
    refused: u64,
    turns: u64,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let mut rng = DeterministicRng::new(seed);
        let move_chance = 0.1 + rng.next_bound(80) as f64 / 100.0;
        Self {
            seed,
            rng,
            base: Instant::now(),
            now_ms: 0,
            remote: Remote::Up,
            remote_until_ms: 0,
            push: None,
            next_tick_ms: TICK_MS,
            retry_at_ms: 0,
            retry_wait_ms: TICK_MS,
            move_chance,
            ledger: PushBudget::new(PUSH_LAG_BUDGET),
            confirmed_ms: Vec::new(),
            carried: 0,
            reported_refusing: false,
            refused: 0,
            turns: 0,
        }
    }

    fn at(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }

    /// The oldest confirmed move the remote lacks, when it has waited past
    /// the budget: its index and how long it has waited.
    fn overdue(&self) -> Option<(usize, u64)> {
        let confirmed = *self.confirmed_ms.get(self.carried)?;
        let waited = self.now_ms - confirmed;
        (waited > PUSH_LAG_BUDGET.as_millis() as u64).then_some((self.carried, waited))
    }

    fn saw(&mut self, turn: Option<Turn>) {
        let Some(turn) = turn else {
            return;
        };
        let expected = if self.reported_refusing {
            Turn::Confirming
        } else {
            Turn::Refusing
        };
        assert_eq!(
            turn, expected,
            "seed {} at {} ms: the refusal is reported once when it starts and once when it ends",
            self.seed, self.now_ms
        );
        self.reported_refusing = !self.reported_refusing;
        self.turns += 1;
    }

    /// The remote's weather: up spells of 1 to 90 s, stalls of 2 to 120 s.
    fn weather(&mut self, stalls: bool) {
        if !stalls {
            self.remote = Remote::Up;
            return;
        }
        while self.now_ms >= self.remote_until_ms {
            let (next, seconds) = match self.remote {
                Remote::Up => {
                    let kind = if self.rng.chance(0.5) {
                        Remote::Hung
                    } else {
                        Remote::Failing
                    };
                    (kind, 2 + self.rng.next_bound(119) as u64)
                }
                Remote::Failing | Remote::Hung => (Remote::Up, 1 + self.rng.next_bound(90) as u64),
            };
            self.remote = next;
            self.remote_until_ms += seconds * 1_000;
        }
    }

    fn finish_push(&mut self) {
        let (now_ms, now) = (self.now_ms, self.at(self.now_ms));
        let Some(push) = self.push.take_if(|push| push.ends_ms <= now_ms) else {
            return;
        };
        if push.up_at_start && self.remote == Remote::Up {
            self.carried = self.carried.max(push.carries);
            let turn = self.ledger.pushed(push.through, now);
            self.saw(turn);
            self.retry_wait_ms = TICK_MS;
            self.retry_at_ms = self.now_ms;
        } else {
            self.retry_at_ms = self.now_ms + self.retry_wait_ms;
            self.retry_wait_ms = (self.retry_wait_ms * 2).min(MAX_BACKOFF_MS);
        }
    }

    fn tick(&mut self) {
        while self.next_tick_ms <= self.now_ms {
            self.next_tick_ms += TICK_MS;
            let now = self.at(self.now_ms);
            let turn = self.ledger.observe(now);
            self.saw(turn);
            assert!(
                !self.ledger.refusing() || self.overdue().is_some(),
                "seed {} at {} ms: moves are refused though no confirmed move the remote lacks \
                 has waited past the budget",
                self.seed,
                self.now_ms
            );
            let due = self.now_ms >= self.retry_at_ms || self.ledger.pressing(now);
            if self.push.is_none() && due {
                let lasts_ms = match self.remote {
                    Remote::Up => 50 + self.rng.next_bound(1_950) as u64,
                    Remote::Failing => 5 + self.rng.next_bound(200) as u64,
                    Remote::Hung => PUSH_TIMEOUT_MS,
                };
                self.push = Some(Push {
                    through: self.ledger.newest(),
                    carries: self.confirmed_ms.len(),
                    ends_ms: self.now_ms + lasts_ms,
                    up_at_start: self.remote == Remote::Up,
                });
            }
        }
    }

    fn maybe_move(&mut self) {
        if !self.rng.chance(self.move_chance) {
            return;
        }
        let overdue = self.overdue();
        let now = self.at(self.now_ms);
        let (admitted, turn) = self.ledger.admit(now);
        self.saw(turn);
        match (admitted, overdue) {
            (Ok(()), Some((index, waited))) => panic!(
                "seed {} at {} ms: a move was confirmed while confirmed move {index} has waited \
                 {waited} ms for the remote, past the {} ms budget",
                self.seed,
                self.now_ms,
                PUSH_LAG_BUDGET.as_millis()
            ),
            (Err(waited), None) => panic!(
                "seed {} at {} ms: a move was refused (waited {waited:?}) though no confirmed \
                 move the remote lacks has waited past the budget",
                self.seed, self.now_ms
            ),
            (Ok(()), None) => {
                self.ledger.confirmed(now);
                self.confirmed_ms.push(self.now_ms);
            }
            (Err(_), Some(_)) => self.refused += 1,
        }
    }

    fn step(&mut self, stalls: bool) {
        self.now_ms += 10 + self.rng.next_bound(490) as u64;
        self.weather(stalls);
        self.finish_push();
        self.tick();
        self.maybe_move();
    }
}

#[test]
fn dst_no_move_is_confirmed_while_an_unpushed_one_is_older_than_the_budget() {
    let (mut confirmed, mut refused, mut turns) = (0, 0, 0);
    for seed in 0..SEEDS {
        let mut sim = Sim::new(seed);
        for _ in 0..STEPS {
            sim.step(true);
        }
        // The remote comes back for good: every move is pushed and moves are
        // confirmed again.
        let calm_until = sim.now_ms + 2 * MAX_BACKOFF_MS + PUSH_LAG_BUDGET.as_millis() as u64;
        while sim.now_ms < calm_until {
            sim.step(false);
        }
        assert!(
            !sim.ledger.refusing() && sim.overdue().is_none(),
            "seed {seed}: moves are confirmed again once the remote is back"
        );
        confirmed += sim.confirmed_ms.len();
        refused += sim.refused;
        turns += sim.turns;
    }
    assert!(
        refused > 0 && turns > 0 && confirmed > 0,
        "the stalls must outlast the budget for the scenario to test it: {confirmed} confirmed, \
         {refused} refused, {turns} turns"
    );
}
