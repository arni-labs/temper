//! Regression: loading identical on-disk specs must preserve a passed result.
use super::*;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};

const IOA: &str = r#"[automaton]
name = "Order"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]
[[action]]
name = "Touch"
from = ["Ready"]
to = "Ready"
"#;

fn result(passed: bool) -> EntityVerificationResult {
    EntityVerificationResult {
        all_passed: passed,
        levels: vec![EntityLevelSummary {
            level: "L0".into(),
            passed,
            summary: "persisted evidence".into(),
            details: None,
        }],
        verified_at: "2026-10-01T00:00:00Z".into(),
    }
}

fn reload(status: VerificationStatus, changed: bool) -> SpecRegistry {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("model.csdl.xml"),
        include_str!("../../../../test-fixtures/specs/model.csdl.xml"),
    )
    .unwrap();
    let path = directory.path().join("order.ioa.toml");
    std::fs::write(&path, IOA).unwrap();
    let mut registry = SpecRegistry::new();
    let tenant = TenantId::new("application");
    load_into_registry(
        &mut registry,
        directory.path().to_str().unwrap(),
        "application",
    )
    .unwrap();
    registry.set_verification_status(&tenant, "Order", status);
    if changed {
        std::fs::write(&path, format!("{IOA}\n# changed spec\n")).unwrap();
    }
    load_into_registry(
        &mut registry,
        directory.path().to_str().unwrap(),
        "application",
    )
    .unwrap();
    registry
}

#[test]
fn identical_disk_reload_preserves_completed_evidence() {
    let registry = reload(VerificationStatus::Completed(result(true)), false);
    let status = registry
        .get_verification_status(&TenantId::new("application"), "Order")
        .unwrap();
    assert!(
        status.is_passed(),
        "identical reload must retain verification success"
    );
    let VerificationStatus::Completed(evidence) = status else {
        panic!("preserve original evidence kind")
    };
    assert_eq!(evidence.verified_at, "2026-10-01T00:00:00Z");
    assert_eq!(evidence.levels[0].summary, "persisted evidence");
}

#[test]
fn identical_disk_reload_preserves_restored_evidence() {
    let registry = reload(VerificationStatus::Restored(result(true)), false);
    assert!(
        matches!(registry.get_verification_status(&TenantId::new("application"), "Order"), Some(VerificationStatus::Restored(r)) if r.all_passed)
    );
}

#[test]
fn changed_disk_reload_requires_verification() {
    let registry = reload(VerificationStatus::Completed(result(true)), true);
    assert!(matches!(
        registry.get_verification_status(&TenantId::new("application"), "Order"),
        Some(VerificationStatus::Pending)
    ));
}

#[test]
fn disk_reload_does_not_promote_unverified_specs() {
    for status in [
        VerificationStatus::Pending,
        VerificationStatus::Running,
        VerificationStatus::Completed(result(false)),
    ] {
        let registry = reload(status, false);
        assert!(matches!(
            registry.get_verification_status(&TenantId::new("application"), "Order"),
            Some(VerificationStatus::Pending)
        ));
        assert!(
            registry
                .get_verification_status(&TenantId::new("other"), "Order")
                .is_none()
        );
    }
}
