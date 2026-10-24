//! Forgetting a deleted tenant's cached replies.

use temper_runtime::tenant::TenantId;

use super::IdempotencyCache;

impl IdempotencyCache {
    /// Drop every reply cached for a deleted `tenant`'s entities, so a retry
    /// on a tenant created again under the same id runs its action instead
    /// of answering with the deleted tenant's reply.
    ///
    /// Actor keys are `{tenant}:{entity_type}:{entity_id}`, and a tenant id
    /// holds no `:`, so the prefix names exactly this tenant's actors.
    pub fn forget_tenant(&self, tenant: &TenantId) {
        let prefix = format!("{tenant}:");
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let owned: Vec<(String, String)> = entries
            .by_actor
            .range(prefix.clone()..)
            .take_while(|(actor_key, _)| actor_key.starts_with(&prefix))
            .flat_map(|(actor_key, replies)| {
                replies
                    .keys()
                    .map(|idem_key| (actor_key.clone(), idem_key.clone()))
            })
            .collect();
        for (actor_key, idem_key) in owned {
            entries.remove(&actor_key, &idem_key);
        }
        debug_assert!(
            entries
                .by_actor
                .keys()
                .all(|actor_key| !actor_key.starts_with(&prefix))
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use super::*;
    use crate::entity_actor::{EntityResponse, EntityState};

    fn reply() -> EntityResponse {
        EntityResponse {
            success: true,
            state: EntityState {
                entity_type: "Task".to_string(),
                entity_id: "t1".to_string(),
                status: "Done".to_string(),
                item_count: 0,
                counters: BTreeMap::new(),
                booleans: BTreeMap::new(),
                lists: BTreeMap::new(),
                fields: serde_json::json!({}),
                events: VecDeque::new(),
                total_event_count: 0,
                events_since_snapshot: 0,
                last_snapshot_sequence_nr: 0,
                sequence_nr: 0,
                processed_idempotency_keys: BTreeMap::new(),
                status_clock: Default::default(),
            },
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }
    }

    #[test]
    fn a_deleted_tenants_replies_are_forgotten_and_other_tenants_keep_theirs() {
        let cache = IdempotencyCache::new();
        cache.put_effects_applied("acme-two:Task:t1", "k1", &reply());
        cache.put_effects_applied("other:Task:t1", "k1", &reply());
        let kept_bytes = cache.held_bytes();
        cache.put_effects_applied("acme:Task:t1", "k1", &reply());
        cache.put("acme:Task:t2", "k2", &reply());

        cache.forget_tenant(&TenantId::new("acme"));

        assert!(cache.get("acme:Task:t1", "k1").is_none());
        assert!(cache.get("acme:Task:t2", "k2").is_none());
        assert!(cache.get("acme-two:Task:t1", "k1").is_some());
        assert!(cache.get("other:Task:t1", "k1").is_some());
        assert_eq!(
            cache.held_bytes(),
            kept_bytes,
            "the forgotten replies' bytes are freed"
        );
    }
}
