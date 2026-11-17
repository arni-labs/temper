//! An install runs to its end on the server, whatever its caller does.
//!
//! A request starts the install and waits for it up to [`ANSWER_WITHIN`]. An
//! install that ends by then answers with what it did. One that does not
//! answers `202` with its id and goes on; the installs list
//! (`GET /api/genesis/apps/installs`) says how it went, each app carrying the
//! last install this process ran for it: `running`, `installed`, or `failed`
//! with the reason. The runs are kept in memory: an install a restart cut off
//! is gone from the list, and the app's install record says which version
//! runs. Installing a ref again is safe, and asking for the ref an install is
//! still running answers that install.

use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;

use super::*;

/// How long a request waits for its install before answering `202`.
///
/// Not a limit: the install goes on either way. It keeps a caller's request
/// well under the 30 s that common HTTP routers and serverless callers allow a
/// quiet request before they cut it, so the caller hears back instead of
/// losing the answer.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(25);

/// One install of an app, as the installs list shows it.
#[derive(Debug, Clone, Serialize)]
pub struct InstallRun {
    /// The install's id, as its `202` named it.
    pub id: String,
    /// The version it installs, `owner/app@hash`.
    pub app_ref: String,
    #[serde(flatten)]
    pub outcome: RunOutcome,
}

/// Where an install is.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    Running,
    Installed,
    Failed { reason: String },
}

/// The last install of each app in this process, by tenant and app name.
#[derive(Default)]
pub struct InstallRuns {
    last: Mutex<BTreeMap<(String, String), InstallRun>>,
}

impl InstallRuns {
    /// The runs; a panic while one was held left them whole, so they are read on.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), InstallRun>> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every app of `tenant` with its last install.
    pub fn of_tenant(&self, tenant: &str) -> Vec<(String, InstallRun)> {
        let last = self.lock();
        last.iter()
            .filter(|((run_tenant, _), _)| run_tenant == tenant)
            .map(|((_, app), run)| (app.clone(), run.clone()))
            .collect()
    }

    /// The install `ref_` of `app` still running, or a new one, marked running.
    fn start(&self, tenant: &str, app: &str, app_ref: &str) -> (InstallRun, bool) {
        let mut last = self.lock();
        let key = (tenant.to_string(), app.to_string());
        if let Some(run) = last.get(&key)
            && matches!(run.outcome, RunOutcome::Running)
            && run.app_ref == app_ref
        {
            return (run.clone(), false);
        }
        let run = InstallRun {
            id: format!("install-{}", temper_runtime::scheduler::sim_uuid()),
            app_ref: app_ref.to_string(),
            outcome: RunOutcome::Running,
        };
        last.insert(key, run.clone());
        (run, true)
    }

    /// Record how install `id` ended, unless a later install of the app took its place.
    fn finish(&self, tenant: &str, app: &str, id: &str, outcome: RunOutcome) {
        let mut last = self.lock();
        if let Some(run) = last.get_mut(&(tenant.to_string(), app.to_string()))
            && run.id == id
        {
            run.outcome = outcome;
        }
    }
}

/// What a request to install answers.
pub enum InstallAnswer {
    /// The install ended and the version runs.
    Installed {
        id: String,
        result: Box<GenesisRegistryInstallResult>,
    },
    /// The install goes on; the installs list says how it ends.
    Running(InstallRun),
    /// Nothing was installed, and why. `id` is `None` when the request was
    /// refused before an install started.
    Failed { id: Option<String>, error: String },
}

impl InstallAnswer {
    /// The HTTP answer: `200` with the install, `202` while it runs, or the
    /// failure's status with its reason.
    pub fn response(self) -> (StatusCode, Value) {
        match self {
            Self::Installed { id, result } => {
                let mut body = serde_json::to_value(*result).unwrap_or_else(|_| Value::Null);
                if let Some(body) = body.as_object_mut() {
                    body.insert("install_id".to_string(), Value::String(id));
                    body.insert("status".to_string(), "installed".into());
                }
                (StatusCode::OK, body)
            }
            Self::Running(run) => {
                let body = serde_json::json!({
                    "install_id": run.id,
                    "app_ref": run.app_ref,
                    "status": "running",
                });
                (StatusCode::ACCEPTED, body)
            }
            Self::Failed { id, error } => {
                let status = if id.is_none() {
                    StatusCode::BAD_REQUEST
                } else if error.contains("refused: spec '") && error.contains("does not parse:") {
                    // A spec of the closure did not parse: nothing changed.
                    // Matched first, since its words may say "not found".
                    StatusCode::UNPROCESSABLE_ENTITY
                } else if error.contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                let mut body = serde_json::json!({ "status": "failed", "error": error });
                if let Some(id) = id {
                    body["install_id"] = Value::String(id);
                }
                (status, body)
            }
        }
    }
}

/// Install `request` on the server, answering within [`ANSWER_WITHIN`].
pub async fn install_answering(
    platform: &PlatformState,
    request: GenesisRegistryInstallRequest,
) -> InstallAnswer {
    let app = match parse_registry_app_ref(&request.app_ref) {
        Ok(app) if app.version_hash.is_some() => app.name,
        Ok(_) => {
            return InstallAnswer::Failed {
                id: None,
                error: "Genesis app install requires a pinned owner/app@hash ref".to_string(),
            };
        }
        Err(error) => return InstallAnswer::Failed { id: None, error },
    };
    let tenant = request.tenant.clone();
    let (run, started) = platform
        .installs
        .start(&tenant, &app, request.app_ref.trim());
    if !started {
        return InstallAnswer::Running(run);
    }
    let (sender, ended) = tokio::sync::oneshot::channel();
    let install = tokio::spawn({
        let platform = platform.clone();
        async move { install_genesis_app_from_registry(&platform, request).await }
    });
    tokio::spawn({
        let platform = platform.clone();
        let id = run.id.clone();
        async move {
            let result = install
                .await
                .unwrap_or_else(|error| Err(format!("the install stopped: {error}")));
            let outcome = match &result {
                Ok(_) => RunOutcome::Installed,
                Err(reason) => RunOutcome::Failed {
                    reason: reason.clone(),
                },
            };
            platform.installs.finish(&tenant, &app, &id, outcome);
            let _ = sender.send(result);
        }
    });
    match tokio::time::timeout(ANSWER_WITHIN, ended).await {
        Ok(Ok(Ok(result))) => InstallAnswer::Installed {
            id: run.id,
            result: Box::new(result),
        },
        Ok(Ok(Err(error))) => InstallAnswer::Failed {
            id: Some(run.id),
            error,
        },
        Ok(Err(_)) => InstallAnswer::Failed {
            id: Some(run.id),
            error: "the install stopped before it answered".to_string(),
        },
        Err(_) => InstallAnswer::Running(run),
    }
}
