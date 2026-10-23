//! Public object keys and ids for published artifacts.

use temper_runtime::tenant::TenantId;

use crate::aws_sigv4::sha256_hex;
use crate::blob_store::TENANT_NAMESPACES_DIR;

/// The public object key of an artifact: `{namespace}/tenants/{sha256 of tenant}/{owner_ref_type}/{owner_ref_id}/
/// {label}-{hash}.{ext}`. A public bucket may be shared by every tenant on a server, so the key names the publishing
/// tenant, and two tenants that choose the same namespace, owner refs and label write different objects. Artifacts
/// published before the tenant segment keep their keys and URLs; only new publishes use this layout.
pub(super) fn public_storage_key(
    tenant: &TenantId,
    namespace: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
    mime_type: &str,
) -> Result<String, String> {
    let hash = content_hash.trim_start_matches("sha256:");
    validate_path_segment("namespace", namespace)?;
    validate_path_segment("owner_ref_type", owner_ref_type)?;
    validate_path_segment("owner_ref_id", owner_ref_id)?;
    validate_path_segment("label", label)?;
    validate_path_segment("content_hash", hash)?;
    let tenant_segment = sha256_hex(tenant.as_str().as_bytes());
    Ok(format!(
        "{namespace}/{TENANT_NAMESPACES_DIR}/{tenant_segment}/{owner_ref_type}/{owner_ref_id}/{label}-{hash}.{}",
        extension_for_mime(mime_type)
    ))
}

/// Refuse bytes that do not hash to the content hash recorded for them. The public key carries that hash, so a key
/// only ever holds the bytes it names.
pub(super) fn check_content_hash(
    source_display: &str,
    content_hash: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let recorded = content_hash.trim_start_matches("sha256:");
    if recorded != sha256_hex(bytes) {
        return Err(format!(
            "{source_display} bytes do not match its content hash '{content_hash}'"
        ));
    }
    Ok(())
}

pub(super) fn published_artifact_id(
    tenant: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
) -> String {
    let seed = format!("{tenant}\n{owner_ref_type}\n{owner_ref_id}\n{label}\n{content_hash}");
    let digest = sha256_hex(seed.as_bytes());
    format!("part-{}", &digest[..32])
}

pub(super) fn validate_path_segment(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value == "." || value == ".." {
        return Err(format!("{field} must not be a relative path segment"));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~'))
    {
        return Err(format!(
            "{field} must be one URI-safe path segment using only letters, digits, '-', '_', '.' or '~'"
        ));
    }
    Ok(())
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type.split(';').next().unwrap_or("").trim() {
        "text/html" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" => "js",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/json" => "json",
        "text/markdown" => "md",
        _ => "bin",
    }
}
