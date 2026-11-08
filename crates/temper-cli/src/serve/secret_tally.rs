//! What loading each tenant's secrets found during a boot: the tenants
//! restored before the server serves, then those the restore walk brings back
//! after (`super::restore`). The walk's final line is the one that covers
//! every tenant.

use std::collections::{BTreeMap, BTreeSet};

use temper_platform::state::PlatformState;
use temper_server::state::SecretLoadReport;

/// Each tenant's secret load report, and the tenants whose secrets are due.
#[derive(Default)]
pub(super) struct SecretTally {
    /// Tenants whose secrets should be loaded this boot.
    due: BTreeSet<String>,
    /// The latest report of each tenant whose secrets were loaded.
    loaded: BTreeMap<String, SecretLoadReport>,
}

impl SecretTally {
    /// Note that `tenant`'s secrets should be loaded this boot.
    pub(super) fn expect(&mut self, tenant: &str) {
        self.due.insert(tenant.to_string());
    }

    /// Record what loading `tenant`'s secrets found. A later load of the same
    /// tenant (a retried restore) replaces the earlier one.
    pub(super) fn record(&mut self, tenant: &str, report: SecretLoadReport) {
        self.due.insert(tenant.to_string());
        self.loaded.insert(tenant.to_string(), report);
    }

    /// Print one line: the rows of every tenant loaded so far, by outcome,
    /// and how many due tenants were not loaded. Nothing without a vault and
    /// a store, or when there is nothing to count.
    pub(super) fn print(&self, state: &PlatformState, covering: &str) {
        if state.server.secrets_vault.is_none() || state.server.storage_stack.is_none() {
            return;
        }
        let mut total = SecretLoadReport::default();
        for report in self.loaded.values() {
            total.add(*report);
        }
        let unchecked = self
            .due
            .iter()
            .filter(|tenant| !self.loaded.contains_key(*tenant))
            .count();
        if total.total() == 0 && unchecked == 0 {
            return;
        }
        // Each unreadable row was already logged as an error.
        let unreadable_hint = if total.unreadable > 0 {
            " Unreadable rows open with neither vault key; see the errors above."
        } else {
            ""
        };
        let unchecked_hint = if unchecked > 0 {
            " Unchecked tenants' secrets were not loaded; their rows may still need the previous key."
        } else {
            ""
        };
        println!(
            "  Secrets from database ({covering}): {} kept, {} re-sealed, {} deferred, {} unreadable, {unchecked} tenants unchecked.{unreadable_hint}{unchecked_hint}",
            total.kept, total.resealed, total.deferred, total.unreadable
        );
    }
}
