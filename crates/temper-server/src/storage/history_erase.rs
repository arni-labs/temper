use temper_runtime::persistence::{HistoryErase, HistoryErased, PersistenceError};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

/// Erasing a deleted entity's history from the store that holds its journal.
#[async_trait::async_trait]
pub trait HistoryEraseStore: Send + Sync {
    async fn erase_entity_history(
        &self,
        erase: HistoryErase<'_>,
    ) -> Result<HistoryErased, PersistenceError>;
}

macro_rules! forward_history_erase_store {
    ($store:ty) => {
        #[async_trait::async_trait]
        impl HistoryEraseStore for $store {
            async fn erase_entity_history(
                &self,
                erase: HistoryErase<'_>,
            ) -> Result<HistoryErased, PersistenceError> {
                <$store>::erase_entity_history(self, erase).await
            }
        }
    };
}

forward_history_erase_store!(PostgresEventStore);
forward_history_erase_store!(TursoEventStore);
