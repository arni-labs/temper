//! Tenant removal: storage goes only once the tenant's actors have ended, and memory is forgotten after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::actor::{Actor, ActorCell, ActorContext, ActorError, ActorId, ActorRef};
use temper_spec::csdl::CsdlDocument;

use super::*;
use crate::entity_actor::EntityMsg;
use crate::secrets::vault::SecretsVault;

fn state() -> ServerState {
    ServerState::new(
        ActorSystem::new("remove-tenant"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    )
}

/// An actor that takes `busy` over every message and records when it has ended.
struct SlowActor {
    busy: Duration,
    ended: Arc<AtomicBool>,
}

impl Actor for SlowActor {
    type Msg = EntityMsg;
    type State = ();

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<(), ActorError> {
        Ok(())
    }

    async fn handle(
        &self,
        _msg: EntityMsg,
        _state: &mut (),
        _ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        tokio::time::sleep(self.busy).await;
        Ok(())
    }

    async fn post_stop(&self, _state: (), _ctx: &mut ActorContext<Self>) {
        self.ended.store(true, Ordering::SeqCst);
    }
}

/// Register a busy actor for `acme:Task:t1` whose mailbox is full, so a stop signal sent through it has no room.
async fn busy_actor(state: &ServerState, busy: Duration) -> (ActorRef<EntityMsg>, Arc<AtomicBool>) {
    let ended = Arc::new(AtomicBool::new(false));
    let actor = ActorCell::new(
        SlowActor {
            busy,
            ended: Arc::clone(&ended),
        },
        ActorId::new("acme:Task:t1", "/remove-tenant/acme:Task:t1"),
    )
    .with_mailbox_capacity(1)
    .spawn();
    actor.tell(EntityMsg::GetState).expect("first message");
    // The actor takes the first message off the queue before it starts on it; then one more fills the mailbox.
    while actor.mailbox_depth() > 0 {
        tokio::task::yield_now().await;
    }
    actor.tell(EntityMsg::GetState).expect("queued message");
    assert_eq!(actor.stop().unwrap_err(), ActorError::MailboxFull);
    state
        .actor_registry
        .write()
        .expect("actor registry")
        .insert("acme:Task:t1".to_string(), actor.clone());
    (actor, ended)
}

#[tokio::test]
async fn a_tenants_storage_is_removed_only_after_its_busy_actor_has_ended() {
    let state = state();
    let (_actor, ended) = busy_actor(&state, Duration::from_millis(300)).await;

    let ended_at_removal = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&ended_at_removal);
    let existed = state
        .remove_tenant(&TenantId::new("acme"), || {
            seen.store(ended.load(Ordering::SeqCst), Ordering::SeqCst);
            async { Ok(true) }
        })
        .await
        .expect("removal finishes");

    assert!(existed);
    assert!(
        ended_at_removal.load(Ordering::SeqCst),
        "the actor had ended before the tenant's storage was removed"
    );
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry")
            .contains_key("acme:Task:t1")
    );
}

#[tokio::test]
async fn an_actor_that_does_not_end_in_time_fails_the_removal_and_a_retry_finishes_it() {
    let mut state = state();
    state.action_dispatch_timeout = Duration::from_millis(50);
    let (actor, ended) = busy_actor(&state, Duration::from_millis(400)).await;
    let acme = TenantId::new("acme");

    let storage_removed = Arc::new(AtomicBool::new(false));
    let removed = Arc::clone(&storage_removed);
    let error = state
        .remove_tenant(&acme, || {
            removed.store(true, Ordering::SeqCst);
            async { Ok(true) }
        })
        .await
        .expect_err("the actor is still handling its message");
    assert!(
        matches!(
            error,
            TenantRemovalError::ActorsStillRunning { running: 1, .. }
        ),
        "{error}"
    );
    assert!(
        !storage_removed.load(Ordering::SeqCst),
        "storage is kept while an actor runs"
    );
    assert!(actor.is_closed(), "the halted actor takes no new work");

    actor.stopped().await;
    assert!(ended.load(Ordering::SeqCst));
    let existed = state
        .remove_tenant(&acme, || {
            removed.store(true, Ordering::SeqCst);
            async { Ok(true) }
        })
        .await
        .expect("the retry finishes the removal");
    assert!(existed);
    assert!(storage_removed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_failed_storage_removal_keeps_what_the_tenant_needs_to_retry() {
    let state = state();
    let policy = r#"permit(principal, action, resource);"#;
    state
        .authz
        .reload_tenant_policies("acme", policy)
        .expect("policy");

    let error = state
        .remove_tenant(&TenantId::new("acme"), || async {
            Err(PersistenceError::Storage("store unavailable".to_string()))
        })
        .await
        .expect_err("storage removal failed");
    assert!(matches!(error, TenantRemovalError::Storage(_)), "{error}");
    assert!(
        state.authz.get_tenant_policy_text("acme").is_some(),
        "the policies that authorize deleting again are kept"
    );
}

#[tokio::test]
async fn a_deleted_tenants_policies_secrets_and_routes_are_forgotten_and_other_tenants_keep_theirs()
{
    let state = state().with_secrets_vault(SecretsVault::new(&[3_u8; 32]));
    let policy = r#"permit(principal, action, resource);"#;
    let vault = state.secrets_vault.clone().expect("vault");
    for tenant in ["acme", "other"] {
        state
            .authz
            .reload_tenant_policies(tenant, policy)
            .expect("policy");
        vault.cache_secret(tenant, "api_key", format!("{tenant}-value"));
        state
            .http_endpoint_tables
            .table_for(&TenantId::new(tenant))
            .await;
    }

    let existed = state
        .remove_tenant(&TenantId::new("acme"), || async { Ok(true) })
        .await
        .expect("removal finishes");
    assert!(existed);

    assert_eq!(state.authz.get_tenant_policy_text("acme"), None);
    assert_eq!(vault.get_secret("acme", "api_key"), None);
    assert!(
        state
            .http_endpoint_tables
            .get(&TenantId::new("acme"))
            .await
            .is_none(),
        "the deleted tenant's routes are gone"
    );
    assert!(state.authz.get_tenant_policy_text("other").is_some());
    assert!(
        state
            .http_endpoint_tables
            .get(&TenantId::new("other"))
            .await
            .is_some()
    );
    assert_eq!(
        vault.get_secret("other", "api_key").as_deref(),
        Some("other-value")
    );
}
