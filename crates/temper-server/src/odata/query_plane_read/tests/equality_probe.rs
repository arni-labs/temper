//! Regression for complete equality reconciliation beyond the candidate probe.
use super::dst_projection_lag::{SimQueryPlane, sim_state};
use super::equality_delta::notes;
use super::*;
use std::collections::BTreeSet;
use std::sync::Arc;
use temper_runtime::scheduler::install_deterministic_context;

// Regression for the candidate probe becoming a 413 despite a one-row page.
// Include a pending-only match beyond the probe and a stale indexed match that
// must be removed. Expected pages/counts come from the acknowledged actor states.
#[tokio::test]
async fn dst_equality_delta_exceeding_probe_preserves_complete_results() {
    for seed in 0..64 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let qp = Arc::new(SimQueryPlane::default());
        let (state, _) = sim_state(seed, qp.clone());
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("equality-probe");
        let mut expected = Vec::new();
        let mut pending = BTreeSet::new();
        for i in 0..24 {
            let id = format!("ord-{i:02}");
            let created = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &id,
                    "AddItem",
                    serde_json::json!({"ProductId": "p", "Quantity": 1,
                        "Notes": if i == 23 { "noise" } else { "needle" }}),
                    &agent,
                )
                .await
                .unwrap();
            assert!(created.success);
            let value = if i == 0 { "noise" } else { "needle" };
            if i == 0 || i == 23 || (seed % 2 == 1 && i >= 12) {
                let key = (tenant.to_string(), "Order".into(), id.clone());
                // Keep the old index: the last id becomes a new match.
                let old = qp.catalog.lock().unwrap().get(&key).unwrap().clone();
                let updated = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Order",
                        &id,
                        "AddItem",
                        serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": value}),
                        &agent,
                    )
                    .await
                    .unwrap();
                assert!(updated.success);
                qp.catalog.lock().unwrap().insert(key, old);
                pending.insert(id.clone());
            }
            if value == "needle" {
                expected.push(id);
            }
        }
        let pending_count = pending.len();
        *qp.pending.lock().unwrap() = Some(pending);
        let security = SecurityContext::system();
        let descending = seed % 3 == 0;
        let mut expected = expected.clone();
        if descending {
            expected.reverse();
        }
        for value in ["needle", "absent"] {
            let options = QueryOptions {
                top: Some(1),
                skip: Some((seed % 23) as usize),
                orderby: Some(vec![OrderByClause {
                    property: "Id".into(),
                    direction: if descending {
                        OrderDirection::Desc
                    } else {
                        OrderDirection::Asc
                    },
                }]),
                ..notes(value)
            };
            let result = read_entity_set_page(QueryPlaneReadRequest {
                state: &state,
                tenant: &tenant,
                security_ctx: &security,
                entity_type: "Order",
                entity_set_name: "Orders",
                query_options: &options,
                budget: QueryPlaneReadBudget {
                    default_page_size: 1,
                    max_entities: 1,
                },
            })
            .await
            .unwrap_or_else(|_| panic!("seed {seed}: probe size must not reject a page"));
            assert_eq!(
                result.count,
                Some(if value == "needle" { 23 } else { 0 }),
                "seed {seed}"
            );
            assert_eq!(result.entities.len(), usize::from(value == "needle"));
            assert_eq!(
                result.telemetry.candidate_count,
                if value == "needle" { 24 } else { pending_count },
                "seed {seed}: hydrate only projected matches plus the complete delta"
            );
            if value == "needle" {
                assert_eq!(
                    result.entities[0]["entity_id"],
                    expected[(seed % 23) as usize]
                );
            }
        }
    }
}

#[tokio::test]
async fn equality_delta_eleven_matches_and_pending_nonmatch_answer_top_one() {
    for match_count in [11, 270] {
        let dir = tempfile::tempdir().unwrap();
        let store = TursoEventStore::new(
            &format!("file:{}", dir.path().join("probe.db").display()),
            None,
        )
        .await
        .unwrap();
        let state = build_order_state_with_store("delta-probe", &store);
        let tenant = TenantId::default();
        for i in 0..=match_count {
            let id = format!("ord-{i:03}");
            let created = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &id,
                    "AddItem",
                    serde_json::json!({"ProductId": "p", "Quantity": 1,
                "Notes": if i == match_count { "noise" } else { "needle" }}),
                    &AgentContext::for_service("delta-probe"),
                )
                .await
                .unwrap();
            assert!(created.success);
        }
        store
            .remove_query_projection(tenant.as_str(), "Order", &format!("ord-{match_count:03}"))
            .await
            .unwrap();
        let options = QueryOptions {
            top: Some(1),
            skip: (match_count > 11).then_some(match_count - 1),
            ..notes("needle")
        };
        let security = SecurityContext::system();
        for caught_up in [false, true] {
            if caught_up {
                let updated = state
                    .dispatch_tenant_action(
                        &tenant,
                        "Order",
                        &format!("ord-{match_count:03}"),
                        "AddItem",
                        serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": "noise"}),
                        &AgentContext::for_service("delta-probe"),
                    )
                    .await
                    .unwrap();
                assert!(updated.success);
            }
            for count in [None, Some(true)] {
                let options = QueryOptions {
                    count,
                    ..options.clone()
                };
                let result = read_entity_set_page(QueryPlaneReadRequest {
                    state: &state,
                    tenant: &tenant,
                    security_ctx: &security,
                    entity_type: "Order",
                    entity_set_name: "Orders",
                    query_options: &options,
                    budget: QueryPlaneReadBudget {
                        default_page_size: 1,
                        max_entities: 1,
                    },
                })
                .await
                .unwrap_or_else(|error| {
                    let detail = match error {
                        QueryPlaneReadError::QueryTooLarge { telemetry } => {
                            format!("QueryTooLarge: {telemetry:?}")
                        }
                        QueryPlaneReadError::ReadUnavailable => "ReadUnavailable".to_string(),
                        _ => "unexpected query error".to_string(),
                    };
                    panic!(
                        "{match_count} matches, caught_up {caught_up}, count {count:?}: {detail}"
                    )
                });
                assert_eq!(result.count, count.map(|_| match_count));
                assert_eq!(result.entities.len(), 1);
                assert_eq!(
                    result.entities[0]["entity_id"],
                    if match_count == 11 {
                        "ord-000".to_string()
                    } else {
                        format!("ord-{:03}", match_count - 1)
                    }
                );
                if !caught_up {
                    assert_eq!(result.telemetry.candidate_count, match_count + 1);
                }
            }
        }
    }
}
