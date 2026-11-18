//! Making the vectors records wait for, just after their saves.
//!
//! A save writes a record's text in its commit and leaves the vector empty when the
//! text changed. This worker finds those rows, asks the model for their vectors, and
//! keeps each one only if its row still holds the text it was made from. A record
//! that changes again meanwhile waits for its newer text; a crash loses nothing,
//! since a waiting row stays waiting.

use temper_runtime::persistence::meaning_input;

use super::MeaningModel;
use crate::state::ServerState;

/// How many records one model call embeds. A batch of this many 256-piece texts
/// keeps the model's working memory near 60 MB and holds its thread for seconds,
/// not minutes, on one slow core.
pub const BATCH: usize = 16;

/// Make vectors for up to [`BATCH`] of `tenant`'s waiting records. Returns how many
/// rows it settled; zero means none waits.
pub async fn fill_meaning_batch(
    state: &ServerState,
    model: &dyn MeaningModel,
    tenant: &str,
) -> Result<usize, String> {
    let Some((store, _)) = state.event_journal() else {
        return Ok(0);
    };
    let tag = model.tag();
    let pending = store
        .meaning_pending(tenant, tag, BATCH)
        .await
        .map_err(|error| error.to_string())?;
    if pending.is_empty() {
        return Ok(0);
    }
    let inputs: Vec<String> = pending.iter().map(|row| meaning_input(&row.text)).collect();
    // A record whose text holds no words the model reads has no meaning: it keeps an
    // empty vector by this model, so it no longer waits and ranks only by words.
    let texts: Vec<String> = inputs.iter().filter(|t| !t.is_empty()).cloned().collect();
    let mut vectors = if texts.is_empty() {
        Vec::new()
    } else {
        model.embed(&texts, super::MeaningKind::Passage).await?
    }
    .into_iter();
    if vectors.len() != texts.len() {
        return Err(format!(
            "the model returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        ));
    }
    for (row, input) in pending.iter().zip(&inputs) {
        let vector = if input.is_empty() {
            Vec::new()
        } else {
            vectors.next().unwrap_or_default()
        };
        store
            .set_meaning_vector(
                tenant,
                &row.entity_type,
                &row.entity_id,
                &row.text_hash,
                tag,
                &vector,
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(pending.len())
}

/// Make vectors for every waiting record of every tenant. Returns how many rows it
/// settled.
pub async fn fill_meaning(state: &ServerState, model: &dyn MeaningModel) -> Result<usize, String> {
    let tenants: Vec<String> = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry
            .tenant_ids()
            .into_iter()
            .map(|tenant| tenant.as_str().to_string())
            .collect()
    };
    let mut settled = 0;
    for tenant in &tenants {
        loop {
            let done = fill_meaning_batch(state, model, tenant).await?;
            settled += done;
            if done == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    }
    Ok(settled)
}

/// Run for the life of the server: fill every waiting record, then wait until a
/// save or a backfill says a text may wait again.
pub async fn run_meaning_worker(state: ServerState) {
    let Some(model) = state.meaning_model() else {
        return;
    };
    let Some((store, _)) = state.event_journal() else {
        return;
    };
    let wake = store.meaning_wake();
    loop {
        // Take the wake before the pass, so a save during the pass runs one more.
        let woken = wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        match fill_meaning(&state, model.as_ref()).await {
            Ok(settled) if settled > 0 => {
                tracing::debug!(settled, model = model.tag(), "meaning: vectors made")
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                model = model.tag(),
                "meaning: could not make vectors; the records wait for the next save"
            ),
        }
        woken.await;
    }
}
