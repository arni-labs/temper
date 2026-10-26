//! Whether a kind of sender may send an action, decided from the policies
//! alone, without a request.
//!
//! The joint model of an app asks who can make each move: someone outside
//! the kernel, or one of the kernel's own services. A real request names one
//! principal, one record and its fields; this question names only the kind of
//! sender, the action and the entity type. Each policy is evaluated with what
//! that leaves unknown (an outside principal's id and attributes, the record's
//! id and fields, the context keys a record's fields may supply), and each
//! condition comes out as the set of results it can have: true, false or an
//! evaluation error, which Cedar takes as the policy not applying.
//!
//! The answer is [`May::Yes`] only when some permit applies whatever the
//! unknowns are and no forbid can, [`May::No`] when no permit can apply or a
//! forbid always does, and [`May::Maybe`] otherwise. A caller that knows some
//! of the record's attributes passes them, and those are unknown no longer.
//!
//! A reaction with no declared principal runs as whoever sent its source's
//! action, so a model asks too what such a sender may send when it is
//! someone outside: [`SendAnalysis::caller`] makes that sender, as the
//! classes of outside principals the policies may let send the source's
//! action (`class.rs`).

mod class;
mod json;
mod request;

use std::collections::BTreeMap;

use cedar_policy::{Effect, PolicySet};
use serde_json::Value as Json;

use super::platform_policy::merge_system_platform_policy;
use crate::error::AuthzError;
use class::Class;
use request::{Request, TRUE, may_act};

/// A kind of sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// Anyone outside the kernel: a person, an agent, an operator's key, or a
    /// module calling the API as whoever invoked it. Never a `System`
    /// principal and never a `service:` id: those only the kernel builds.
    Outside,
    /// A kernel service as `AgentContext::for_service` builds it:
    /// `Agent::"service:<name>"` with `agent_type` the name, `role`
    /// "service" and `agentTypeVerified` false, and these context
    /// attributes besides the ones that builds.
    Service {
        /// The service's name, without `service:`.
        name: String,
        /// Context attributes the kernel adds, such as a declared trigger's.
        context: BTreeMap<String, Attr>,
    },
    /// Someone outside the kernel whom Cedar may let send a certain action,
    /// as [`SendAnalysis::caller`] makes it.
    Caller(Callers),
}

/// The classes of outside principals the policies may let send an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callers(Vec<Class>);

/// A context attribute the kernel sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attr {
    /// This value.
    Value(Json),
    /// The id of the record the request acts on, as `resource.id` holds it.
    ResourceId,
    /// A string known only by which others it equals: the same as each
    /// `Same` with this name and no other, and never empty. A model that
    /// names the values it makes up (rather than holding real ones) knows
    /// them only this way, so one is never taken to be a policy's literal.
    Same(String),
    /// Some value not known here.
    Unknown,
}

/// Whether a kind of sender may send an action to an entity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum May {
    /// Cedar refuses it whatever the request holds.
    No,
    /// It depends on what is not known here: which outside principal, or the
    /// record's fields.
    Maybe,
    /// Cedar allows it whatever the request holds.
    Yes,
}

/// A tenant's policies with the built-in platform policy, read for
/// [`SendAnalysis::may`].
#[derive(Debug, Clone)]
pub struct SendAnalysis {
    policies: Vec<(Effect, Json)>,
}

impl SendAnalysis {
    /// Read `policy_text` (a tenant's policies) with the platform's own.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        let mut set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut set);
        let policies = set
            .policies()
            .map(|policy| {
                // A template-linked policy has no JSON form of its own; an
                // unknown condition makes it a policy that may apply.
                let json = policy.to_json().unwrap_or(Json::Null);
                (policy.effect(), json)
            })
            .collect();
        Ok(Self { policies })
    }

    /// Whether `asker` may send `action` to a record of `resource_type`, of
    /// which the caller knows the attributes in `record` (`id` its id).
    /// For a [`Asker::Caller`], the answer for each of its classes when
    /// they all agree, and [`May::Maybe`] when they do not.
    pub fn may(
        &self,
        asker: &Asker,
        action: &str,
        resource_type: &str,
        record: &BTreeMap<String, Attr>,
    ) -> May {
        let Asker::Caller(Callers(classes)) = asker else {
            return self.decide(asker, None, action, resource_type, record);
        };
        classes
            .iter()
            .map(|class| self.decide(&Asker::Outside, Some(class), action, resource_type, record))
            .reduce(|one, other| if one == other { one } else { May::Maybe })
            .unwrap_or(May::No)
    }

    /// Someone outside whom Cedar may let send `action` to a record of
    /// `resource_type`: [`Asker::Outside`] when it may let anyone outside.
    /// The classes are those of the policies that may apply to `action`, so
    /// another action is decided for them with the attributes those policies
    /// do not read unknown.
    pub fn caller(&self, action: &str, resource_type: &str) -> Asker {
        let policies = self
            .policies
            .iter()
            .map(|(_, policy)| policy)
            .filter(|policy| may_act(policy, action));
        let every = class::classes(policies);
        let none = BTreeMap::new();
        let classes: Vec<Class> = every
            .iter()
            .filter(|class| {
                self.decide(&Asker::Outside, Some(class), action, resource_type, &none) != May::No
            })
            .cloned()
            .collect();
        if classes.len() == every.len() {
            Asker::Outside
        } else {
            Asker::Caller(Callers(classes))
        }
    }

    /// Whether `asker`, of `class` when it is someone outside of one, may
    /// send `action` to a record of `resource_type`.
    fn decide(
        &self,
        asker: &Asker,
        class: Option<&Class>,
        action: &str,
        resource_type: &str,
        record: &BTreeMap<String, Attr>,
    ) -> May {
        let request = Request {
            asker,
            action,
            resource_type,
            record,
            class,
        };
        let (mut permit_can, mut permit_always) = (false, false);
        let (mut forbid_can, mut forbid_always) = (false, false);
        for (effect, policy) in &self.policies {
            let applies = request.applies(policy);
            let (can, always) = (applies.can(TRUE), applies == TRUE);
            match effect {
                Effect::Permit => {
                    permit_can |= can;
                    permit_always |= always;
                }
                Effect::Forbid => {
                    forbid_can |= can;
                    forbid_always |= always;
                }
            }
        }
        if !permit_can || forbid_always {
            May::No
        } else if permit_always && !forbid_can {
            May::Yes
        } else {
            May::Maybe
        }
    }
}
