//! Which keys resolve inside a store's own namespace.
//!
//! Every tenant but `default` keeps its objects under `tenants/<sha256 of its name>`, below the same local root and
//! bucket prefix the default tenant uses for its own objects. Older default-tenant objects already sit at that root,
//! so the default tenant keeps it, and the `tenants` directory below it belongs to the other tenants: no
//! default-tenant key may begin there.

use percent_encoding::percent_decode_str;

use super::{BlobStore, BlobStoreBackend, S3BlobStore};

/// The directory, below a deployment's blob root or bucket prefix, that holds every non-default tenant's namespace.
pub(crate) const TENANT_NAMESPACES_DIR: &str = "tenants";

impl BlobStore {
    /// Mark this store as one whose root also holds the other tenants' namespaces (the default tenant's), so none of
    /// its keys may begin in [`TENANT_NAMESPACES_DIR`].
    pub(crate) fn holding_tenant_namespaces(mut self) -> Self {
        self.holds_tenant_namespaces = true;
        self
    }

    /// Refuse a key that would resolve outside this store's own namespace. Every operation that takes a key calls
    /// this first.
    pub(super) fn check_key(&self, key: &str) -> Result<(), String> {
        let (resolved, folds_names) = match &self.backend {
            // `local_blob_path` refuses empty, dot and backslash segments, so the key is the path below the root.
            BlobStoreBackend::LocalFs { .. } => (key.to_string(), true),
            BlobStoreBackend::S3(store) => (store.resolved_key(key)?, false),
        };
        if !self.holds_tenant_namespaces {
            return Ok(());
        }
        let first_segment = resolved.split('/').next().unwrap_or_default();
        // A local root may be on a case-insensitive filesystem that also folds Unicode forms (the macOS default), so
        // there any spelling that could name the same directory is refused: other cases, and names outside ASCII.
        let names_tenant_namespaces = first_segment.eq_ignore_ascii_case(TENANT_NAMESPACES_DIR)
            || (folds_names && !first_segment.is_ascii());
        if names_tenant_namespaces {
            return Err(format!(
                "invalid blob key '{key}': '{TENANT_NAMESPACES_DIR}/' holds other tenants' objects"
            ));
        }
        Ok(())
    }
}

impl S3BlobStore {
    /// The object key the store sees for `key`: the request path as the HTTP client sends it (dot segments removed,
    /// `\` read as `/`), percent-decoded the way the store decodes it, below this store's bucket and prefix.
    fn resolved_key(&self, key: &str) -> Result<String, String> {
        let invalid = || format!("invalid blob key '{key}'");
        let decoded_path = |url: &str| {
            reqwest::Url::parse(url)
                .map(|url| {
                    percent_decode_str(url.path())
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .map_err(|_| invalid())
        };
        let base = decoded_path(&self.object_url(""))?;
        let full = decoded_path(&self.object_url(key))?;
        let resolved = full.strip_prefix(&base).ok_or_else(invalid)?;
        if resolved.is_empty()
            || resolved
                .split('/')
                .any(|segment| segment == "." || segment == "..")
        {
            return Err(invalid());
        }
        Ok(resolved.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(prefix: Option<&str>) -> BlobStore {
        BlobStore::s3(
            "http://127.0.0.1:9",
            "bucket",
            None,
            None,
            "/tmp",
            prefix.map(str::to_string),
        )
    }

    #[test]
    fn the_default_tenants_keys_never_begin_in_the_other_tenants_directory() {
        let other = format!("{TENANT_NAMESPACES_DIR}/0a1b/records/value.json");
        for store in [
            BlobStore::local_fs("/tmp/blobs").holding_tenant_namespaces(),
            s3(None).holding_tenant_namespaces(),
            s3(Some("app")).holding_tenant_namespaces(),
        ] {
            assert!(store.check_key("records/value.json").is_ok());
            assert!(store.check_key("published/tenants/a.md").is_ok());
            for key in [
                other.clone(),
                other.replacen("tenants", "Tenants", 1),
                other.replacen("tenants", "TENANTS", 1),
            ] {
                assert!(store.check_key(&key).is_err(), "{key} for {store:?}");
            }
        }
    }

    #[test]
    fn a_local_default_store_refuses_names_a_filesystem_could_fold_onto_the_tenants_directory() {
        let store = BlobStore::local_fs("/tmp/blobs").holding_tenant_namespaces();
        // U+017F LATIN SMALL LETTER LONG S case-folds to "s".
        assert!(store.check_key("tenant\u{17f}/0a1b/value").is_err());
        assert!(store.check_key("café/value").is_err());
        assert!(store.check_key("notes/café").is_ok());
    }

    #[test]
    fn an_object_store_key_is_checked_as_the_store_will_resolve_it() {
        let store = s3(Some("app")).holding_tenant_namespaces();
        for key in [
            "tenants%2F0a1b%2Fvalue",
            "x/../tenants/0a1b/value",
            "x/%2e%2e/tenants/0a1b/value",
            "tenants\\0a1b\\value",
            "../escape",
            "%2e%2e/escape",
        ] {
            assert!(store.check_key(key).is_err(), "{key}");
        }
    }

    #[test]
    fn other_tenants_stores_keep_any_key_inside_their_own_namespace() {
        let local = BlobStore::local_fs("/tmp/blobs/tenants/0a1b");
        let remote = s3(Some("tenants/0a1b"));
        for store in [local, remote] {
            assert!(store.check_key("tenants/inner/value").is_ok());
        }
        assert!(s3(Some("tenants/0a1b")).check_key("../escape").is_err());
    }
}
