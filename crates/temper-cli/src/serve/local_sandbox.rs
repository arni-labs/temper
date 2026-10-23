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

use temper_server::state::ServerState;

/// Script that implements the sandbox HTTP API, relative to the checkout root.
const SANDBOX_SCRIPT: &str = "os-apps/temper-agent/sandbox/local_sandbox.py";

/// Environment variable through which the sandbox receives its token.
const SANDBOX_TOKEN_ENV: &str = "TEMPER_SANDBOX_TOKEN";

/// Start the local sandbox on `127.0.0.1:sandbox_port` and register it with
/// `server`, so WASM requests to `sandbox_url` carry its token.
pub(super) fn start(server: &ServerState, sandbox_port: u16, sandbox_url: &str) {
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
    let mut sandbox_cmd = std::process::Command::new("python3"); // determinism-ok: local dev sandbox
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
        .env(SANDBOX_TOKEN_ENV, &token)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let started = server
        .set_local_sandbox(sandbox_url, token)
        .and_then(|()| sandbox_cmd.spawn().map(drop).map_err(|e| e.to_string()));
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

/// A fresh 256-bit token, hex encoded.
fn new_token() -> String {
    // determinism-ok: OsRng used once at startup for the sandbox token
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
