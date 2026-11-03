//! Balanced trees for chains of `and` and `or`.

use crate::query::types::{BinaryOperator, FilterExpr};

/// Build an order-preserving balanced tree for an associative boolean operator.
///
/// A left fold makes an accepted wide filter's AST as deep as its width. Every
/// downstream consumer then inherits that attacker-controlled recursion depth,
/// including in-memory evaluation, SQL translation, and `Drop`. Pairing adjacent
/// operands keeps the same left-to-right order and boolean meaning while bounding
/// tree depth logarithmically.
pub(super) fn balance_associative(mut operands: Vec<FilterExpr>, op: BinaryOperator) -> FilterExpr {
    assert!(
        !operands.is_empty(),
        "boolean chain must contain an operand"
    );

    while operands.len() > 1 {
        let mut next_level = Vec::with_capacity(operands.len().div_ceil(2));
        let mut iter = operands.into_iter();
        while let Some(left) = iter.next() {
            if let Some(right) = iter.next() {
                next_level.push(FilterExpr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                });
            } else {
                next_level.push(left);
            }
        }
        operands = next_level;
    }

    debug_assert_eq!(operands.len(), 1, "balancing must produce one root");
    operands.remove(0)
}
