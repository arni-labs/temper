//! One step of the automatic-move graph, using the dispatcher's functions.

use temper_authz::{AuthzEngine, SendAnalysis};
use temper_spec::automaton::{Action, Automaton, Effect, StateTimeout, TriggerKind};

use crate::request_context::AgentContext;
use crate::state::{mark_timer_fire, timer_agent};
use crate::trigger::{effective_trigger_security_context, resolve_trigger_principal};

use super::{
    Finding, Pending, RECORD, REFUSED_FOLLOW_UP, Seen, judge, kind_of, principal_id, remember,
};

pub(super) fn expand_triggers(
    analysis: &SendAnalysis,
    engine: &AuthzEngine,
    entity: &str,
    action_name: &str,
    action: &Action,
    invoker: &AgentContext,
    seen: &mut Seen,
    work: &mut Vec<Pending>,
    errors: &mut Vec<Finding>,
    warnings: &mut Vec<Finding>,
) {
    for trigger in &action.triggers {
        match trigger.kind {
            TriggerKind::Entity => {
                let (Some(target_entity), Some(target_action)) = (
                    trigger.target_entity.as_deref(),
                    trigger.target_action.as_deref(),
                ) else {
                    continue;
                };
                let rule = format!("{entity}:{action_name}:{}", trigger.name);
                let (ctx, cause) = resolve_trigger_principal(
                    trigger.principal.as_deref(),
                    invoker,
                    &rule,
                    entity,
                    RECORD,
                    action_name,
                    0,
                );
                let security = effective_trigger_security_context(&ctx, &cause);
                judge(
                    analysis,
                    engine,
                    &ctx,
                    &security,
                    entity,
                    action_name,
                    target_entity,
                    target_action,
                    errors,
                    warnings,
                );
                remember(seen, work, target_entity, target_action, &ctx);
            }
            TriggerKind::Wasm | TriggerKind::Adapter | TriggerKind::Webhook => {
                let source = trigger
                    .module
                    .as_deref()
                    .or(trigger.adapter.as_deref())
                    .unwrap_or(trigger.kind.as_str());
                for callback in trigger.on_success.iter().chain(&trigger.on_failure) {
                    match answer(invoker, entity, callback, source) {
                        Ok(ctx) => remember(seen, work, entity, callback, &ctx),
                        Err(error) => errors.push(Finding {
                            code: REFUSED_FOLLOW_UP,
                            entity_type: entity.to_string(),
                            action: callback.clone(),
                            principal: principal_id(&kind_of(invoker)),
                            after: Some(format!("{entity}.{action_name}")),
                            detail: format!(
                                "{entity}.{callback} following {entity}.{action_name} is refused \
                                 before it runs: {error}"
                            ),
                        }),
                    }
                }
            }
            TriggerKind::Hook => {}
        }
    }
}

pub(super) fn expand_effects(
    entity: &str,
    action: &Action,
    invoker: &AgentContext,
    seen: &mut Seen,
    work: &mut Vec<Pending>,
) {
    for effect in &action.effect {
        match effect {
            Effect::Schedule {
                action: next,
                delay_seconds,
            } => {
                let mut follow = invoker.for_scheduled_follow_on(
                    next,
                    0,
                    delay_seconds.saturating_mul(1000).max(1000),
                );
                mark_timer_fire(&mut follow);
                remember(seen, work, entity, next, &follow);
            }
            Effect::ScheduleAt { action: next, .. } => {
                let mut follow = invoker.for_scheduled_follow_on(next, 0, 1000);
                mark_timer_fire(&mut follow);
                remember(seen, work, entity, next, &follow);
            }
            Effect::Spawn {
                entity_type,
                initial_action,
                ..
            } => {
                let child = invoker.for_spawned_child(entity_type, RECORD);
                remember(seen, work, entity_type, initial_action, &child);
            }
            Effect::Assign { .. } | Effect::Append { .. } | Effect::RemoveAt { .. } => {}
        }
    }
}

pub(super) fn expand_timers(
    automaton: &Automaton,
    action: &Action,
    invoker: &AgentContext,
    seen: &mut Seen,
    work: &mut Vec<Pending>,
) {
    for timer in &automaton.state_timeouts {
        if arms(action, timer) {
            let mut fired = timer_agent(invoker);
            mark_timer_fire(&mut fired);
            remember(
                seen,
                work,
                &automaton.automaton.name,
                &timer.on_timeout,
                &fired,
            );
        }
    }
}

fn arms(action: &Action, timer: &StateTimeout) -> bool {
    let enters = action.to.as_deref() == Some(timer.state.as_str())
        && (action.from.is_empty() || action.from.iter().any(|from| from != &timer.state));
    let resets = timer.reset_on.iter().any(|name| name == &action.name)
        && action.to.as_deref().is_none_or(|to| to == timer.state);
    enters || resets
}

fn answer(
    invoker: &AgentContext,
    entity: &str,
    action: &str,
    source: &str,
) -> Result<AgentContext, String> {
    invoker
        .for_callback(entity, RECORD, action, source)
        .map(|ctx| ctx.for_answer())
        .map_err(|error| error.to_string())
}
