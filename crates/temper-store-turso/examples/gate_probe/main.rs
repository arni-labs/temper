//! Measures the Turso write gate on a real drive: how long a move's write
//! holds the permit, how long writers wait for it, and when the queue grows.
//!
//! A move is what the entity actor writes for one transition: one event with
//! its query-plane row and state-timer row in the journal's transaction, at
//! high priority, then its trajectory row at low priority. Each move runs on a
//! task of its own, and moves on one entity wait for each other, as actors do.
//! The gate's wait comes from the gate itself (its `wait_ms` per acquire).
//!
//! ```text
//! gate_probe <db file> closed <moves>                 one move at a time
//! gate_probe <db file> open <moves/s> <secs>          Poisson arrivals
//! gate_probe <db file> slow <moves/s> <secs> <kb>     open, plus one big move halfway
//! ```
//!
//! Prints one JSON line of results. The database is a local file: no Turso
//! Sync, so no push and no change log; pushes run after a request answers.

mod gate_layer;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{
    CommitRows, EventMetadata, EventStore, PersistenceEnvelope, ProjectionRow, StateTimerRecord,
    StateTimerRow,
};
use temper_store_turso::{TursoEventStore, TursoTrajectoryInsert};
use tracing::Instrument;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

use gate_layer::{ACQUIRES, GateLayer};

const ENTITIES: usize = 50;

/// What one move saw.
#[derive(Debug, Clone, Default)]
struct Seen {
    /// From its arrival to its turn on its entity.
    entity_wait_us: u64,
    /// The append call, gate wait included.
    append_us: u64,
    /// From its planned arrival to the append's answer: what a caller waits,
    /// a runtime busy with other moves included.
    latency_us: u64,
    /// Synced store: the push its answer waits for after the append.
    push_us: u64,
    ok: bool,
    big: bool,
    arrived_at_ms: u64,
}

struct Probe {
    store: TursoEventStore,
    sequences: Vec<tokio::sync::Mutex<u64>>,
    payload: String,
    in_flight: AtomicUsize,
    started: Instant,
}

