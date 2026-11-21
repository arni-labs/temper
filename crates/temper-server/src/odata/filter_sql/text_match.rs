//! Candidate pushdown for substring filters over catalog text fields.

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::persistence::CatalogTextMatch;

use super::event_time::collect_conjuncts;
use super::field_compare::DeclaredFields;

pub(in crate::odata) struct SplitTextMatches {
    pub(in crate::odata) groups: Vec<Vec<CatalogTextMatch>>,
    pub(in crate::odata) rest: Option<FilterExpr>,
}

/// Push a positive contains test, or an OR made entirely of such tests, as
/// a whole. An unsupported alternative could satisfy that conjunct on its
/// own, so it leaves the entire OR for the ordinary row evaluator.
pub(in crate::odata) fn split_text_matches(
    filter: &FilterExpr,
    declared: &DeclaredFields,
    defaults: &serde_json::Value,
) -> SplitTextMatches {
    let mut conjuncts = Vec::new();
    collect_conjuncts(filter, &mut conjuncts);
    let mut groups = Vec::new();
    let mut rest = None;
    for conjunct in conjuncts {
        let mut alternatives = Vec::new();
        if collect_matches(conjunct, declared, defaults, &mut alternatives).is_some() {
            groups.push(alternatives);
        } else {
            rest = Some(match rest {
                Some(left) => FilterExpr::BinaryOp {
                    left: Box::new(left),
                    op: BinaryOperator::And,
                    right: Box::new(conjunct.clone()),
                },
                None => conjunct.clone(),
            });
        }
    }
    SplitTextMatches { groups, rest }
}

fn collect_matches(
    expr: &FilterExpr,
    declared: &DeclaredFields,
    defaults: &serde_json::Value,
    alternatives: &mut Vec<CatalogTextMatch>,
) -> Option<()> {
    match expr {
        FilterExpr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            collect_matches(left, declared, defaults, alternatives)?;
            collect_matches(right, declared, defaults, alternatives)
        }
        FilterExpr::FunctionCall { name, args } if name == "contains" => {
            let [
                FilterExpr::Property(field),
                FilterExpr::Literal(ODataValue::String(needle)),
            ] = args.as_slice()
            else {
                return None;
            };
            if !declared.is_text(field) || needle.contains('\0') {
                return None;
            }
            alternatives.push(CatalogTextMatch {
                field: field.clone(),
                needle: needle.clone(),
                or_missing: defaults
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| value.contains(needle)),
            });
            Some(())
        }
        _ => None,
    }
}
