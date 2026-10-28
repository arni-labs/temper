//! Replace-mode removal leaves history behind without disabling current types.
use super::*;
use crate::platform_store::{PlatformStore, SimPlatformStore};
use crate::registry_bootstrap::restore_registry_from_platform_store;
use temper_runtime::persistence::EventStore;

fn restart_after_type_removal(seed: u64, cut: usize) {
    let clock = Arc::new(ProcessClock::new());
    let _context = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let events = SimEventStore::no_faults(seed);
    let specs = SimPlatformStore::no_faults(seed);
    let qp = Arc::new(FlakyQueryPlane {
        fail_write: usize::MAX,
        ..Default::default()
    });
    let rows = Arc::new(Rows::default());
    let tenant = TenantId::new(TENANT);
    let task_ioa = IOA.replace("Order", "Task");
    let task_csdl = CSDL_XML.replace("Order", "Task");
    let first = runtime();
    first.block_on(async {
        clock.begin();
        let source = server(&events, qp.clone(), rows.clone());
        source.registry.write().unwrap().try_register_tenant_with_constraints(
            TENANT, parse_csdl(&task_csdl).unwrap(), task_csdl.clone(),
            &[("Task", &task_ioa)], None, true,
        ).unwrap();
        for (ty, ioa, csdl) in [("Order", IOA, CSDL_XML), ("Task", task_ioa.as_str(), task_csdl.as_str())] {
            specs.upsert_spec(TENANT, ty, ioa, csdl, &temper_store_turso::spec_content_hash(ioa)).await.unwrap();
        }
        specs.commit_specs(TENANT).await.unwrap();
        // Both types have real journals. The remaining type also needs repair.
        let created = source.get_or_create_tenant_entity(&tenant, "Task", "current", serde_json::json!({})).await.unwrap();
        let moved = source.dispatch_tenant_action(&tenant, "Task", "current", "Review", serde_json::json!({}), &AgentContext::for_service("retirement-regression")).await.unwrap();
        assert!(moved.success, "{:?}", moved.error);
        if seed.is_multiple_of(2) {
            qp.remove_projection(TENANT, "Task", "current").await.unwrap();
        } else {
            qp.inner.upsert_projection(TENANT, "Task", "current", &created.state.status, &created.state.fields,
                &serde_json::to_value(&created.state).unwrap(), created.state.sequence_nr).await.unwrap();
        }
        // Exercise both a saved caller and a missing timer for the current type.
        if seed.is_multiple_of(2) { rows.rows.lock().unwrap().remove("current"); }
        source.get_or_create_tenant_entity(&tenant, "Order", "retired", serde_json::json!({})).await.unwrap();
        if cut == 0 { qp.hang_sequence.store(2, Ordering::SeqCst); }
        else { rows.cut.store(cut, Ordering::SeqCst); }
        let ctx = AgentContext::for_service("retirement-regression");
        {
            let dispatch = source.dispatch_tenant_action(&tenant, "Order", "retired", "Review", serde_json::json!({}), &ctx);
            tokio::pin!(dispatch);
            tokio::select! {
                result = &mut dispatch => panic!("seed {seed}, cut {cut}: dispatch escaped: {result:?}"),
                _ = async { if cut == 0 { qp.hanging.notified().await } else { rows.hanging.notified().await } } => {}
            }
        }
        // Replace mode retires Order's spec and routes, preserving its data.
        source.registry.write().unwrap().register_tenant(
            TENANT, parse_csdl(&task_csdl).unwrap(), task_csdl.clone(), &[("Task", &task_ioa)],
        );
        assert!(source.registry.read().unwrap().get_table(&tenant, "Order").is_none());
        assert!(source.registry.read().unwrap().resolve_entity_type(&tenant, "Orders").is_none());
        specs.delete_spec(TENANT, "Order").await.unwrap();
        specs.upsert_spec(TENANT, "Task", &task_ioa, &task_csdl, &temper_store_turso::spec_content_hash(&task_ioa)).await.unwrap();
        specs.commit_specs(TENANT).await.unwrap();
        clock.end(Duration::from_secs(600));
    });
    drop(first);
    qp.hang_sequence.store(0, Ordering::SeqCst);
    rows.cut.store(0, Ordering::SeqCst);
    runtime().block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        assert_eq!(
            restore_registry_from_platform_store(&mut registry, &specs)
                .await
                .unwrap(),
            1
        );
        assert!(registry.get_table(&tenant, "Order").is_none());
        assert!(registry.resolve_entity_type(&tenant, "Orders").is_none());
        assert_eq!(
            registry.resolve_entity_type(&tenant, "Tasks"),
            Some("Task".to_string())
        );
        let restarted = server(&events, qp.clone(), rows.clone());
        *restarted.registry.write().unwrap() = registry;
        let before = qp
            .load_entity_catalog_rows(TENANT, "Order", &["retired".into()])
            .await
            .unwrap()
            .unwrap();
        let pid = format!("{TENANT}:Order:retired");
        let journal = events.read_events(&pid, 0).await.unwrap();
        assert_eq!(journal.last().unwrap().payload["to_status"], "Checked");
        if cut == 0 {
            assert!(before[0].sequence_nr < journal.last().unwrap().sequence_nr);
        }
        let walk = tokio::time::timeout(Duration::from_secs(5), restarted.start_runtime())
            .await
            .expect("retired history must not block boot")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), walk)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::odata::unavailable_tenant_response(&restarted, &tenant).is_none(),
            "seed {seed}, cut {cut}: retired type must not disable remaining app"
        );
        assert!(
            restarted
                .resident_entity_sequence(&tenant, "Task", "current")
                .await
                .is_none()
        );
        let body = try_load_entity_body_from_catalog(
            &restarted, &tenant, "Task", "Tasks", "current", true,
        )
        .await
        .expect("current type serves repaired catalog");
        assert_eq!(body["status"], "Checked");
        assert_eq!(body["sequence_nr"], 2);
        assert_eq!(
            restarted.state_timeout_tracker.pending_snapshot(),
            vec![("Task".to_string(), 1)]
        );
        let timers = rows.list_state_timers(TENANT).await.unwrap();
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].entity_type, "Task");
        let after = qp
            .load_entity_catalog_rows(TENANT, "Order", &["retired".into()])
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
        tokio::time::sleep(Duration::from_secs(1499)).await;
        let waiting = restarted
            .get_tenant_entity_state(&tenant, "Task", "current")
            .await
            .unwrap();
        assert_eq!(waiting.state.status, "Checked");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = restarted
            .get_tenant_entity_state(&tenant, "Task", "current")
            .await
            .unwrap();
        assert_eq!(after.state.status, "Done");
        assert_eq!(
            after.state.sequence_nr, 3,
            "remaining timer fires once at original deadline"
        );
    });
}

#[test]
fn replace_mode_removal_does_not_withhold_remaining_app_at_restart() {
    restart_after_type_removal(166, 0);
}

#[test]
fn dst_replace_mode_removal_after_interrupted_row_writes() {
    for seed in 0..32 {
        let cut = DeterministicRng::new(seed ^ 0x166).next_bound(3);
        restart_after_type_removal(seed, cut);
    }
}
