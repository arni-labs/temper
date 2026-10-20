//! Moving a database that was never synced onto Turso Sync.
//!
//! The sync engine cannot take over an existing file: it would replace it
//! with the remote's copy, and its change log holds none of the file's rows.
//! So the file is set aside, a synced database is made fresh with today's
//! schema (primary keys included), and every row is copied into it in chunks,
//! each pushed to the remote before the next. The set-aside file stays as it
//! was, for a rollback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::TursoEventStore;
use crate::driver::{Value, params_from_iter};

/// Rows per chunk, and roughly the most bytes, so one push stays small.
const CHUNK_ROWS: usize = 500;
const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// How long each chunk's push may keep retrying.
const CHUNK_PUSH_BUDGET: Duration = Duration::from_secs(120);

/// With the app stopped: move the never-synced database at `path` onto the
/// empty remote at `sync_url`. Returns a line per table copied.
pub async fn adopt(
    path: &str,
    sync_url: &str,
    auth_token: &str,
) -> Result<String, PersistenceError> {
    let local = PathBuf::from(path.strip_prefix("file:").unwrap_or(path));
    let refuse = |why: String| Err(PersistenceError::Storage(why));
    if !local.exists() {
        return refuse(format!("{} does not exist", local.display()));
    }
    if sidecar(&local, "-info").exists() {
        return refuse(format!("{} is already synced", local.display()));
    }
    let aside = set_aside(&local)?;
    match copy_into_sync(&local, &aside, sync_url, auth_token).await {
        Ok(report) => Ok(format!("set aside: {}\n{report}", aside.display())),
        Err(error) => {
            // Leave the app as it was: its file back in place, no synced one.
            remove_database(&local);
            put_back(&local, &aside)?;
            Err(error)
        }
    }
}

async fn copy_into_sync(
    local: &Path,
    aside: &Path,
    sync_url: &str,
    auth_token: &str,
) -> Result<String, PersistenceError> {
    let refuse = |why: String| Err(PersistenceError::Storage(why));
    let old_path = aside.join(local.file_name().unwrap_or_default());
    let old = TursoEventStore::new(&format!("file:{}", old_path.display()), None).await?;
    let new = TursoEventStore::new_synced(&local.to_string_lossy(), sync_url, auth_token).await?;
    let copy = new
        .sync_copy()
        .cloned()
        .ok_or_else(|| PersistenceError::Storage("no sync copy".to_string()))?;
    if copy.state() != crate::driver::CopyState::Copying {
        return refuse(format!(
            "the remote copy cannot be used: {:?}",
            copy.state()
        ));
    }
    if new.count_rows("events").await? > 0 {
        return refuse(
            "the remote database already holds events; adopt needs an empty one".to_string(),
        );
    }

    let mut report = Vec::new();
    for table in old.user_tables().await? {
        let new_columns = new.table_columns(&table).await?;
        if new_columns.is_empty() {
            report.push(format!(
                "{table}: not in today's schema, left in the set-aside file"
            ));
            continue;
        }
        let columns: Vec<String> = old
            .table_columns(&table)
            .await?
            .into_iter()
            .filter(|column| new_columns.contains(column))
            .collect();
        let copied = copy_table(&old, &new, &table, &columns, &copy).await?;
        let (had, has) = (old.count_rows(&table).await?, new.count_rows(&table).await?);
        if has < had {
            return refuse(format!(
                "{table}: copied {copied} rows but the new database has {has} of {had}"
            ));
        }
        report.push(format!("{table}: {had} rows"));
    }
    copy.push_committed_within(CHUNK_PUSH_BUDGET).await?;
    Ok(report.join("\n"))
}

