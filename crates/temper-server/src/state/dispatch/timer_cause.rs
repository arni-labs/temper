//! The cause a timer's fire carries.
//!
//! A timer fires as whoever armed it, and its own move is not checked by
//! Cedar: the arming move already passed Cedar to enter the timed state, and
//! the timer is that state's own consequence in the spec. The moves its move
//! sets off are checked, and they inherit this cause, so Cedar sees them as
//! `via == "timer"`, armed by the timer's principal.

use temper_authz::{MoveCause, TimerArm};

use crate::request_context::AgentContext;

/// Mark `agent`, the identity a timer fires as, as that timer's fire, armed
/// by its own principal and, when known, by `armed_action`. It replaces any
/// cause the arming move carried: a timer's chain starts at its fire. A
/// context with no security context (an internal adapter) carries no cause.
pub(super) fn mark_timer_fire(agent: &mut AgentContext, armed_action: Option<&str>) {
    if let Some(security_ctx) = agent.security_ctx.as_mut() {
        security_ctx.cause = Some(MoveCause::timer_fire(TimerArm {
            armed_action: armed_action.map(str::to_string),
            armed_by: security_ctx.principal.id.clone(),
        }));
    }
}
