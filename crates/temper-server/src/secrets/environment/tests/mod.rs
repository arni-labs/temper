use std::ffi::OsString;
use std::io;
use std::sync::{Arc, Mutex};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_wasm::WasmAuthzContext;

use super::*;
use crate::authz::CedarWasmAuthzGate;
use crate::registry::SpecRegistry;
use crate::state::ServerState;

const TENANT: &str = "tenant-a";
const MODULE: &str = "token-reader";
/// Lets the module read `build_token` and nothing else.
const POLICIES: &str = r#"
permit(principal == Agent::"token-reader", action == Action::"access_secret", resource == Secret::"build_token");
"#;
/// A value no name, reason or message contains, for the checks that nothing
/// reports a value.
const DISTINCT_VALUE: &str = "value-kept-out-of-reports";

fn test_vault() -> SecretsVault {
    SecretsVault::new(&[0x42; 32])
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

/// The lookup a module's `get_secret` call goes through: the tenant's Cedar
/// policies decide, then the vault answers.
fn module_reads(vault: SecretsVault, module: &str, secret: &str) -> Result<String, String> {
    let state =
        ServerState::from_registry(ActorSystem::new("env-secret-tests"), SpecRegistry::new())
            .with_secrets_vault(vault);
    state
        .authz
        .reload_tenant_policies(TENANT, POLICIES)
        .expect("policies load");
    let gate = Arc::new(CedarWasmAuthzGate::new(state.authz.clone()));
    let authz_ctx = WasmAuthzContext {
        tenant: TENANT.to_string(),
        module_name: module.to_string(),
        agent_id: None,
        session_id: None,
        entity_type: "Build".to_string(),
        trigger_action: "Start".to_string(),
    };
    let resolver = state
        .authorized_wasm_secret_resolver(&TenantId::new(TENANT), gate, authz_ctx)
        .expect("resolver exists when a vault is configured");
    resolver(secret)
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Seed `variables` and return the report with every log line and span the
/// seeding produced, at the most verbose level.
fn seed_with_log(
    vault: &SecretsVault,
    variables: Vec<(OsString, OsString)>,
) -> (EnvironmentSecretsReport, String) {
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let report = tracing::subscriber::with_default(subscriber, || {
        seed_platform_secrets_from_environment(vault, variables)
    });
    let log =
        String::from_utf8(buffer.0.lock().expect("log buffer lock").clone()).expect("log is UTF-8");
    (report, log)
}

mod authorization;
mod naming;
mod precedence;
mod reporting;
