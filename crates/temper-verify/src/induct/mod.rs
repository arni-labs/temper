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
mod guess;
mod hints;
mod live;
mod moves;
mod proof;
mod run;
mod solver;
mod vocab;

#[cfg(test)]
mod regression_test;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Instant;

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name};

use encode::{Enc, initial, prelude};
use guess::Guess;
pub use hints::{Hints, Lemma};
pub use live::{Measure, Region};
use moves::Moves;
pub use proof::{Obligation, Outcome, RuleProof};
use run::{Ask, Session, few_in_flight, run};
use vocab::Vocab;

/// One attempt at a rule: its obligations, what the solver refused, and the
/// guesses it held.
struct Attempt {
    obligations: Vec<Obligation>,
    errors: Vec<String>,
    kept: Vec<Lemma>,
    guesses: Vec<Guess>,
}

impl Attempt {
    fn proved(&self) -> bool {
        self.errors.is_empty()
            && self
                .obligations
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Holds))
    }
}

/// Prove each invariant and `eventually` rule of `automatons` with the hints
/// written beside it, and those `extra` gives by the rule's name.
pub fn prove(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    extra: &Hints,
) -> Vec<RuleProof> {
    prove_some(automatons, policies, extra, |_, _| true, None)
}

/// [`prove`], for the rules `wanted` picks by their type and name. Solvers
/// still checking at `deadline` stop, and their rules are not proved.
pub fn prove_some(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    extra: &Hints,
    wanted: impl Fn(&str, &str) -> bool,
    deadline: Option<Instant>,
) -> Vec<RuleProof> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut rules = Vec::new();
    for automaton in automatons {
        let owner = automaton.automaton.name.as_str();
        for invariant in automaton
            .invariants
            .iter()
            .filter(|i| wanted(owner, &i.name))
        {
            let given = Hints::given(extra, &invariant.name, &invariant.lemma, &[], &[]);
            let rule = Rule::Always(&invariant.assert);
            rules.push((owner, invariant.name.as_str(), rule, given));
        }
        for liveness in automaton.liveness.iter().filter(|l| wanted(owner, &l.name)) {
            if let Some(goal) = &liveness.eventually {
                let given = Hints::given(
                    extra,
                    &liveness.name,
                    &liveness.lemma,
                    &liveness.measure,
                    &liveness.region,
                );
                let rule = Rule::Eventually(goal, &liveness.relies_on);
                rules.push((owner, liveness.name.as_str(), rule, given));
            }
        }
    }
    // The rules are proved side by side, and their solvers share the
    // machine's cores.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let shared = &Shared {
        by_name,
        policies,
        answered: Mutex::new(BTreeMap::new()),
        solvers: (cores / rules.len().max(1)).max(1),
        deadline,
    };
    std::thread::scope(|scope| {
        let proofs: Vec<_> = rules
            .iter()
            .map(|(owner, name, rule, given)| {
                let start = Instant::now();
                let proof = scope.spawn(move || prove_one(shared, owner, name, *rule, given));
                (owner, name, start, proof)
            })
            .collect();
        proofs
            .into_iter()
            .map(|(owner, name, start, proof)| {
                // A solver that could not run proves nothing, and says why.
                proof
                    .join()
                    .expect("a proof thread panicked")
                    .unwrap_or_else(|e| RuleProof {
                        owner: owner.to_string(),
                        rule: name.to_string(),
                        scope: BTreeSet::from([owner.to_string()]),
                        obligations: Vec::new(),
                        limits: Vec::new(),
                        unheld: BTreeSet::new(),
                        found: Vec::new(),
                        premise_excluded: false,
                        errors: vec![format!("the solver did not run: {e}")],
                        took: start.elapsed(),
                    })
            })
            .collect()
    })
}

/// What the proofs of one app's rules share.
struct Shared<'a> {
    by_name: BTreeMap<&'a str, &'a Automaton>,
    policies: Option<&'a SendAnalysis>,
    /// What Cedar answers is the same for every rule.
    answered: Mutex<BTreeMap<String, May>>,
    /// Solvers for each rule.
    solvers: usize,
    deadline: Option<Instant>,
}

