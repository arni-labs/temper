//! Proof by induction of an app's invariants, for every run.
//!
//! The joint walk checks a rule over the states a bounded walk reaches. This
//! proves it for all of them: a set of clauses `J` (the rule, read at states
//! with nothing in flight, and the lemmas it needs) holds at the start, and
//! every move from a state where `J` holds leads to one where it holds. Each
//! move's obligation is one solver check, whatever the number of states.
//!
//! When an obligation fails, the solver's model is a counterexample to
//! induction: a state where `J` holds and a move from it after which it does
//! not. If that state is reachable, the rule is broken; if not, a lemma that
//! rules it out is missing. Lemmas are SMT-LIB over `$`-framed names (see
//! `encode.rs`), read at both ends of each move.
//!
//! The moves allow what the joint walk does and more (`moves.rs`), so a
//! proof here holds wherever the walk's model of the kernel does. An
//! `eventually` rule is proved with a measure and helpful moves
//! (`live.rs`).

mod cedar;
mod effects;
mod encode;
mod live;
mod moves;
mod run;
mod solver;
mod vocab;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name};

use encode::{Enc, initial, prelude};
pub use live::{Measure, Region};
use moves::Moves;
use run::{Ask, few_in_flight, run};
use vocab::Vocab;

/// What an agent gives the proofs: lemmas, and for each `eventually` rule a
/// measure and the regions that say which system move brings it closer.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Hints {
    #[serde(default)]
    pub lemma: Vec<Lemma>,
    #[serde(default)]
    pub measure: Vec<Measure>,
    #[serde(default)]
    pub region: Vec<Region>,
}

/// A lemma: a clause the proof holds alongside the rule.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Lemma {
    /// The rule it is for.
    pub rule: String,
    pub name: String,
    /// SMT-LIB over `$`-framed names.
    pub assert: String,
}

/// One obligation's outcome.
#[derive(Debug, Clone)]
pub struct Obligation {
    /// The move, or `start`.
    pub name: String,
    pub took: Duration,
    pub outcome: Outcome,
    /// Whether the move can happen at all from a state where the clauses
    /// hold. One that cannot holds trivially, which a reader should see.
    pub possible: bool,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Holds,
    /// Some clauses fail after it: their names, and the counterexample.
    Fails {
        clauses: Vec<String>,
        cti: String,
    },
    Unknown,
}

/// A rule's proof.
#[derive(Debug, Clone)]
pub struct RuleProof {
    pub rule: String,
    pub scope: BTreeSet<String>,
    pub obligations: Vec<Obligation>,
    /// What the encoding could not express.
    pub limits: Vec<String>,
    /// What the rule itself reads that the proof does not hold, each read
    /// as an unknown.
    pub unheld: BTreeSet<String>,
    /// What the solver refused, such as a lemma naming an unknown kind.
    pub errors: Vec<String>,
    /// Wall time, from reading the specs to the last check.
    pub took: Duration,
}

impl RuleProof {
    pub fn proved(&self) -> bool {
        self.errors.is_empty()
            && self
                .obligations
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Holds))
    }
}

/// Prove each invariant and `eventually` rule of `automatons` with the hints
/// given for it.
pub fn prove(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    hints: &Hints,
) -> std::io::Result<Vec<RuleProof>> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut rules = Vec::new();
    for automaton in automatons {
        let owner = automaton.automaton.name.as_str();
        for invariant in &automaton.invariants {
            rules.push((
                owner,
                invariant.name.as_str(),
                Rule::Always(&invariant.assert),
            ));
        }
        for liveness in &automaton.liveness {
            if let Some(goal) = &liveness.eventually {
                let rule = Rule::Eventually(goal, &liveness.relies_on);
                rules.push((owner, liveness.name.as_str(), rule));
            }
        }
    }
    // The rules are proved side by side, and their solvers share the
    // machine's cores.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let solvers = (cores / rules.len().max(1)).max(1);
    std::thread::scope(|scope| {
        let proofs: Vec<_> = rules
            .iter()
            .map(|&(owner, name, rule)| {
                let by_name = &by_name;
                scope.spawn(move || prove_one(by_name, owner, name, rule, hints, policies, solvers))
            })
            .collect();
        proofs
            .into_iter()
            .map(|proof| proof.join().expect("a proof thread panicked"))
            .collect()
    })
}

/// A rule: always, at states with no dispatch in flight; or eventually, as
/// the system's moves and the actions it relies on bring it about.
#[derive(Clone, Copy)]
enum Rule<'r> {
    Always(&'r Expr),
    Eventually(&'r Expr, &'r [String]),
}

