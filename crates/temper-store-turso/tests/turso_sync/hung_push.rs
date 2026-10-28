//! A push that hangs leaves enough in the logs and traces to tell where it
//! waited: the engine's own steps inside the push's span, and a fresh request
//! to the remote timed step by step beside it.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use temper_store_turso::TursoEventStore;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use crate::support::*;

/// One logged line: the span it was logged in, its message and its fields.
#[derive(Clone, Debug)]
struct Line {
    span: Option<String>,
    target: String,
    message: String,
    fields: BTreeMap<String, String>,
}

/// Every line logged on this thread while it listens.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<Line>>>);

impl Lines {
    fn find(&self, matches: impl Fn(&Line) -> bool) -> Option<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|line| matches(line))
            .cloned()
    }
}

impl<S> tracing_subscriber::Layer<S> for Lines
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let message = fields.0.remove("message").unwrap_or_default();
        self.0.lock().unwrap().push(Line {
            span: ctx.event_span(event).map(|span| span.name().to_string()),
            target: event.metadata().target().to_string(),
            message,
            fields: fields.0,
        });
    }
}

#[tokio::test]
async fn a_hung_push_shows_where_it_waited() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, open) = valve(&server.url).await;
    let (_dir, path) = machine();
    let store = TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open through the valve");
    let lines = Lines::default();
    // This thread's own listener: the test runtime runs every task here.
    let _listening = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(lines.clone())
            .with(tracing_subscriber::filter::LevelFilter::INFO),
    );

    open.store(false, Ordering::Release);
    write(&store, "s1", 0).await;
    store
        .sync_copy()
        .expect("copy")
        .push_committed()
        .await
        .expect_err("a push nobody answers is not a success");

    // The engine's own steps are in the push's span, which traces ship.
    let engine = lines
        .find(|line| {
            line.target.starts_with("turso_sync_engine") && line.message.starts_with("push_changes")
        })
        .expect("the engine logged its push");
    assert_eq!(
        engine.span.as_deref(),
        Some("turso.sync.push"),
        "{engine:?}"
    );

    // Beside the hung push, a fresh request found the remote's name and
    // address at once, and the remote never answered it.
    let started = Instant::now();
    let fresh = loop {
        if let Some(line) = lines.find(|line| line.message.starts_with("turso.sync push slow")) {
            break line;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "a slow push timed no fresh request"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(fresh.fields.contains_key("lookup_ms"), "{fresh:?}");
    assert!(fresh.fields.contains_key("connect_ms"), "{fresh:?}");
    assert!(!fresh.fields.contains_key("answer_ms"), "{fresh:?}");
    let failed = fresh
        .fields
        .get("failed")
        .map(String::as_str)
        .unwrap_or_default();
    assert!(failed.starts_with("answer: nothing within"), "{fresh:?}");
}