/// A rule: always, at states with no dispatch in flight; or eventually, as
/// the system's moves and the actions it relies on bring it about.
#[derive(Clone, Copy)]
enum Rule<'r> {
    Always(&'r Expr),
    Eventually(&'r Expr, &'r [String]),
}

fn prove_one(
    shared: &Shared<'_>,
    owner: &str,
    rule: &str,
    kind: Rule<'_>,
    hints: &Hints,
) -> std::io::Result<RuleProof> {
    let start = Instant::now();
    let by_name = &shared.by_name;
    let expr = match kind {
        Rule::Always(expr) | Rule::Eventually(expr, _) => expr,
    };
    let mut scope = BTreeSet::from([owner.to_string()]);
    expr.for_each_name(&mut |name| {
        if let Name::CrossStatus { entity_type, .. } | Name::CrossField { entity_type, .. } = name
            && by_name.contains_key(entity_type)
        {
            scope.insert(entity_type.to_string());
        }
    });
    let lemmas: Vec<&Lemma> = hints.lemma.iter().filter(|l| l.rule == rule).collect();
    let regions: Vec<&Region> = hints.region.iter().filter(|r| r.rule == rule).collect();
    let measure = hints.measure.iter().find(|m| m.rule == rule);
    let v = Vocab::new(
        by_name.clone(),
        scope.clone(),
        shared.policies,
        &shared.answered,
    );
    let enc = Enc::new(&v);
    let builder = Moves::new(&enc);
    let moves = builder.all();
    let read_by_moves = enc.take_unheld();
    enc.expr(expr, owner, "pre");
    let unheld = enc.take_unheld();
    // A kept guess, as a lemma the agent can write beside the rule: its
    // clause before a move, over `$`-framed names.
    let pinned = |guess: &Guess| {
        let holds = match kind {
            Rule::Always(assert) => enc.expr(assert, owner, "pre"),
            Rule::Eventually(..) => String::new(),
        };
        guess.lemma(rule, &holds)
    };
    // The clauses of J with `guesses`, by name, as functions of the frame.
    let clauses = |guesses: &[Guess], frame: &str| -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Rule::Always(assert) = kind {
            let holds = enc.expr(assert, owner, frame);
            out.push((rule.to_string(), format!("(=> {frame}.settled {holds})")));
            out.extend(
                guesses
                    .iter()
                    .map(|g| (g.name(rule), g.read(frame, &holds))),
            );
        }
        out.extend(
            lemmas
                .iter()
                .map(|l| (l.name.clone(), l.assert.replace('$', &format!("{frame}."))))
                .collect::<Vec<_>>(),
        );
        out
    };
    // One attempt with `guesses`, dropping those a move breaks until the
    // rest hold or the rule fails; `None` once every guess is dropped,
    // unless the attempt may go on with none.
    let attempt = |mut guesses: Vec<Guess>, bare: bool| -> std::io::Result<Option<Attempt>> {
        loop {
            let guessed: BTreeSet<String> = guesses.iter().map(|g| g.name(rule)).collect();
            let clause = |frame: &str| clauses(&guesses, frame);
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
                    regions: regions.clone(),
                };
                live::asks(&enc, &moves, &live, &clause, &mut asks, &mut errors);
            }
            let (mut obligations, mut refused) = solve(shared, &v, rule, &asks, &guessed)?;
            errors.append(&mut refused);
            fn broken(o: &Obligation) -> Option<&Vec<String>> {
                match &o.outcome {
                    Outcome::Fails { clauses, .. } => Some(clauses),
                    _ => None,
                }
            }
            let only_guesses = |o: &Obligation| {
                broken(o).is_some_and(|c| !c.is_empty() && c.iter().all(|n| guessed.contains(n)))
            };
            let dropped: BTreeSet<String> = obligations
                .iter()
                .filter(|o| only_guesses(o))
                .filter_map(broken)
                .flatten()
                .cloned()
                .collect();
            let failed = obligations
                .iter()
                .any(|o| !matches!(o.outcome, Outcome::Holds) && !only_guesses(o));
            if failed || dropped.is_empty() || !errors.is_empty() {
                obligations.retain(|o| !only_guesses(o));
                return Ok(Some(Attempt {
                    obligations,
                    errors,
                    kept: guesses.iter().map(pinned).collect(),
                    guesses,
                }));
            }
            guesses.retain(|g| !dropped.contains(&g.name(rule)));
            if guesses.is_empty() && !bare {
                return Ok(None);
            }
        }
    };
    // An invariant the agent gave no lemmas is tried at every state, and
    // as it is once that is dropped; then with the prover's guesses about
    // the flags it reads. A failure with more clauses held replaces the one
    // before.
    let tries = match kind {
        Rule::Always(_) if lemmas.is_empty() => {
            let all = Guess::all(&v, owner, expr);
            let mut tries = vec![(vec![Guess::Everywhere], true)];
            if all.len() > 1 {
                tries.push((all, false));
            }
            tries
        }
        _ => vec![(Vec::new(), true)],
    };
    let mut last = None;
    for (guesses, bare) in tries {
        if let Some(tried) = attempt(guesses, bare)? {
            let proved = tried.proved();
            last = Some(tried);
            if proved {
                break;
            }
        }
    }
    let last = last.expect("the first attempt holds no guess to drop");
    // A proved rule whose premise no state of J meets says nothing.
    let premise_excluded = match crate::composite::joint::premise(expr) {
        Some(premise) if last.proved() => {
            let at = clauses(&last.guesses, "pre");
            let premise = enc.expr(premise, owner, "pre");
            let mut text = String::from("(push 1)\n");
            for d in enc.take_decls() {
                text.push_str(&format!("{d}\n"));
            }
            for (_, c) in &at {
                text.push_str(&format!("(assert {c})\n"));
            }
            text.push_str(&format!("(assert {premise})\n"));
            // An invariant's premise counts where nothing is in flight.
            if let Rule::Always(_) = kind {
                text.push_str("(assert pre.settled)\n");
            }
            let goal = vec![("premise met".to_string(), "false".to_string())];
            let ask = Ask::new(&v, "premise", text, goal, &at);
            let (checked, _) = solve(shared, &v, rule, &[ask], &BTreeSet::new())?;
            matches!(checked.first().map(|o| &o.outcome), Some(Outcome::Holds))
        }
        _ => false,
    };
    let Attempt {
        obligations,
        errors,
        kept,
        ..
    } = last;
    let found = match errors.is_empty()
        && obligations
            .iter()
            .all(|o| matches!(o.outcome, Outcome::Holds))
    {
        true => kept,
        false => Vec::new(),
    };
    let mut limits: Vec<String> = builder.limits.into_inner().into_iter().collect();
    limits.extend(
        read_by_moves
            .into_iter()
            .map(|reason| format!("a guard, effect or reaction reads {reason}")),
    );
    Ok(RuleProof {
        owner: owner.to_string(),
        rule: rule.to_string(),
        scope,
        obligations,
        limits,
        unheld,
        found,
        premise_excluded,
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

/// Check `asks` over the rule's solvers, sharing the string literals they
/// name: each distinct from the others, and the empty string a parameter is
/// checked against.
fn solve(
    shared: &Shared<'_>,
    v: &Vocab<'_>,
    rule: &str,
    asks: &[Ask],
    guessed: &BTreeSet<String>,
) -> std::io::Result<(Vec<Obligation>, Vec<String>)> {
    let mut literals = BTreeSet::from([String::new()]);
    for ask in asks {
        literals.extend(quoted_literals(&ask.text));
        for (_, c) in &ask.goal {
            literals.extend(quoted_literals(c));
        }
    }
    let prelude = prelude(v, &literals);
    let shown: Vec<String> = literals.iter().map(|l| Enc::lit(l)).collect();
    let few = few_in_flight(v);
    let session = Session {
        prelude: &prelude,
        few: &few,
        literals: &shown,
        solvers: shared.solvers.clamp(1, asks.len().max(1)),
        deadline: shared.deadline,
    };
    run(rule, &session, asks, guessed)
}
