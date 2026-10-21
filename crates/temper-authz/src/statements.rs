//! Cedar statements of a policy text, compared by meaning rather than bytes.
//!
//! A statement's identity is its parsed EST JSON, so the same policy written
//! with other whitespace, line endings or comments is the same statement.
//! Removal cuts each statement's source span and keeps every other byte, so
//! comments and layout around the remaining statements survive.

use std::collections::BTreeSet;
use std::ops::Range;

use cedar_policy::{Policy, Template};
use cedar_policy_core::parser::text_to_cst::parse_policies;

use crate::AuthzError;

/// The parsed identity of one Cedar statement (static policy or template).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatementKey(String);

/// Every statement of `text` with its source span, in source order.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn policy_statements(text: &str) -> Result<Vec<(Range<usize>, StatementKey)>, AuthzError> {
    let cst = parse_policies(text).map_err(|error| AuthzError::PolicyParse(error.to_string()))?;
    let Some(policies) = cst.node else {
        return Ok(Vec::new());
    };
    let mut statements = Vec::with_capacity(policies.0.len());
    for policy in &policies.0 {
        let loc = policy.loc.as_ref().ok_or_else(|| {
            AuthzError::PolicyParse("a parsed statement has no source location".into())
        })?;
        let span = loc.start()..loc.end();
        assert!(span.end <= text.len(), "statement span outside its text");
        let key = statement_key(&text[span.clone()])?;
        statements.push((span, key));
    }
    Ok(statements)
}

/// The identities of every statement in `text`.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn statement_keys(text: &str) -> Result<BTreeSet<StatementKey>, AuthzError> {
    Ok(policy_statements(text)?
        .into_iter()
        .map(|(_, key)| key)
        .collect())
}

/// `text` without the statements whose identity is in `remove`.
///
/// Each removed statement takes the line break that followed it; all other
/// bytes stay as they were.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn remove_statements(
    text: &str,
    remove: &BTreeSet<StatementKey>,
) -> Result<String, AuthzError> {
    if remove.is_empty() {
        return Ok(text.to_string());
    }
    let mut kept = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut removed = false;
    for (span, key) in policy_statements(text)? {
        if !remove.contains(&key) {
            continue;
        }
        kept.push_str(&text[cursor..span.start]);
        cursor = span.end;
        for line_break in ["\r\n", "\n"] {
            if text[cursor..].starts_with(line_break) {
                cursor += line_break.len();
                break;
            }
        }
        removed = true;
    }
    if !removed {
        return Ok(text.to_string());
    }
    kept.push_str(&text[cursor..]);
    debug_assert!(kept.len() < text.len());
    Ok(kept)
}

/// Whether `text` compiles as a whole policy set, as the engine loads it.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] with Cedar's reason when it does not.
pub fn check_compiles(text: &str) -> Result<(), AuthzError> {
    text.parse::<cedar_policy::PolicySet>()
        .map(|_| ())
        .map_err(|error| AuthzError::PolicyParse(error.to_string()))
}

/// The annotation that names where a statement came from, so a decision can
/// say which rule decided it (see [`label_statements`]).
pub const SOURCE_ANNOTATION: &str = "source";

/// `text` with every statement annotated `@source("{label}#{n}")`, `n` the
/// statement's 1-based position in `text`, so a denial reads
/// `katagami-commons/art_style.cedar#2` instead of a positional `policy1874`.
/// The annotation travels with the text into every store and reload; Cedar
/// keeps it on the parsed policy. A statement that already has a source
/// annotation keeps it. Every other byte stays as it was.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn label_statements(text: &str, label: &str) -> Result<String, AuthzError> {
    let label = label.replace('\\', "\\\\").replace('"', "\\\"");
    let mut labelled = String::with_capacity(text.len() + 64);
    let mut cursor = 0;
    for (index, (span, _)) in policy_statements(text)?.into_iter().enumerate() {
        if has_source_annotation(&text[span.clone()]) {
            continue;
        }
        labelled.push_str(&text[cursor..span.start]);
        labelled.push_str(&format!(
            "@{SOURCE_ANNOTATION}(\"{label}#{}\")\n",
            index + 1
        ));
        cursor = span.start;
    }
    labelled.push_str(&text[cursor..]);
    Ok(labelled)
}

