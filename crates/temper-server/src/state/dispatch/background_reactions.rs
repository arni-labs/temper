//! A committed move's reactions, sent off the request when it does not wait
//! for them. At most [`DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY`] run at
//! once; past that the caller sends them inline. Each holds the move's
//! follow-on row until its reactions are sent.

use std::sync::{Arc, OnceLock};

use temper_runtime::tenant::TenantId;
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::request_context::AgentContext;
use crate::trigger::dispatcher::CommittedMove;

const DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY: usize = 64;

/// A committed move whose reactions are sent in the background.
pub(super) struct BackgroundReactionDispatch {
    pub(super) tenant: TenantId,
    pub(super) entity_type: String,
    pub(super) entity_id: String,
    pub(super) action: String,
    pub(super) to_state: String,
    pub(super) fields: serde_json::Value,
    /// The reactions the move's action declared when it was sent.
    pub(super) declared: Option<Vec<crate::trigger::ReactionRule>>,
    pub(super) agent_ctx: AgentContext,
}

/// The permits the background reactions of every move share.
fn background_reaction_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(
        SEMAPHORE
            .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY))),
    )
}

impl crate::state::ServerState {
    /// Send `dispatch`'s reactions in the background, or hand it back when
    /// every permit is taken, for the caller to send inline.
    pub(super) fn try_spawn_background_reactions(
        &self,
        dispatch: BackgroundReactionDispatch,
    ) -> Option<BackgroundReactionDispatch> {
        let Ok(permit) = background_reaction_semaphore().try_acquire_owned() else {
            return Some(dispatch);
        };

        let state = self.clone();
        let BackgroundReactionDispatch {
            tenant,
            entity_type,
            entity_id,
            action,
            to_state,
            fields,
            declared,
            agent_ctx,
        } = dispatch;
        let span = crate::trace_context::runtime_work_span!(
            crate::trace_context::cause(),
            kind = "reaction",
            "reaction.dispatch.background",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            action_name = %action,
        );

        let hold = self.hold_follow_on(&agent_ctx);
        let reaction_task = async move {
            let _permit = permit;
            let moved = CommittedMove {
                entity_type: &entity_type,
                entity_id: &entity_id,
                action: &action,
                to_state: &to_state,
                fields: &fields,
            };
            let results = state
                .dispatch_declared_reactions(&tenant, &moved, declared.as_deref(), &agent_ctx)
                .await;
            tracing::info!(
                tenant = %tenant,
                entity_type = %entity_type,
                entity_id = %entity_id,
                action_name = %action,
                reaction.result_count = results.len(),
                "background reactions completed"
            );
            state.settle_follow_on(hold);
        }
        .instrument(span);
        tokio::spawn(reaction_task); // determinism-ok: production-only post-commit reaction side effects
        None
    }
}
