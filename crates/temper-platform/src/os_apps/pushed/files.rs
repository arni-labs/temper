//! Where pushed app files live on disk, and how they get there.
//!
//! `<base>/<tenant>/<app>/` holds one directory per pushed version, named by
//! the digest of its files, each an apps directory holding `<app>/`. The
//! `current` file there names the version installed last.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use temper_server::state::ServerState;

use super::{PushRefused, PushedFile};

/// The file naming the version installed last.
const CURRENT: &str = "current";

/// Every pushed app's files on this server.
pub(super) fn pushed_base(server: &ServerState) -> PathBuf {
    if server.data_dir.as_os_str().is_empty() {
        std::env::temp_dir().join("temper-pushed-apps")
    } else {
        server.data_dir.join("pushed-apps")
    }
}

/// One tenant's pushed versions of one app.
///
/// The tenant and app names become directories, so each must be one plain
/// path component: anything else (`..`, a `/`) would write outside this
/// server's pushed-apps directory.
pub(super) fn app_home(server: &ServerState, tenant: &str, app: &str) -> Result<PathBuf, String> {
    for (label, value) in [("tenant", tenant), ("app name", app)] {
        let plain = !value.is_empty()
            && !matches!(value, "." | "..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !plain {
            return Err(format!(
                "the {label} '{value}' must be letters, digits, '.', '-' or '_' to push files to it"
            ));
        }
    }
    Ok(pushed_base(server).join(tenant).join(app))
}

/// Refuse a path that is not a plain relative path inside the app: absolute,
/// with an empty, `.` or `..` part, or with a NUL byte. A `.` part would let
/// two paths name one file, and `..` or an absolute path a file outside it.
pub(super) fn check_path(path: &str) -> Result<(), String> {
    let plain = !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."));
    if plain {
        Ok(())
    } else {
        Err(format!(
            "'{path}' is not a relative path inside the app: no leading '/', and no empty, '.' or '..' parts"
        ))
    }
}

/// The digest the version installed last is under, when its files are here.
pub(super) fn current(home: &Path) -> Option<String> {
    let digest = std::fs::read_to_string(home.join(CURRENT)).ok()?;
    let digest = digest.trim();
    let is_digest = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
    (is_digest && home.join(digest).is_dir()).then(|| digest.to_string())
}

/// Name `digest` as the version installed last, or name none. The new name is
/// written beside the old one and renamed over it, so a crash leaves one or
/// the other.
pub(super) fn set_current(home: &Path, digest: Option<&str>) -> std::io::Result<()> {
    let Some(digest) = digest else {
        return match std::fs::remove_file(home.join(CURRENT)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    };
    let mut file = tempfile::NamedTempFile::new_in(home)?;
    std::io::Write::write_all(&mut file, digest.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(home.join(CURRENT))
        .map_err(|error| error.error)?;
    Ok(())
}

/// Each file's bytes. A kept file is read from `kept_from`, the installed
/// version's app directory, and must hash to what the caller named.
pub(super) fn resolve(
    files: BTreeMap<String, PushedFile>,
    kept_from: Option<&Path>,
) -> Result<BTreeMap<String, Vec<u8>>, PushRefused> {
    let mut resolved = BTreeMap::new();
    for (path, file) in files {
        check_path(&path).map_err(PushRefused::Request)?;
        let bytes = match file {
            PushedFile::Text(text) => text.into_bytes(),
            PushedFile::Bytes(file) => base64::engine::general_purpose::STANDARD
                .decode(file.base64.as_bytes())
                .map_err(|error| {
                    PushRefused::Request(format!("'{path}' is not valid base64: {error}"))
                })?,
            PushedFile::Kept(file) => {
                let Some(dir) = kept_from else {
                    return Err(PushRefused::Request(format!(
                        "'{path}' is kept, but this app has no pushed version installed to keep it from"
                    )));
                };
                let bytes = std::fs::read(dir.join(&path)).map_err(|_| {
                    PushRefused::Request(format!(
                        "'{path}' is kept, but the installed version has no such file"
                    ))
                })?;
                if !sha256_hex(&bytes).eq_ignore_ascii_case(&file.keep) {
                    return Err(PushRefused::Request(format!(
                        "'{path}' is kept as {}, but the installed version's file hashes to {}",
                        file.keep,
                        sha256_hex(&bytes)
                    )));
                }
                bytes
            }
        };
        resolved.insert(path, bytes);
    }
    // A path that is also another's directory cannot be written.
    for path in resolved.keys() {
        let mut parent = path.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            if resolved.contains_key(dir) {
                return Err(PushRefused::Request(format!(
                    "'{dir}' is a file, so '{path}' cannot be inside it"
                )));
            }
            parent = dir;
        }
    }
    Ok(resolved)
}

/// The digest of every file's path and bytes, in path order. Each part is
/// length-prefixed so no two lists run together into the same bytes.
pub(super) fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Write `files` as `<home>/<digest>/<app>/`. They are written to a
/// directory beside it and renamed into place, so a directory with a digest's
/// name always holds all of that digest's files.
pub(super) fn materialize(
    home: &Path,
    digest: &str,
    app: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(home)?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(home)?;
    let app_dir = staging.path().join(app);
    for (path, bytes) in files {
        let file = app_dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, bytes)?;
    }
    let root = home.join(digest);
    std::fs::rename(staging.path(), &root)?;
    // Renamed into place: nothing is left for the staging guard to remove.
    let _ = staging.keep();
    Ok(root)
}

/// Delete every version in `home` but `keep`, and anything a crashed push
/// left. `forget` is told each version deleted, before it goes.
pub(super) fn prune(home: &Path, keep: &[&str], forget: impl Fn(&Path)) {
    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == CURRENT || keep.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            forget(&path);
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = removed {
            tracing::warn!(path = %path.display(), %error, "an old pushed app version could not be deleted");
        }
    }
}

/// Every `(tenant, app, root)` whose `current` names a version on disk. A
/// `current` naming a version that is gone is logged and left out.
pub(super) fn installed_roots(server: &ServerState) -> Vec<(String, String, PathBuf)> {
    let mut roots = Vec::new();
    for tenant in subdirectories(&pushed_base(server)) {
        for app in subdirectories(&tenant) {
            let (Some(tenant_name), Some(app_name)) = (file_name(&tenant), file_name(&app)) else {
                continue;
            };
            match current(&app) {
                Some(digest) => roots.push((tenant_name, app_name, app.join(digest))),
                None if app.join(CURRENT).exists() => tracing::error!(
                    tenant = %tenant_name,
                    app = %app_name,
                    "the pushed version this app installed last is missing; it is not restored"
                ),
                None => {}
            }
        }
    }
    roots
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_string)
}
