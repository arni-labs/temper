//! Reviewing one module: rate every item in each window, then ask where each item found in a window shows in it (a
//! part, then a line). Jev rates and chooses; it writes nothing, so each finding's words are its item's own with the
//! app's names and the line filled in. Each window is judged on what it reads alone, so a window as it was before gets
//! the same findings, wherever the rest of the module changed.

use std::collections::BTreeMap;

use futures::future::{join_all, try_join_all};

use super::excerpt::{excerpt, renamed};
use super::items::{DOES_ITEM, DOES_KEY, DOES_MATCHES_AT, FOUND_AT, ITEMS, Item, LARGE_LINES};
use super::questions::{
    item_questions, line_key, line_questions, review_state, runners, runs_from, where_key,
    where_questions,
};
use super::source::{Line, Part, calls_out, from_one, lines_of, parts_of, said, windows_of};
use super::types::{
    Answers, Finding, Judge, JudgeUnavailable, ModuleReview, ModuleSource, SpecSource,
};

/// An item Jev rated at or above [`FOUND_AT`] in one window, which is where it is looked for.
struct Found {
    item: &'static Item,
    rating: f64,
    window: usize,
}

/// One window of the module as Jev reads it: its lines numbered from 1, and how many lines of the module come before.
struct Window {
    before: usize,
    lines: Vec<Line>,
}

/// Review one module against the specs of its app. `names` gives each of the app's modules by the name the specs run
/// it by, and the source file the review names it by.
///
/// # Errors
///
/// [`JudgeUnavailable`] when Jev gave no rating. Once the items are rated, a Jev that does not say where one shows
/// leaves that finding without a line rather than failing the review.
pub async fn review_module(
    judge: &dyn Judge,
    module: &ModuleSource,
    specs: &[SpecSource],
    names: &BTreeMap<String, String>,
) -> Result<ModuleReview, JudgeUnavailable> {
    let lines = lines_of(&module.source);
    let runners = runners(&module.module, specs);
    let mut read = excerpt(&module.module, specs);
    read.text = renamed(&read.text, names);
    let windows: Vec<Window> = windows_of(&lines)
        .iter()
        .map(|window| {
            let (before, lines) = from_one(window);
            Window { before, lines }
        })
        .collect();
    let split = windows.len() > 1;
    let states: Vec<String> = windows
        .iter()
        .map(|window| review_state(&module.path, &window.lines, &read, split))
        .collect();
    debug_assert_eq!(states.len(), windows.len());

    let questions = item_questions(module.does.as_deref());
    let ratings = try_join_all(
        states
            .iter()
            .map(|state| judge.ask(&module.module, state, &questions)),
    )
    .await?;
    let found = found_items(&ratings, calls_out(&module.source));
    let (placed, placing_tokens) = place(judge, &module.module, &windows, &states, &found).await;

    let runs = runs_from(&runners);
    let mut findings: Vec<Finding> = found
        .iter()
        .map(|found| {
            let line = placed
                .iter()
                .find(|(window, number, _)| *window == found.window && *number == found.item.number)
                .map(|(_, _, line)| line);
            finding(found.item, found.rating, &module.path, line, &runs)
        })
        .collect();
    if let Some(does) = &module.does
        && let Some(found) = does_mismatch(does, &ratings, &module.path, &runs)
    {
        findings.push(found);
    }
    let size = said(&lines);
    if size > LARGE_LINES {
        findings.push(large(size, &lines, &module.path, &runs));
    }
    let input_tokens = ratings
        .iter()
        .map(|answers| answers.input_tokens)
        .sum::<u64>()
        + placing_tokens;
    Ok(ModuleReview {
        module: module.module.clone(),
        findings,
        input_tokens,
        left_out: read.left_out,
    })
}

/// Each item rated at or above [`FOUND_AT`] in a window, once for each such window, item by item. An item only an
/// outside call can show is never found in a module that makes none: Jev reading a window of pure math has rated a
/// bisection closure as several steps (Axia's value module, 21 Nov 2026), and no rating can make code that calls
/// nothing call two services.
fn found_items(ratings: &[Answers], calls_out: bool) -> Vec<Found> {
    ITEMS
        .iter()
        .filter(|item| calls_out || !item.needs_a_call)
        .flat_map(|item| {
            ratings
                .iter()
                .enumerate()
                .filter_map(move |(window, answers)| {
                    let rating = answers.rating(item.kind)?;
                    (rating >= FOUND_AT).then_some(Found {
                        item,
                        rating,
                        window,
                    })
                })
        })
        .collect()
}

/// The line where each found item shows, by window and item number, numbered in the module, and the tokens asking
/// cost. Windows are asked at once.
async fn place(
    judge: &dyn Judge,
    module: &str,
    windows: &[Window],
    states: &[String],
    found: &[Found],
) -> (Vec<(usize, u8, Line)>, u64) {
    let asks = windows.iter().enumerate().filter_map(|(index, window)| {
        let here: Vec<&'static Item> = found
            .iter()
            .filter(|f| f.window == index)
            .map(|f| f.item)
            .collect();
        (!here.is_empty()).then_some(async move {
            let (lines, cost) =
                place_in_window(judge, module, &window.lines, &states[index], here).await;
            let lines: Vec<(usize, u8, Line)> = lines
                .into_iter()
                .map(|(item, line)| {
                    let number = line.number + window.before;
                    (index, item, Line { number, ..line })
                })
                .collect();
            (lines, cost)
        })
    });
    let mut placed = Vec::new();
    let mut tokens = 0;
    for (lines, cost) in join_all(asks).await {
        placed.extend(lines);
        tokens += cost;
    }
    (placed, tokens)
}

