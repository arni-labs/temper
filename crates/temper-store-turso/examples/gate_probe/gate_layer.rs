//! The tracing layer that ties each write-gate acquire to its probe move.

use std::sync::Mutex;

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub(crate) const GATE_TARGET: &str = "temper_store_turso::store::write_gate";

/// One acquire of the gate's permit, as the gate logged it.
#[derive(Debug, Clone)]
pub(crate) struct Acquire {
    /// The probe move it was for; none for a write on a task of its own.
    pub(crate) mv: Option<u64>,
    pub(crate) operation: String,
    pub(crate) wait_ms: u64,
}

pub(crate) static ACQUIRES: Mutex<Vec<Acquire>> = Mutex::new(Vec::new());

/// The probe move a span belongs to.
pub(crate) struct MoveId(pub(crate) u64);

/// Records every gate acquire with the probe move whose span it ran in.
pub(crate) struct GateLayer;

#[derive(Default)]
struct Fields {
    pub(crate) mv: Option<u64>,
    pub(crate) operation: String,
    wait_ms: Option<u64>,
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "wait_ms" => self.wait_ms = Some(value),
            "mv" => self.mv = Some(value),
            _ => {}
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "operation" {
            self.operation = value.to_string();
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for GateLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != "probe_move" {
            return;
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let (Some(span), Some(mv)) = (ctx.span(id), fields.mv) {
            span.extensions_mut().insert(MoveId(mv));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != GATE_TARGET {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let Some(wait_ms) = fields.wait_ms else {
            return;
        };
        let mv = ctx.event_scope(event).and_then(|scope| {
            scope
                .from_root()
                .find_map(|span| span.extensions().get::<MoveId>().map(|m| m.0))
        });
        ACQUIRES.lock().unwrap().push(Acquire {
            mv,
            operation: fields.operation,
            wait_ms,
        });
    }
}
