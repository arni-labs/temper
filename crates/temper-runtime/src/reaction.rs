//! Budgets for cross-entity reaction dispatch.
//!
//! Reactions are declared as entity-kind `[[action.triggers]]` on the
//! source action; the server synthesizes and dispatches them.

/// How many reactions one action can set off in a chain: A's action fires a
/// reaction on B, B's fires one on C, and so on.
///
/// Forced: a chain can loop (A -> B -> A, or an action that reacts to itself),
/// and nothing else ends such a loop. Each hop commits an action and writes an
/// event, so without a bound one action writes events forever. The value is a
/// choice, not a measurement; it is what the composite verifier explores too
/// (`temper-verify`'s `MAX_TRIGGER_DEPTH`).
///
/// Reaching it is reported, never dropped: the reaction that would have run
/// next is not run, the dispatcher returns a failed `ReactionResult` for it,
/// and the entity whose action would have fired it gets a
/// `reaction_cascade_stopped` observe event. Inline integration callbacks
/// nest under the same bound (`AgentContext::for_callback`), where reaching it
/// is reported as `integration_callback_rejected`.
pub const MAX_REACTION_DEPTH: u32 = 8;
