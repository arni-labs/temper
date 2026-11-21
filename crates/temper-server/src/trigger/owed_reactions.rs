//! A reaction's move on its target, owed once the source move commits.
//!
//! Each try reads the target, authorizes the move on it as it is now, and
//! sends the move through the core dispatch, without its own reaction
//! cascade, which its follow-on row keeps: `ServerState::reaction_cascade`
//! runs it once the move lands. A try the store failed, in the read or the send, is kept
//! (`state::dispatch::owed_writes`) and tried again under the reaction's key
//! until it lands or fails otherwise, and its cascade runs when it lands.

use std::sync::Arc;

use temper_authz::SecurityContext;

use crate::blobs::StoredReadError;
use crate::entity_actor::EntityResponse;
use crate::state::owed_writes::{Attempt, OwedKind, OwedMove, OwedTry};
use crate::state::{DispatchError, ServerState};

/// What one try of a reaction did.
pub(crate) enum ReactionTry {
    /// Its target, or a value it copies, could not be read.
    Unread(DispatchError),
    /// Cedar refused the move on its target, in `target_status`.
    Denied {
        reason: String,
        target_status: String,
    },
    /// The move was sent: the dispatch's reply.
    Sent(Box<Result<EntityResponse, DispatchError>>),
}

impl ReactionTry {
    fn attempt(&self) -> Attempt {
        match self {
            Self::Unread(error) => Attempt::of_error(error),
            Self::Denied { reason, .. } => Attempt::Failed(reason.clone()),
            Self::Sent(reply) => Attempt::of(reply),
        }
    }
}

/// A reaction's move on its target, sent as its context, which carries the
/// reaction's key, and who the move is authorized as.
pub(crate) struct Reaction {
    pub(crate) sent: OwedMove,
    pub(crate) authorized_as: SecurityContext,
}

impl ServerState {
    /// Try `reaction` at cascade `depth`, and keep it if the store failed it.
    pub(crate) async fn dispatch_reaction(&self, reaction: Reaction, depth: u32) -> ReactionTry {
        let tried = try_reaction(self, &reaction).await;
        if let Attempt::StoreFailed(error) = tried.attempt() {
            let owed = reaction.sent.owed(OwedKind::Reaction);
            let reaction = Arc::new(reaction);
            self.keep_owed_write(owed, &error, move |state| {
                react_again(state, Arc::clone(&reaction), depth)
            });
        }
        tried
    }
}

/// Read the reaction's target, authorize its move on the target as it is
/// now, and send it.
async fn try_reaction(state: &ServerState, reaction: &Reaction) -> ReactionTry {
    let sent = &reaction.sent;
    let snapshot = match state
        .load_authz_resource_snapshot(&sent.tenant, &sent.entity_type, &sent.entity_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return ReactionTry::Unread(error),
    };
    if let Err(denial) = state.authorize_with_context(
        &reaction.authorized_as,
        &sent.action,
        &sent.entity_type,
        &snapshot.resource_attrs,
        sent.tenant.as_str(),
    ) {
        let reason = denial.to_string();
        // Every final Cedar refusal is reported here, including a retry whose
        // first try could not read or send. The dispatcher only counts its
        // immediate results on the fan-out span.
        reaction.record_withheld(
            state,
            &snapshot.current_state.state.status,
            &snapshot.resource_attrs,
            &reason,
        );
        return ReactionTry::Denied {
            reason,
            target_status: snapshot.current_state.state.status,
        };
    }
    // An arg that copies a field the source stores out of line sends its
    // value (`blobs::resolve_copied_values`), read in this try so a store
    // that does not answer keeps the reaction for another. One that can never
    // be read stays a reference, which the target's move refuses.
    let params = match state.copied_values(&sent.tenant, &sent.params).await {
        Ok(params) => params,
        Err(StoredReadError::Unavailable(error)) => {
            return ReactionTry::Unread(DispatchError::StoreUnavailable(error));
        }
        Err(StoredReadError::Invalid(_)) => sent.params.clone(),
    };
    let reply = state
        .dispatch_tenant_action_core(
            &sent.tenant,
            &sent.entity_type,
            &sent.entity_id,
            &sent.action,
            params,
            &sent.agent_ctx,
            false,
            crate::state::write_record::WriteCaller::Kernel,
            None,
            // Its follow-on row keeps the reactions it owes in turn, taken
            // from the table it is applied under (`reaction_cascade`).
            true,
        )
        .await;
    if let Ok(response) = &reply
        && response.refused
    {
        reaction.record_refused(state, response);
    }
    ReactionTry::Sent(Box::new(reply))
}

/// Try an owed reaction again; once it lands, run its cascade at the next
/// depth, as the dispatcher does for a reaction that lands at once
/// (`reaction_cascade`).
fn react_again(state: ServerState, reaction: Arc<Reaction>, depth: u32) -> OwedTry {
    Box::pin(async move {
        let tried = try_reaction(&state, &reaction).await;
        let attempt = tried.attempt();
        if let ReactionTry::Sent(reply) = tried
            && let Ok(response) = *reply
            && response.success
        {
            Box::pin(state.reaction_cascade(&reaction.sent, &response, depth)).await;
        }
        attempt
    })
}
