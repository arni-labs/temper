//! Embed texts given one per line on stdin, then print each vector as JSON and the cost.
//!
//! `cargo run --release -p temper-meaning --example measure < texts.txt > vectors.json`
//! Each output line: {"text", "kind", "ms", "vector"}. The last line: load time, and the
//! process's resident memory after loading and after embedding (Linux and macOS).

use std::io::{BufRead, Write};
use std::time::Instant;

use temper_meaning::{Embedder, Kind};

fn resident_mb() -> f64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

fn main() {
    let before = resident_mb();
    let started = Instant::now();
    let embedder = Embedder::load().expect("load the model");
    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let loaded = resident_mb();
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("read a line");
        let (kind, text) = match line.strip_prefix("query\t") {
            Some(text) => (Kind::Query, text.to_string()),
            None => (Kind::Passage, line.clone()),
        };
        let started = Instant::now();
        let vector = embedder.embed(&text, kind).expect("embed");
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let numbers: Vec<String> = vector.iter().map(|v| format!("{v:.6}")).collect();
        writeln!(
            out,
            "{{\"text\":{:?},\"kind\":\"{:?}\",\"ms\":{ms:.2},\"vector\":[{}]}}",
            text,
            kind,
            numbers.join(","),
        )
        .expect("write a vector");
    }
    writeln!(
        out,
        "{{\"model\":\"{}\",\"load_ms\":{load_ms:.1},\"rss_mb_before\":{before:.1},\"rss_mb_loaded\":{loaded:.1},\"rss_mb_after\":{:.1}}}",
        temper_meaning::MODEL_TAG,
        resident_mb(),
    )
    .expect("write the cost");
}
