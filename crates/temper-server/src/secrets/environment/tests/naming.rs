use super::*;

#[test]
fn seeded_secret_reaches_every_tenant() {
    let vault = test_vault();

    let report = seed_platform_secrets_from_environment(
        &vault,
        vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]),
    );

    assert_eq!(report.seeded, vec!["build_token".to_string()]);
    assert!(report.skipped.is_empty());
    assert_eq!(vault.get_platform_secret("build_token"), Some("abc".into()));
    assert_eq!(
        vault.get_secret("tenant-a", "build_token"),
        Some("abc".into())
    );
    assert_eq!(
        vault.get_secret("tenant-b", "build_token"),
        Some("abc".into())
    );
    assert!(
        vault
            .list_keys("tenant-b")
            .contains(&"build_token".to_string())
    );
}

#[test]
fn several_prefixed_variables_are_all_seeded() {
    let vault = test_vault();

    let report = seed_platform_secrets_from_environment(
        &vault,
        vars(&[
            ("TEMPER_SECRET_REGION", "north"),
            ("TEMPER_SECRET_BUILD_TOKEN", "abc"),
            ("TEMPER_SECRET_KEY_2", "def"),
        ]),
    );

    assert_eq!(
        report.seeded,
        vec![
            "build_token".to_string(),
            "key_2".to_string(),
            "region".to_string()
        ]
    );
    assert_eq!(vault.get_platform_secret("build_token"), Some("abc".into()));
    assert_eq!(vault.get_platform_secret("key_2"), Some("def".into()));
    assert_eq!(vault.get_platform_secret("region"), Some("north".into()));
}

#[test]
fn empty_value_seeds_nothing_and_is_not_reported() {
    let vault = test_vault();

    let (report, log) = seed_with_log(
        &vault,
        vars(&[("TEMPER_SECRET_BUILD_TOKEN", ""), ("TEMPER_SECRET_", "")]),
    );

    assert_eq!(report, EnvironmentSecretsReport::default());
    assert_eq!(vault.get_platform_secret("build_token"), None);
    assert!(vault.get_platform_secrets().is_empty());
    assert_eq!(log, "");
}

#[test]
fn variables_without_the_prefix_are_ignored() {
    let vault = test_vault();

    let (report, log) = seed_with_log(
        &vault,
        vars(&[
            ("BUILD_TOKEN", "abc"),
            ("TEMPER_SECRET", "abc"),
            ("TEMPER_SECRETS_BUILD_TOKEN", "abc"),
            ("temper_secret_BUILD_TOKEN", "abc"),
            ("MY_TEMPER_SECRET_BUILD_TOKEN", "abc"),
        ]),
    );

    assert_eq!(report, EnvironmentSecretsReport::default());
    assert!(vault.get_platform_secrets().is_empty());
    assert_eq!(log, "");
}

#[test]
fn no_prefixed_variable_seeds_nothing_and_logs_nothing() {
    let vault = test_vault();

    let (report, log) = seed_with_log(&vault, vars(&[("HOME", "/home/user"), ("PATH", "/bin")]));

    assert_eq!(report, EnvironmentSecretsReport::default());
    assert!(vault.get_platform_secrets().is_empty());
    assert_eq!(log, "");
}

#[test]
fn badly_named_variables_are_skipped_and_each_reported_once_by_name() {
    let vault = test_vault();
    let bad_names = [
        "TEMPER_SECRET_",
        "TEMPER_SECRET_1TOKEN",
        "TEMPER_SECRET_BUILD-TOKEN",
        "TEMPER_SECRET_BUILD.TOKEN",
        "TEMPER_SECRET_Build_Token",
        "TEMPER_SECRET__TOKEN",
        "TEMPER_SECRET_build_token",
    ];
    let mut variables = vars(&[("TEMPER_SECRET_BUILD_TOKEN", "abc")]);
    variables.extend(
        bad_names
            .iter()
            .flat_map(|name| vars(&[(name, DISTINCT_VALUE)])),
    );

    let (report, log) = seed_with_log(&vault, variables);

    // The well-named variable beside them is still seeded.
    assert_eq!(report.seeded, vec!["build_token".to_string()]);
    assert_eq!(
        vault.get_platform_secrets().keys().collect::<Vec<_>>(),
        vec!["build_token"]
    );
    assert_eq!(
        report.skipped,
        bad_names
            .iter()
            .map(|name| (name.to_string(), EnvironmentSecretSkip::InvalidName))
            .collect::<Vec<_>>()
    );
    for name in bad_names {
        let lines: Vec<&str> = log
            .lines()
            .filter(|line| line.contains(&format!("variable={name} ")))
            .collect();
        assert_eq!(lines.len(), 1, "{name} must be reported once: {log}");
        assert!(lines[0].contains("WARN"), "{log}");
    }
    assert_eq!(log.lines().count(), bad_names.len() + 1, "{log}");
}

#[cfg(unix)]
#[test]
fn value_that_is_not_utf8_is_skipped_and_reported_by_name() {
    use std::os::unix::ffi::OsStringExt;

    let vault = test_vault();
    let variables = vec![(
        OsString::from("TEMPER_SECRET_BUILD_TOKEN"),
        OsString::from_vec(vec![0x61, 0xff, 0x62]),
    )];

    let (report, log) = seed_with_log(&vault, variables);

    assert!(report.seeded.is_empty());
    assert_eq!(
        report.skipped,
        vec![(
            "TEMPER_SECRET_BUILD_TOKEN".to_string(),
            EnvironmentSecretSkip::ValueNotUnicode
        )]
    );
    assert_eq!(vault.get_platform_secret("build_token"), None);
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("variable=TEMPER_SECRET_BUILD_TOKEN "), "{log}");
}

#[cfg(unix)]
#[test]
fn name_that_is_not_utf8_is_skipped_and_reported() {
    use std::os::unix::ffi::OsStringExt;

    let vault = test_vault();
    let mut name = b"TEMPER_SECRET_BUILD".to_vec();
    name.push(0xff);
    let variables = vec![(OsString::from_vec(name), OsString::from("abc"))];

    let report = seed_platform_secrets_from_environment(&vault, variables);

    assert!(report.seeded.is_empty());
    assert_eq!(
        report.skipped,
        vec![(
            "TEMPER_SECRET_BUILD\u{fffd}".to_string(),
            EnvironmentSecretSkip::InvalidName
        )]
    );
    assert!(vault.get_platform_secrets().is_empty());
}

#[test]
fn value_over_the_size_limit_is_skipped_and_reported_by_name() {
    let vault = test_vault();
    let limit = crate::secrets::vault::MAX_SECRET_VALUE_BYTES;
    let at_limit = "a".repeat(limit);
    let over_limit = "b".repeat(limit + 1);

    let (report, log) = seed_with_log(
        &vault,
        vars(&[
            ("TEMPER_SECRET_AT_LIMIT", &at_limit),
            ("TEMPER_SECRET_OVER_LIMIT", &over_limit),
        ]),
    );

    assert_eq!(report.seeded, vec!["at_limit".to_string()]);
    assert_eq!(
        report.skipped,
        vec![(
            "TEMPER_SECRET_OVER_LIMIT".to_string(),
            EnvironmentSecretSkip::ValueTooLarge
        )]
    );
    assert_eq!(vault.get_platform_secret("at_limit"), Some(at_limit));
    assert_eq!(vault.get_platform_secret("over_limit"), None);
    assert!(log.contains("variable=TEMPER_SECRET_OVER_LIMIT "), "{log}");
    assert!(!log.contains("bbbb"), "the log must not contain the value");
}
