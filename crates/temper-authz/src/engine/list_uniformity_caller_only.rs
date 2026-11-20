//! Policies a list read can leave out: those whose caller-only part already
//! rules out this caller ([`can_hold_for_some_row`]).

use cedar_policy::Policy;

use super::{CallerOnly, collect_resource_attrs, json_contains_var};

/// Whether some row could satisfy `policy`, judged from the parts of its
/// conditions that read neither the row nor a context key a row can fill.
///
/// A policy is satisfied only when every `when` body is true and every
/// `unless` body is false. So when a top-level conjunct of a `when` body is
/// false for this caller (or errors, which Cedar skips), or a top-level
/// disjunct of an `unless` body is true (or errors), no row satisfies it:
/// the policy neither permits nor forbids anything in this read. Such a part
/// has the same value for every row, so it is evaluated once. `true` keeps
/// the policy whenever this cannot be shown.
///
/// Only a part Cedar reaches before any part that reads the row counts.
/// Cedar evaluates a policy's conditions in order and each chain left to
/// right, stopping at the first part that settles it; a part that reads the
/// row and comes first is evaluated for every row, and one that exhausts
/// Cedar's stack fails the whole check closed (`evaluate_request`). Leaving
/// such a policy out would let that row through, so it stays.
pub(super) fn can_hold_for_some_row(
    policy: &Policy,
    request_fills: &dyn Fn(&str) -> bool,
    decide: &CallerOnly<'_>,
) -> bool {
    let Ok(json) = policy.to_json() else {
        return true;
    };
    let Some(conditions) = json.get("conditions").and_then(|value| value.as_array()) else {
        return true;
    };
    for condition in conditions {
        let Some(body) = condition.get("body") else {
            return true;
        };
        let (operator, must_be) = match condition.get("kind").and_then(|kind| kind.as_str()) {
            Some("when") => ("&&", true),
            Some("unless") => ("||", false),
            _ => return true,
        };
        let mut parts = Vec::new();
        operands(body, operator, &mut parts);
        for part in parts {
            if !reads_only_the_request(part, request_fills) {
                // Every row evaluates this part before any later one.
                return true;
            }
            // `when { P }`: no row passes unless P is true. `unless { P }`:
            // no row passes unless P is false, so ask whether !P is true.
            let asked = if must_be {
                part.clone()
            } else {
                serde_json::json!({ "!": { "arg": part } })
            };
            let Some(guard) = caller_only_permit(asked) else {
                return true;
            };
            match decide(&guard) {
                Some(false) => return false,
                Some(true) => {}
                // The engine could not decide it, so nothing is concluded.
                None => return true,
            }
        }
    }
    true
}

/// The operands of a chain of one boolean operator (`&&` or `||`), left to
/// right, as Cedar's JSON form nests them.
fn operands<'a>(
    value: &'a serde_json::Value,
    operator: &str,
    out: &mut Vec<&'a serde_json::Value>,
) {
    match value.get(operator) {
        Some(node) => match (node.get("left"), node.get("right")) {
            (Some(left), Some(right)) => {
                operands(left, operator, out);
                operands(right, operator, out);
            }
            _ => out.push(value),
        },
        None => out.push(value),
    }
}

/// Whether an expression has the same value for every row of the read: it
/// names no resource, no row id and no context key a row's field can fill.
fn reads_only_the_request(value: &serde_json::Value, request_fills: &dyn Fn(&str) -> bool) -> bool {
    if json_contains_var(value, "resource") {
        return false;
    }
    let scan = collect_resource_attrs(value);
    !scan.unknown
        && !scan.identity
        && scan.keys.is_empty()
        && scan.context_keys.iter().all(|key| request_fills(key))
        && scan
            .nested_context_keys
            .iter()
            .all(|key| request_fills(key))
}

/// `permit(principal, action, resource) when { <condition> };`
fn caller_only_permit(condition: serde_json::Value) -> Option<Policy> {
    let json = serde_json::json!({
        "effect": "permit",
        "principal": { "op": "All" },
        "action": { "op": "All" },
        "resource": { "op": "All" },
        "conditions": [{ "kind": "when", "body": condition }],
    });
    Policy::from_json(None, json).ok()
}
