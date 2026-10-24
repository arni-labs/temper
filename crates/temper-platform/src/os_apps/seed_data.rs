//! Seed data: entities an app declares in `seed-data/*.toml` and creates on
//! first install.

use std::path::{Path, PathBuf};

use serde::Serialize;
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

/// A seed data instance to create on first install.
///
/// Parsed from `seed-data/*.toml` files using `[[instance]]` blocks.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct SeedInstance {
    /// Entity type name (must be a registered type).
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Optional explicit entity ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Fields to set on the entity.
    #[serde(default)]
    pub fields: serde_json::Value,
    /// Actions to dispatch after creation, in order.
    #[serde(default)]
    pub actions: Vec<SeedAction>,
}

/// An action to dispatch on a seed entity after creation.
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct SeedAction {
    /// Action name (e.g. "Activate", "Register").
    pub name: String,
    /// Action parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Container for parsing `seed-data/*.toml` files.
#[derive(Debug, serde::Deserialize)]
struct SeedFile {
    #[serde(rename = "instance", default)]
    instances: Vec<SeedInstance>,
}

/// Discover seed data instances from `seed-data/*.toml` files.
///
/// Each TOML file contains `[[instance]]` blocks that declare entities
/// to create on first install.
pub(super) fn find_seed_data(app_dir: &Path) -> Vec<SeedInstance> {
    let seed_dir = app_dir.join("seed-data");
    if !seed_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&seed_dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".toml"))
        .map(|e| e.path())
        .collect();
    files.sort();

    let mut all_instances = Vec::new();
    for path in &files {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<SeedFile>(&content) {
                Ok(seed_file) => all_instances.extend(seed_file.instances),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse seed data file"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read seed data file"
                );
            }
        }
    }
    all_instances
}

/// Bootstrap seed data instances into the tenant.
///
/// For each seed instance:
/// 1. Check if the entity type is registered
/// 2. Create the entity
/// 3. Dispatch each action in order
///
/// Returns descriptions of successfully created instances.
pub(super) async fn bootstrap_seed_data(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    instances: &[SeedInstance],
) -> Vec<String> {
    if instances.is_empty() {
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");
    let mut created = Vec::new();

    for instance in instances {
        // Check if entity type is registered.
        let type_exists = {
            let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(tenant_id, &instance.entity_type)
                .is_some()
        };
        if !type_exists {
            tracing::warn!(
                tenant,
                entity_type = %instance.entity_type,
                "Skipping seed instance — entity type not registered"
            );
            continue;
        }

        // Determine entity ID.
        let entity_id = instance.id.clone().unwrap_or_else(|| {
            // Generate a deterministic ID from type + fields.
            let hash_input = format!("{}-{}", instance.entity_type, instance.fields);
            format!(
                "seed-{}",
                &format!("{:x}", md5_like_hash(&hash_input))[..12]
            )
        });

        // Check if entity already exists (idempotent).
        if state
            .server
            .entity_exists(tenant_id, &instance.entity_type, &entity_id)
        {
            tracing::debug!(
                tenant,
                entity_type = %instance.entity_type,
                entity_id = %entity_id,
                "Seed entity already exists — skipping"
            );
            created.push(format!("{}({})", instance.entity_type, entity_id));
            continue;
        }

        // Create entity with initial fields.
        let initial_fields = if instance.fields.is_null() {
            serde_json::json!({})
        } else {
            instance.fields.clone()
        };

        match state
            .server
            .get_or_create_tenant_entity(
                tenant_id,
                &instance.entity_type,
                &entity_id,
                initial_fields,
            )
            .await
        {
            Ok(_) => {
                // Dispatch each action in order.
                for action in &instance.actions {
                    let params = if action.params.is_null() {
                        serde_json::json!({})
                    } else {
                        action.params.clone()
                    };
                    if let Err(e) = state
                        .server
                        .dispatch(temper_server::state::DispatchCommand {
                            tenant: tenant_id,
                            entity_type: &instance.entity_type,
                            entity_id: &entity_id,
                            action: &action.name,
                            params,
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        })
                        .await
                    {
                        tracing::warn!(
                            tenant,
                            entity_type = %instance.entity_type,
                            entity_id = %entity_id,
                            action = %action.name,
                            error = %e,
                            "Failed to dispatch seed action"
                        );
                    }
                }
                tracing::info!(
                    tenant,
                    entity_type = %instance.entity_type,
                    entity_id = %entity_id,
                    "Seed entity created"
                );
                created.push(format!("{}({})", instance.entity_type, entity_id));
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    entity_type = %instance.entity_type,
                    entity_id = %entity_id,
                    error = %e,
                    "Failed to create seed entity"
                );
            }
        }
    }
    created
}

/// Simple hash for generating deterministic seed entity IDs.
fn md5_like_hash(input: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}
