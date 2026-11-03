//! `left in (v1, v2, ...)`: membership in a list of values.
//!
//! It parses to `left eq v1 or left eq v2 ...`, the expression it means, so
//! every reader of a filter (in-memory evaluation, SQL translation, index
//! pushdown) handles it as the equalities it already knows.

use super::{FilterParser, balance_associative};
use crate::error::ODataError;
use crate::query::types::{BinaryOperator, FilterExpr};

impl FilterParser<'_> {
    /// Parse the `in (...)` after `left`; the next token is `in`.
    pub(super) fn parse_in_list(&mut self, left: FilterExpr) -> Result<FilterExpr, ODataError> {
        let operator_offset = self.current_offset();
        self.budget.consume_operator(operator_offset)?;
        self.advance(); // `in`
        self.expect_text("(")?;

        // `left` is repeated once per value, so each copy is charged to the
        // node budget as the parser would have charged it written out.
        let left_nodes = node_count(&left);
        let mut equalities = Vec::new();
        loop {
            let value_offset = self.current_offset();
            self.budget.consume_argument(value_offset)?;
            self.descend()?;
            let value = self.parse_primary();
            self.ascend();
            let value = value?;
            if !equalities.is_empty() {
                for _ in 0..left_nodes {
                    self.budget.consume_node(value_offset)?;
                }
                self.budget.consume_operator(value_offset)?;
            }
            self.budget.consume_node(value_offset)?;
            equalities.push(FilterExpr::BinaryOp {
                left: Box::new(left.clone()),
                op: BinaryOperator::Eq,
                right: Box::new(value),
            });
            if self.peek_text_is(",") {
                self.advance();
            } else {
                break;
            }
        }
        self.expect_text(")")?;
        Ok(balance_associative(equalities, BinaryOperator::Or))
    }
}

fn node_count(expr: &FilterExpr) -> usize {
    match expr {
        FilterExpr::BinaryOp { left, right, .. } => 1 + node_count(left) + node_count(right),
        FilterExpr::UnaryOp { operand, .. } => 1 + node_count(operand),
        FilterExpr::FunctionCall { args, .. } => 1 + args.iter().map(node_count).sum::<usize>(),
        FilterExpr::Property(_) | FilterExpr::Literal(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse_filter;
    use crate::query::types::{BinaryOperator, FilterExpr, ODataValue};

    fn status_eq(value: &str) -> FilterExpr {
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(value.into()))),
        }
    }

    #[test]
    fn in_parses_to_the_equalities_it_means() {
        assert_eq!(
            parse_filter("Status in ('A', 'B')").unwrap(),
            parse_filter("Status eq 'A' or Status eq 'B'").unwrap()
        );
        assert_eq!(parse_filter("Status in ('A')").unwrap(), status_eq("A"));
        assert_eq!(
            parse_filter("Status in ('A','B') and Place eq 'x'").unwrap(),
            parse_filter("(Status eq 'A' or Status eq 'B') and Place eq 'x'").unwrap()
        );
    }

    #[test]
    fn in_needs_a_parenthesized_list_of_values() {
        assert!(parse_filter("Status in ()").is_err());
        assert!(parse_filter("Status in 'A'").is_err());
        assert!(parse_filter("Status in ('A',)").is_err());
        assert!(parse_filter("Status in ('A'").is_err());
    }
}
