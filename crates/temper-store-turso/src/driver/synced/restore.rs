//! How long opening the engine may take. A restore downloads the whole
//! database and the engine sets no timeout of its own.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

/// How long opening may go without writing to the database's files.
pub(super) const RESTORE_STALL_BUDGET: Duration = Duration::from_secs(300);

/// How often a running open looks at the files.
pub(super) const PROGRESS_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Run `opening` to the end, or `None` once it has run out of time.
pub(super) async fn while_progressing<T, S: PartialEq>(
    opening: impl Future<Output = T>,
    _footprint: impl FnMut() -> S,
) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(120), opening)
        .await
        .ok()
}

/// The length of the database and of each file the engine keeps beside it.
pub(super) fn footprint(path: &Path) -> BTreeMap<String, u64> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            let ours = file == name || file.starts_with(&format!("{name}-"));
            let meta = entry.metadata().ok().filter(|_| ours)?;
            Some((file, meta.len()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Instant, sleep};

    #[tokio::test(start_paused = true)]
    async fn an_open_that_keeps_writing_runs_until_it_finishes() {
        let start = Instant::now();
        let opened = while_progressing(
            async {
                sleep(Duration::from_secs(1800)).await;
                "opened"
            },
            || start.elapsed().as_secs() / 30,
        )
        .await;
        assert_eq!(opened, Some("opened"), "30 minutes of steady writing");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_shorter_than_the_budget_is_not_a_stall() {
        let start = Instant::now();
        let quiet = RESTORE_STALL_BUDGET - PROGRESS_CHECK_EVERY;
        let opened = while_progressing(
            async {
                sleep(RESTORE_STALL_BUDGET * 3).await;
                "opened"
            },
            || match start.elapsed() {
                elapsed if elapsed < quiet => 0,
                elapsed => elapsed.as_secs(),
            },
        )
        .await;
        assert_eq!(opened, Some("opened"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_that_stops_writing_fails_once_the_budget_passes() {
        let start = Instant::now();
        let wrote_until = Duration::from_secs(600);
        let opened = while_progressing(std::future::pending::<()>(), || {
            start.elapsed().min(wrote_until).as_secs()
        })
        .await;
        let waited = start.elapsed();
        assert_eq!(opened, None, "a stalled open fails");
        assert!(
            waited >= wrote_until + RESTORE_STALL_BUDGET
                && waited <= wrote_until + RESTORE_STALL_BUDGET + PROGRESS_CHECK_EVERY,
            "fails one budget after the last write, not before: {waited:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_page_written_into_a_file_already_at_full_size_counts() {
        use std::os::unix::fs::FileExt;
        // The engine sizes the file to the whole database before the first
        // page arrives, so the length stays put while the download runs.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(64 * 1024 * 1024).expect("size");
        let before = footprint(&path);
        file.write_at(&[7; 4096], 32 * 1024 * 1024).expect("page");
        file.sync_all().expect("sync");
        assert_ne!(footprint(&path), before);
    }
}
