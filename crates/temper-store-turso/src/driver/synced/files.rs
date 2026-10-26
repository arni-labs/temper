//! The database file and the files the sync engine keeps beside it.

use std::path::{Path, PathBuf};

use super::DriverError;

/// Written next to the database when the remote refused a change.
pub(super) const DIVERGED_SUFFIX: &str = "-sync-diverged";

/// Written next to the database while it is restored from the remote, and
/// removed once the restore has finished. A file found with it holds only part
/// of the remote, and nothing of its own.
pub(super) const RESTORING_SUFFIX: &str = "-sync-restoring";

/// Move the database file and every file the engine keeps beside it to
/// `<name>.behind-<unix seconds>`, so the next open restores from the remote.
pub(super) fn move_aside(path: &Path) -> Result<PathBuf, DriverError> {
    let failed =
        |error: std::io::Error| DriverError::SyncSetup(format!("move {}: {error}", path.display()));
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let stamp = std::time::SystemTime::now() // determinism-ok: names a file moved aside once
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let aside = dir.join(format!("{name}.behind-{stamp}"));
    std::fs::create_dir_all(&aside).map_err(failed)?;
    for entry in std::fs::read_dir(dir).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if file == name || file.starts_with(&format!("{name}-")) {
            std::fs::rename(entry.path(), aside.join(&file)).map_err(failed)?;
        }
    }
    Ok(aside)
}

/// Written next to the database while it waits to be checked against the
/// remote: the newest change the file held before that start committed
/// anything. A later start keeps the number, since what it commits meanwhile
/// says nothing about whether the file is behind.
const UNCHECKED_SUFFIX: &str = "-sync-unchecked";