fn has_source_annotation(statement: &str) -> bool {
    match Policy::parse(None, statement) {
        Ok(policy) => policy.annotation(SOURCE_ANNOTATION).is_some(),
        Err(_) => Template::parse(None, statement)
            .is_ok_and(|template| template.annotation(SOURCE_ANNOTATION).is_some()),
    }
}

fn statement_key(source: &str) -> Result<StatementKey, AuthzError> {
    let parse_error = |error: &dyn std::fmt::Display| AuthzError::PolicyParse(error.to_string());
    // A statement with slots parses only as a template, one without only as a policy.
    let json = match Policy::parse(None, source) {
        Ok(policy) => policy.to_json().map_err(|error| parse_error(&error))?,
        Err(policy_error) => Template::parse(None, source)
            .map_err(|_| parse_error(&policy_error))?
            .to_json()
            .map_err(|error| parse_error(&error))?,
    };
    Ok(StatementKey(json.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBID: &str = "forbid(principal, action == Action::\"Withdraw\", resource is Thing);";
    const PERMIT: &str = "permit(principal, action, resource is Thing);";

    #[test]
    fn labels_each_statement_with_its_source_and_position() {
        let text = format!("// rules\n{PERMIT}\n\n{FORBID}\n");
        let labelled = label_statements(&text, "bank/policies.cedar").unwrap();
        assert_eq!(
            labelled,
            format!(
                "// rules\n@source(\"bank/policies.cedar#1\")\n{PERMIT}\n\n@source(\"bank/policies.cedar#2\")\n{FORBID}\n"
            )
        );
        // Already labelled statements are left as they are.
        assert_eq!(
            label_statements(&labelled, "other.cedar").unwrap(),
            labelled
        );
        let set: cedar_policy::PolicySet = labelled.parse().unwrap();
        let mut sources: Vec<&str> = set
            .policies()
            .filter_map(|policy| policy.annotation(SOURCE_ANNOTATION))
            .collect();
        sources.sort_unstable();
        assert_eq!(sources, ["bank/policies.cedar#1", "bank/policies.cedar#2"]);
    }

    #[test]
    fn same_statement_in_other_layout_has_the_same_key() {
        let reformatted = "// frozen\r\nforbid(\r\n  principal,\r\n  action == Action::\"Withdraw\",\r\n  resource is Thing\r\n);";
        assert_eq!(
            statement_keys(FORBID).unwrap(),
            statement_keys(reformatted).unwrap()
        );
        assert_ne!(
            statement_keys(FORBID).unwrap(),
            statement_keys(PERMIT).unwrap()
        );
    }

    #[test]
    fn removes_only_the_named_statements() {
        let text = format!(
            "// keep me\n{PERMIT}\nforbid(\r\n  principal,\r\n  action == Action::\"Withdraw\",\r\n  resource is Thing\r\n);\r\n// trailing\n"
        );
        let remove = statement_keys(FORBID).unwrap();
        let stripped = remove_statements(&text, &remove).unwrap();
        assert_eq!(stripped, format!("// keep me\n{PERMIT}\n// trailing\n"));
        assert!(statement_keys(&stripped).unwrap().is_disjoint(&remove));
    }

    #[test]
    fn text_without_the_statement_is_unchanged() {
        let remove = statement_keys(FORBID).unwrap();
        assert_eq!(remove_statements(PERMIT, &remove).unwrap(), PERMIT);
        assert_eq!(remove_statements("", &remove).unwrap(), "");
        assert!(remove_statements("permit(", &remove).is_err());
    }
}
