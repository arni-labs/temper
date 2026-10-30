//! The signal left by a reaction Cedar refuses, on its first try or a retry.

use std::collections::BTreeMap;

use temper_runtime::scheduler::sim_now;

use super::owed_reactions::Reaction;
use crate::ServerState;
use crate::state::{TrajectoryEntry, TrajectorySource};

impl Reaction {
    pub(super) fn record_withheld(
        &self,
        state: &ServerState,
        status: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
        reason: &str,
    ) {
        let sent = &self.sent;
        let principal = &self.authorized_as.principal;
        let cause = self
            .authorized_as
            .cause
            .as_ref()
            .expect("a reaction carries its cause");
        let hop = cause
            .reaction
            .as_ref()
            .expect("a reaction carries its source move");
        tracing::warn!(
            tenant = %sent.tenant,
            entity_type = sent.entity_type,
            entity_id = sent.entity_id,
            action = sent.action,
            status,
            rule = hop.rule,
            source_entity_type = hop.entity_type,
            source_entity_id = hop.entity_id,
            source_action = hop.action,
            principal_id = principal.id,
            principal_kind = ?principal.kind,
            acting_for = ?principal.acting_for,
            via = cause.via(),
            depth = cause.depth,
            armed_by = ?cause.timer.as_ref().map(|arm| arm.armed_by.as_str()),
            reason,
            "reaction withheld"
        );
        crate::runtime_metrics::record_reaction_withheld(
            sent.tenant.as_str(),
            &sent.entity_type,
            &sent.action,
        );
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: sent.tenant.to_string(),
            entity_type: sent.entity_type.clone(),
            entity_id: sent.entity_id.clone(),
            action: sent.action.clone(),
            success: false,
            from_status: Some(status.to_string()),
            to_status: None,
            error: Some(format!("reaction withheld: {reason}")),
            agent_id: Some(principal.id.clone()),
            session_id: sent.agent_ctx.session_id.clone(),
            authz_denied: Some(true),
            denied_resource: Some(format!("{}:{}", sent.entity_type, sent.entity_id)),
            denied_module: None,
            source: Some(TrajectorySource::Authz),
            spec_governed: Some(true),
            agent_type: principal.agent_type.clone(),
            request_body: Some(serde_json::to_value(resource_attrs).unwrap_or_default()),
            intent: sent.agent_ctx.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        if !state.enqueue_trajectory_entry(entry) {
            tracing::warn!("failed to enqueue withheld reaction trajectory");
        }
    }
}
