use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

/// Armed state timers (ADR-0056), kept so a restart arms them again.
#[async_trait::async_trait]
pub trait StateTimerStore: Send + Sync {
    /// Record an armed timer; a row armed at a higher sequence is kept.
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError>;

    /// Remove an entity's timer row if it still holds the arming `arm_id`.
    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError>;

    /// Every armed timer row for `tenant`.
    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError>;
}

macro_rules! forward_state_timer_store {
    ($store:ty) => {
        #[async_trait::async_trait]
        impl StateTimerStore for $store {
            async fn upsert_state_timer(
                &self,
                timer: &StateTimerRecord,
            ) -> Result<(), PersistenceError> {
                <$store>::upsert_state_timer(self, timer).await
            }

            async fn delete_state_timer(
                &self,
                tenant: &str,
                entity_type: &str,
                entity_id: &str,
                arm_id: &str,
            ) -> Result<(), PersistenceError> {
                <$store>::delete_state_timer(self, tenant, entity_type, entity_id, arm_id).await
            }

            async fn list_state_timers(
                &self,
                tenant: &str,
            ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
                <$store>::list_state_timers(self, tenant).await
            }
        }
    };
}

forward_state_timer_store!(PostgresEventStore);
forward_state_timer_store!(TursoEventStore);
