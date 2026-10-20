//! `temper sync-copy`: inspect or repair a database whose changes are copied
//! to Turso Cloud (`TURSO_SYNC_URL`).
//!
//! When the remote refuses a change it still commits the rest of that push,
//! so it is missing one, and the server stops confirming writes (it answers
//! "SavingStopped") until an operator acts, with the app stopped:
//!
//! - `status` shows the stop and how far the file and its last push got.
//! - If the file is right: rebuild the remote from a copy of it (drop the
//!   copy's `turso_cdc` and `turso_cdc_version` tables, upload it as the app's
//!   Turso database), then `restore` so the next start downloads that copy
//!   and gives the file fresh sync records.
//! - If the remote is right: `restore` alone; the file is moved aside (kept
//!   for inspection) and the next start downloads the remote.
//! - If the stop was raised in error and the remote holds every change:
//!   `resume` clears it and the next start confirms writes again.

use std::io::Write;

use anyhow::Result;
use temper_store_turso::SyncCopy;

#[derive(clap::Subcommand)]
pub enum Action {
    /// Show the stop, if any, and how far the file and its last push got.
    Status {
        /// The database file (the app's TURSO_URL, `file:` optional).
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: clear the stop so the next start confirms writes again.
    Resume {
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: move the file aside so the next start restores it from the remote.
    Restore {
        #[arg(long)]
        db: String,
    },
}

pub fn run(action: Action) -> Result<()> {
    let report = match action {
        Action::Status { db } => SyncCopy::describe(&db)?,
        Action::Resume { db } => match SyncCopy::clear_stop(&db)? {
            Some(cause) => format!("cleared the stop ({cause}); the next start confirms writes"),
            None => format!("{db} is not stopped"),
        },
        Action::Restore { db } => format!(
            "moved to {}; the next start restores the database from its remote copy",
            SyncCopy::set_aside(&db)?.display()
        ),
    };
    writeln!(std::io::stdout(), "{report}")?;
    Ok(())
}
