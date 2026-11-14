//! Feature flags this process reads from outside, so unfinished code can ship
//! turned off and be turned on without a new binary.
//!
//! The deployment that runs this process names an address that answers the
//! flags on for it (`TEMPER_FLAGS_URL`, `GET` with `TEMPER_FLAGS_TOKEN` as a
//! bearer token) as JSON: `{"on": ["name", ...]}`. Who a flag is on for is
//! decided there; this process only asks whether a name is in the list.
//!
//! Every flag is off until a read succeeds, and stays as the last good read
//! when a later one fails, so a flag service that cannot be reached never
//! turns anything on. With no address set, every flag is off. The address is
//! read again every [`READ_EVERY`], so a change shows within about that long.
//!
//! A flag decides what the process does at its edges (a route, a background
//! loop); it is not read inside an entity's transition, whose outcome must
//! not depend on anything outside its own events.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;

/// How often the flags are read again.
pub const READ_EVERY: Duration = Duration::from_secs(60);
/// How long one read may take before it counts as failed.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The flags as last read: every flag off before the first good read.
#[derive(Debug, Default)]
pub struct Flags {
    on: RwLock<Option<BTreeSet<String>>>,
}

/// What the flag address answers.
#[derive(Debug, Deserialize)]
struct Answer {
    on: Vec<String>,
}

impl Flags {
    /// Whether the flag `name` is on for this process. Off for a flag never
    /// read, an unknown name, or before the first good read.
    pub fn on(&self, name: &str) -> bool {
        self.on
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|on| on.contains(name))
    }

    /// Every flag on for this process, sorted; empty before the first good
    /// read.
    pub fn names_on(&self) -> Vec<String> {
        self.on
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|on| on.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Keep an answer's body if it is one; anything else leaves the flags as
    /// they were. Returns whether it was kept.
    fn keep(&self, body: &[u8]) -> bool {
        let Ok(answer) = serde_json::from_slice::<Answer>(body) else {
            return false;
        };
        let on = answer.on.into_iter().filter(|n| !n.is_empty()).collect();
        *self.on.write().unwrap_or_else(|e| e.into_inner()) = Some(on);
        true
    }
}

/// Read the flags from `url` now and every [`READ_EVERY`] after, for as long
/// as the process runs.
pub fn spawn_reader(flags: Arc<Flags>, url: String, token: String) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|error| format!("flags client: {error}"))?;
    tokio::spawn(async move {
        let mut every = tokio::time::interval(READ_EVERY); // determinism-ok: the process's own clock
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            if let Err(error) = read_once(&client, &flags, &url, &token).await {
                tracing::warn!(%error, "flags read failed; the last good read stands");
            }
        }
    });
    Ok(())
}

async fn read_once(
    client: &reqwest::Client,
    flags: &Flags,
    url: &str,
    token: &str,
) -> Result<(), String> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("answered {status}"));
    }
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if flags.keep(&body) {
        Ok(())
    } else {
        Err("the answer is not {\"on\": [names]}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flag_is_off_before_a_good_read() {
        let flags = Flags::default();
        assert!(!flags.on("anything"));
        assert!(flags.names_on().is_empty());
    }

    #[test]
    fn a_good_read_turns_on_only_what_it_names() {
        let flags = Flags::default();
        assert!(flags.keep(br#"{"on": ["b", "a", ""]}"#));
        assert!(flags.on("a") && flags.on("b"));
        assert!(!flags.on("c") && !flags.on(""));
        assert_eq!(flags.names_on(), ["a", "b"]);
    }

    #[test]
    fn a_bad_answer_leaves_the_last_good_read() {
        let flags = Flags::default();
        assert!(flags.keep(br#"{"on": ["a"]}"#));
        for bad in [&b"not json"[..], br#"{"off": []}"#, br#"{"on": "a"}"#] {
            assert!(!flags.keep(bad));
            assert!(flags.on("a"));
        }
        assert!(flags.keep(br#"{"on": []}"#));
        assert!(!flags.on("a"));
    }
}
