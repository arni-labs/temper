//! A scheduled action armed in this process, and the key each of its fires is
//! made under.

use chrono::{DateTime, Utc};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityResponse;
use crate::idempotency::IdempotencyKey;
use crate::request_context::{AgentContext, ScheduledFire};

/// A scheduled action armed in this process.
pub(super) struct ScheduledTimer {
    pub(super) tenant: TenantId,
    pub(super) entity_type: String,
    pub(super) entity_id: String,
    /// The row's id: the arming event's sequence and the action's index.
    pub(super) arm_id: String,
    pub(super) action: String,
    /// When it is due, as its row says.
    pub(super) due_at: DateTime<Utc>,
    /// Who armed it; it fires as them.
    pub(super) agent_ctx: AgentContext,
    /// The time in its idempotency key, when the row's first key is taken: it
    /// goes on from an earlier fire of the same row, which wrote nothing, or a
    /// boot looks at a row past due.
    pub(super) key_at: Option<DateTime<Utc>>,
}

impl ScheduledTimer {
    /// The row this timer fires.
    pub(super) fn row(&self) -> ScheduledFire {
        ScheduledFire {
            tenant: self.tenant.as_str().to_string(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            arm_id: self.arm_id.clone(),
        }
    }

    /// The key every attempt of this fire is made under.
    pub(super) fn fire_key(&self) -> IdempotencyKey {
        let key = self
            .agent_ctx
            .idempotency_key
            .clone()
            .unwrap_or_else(|| IdempotencyKey::Request(format!("schedule:{}", self.arm_id)));
        let Some(at) = self.key_at else {
            return key;
        };
        let due = at.to_rfc3339();
        match key {
            IdempotencyKey::Request(key) => IdempotencyKey::Request(format!("{key}@{due}")),
            IdempotencyKey::FollowUp(key) => IdempotencyKey::FollowUp(format!("{key}@{due}")),
        }
    }

    /// The next fire of this row, when `response` answers a fire that wrote
    /// nothing and armed only itself (`EntityActor::answer_unrecorded`).
    pub(super) fn goes_on(&self, response: &EntityResponse) -> Option<Self> {
        response.unrecorded_at?;
        let next = response.scheduled_actions.first()?;
        let due_at = next.due_at.unwrap_or(DateTime::<Utc>::MAX_UTC);
        Some(Self {
            tenant: self.tenant.clone(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            arm_id: self.arm_id.clone(),
            action: self.action.clone(),
            due_at,
            agent_ctx: self.agent_ctx.clone(),
            key_at: Some(due_at),
        })
    }
}