impl Probe {
    async fn one_move(
        self: &Arc<Self>,
        n: u64,
        entity: usize,
        big_kb: usize,
        arrived: Instant,
    ) -> Seen {
        let mut seq = self.sequences[entity].lock().await;
        let entity_wait = arrived.elapsed();
        let entity_id = format!("probe-{entity:04}");
        let pid = format!("default:ProbeTicker:{entity_id}");
        let payload = if big_kb > 0 {
            "x".repeat(big_kb * 1024)
        } else {
            self.payload.clone()
        };
        let now = chrono::Utc::now();
        let event = PersistenceEnvelope {
            sequence_nr: *seq + 1,
            event_type: "Rest".to_string(),
            payload: serde_json::json!({ "action": "Rest", "params": { "moved_via": "gate-probe", "body": payload } }),
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: now,
                actor_id: pid.clone(),
            },
        };
        let fields = serde_json::json!({ "Id": entity_id, "Status": "Resting", "moved_via": "gate-probe", "moved_at": now.to_rfc3339(), "count": *seq + 1 });
        let state =
            serde_json::json!({ "status": "Resting", "fields": fields, "sequence_nr": *seq + 1 });
        let timer = StateTimerRecord {
            tenant: "default".to_string(),
            entity_type: "ProbeTicker".to_string(),
            entity_id: entity_id.clone(),
            state: "Resting".to_string(),
            arm_id: uuid::Uuid::new_v4().to_string(),
            sequence_nr: *seq + 1,
            started_at: now.to_rfc3339(),
            params: "{}".to_string(),
            armed_by: r#"{"principal":"gate-probe"}"#.to_string(),
        };
        let rows = CommitRows {
            projection: Some(ProjectionRow::Upsert {
                status: "Resting",
                fields: &fields,
                state: &state,
            }),
            state_timer: Some(StateTimerRow::Arm(&timer)),
            ..CommitRows::default()
        };
        let span = tracing::info_span!("probe_move", mv = n);
        let append_started = Instant::now();
        let result = self
            .store
            .append_with_rows(&pid, *seq, std::slice::from_ref(&event), rows)
            .instrument(span.clone())
            .await;
        let append = append_started.elapsed();
        // A synced store answers a request only after its changes reach the
        // copy (temper-cli serve/sync_copy.rs), outside the write permit.
        let push_started = Instant::now();
        if let Some(copy) = self.store.sync_copy() {
            if let Err(error) = copy.push_committed().await {
                eprintln!("move {n}: push before answering failed: {error}");
            }
        }
        let push = push_started.elapsed();
        let latency = arrived.elapsed();
        if let Ok(new_seq) = result {
            *seq = new_seq;
        } else if let Err(error) = &result {
            eprintln!("move {n} on {entity_id}: {error}");
        }
        drop(seq);
        // The trajectory row is written behind the move, as the server's
        // outbox does: nothing waits for it.
        let store = self.store.clone();
        let created_at = now.to_rfc3339();
        tokio::spawn(
            async move {
                let _ = store
                    .persist_trajectory(TursoTrajectoryInsert {
                        tenant: "default",
                        entity_type: "ProbeTicker",
                        entity_id: &entity_id,
                        action: "Rest",
                        success: true,
                        from_status: Some("Swinging"),
                        to_status: Some("Resting"),
                        error: None,
                        agent_id: Some("gate-probe"),
                        session_id: None,
                        authz_denied: Some(false),
                        denied_resource: None,
                        denied_module: None,
                        source: Some("probe"),
                        spec_governed: Some(true),
                        created_at: &created_at,
                        request_body: Some(r#"{"moved_via":"gate-probe"}"#),
                        intent: None,
                        matched_policy_ids: None,
                        capture_seq: None,
                    })
                    .await;
            }
            .instrument(span),
        );
        Seen {
            entity_wait_us: entity_wait.as_micros() as u64,
            append_us: append.as_micros() as u64,
            latency_us: latency.as_micros() as u64,
            push_us: push.as_micros() as u64,
            ok: result.is_ok(),
            big: big_kb > 0,
            arrived_at_ms: (arrived - self.started).as_millis() as u64,
        }
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn summary(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    serde_json::json!({
        "n": values.len(),
        "p50": pct(&values, 0.50),
        "p90": pct(&values, 0.90),
        "p99": pct(&values, 0.99),
        "max": values.last().copied().unwrap_or(0),
    })
}

/// A seeded exponential gap for Poisson arrivals at `rate` per second.
fn next_gap(state: &mut u64, rate: f64) -> Duration {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    let u = ((*state >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0);
    Duration::from_secs_f64(-u.ln() / rate)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage =
        "gate_probe <db file> closed <moves> | open <rate> <secs> | slow <rate> <secs> <kb>";
    let path = args.get(1).expect(usage).clone();
    let mode = args.get(2).expect(usage).clone();
    let nums: Vec<f64> = args[3..].iter().map(|a| a.parse().expect(usage)).collect();
    let num = move |i: usize| -> f64 { *nums.get(i - 3).expect(usage) };
    let arg_tail = args[3..].to_vec();

    let subscriber = tracing_subscriber::registry()
        .with(
            GateLayer.with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target(
                        "temper_store_turso::store::write_gate",
                        tracing::Level::DEBUG,
                    )
                    .with_target("gate_probe", tracing::Level::INFO),
            ),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
        );
    tracing::subscriber::set_global_default(subscriber).expect("subscriber");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let store = match std::env::var("PROBE_SYNC_URL") {
            Ok(url) => {
                let token = std::env::var("PROBE_SYNC_TOKEN").unwrap_or_else(|_| "local".to_string());
                TursoEventStore::new_synced(&path, &url, &token)
                    .await
                    .expect("open synced store")
            }
            Err(_) => TursoEventStore::new(&format!("file:{path}"), None)
                .await
                .expect("open store"),
        };
        let upkeep_log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let upkeep = store.sync_copy().cloned().map(|copy| {
            let (store, log) = (store.clone(), upkeep_log.clone());
            let started = Instant::now();
            let max_wal: u64 = std::env::var("PROBE_CHECKPOINT_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
                * 1024
                * 1024;
            // As temper-cli serve/sync_copy.rs spawn_upkeep: each tick pushes,
            // trims the change log for up to 1 s, and checkpoints once the
            // WAL passes 16 MB. Each step's time is recorded.
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let at = started.elapsed().as_millis() as u64;
                    let t = Instant::now();
                    let pushed = copy.push_committed().await.is_ok();
                    let push_ms = t.elapsed().as_millis() as u64;
                    let t = Instant::now();
                    let trimmed = store.trim_change_log(Duration::from_secs(1)).await.unwrap_or(0);
                    let trim_ms = t.elapsed().as_millis() as u64;
                    let t = Instant::now();
                    let checkpointed = copy.checkpoint_if_wal_over(max_wal).await;
                    let checkpoint_ms = t.elapsed().as_millis() as u64;
                    if push_ms >= 50 || trim_ms >= 50 || checkpoint_ms >= 20 || matches!(checkpointed, Ok(true)) {
                        log.lock().unwrap().push(serde_json::json!({
                            "at_ms": at, "push_ms": push_ms, "pushed": pushed,
                            "trim_ms": trim_ms, "trimmed": trimmed,
                            "checkpoint_ms": checkpoint_ms,
                            "checkpointed": format!("{checkpointed:?}"),
                        }));
                    }
                }
            })
        });
        let probe = Arc::new(Probe {
            store,
            sequences: (0..ENTITIES).map(|_| tokio::sync::Mutex::new(0)).collect(),
            payload: "x".repeat(
                std::env::var("PROBE_PAYLOAD_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(400),
            ),
            in_flight: AtomicUsize::new(0),
            started: Instant::now(),
        });
        // Each entity's sequence continues the file's journal, so a run can
        // follow another on the same file.
        for entity in 0..ENTITIES {
            let pid = format!("default:ProbeTicker:probe-{entity:04}");
            let events = probe.store.read_events(&pid, 0).await.expect("read journal");
            *probe.sequences[entity].lock().await =
                events.last().map(|e| e.sequence_nr).unwrap_or(0);
        }
        ACQUIRES.lock().unwrap().clear();

        let started = Instant::now();
        let seen: Arc<Mutex<BTreeMap<u64, Seen>>> = Arc::default();
        let mut depth: Vec<(u64, usize)> = Vec::new();
        let mut planned = 0u64;
        match mode.as_str() {
            "closed" => {
                let moves = num(3) as u64;
                for n in 0..moves {
                    let s = probe
                        .one_move(n, (n as usize) % ENTITIES, 0, Instant::now())
                        .await;
                    seen.lock().unwrap().insert(n, s);
                }
                planned = moves;
            }
            "open" | "slow" => {
                let rate = num(3);
                let secs = num(4);
                let big_kb = if mode == "slow" { num(5) as usize } else { 0 };
                let mut rng = 0x9E37_79B9_7F4A_7C15u64;
                let mut next = Instant::now();
                let end = started + Duration::from_secs_f64(secs);
                let mut big_sent = big_kb == 0;
                let mut handles = Vec::new();
                let mut last_sample = Instant::now();
                while next < end {
                    tokio::time::sleep_until(next.into()).await;
                    let n = planned;
                    planned += 1;
                    let big = if !big_sent && started.elapsed().as_secs_f64() >= secs / 2.0 {
                        big_sent = true;
                        big_kb
                    } else {
                        0
                    };
                    let planned = next;
                    let (probe2, seen2) = (probe.clone(), seen.clone());
                    probe.in_flight.fetch_add(1, Ordering::AcqRel);
                    handles.push(tokio::spawn(async move {
                        let s = probe2
                            .one_move(n, (n as usize * 7919) % ENTITIES, big, planned)
                            .await;
                        probe2.in_flight.fetch_sub(1, Ordering::AcqRel);
                        seen2.lock().unwrap().insert(n, s);
                    }));
                    if last_sample.elapsed() >= Duration::from_millis(250) {
                        depth.push((
                            started.elapsed().as_millis() as u64,
                            probe.in_flight.load(Ordering::Acquire),
                        ));
                        last_sample = Instant::now();
                    }
                    next += next_gap(&mut rng, rate);
                }
                for handle in handles {
                    let _ = handle.await;
                }
            }
            other => panic!("unknown mode {other}: {usage}"),
        }
        let elapsed = started.elapsed();
        // Let the trajectory rows behind the last moves land.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Some(upkeep) = upkeep {
            upkeep.abort();
        }

