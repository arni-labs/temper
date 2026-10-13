use super::{cache_platform_secret_if_present, ioa_sources_requiring_background_verification};
use std::collections::{BTreeMap, HashMap};
use temper_server::secrets::vault::SecretsVault;

fn test_vault() -> SecretsVault {
    SecretsVault::new(&[0x42; 32])
}

#[test]
fn cache_platform_secret_if_present_stores_present_value() {
    let vault = test_vault();

    cache_platform_secret_if_present(&vault, "exa_api_key", Some("exa-live".to_string()));

    assert_eq!(
        vault.get_platform_secret("exa_api_key"),
        Some("exa-live".to_string())
    );
}

#[test]
fn cache_platform_secret_if_present_skips_missing_value() {
    let vault = test_vault();

    cache_platform_secret_if_present(&vault, "exa_api_key", None);

    assert_eq!(vault.get_platform_secret("exa_api_key"), None);
}

#[test]
fn background_verification_skips_cached_verified_hashes() {
    let mut ioa_sources = HashMap::new();
    ioa_sources.insert(
        "Issue".to_string(),
        "[automaton]\nname = \"Issue\"\n".to_string(),
    );
    ioa_sources.insert(
        "Task".to_string(),
        "[automaton]\nname = \"Task\"\n".to_string(),
    );

    let mut verification_cache = BTreeMap::new();
    verification_cache.insert(
        "Issue".to_string(),
        (
            temper_store_turso::spec_content_hash(ioa_sources.get("Issue").expect("Issue source")),
            true,
        ),
    );
    verification_cache.insert(
        "Task".to_string(),
        (
            temper_store_turso::spec_content_hash("different source"),
            true,
        ),
    );

    let pending = ioa_sources_requiring_background_verification(ioa_sources, &verification_cache);

    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key("Task"));
}
