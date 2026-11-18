//! A record's meaning row (ADR-0186).
//!
//! Every store keeps one row per record beside its query-plane row, written in the same
//! commit: the record's text, by the one general rule [`record_text`], and its hash. When
//! the text changes the row's vector is cleared, and the kernel's model fills it after the
//! save ([`EventStore::meaning_pending`], [`EventStore::set_meaning_vector`]). Search reads
//! the rows: the words for word match, the vector for meaning.
//!
//! [`EventStore::meaning_pending`]: super::EventStore::meaning_pending
//! [`EventStore::set_meaning_vector`]: super::EventStore::set_meaning_vector

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Which version of [`record_text`] wrote a type's rows. A backfill watermark naming
/// another version re-derives every record's text.
pub const TEXT_RULE: &str = "text-v1";

/// A record's text: every non-empty string among its query-plane fields (nested values
/// included), in the order the fields' map keeps them, one per line. Numbers and flags
/// are not text, nor are the fields the kernel writes (the id, the status, event
/// metadata), which `$filter` reads. A field a spec keeps out of the query plane
/// (`query_indexed = false`) is not in `fields`, so it is not searched either.
pub fn record_text(fields: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    match fields {
        serde_json::Value::Object(map) => map
            .iter()
            .filter(|(name, _)| !temper_spec::automaton::is_server_derived_field_name(name))
            .for_each(|(_, value)| collect_strings(value, &mut lines)),
        other => collect_strings(other, &mut lines),
    }
    lines.join("\n")
}

fn collect_strings<'a>(value: &'a serde_json::Value, lines: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() {
                lines.push(text);
            }
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|item| collect_strings(item, lines))
        }
        serde_json::Value::Object(map) => {
            map.values().for_each(|item| collect_strings(item, lines))
        }
        _ => {}
    }
}

/// The hash a meaning row keeps of its text: a write whose text hashes the same leaves
/// the row (and its vector) as it is.
pub fn text_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The text the model reads: [`record_text`] without the words that hold a digit (ids,
/// hashes, dates, numbers, stored vectors). They carry no meaning to the model and would
/// fill the 256 pieces it reads; word match still finds them.
pub fn meaning_input(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .filter(|word| !word.chars().any(char::is_numeric))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text folded for word match: compatibility-decomposed, accents dropped, lower case. A
/// search word matches a record when its folded form appears in the record's folded text.
pub fn fold(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// A meaning row as search reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct MeaningRow {
    /// The record.
    pub entity_id: String,
    /// Its text, by [`record_text`].
    pub text: String,
    /// The model that made `vector`; empty while the vector waits to be made.
    pub model_tag: String,
    /// Its meaning, or None while it waits for the model.
    pub vector: Option<Vec<f32>>,
}

/// A meaning row that waits for the model.
#[derive(Debug, Clone, PartialEq)]
pub struct MeaningPending {
    /// The record's type.
    pub entity_type: String,
    /// The record.
    pub entity_id: String,
    /// Its text.
    pub text: String,
    /// The hash of its text, so a vector made from an older text is not kept.
    pub text_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_takes_every_string_in_field_order() {
        let fields = serde_json::json!({
            "Id": "trip-1",
            "Status": "Draft",
            "blank": "  ",
            "count": 3,
            "done": false,
            "notes": ["pack the tent", {"who": "Ana"}],
            "title": "Ferry to the coast",
        });
        assert_eq!(
            record_text(&fields),
            "pack the tent\nAna\nFerry to the coast"
        );
    }

    #[test]
    fn the_model_reads_no_digit_words() {
        assert_eq!(
            meaning_input("Ferry r_9u9n7sz at 10:30\n2026 0.12,0.4"),
            "Ferry at"
        );
    }

    #[test]
    fn folding_drops_case_and_accents() {
        assert_eq!(fold("Árbol ÉCOLE Straße ﬁle"), "arbol ecole straße file");
    }
}
