//! `temper serve` started as a real process against Postgres: the start-up
//! line that names the database shows its connection URL with the password
//! masked, and the password appears nowhere in what the server writes.
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).expect("read output file")).into_owned()
}

/// Start `temper serve --storage postgres` with only `DATABASE_URL` set, wait
/// until it listens, stop it, and return everything it wrote.
async fn serve_until_listening(database_url: &str) -> String {
    let dir = tempfile::tempdir().expect("temporary directory");
    let stdout_path = dir.path().join("stdout.txt");
    let stderr_path = dir.path().join("stderr.txt");
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::create_dir_all(&cwd).expect("working directory");

    let mut command = Command::new(env!("CARGO_BIN_EXE_temper"));
    command
        .args([
            "serve",
            "--storage",
            "postgres",
            "--port",
            "0",
            "--no-observe",
        ])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .env("DATABASE_URL", database_url)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("stderr file"),
        ));
    let mut process = Process(command.spawn().expect("start temper serve"));

    // A loaded test run starts the server slowly; a server that fails to start
    // exits, and one that hangs runs into the deadline.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !read(&stdout_path)
        .lines()
        .any(|line| line.starts_with("Listening on "))
    {
        if let Some(status) = process.0.try_wait().expect("read child status") {
            panic!(
                "temper serve exited with {status} before listening:\n{}\n{}",
                read(&stdout_path),
                read(&stderr_path)
            );
        }
        assert!(
            Instant::now() < deadline,
            "temper serve was not listening in time:\n{}",
            read(&stderr_path)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(process);
    format!("{}\n{}", read(&stdout_path), read(&stderr_path))
}

#[tokio::test]
async fn the_database_password_never_appears_in_what_the_server_writes() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let (scheme, rest) = url.split_once("://").expect("DATABASE_URL has a scheme");
    let (credentials, host_and_database) = rest.split_once('@').expect("DATABASE_URL names a user");
    let (user, password) = credentials
        .split_once(':')
        .expect("DATABASE_URL carries a password");
    assert!(!password.is_empty(), "DATABASE_URL carries a password");
    let (host, _database) = host_and_database
        .split_once('/')
        .expect("DATABASE_URL names a database");

    // A database of its own, so the server's migrations and start-up rows
    // touch no other test's tables.
    let admin = sqlx::PgPool::connect(&url)
        .await
        .expect("connect to DATABASE_URL");
    let database = format!("serve_url_redaction_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .expect("create the test database");
    let database_url = format!("{scheme}://{user}:{password}@{host}/{database}");

    let output = serve_until_listening(&database_url).await;

    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
        .execute(&admin)
        .await;
    let masked = format!("Storage: postgres ({scheme}://{user}:***@{host}/{database})");
    assert!(
        output.contains(&masked),
        "start-up names the database with the password masked as {masked:?}:\n{output}"
    );
    assert!(
        !output.contains(&format!(":{password}@")),
        "the database password appears in what the server wrote:\n{output}"
    );
}