async fn copy_table(
    old: &TursoEventStore,
    new: &TursoEventStore,
    table: &str,
    columns: &[String],
    copy: &crate::driver::SyncCopy,
) -> Result<usize, PersistenceError> {
    let list = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    let marks = vec!["?"; columns.len()].join(", ");
    let insert = format!(
        "INSERT OR REPLACE INTO {} ({list}) VALUES ({marks})",
        quote(table)
    );
    let source = old.connection().await?;
    let mut rows = source
        .query(&format!("SELECT {list} FROM {}", quote(table)), ())
        .await
        .map_err(storage_error)?;
    let mut chunk: Vec<Vec<Value>> = Vec::new();
    let mut bytes = 0;
    let mut copied = 0;
    loop {
        let row = rows.next().await.map_err(storage_error)?;
        if let Some(row) = &row {
            let values = (0..columns.len())
                .map(|i| row.get_value(i).map_err(storage_error))
                .collect::<Result<Vec<_>, _>>()?;
            bytes += values.iter().map(value_bytes).sum::<usize>();
            chunk.push(values);
        }
        if chunk.len() >= CHUNK_ROWS || bytes >= CHUNK_BYTES || (row.is_none() && !chunk.is_empty())
        {
            let target = new.connection().await?;
            let tx = target.begin_immediate().await.map_err(storage_error)?;
            for values in chunk.drain(..) {
                tx.execute(&insert, params_from_iter(values))
                    .await
                    .map_err(storage_error)?;
                copied += 1;
            }
            tx.commit().await.map_err(storage_error)?;
            copy.push_committed_within(CHUNK_PUSH_BUDGET).await?;
            bytes = 0;
        }
        if row.is_none() {
            return Ok(copied);
        }
    }
}

impl TursoEventStore {
    async fn user_tables(&self) -> Result<Vec<String>, PersistenceError> {
        let conn = self.connection().await?;
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT GLOB 'sqlite_*' \
                 AND name NOT GLOB 'turso_*' AND name NOT GLOB '__turso_*' ORDER BY name",
                (),
            )
            .await
            .map_err(storage_error)?;
        let mut tables = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            tables.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(tables)
    }

    async fn table_columns(&self, table: &str) -> Result<Vec<String>, PersistenceError> {
        let conn = self.connection().await?;
        let mut rows = conn
            .query(&format!("PRAGMA table_info({})", quote(table)), ())
            .await
            .map_err(storage_error)?;
        let mut columns = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            columns.push(row.get::<String>(1).map_err(storage_error)?);
        }
        Ok(columns)
    }

    async fn count_rows(&self, table: &str) -> Result<i64, PersistenceError> {
        let conn = self.connection().await?;
        let mut rows = conn
            .query(&format!("SELECT COUNT(*) FROM {}", quote(table)), ())
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error),
            None => Ok(0),
        }
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Text(text) => text.len(),
        Value::Blob(blob) => blob.len(),
        _ => 8,
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Delete the database at `path` and every file beside it named after it.
fn remove_database(path: &Path) {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let Some(dir) = path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if (file == name || file.starts_with(&format!("{name}-"))) && entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Undo [`set_aside`].
fn put_back(path: &Path, aside: &Path) -> Result<(), PersistenceError> {
    let failed = |error: std::io::Error| {
        PersistenceError::Storage(format!("put back {}: {error}", path.display()))
    };
    for entry in std::fs::read_dir(aside).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        std::fs::rename(entry.path(), path.with_file_name(entry.file_name())).map_err(failed)?;
    }
    std::fs::remove_dir(aside).map_err(failed)
}

/// Move the database and its WAL and shared-memory files into
/// `<name>.before-sync-<unix seconds>/`.
fn set_aside(path: &Path) -> Result<PathBuf, PersistenceError> {
    let failed = |error: std::io::Error| {
        PersistenceError::Storage(format!("set aside {}: {error}", path.display()))
    };
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let stamp = std::time::SystemTime::now() // determinism-ok: names a folder made once
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let aside = path.with_file_name(format!("{name}.before-sync-{stamp}"));
    std::fs::create_dir_all(&aside).map_err(failed)?;
    for suffix in ["", "-wal", "-shm"] {
        let file = sidecar(path, suffix);
        if file.exists() {
            std::fs::rename(&file, aside.join(format!("{name}{suffix}"))).map_err(failed)?;
        }
    }
    Ok(aside)
}
