//! Encrypted at-rest secret storage per tenant.
//!
//! Uses AES-256-GCM for authenticated encryption with a master key
//! (`TEMPER_VAULT_KEY` env var). Secrets are cached in memory and
//! persisted to Postgres as `(ciphertext, nonce)` pairs.
//!
//! During a key rotation the vault also holds the previous master key
//! (`TEMPER_VAULT_KEY_PREVIOUS`). It only opens rows with it; every seal uses
//! the current key, so a row opened with the previous key is sealed again
//! under the current one (see ADR-0075).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use aes_gcm::aead::{Aead, OsRng}; // determinism-ok: cryptographic nonce generation, not simulation-visible
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};

/// Maximum secret value size in bytes (TigerStyle budget).
pub const MAX_SECRET_VALUE_BYTES: usize = 8192;

/// Encrypted secret storage with AES-256-GCM.
///
/// Holds a cipher derived from the master key and an in-memory cache
/// of decrypted secrets per tenant. The cache is populated from the
/// persistence layer on startup and kept in sync on writes.
pub struct SecretsVault {
    /// AES-256-GCM cipher of the current master key: seals and opens.
    cipher: Arc<Aes256Gcm>,
    /// Cipher of the previous master key during a rotation: opens only.
    previous: Option<Arc<Aes256Gcm>>,
    /// Shared platform secrets available to every tenant.
    platform: Arc<RwLock<BTreeMap<String, String>>>,
    /// In-memory cache: tenant → (key_name → plaintext_value).
    cache: Arc<RwLock<BTreeMap<String, BTreeMap<String, String>>>>,
}

impl SecretsVault {
    /// Create a new vault from a 32-byte master key.
    pub fn new(master_key: &[u8; 32]) -> Self {
        // determinism-ok: cryptographic operations are CPU-bound
        let key = Key::<Aes256Gcm>::from_slice(master_key);
        let cipher = Aes256Gcm::new(key);
        Self {
            cipher: Arc::new(cipher),
            previous: None,
            platform: Arc::new(RwLock::new(BTreeMap::new())),
            cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Also open rows sealed under `previous_key`, the master key this one
    /// replaces. Nothing is ever sealed with it.
    pub fn with_previous_key(mut self, previous_key: &[u8; 32]) -> Self {
        debug_assert!(self.previous.is_none(), "one previous key at most");
        let key = Key::<Aes256Gcm>::from_slice(previous_key);
        self.previous = Some(Arc::new(Aes256Gcm::new(key)));
        self
    }

    /// Whether this vault holds a previous master key.
    pub fn has_previous_key(&self) -> bool {
        self.previous.is_some()
    }

    /// Encrypt a plaintext value, returning `(ciphertext, nonce)`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        debug_assert!(
            plaintext.len() <= MAX_SECRET_VALUE_BYTES,
            "secret value exceeds budget: {} > {}",
            plaintext.len(),
            MAX_SECRET_VALUE_BYTES
        );
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // determinism-ok: cryptographic nonce generation
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| format!("encryption failed: {e}"))?;
        Ok((ciphertext, nonce.to_vec()))
    }

    /// Decrypt a ciphertext with the given nonce under the current key.
    pub fn decrypt(&self, ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
        decrypt_with(&self.cipher, ciphertext, nonce_bytes)
    }

    /// Open a stored row with the current key, or else the previous one.
    ///
    /// The error is the current key's: without a previous key this is
    /// exactly [`Self::decrypt`].
    pub fn open(&self, ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Opened, String> {
        let current_error = match self.decrypt(ciphertext, nonce_bytes) {
            Ok(plaintext) => return Ok(Opened::Current(plaintext)),
            Err(e) => e,
        };
        match &self.previous {
            Some(previous) => decrypt_with(previous, ciphertext, nonce_bytes)
                .map(Opened::Previous)
                .map_err(|_| format!("{current_error} under the current and the previous key")),
            None => Err(current_error),
        }
    }

    /// Cache a decrypted secret in memory, adding it or replacing its value.
    ///
    /// A tenant may hold any number of secrets; nothing forces a count.
    pub fn cache_secret(&self, tenant: &str, key: &str, value: String) {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        let tenant_secrets = cache.entry(tenant.to_string()).or_default();
        tenant_secrets.insert(key.to_string(), value);
    }

    /// Cache a decrypted secret only if the tenant has none cached under
    /// `key`, so a value a request cached meanwhile is kept. Returns whether
    /// it was cached.
    pub fn cache_secret_if_absent(&self, tenant: &str, key: &str, value: String) -> bool {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        let tenant_secrets = cache.entry(tenant.to_string()).or_default();
        if tenant_secrets.contains_key(key) {
            return false;
        }
        tenant_secrets.insert(key.to_string(), value);
        true
    }

    /// Cache a shared platform secret in memory.
    ///
    /// Platform secrets act as a baseline for all tenants but are not
    /// associated with any specific tenant ID.
    pub fn cache_platform_secret(&self, key: &str, value: String) {
        let mut platform = self
            .platform
            .write()
            .expect("platform secrets lock poisoned");
        platform.insert(key.to_string(), value);
    }

    /// Get a platform secret value.
    pub fn get_platform_secret(&self, key: &str) -> Option<String> {
        let platform = self
            .platform
            .read()
            .expect("platform secrets lock poisoned");
        platform.get(key).cloned()
    }

    /// Get all platform secrets.
    pub fn get_platform_secrets(&self) -> BTreeMap<String, String> {
        let platform = self
            .platform
            .read()
            .expect("platform secrets lock poisoned");
        platform.clone()
    }

    /// Get a single secret value for a tenant.
    ///
    /// Falls back to the platform secrets layer so shared infrastructure
    /// configuration remains available until a tenant overrides it.
    pub fn get_secret(&self, tenant: &str, key: &str) -> Option<String> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        cache
            .get(tenant)
            .and_then(|secrets| secrets.get(key).cloned())
            .or_else(|| self.get_platform_secret(key))
    }

    /// Remove a secret from the in-memory cache.
    pub fn remove_secret(&self, tenant: &str, key: &str) -> bool {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        cache
            .get_mut(tenant)
            .map(|secrets| secrets.remove(key).is_some())
            .unwrap_or(false)
    }

    /// List secret key names for a tenant (never values).
    ///
    /// Platform secrets are included because they are visible through the
    /// same fallback path as tenant secrets.
    pub fn list_keys(&self, tenant: &str) -> Vec<String> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        let platform = self
            .platform
            .read()
            .expect("platform secrets lock poisoned");
        let mut keys = BTreeSet::new();
        keys.extend(platform.keys().cloned());
        if let Some(secrets) = cache.get(tenant) {
            keys.extend(secrets.keys().cloned());
        }
        keys.into_iter().collect()
    }

    /// Get all decrypted secrets for a tenant (for WASM host injection).
    ///
    /// Platform secrets act as a baseline and tenant-local secrets override
    /// them when present.
    pub fn get_tenant_secrets(&self, tenant: &str) -> BTreeMap<String, String> {
        let mut merged = self.get_platform_secrets();
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        if let Some(tenant_secrets) = cache.get(tenant) {
            merged.extend(tenant_secrets.clone());
        }
        merged
    }
}

