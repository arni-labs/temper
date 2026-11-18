//! The kernel's text embedder (ADR-0186).
//!
//! `intfloat/multilingual-e5-small` (a 12-layer, 384-wide BERT encoder over a 250,037-piece
//! multilingual vocabulary) runs inside the Temper process, so search by meaning needs no
//! outside service and no app declares anything. The weights are packed into the binary as
//! f16 by `build.rs`:
//!
//! - the encoder layers, position and type tables are widened to f32 once, at
//!   [`Embedder::load`] (about 86 MB of heap);
//! - the vocabulary table (192 MB as f16) stays in the binary's read-only data and is read
//!   one row per word piece, so only the rows a Temper's texts use are ever paged in.
//!
//! One text is embedded at a time, on the calling thread, with single-threaded matrix
//! products, so a given build gives the same vector for the same text every time.

mod layout {
    include!(concat!(env!("OUT_DIR"), "/layout.rs"));
}
include!(concat!(env!("OUT_DIR"), "/model_tag.rs"));

use gemm::{Parallelism, gemm};
use half::f16;
mod tokenizer;

use tokenizer::Tokenizer;

static WEIGHTS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/weights.f16"));

/// Numbers in one meaning vector.
pub const DIMS: usize = 384;
/// Word pieces read from one text, counting the start and end marks. The model reads up to
/// 512; ADR-0186 keeps one meaning per record, from its first 256 pieces, which also
/// bounds the cost of one embedding.
pub const MAX_PIECES: usize = 256;

const LAYERS: usize = 12;
const HEADS: usize = 12;
const HEAD_DIMS: usize = DIMS / HEADS;
const FFN: usize = 1536;
const LAYER_NORM_EPS: f32 = 1e-12;

/// What a text is, which the model was trained to tell apart by a prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// A record's own text, stored in the index.
    Passage,
    /// The words someone searches with.
    Query,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Passage => "passage: ",
            Kind::Query => "query: ",
        }
    }
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    attention_out: Linear,
    attention_norm: Norm,
    intermediate: Linear,
    output: Linear,
    output_norm: Norm,
}

struct Linear {
    /// `out x in`, row major, as the checkpoint stores it.
    weight: Vec<f32>,
    bias: Vec<f32>,
    inputs: usize,
    outputs: usize,
}

struct Norm {
    weight: Vec<f32>,
    bias: Vec<f32>,
}

/// The loaded model. Loading takes a fraction of a second and about 86 MB.
pub struct Embedder {
    tokenizer: Tokenizer,
    words: &'static [u8],
    positions: Vec<f32>,
    token_type: Vec<f32>,
    embedding_norm: Norm,
    layers: Vec<Layer>,
}

fn tensor(name: &str) -> (&'static [u8], usize) {
    let (_, offset, len) = layout::TENSORS
        .iter()
        .find(|(tensor, _, _)| *tensor == name)
        .unwrap_or_else(|| panic!("the packed model has no tensor {name}"));
    (&WEIGHTS[offset * 2..(offset + len) * 2], *len)
}

fn widen(name: &str) -> Vec<f32> {
    let (bytes, len) = tensor(name);
    let values: Vec<f32> = bytes
        .chunks_exact(2)
        .map(|pair| f16::from_le_bytes([pair[0], pair[1]]).to_f32())
        .collect();
    debug_assert_eq!(values.len(), len);
    values
}

fn linear(prefix: &str, inputs: usize, outputs: usize) -> Linear {
    let weight = widen(&format!("{prefix}.weight"));
    let bias = widen(&format!("{prefix}.bias"));
    assert_eq!(
        weight.len(),
        inputs * outputs,
        "{prefix} has the wrong shape"
    );
    assert_eq!(bias.len(), outputs, "{prefix} has the wrong bias");
    Linear {
        weight,
        bias,
        inputs,
        outputs,
    }
}

fn norm(prefix: &str) -> Norm {
    Norm {
        weight: widen(&format!("{prefix}.weight")),
        bias: widen(&format!("{prefix}.bias")),
    }
}