pub(super) fn unchecked_since(path: &Path) -> Option<i64> {
    std::fs::read_to_string(sidecar(path, UNCHECKED_SUFFIX))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(super) fn mark_unchecked(path: &Path, opened_at: i64) -> Result<(), DriverError> {
    if unchecked_since(path).is_some() {
        return Ok(());
    }
    std::fs::write(sidecar(path, UNCHECKED_SUFFIX), opened_at.to_string())
        .map_err(|error| DriverError::SyncSetup(format!("record the unchecked start: {error}")))
}

pub(super) fn mark_checked(path: &Path) {
    let _ = std::fs::remove_file(sidecar(path, UNCHECKED_SUFFIX));
}

/// The sync engine's metadata for the file (`<db>-info`).
pub(super) fn sync_metadata(path: &Path) -> Result<serde_json::Value, DriverError> {
    let info = std::fs::read(sidecar(path, "-info"))
        .map_err(|error| DriverError::SyncSetup(format!("read sync metadata: {error}")))?;
    serde_json::from_slice(&info)
        .map_err(|error| DriverError::SyncSetup(format!("parse sync metadata: {error}")))
}

/// Remove the recorded stop, returning the cause it held.
pub(super) fn clear_stop(path: &Path) -> Result<Option<String>, DriverError> {
    let marker = sidecar(path, DIVERGED_SUFFIX);
    let Ok(cause) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let cause = cause.trim().to_string();
    std::fs::remove_file(&marker)
        .map_err(|error| DriverError::SyncSetup(format!("remove {}: {error}", marker.display())))?;
    Ok(Some(cause))
}

/// A short account of the file, its sync metadata and any stop.
pub(super) fn describe(path: &Path) -> Result<String, DriverError> {
    let size = |file: &Path| std::fs::metadata(file).map(|meta| meta.len()).ok();
    let mut lines = vec![match size(path) {
        Some(bytes) => format!("database: {} ({bytes} bytes)", path.display()),
        None => format!(
            "database: {} (missing; the next start restores it from the remote)",
            path.display()
        ),
    }];
    match sync_metadata(path) {
        Ok(info) => lines.push(format!(
            "synced: client {}, last change the remote confirmed {}",
            info["client_unique_id"].as_str().unwrap_or("?"),
            info["last_pushed_change_id_hint"].as_i64().unwrap_or(0)
        )),
        Err(_) => lines.push("synced: no sync metadata".to_string()),
    }
    if sidecar(path, RESTORING_SUFFIX).exists() {
        lines.push("restoring: a restore from the remote started and has not finished".to_string());
    }
    if let Some(opened_at) = unchecked_since(path) {
        lines.push(format!(
            "unchecked: since a start with changes up to {opened_at}"
        ));
    }
    lines.push(
        match std::fs::read_to_string(sidecar(path, DIVERGED_SUFFIX)) {
            Ok(cause) => format!("stopped: {}", cause.trim()),
            Err(_) => "stopped: no".to_string(),
        },
    );
    Ok(lines.join("\n"))
}

/// Refuse to open a local database that the sync engine would replace.
///
/// With no sync metadata next to it, the engine bootstraps the file from the
/// remote copy, which truncates whatever the file held. That is right for a
/// fresh machine (no file, or an empty one) and wrong for a database that was
/// written before sync was turned on: its rows were never sent anywhere.
pub(super) fn check_local_files(path: &Path) -> Result<(), DriverError> {
    if sidecar(path, "-info").exists() {
        return Ok(());
    }
    let holds_data = |file: &Path| std::fs::metadata(file).is_ok_and(|meta| meta.len() > 0);
    let wal = sidecar(path, "-wal");
    if holds_data(path) || holds_data(&wal) {
        return Err(DriverError::SyncSetup(format!(
            "{} holds data that was never copied to the sync database, and opening it with \
             sync would replace it with the remote copy. Copy the file to the remote database \
             first, or move it away to restore from the remote copy",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_local_file_written_before_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"SQLite format 3\0").expect("write db");

        let error = check_local_files(&path).expect_err("must refuse");
        assert!(error.to_string().contains("never copied"), "{error}");
    }

    #[test]
    fn refuses_a_wal_written_before_sync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"").expect("write db");
        std::fs::write(sidecar(&path, "-wal"), b"frames").expect("write wal");

        assert!(check_local_files(&path).is_err());
    }

    #[test]
    fn opens_a_missing_or_empty_file_and_a_synced_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        check_local_files(&path).expect("missing file restores from the remote");

        std::fs::write(&path, b"").expect("write db");
        check_local_files(&path).expect("empty file restores from the remote");

        std::fs::write(&path, b"SQLite format 3\0").expect("write db");
        std::fs::write(sidecar(&path, "-info"), b"{}").expect("write info");
        check_local_files(&path).expect("a synced file opens as it is");
    }

    #[test]
    fn a_restore_cut_short_is_moved_aside_and_run_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(&path, b"SQLite format 3\0part of the remote").expect("write db");
        std::fs::write(sidecar(&path, RESTORING_SUFFIX), b"").expect("write marker");

        check_local_files(&path).expect("a partial restore is restored again");
        assert!(!path.exists(), "the next restore starts from no file");
        let aside = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .find(|entry| entry.is_dir())
            .expect("the partial file is kept aside");
        assert_eq!(
            std::fs::read(aside.join("app.db")).expect("kept"),
            b"SQLite format 3\0part of the remote"
        );
        assert!(
            aside.join("app.db-sync-restoring").exists(),
            "the marker goes with the file it describes"
        );
    }

    #[test]
    fn a_restore_that_wrote_nothing_or_finished_stays_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        std::fs::write(sidecar(&path, RESTORING_SUFFIX), b"").expect("write marker");
        check_local_files(&path).expect("nothing was written, so nothing to keep");
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            1,
            "only the marker"
        );

        // The engine writes its metadata only after the last page is on disk.
        std::fs::write(&path, b"SQLite format 3\0").expect("write db");
        std::fs::write(sidecar(&path, "-info"), b"{}").expect("write info");
        check_local_files(&path).expect("a finished restore opens as it is");
        assert!(path.exists());
    }
}
