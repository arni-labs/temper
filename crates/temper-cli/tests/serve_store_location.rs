//! Where `temper serve`, started as a real process in an empty home, keeps
//! its one local database: in `TURSO_URL` when that is set, otherwise in its
//! data folder (`TEMPER_DATA_DIR`, or `$HOME/.local/share/temper` without
//! one). A server given its own data folder must never open the database
//! every other server on the machine shares by default.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A started-and-stopped server's folders and what it printed.
struct Started {
    _dir: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    named: PathBuf,
    stdout: String,
}

impl Started {
    /// The database every server shares when given no folder of its own.
    fn shared_store(&self) -> PathBuf {
        self.home.join(".local/share/temper/agents.db")
    }

    fn data_store(&self) -> PathBuf {
        self.data.join("agents.db")
    }

    fn reported(&self, store: &Path) -> bool {
        self.stdout
            .contains(&format!("Storage: turso (file:{})", store.display()))
    }
}

/// Start `temper serve` in an empty home with `TEMPER_DATA_DIR` (`data`)
/// and `TURSO_URL` (`named`) set or not, wait until it listens, and stop it.
fn start(with_data_dir: bool, with_turso_url: bool) -> Started {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    let data = dir.path().join("data");
    let named = dir.path().join("named").join("store.db");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::create_dir_all(&cwd).expect("working directory");
    std::fs::create_dir_all(named.parent().expect("named parent")).expect("named directory");
    let stdout_path = dir.path().join("stdout.txt");
    let stderr_path = dir.path().join("stderr.txt");

    let mut command = Command::new(env!("CARGO_BIN_EXE_temper"));
    command
        .args(["serve", "--port", "0", "--no-observe"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("stderr file"),
        ));
    if with_data_dir {
        command.env("TEMPER_DATA_DIR", &data);
    }
    if with_turso_url {
        command.env("TURSO_URL", format!("file:{}", named.display()));
    }
    let mut process = Process(command.spawn().expect("start temper serve"));

    // A loaded workspace test run starts the server slowly; a server that
    // fails to start exits, and one that hangs runs into the deadline.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !read(&stdout_path)
        .lines()
        .any(|line| line.starts_with("Listening on "))
    {
        if let Some(status) = process.0.try_wait().expect("read child status") {
            panic!(
                "temper serve exited with {status} before listening:\n{}",
                read(&stderr_path)
            );
        }
        assert!(
            Instant::now() < deadline,
            "temper serve was not listening in time:\n{}",
            read(&stderr_path)
        );
        thread::sleep(Duration::from_millis(50));
    }
    drop(process);

    let stdout = read(&stdout_path);
    Started {
        _dir: dir,
        home,
        data,
        named,
        stdout,
    }
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
}

#[test]
fn a_data_dir_without_turso_url_keeps_the_store_in_the_data_dir() {
    let started = start(true, false);
    assert!(
        started.reported(&started.data_store()),
        "{}",
        started.stdout
    );
    assert!(started.data_store().is_file());
    assert!(
        !started.shared_store().exists(),
        "the server opened the shared store"
    );
}

#[test]
fn turso_url_alone_names_the_store() {
    let started = start(false, true);
    assert!(started.reported(&started.named), "{}", started.stdout);
    assert!(started.named.is_file());
    assert!(!started.shared_store().exists());
}

#[test]
fn turso_url_wins_over_the_data_dir() {
    let started = start(true, true);
    assert!(started.reported(&started.named), "{}", started.stdout);
    assert!(started.named.is_file());
    assert!(!started.data_store().exists());
    assert!(!started.shared_store().exists());
}

#[test]
fn neither_keeps_the_store_under_home() {
    let started = start(false, false);
    assert!(
        started.reported(&started.shared_store()),
        "{}",
        started.stdout
    );
    assert!(started.shared_store().is_file());
}
