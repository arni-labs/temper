//! One check at a time on a host.
//!
//! Sibling Temper processes on one host share its memory, and each reads the
//! same free memory ([`crate::host_room`]). A spec check sizes its states
//! budget to that, and an install's joint proofs walk in it, so two at once,
//! in two processes, would each take the share meant for one. On a 2 GB
//! sandbox shared with the daemon that holds its data volume, the kernel then
//! kills whichever crosses the limit, and killing that daemon unmounts the
//! volume (15 Oct 2026). So each caller that checks or installs specs it was
//! sent (`POST /api/specs/validate-ioa`, an app's pushed files) takes an
//! exclusive lock on one fixed file in the system's temporary directory
//! before it reads free memory, and holds it until its work has ended.
//!
//! A caller takes the turn once: a second lock on a new handle in the same
//! process waits for the first, so a function that already holds it must not
//! call one that takes it again.
//!
//! Only those two HTTP handlers take it, and no simulation runs either, so the
//! real file below is never in a simulated run: the lock exists to coordinate
//! real processes, which a simulation has one of.

use std::fs::{File, OpenOptions}; // determinism-ok: host-wide lock file, HTTP handlers only

/// The file every Temper process on this host locks for its turn.
const LOCK_FILE: &str = "temper-installs.lock";

/// This host's turn to check or install, held until the returned file is
/// dropped.
///
/// The lock is taken on a blocking thread: it waits for whichever holder has
/// it, in this process or another. The operating system releases it when the
/// file closes, so a process that dies holding it frees it.
pub async fn take() -> Result<File, String> {
    let path = std::env::temp_dir().join(LOCK_FILE);
    // determinism-ok: a blocking thread waits on the host-wide lock
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "the host's check lock '{}' could not be opened: {error}",
                    path.display()
                )
            })?;
        file.lock().map_err(|error| {
            format!(
                "the host's check lock '{}' could not be taken: {error}",
                path.display()
            )
        })?;
        Ok(file)
    })
    .await
    .map_err(|error| format!("the host's check lock's thread failed: {error}"))?
}
