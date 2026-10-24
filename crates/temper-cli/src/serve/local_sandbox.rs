//! The local development sandbox `temper serve` starts when no `SANDBOX_URL`
//! is configured.
//!
//! The sandbox runs commands and reads and writes files on this machine, so
//! only this server may use it: it listens on 127.0.0.1 alone and answers only
//! requests that carry the token generated here at each start. The token goes
//! to the sandbox in its environment (not its argv, which other local users
//! can list) and to the WASM host through the server state. Guests never see
//! it.

use std::path::Path;
use std::time::Duration;

use temper_server::state::ServerState;

/// Script that implements the sandbox HTTP API, relative to the checkout root.
const SANDBOX_SCRIPT: &str = "os-apps/temper-agent/sandbox/local_sandbox.py";

/// Environment variable through which the sandbox receives its token.
const SANDBOX_TOKEN_ENV: &str = "TEMPER_SANDBOX_TOKEN";

/// Start the local sandbox on `127.0.0.1:sandbox_port` and register it with
/// `server`, so WASM requests to `sandbox_url` carry its token.
///
/// The sandbox lives exactly as long as this process: it reads a pipe this
/// process holds open and exits at its end of input, so it stops however
/// the server stops, and the next start finds the port free. Success is
/// reported only once the sandbox answers `/health` with this start's token.
pub(super) async fn start(server: &ServerState, sandbox_port: u16, sandbox_url: &str) {
    let sandbox_script = Path::new(SANDBOX_SCRIPT);
    if !sandbox_script.exists() {
        eprintln!("  Warning: local sandbox script not found at {sandbox_script:?}");
        eprintln!("  Set SANDBOX_URL env var or ensure {SANDBOX_SCRIPT} exists");
        return;
    }

    // Use /tmp/temper-sandbox as the base; create /workspace for tool_runner
    // which sends cwd="/workspace" by default (matching E2B's layout).
    let _ = std::fs::create_dir_all("/tmp/temper-sandbox");
    let _ = std::fs::create_dir_all("/workspace");

    let token = new_token();
    let mut sandbox_cmd = tokio::process::Command::new("python3"); // determinism-ok: local dev sandbox
    sandbox_cmd
        .arg(sandbox_script)
        .arg("--port")
        .arg(sandbox_port.to_string())
        .arg("--workdir")
        .arg("/tmp/temper-sandbox")
        // Named explicitly so a script that cannot check the token refuses
        // the unknown flag and does not start.
        .arg("--token-env")
        .arg(SANDBOX_TOKEN_ENV)
        .arg("--exit-with-stdin")
        .env(SANDBOX_TOKEN_ENV, &token)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let started = match server.set_local_sandbox(sandbox_url, token.clone()) {
        Ok(()) => match sandbox_cmd.spawn() {
            Ok(child) => wait_until_ready(child, sandbox_url, &token).await,
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e),
    };
    match started {
        Ok(()) => println!("  Local sandbox: {sandbox_url} (auto-started)"),
        Err(e) => {
            eprintln!("  Warning: failed to start local sandbox: {e}");
            eprintln!(
                "  Run manually: {SANDBOX_TOKEN_ENV}=<token> python3 {SANDBOX_SCRIPT} --port {sandbox_port}"
            );
        }
    }
}

/// How long a starting sandbox has to answer `/health`: python3 starts the
/// script in well under a second, so this only bounds a sandbox that hangs.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait until the sandbox answers `/health` with `token`, then keep it
/// running for the life of this process. An error if it exits first (its
/// port is taken, for example) or does not answer in time.
async fn wait_until_ready(
    mut child: tokio::process::Child,
    sandbox_url: &str,
    token: &str,
) -> Result<(), String> {
    // Straight to loopback: a configured HTTP proxy must not see the token.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let health = format!("{sandbox_url}/health");
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT; // determinism-ok: local dev sandbox start
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "the sandbox exited with {status}; is {sandbox_url} already in use?"
            ));
        }
        let answered = client.get(&health).bearer_auth(token).send().await;
        if answered.is_ok_and(|response| response.status().is_success()) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the sandbox did not answer {health} in time"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await; // determinism-ok: local dev sandbox start
    }
    // `wait` closes the child's stdin, which would stop the sandbox, so the
    // pipe is held here until the sandbox exits or this process does.
    let stdin = child.stdin.take();
    tokio::spawn(async move {
        let _stdin = stdin;
        if let Ok(status) = child.wait().await {
            tracing::warn!(%status, "local sandbox exited");
        }
    });
    Ok(())
}

/// A fresh 256-bit token, hex encoded.
fn new_token() -> String {
    // determinism-ok: OsRng used once at startup for the sandbox token
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