/// Which master key opened a stored row, with its plaintext.
#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    /// The current key: the row stays as stored.
    Current(Vec<u8>),
    /// Only the previous key: the row must be sealed again under the current one.
    Previous(Vec<u8>),
}

fn decrypt_with(
    cipher: &Aes256Gcm,
    ciphertext: &[u8],
    nonce_bytes: &[u8],
) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != 12 {
        return Err(format!(
            "invalid nonce length: expected 12, got {}",
            nonce_bytes.len()
        ));
    }
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("decryption failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let vault = SecretsVault::new(&test_key());
        let plaintext = b"super-secret-api-key";
        let (ciphertext, nonce) = vault.encrypt(plaintext).unwrap();
        let decrypted = vault.decrypt(&ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn different_nonce_per_encryption() {
        let vault = SecretsVault::new(&test_key());
        let (_, nonce1) = vault.encrypt(b"test").unwrap();
        let (_, nonce2) = vault.encrypt(b"test").unwrap();
        assert_ne!(nonce1, nonce2, "each encryption should use a unique nonce");
    }

    #[test]
    fn cache_and_retrieve() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_secret("tenant-a", "API_KEY", "sk-123".into());
        assert_eq!(
            vault.get_secret("tenant-a", "API_KEY"),
            Some("sk-123".into())
        );
        assert_eq!(vault.get_secret("tenant-a", "MISSING"), None);
        assert_eq!(vault.get_secret("tenant-b", "API_KEY"), None);
    }

    #[test]
    fn platform_secret_fallback() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_platform_secret("API_KEY", "sk-platform".into());

        assert_eq!(
            vault.get_secret("tenant-b", "API_KEY"),
            Some("sk-platform".into())
        );
    }

    #[test]
    fn tenant_overrides_platform() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_platform_secret("API_KEY", "sk-platform".into());
        vault.cache_secret("tenant-a", "API_KEY", "sk-tenant".into());

        assert_eq!(
            vault.get_secret("tenant-a", "API_KEY"),
            Some("sk-tenant".into())
        );
    }

    #[test]
    fn a_tenant_keeps_every_secret_it_sets() {
        // The cache refused a tenant's 101st secret until this was removed.
        let vault = SecretsVault::new(&test_key());
        for i in 0..150 {
            vault.cache_secret("t", &format!("key-{i}"), format!("v{i}"));
            vault.cache_platform_secret(&format!("platform-{i}"), format!("p{i}"));
        }
        assert_eq!(vault.get_secret("t", "key-149"), Some("v149".into()));
        assert_eq!(
            vault.get_platform_secret("platform-149"),
            Some("p149".into())
        );
        assert_eq!(vault.get_platform_secrets().len(), 150);

        vault.cache_secret("t", "key-0", "updated".into());
        assert_eq!(vault.get_secret("t", "key-0"), Some("updated".into()));
    }

    #[test]
    fn remove_secret_works() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_secret("t", "k", "v".into());
        assert!(vault.remove_secret("t", "k"));
        assert!(!vault.remove_secret("t", "k")); // already removed
        assert_eq!(vault.get_secret("t", "k"), None);
    }

    #[test]
    fn list_keys_returns_names_only() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_platform_secret("GLOBAL", "platform".into());
        vault.cache_secret("t", "B_KEY", "val-b".into());
        vault.cache_secret("t", "A_KEY", "val-a".into());
        let keys = vault.list_keys("t");
        assert_eq!(keys, vec!["A_KEY", "B_KEY", "GLOBAL"]); // BTree order
    }

    #[test]
    fn platform_secrets_merged() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_platform_secret("GLOBAL", "base".into());
        vault.cache_secret("t", "K1", "V1".into());
        vault.cache_secret("t", "K2", "V2".into());
        vault.cache_secret("t", "GLOBAL", "override".into());
        let secrets = vault.get_tenant_secrets("t");
        assert_eq!(secrets.len(), 3);
        assert_eq!(secrets["GLOBAL"], "override");
        assert_eq!(secrets["K1"], "V1");
        assert_eq!(secrets["K2"], "V2");
    }

    #[test]
    fn no_default_special_casing() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_platform_secret("API_KEY", "sk-platform".into());
        vault.cache_secret("default", "API_KEY", "sk-default".into());
        vault.cache_secret("tenant-a", "OTHER", "value".into());

        assert_eq!(
            vault.get_secret("default", "API_KEY"),
            Some("sk-default".into())
        );
        assert_eq!(
            vault.get_secret("default", "OTHER"),
            None,
            "default should not inherit another tenant's secrets"
        );
        assert_eq!(
            vault.get_secret("tenant-b", "API_KEY"),
            Some("sk-platform".into())
        );
    }

    #[test]
    fn invalid_nonce_length_fails() {
        let vault = SecretsVault::new(&test_key());
        let result = vault.decrypt(b"ciphertext", b"short");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid nonce length"));
    }

    #[test]
    fn previous_key_opens_old_rows_and_never_seals() {
        let old = SecretsVault::new(&[0x01; 32]);
        let (old_ct, old_nonce) = old.encrypt(b"old-value").unwrap();
        let (stray_ct, stray_nonce) = SecretsVault::new(&[0x03; 32]).encrypt(b"x").unwrap();

        let rotated = SecretsVault::new(&[0x02; 32]).with_previous_key(&[0x01; 32]);
        assert_eq!(
            rotated.open(&old_ct, &old_nonce),
            Ok(Opened::Previous(b"old-value".to_vec()))
        );
        let (new_ct, new_nonce) = rotated.encrypt(b"new-value").unwrap();
        assert_eq!(
            rotated.open(&new_ct, &new_nonce),
            Ok(Opened::Current(b"new-value".to_vec()))
        );
        assert!(
            old.decrypt(&new_ct, &new_nonce).is_err(),
            "a rotated vault seals with the current key only"
        );
        assert!(rotated.open(&stray_ct, &stray_nonce).is_err());

        let without_previous = SecretsVault::new(&[0x02; 32]);
        assert!(without_previous.open(&old_ct, &old_nonce).is_err());
    }
}
