//! Kernel-owned provenance, captured before commit and restored with the event.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::request_context::AgentContext;

/// The immediate source of an entity move, separate from its acting principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVia {
    /// An authenticated request.
    Request,
    /// A state timeout or scheduled action.
    Timer,
    /// A reaction, trigger, integration callback, or other kernel-owned move.
    Reaction,
}

/// Provenance persisted with an event. Older events have unknown provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOrigin {
    /// How the event came about; null for events written before provenance.
    pub via: Option<EventVia>,
    /// The authenticated request subject, never a timer's acting principal.
    pub by: Option<String>,
}

impl EventOrigin {
    /// A kernel-owned move with no request subject.
    pub(crate) fn internal() -> Self {
        Self {
            via: Some(EventVia::Reaction),
            by: None,
        }
    }

    /// Capture only the kernel's dispatch context, never action parameters.
    pub(crate) fn from_caller(caller: Option<&AgentContext>) -> Self {
        let Some(caller) = caller else {
            return Self::internal();
        };
        if let Some(origin) = &caller.event_origin {
            return origin.clone();
        }
        let cause = caller
            .security_ctx
            .as_ref()
            .and_then(|ctx| ctx.cause.as_ref());
        // A reaction to a timer is a reaction itself. Cedar's `via` describes
        // the chain's root instead, so retain its existing authorization meaning.
        if cause.is_some_and(|cause| cause.reaction.is_some()) {
            return Self::internal();
        }
        if caller.fires_state_timer.is_some() || caller.fires_scheduled_action.is_some() {
            return Self {
                via: Some(EventVia::Timer),
                by: None,
            };
        }
        if caller.callback_hops > 0 {
            return Self::internal();
        }
        if cause.is_some_and(|cause| cause.timer.is_some()) {
            return Self {
                via: Some(EventVia::Timer),
                by: None,
            };
        }
        let Some(ctx) = caller.security_ctx.as_ref() else {
            return Self::internal();
        };
        if ctx.principal.kind == temper_authz::PrincipalKind::System
            || ctx.principal.role.as_deref() == Some("service")
        {
            return Self::internal();
        }
        Self {
            via: Some(EventVia::Request),
            by: (!ctx.principal.id.is_empty() && ctx.principal.id != "anonymous")
                .then(|| ctx.principal.id.clone()),
        }
    }
}

/// Creation time and the newest applied event, retained independently of history.
/// Flattened into each row using the kernel's top-level snake_case convention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastEvent {
    /// Timestamp at journal sequence one; unknown if that event is no longer held.
    #[serde(default, serialize_with = "serialize_first_event_at")]
    pub first_event_at: Option<DateTime<Utc>>,
    /// Timestamp of the newest event, null when no event is known.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Immediate source of that same event.
    pub last_event_via: Option<EventVia>,
    /// Request subject of that same event, or null for kernel-owned work.
    pub last_event_by: Option<String>,
}

impl LastEvent {
    pub(super) fn record(&mut self, event: &super::EntityEvent) {
        self.last_event_at = Some(event.timestamp);
        self.last_event_via = event.origin.via;
        self.last_event_by = event.origin.by.clone();
    }
}

fn serialize_first_event_at<S: serde::Serializer>(
    timestamp: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match timestamp {
        Some(timestamp) => serializer
            .serialize_some(&timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_authz::{MoveCause, ReactionHop, SecurityContext, TimerArm};

    fn caller() -> AgentContext {
        AgentContext {
            security_ctx: Some(SecurityContext::from_resolved_identity(
                "ann", "person", None,
            )),
            // An observation field is not the authenticated subject.
            agent_id: Some("forged-observation".into()),
            ..AgentContext::default()
        }
    }

    #[test]
    fn only_requests_name_the_authenticated_subject() {
        let mut caller = caller();
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("ann".into())
            }
        );
        let timer = MoveCause::timer_fire(TimerArm {
            armed_by: "ann".into(),
        });
        caller.security_ctx.as_mut().unwrap().cause = Some(timer.clone());
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin {
                via: Some(EventVia::Timer),
                by: None
            }
        );
        caller.security_ctx.as_mut().unwrap().cause = Some(MoveCause::reaction(
            ReactionHop {
                rule: "Job:Expire:finish".into(),
                action: "Expire".into(),
                entity_type: "Job".into(),
                entity_id: "job".into(),
            },
            Some(&timer),
        ));
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin::internal()
        );
    }

    #[test]
    fn callbacks_are_reactions_even_when_their_caller_is_a_request() {
        let mut caller = caller();
        caller.callback_hops = 1;
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin::internal()
        );
        assert_eq!(EventOrigin::from_caller(None), EventOrigin::internal());
        assert_eq!(
            EventOrigin::from_caller(Some(&AgentContext::system())),
            EventOrigin::internal()
        );
        assert_eq!(
            EventOrigin::from_caller(Some(&AgentContext::for_service("test"))),
            EventOrigin::internal()
        );
    }

    #[test]
    fn a_scheduled_fire_inside_a_callback_is_a_timer_and_its_answer_is_a_reaction() {
        let mut caller = caller();
        caller.callback_hops = 3; // Zero-delay schedules retain their callback budget.
        caller.security_ctx.as_mut().unwrap().cause = Some(MoveCause::timer_fire(TimerArm {
            armed_by: "ann".into(),
        }));
        caller.fires_scheduled_action = Some(crate::request_context::ScheduledFire {
            tenant: "default".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            arm_id: "arm".into(),
        });
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin {
                via: Some(EventVia::Timer),
                by: None
            }
        );
        let answer = caller
            .for_callback("Job", "job", "Answer", "test")
            .unwrap()
            .for_answer();
        assert_eq!(
            EventOrigin::from_caller(Some(&answer)),
            EventOrigin::internal()
        );
    }

    #[test]
    fn old_events_do_not_invent_provenance() {
        let event: super::super::EntityEvent = serde_json::from_value(serde_json::json!({
            "action":"Expire", "from_status":"Waiting", "to_status":"Done",
            "timestamp":"2026-01-01T00:03:00Z", "params":{"moved_by":"stale"}
        }))
        .unwrap();
        assert_eq!(event.origin, EventOrigin::default());
        let mut last = LastEvent::default();
        last.record(&event);
        assert_eq!(last.last_event_at, Some(event.timestamp));
        assert_eq!(last.last_event_via, None);
        assert_eq!(last.last_event_by, None);
    }
}