        let seen = seen.lock().unwrap().clone();
        let acquires = ACQUIRES.lock().unwrap().clone();
        let mut append_wait: BTreeMap<u64, u64> = BTreeMap::new();
        let mut low_waits = Vec::new();
        for a in &acquires {
            match (a.operation.as_str(), a.mv) {
                ("turso.append", Some(mv)) => {
                    // The gate rounds to whole ms; a retry adds its wait.
                    *append_wait.entry(mv).or_default() += a.wait_ms;
                }
                _ => low_waits.push(a.wait_ms),
            }
        }
        let ok: Vec<(&u64, &Seen)> = seen.iter().filter(|(_, s)| s.ok).collect();
        let hold_us: Vec<u64> = ok
            .iter()
            .map(|(n, s)| s.append_us.saturating_sub(append_wait.get(n).copied().unwrap_or(0) * 1000))
            .collect();
        let big: Vec<serde_json::Value> = seen
            .iter()
            .filter(|(_, s)| s.big)
            .map(|(n, s)| {
                let wait = append_wait.get(n).copied().unwrap_or(0);
                serde_json::json!({ "move": n, "arrived_at_ms": s.arrived_at_ms, "append_ms": s.append_us / 1000, "latency_ms": s.latency_us / 1000, "gate_wait_ms": wait, "hold_ms": (s.append_us / 1000).saturating_sub(wait) })
            })
            .collect();
        // Moves that arrived in the 5 s after the big one: what it did to them.
        let behind_big: Vec<u64> = match seen.iter().find(|(_, s)| s.big) {
            Some((_, b)) => seen
                .iter()
                .filter(|(_, s)| !s.big && s.arrived_at_ms >= b.arrived_at_ms && s.arrived_at_ms < b.arrived_at_ms + 5000)
                .map(|(_, s)| s.latency_us / 1000)
                .collect(),
            None => Vec::new(),
        };
        // When the slow holds happened, to see whether they follow the
        // drive's own rhythm (tlfs publishes every few seconds).
        let slow_holds: Vec<(u64, u64)> = ok
            .iter()
            .map(|(n, s)| {
                let wait = append_wait.get(n).copied().unwrap_or(0);
                (s.arrived_at_ms, (s.append_us / 1000).saturating_sub(wait))
            })
            .filter(|(_, hold)| *hold >= 100)
            .take(60)
            .collect();
        let depth_last: Vec<usize> = depth.iter().rev().take(8).map(|d| d.1).collect();
        let depth_first: Vec<usize> = depth.iter().take(8).map(|d| d.1).collect();
        let out = serde_json::json!({
            "mode": mode,
            "args": arg_tail,
            "planned_moves": planned,
            "moves_ok": ok.len(),
            "moves_failed": seen.len() - ok.len(),
            "secs": elapsed.as_secs_f64(),
            "moves_per_s": ok.len() as f64 / elapsed.as_secs_f64(),
            "append_total_us": summary(ok.iter().map(|(_, s)| s.append_us).collect()),
            "append_hold_us": summary(hold_us),
            "append_gate_wait_ms": summary(ok.iter().map(|(n, _)| append_wait.get(n).copied().unwrap_or(0)).collect()),
            "entity_wait_us": summary(ok.iter().map(|(_, s)| s.entity_wait_us).collect()),
            "low_priority_gate_wait_ms": summary(low_waits),
            "in_flight_first_2s": depth_first,
            "in_flight_last_2s": depth_last,
            "in_flight_max": depth.iter().map(|d| d.1).max().unwrap_or(0),
            "big_move": big,
            "slow_holds_at_ms_hold_ms": slow_holds,
            "latency_ms_of_moves_arriving_5s_after_big": summary(behind_big),
            "latency_from_arrival_us": summary(ok.iter().map(|(_, s)| s.latency_us).collect()),
            "push_before_answer_us": summary(ok.iter().map(|(_, s)| s.push_us).collect()),
            "upkeep": upkeep_log.lock().unwrap().clone(),
        });
        println!("{out}");
    });
}
