//! The journal mode a synced file is opened in.
//!
//! This release writes in WAL mode. A release on Turso's concurrent writes
//! leaves its files in MVCC mode, and this release is the one such a file
//! comes back to on a rollback, so it returns them to WAL before opening. A
//! file that is not synced needs nothing here: the store's migration sets
//! WAL mode on its first connection.

use std::io::Read;
use std::path::Path;

use super::DriverError;

/// A database file's header holds its read and write format versions at
/// bytes 18 and 19: 2 in WAL mode, 255 in MVCC mode (turso_core 0.8.3
/// `storage/sqlite3_ondisk.rs` 198-201).
const FORMAT_VERSIONS: std::ops::Range<usize> = 18..20;
const MVCC_FORMAT: u8 = 255;

/// Whether the file at `path` is in MVCC mode. A missing file, or one too
/// short to hold a header, is not: the engine makes it in the mode it is
/// opened in.
fn is_mvcc(path: &Path) -> Result<bool, DriverError> {
    let mut header = [0u8; FORMAT_VERSIONS.end];
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(DriverError::SyncSetup(format!(
                "read the header of {}: {error}",
                path.display()
            )));
        }
    };
    match file.read_exact(&mut header) {
        Ok(()) => Ok(header[FORMAT_VERSIONS].contains(&MVCC_FORMAT)),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(DriverError::SyncSetup(format!(
            "read the header of {}: {error}",
            path.display()
        ))),
    }
}

/// Return a file in MVCC mode to WAL, with every row it holds.
///
/// Switching checkpoints the MVCC log into the file, including transactions
/// a process that stopped without closing left only in the log, and then
/// clears the log. It runs on a plain connection before any other opens the
/// file: Turso refuses the switch on a connection that captures changes for
/// Turso Sync, since capture would stop. The connection only switches: a
/// row written on it would not be captured, so it would never be pushed.
pub(crate) async fn return_to_wal(path: &Path) -> Result<(), DriverError> {
    if !is_mvcc(path)? {
        return Ok(());
    }
    let local = path.to_string_lossy();
    let db = turso::Builder::new_local(&local).build().await?;
    let conn = db.connect()?;
    let mut rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    let mode = match rows.next().await? {
        Some(row) => row.get::<String>(0)?,
        None => String::new(),
    };
    drop(rows);
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(DriverError::SyncSetup(format!(
            "{} is in MVCC mode and did not switch to WAL (journal mode now {mode})",
            path.display()
        )));
    }
    tracing::warn!(
        path = %path.display(),
        "turso.sync a file left in MVCC mode by a newer release is back in WAL mode"
    );
    Ok(())
}

/// Scratch measurement only: a new file is restored from its remote, then
/// switched to MVCC on a plain connection before any table exists, so no
/// sequence or change number has to carry over the switch. A file that
/// already exists must already be in MVCC mode.
pub(crate) async fn fresh_into_mvcc(
    path: &Path,
    remote_url: &str,
    auth_token: &str,
) -> Result<(), DriverError> {
    if path.exists() {
        return if is_mvcc(path)? {
            Ok(())
        } else {
            Err(DriverError::SyncSetup(format!(
                "{} exists in WAL mode; the MVCC measurement needs a new file",
                path.display()
            )))
        };
    }
    drop(super::SyncCopy::build(path, remote_url, auth_token).await?);
    let local = path.to_string_lossy();
    let db = turso::Builder::new_local(&local).build().await?;
    let conn = db.connect()?;
    let mut rows = conn.query("PRAGMA journal_mode = 'mvcc'", ()).await?;
    let mode = match rows.next().await? {
        Some(row) => row.get::<String>(0)?,
        None => String::new(),
    };
    drop(rows);
    if !mode.eq_ignore_ascii_case("mvcc") {
        return Err(DriverError::SyncSetup(format!(
            "{} did not switch to MVCC (journal mode now {mode})",
            path.display()
        )));
    }
    tracing::warn!(path = %path.display(), "turso.sync a new file is in MVCC mode");
    Ok(())
}
