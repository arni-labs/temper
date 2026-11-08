//! Secret writes and loads against a local Turso store.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::SecretPrecondition;
use temper_store_turso::TursoEventStore;

use super::{SecretLoadReport, SecretWriteOutcome};
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::state::ServerState;
use crate::storage::StorageStack;

fn make_state() -> ServerState {
    let system = ActorSystem::new("test-secrets-persistence");
    ServerState::from_registry(system, SpecRegistry::new())
        .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
}

#[tokio::test]
async fn turso_secret_round_trip() {
    let db_path = std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut state = make_state();
    state.set_storage_stack(StorageStack::from_turso(store));

    let vault = state.secrets_vault.as_ref().expect("vault configured");
    let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

    let outcome = state
        .write_secret(
            "tenant-a",
            "API_KEY",
            &ciphertext,
            &nonce,
            SecretPrecondition::Any,
        )
        .await
        .expect("turso secret write should succeed");
    let SecretWriteOutcome::Written { version_id } = outcome else {
        panic!("an unconditional write is always written");
    };
    assert_eq!(
        state.secret_versions("tenant-a").await.unwrap()["API_KEY"],
        version_id
    );

    // Load and decrypt.
    let loaded = state
        .load_tenant_secrets("tenant-a")
        .await
        .expect("turso secret load should succeed");
    assert_eq!(loaded.kept, 1, "should have loaded 1 secret");

    // Verify the decrypted value is cached.
    let cached = vault
        .get_secret("tenant-a", "API_KEY")
        .expect("secret should be cached");
    assert_eq!(cached, "secret-value");

    // Delete secret.
    let deleted = state
        .delete_secret("tenant-a", "API_KEY")
        .await
        .expect("turso secret delete should succeed");
    assert!(deleted, "should have deleted 1 row");

    let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
}

async fn state_on(db_url: &str, vault: SecretsVault) -> ServerState {
    let store = TursoEventStore::new(db_url, None)
        .await
        .expect("open local turso db");
    let system = ActorSystem::new("test-secrets-rotation");
    let mut state =
        ServerState::from_registry(system, SpecRegistry::new()).with_secrets_vault(vault);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn seal_row(state: &ServerState, key_name: &str, value: &[u8]) {
    let vault = state.secrets_vault.as_ref().expect("vault configured");
    let (ciphertext, nonce) = vault.encrypt(value).expect("encrypt");
    state
        .write_secret("t", key_name, &ciphertext, &nonce, SecretPrecondition::Any)
        .await
        .expect("write secret");
}

#[tokio::test]
async fn previous_key_rows_are_resealed_once_and_strays_are_counted() {
    let db_path = std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
    let db_url = format!("file:{}", db_path.display());
    let (old, new, stray) = ([1_u8; 32], [2_u8; 32], [3_u8; 32]);

    seal_row(
        &state_on(&db_url, SecretsVault::new(&old)).await,
        "OLD_A",
        b"a",
    )
    .await;
    seal_row(
        &state_on(&db_url, SecretsVault::new(&old)).await,
        "OLD_B",
        b"b",
    )
    .await;
    seal_row(
        &state_on(&db_url, SecretsVault::new(&stray)).await,
        "STRAY",
        b"s",
    )
    .await;
    seal_row(
        &state_on(&db_url, SecretsVault::new(&new)).await,
        "NEW",
        b"n",
    )
    .await;

    let rotated = state_on(&db_url, SecretsVault::new(&new).with_previous_key(&old)).await;
    let first = rotated.load_tenant_secrets("t").await.expect("first load");
    assert_eq!(
        first,
        SecretLoadReport {
            kept: 1,
            resealed: 2,
            deferred: 0,
            unreadable: 1
        }
    );
    let vault = rotated.secrets_vault.as_ref().unwrap();
    assert_eq!(vault.get_secret("t", "OLD_A").as_deref(), Some("a"));
    assert_eq!(vault.get_secret("t", "STRAY"), None);
    drop(rotated);

    // A second boot with both keys converges: nothing left to re-seal.
    let again = state_on(&db_url, SecretsVault::new(&new).with_previous_key(&old)).await;
    let second = again.load_tenant_secrets("t").await.expect("second load");
    assert_eq!(
        second,
        SecretLoadReport {
            kept: 3,
            resealed: 0,
            deferred: 0,
            unreadable: 1
        }
    );
    drop(again);

    // The previous key is no longer needed for the re-sealed rows.
    let after = state_on(&db_url, SecretsVault::new(&new)).await;
    let third = after
        .load_tenant_secrets("t")
        .await
        .expect("load without previous");
    assert_eq!(third.kept, 3);
    assert_eq!(
        after
            .secrets_vault
            .as_ref()
            .unwrap()
            .get_secret("t", "OLD_B")
            .as_deref(),
        Some("b")
    );

    let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
}
