//! Fetch the pinned model once, check it, and pack its weights as f16 for the binary.
//!
//! The model is `intfloat/multilingual-e5-small` at a pinned revision (ADR-0186). Its files
//! are too large for the repository (470 MB), so the build fetches them from the model's
//! public home, checks each against its SHA-256, and keeps them in a cache under
//! `CARGO_HOME` so a build fetches them once per machine. The weights go into the binary as
//! f16 (half the size); the embedder widens them to f32 when it loads.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// The model and the revision every Temper embeds with. Changing either changes every
/// stored meaning: the kernel re-embeds when the tag it wrote differs (ADR-0186).
const REPO: &str = "intfloat/multilingual-e5-small";
const REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";
const FILES: [(&str, &str); 2] = [
    (
        "model.safetensors",
        "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477",
    ),
    (
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let cache = cache_dir().join(REVISION);
    std::fs::create_dir_all(&cache).expect("create the model cache");
    let model = fetch(&cache, FILES[0]);
    let tokenizer = fetch(&cache, FILES[1]);
    pack_vocabulary(&tokenizer, &out);
    pack(&model, &out);
    let mut tag = std::fs::File::create(out.join("model_tag.rs")).expect("model_tag.rs");
    writeln!(
        tag,
        "/// The model's id and revision, stored beside every meaning it makes.\npub const MODEL_TAG: &str = \"{REPO}@{REVISION}\";"
    )
    .expect("write model_tag.rs");
}

fn cache_dir() -> PathBuf {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .expect("CARGO_HOME or HOME");
    home.join("temper-meaning-cache")
}

/// The cached file, fetched first when it is missing or does not match its hash.
fn fetch(cache: &Path, (name, sha): (&str, &str)) -> PathBuf {
    let path = cache.join(name);
    if path.exists() && sha256(&path) == sha {
        return path;
    }
    let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{name}");
    // One partial file per build script: builds in separate target folders run this at the same time against one
    // CARGO_HOME, and a shared partial was renamed away by the first to finish while the others still read it.
    // The rename into place is atomic and every copy is checked against the same hash, so the last one wins harmlessly.
    let partial = cache.join(format!("{name}.{}.partial", std::process::id()));
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "5", "--retry-all-errors", "-o"])
        .arg(&partial)
        .arg(&url)
        .status()
        .expect("run curl to fetch the model");
    assert!(status.success(), "fetching {url} failed: {status}");
    let got = sha256(&partial);
    assert_eq!(got, sha, "{url} has SHA-256 {got}, expected {sha}");
    std::fs::rename(&partial, &path).expect("move the fetched file into the cache");
    path
}

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read a cached model file");
    Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Pack the tokenizer as flat little-endian tables the embedder reads in place, so the
/// 250,002-piece vocabulary costs no heap: `pieces.bin` (every piece's UTF-8, end to end),
/// `piece_ends.bin` (u32 end offset of each piece), `scores.bin` (f64 score of each piece),
/// `sorted.bin` (u32 piece ids in byte order of their text, for prefix search) and
/// `charsmap.bin` (the normalizer's precompiled character map). The checks pin the shape
/// the tokenizer code assumes: a unigram model, its normalizer and Metaspace pre-tokenizer.
fn pack_vocabulary(tokenizer: &Path, out: &Path) {
    use base64::Engine;
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tokenizer).expect("read tokenizer.json"))
            .expect("parse tokenizer.json");
    let model = &json["model"];
    assert_eq!(model["type"], "Unigram");
    assert_eq!(model["unk_id"], 3);
    assert_eq!(json["pre_tokenizer"]["type"], "Metaspace");
    assert_eq!(json["pre_tokenizer"]["add_prefix_space"], true);
    let normalizers = json["normalizer"]["normalizers"]
        .as_array()
        .expect("normalizers");
    assert_eq!(normalizers.len(), 2);
    assert_eq!(normalizers[0]["type"], "Precompiled");
    assert_eq!(normalizers[1]["type"], "Replace");
    assert_eq!(normalizers[1]["pattern"]["Regex"], " {2,}");
    let charsmap = base64::engine::general_purpose::STANDARD
        .decode(
            normalizers[0]["precompiled_charsmap"]
                .as_str()
                .expect("charsmap"),
        )
        .expect("decode charsmap");
    std::fs::write(out.join("charsmap.bin"), charsmap).expect("charsmap.bin");

    let vocab = model["vocab"].as_array().expect("vocab");
    let mut pieces = Vec::new();
    let mut ends = Vec::new();
    let mut scores = Vec::new();
    let mut texts: Vec<(&str, u32)> = Vec::with_capacity(vocab.len());
    for (id, entry) in vocab.iter().enumerate() {
        let piece = entry[0].as_str().expect("piece");
        let score = entry[1].as_f64().expect("score");
        pieces.extend_from_slice(piece.as_bytes());
        ends.extend_from_slice(&(pieces.len() as u32).to_le_bytes());
        scores.extend_from_slice(&score.to_le_bytes());
        texts.push((piece, id as u32));
    }
    texts.sort();
    let sorted: Vec<u8> = texts.iter().flat_map(|(_, id)| id.to_le_bytes()).collect();
    std::fs::write(out.join("pieces.bin"), pieces).expect("pieces.bin");
    std::fs::write(out.join("piece_ends.bin"), ends).expect("piece_ends.bin");
    std::fs::write(out.join("scores.bin"), scores).expect("scores.bin");
    std::fs::write(out.join("sorted.bin"), sorted).expect("sorted.bin");
}

/// Write every tensor the encoder uses as little-endian f16 into `weights.f16`, and
/// `layout.rs` naming each tensor's offset and length in elements.
fn pack(model: &Path, out: &Path) {
    let bytes = std::fs::read(model).expect("read model.safetensors");
    let header_len = u64::from_le_bytes(bytes[..8].try_into().expect("header length")) as usize;
    let header: serde_json::Value =
        serde_json::from_slice(&bytes[8..8 + header_len]).expect("safetensors header");
    let data = &bytes[8 + header_len..];
    let mut names: Vec<&String> = header
        .as_object()
        .expect("header object")
        .keys()
        .filter(|name| {
            name.as_str() != "__metadata__"
                && !name.starts_with("pooler.")
                && name.as_str() != "embeddings.position_ids"
        })
        .collect();
    names.sort();
    let mut weights = std::io::BufWriter::new(
        std::fs::File::create(out.join("weights.f16")).expect("weights.f16"),
    );
    let mut layout = String::from(
        "/// Each tensor in `weights.f16`: its name, offset and length, in f16 elements.\npub(crate) static TENSORS: &[(&str, usize, usize)] = &[\n",
    );
    let mut offset = 0usize;
    for name in names {
        let info = &header[name.as_str()];
        assert_eq!(info["dtype"], "F32", "{name} is not F32");
        let range = info["data_offsets"].as_array().expect("data_offsets");
        let start = range[0].as_u64().expect("start") as usize;
        let end = range[1].as_u64().expect("end") as usize;
        let len = (end - start) / 4;
        for chunk in data[start..end].chunks_exact(4) {
            let value = f32::from_le_bytes(chunk.try_into().expect("f32"));
            weights
                .write_all(&half::f16::from_f32(value).to_le_bytes())
                .expect("write weights.f16");
        }
        layout.push_str(&format!("    (\"{name}\", {offset}, {len}),\n"));
        offset += len;
    }
    layout.push_str("];\n");
    weights.flush().expect("flush weights.f16");
    std::fs::write(out.join("layout.rs"), layout).expect("layout.rs");
}
