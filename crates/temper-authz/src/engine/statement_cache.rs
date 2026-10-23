//! A tenant's Cedar kept parsed one statement at a time (ADR-0183).
//!
//! A reload cuts the new text into statements at each top-level `;` (outside
//! strings and `//` comments, the only places Cedar allows one), and parses
//! only the statements whose text it has not parsed before. The rest come from
//! the previous reload, so changing one statement in a tenant parses one
//! statement. A statement keyed by its exact text parses to the same policy
//! every time, so a cached statement is never stale.
//!
//! Text the cut cannot read as plain statements (a template, a syntax error,
//! an unterminated string) is parsed whole, as before, and is not cached.

use std::collections::HashMap;
use std::sync::Arc;

use cedar_policy::{Policy, PolicySet};

use super::group_candidates::StatementGroups;
use crate::error::AuthzError;

/// One parsed statement and the groups it can ask about.
#[derive(Debug)]
pub(super) struct ParsedStatement {
    pub(super) policy: Policy,
    pub(super) groups: StatementGroups,
}

/// The statements of a tenant's last reload, by their exact text.
#[derive(Debug, Default)]
pub(super) struct StatementCache {
    by_text: HashMap<String, Arc<ParsedStatement>>,
}

/// One policy text, parsed.
pub(super) enum ParsedText {
    /// Plain statements in source order.
    Statements(Vec<Arc<ParsedStatement>>),
    /// Text parsed whole: it has a template, or does not cut into statements.
    Whole(PolicySet),
}

impl StatementCache {
    /// Parse `text`, taking every statement already in `self` from it, and
    /// record its statements in `next`. Returns how many statements were parsed.
    ///
    /// # Errors
    ///
    /// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
    pub(super) fn parse(
        &self,
        text: &str,
        next: &mut StatementCache,
    ) -> Result<(ParsedText, usize), AuthzError> {
        if let Some(chunks) = split_statements(text) {
            let mut statements = Vec::with_capacity(chunks.len());
            let mut fresh = Vec::new();
            let mut parsed = 0;
            for chunk in chunks {
                let known = self
                    .by_text
                    .get(chunk)
                    .or_else(|| next.by_text.get(chunk))
                    .cloned();
                let statement = match known {
                    Some(statement) => statement,
                    None => match Policy::parse(None, chunk) {
                        Ok(policy) => {
                            parsed += 1;
                            let groups = StatementGroups::of(&policy);
                            Arc::new(ParsedStatement { policy, groups })
                        }
                        // A template or an error: the whole-text parse decides.
                        Err(_) => return Self::parse_whole(text),
                    },
                };
                fresh.push((chunk, Arc::clone(&statement)));
                statements.push(statement);
            }
            for (chunk, statement) in fresh {
                next.by_text.insert(chunk.to_string(), statement);
            }
            return Ok((ParsedText::Statements(statements), parsed));
        }
        Self::parse_whole(text)
    }

    fn parse_whole(text: &str) -> Result<(ParsedText, usize), AuthzError> {
        let set = text
            .parse::<PolicySet>()
            .map_err(|error| AuthzError::PolicyParse(error.to_string()))?;
        let count = set.policies().count() + set.templates().count();
        Ok((ParsedText::Whole(set), count))
    }

    /// Statements held, for the engine's memory figure.
    pub(super) fn len(&self) -> usize {
        self.by_text.len()
    }
}

/// `text` cut after each top-level `;`, each statement without the whitespace
/// and comments before it, so a statement is one key wherever it sits. `None`
/// when a string is unterminated or anything but whitespace and comments
/// follows the last `;`.
fn split_statements(text: &str) -> Option<Vec<&str>> {
    let bytes = text.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if i >= bytes.len() {
                    return None;
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b';' => {
                i += 1;
                chunks.push(trim_trivia_start(&text[start..i]));
                start = i;
            }
            _ => i += 1,
        }
    }
    only_trivia(&text[start..]).then_some(chunks)
}

fn only_trivia(rest: &str) -> bool {
    trim_trivia_start(rest).is_empty()
}

fn trim_trivia_start(mut rest: &str) -> &str {
    loop {
        rest = rest.trim_start();
        if !rest.starts_with("//") {
            return rest;
        }
        rest = rest.find('\n').map_or("", |end| &rest[end..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_cut_at_top_level_semicolons_only() {
        let text = "// a; comment\n@source(\"x;y\") permit(principal, action, resource) when { resource.s == \"a\\\";b\" };\n forbid(principal, action, resource); // tail;\n";
        let chunks = split_statements(text).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("@source(\"x;y\")") && chunks[0].ends_with("};"));
        assert_eq!(chunks[1], "forbid(principal, action, resource);");
        assert_eq!(
            split_statements("permit(principal, action, resource)"),
            None
        );
        assert_eq!(
            split_statements("permit(principal, action, resource) when { \"a;"),
            None
        );
        assert_eq!(split_statements("  // only a comment\n"), Some(vec![]));
    }
}
