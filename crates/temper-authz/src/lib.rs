//! temper-authz: Cedar-based authorization for Temper.
//!
//! Integrates AWS Cedar policy language for fine-grained, entity-aware
//! authorization decisions on OData operations. Every actor message dispatch
//! goes through: Request → SecurityContext → Cedar Evaluate → Allow/Deny.

mod cause;
mod context;
mod engine;
mod error;
mod metrics;
mod policy_gen;
pub mod statements;
mod verified_token;

pub use cause::{MoveCause, ReactionHop, TimerArm};
pub use context::{
    AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext,
    is_cedar_authority_context_key,
};
pub use engine::{AuthzDecision, AuthzEngine, ListReadUniformity, VERIFIED_OPERATOR_WHEN};
pub use error::{AuthzDenial, AuthzError};
pub use metrics::init_metrics;
pub use policy_gen::{
    ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope,
    generate_cedar_from_matrix, validate_policy_scope_matrix,
};
pub use verified_token::{TokenKind, VerifiedToken};
