//! The folder a server keeps what it writes to its own disk in.

use std::path::PathBuf;

/// `TEMPER_DATA_DIR`, or `$HOME/.local/share/temper` (under `/tmp` without a
/// HOME). The server's files (blobs, caches, the spec registry) live here,
/// and so does its local database unless `TURSO_URL` (one database) or
/// `TURSO_LOCAL_BASE_DIR` (routed tenants) names another place. A server
/// given its own folder therefore never opens the database every other
/// server on the machine shares by default.
pub(super) fn data_dir() -> PathBuf {
    match std::env::var_os("TEMPER_DATA_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/share/temper")
        }
    }
}
