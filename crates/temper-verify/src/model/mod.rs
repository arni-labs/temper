//! Generate Stateright models from I/O Automaton specifications.
//!
//! This module translates a parsed `Automaton` directly into a Stateright `Model`
//! for exhaustive state-space exploration. The generated model captures:
//!   - Multi-variable state (status + named counters + named booleans)
//!   - Transitions as named actions with guards and effects
//!   - Safety invariants as Stateright "always" properties
//!   - Liveness properties as "eventually" / "always" properties
//!
//! Because Stateright's `Property::always` requires a bare function pointer
//! (not a capturing closure), all invariant data lives inside `TemperModel`
//! and is accessed via the `&TemperModel` reference in property conditions.

pub mod builder;
mod expansion;
pub mod semantics;
mod stateright_impl;
mod string;
mod strings;
pub mod types;

pub use builder::{build_model_from_automaton, build_model_from_ioa, build_model_tracking};
pub(crate) use stateright_impl::REACHES_TERMINAL;
pub(crate) use types::ExploreLimit;
pub use types::{
    LivenessKind, ModelEffect, ModelString, ParamValue, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition, StringParam, TemperModel, TemperModelAction, TemperModelState,
};