/// Which part of one window shows each item, then which line of that part. A Jev that stops answering here leaves the
/// items unplaced: they are known, only where they show is not, and a guessed line would mislead.
async fn place_in_window(
    judge: &dyn Judge,
    module: &str,
    window: &[Line],
    state: &str,
    items: Vec<&'static Item>,
) -> (Vec<(u8, Line)>, u64) {
    let parts = parts_of(window);
    let Some(first_part) = parts.first() else {
        return (Vec::new(), 0);
    };
    let mut tokens = 0;
    let where_answers = if parts.len() > 1 {
        match judge
            .ask(module, state, &where_questions(&items, &parts))
            .await
        {
            Ok(answers) => answers,
            Err(_) => return (Vec::new(), 0),
        }
    } else {
        Answers::default()
    };
    tokens += where_answers.input_tokens;
    let chosen: Vec<(&Item, &Part)> = items
        .iter()
        .map(|item| {
            let key = where_answers.choice(&where_key(item));
            let part = parts
                .iter()
                .find(|part| Some(part.key.as_str()) == key)
                .unwrap_or(first_part);
            (*item, part)
        })
        .collect();
    let to_choose: Vec<(&Item, &Part)> = chosen
        .iter()
        .copied()
        .filter(|(_, part)| part.lines.len() > 1)
        .collect();
    let line_answers = if to_choose.is_empty() {
        Answers::default()
    } else {
        match judge.ask(module, state, &line_questions(&to_choose)).await {
            Ok(answers) => answers,
            Err(_) => return (Vec::new(), tokens),
        }
    };
    tokens += line_answers.input_tokens;
    let placed = chosen
        .iter()
        .filter_map(|(item, part)| {
            let key = line_answers.choice(&line_key(item));
            let line = part
                .lines
                .iter()
                .find(|line| Some(format!("L{}", line.number).as_str()) == key);
            Some((item.number, line.or(part.lines.first())?.clone()))
        })
        .collect();
    (placed, tokens)
}

fn finding(item: &Item, rating: f64, path: &str, line: Option<&Line>, runs: &str) -> Finding {
    debug_assert!((0.0..=1.0).contains(&rating), "a rating is 0..1");
    Finding {
        item: item.number,
        kind: item.kind.to_string(),
        title: item.title.to_string(),
        rating: (rating * 100.0).round() / 100.0,
        location: line.map_or_else(
            || path.to_string(),
            |line| format!("{path}:{}", line.number),
        ),
        number: line.map(|line| line.number),
        line: line.map(|line| line.text.trim().to_string()),
        why: format!("In this app the module runs from {runs}; {}.", item.why),
        instead: format!("Instead: {}.", item.instead),
    }
}

/// Item 7 by size: a module this long is read as several concerns until its author says why it is not states.
fn large(size: usize, lines: &[Line], path: &str, runs: &str) -> Finding {
    let several_concerns = &ITEMS[6];
    debug_assert_eq!(several_concerns.number, 7);
    let branches = lines.iter().filter(|line| branches_on(&line.text)).count();
    Finding {
        kind: "large_module".to_string(),
        title: "A large module".to_string(),
        why: format!(
            "It is {size} lines with {branches} branches; a module this long almost always holds steps and decisions \
             the record's states should show."
        ),
        instead: format!(
            "Instead: {}, or say why it can't be split into states.",
            several_concerns.instead
        ),
        ..finding(several_concerns, 1.0, path, None, runs)
    }
}

/// The module's sentence as a finding when no window of its code reads as doing it: its highest rating is under
/// [`DOES_MATCHES_AT`]. A window is part of the module, so one window that does what the sentence says is enough. None
/// when Jev rated it in no window: the sentence is unknown, not wrong.
fn does_mismatch(does: &str, ratings: &[Answers], path: &str, runs: &str) -> Option<Finding> {
    let rating = ratings
        .iter()
        .filter_map(|answers| answers.rating(DOES_KEY))
        .max_by(f64::total_cmp)?;
    debug_assert!((0.0..=1.0).contains(&rating), "a rating is 0..1");
    (rating < DOES_MATCHES_AT).then(|| Finding {
        item: DOES_ITEM,
        kind: "does_mismatch".to_string(),
        title: "A sentence that is not what the module does".to_string(),
        rating: (rating * 100.0).round() / 100.0,
        location: path.to_string(),
        number: None,
        line: None,
        why: format!(
            "In this app the module runs from {runs}; its sentence says \"{does}\", and Jev does not read its code as \
             doing that, so whoever reads the app is told something the module does not do."
        ),
        instead: "Instead: say what the module computes, and from what, as its code does it, or make the code do what \
                  the sentence says."
            .to_string(),
    })
}

/// Whether a line branches or loops (`if`, `match`, `while`, `loop`, `for` as words).
fn branches_on(text: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| matches!(word, "if" | "match" | "while" | "loop" | "for"))
}