impl Embedder {
    /// Load the model packed into this binary.
    pub fn load() -> Result<Self, String> {
        let tokenizer = Tokenizer::load()?;
        let layers = (0..LAYERS)
            .map(|i| {
                let p = format!("encoder.layer.{i}");
                Layer {
                    query: linear(&format!("{p}.attention.self.query"), DIMS, DIMS),
                    key: linear(&format!("{p}.attention.self.key"), DIMS, DIMS),
                    value: linear(&format!("{p}.attention.self.value"), DIMS, DIMS),
                    attention_out: linear(&format!("{p}.attention.output.dense"), DIMS, DIMS),
                    attention_norm: norm(&format!("{p}.attention.output.LayerNorm")),
                    intermediate: linear(&format!("{p}.intermediate.dense"), DIMS, FFN),
                    output: linear(&format!("{p}.output.dense"), FFN, DIMS),
                    output_norm: norm(&format!("{p}.output.LayerNorm")),
                }
            })
            .collect();
        let (words, _) = tensor("embeddings.word_embeddings.weight");
        Ok(Self {
            tokenizer,
            words,
            positions: widen("embeddings.position_embeddings.weight"),
            token_type: widen("embeddings.token_type_embeddings.weight"),
            embedding_norm: norm("embeddings.LayerNorm"),
            layers,
        })
    }

    /// The word-piece ids the model reads for `text`, marks included, as the checkpoint's
    /// own tokenizer gives them.
    pub fn piece_ids(&self, text: &str, kind: Kind) -> Vec<u32> {
        self.tokenizer
            .encode(&format!("{}{text}", kind.prefix()), MAX_PIECES)
    }

    /// The text's meaning: a unit-length vector of [`DIMS`] numbers. Only the first
    /// [`MAX_PIECES`] word pieces are read.
    pub fn embed(&self, text: &str, kind: Kind) -> Result<Vec<f32>, String> {
        let mut meanings = self.embed_many(&[text], kind)?;
        Ok(meanings.pop().expect("one meaning for one text"))
    }

    /// The meaning of each text, in order, computed in one pass: every text's pieces are
    /// stacked into one matrix, so the layers' weights are read once for all of them, and
    /// attention stays within each text. The caller bounds the batch (ADR-0186).
    pub fn embed_many(&self, texts: &[&str], kind: Kind) -> Result<Vec<Vec<f32>>, String> {
        let mut spans = Vec::with_capacity(texts.len());
        let mut ids = Vec::new();
        for text in texts {
            let pieces = self.piece_ids(text, kind);
            assert!(
                (2..=MAX_PIECES).contains(&pieces.len()),
                "truncation kept {} pieces",
                pieces.len()
            );
            spans.push((ids.len(), pieces.len()));
            ids.extend(pieces);
        }
        let rows = ids.len();
        let mut x = vec![0f32; rows * DIMS];
        for &(first, len) in &spans {
            for t in 0..len {
                let at = first + t;
                let start = ids[at] as usize * DIMS * 2;
                let word = &self.words[start..start + DIMS * 2];
                let position = &self.positions[t * DIMS..(t + 1) * DIMS];
                let row = &mut x[at * DIMS..(at + 1) * DIMS];
                for d in 0..DIMS {
                    let value = f16::from_le_bytes([word[2 * d], word[2 * d + 1]]).to_f32();
                    row[d] = value + position[d] + self.token_type[d];
                }
            }
        }
        layer_norm(&mut x, &self.embedding_norm);

        for layer in &self.layers {
            let q = layer.query.apply(&x, rows);
            let k = layer.key.apply(&x, rows);
            let v = layer.value.apply(&x, rows);
            let mut context = vec![0f32; rows * DIMS];
            for &(first, len) in &spans {
                let span = first * DIMS..(first + len) * DIMS;
                attention(
                    &q[span.clone()],
                    &k[span.clone()],
                    &v[span.clone()],
                    &mut context[span],
                    len,
                );
            }
            let mut attended = layer.attention_out.apply(&context, rows);
            add(&mut attended, &x);
            layer_norm(&mut attended, &layer.attention_norm);
            let mut hidden = layer.intermediate.apply(&attended, rows);
            hidden.iter_mut().for_each(|h| *h = gelu(*h));
            let mut out = layer.output.apply(&hidden, rows);
            add(&mut out, &attended);
            layer_norm(&mut out, &layer.output_norm);
            x = out;
        }

        spans
            .iter()
            .map(|&(first, len)| mean_unit(&x[first * DIMS..(first + len) * DIMS]))
            .collect()
    }
}

