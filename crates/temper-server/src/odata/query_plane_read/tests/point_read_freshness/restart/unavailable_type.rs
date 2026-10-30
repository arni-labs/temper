//! An unreadable type must not hold unrelated tenants in the boot retry loop.
use super::*;
use crate::platform_store::{PlatformStore, SimPlatformStore};
use crate::registry_bootstrap::restore_registry_from_platform_store;
use temper_runtime::persistence::EventStore;

const UNAVAILABLE: &str = "a-unreadable-app";
const BROKEN_IOA: &str = "this is not an IOA spec";

#[tokio::test(start_paused = true)]
async fn an_unreadable_type_with_a_lagging_row_withholds_only_its_tenant() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(166);
    let events = SimEventStore::no_faults(166);
    let qp = Arc::new(FlakyQueryPlane {
        fail_write: usize::MAX,
        ..Default::default()
    });
    // Produce real journals, then leave their projections one event behind.
    let source = server(&events, qp.clone(), Arc::new(Rows::default()));
    source.registry.write().unwrap().register_tenant(
        UNAVAILABLE,
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", IOA)],
    );
    source
        .authz
        .reload_tenant_policies(UNAVAILABLE, "permit(principal, action, resource);")
        .unwrap();
    for name in [UNAVAILABLE, TENANT] {
        let tenant = TenantId::new(name);
        let created = source
            .get_or_create_tenant_entity(&tenant, "Order", "release", serde_json::json!({}))
            .await
            .unwrap();
        let moved = source
            .dispatch_tenant_action(
                &tenant,
                "Order",
                "release",
                "Review",
                serde_json::json!({}),
                &AgentContext::for_service("unreadable-type-regression"),
            )
            .await
            .unwrap();
        assert!(moved.success, "{:?}", moved.error);
        qp.inner
            .upsert_projection(
                name,
                "Order",
                "release",
                &created.state.status,
                &created.state.fields,
                &serde_json::to_value(&created.state).unwrap(),
                created.state.sequence_nr,
            )
            .await
            .unwrap();
    }
    let before = qp
        .load_entity_catalog_rows(UNAVAILABLE, "Order", &["release".into()])
        .await
        .unwrap()
        .unwrap();
    let pid = format!("{UNAVAILABLE}:Order:release");
    let journal = events.read_events(&pid, 0).await.unwrap();
    assert!(before[0].sequence_nr < journal.last().unwrap().sequence_nr);

    // Real restoration keeps the unreadable Order on disk, omits its replay
    // table, and still registers this tenant's readable Task type.
    let specs = SimPlatformStore::no_faults(166);
    let task_ioa = IOA.replace("Order", "Task");
    let task_csdl = CSDL_XML.replace("Order", "Task");
    for (tenant, ty, ioa, csdl) in [
        (UNAVAILABLE, "Order", BROKEN_IOA, CSDL_XML),
        (UNAVAILABLE, "Task", task_ioa.as_str(), task_csdl.as_str()),
        (TENANT, "Order", IOA, CSDL_XML),
    ] {
        specs
            .upsert_spec(
                tenant,
                ty,
                ioa,
                csdl,
                &temper_store_turso::spec_content_hash(ioa),
            )
            .await
            .unwrap();
    }
    for tenant in [UNAVAILABLE, TENANT] {
        specs.commit_specs(tenant).await.unwrap();
    }
    let mut registry = SpecRegistry::new();
    assert_eq!(
        restore_registry_from_platform_store(&mut registry, &specs)
            .await
            .unwrap(),
        2
    );
    let unavailable = TenantId::new(UNAVAILABLE);
    assert!(registry.get_table(&unavailable, "Order").is_none());
    assert!(registry.get_table(&unavailable, "Task").is_some());
    assert_eq!(
        registry.resolve_entity_type(&unavailable, "Orders"),
        Some("Order".to_string())
    );
    let restarted = server(&events, qp.clone(), Arc::new(Rows::default()));
    *restarted.registry.write().unwrap() = registry;
    // A new process has neither resident actors nor saved timer rows.
    assert!(
        restarted
            .resident_entity_sequence(&TenantId::new(TENANT), "Order", "release")
            .await
            .is_none()
    );
    let walk = tokio::time::timeout(Duration::from_secs(5), restarted.start_runtime())
        .await
        .expect("an unavailable type must not block healthy tenants at boot")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), walk)
        .await
        .unwrap()
        .unwrap();

    let response = crate::odata::unavailable_tenant_response(&restarted, &unavailable)
        .expect("the affected tenant must be explicitly withheld");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    // Withholding is in memory only: neither stored rows, journals nor specs
    // are deleted or rewritten to get the healthy tenant through boot.
    let after = qp
        .load_entity_catalog_rows(UNAVAILABLE, "Order", &["release".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after[0].sequence_nr, before[0].sequence_nr);
    assert_eq!(after[0].state, before[0].state);
    let after_journal = events.read_events(&pid, 0).await.unwrap();
    assert_eq!(after_journal.len(), journal.len());
    assert_eq!(
        after_journal.last().unwrap().payload,
        journal.last().unwrap().payload
    );
    assert!(
        specs
            .load_specs()
            .await
            .unwrap()
            .iter()
            .any(|r| r.tenant == UNAVAILABLE
                && r.entity_type == "Order"
                && r.ioa_source == BROKEN_IOA)
    );
    let healthy = TenantId::new(TENANT);
    assert!(crate::odata::unavailable_tenant_response(&restarted, &healthy).is_none());
    let body =
        try_load_entity_body_from_catalog(&restarted, &healthy, "Order", "Orders", "release", true)
            .await
            .expect("healthy tenant serves its repaired catalog");
    assert_eq!(body["status"], "Checked");
    assert_eq!(body["sequence_nr"], 2);
    assert_eq!(
        restarted.state_timeout_tracker.pending_snapshot(),
        vec![("Order".to_string(), 1)]
    );
}
