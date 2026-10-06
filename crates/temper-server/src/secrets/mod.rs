//! Tenant secret management: encrypted storage, template resolution and
//! platform secrets supplied through the environment.

pub mod environment;
pub mod template;
pub mod vault;

pub use environment::seed_platform_secrets_from_environment;
pub use template::resolve_secret_templates;
pub use vault::SecretsVault;