/// The mean of the rows, scaled to unit length.
fn mean_unit(rows: &[f32]) -> Result<Vec<f32>, String> {
    let mut meaning = vec![0f32; DIMS];
    for row in rows.chunks_exact(DIMS) {
        meaning.iter_mut().zip(row).for_each(|(m, v)| *m += v);
    }
    let length = meaning
        .iter()
        .map(|m| f64::from(*m) * f64::from(*m))
        .sum::<f64>()
        .sqrt();
    if !(length.is_finite() && length > 0.0) {
        return Err("the model gave a vector with no length".to_string());
    }
    meaning
        .iter_mut()
        .for_each(|m| *m = (f64::from(*m) / length) as f32);
    Ok(meaning)
}

impl Linear {
    /// `x (rows x inputs) . weightᵀ + bias`.
    fn apply(&self, x: &[f32], rows: usize) -> Vec<f32> {
        assert_eq!(x.len(), rows * self.inputs);
        let mut out = vec![0f32; rows * self.outputs];
        // SAFETY: every pointer spans its matrix: `out` is rows x outputs (row stride
        // outputs), `x` is rows x inputs (row stride inputs) and `weight` is outputs x inputs,
        // read as its transpose (inputs x outputs: step 1 down a column, `inputs` across).
        unsafe {
            gemm(
                rows,
                self.outputs,
                self.inputs,
                out.as_mut_ptr(),
                1,
                self.outputs as isize,
                false,
                x.as_ptr(),
                1,
                self.inputs as isize,
                self.weight.as_ptr(),
                self.inputs as isize,
                1,
                0.0,
                1.0,
                false,
                false,
                false,
                Parallelism::None,
            );
        }
        for row in out.chunks_exact_mut(self.outputs) {
            row.iter_mut().zip(&self.bias).for_each(|(o, b)| *o += b);
        }
        out
    }
}

/// Scaled dot-product attention over every head for one text, all its pieces visible to
/// each other, written into `context`.
fn attention(q: &[f32], k: &[f32], v: &[f32], context: &mut [f32], pieces: usize) {
    let mut scores = vec![0f32; pieces * pieces];
    let scale = 1.0 / (HEAD_DIMS as f32).sqrt();
    for head in 0..HEADS {
        let column = head * HEAD_DIMS;
        // SAFETY: q, k, v and context are pieces x DIMS (row stride DIMS); each head reads
        // and writes HEAD_DIMS columns starting at `column`. `scores` is pieces x pieces.
        unsafe {
            gemm(
                pieces,
                pieces,
                HEAD_DIMS,
                scores.as_mut_ptr(),
                1,
                pieces as isize,
                false,
                q.as_ptr().add(column),
                1,
                DIMS as isize,
                k.as_ptr().add(column),
                DIMS as isize,
                1,
                0.0,
                scale,
                false,
                false,
                false,
                Parallelism::None,
            );
        }
        for row in scores.chunks_exact_mut(pieces) {
            let top = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let mut sum = 0f32;
            row.iter_mut().for_each(|s| {
                *s = (*s - top).exp();
                sum += *s;
            });
            row.iter_mut().for_each(|s| *s /= sum);
        }
        // SAFETY: as above; the product writes this head's columns of `context`.
        unsafe {
            gemm(
                pieces,
                HEAD_DIMS,
                pieces,
                context.as_mut_ptr().add(column),
                1,
                DIMS as isize,
                false,
                scores.as_ptr(),
                1,
                pieces as isize,
                v.as_ptr().add(column),
                1,
                DIMS as isize,
                0.0,
                1.0,
                false,
                false,
                false,
                Parallelism::None,
            );
        }
    }
}

fn add(into: &mut [f32], other: &[f32]) {
    into.iter_mut().zip(other).for_each(|(a, b)| *a += b);
}

fn layer_norm(x: &mut [f32], norm: &Norm) {
    for row in x.chunks_exact_mut(DIMS) {
        let mean = row.iter().map(|v| f64::from(*v)).sum::<f64>() / DIMS as f64;
        let variance = row
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / DIMS as f64;
        let inverse = 1.0 / (variance + f64::from(LAYER_NORM_EPS)).sqrt();
        for (d, v) in row.iter_mut().enumerate() {
            *v = ((f64::from(*v) - mean) * inverse) as f32 * norm.weight[d] + norm.bias[d];
        }
    }
}

/// The exact (erf) GELU the checkpoint was trained with.
fn gelu(x: f32) -> f32 {
    0.5 * x * (1.0 + libm::erff(x / std::f32::consts::SQRT_2))
}
