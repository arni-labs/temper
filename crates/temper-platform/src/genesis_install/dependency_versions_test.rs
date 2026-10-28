use std::collections::BTreeMap;

use super::*;

/// Answers from a fixed table keyed by (base, head); a missing pair is a
/// registry that could not answer.
struct Fixed(BTreeMap<(String, String), VersionComparison>);

impl VersionAncestry for Fixed {
    async fn compare(
        &self,
        _owner: &str,
        _name: &str,
        base: &str,
        head: &str,
    ) -> Result<VersionComparison, String> {
        self.0
            .get(&(base.to_string(), head.to_string()))
            .copied()
            .ok_or_else(|| "returned 404 Not Found".to_string())
    }
}

fn record(name: &str, source_kind: &str, version: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        app_name: name.to_string(),
        source_kind: source_kind.to_string(),
        version_hash: version.to_string(),
        ..InstalledAppRecord::default()
    }
}

#[test]
fn only_a_genesis_install_at_another_version_is_compared() {
    let genesis = record("shared", "genesis", "b2");
    assert_eq!(version_to_compare(Some(&genesis), "b1", false), Some("b2"));
    assert_eq!(
        version_to_compare(Some(&genesis), "b2", false),
        None,
        "same version"
    );
    assert_eq!(
        version_to_compare(Some(&genesis), "@b2", false),
        None,
        "same version"
    );
    assert_eq!(version_to_compare(None, "b1", false), None, "not installed");
    let local = record("shared", "local", "");
    assert_eq!(
        version_to_compare(Some(&local), "b1", false),
        None,
        "not from Genesis"
    );
    assert_eq!(
        version_to_compare(Some(&genesis), "b1", true),
        None,
        "named for downgrade: moves to the pin without asking"
    );
}

#[test]
fn the_rule_installs_upgrades_keeps_newer_and_refuses_the_rest() {
    use VersionComparison::*;
    assert_eq!(dependency_move(&Ok(Ahead)), DependencyMove::Install);
    assert_eq!(dependency_move(&Ok(Identical)), DependencyMove::Install);
    assert_eq!(dependency_move(&Ok(Behind)), DependencyMove::Keep);
    for refused in [Ok(Diverged), Ok(Unknown), Err("timed out".to_string())] {
        assert!(
            matches!(dependency_move(&refused), DependencyMove::Refuse(_)),
            "{refused:?} must refuse"
        );
    }
}

fn pinned<'a>(owner: &'a str, name: &'a str, version: &'a str) -> PinnedApp<'a> {
    PinnedApp {
        owner,
        name,
        version,
    }
}

#[tokio::test]
async fn a_plan_keeps_newer_dependencies_and_names_every_refusal() {
    let ancestry = Fixed(BTreeMap::from([
        (("b2".into(), "b1".into()), VersionComparison::Behind),
        (("d1".into(), "d2".into()), VersionComparison::Ahead),
        (("e9".into(), "e1".into()), VersionComparison::Diverged),
    ]));
    let installed = [
        record("root", "genesis", "r0"),
        record("kept", "genesis", "b2"),
        record("upgraded", "genesis", "d1"),
        record("forked", "genesis", "e9"),
        record("unreadable", "genesis", "f9"),
    ];
    let closure = [
        pinned("acme", "root", "r1"),
        pinned("acme", "kept", "b1"),
        pinned("acme", "upgraded", "d2"),
        pinned("acme", "forked", "e1"),
        pinned("acme", "unreadable", "f1"),
        pinned("acme", "fresh", "g1"),
    ];

    let refusal = plan_dependency_moves(&ancestry, "root", &closure, &installed, &BTreeSet::new())
        .await
        .expect_err("diverged and unreadable dependencies refuse");
    assert!(
        refusal.starts_with("refused before any change: "),
        "{refusal}"
    );
    for named in [
        "dependency acme/forked is installed at e9 and the closure pins e1: neither version descends from the other",
        "dependency acme/unreadable is installed at f9 and the closure pins f1: the registry's compare failed: returned 404 Not Found",
    ] {
        assert!(refusal.contains(named), "{refusal}");
    }
    assert!(
        !refusal.contains("acme/kept") && !refusal.contains("acme/root"),
        "{refusal}"
    );

    let downgrade =
        parse_downgrade_dependencies(&["acme/forked".to_string(), " acme/unreadable ".to_string()])
            .expect("owner/name entries");
    let kept = plan_dependency_moves(&ancestry, "root", &closure, &installed, &downgrade)
        .await
        .expect("named dependencies move to the pin");
    assert_eq!(
        kept,
        vec![KeptDependency {
            app: "acme/kept".to_string(),
            installed_version: "b2".to_string(),
            pinned_version: "b1".to_string(),
            name: "kept".to_string(),
        }]
    );
}

#[test]
fn a_downgrade_entry_must_name_owner_and_app() {
    for bad in [
        "paw-agent",
        "temperpaw/",
        "/paw-agent",
        "a/b/c",
        "temperpaw/paw-agent@abc",
    ] {
        assert!(
            parse_downgrade_dependencies(&[bad.to_string()]).is_err(),
            "{bad} must be refused"
        );
    }
    assert!(parse_downgrade_dependencies(&["temperpaw/paw-agent".to_string()]).is_ok());
}
