//! Event-time range conjuncts of a `$filter`, read as bounds on the catalog's
//! event-time key (ADR-0187).
//!
//! `last_event_at gt '2026-11-19T10:00:00Z'` compares instants in memory
//! (`query_eval` normalizes both sides), and a record with no event time
//! meets no comparison. A bound on the key the stores index compares the
//! same way, so a top-level conjunct of that shape is exact in SQL.

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::persistence::{EventTimeBound, EventTimeColumn, KeyComparison, event_time_key};

/// A filter's top-level `and` conjuncts, split into the event-time bounds
/// and the rest.
pub(in crate::odata) struct SplitFilter {
    pub(in crate::odata) bounds: Vec<EventTimeBound>,
    /// The other conjuncts, in their order; `None` when every one was a bound.
    pub(in crate::odata) rest: Option<FilterExpr>,
}

/// Split `filter` into event-time bounds and the conjuncts left over.
pub(in crate::odata) fn split_event_time_bounds(filter: &FilterExpr) -> SplitFilter {
    let mut conjuncts = Vec::new();
    collect_conjuncts(filter, &mut conjuncts);
    let mut bounds = Vec::new();
    let mut rest: Option<FilterExpr> = None;
    for conjunct in conjuncts {
        if let Some(bound) = event_time_bound(conjunct) {
            bounds.push(bound);
            continue;
        }
        rest = Some(match rest {
            Some(left) => FilterExpr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(conjunct.clone()),
            },
            None => conjunct.clone(),
        });
    }
    SplitFilter { bounds, rest }
}

/// The top-level `and` conjuncts of `filter`, in their order.
pub(super) fn collect_conjuncts<'a>(filter: &'a FilterExpr, out: &mut Vec<&'a FilterExpr>) {
    if let FilterExpr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = filter
    {
        collect_conjuncts(left, out);
        collect_conjuncts(right, out);
    } else {
        out.push(filter);
    }
}

/// `event_time op time`, either way round, as a bound. A literal that is not
/// an RFC 3339 time compares as text in memory, so it stays in the filter.
fn event_time_bound(conjunct: &FilterExpr) -> Option<EventTimeBound> {
    let FilterExpr::BinaryOp { left, op, right } = conjunct else {
        return None;
    };
    let comparison = match op {
        BinaryOperator::Eq => KeyComparison::Eq,
        BinaryOperator::Gt => KeyComparison::Gt,
        BinaryOperator::Ge => KeyComparison::Ge,
        BinaryOperator::Lt => KeyComparison::Lt,
        BinaryOperator::Le => KeyComparison::Le,
        _ => return None,
    };
    let (property, literal, comparison) = match (left.as_ref(), right.as_ref()) {
        (FilterExpr::Property(property), FilterExpr::Literal(literal)) => {
            (property, literal, comparison)
        }
        (FilterExpr::Literal(literal), FilterExpr::Property(property)) => {
            (property, literal, flipped(comparison))
        }
        _ => return None,
    };
    let column = EventTimeColumn::of(property)?;
    let key = match literal {
        ODataValue::String(text) => event_time_key(text)?,
        ODataValue::DateTimeOffset(time) => event_time_key(&time.to_rfc3339())?,
        _ => return None,
    };
    Some(EventTimeBound {
        column,
        comparison,
        key,
    })
}

/// `time op event_time` is `event_time flipped(op) time`.
fn flipped(comparison: KeyComparison) -> KeyComparison {
    match comparison {
        KeyComparison::Eq => KeyComparison::Eq,
        KeyComparison::Lt => KeyComparison::Gt,
        KeyComparison::Le => KeyComparison::Ge,
        KeyComparison::Gt => KeyComparison::Lt,
        KeyComparison::Ge => KeyComparison::Le,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(left: FilterExpr, op: BinaryOperator, right: FilterExpr) -> FilterExpr {
        FilterExpr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    fn property(name: &str) -> FilterExpr {
        FilterExpr::Property(name.to_string())
    }

    fn text(value: &str) -> FilterExpr {
        FilterExpr::Literal(ODataValue::String(value.to_string()))
    }

    #[test]
    fn top_level_event_time_comparisons_become_bounds_and_the_rest_stays() {
        let status = compare(property("status"), BinaryOperator::Eq, text("Open"));
        let since = compare(
            property("last_event_at"),
            BinaryOperator::Ge,
            text("2026-11-19T10:00:00.5Z"),
        );
        let until = compare(
            text("2026-11-20T00:00:00Z"),
            BinaryOperator::Gt,
            property("first_event_at"),
        );
        let filter = compare(
            compare(since, BinaryOperator::And, status.clone()),
            BinaryOperator::And,
            until,
        );
        let split = split_event_time_bounds(&filter);
        assert_eq!(
            split.bounds,
            vec![
                EventTimeBound {
                    column: EventTimeColumn::Last,
                    comparison: KeyComparison::Ge,
                    key: "2026-11-19T10:00:00.500000000".to_string(),
                },
                EventTimeBound {
                    column: EventTimeColumn::First,
                    comparison: KeyComparison::Lt,
                    key: "2026-11-20T00:00:00.000000000".to_string(),
                },
            ]
        );
        assert_eq!(split.rest, Some(status));
    }

    #[test]
    fn a_comparison_inside_an_or_or_against_other_text_is_not_a_bound() {
        let since = compare(
            property("last_event_at"),
            BinaryOperator::Gt,
            text("2026-11-19T10:00:00Z"),
        );
        let either = compare(
            since,
            BinaryOperator::Or,
            compare(property("status"), BinaryOperator::Eq, text("Open")),
        );
        let split = split_event_time_bounds(&either);
        assert!(split.bounds.is_empty());
        assert_eq!(split.rest, Some(either));

        let not_a_time = compare(property("last_event_at"), BinaryOperator::Gt, text("2026"));
        let split = split_event_time_bounds(&not_a_time);
        assert!(split.bounds.is_empty());
        assert_eq!(split.rest, Some(not_a_time));
    }
}
