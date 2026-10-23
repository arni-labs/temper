//! Core types for the reaction rule system.
//!
//! All types use `BTreeMap` for deterministic iteration order (DST compliance).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Advisory threshold for a tenant's reaction-rule count. **Not a maximum.**
///
/// [`register_tenant_rules`](super::registry::ReactionRegistry::register_tenant_rules)
/// warns above this number and registers every rule anyway. It asserted until
/// 2026-09-10, when a tenant's fifteenth app took it to 265 rules and the panic
/// crash-looped the platform at startup. Nothing is preallocated from this
/// constant: rules live in growable `BTreeMap`s, so exceeding it corrupts
/// nothing. A consumer must not reject a tenant for being above it. See
/// ADR-0176; [`MAX_REACTION_DEPTH`] is the bound that is still enforced.
pub const MAX_REACTIONS_PER_TENANT: usize = 256;

/// Maximum cascade depth for recursive reaction dispatch; why it exists and
/// how reaching it is reported is written on the definition.
///
/// Owned by `temper-runtime` and re-exported here so the two crates cannot
/// disagree. Three sites enforce it: the production dispatcher, the simulation
/// dispatcher, and `RequestContext`'s callback depth. Until this re-export they
/// read two independently defined copies of the number; nothing kept them
/// equal, and ADR-0176 rests on this bound being the one that still holds.
pub use temper_runtime::reaction::MAX_REACTION_DEPTH;

/// A reaction rule: when a trigger fires, dispatch an action on a target entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRule {
    /// Human-readable name for logging and debugging.
    pub name: String,
    /// The trigger condition (entity type + action + optional state).
    pub when: ReactionTrigger,
    /// The target action to dispatch.
    pub then: ReactionTarget,
    /// How to resolve the target entity ID.
    pub resolve_target: TargetResolver,
    /// Optional principal (registered AgentType name) for elevation
    /// (ADR-0046). When `Some`, dispatch uses a synthetic
    /// `SecurityContext` with every Cedar attribute (`role`, `agent_type`,
    /// `id`) populated from this name. When `None`, dispatch inherits
    /// the invoking principal — the trigger runs as whoever called the
    /// source action.
    #[serde(default)]
    pub principal: Option<String>,
}

/// Trigger condition for a reaction rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTrigger {
    /// The entity type that triggers this reaction (e.g., "Order").
    pub entity_type: String,
    /// The action name that triggers this reaction. `None` = any action.
    pub action: Option<String>,
    /// Optional guard over the source entity's post-action fields and status
    /// (and related entities' statuses). When `Some`, the reaction only fires
    /// if the guard holds. Guard-skipped rules do NOT emit a
    /// [`ReactionResult`] — they never fired.
    #[serde(default)]
    pub guard: Option<temper_spec::predicate::Expr>,
}

/// The target action to dispatch when a reaction fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTarget {
    /// The entity type to dispatch the action on (e.g., "Payment").
    pub entity_type: String,
    /// The action to dispatch (e.g., "AuthorizePayment").
    pub action: String,
    /// Parameters for the target action: literals, or fields of the source
    /// entity read after it commits (`Id` is its id). A field the source does
    /// not have leaves its key out — the reaction still fires with the other
    /// parameters (consistent with `resolver::Field`'s `None`-on-missing
    /// posture).
    ///
    /// `BTreeMap` for deterministic iteration order (DST compliance).
    #[serde(default)]
    pub args: BTreeMap<String, temper_spec::predicate::Arg>,
}

/// How to resolve the target entity ID for a reaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TargetResolver {
    /// Read the target entity ID from a field on the source entity.
    Field {
        /// The field name containing the target entity ID.
        field: String,
    },
    /// Use the same entity ID as the source.
    SameId,
    /// Use a static entity ID.
    Static {
        /// The fixed entity ID.
        entity_id: String,
    },
    /// Create the target entity if it doesn't exist, using a derived ID.
    CreateIfMissing {
        /// Field name containing the target entity ID (or derive from source).
        id_field: String,
    },
    /// Create a genuinely new target entity with a fresh UUID on every
    /// reaction dispatch.
    ///
    /// Unlike [`TargetResolver::CreateIfMissing`] — which keys off a field
    /// on the source entity and is intended for per-source-entity singletons
    /// (one FileVersion per File, etc.) — `Create` returns a new
    /// [`temper_runtime::scheduler::sim_uuid`] every time. This is the
    /// correct resolver for pipeline chaining: each source action spawns a
    /// brand new target entity instance.
    ///
    /// `sim_uuid()` (not `uuid::Uuid::new_v4`) is required for DST
    /// compliance — reaction cascades under `SimReactionSystem` must be
    /// deterministic across seeded runs.
    Create,
}

