//! Prove a spec dir's rules by induction: `induct <dir> [hints.toml]`. The
//! dir holds `*.ioa.toml` and `policies/*.cedar`; the hints file holds
//! `[[lemma]]` tables (`rule`, `name`, `assert`), and for `eventually`
//! rules `[[measure]]` (`rule`, `terms`) and `[[region]]` tables (`rule`,
//! `name`, `region`, `witness`, `step`, `params`), all in SMT-LIB.

use std::io::Write;
use std::time::Instant;

use temper_verify::induct::{Hints, Outcome, prove};

fn main() -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: induct <spec dir> [hints.toml]");
    let hints: Hints = args
        .next()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("hints file");
            toml::from_str(&text).expect("hints file parses")
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
        // A spec in the old predicate grammar is read as its migration.
        let automaton = temper_spec::automaton::parse_automaton(&text)
            .map_err(|e| e.to_string())
            .or_else(|_| {
                let migrated = temper_spec::automaton::legacy::migrate_source(&text)?;
                temper_spec::automaton::parse_automaton(&migrated.source).map_err(|e| e.to_string())
            });
        match automaton {
            Ok(automaton) => automatons.push(automaton),
            Err(e) => println!("skipped {}: {e}", path.display()),
        }
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
    let proofs = prove(&refs, policies.as_ref(), &hints);
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
        if !proof.found.is_empty() {
            writeln!(out, "  kept the prover's guesses, as lemmas:")?;
            for lemma in &proof.found {
                writeln!(out, "    {}: {}", lemma.name, lemma.assert)?;
            }
        }
        for reason in &proof.unheld {
            writeln!(out, "  the rule reads {reason}")?;
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
