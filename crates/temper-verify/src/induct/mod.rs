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

mod alone;
mod cedar;
mod effects;
mod encode;
mod general;
mod guess;
mod hints;
mod houdini;
mod live;
mod moves;
mod pool;
mod proof;
mod run;
mod solver;
mod vocab;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name};

pub use alone::prove_alone;
use encode::{Enc, initial};
use guess::Guess;
pub use hints::{Hints, Lemma};
pub use live::{Measure, Region};
use moves::Moves;
pub use proof::{Obligation, Outcome, RuleProof};
use run::{Ask, Clauses, Goal, Kept, Reads};
use vocab::Vocab;

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
    let answered = Answered::default();
    prove_with(automatons, policies, &answered, extra, wanted, deadline)
}

/// What Cedar answered the proofs of one app's rules, which every proof
/// reads the same: a proof made one rule at a time keeps it between them.
#[derive(Default)]
pub struct Answered(Mutex<BTreeMap<String, May>>);

/// The proof of `owner`'s rule `rule` with the lemmas written beside it,
/// `None` when it is no invariant or `eventually` rule of `automatons`.
/// `answered` is what Cedar answered the app's other proofs.
pub fn prove_rule(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    answered: &Answered,
    (owner, rule): (&str, &str),
    deadline: Option<Instant>,
) -> Option<RuleProof> {
    let wanted = |o: &str, r: &str| o == owner && r == rule;
    prove_with(
        automatons,
        policies,
        answered,
        &Hints::default(),
        wanted,
        deadline,
    )
    .pop()
}