/// The result of dispatching a single reaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionResult {
    /// The rule that was triggered.
    pub rule_name: String,
    /// Whether the target action succeeded.
    pub success: bool,
    /// The target entity's status after the action (if available).
    pub target_status: Option<String>,
    /// Error message if the action failed.
    pub error: Option<String>,
    /// The cascade depth at which this reaction fired.
    pub depth: u32,
}

impl ReactionResult {
    /// A reaction that was not run because its chain reached
    /// [`MAX_REACTION_DEPTH`].
    pub fn stopped_at_depth(rule_name: &str, depth: u32) -> Self {
        debug_assert!(depth >= MAX_REACTION_DEPTH);
        Self {
            rule_name: rule_name.to_string(),
            success: false,
            target_status: None,
            error: Some(format!(
                "reaction not run: its chain already ran {MAX_REACTION_DEPTH} reactions, the \
                 most one action may set off; a longer chain is stopped as a loop"
            )),
            depth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_rule_serializes_roundtrip() {
        let rule = ReactionRule {
            name: "order_confirmed_triggers_payment".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                guard: Some(temper_spec::predicate::parse("status == 'Confirmed'").unwrap()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                args: BTreeMap::new(),
            },
            resolve_target: TargetResolver::Field {
                field: "payment_id".to_string(),
            },
            principal: None,
        };

        let json = serde_json::to_string(&rule).unwrap();
        let deserialized: ReactionRule = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, rule.name);
        assert_eq!(deserialized.when.entity_type, "Order");
        assert_eq!(deserialized.then.action, "AuthorizePayment");
    }

    #[test]
    fn reaction_target_serialises_args() {
        let mut args = BTreeMap::new();
        args.insert(
            "job_type".to_string(),
            temper_spec::predicate::parse_arg("next_stage").unwrap(),
        );
        args.insert(
            "source".to_string(),
            temper_spec::predicate::parse_arg("'curation'").unwrap(),
        );
        let target = ReactionTarget {
            entity_type: "CurationJob".to_string(),
            action: "Submit".to_string(),
            args,
        };
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"job_type\":\"next_stage\""), "{json}");
        assert!(json.contains("\"source\":\"'curation'\""), "{json}");

        let back: ReactionTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.args, target.args);
    }

    #[test]
    fn reaction_target_defaults_args_empty() {
        let json = r#"{"entity_type":"B","action":"Do"}"#;
        let target: ReactionTarget = serde_json::from_str(json).unwrap();
        assert!(target.args.is_empty());
    }

    #[test]
    fn target_resolver_variants_serialize() {
        let field = TargetResolver::Field {
            field: "payment_id".to_string(),
        };
        let json = serde_json::to_string(&field).unwrap();
        assert!(json.contains("\"type\":\"Field\""));

        let same = TargetResolver::SameId;
        let json = serde_json::to_string(&same).unwrap();
        assert!(json.contains("\"type\":\"SameId\""));

        let static_id = TargetResolver::Static {
            entity_id: "singleton".to_string(),
        };
        let json = serde_json::to_string(&static_id).unwrap();
        assert!(json.contains("\"type\":\"Static\""));

        let create = TargetResolver::CreateIfMissing {
            id_field: "payment_id".to_string(),
        };
        let json = serde_json::to_string(&create).unwrap();
        assert!(json.contains("\"type\":\"CreateIfMissing\""));

        let fresh = TargetResolver::Create;
        let json = serde_json::to_string(&fresh).unwrap();
        assert!(json.contains("\"type\":\"Create\""));
    }
}
