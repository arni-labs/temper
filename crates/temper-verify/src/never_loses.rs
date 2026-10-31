//! Measurement only (scratch branches): does any reachable state lose a place?
//!
//! A state loses a place when another state in the same place can still reach
//! it and this one never can. A state whose future has a step past the bounds
//! is reported apart ("bound"), since what lies past the bound is not walked.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use stateright::Model;

use crate::model::semantics::successors_noting_cuts;
use crate::model::{TemperModel, TemperModelState, build_model_from_ioa};

const MAX_STATES: usize = 300_000;
const MAX_TIME: Duration = Duration::from_secs(60);

struct Walk {
    states: Vec<TemperModelState>,
    edges: Vec<Vec<usize>>,
    parent: Vec<Option<(usize, String)>>,
    depth: Vec<usize>,
    cut: Vec<bool>,
}

fn walk(model: &TemperModel) -> Result<Walk, String> {
    let started = Instant::now();
    let mut w = Walk {
        states: Vec::new(),
        edges: Vec::new(),
        parent: Vec::new(),
        depth: Vec::new(),
        cut: Vec::new(),
    };
    let mut index: HashMap<TemperModelState, usize> = HashMap::new();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        if !index.contains_key(&init) {
            index.insert(init.clone(), w.states.len());
            queue.push_back(w.states.len());
            w.states.push(init);
            w.edges.push(Vec::new());
            w.parent.push(None);
            w.depth.push(0);
            w.cut.push(false);
        }
    }
    while let Some(at) = queue.pop_front() {
        if w.states.len() > MAX_STATES || started.elapsed() > MAX_TIME {
            return Err(format!(
                "too big: {} states in {:?}",
                w.states.len(),
                started.elapsed()
            ));
        }
        let state = w.states[at].clone();
        let mut cut = BTreeMap::new();
        let mut out = Vec::new();
        for t in &model.transitions {
            if !model.transition_enabled(t, &state) {
                continue;
            }
            for (_, next) in successors_noting_cuts(model, t, &state, &mut cut) {
                let to = match index.get(&next) {
                    Some(&to) => to,
                    None => {
                        let to = w.states.len();
                        index.insert(next.clone(), to);
                        w.states.push(next);
                        w.edges.push(Vec::new());
                        w.parent.push(Some((at, t.name.clone())));
                        w.depth.push(w.depth[at] + 1);
                        w.cut.push(false);
                        queue.push_back(to);
                        to
                    }
                };
                if !out.contains(&to) {
                    out.push(to);
                }
            }
        }
        w.edges[at] = out;
        w.cut[at] = !cut.is_empty();
    }
    Ok(w)
}

/// Strongly connected components, each emitted after every one it reaches.
fn components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        let mut calls = vec![(root, 0usize)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut i)) = calls.last_mut() {
            if *i < edges[v].len() {
                let w = edges[v][*i];
                *i += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                calls.pop();
                if let Some(&(u, _)) = calls.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
            }
        }
    }
    out
}

fn names(model: &TemperModel, mask: u128) -> String {
    let places: Vec<&str> = model
        .states
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, s)| s.as_str())
        .collect();
    places.join(",")
}

fn path_to(w: &Walk, mut at: usize) -> String {
    let mut moves = Vec::new();
    while let Some((from, name)) = &w.parent[at] {
        moves.push(name.clone());
        at = *from;
    }
    moves.reverse();
    moves.join(" > ")
}

/// One `SPEC` line and a `LOST` line per (place, lost places, sure or bound).
pub fn measure(label: &str, src: &str) -> Vec<String> {
    let model = match build_model_from_ioa(src, 2) {
        Ok(model) => model,
        Err(e) => return vec![format!("SPEC\t{label}\tUNPARSED\t{}", e.replace('\n', " "))],
    };
    if model.states.len() > 128 {
        return vec![format!("SPEC\t{label}\tSKIPPED\tmore than 128 places")];
    }
    let place_bit: BTreeMap<&str, u128> = model
        .states
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), 1u128 << i))
        .collect();
    let w = match walk(&model) {
        Ok(w) => w,
        Err(e) => return vec![format!("SPEC\t{label}\tSKIPPED\t{e}")],
    };
    let n = w.states.len();
    let mut reach = vec![0u128; n];
    let mut tainted = vec![false; n];
    for comp in components(&w.edges) {
        let mut mask = 0u128;
        let mut cut = false;
        for &v in &comp {
            mask |= place_bit
                .get(w.states[v].status.as_str())
                .copied()
                .unwrap_or(0);
            cut |= w.cut[v];
            for &to in &w.edges[v] {
                if !comp.contains(&to) {
                    mask |= reach[to];
                    cut |= tainted[to];
                }
            }
        }
        for &v in &comp {
            reach[v] = mask;
            tainted[v] = cut;
        }
    }
    let mut union: BTreeMap<&str, u128> = BTreeMap::new();
    for v in 0..n {
        *union.entry(w.states[v].status.as_str()).or_default() |= reach[v];
    }
    // (place, lost, bound) -> (count, shallowest example)
    let mut groups: BTreeMap<(String, u128, bool), (usize, usize)> = BTreeMap::new();
    for v in 0..n {
        let place = w.states[v].status.as_str();
        let lost = union[place] & !reach[v];
        if lost == 0 {
            continue;
        }
        let entry = groups
            .entry((place.to_string(), lost, tainted[v]))
            .or_insert((0, v));
        entry.0 += 1;
        if w.depth[v] < w.depth[entry.1] {
            entry.1 = v;
        }
    }
    let sure = groups.keys().filter(|(_, _, bound)| !bound).count();
    let cut_states = w.cut.iter().filter(|c| **c).count();
    let mut lines = vec![format!(
        "SPEC\t{label}\t{n} states\t{cut_states} cut\t{sure} sure\t{} bound",
        groups.len() - sure
    )];
    for ((place, lost, bound), (count, example)) in &groups {
        let witness = (0..n)
            .filter(|&v| w.states[v].status == *place && reach[v] & lost != 0)
            .min_by_key(|&v| w.depth[v])
            .unwrap();
        lines.push(format!(
            "LOST\t{label}\t{}\t{place}\t{}\t{count}\t{}\t{}\t{}\t{}",
            if *bound { "bound" } else { "sure" },
            names(&model, *lost),
            w.states[*example],
            path_to(&w, *example),
            w.states[witness],
            path_to(&w, witness),
        ));
    }
    lines
}
