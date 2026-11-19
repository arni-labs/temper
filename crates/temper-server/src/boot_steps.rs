//! The boot step a starting server is in, for `/healthz` to name.
//!
//! Boot work runs in spans whose `otel.name` is `boot.<step>`, and a step that
//! works on one tenant carries it as `tenant`. [`BootSteps`] is a tracing layer
//! that keeps the boot spans open now, so a start says where it is however long
//! a step takes: one stuck in a step names that step and how long it has been
//! in it, and nothing needs a deadline to see it. It sees the spans its
//! subscriber lets through; with boot spans filtered out it names no step.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, Layer};

/// The boot spans open now, oldest first. Cloned handles share them: the
/// embedder puts one on its subscriber and gives one to the server.
#[derive(Clone, Default)]
pub struct BootSteps(Arc<Mutex<Vec<OpenStep>>>);

struct OpenStep {
    id: Id,
    step: String,
    tenant: Option<String>,
    since: DateTime<Utc>,
}

/// A boot step open now, as `/healthz` and the boot's report say it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BootStep {
    /// The span's name without `boot.`, as `projection_compare`.
    pub step: String,
    /// The tenant the step works on, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// How long the step has been open.
    pub since_secs: u64,
}

impl BootSteps {
    /// The innermost boot step open now, or None when no boot span is open.
    pub fn current(&self) -> Option<BootStep> {
        let open = self.0.lock().expect("boot steps lock poisoned");
        let step = open.last()?;
        Some(BootStep {
            step: step.step.clone(),
            tenant: step.tenant.clone(),
            since_secs: u64::try_from(
                (temper_runtime::scheduler::sim_now() - step.since).num_seconds(),
            )
            .unwrap_or(0),
        })
    }
}

impl<S: Subscriber> Layer<S> for BootSteps {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut fields = StepFields::default();
        attrs.record(&mut fields);
        let Some(step) = fields
            .name
            .as_deref()
            .and_then(|name| name.strip_prefix("boot."))
        else {
            return;
        };
        let opened = OpenStep {
            id: id.clone(),
            step: step.to_string(),
            tenant: fields.tenant,
            since: temper_runtime::scheduler::sim_now(),
        };
        self.0
            .lock()
            .expect("boot steps lock poisoned")
            .push(opened);
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        self.0
            .lock()
            .expect("boot steps lock poisoned")
            .retain(|open| open.id != id);
    }
}

/// The two fields of a span that name a boot step.
#[derive(Default)]
struct StepFields {
    name: Option<String>,
    tenant: Option<String>,
}

impl StepFields {
    fn keep(&mut self, field: &Field, value: String) {
        match field.name() {
            "otel.name" => self.name = Some(value),
            "tenant" => self.tenant = Some(value),
            _ => {}
        }
    }
}

impl Visit for StepFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.keep(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.keep(field, format!("{value:?}"));
    }
}

#[cfg(test)]
#[path = "boot_steps_test.rs"]
mod tests;
