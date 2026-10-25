//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

pub(crate) mod action_input;
mod actor;
pub(crate) mod bootstrap;
pub mod effects;
pub mod event;
mod field_updates;
mod replay_validation;
mod scheduled_rows;
pub mod sim_handler;
mod snapshot_queue;
pub mod types;
pub(crate) mod undeclared_params;

pub use actor::EntityActor;
pub(crate) use actor::RehydrateHook;
#[cfg(test)]
pub(crate) use actor::recover_authoritative_entity_state_from_store;
pub(crate) use actor::recover_entity_state_from_store;
pub use effects::{
    ProcessResult, ScheduledAction, apply_effects, apply_new_state_fallback, build_eval_context,
    process_action, process_action_with_xref, sync_fields,
};
pub use event::{EffectOutcome, EntityEvent};
pub use sim_handler::EntityActorHandler;
pub(crate) use snapshot_queue::SnapshotWriteQueue;
pub use types::{EntityMsg, EntityResponse, EntityState, StatusClock};
