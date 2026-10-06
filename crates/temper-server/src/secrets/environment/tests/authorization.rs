use super::*;

#[test]
fn permitted_module_reads_a_secret_seeded_from_a_prefixed_variable() {
    let vault = test_vault();

    seed_platform_secrets_from_environment(&vault, vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]));

    assert_eq!(
        module_reads(vault, MODULE, "build_token"),
        Ok("abc".to_string())
    );
}

#[test]
fn module_without_permission_is_still_refused_a_seeded_secret() {
    let vault = test_vault();
    seed_platform_secrets_from_environment(&vault, vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]));

    let refused = module_reads(vault, "another-module", "build_token")
        .expect_err("a module the policies do not name must be refused");

    assert!(
        refused.contains("authorization denied for secret 'build_token'"),
        "{refused}"
    );
    assert!(!refused.contains("abc"), "{refused}");
}

#[test]
fn permitted_module_is_refused_a_seeded_secret_its_policy_does_not_name() {
    let vault = test_vault();
    seed_platform_secrets_from_environment(
        &vault,
        vars(&[
            ("TEMPER_SECRET_BUILD_TOKEN", "abc"),
            ("TEMPER_SECRET_DEPLOY_TOKEN", "def"),
        ]),
    );

    let refused =
        module_reads(vault, MODULE, "deploy_token").expect_err("the policy names build_token only");

    assert!(
        refused.contains("authorization denied for secret 'deploy_token'"),
        "{refused}"
    );
}

#[test]
fn integration_config_template_resolves_a_seeded_secret_like_any_other() {
    let vault = test_vault();
    seed_platform_secrets_from_environment(&vault, vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]));
    let config = std::collections::BTreeMap::from([(
        "authorization".to_string(),
        "Bearer {secret:build_token}".to_string(),
    )]);

    // Templates are resolved from the vault when an integration runs, for
    // every secret the tenant can read, without the `access_secret` check
    // that `get_secret` goes through.
    let resolved = crate::secrets::resolve_secret_templates(&config, &vault, "tenant-b");

    assert_eq!(resolved["authorization"], "Bearer abc");
}