fn prove_one(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    rule: &str,
    kind: Rule<'_>,
    hints: &Hints,
    policies: Option<&SendAnalysis>,
    solvers: usize,
) -> std::io::Result<RuleProof> {
    let start = Instant::now();
    let expr = match kind {
        Rule::Always(expr) | Rule::Eventually(expr, _) => expr,
    };
    let mut scope = BTreeSet::from([owner.to_string()]);
    expr.for_each_name(&mut |name| {
        if let Name::CrossStatus { entity_type, .. } | Name::CrossField { entity_type, .. } = name {
            if by_name.contains_key(entity_type) {
                scope.insert(entity_type.to_string());
            }
        }
    });
    let lemmas: Vec<&Lemma> = hints.lemma.iter().filter(|l| l.rule == rule).collect();
    let regions: Vec<&Region> = hints.region.iter().filter(|r| r.rule == rule).collect();
    let measure = hints.measure.iter().find(|m| m.rule == rule);
    let v = Vocab::new(by_name.clone(), scope.clone(), policies);
    let enc = Enc::new(&v);
    let builder = Moves::new(&enc);
    let moves = builder.all();
    let read_by_moves = enc.take_unheld();
    enc.expr(expr, owner, "pre");
    let unheld = enc.take_unheld();
    // The clauses of J, by name, as functions of the frame.
    let clause = |frame: &str| -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Rule::Always(assert) = kind {
            out.push((
                rule.to_string(),
                format!("(=> {frame}.settled {})", enc.expr(assert, owner, frame)),
            ));
        }
        out.extend(
            lemmas
                .iter()
                .map(|l| (l.name.clone(), l.assert.replace('$', &format!("{frame}."))))
                .collect::<Vec<_>>(),
        );
        out
    };
    let mut asks = Vec::new();
    // The start.
    {
        let at_start = clause("pre");
        let mut text = String::from("(push 1)\n");
        enc.take_decls()
            .iter()
            .for_each(|d| text.push_str(&format!("{d}\n")));
        for fact in initial(&v) {
            text.push_str(&format!("(assert {fact})\n"));
        }
        asks.push(Ask::new(&v, "start", text, at_start.clone(), &at_start));
    }
    for mv in &moves {
        let before = clause("pre");
        let after = clause("post");
        let decls = enc.take_decls();
        let mut text = String::from("(push 1)\n");
        decls
            .iter()
            .chain(&mv.decls)
            .for_each(|d| text.push_str(&format!("{d}\n")));
        for (_, c) in &before {
            text.push_str(&format!("(assert {c})\n"));
        }
        for fact in mv.body() {
            text.push_str(&format!("(assert {fact})\n"));
        }
        asks.push(Ask::new(&v, &mv.label, text, after, &before));
    }
    let mut errors = Vec::new();
    if let Rule::Eventually(goal, relies_on) = kind {
        let live = live::Live {
            owner,
            goal,
            relies_on,
            measure,
            regions,
        };
        live::asks(&enc, &moves, &live, &clause, &mut asks, &mut errors);
    }
    let solvers = solvers.clamp(1, asks.len().max(1));
    // The string literals the obligations name, each distinct from the
    // others, and the empty string a parameter is checked against.
    let mut literals = BTreeSet::from([String::new()]);
    for ask in &asks {
        literals.extend(quoted_literals(&ask.text));
        for (_, c) in &ask.goal {
            literals.extend(quoted_literals(c));
        }
    }
    let prelude = prelude(&v, &literals);
    let shown: Vec<String> = literals.iter().map(|l| Enc::lit(l)).collect();
    let (obligations, mut refused) =
        run(rule, &prelude, &few_in_flight(&v), &shown, &asks, solvers)?;
    errors.append(&mut refused);
    let mut limits: Vec<String> = builder.limits.into_inner().into_iter().collect();
    limits.extend(
        read_by_moves
            .into_iter()
            .map(|reason| format!("a guard, effect or reaction reads {reason}")),
    );
    Ok(RuleProof {
        rule: rule.to_string(),
        scope,
        obligations,
        limits,
        unheld,
        errors,
        took: start.elapsed(),
    })
}

/// The string literals `text` names, as `|'x'|`.
fn quoted_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("|'") {
        let after = &rest[at + 2..];
        let Some(end) = after.find("'|") else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 2..];
    }
    out
}
