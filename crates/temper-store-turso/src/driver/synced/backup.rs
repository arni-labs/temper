//! How far the remote copy is behind the file.

use std::time::Duration;

use temper_runtime::persistence::PersistenceError;

use super::{CopyState, SyncCopy};

/// Where the backup of one database stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupStatus {
    /// What the backup can do now.
    pub state: CopyState,
    /// Changes committed to the file that the remote does not have yet.
    pub unpushed_changes: u64,
    /// How long the remote has lacked a committed change.
    pub behind_for: Duration,
    /// Whether a push is running now.
    pub pushing: bool,
    /// Push attempts that failed since this start.
    pub push_failures: u64,
}

impl SyncCopy {
    /// Where the backup stands. Not measured yet: reports nothing behind.
    pub async fn backup_status(&self) -> Result<BackupStatus, PersistenceError> {
        Ok(BackupStatus {
            state: self.state(),
            unpushed_changes: 0,
            behind_for: Duration::ZERO,
            pushing: false,
            push_failures: 0,
        })
    }
}