fn prove_with(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    answered: &Answered,
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
        answered: &answered.0,
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
                        dropped: BTreeMap::new(),
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
    answered: &'a Mutex<BTreeMap<String, May>>,
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
        shared.answered,
    );
    let enc = Enc::new(&v);
    let builder = Moves::new(&enc);
    let moves = builder.all();
    let groups = builder.groups(&moves);
    let read_by_moves = enc.take_unheld();
    enc.expr(expr, owner, "pre");
    let unheld = enc.take_unheld();
    // The prover's guesses, for an invariant: none a lemma already names.
    let guesses: Vec<Guess> = match kind {
        Rule::Always(_) => Guess::all(&v, owner, expr)
            .into_iter()
            .filter(|g| lemmas.iter().all(|l| l.name != g.name(rule)))
            .collect(),
        Rule::Eventually(..) => Vec::new(),
    };
    // A kept guess, as a lemma the agent can write beside the rule: its
    // clause before a move, over `$`-framed names.
    let pinned = |guess: &Guess| {
        let holds = match kind {
            Rule::Always(assert) => enc.expr(assert, owner, "pre"),
            Rule::Eventually(..) => String::new(),
        };
        guess.lemma(rule, &holds)
    };
    // Every clause of J the proof may hold, by name, read in `frame`: the
    // rule, the prover's guesses and the agent's lemmas.
    let read = |frame: &str| -> Vec<(String, String)> {
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
            (lemmas.iter()).map(|l| (l.name.clone(), l.assert.replace('$', &format!("{frame}.")))),
        );
        out
    };
    let (before, after) = (read("pre"), read("post"));
    let clauses = Clauses {
        decls: enc.take_decls(),
        named: (before.iter().zip(&after))
            .map(|((name, pre), (_, post))| (name.clone(), pre.clone(), post.clone()))
            .collect(),
    };
    let mut asks = Vec::new();
    // The start.
    {
        let mut text = String::from("(push 1)\n");
        for fact in initial(&v) {
            text.push_str(&format!("(assert {fact})\n"));
        }
        let reads = Reads::new(&v, &before, &before);
        asks.push(Ask::new("start", text, clauses.read("pre").into(), &reads));
    }
    let moved = Reads::new(&v, &before, &after);
    let post: Arc<[(Goal, String)]> = clauses.read("post").into();
    let ask = |mv: &moves::Move| {
        let mut text = String::from("(push 1)\n");
        mv.decls
            .iter()
            .for_each(|d| text.push_str(&format!("{d}\n")));
        for fact in mv.body() {
            text.push_str(&format!("(assert {fact})\n"));
        }
        Ask {
            assumes: true,
            ..Ask::new(&mv.label, text, Arc::clone(&post), &moved)
        }
    };
    general::lay_out(&groups, &moves, &ask, &mut asks);
    let mut errors = Vec::new();
    if let Rule::Eventually(goal, relies_on) = kind {
        let live = live::Live {
            owner,
            goal,
            relies_on,
            measure,
            regions,
        };
        live::asks(&enc, &moves, &live, &before, &mut asks, &mut errors);
    }
    let proof = asks.len();
    // A proved rule whose premise no state of J meets says nothing.
    if let Some(premise) = crate::composite::joint::premise(expr) {
        let premise = enc.expr(premise, owner, "pre");
        let mut text = String::from("(push 1)\n");
        for d in enc.take_decls() {
            text.push_str(&format!("{d}\n"));
        }
        text.push_str(&format!("(assert {premise})\n"));
        // An invariant's premise counts where nothing is in flight.
        if let Rule::Always(_) = kind {
            text.push_str("(assert pre.settled)\n");
        }
        let goal = vec![("premise met".to_string(), "false".to_string())];
        let reads = Reads::new(&v, &before, &goal);
        asks.push(Ask {
            assumes: true,
            ..Ask::new(
                "premise",
                text,
                goal.into_iter()
                    .map(|(name, text)| (Goal::Required(name), text))
                    .collect(),
                &reads,
            )
        });
    }
    // Every try holds the agent's lemmas, and drops each a move breaks. An
    // invariant is tried first with its lemmas alone, or with none written
    // with the rule at every state, then with every guess of the prover's.
    // A failure with more clauses held replaces the one before.
    let guessed: BTreeSet<String> = guesses.iter().map(|g| g.name(rule)).collect();
    let droppable: BTreeSet<usize> = clauses
        .named
        .iter()
        .enumerate()
        .filter(|(_, (name, ..))| {
            name != rule && (guessed.contains(name) || lemmas.iter().any(|l| l.name == *name))
        })
        .map(|(i, _)| i)
        .collect();
    let everywhere = Guess::Everywhere.name(rule);
    let first: BTreeSet<usize> = clauses
        .named
        .iter()
        .enumerate()
        .filter(|(_, (name, ..))| {
            guessed.contains(name) && (!lemmas.is_empty() || *name != everywhere)
        })
        .map(|(i, _)| i)
        .collect();
    let mut tries = Vec::new();
    if !first.is_empty() {
        tries.push((first, true));
    }
    tries.push((BTreeSet::new(), tries.is_empty()));
    let (settled, premise_excluded) =
        pool::with_pool(shared, &v, rule, (&clauses, &asks), |pool| {
            let mut last: Option<houdini::Settled> = None;
            for (left_out, may_end_bare) in &tries {
                let attempt = (&clauses, &asks[..proof]);
                let tried = houdini::settle(pool, attempt, &droppable, (left_out, *may_end_bare))?;
                if let Some(tried) = tried {
                    let proved = tried.proved();
                    last = Some(tried);
                    if proved {
                        break;
                    }
                }
            }
            let mut last = last.expect("the first try may end with no clause to drop");
            last.errors.splice(0..0, errors);
            let excluded = match asks.get(proof) {
                Some(_) if last.proved() => {
                    let kept = Kept::new(&clauses, last.out.clone());
                    let checked = pool.check(&[proof], &kept)?;
                    let holds =
                        |(o, _, _): &(Obligation, _, _)| matches!(o.outcome, Outcome::Holds);
                    checked.first().and_then(Option::as_ref).is_some_and(holds)
                }
                _ => false,
            };
            Ok((last, excluded))
        })?;
    let proved = settled.proved();
    let out: BTreeSet<&str> = settled
        .out
        .iter()
        .map(|&i| clauses.named[i].0.as_str())
        .collect();
    let found = match proved {
        true => (guesses.iter())
            .filter(|g| !out.contains(g.name(rule).as_str()))
            .map(pinned)
            .collect(),
        false => Vec::new(),
    };
    let dropped = (settled.dropped.into_iter())
        .map(|(i, why)| (clauses.named[i].0.clone(), why))
        .filter(|(name, _)| lemmas.iter().any(|l| &l.name == name))
        .collect();
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
        obligations: settled.obligations,
        limits,
        unheld,
        found,
        dropped,
        premise_excluded,
        errors: settled.errors,
        took: start.elapsed(),
    })
}
