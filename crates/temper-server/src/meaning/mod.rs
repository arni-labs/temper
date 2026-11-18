//! Search by meaning (ADR-0186).
//!
//! Every record's store keeps a meaning row beside its query-plane row: the record's
//! text, written in the commit that saves it, and a vector the kernel's model makes
//! from that text just after the save ([`worker`]). `$search` and `Temper.Nearest`
//! rank records by those rows ([`rank`]). Apps declare nothing.
//!
//! The model is a [`MeaningModel`] the binary installs on the server
//! ([`crate::ServerState::install_meaning_model`]). This crate never runs one itself,
//! so simulation-visible code stays deterministic: a simulation installs a
//! deterministic model, and a server without a model still finds records by their
//! words.

use std::future::Future;
use std::pin::Pin;

pub mod rank;
pub mod worker;

/// What a text is to the model: a record's own text, or the words of a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeaningKind {
    /// A record's text.
    Passage,
    /// The words a caller searched for.
    Query,
}

/// A model that turns text into vectors of meaning.
///
/// Vectors from one model (one [`MeaningModel::tag`]) are compared only with each
/// other; a row made by another model waits for this one.
pub trait MeaningModel: Send + Sync + 'static {
    /// The model's id and revision. Stored with every vector it makes.
    fn tag(&self) -> &str;

    /// One vector per text, in order.
    fn embed<'a>(
        &'a self,
        texts: &'a [String],
        kind: MeaningKind,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, String>> + Send + 'a>>;
}

/// A deterministic stand-in for the model, for simulation and tests: a text's
/// vector counts its folded words into fixed buckets, so texts that share words are
/// close and texts that share none are not.
#[cfg(any(test, feature = "sim"))]
#[derive(Debug, Default)]
pub struct WordCountModel;

#[cfg(any(test, feature = "sim"))]
impl WordCountModel {
    /// The tag this model stores.
    pub const TAG: &'static str = "word-count@1";
    const DIMS: usize = 64;

    fn vector(text: &str) -> Vec<f32> {
        let mut vector = vec![0f32; Self::DIMS];
        for word in temper_runtime::persistence::fold(text)
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            // FNV-1a: the same bucket for the same word on every run.
            let hash = word.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
            vector[(hash % Self::DIMS as u64) as usize] += 1.0;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            vector.iter_mut().for_each(|v| *v /= norm);
        }
        vector
    }
}

#[cfg(any(test, feature = "sim"))]
impl MeaningModel for WordCountModel {
    fn tag(&self) -> &str {
        Self::TAG
    }

    fn embed<'a>(
        &'a self,
        texts: &'a [String],
        _kind: MeaningKind,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, String>> + Send + 'a>> {
        Box::pin(async move { Ok(texts.iter().map(|text| Self::vector(text)).collect()) })
    }
}
