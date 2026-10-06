//! `temper serve` started as a real process, with and without
//! `TEMPER_SECRET_<NAME>` variables: what start-up reports, and that it starts.
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SEEDED_MESSAGE: &str = "seeded platform secrets from TEMPER_SECRET_ environment variables";
const SKIPPED_MESSAGE: &str = "environment variable was not seeded as a secret";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Everything the server wrote until it was listening.
struct StartupOutput {
    stdout: String,
    stderr: String,
}

impl StartupOutput {
    /// The structured log events with the given message.
    fn events(&self, message: &str) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| event["fields"]["message"] == message)
            .collect()
    }

    fn contains(&self, text: &str) -> bool {
        self.stdout.contains(text) || self.stderr.contains(text)
    }
}

/// Start `temper serve` with only the given variables, in an empty home and
/// working directory, wait until it listens, and stop it.
fn start_and_stop(variables: &[(&str, &str)]) -> StartupOutput {
    let dir = tempfile::tempdir().expect("temporary directory");
    let stdout_path = dir.path().join("stdout.txt");
    let stderr_path = dir.path().join("stderr.txt");
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::create_dir_all(&cwd).expect("working directory");

    let mut command = Command::new(env!("CARGO_BIN_EXE_temper"));
    command
        .args(["serve", "--port", "0", "--no-observe"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .envs(variables.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("stderr file"),
        ));
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

    StartupOutput {
        stdout: read(&stdout_path),
        stderr: read(&stderr_path),
    }
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).expect("read output file")).into_owned()
}

#[test]
fn prefixed_variables_are_reported_by_count_and_bad_names_once_and_the_server_starts() {
    let values = [
        "seeded-value-one",
        "seeded-value-two",
        "bad-name-value",
        "fixed-variable-value",
        "prefixed-form-value",
    ];
    let bad_names = [
        "TEMPER_SECRET_",
        "TEMPER_SECRET_BUILD-TOKEN",
        "TEMPER_SECRET_build_token",
    ];

    let output = start_and_stop(&[
        ("TEMPER_SECRET_BUILD_TOKEN", values[0]),
        ("TEMPER_SECRET_REGION", values[1]),
        ("TEMPER_SECRET_EMPTY", ""),
        (bad_names[0], values[2]),
        (bad_names[1], values[2]),
        (bad_names[2], values[2]),
        ("ANTHROPIC_API_KEY", values[3]),
        ("TEMPER_SECRET_ANTHROPIC_API_KEY", values[4]),
    ]);

    let seeded = output.events(SEEDED_MESSAGE);
    assert_eq!(seeded.len(), 1, "{seeded:?}");
    assert_eq!(seeded[0]["level"], "INFO");
    assert_eq!(seeded[0]["fields"]["count"], 2);

    let skipped = output.events(SKIPPED_MESSAGE);
    let reported: Vec<&str> = skipped
        .iter()
        .map(|event| event["fields"]["variable"].as_str().expect("variable name"))
        .collect();
    // The fixed variable was seeded first, so its prefixed form is the one skipped.
    assert_eq!(
        reported,
        [
            bad_names[0],
            "TEMPER_SECRET_ANTHROPIC_API_KEY",
            bad_names[1],
            bad_names[2],
        ]
    );
    assert!(skipped.iter().all(|event| event["level"] == "WARN"));
    assert_eq!(
        skipped[1]["fields"]["reason"],
        "the server already sets a secret of that name"
    );

    for value in values {
        assert!(!output.contains(value), "start-up output contains {value}");
    }
}

#[test]
fn start_up_without_a_prefixed_variable_reports_nothing_about_them() {
    let output = start_and_stop(&[("ANTHROPIC_API_KEY", "fixed-variable-value")]);

    assert!(output.events(SEEDED_MESSAGE).is_empty());
    assert!(output.events(SKIPPED_MESSAGE).is_empty());
    assert!(!output.contains("TEMPER_SECRET_"));
    assert!(!output.contains("fixed-variable-value"));
}
