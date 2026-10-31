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
///
/// `for` and `agent_type` are recorded only with `by`, and only when the
/// subject carries them: an agent acting for a person names that person and
/// its type. A person acting in person records `by` alone. They are left out
/// of the event when absent, so events without them keep their old shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOrigin {
    /// How the event came about; null for events written before provenance.
    pub via: Option<EventVia>,
    /// The authenticated request subject, never a timer's acting principal.
    pub by: Option<String>,
    /// The principal `by` acted for, as its authenticated principal names it.
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub acting_for: Option<String>,
    /// The agent type of `by`, as its authenticated principal names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

impl EventOrigin {
    /// A kernel-owned move with no request subject.
    pub(crate) fn internal() -> Self {
        Self {
            via: Some(EventVia::Reaction),
            ..Self::default()
        }
    }

    /// A state timeout or scheduled action, which names no request subject.
    fn timer() -> Self {
        Self {
            via: Some(EventVia::Timer),
            ..Self::default()
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
            return Self::timer();
        }
        if caller.callback_hops > 0 {
            return Self::internal();
        }
        if cause.is_some_and(|cause| cause.timer.is_some()) {
            return Self::timer();
        }
        let Some(ctx) = caller.security_ctx.as_ref() else {
            return Self::internal();
        };
        if ctx.principal.kind == temper_authz::PrincipalKind::System
            || ctx.principal.role.as_deref() == Some("service")
        {
            return Self::internal();
        }
        let principal = &ctx.principal;
        if principal.id.is_empty() || principal.id == "anonymous" {
            return Self {
                via: Some(EventVia::Request),
                ..Self::default()
            };
        }
        let present = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
        Self {
            via: Some(EventVia::Request),
            by: Some(principal.id.clone()),
            acting_for: present(&principal.acting_for),
            agent_type: present(&principal.agent_type),
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
    use temper_authz::{MoveCause, PrincipalKind, ReactionHop, SecurityContext, TimerArm};

    /// A person in person, as a verified token with only a `sub` resolves.
    fn caller() -> AgentContext {
        AgentContext {
            security_ctx: Some(SecurityContext::from_verified_jwt(
                "ann",
                PrincipalKind::Customer,
                None,
                None,
                Some("owner"),
                None,
            )),
            // An observation field is not the authenticated subject.
            agent_id: Some("forged-observation".into()),
            ..AgentContext::default()
        }
    }

    /// An agent acting for ann, as a verified token with an `agent_type` resolves.
    fn agent() -> AgentContext {
        AgentContext {
            security_ctx: Some(SecurityContext::from_verified_jwt(
                "hub-client",
                PrincipalKind::Agent,
                Some("claude-code"),
                Some("ann"),
                Some("contributor"),
                None,
            )),
            // Observation fields are not the authenticated subject or its type.
            agent_id: Some("forged-observation".into()),
            agent_type: Some("forged-type".into()),
            ..AgentContext::default()
        }
    }

    fn timer_cause() -> MoveCause {
        MoveCause::timer_fire(TimerArm {
            armed_by: "ann".into(),
        })
    }

    fn reaction_cause() -> MoveCause {
        MoveCause::reaction(
            ReactionHop {
                rule: "Job:Expire:finish".into(),
                action: "Expire".into(),
                entity_type: "Job".into(),
                entity_id: "job".into(),
            },
            Some(&timer_cause()),
        )
    }

    #[test]
    fn only_requests_name_the_authenticated_subject() {
        let mut caller = caller();
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("ann".into()),
                ..EventOrigin::default()
            }
        );
        caller.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin {
                via: Some(EventVia::Timer),
                ..EventOrigin::default()
            }
        );
        caller.security_ctx.as_mut().unwrap().cause = Some(reaction_cause());
        assert_eq!(
            EventOrigin::from_caller(Some(&caller)),
            EventOrigin::internal()
        );
    }

    #[test]
    fn an_agent_request_records_whom_it_acted_for_and_its_type() {
        let origin = EventOrigin::from_caller(Some(&agent()));
        assert_eq!(
            origin,
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("hub-client".into()),
                acting_for: Some("ann".into()),
                agent_type: Some("claude-code".into()),
            }
        );
        // The journaled event carries them at its top level, as `for` and
        // `agent_type`, and reads them back.
        let mut event = super::super::EntityEvent::bootstrap("Idle".into());
        event.origin = origin;
        let journaled = serde_json::to_value(&event).unwrap();
        assert_eq!(journaled["by"], "hub-client");
        assert_eq!(journaled["for"], "ann");
        assert_eq!(journaled["agent_type"], "claude-code");
        assert!(journaled.get("acting_for").is_none(), "{journaled}");
        let read: super::super::EntityEvent = serde_json::from_value(journaled).unwrap();
        assert_eq!(read.origin, event.origin);
    }

    #[test]
    fn a_person_in_person_records_only_by() {
        let origin = EventOrigin::from_caller(Some(&caller()));
        assert_eq!(
            origin,
            EventOrigin {
                via: Some(EventVia::Request),
                by: Some("ann".into()),
                acting_for: None,
                agent_type: None,
            }
        );
        let mut event = super::super::EntityEvent::bootstrap("Idle".into());
        event.origin = origin;
        let journaled = serde_json::to_value(&event).unwrap();
        assert!(journaled.get("for").is_none(), "{journaled}");
        assert!(journaled.get("agent_type").is_none(), "{journaled}");
    }

    #[test]
    fn timer_and_reaction_moves_record_no_agent_or_person() {
        let timer = EventOrigin {
            via: Some(EventVia::Timer),
            ..EventOrigin::default()
        };
        let kernel = EventOrigin {
            via: Some(EventVia::Reaction),
            ..EventOrigin::default()
        };
        let mut fired = agent();
        fired.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
        assert_eq!(EventOrigin::from_caller(Some(&fired)), timer);
        let mut scheduled = agent();
        scheduled.fires_scheduled_action = Some(crate::request_context::ScheduledFire {
            tenant: "default".into(),
            entity_type: "Job".into(),
            entity_id: "job".into(),
            arm_id: "arm".into(),
        });
        assert_eq!(EventOrigin::from_caller(Some(&scheduled)), timer);
        let mut reaction = agent();
        reaction.security_ctx.as_mut().unwrap().cause = Some(reaction_cause());
        assert_eq!(EventOrigin::from_caller(Some(&reaction)), kernel);
        let mut callback = agent();
        callback.callback_hops = 1;
        assert_eq!(EventOrigin::from_caller(Some(&callback)), kernel);
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
        caller.security_ctx.as_mut().unwrap().cause = Some(timer_cause());
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
                ..EventOrigin::default()
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
