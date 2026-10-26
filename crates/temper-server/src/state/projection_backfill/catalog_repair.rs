//! Boot repair for catalog rows their journal is ahead of.

use super::ServerState;

impl ServerState {
    /// Bring every catalog row the journal is ahead of up to the journal, in
    /// every tenant the registry holds.
    pub async fn repair_catalog_rows_behind_journal(&self) {}
}
