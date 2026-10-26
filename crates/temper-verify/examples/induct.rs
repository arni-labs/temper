//! Prove a spec dir's invariants by induction: `induct <dir> [lemmas.toml]`.
//! The dir holds `*.ioa.toml` and `policies/*.cedar`; the lemma file holds
//! `[[lemma]]` tables with `rule`, `name` and `assert` (SMT-LIB).

use std::io::Write;
use std::time::Instant;

use temper_verify::induct::{Lemma, Outcome, prove};

#[derive(serde::Deserialize)]
struct Lemmas {
    #[serde(default)]
    lemma: Vec<Lemma>,
}

fn main() -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: induct <spec dir> [lemmas.toml]");
    let lemmas: Vec<Lemma> = args
        .next()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("lemma file");
            toml::from_str::<Lemmas>(&text)
                .expect("lemma file parses")
                .lemma
        })
        .unwrap_or_default();
    let start = Instant::now();
    let mut automatons = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("spec dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.to_string_lossy().ends_with(".ioa.toml"))
        .collect();
    names.sort();
    for path in names {
        let text = std::fs::read_to_string(&path).expect("spec");
        automatons.push(temper_spec::automaton::parse_automaton(&text).expect("spec parses"));
    }
    let mut files: Vec<_> = std::fs::read_dir(format!("{dir}/policies"))
        .map(|entries| entries.map(|e| e.expect("dir entry").path()).collect())
        .unwrap_or_default();
    files.sort();
    let text: Vec<String> = files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("policy"))
        .collect();
    let policies = (!files.is_empty())
        .then(|| temper_authz::SendAnalysis::new(&text.join("\n\n")).expect("policies parse"));
    let read = start.elapsed();
    let refs: Vec<_> = automatons.iter().collect();
    let proofs = prove(&refs, policies.as_ref(), &lemmas).expect("z3 runs");
    let verbose = std::env::var("VERBOSE").is_ok();
    for proof in &proofs {
        let solver: f64 = proof.obligations.iter().map(|o| o.took.as_secs_f64()).sum();
        let slowest = proof
            .obligations
            .iter()
            .max_by_key(|o| o.took)
            .map(|o| format!("{} {:.1} ms", o.name, o.took.as_secs_f64() * 1000.0))
            .unwrap_or_default();
        writeln!(
            out,
            "{} ({}): {} | {} obligations, solver {:.1} ms, wall {:.1} ms, slowest {slowest}",
            proof.rule,
            proof.scope.iter().cloned().collect::<Vec<_>>().join(" + "),
            if proof.proved() {
                "PROVED"
            } else {
                "NOT PROVED"
            },
            proof.obligations.len(),
            solver * 1000.0,
            proof.took.as_secs_f64() * 1000.0,
        )?;
        for o in &proof.obligations {
            match &o.outcome {
                Outcome::Holds if verbose => writeln!(
                    out,
                    "  ok   {:7.2} ms  {}",
                    o.took.as_secs_f64() * 1000.0,
                    o.name
                )?,
                Outcome::Holds => {}
                Outcome::Unknown => writeln!(
                    out,
                    "  ???  {:7.2} ms  {}",
                    o.took.as_secs_f64() * 1000.0,
                    o.name
                )?,
                Outcome::Fails { clauses, cti } => {
                    writeln!(
                        out,
                        "  FAIL {:7.2} ms  {}: {}",
                        o.took.as_secs_f64() * 1000.0,
                        o.name,
                        clauses.join(", ")
                    )?;
                    if verbose || std::env::var("CTI").is_ok_and(|c| o.name.contains(&c)) {
                        writeln!(out, "{cti}")?;
                    }
                }
            }
        }
        let never: Vec<&str> = proof
            .obligations
            .iter()
            .filter(|o| !o.possible)
            .map(|o| o.name.as_str())
            .collect();
        if !never.is_empty() {
            writeln!(
                out,
                "  {} moves never possible where the clauses hold: {}",
                never.len(),
                never.join("; ")
            )?;
        }
        for limit in &proof.limits {
            writeln!(out, "  limit: {limit}")?;
        }
        for error in &proof.errors {
            writeln!(out, "  solver error: {error}")?;
        }
    }
    writeln!(
        out,
        "read specs {:.1} ms, total {:.1} ms",
        read.as_secs_f64() * 1000.0,
        start.elapsed().as_secs_f64() * 1000.0
    )?;
    Ok(())
}
