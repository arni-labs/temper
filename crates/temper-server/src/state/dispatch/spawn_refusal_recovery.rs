//! Durable refused-spawn work. A failed lookup never proves absence, and a
//! failed parent append never acknowledges the work. The journal survives a
//! server restart; completion is recorded only after the parent write succeeds.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::SpawnRequest;
use crate::entity_actor::spawn_refusal::SpawnRefusal;
use crate::state::ServerState;

const JOURNAL: &str = "__temper_internal:SpawnRefusal:recovery";
const RETRY: Duration = Duration::from_millis(100);

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    tenant: TenantId,
    parent_type: String,
    parent_id: String,
    refusal: SpawnRefusal,
}

impl ServerState {
    pub(in crate::state) fn start_spawn_refusal_recovery(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            // determinism-ok: external journal recovery worker
            let mut sequence = 0;
            let mut pending = BTreeMap::<String, Pending>::new();
            loop {
                let Some((store, _)) = state.event_journal() else {
                    return;
                };
                match store.read_events(JOURNAL, sequence).await {
                    Ok(events) => {
                        for event in events {
                            let Some(id) = event.payload["id"].as_str().map(str::to_owned) else {
                                tracing::error!(
                                    sequence = event.sequence_nr,
                                    "invalid spawn refusal recovery id"
                                );
                                return;
                            };
                            if event.event_type == "Pending" {
                                match serde_json::from_value(event.payload["work"].clone()) {
                                    Ok(work) => {
                                        pending.insert(id, work);
                                    }
                                    Err(error) => {
                                        tracing::error!(%error, "invalid spawn refusal recovery record");
                                        return;
                                    }
                                }
                            } else if event.event_type == "Completed" {
                                pending.remove(&id);
                            }
                            sequence = event.sequence_nr;
                        }
                        for (id, work) in &pending {
                            if let Err(error) = state.reconcile_spawn_refusal(work).await {
                                tracing::warn!(%error, work_id = %id, "spawn refusal repair pending");
                            } else {
                                // On failure the next pass retries. The parent's
                                // compare-and-clear is safe to repeat.
                                if let Err(error) = state
                                    .append_refusal_work("Completed", serde_json::json!({"id": id}))
                                    .await
                                {
                                    tracing::warn!(%error, work_id = %id, "spawn refusal acknowledgement pending");
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "spawn refusal recovery read failed; retaining work")
                    }
                }
                tokio::time::sleep(RETRY).await;
            }
        });
    }

    pub(super) async fn recover_spawn_refusal(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        request: &SpawnRequest,
        error: &str,
    ) {
        let work = Pending {
            tenant: tenant.clone(),
            parent_type: parent_type.to_owned(),
            parent_id: parent_id.to_owned(),
            refusal: SpawnRefusal {
                recovery_id: Some(format!("spawn-refusal:{}", sim_uuid())),
                child_type: request.entity_type.clone(),
                child_id: request.entity_id.clone(),
                field: request.store_id_in.clone(),
                child_exists: false,
                error: error.to_owned(),
            },
        };
        if self.event_journal().is_none() {
            while self.reconcile_spawn_refusal(&work).await.is_err() {
                tokio::time::sleep(RETRY).await;
            }
            return;
        }
        let id = sim_uuid().to_string();
        let payload = serde_json::json!({"id": id, "work": work});
        // Retain ownership until the journal accepts the repair. A store outage
        // must not turn into a dropped callback after a fixed retry budget.
        while let Err(error) = self.append_refusal_work("Pending", payload.clone()).await {
            tracing::warn!(%error, "spawn refusal enqueue failed; retrying");
            tokio::time::sleep(RETRY).await;
        }
    }

    async fn reconcile_spawn_refusal(&self, work: &Pending) -> Result<(), String> {
        let mut refusal = work.refusal.clone();
        refusal.child_exists = self
            .entity_exists_durably(&work.tenant, &refusal.child_type, &refusal.child_id)
            .await?;
        self.record_spawn_refusal(&work.tenant, &work.parent_type, &work.parent_id, refusal)
            .await
            .map(|_| ())
    }

    async fn append_refusal_work(
        &self,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let (store, _) = self
            .event_journal()
            .expect("durable recovery requires a journal");
        let events = store
            .read_events(JOURNAL, 0)
            .await
            .map_err(|e| e.to_string())?;
        let sequence = events.last().map_or(0, |event| event.sequence_nr);
        let envelope = PersistenceEnvelope {
            sequence_nr: sequence + 1,
            event_type: kind.to_owned(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: JOURNAL.to_owned(),
            },
        };
        store
            .append(JOURNAL, sequence, &[envelope])
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
