//! Ranking records by their meaning rows.
//!
//! Ranking is deterministic: rows come from the store in id order, closeness
//! accumulates in f64 in a fixed order, and ties break by id. So a ranking is the
//! same on every store and in simulation.

use std::cmp::Ordering;

use temper_runtime::persistence::{MeaningRow, PersistenceError, fold};

use crate::storage::BoxedEventStore;

/// How many meaning rows one store read returns. A ranking reads a type page by
/// page and keeps only each row's id and score, so it holds one page of texts at a
/// time however many records the type has.
const PAGE: usize = 1000;

/// A record's place in a ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// The record.
    pub entity_id: String,
    /// Whether its text holds every searched word.
    pub words: bool,
    /// How close its meaning is to what was asked, from -1 to 1, when both have one.
    pub score: Option<f32>,
}

/// Cosine closeness of two vectors, or None when their lengths differ.
pub fn closeness(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut a_norm, mut b_norm) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        a_norm += x * x;
        b_norm += y * y;
    }
    if a_norm == 0.0 || b_norm == 0.0 {
        return None;
    }
    Some((dot / (a_norm.sqrt() * b_norm.sqrt())) as f32)
}

/// The searched words, folded: a record matches when its folded text holds each.
pub fn search_words(search: &str) -> Vec<String> {
    fold(search)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Records first that hold every word, then the closer in meaning, then by id.
fn order(a: &Ranked, b: &Ranked) -> Ordering {
    b.words
        .cmp(&a.words)
        .then_with(|| match (a.score, b.score) {
            (Some(a), Some(b)) => b.total_cmp(&a),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.entity_id.cmp(&b.entity_id))
}

/// Every record of `entity_type` ranked for a search: those whose text holds all of
/// `words`, and, when `query` is the search's vector by model `model_tag`, every
/// record with a vector by that model. A record that is neither is left out.
pub async fn rank_search(
    store: &BoxedEventStore,
    tenant: &str,
    entity_type: &str,
    words: &[String],
    query: Option<(&[f32], &str)>,
) -> Result<Vec<Ranked>, PersistenceError> {
    rank_rows(store, tenant, entity_type, |row| {
        let holds = !words.is_empty() && {
            let text = fold(&row.text);
            words.iter().all(|word| text.contains(word.as_str()))
        };
        let score = query.and_then(|(vector, tag)| meaning_score(row, vector, tag));
        (holds || score.is_some()).then(|| Ranked {
            entity_id: row.entity_id.clone(),
            words: holds,
            score,
        })
    })
    .await
}

/// Every other record of `entity_type` with a vector by `model_tag`, closest to
/// `vector` first.
pub async fn rank_nearest(
    store: &BoxedEventStore,
    tenant: &str,
    entity_type: &str,
    vector: &[f32],
    model_tag: &str,
    exclude: &str,
) -> Result<Vec<Ranked>, PersistenceError> {
    rank_rows(store, tenant, entity_type, |row| {
        if row.entity_id == exclude {
            return None;
        }
        meaning_score(row, vector, model_tag).map(|score| Ranked {
            entity_id: row.entity_id.clone(),
            words: false,
            score: Some(score),
        })
    })
    .await
}

fn meaning_score(row: &MeaningRow, vector: &[f32], model_tag: &str) -> Option<f32> {
    if row.model_tag != model_tag {
        return None;
    }
    closeness(row.vector.as_deref()?, vector)
}

async fn rank_rows(
    store: &BoxedEventStore,
    tenant: &str,
    entity_type: &str,
    mut place: impl FnMut(&MeaningRow) -> Option<Ranked>,
) -> Result<Vec<Ranked>, PersistenceError> {
    let mut ranked = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let rows = store
            .meaning_rows(tenant, entity_type, after.as_deref(), PAGE)
            .await?;
        ranked.extend(rows.iter().filter_map(&mut place));
        match rows.last() {
            Some(last) if rows.len() == PAGE => after = Some(last.entity_id.clone()),
            _ => break,
        }
    }
    ranked.sort_by(order);
    Ok(ranked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(id: &str, words: bool, score: Option<f32>) -> Ranked {
        Ranked {
            entity_id: id.to_string(),
            words,
            score,
        }
    }

    #[test]
    fn word_matches_come_first_then_meaning_then_id() {
        let mut list = vec![
            ranked("d", false, None),
            ranked("c", false, Some(0.9)),
            ranked("b", true, Some(0.1)),
            ranked("a", true, None),
            ranked("e", true, Some(0.1)),
        ];
        list.sort_by(order);
        let ids: Vec<&str> = list.iter().map(|r| r.entity_id.as_str()).collect();
        assert_eq!(ids, ["b", "e", "a", "c", "d"]);
    }

    #[test]
    fn closeness_is_cosine() {
        assert_eq!(closeness(&[1.0, 0.0], &[2.0, 0.0]), Some(1.0));
        assert_eq!(closeness(&[1.0, 0.0], &[0.0, 3.0]), Some(0.0));
        assert_eq!(closeness(&[1.0], &[1.0, 0.0]), None);
        assert_eq!(closeness(&[0.0, 0.0], &[1.0, 0.0]), None);
    }

    #[test]
    fn words_fold_case_and_accents() {
        assert_eq!(search_words("  Café  NOIR "), ["cafe", "noir"]);
    }
}
