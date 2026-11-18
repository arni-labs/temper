//! The kernel's meaning model (ADR-0186): multilingual-e5-small, run in this
//! process. Loaded off the boot path; until it is, search ranks by words.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use temper_meaning::{Embedder, Kind};
use temper_server::meaning::{MeaningKind, MeaningModel, worker::run_meaning_worker};

use crate::serve::PlatformState;

/// The embedder, called on a blocking thread so a model call never stalls the
/// async runtime.
struct E5 {
    embedder: Arc<Embedder>,
}

impl MeaningModel for E5 {
    fn tag(&self) -> &str {
        temper_meaning::MODEL_TAG
    }

    fn embed<'a>(
        &'a self,
        texts: &'a [String],
        kind: MeaningKind,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, String>> + Send + 'a>> {
        let embedder = self.embedder.clone();
        let texts = texts.to_vec();
        Box::pin(async move {
            let started = std::time::Instant::now(); // determinism-ok: production-only model timing
            let count = texts.len();
            let vectors = tokio::task::spawn_blocking(move || {
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                let kind = match kind {
                    MeaningKind::Passage => Kind::Passage,
                    MeaningKind::Query => Kind::Query,
                };
                embedder.embed_many(&texts, kind)
            })
            .await
            .map_err(|error| format!("the model's thread failed: {error}"))??;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match kind {
                MeaningKind::Passage => tracing::info!(
                    records = count,
                    ms = ms.round(),
                    ms_per_record = (ms / count.max(1) as f64).round(),
                    "meaning: embedded records"
                ),
                MeaningKind::Query => tracing::debug!(ms = ms.round(), "meaning: embedded search"),
            }
            Ok(vectors)
        })
    }
}

/// Load the model in the background, install it, and keep records' meaning up to
/// date for the life of the server.
pub(super) fn spawn_meaning(state: &PlatformState) {
    if state.server.storage_stack.is_none() {
        return;
    }
    let server = state.server.clone();
    tokio::spawn(async move {
        // determinism-ok: production-only background model
        let started = std::time::Instant::now(); // determinism-ok: production-only model timing
        let rss_before_mb = resident_mb();
        let embedder = match tokio::task::spawn_blocking(Embedder::load).await {
            Ok(Ok(embedder)) => embedder,
            Ok(Err(error)) => {
                tracing::error!(%error, "meaning: the model did not load; search ranks by words");
                return;
            }
            Err(error) => {
                tracing::error!(%error, "meaning: the model's loader failed; search ranks by words");
                return;
            }
        };
        tracing::info!(
            model = temper_meaning::MODEL_TAG,
            load_ms = started.elapsed().as_millis() as u64,
            rss_before_mb,
            rss_after_mb = resident_mb(),
            "meaning: model loaded"
        );
        server.install_meaning_model(Arc::new(E5 {
            embedder: Arc::new(embedder),
        }));
        run_meaning_worker(server).await;
    });
}

/// This process's resident memory in MB, where the platform says (Linux); -1 elsewhere.
fn resident_mb() -> i64 {
    std::fs::read_to_string("/proc/self/statm") // determinism-ok: production-only memory reading
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<i64>().ok())
        .map(|pages| pages * 4096 / 1_000_000)
        .unwrap_or(-1)
}
