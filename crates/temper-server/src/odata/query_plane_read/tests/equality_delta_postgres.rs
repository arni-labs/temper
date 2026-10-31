//! Indexed equality reads on Postgres read the matches and the durable
//! projection gap, never every row of the type.
use super::equality_delta::notes;
use super::*;
use crate::storage::QueryPlaneStore;
use temper_store_postgres::{PostgresEventStore, PostgresSchema};

async fn read_orders(state: &ServerState, value: &str) -> QueryPlaneReadResult {
    let options = notes(value);
    let tenant = TenantId::default();
    let security = SecurityContext::system();
    read_entity_set_page(QueryPlaneReadRequest {
        state,
        tenant: &tenant,
        security_ctx: &security,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    .unwrap_or_else(|_| panic!("an equality read of {value} must answer"))
}

fn entity_ids(result: &QueryPlaneReadResult) -> Vec<String> {
    let mut ids: Vec<_> = result
        .entities
        .iter()
        .map(|entity| entity["entity_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn equality_delta_on_postgres_declines_a_separately_supplied_journal() {
    // Never connects: the journal check answers before any query.
    let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
    let store = PostgresEventStore::new(pool);
    let journal =
        crate::storage::BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(27));
    let result = QueryPlaneStore::reconciled_equality_candidates(
        &store,
        &journal,
        "default",
        "Order",
        &[("Notes".into(), "needle".into())],
        Some(100),
    )
    .await
    .unwrap();
    assert!(
        result.is_none(),
        "a plane's empty delta says nothing about a different journal"
    );
}

/// Gated on DATABASE_URL; the store works in a schema of its own.
#[test]
fn equality_delta_on_postgres_reads_matches_and_gap_not_the_type() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        let name = format!("equality_delta_{}", sim_uuid().simple());
        let schema = PostgresSchema::new(name.clone()).unwrap();
        temper_store_postgres::migration::run_migrations_in_schema(&pool, &schema)
            .await
            .unwrap();
        let store = PostgresEventStore::with_schema(pool.clone(), schema);
        let mut state = build_order_state("equality-delta-postgres");
        state.set_storage_stack(StorageStack::from_postgres(store.clone()));
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("equality-delta-postgres");
        for i in 0..8 {
            let result = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &format!("ord-{i:02}"),
                    "AddItem",
                    serde_json::json!({"ProductId": "p", "Quantity": 1,
                        "Notes": if i < 3 { "needle" } else { "noise" }}),
                    &agent,
                )
                .await
                .unwrap();
            assert!(result.success, "fixture action must commit");
        }
        // A committed match whose catalog row is gone: the index cannot see it.
        store
            .remove_query_projection(tenant.as_str(), "Order", "ord-01")
            .await
            .unwrap();

        let lagging = read_orders(&state, "needle").await;
        assert_eq!(lagging.count, Some(3));
        assert_eq!(entity_ids(&lagging), vec!["ord-00", "ord-01", "ord-02"]);
        assert_eq!(
            lagging.telemetry.fallback_reason,
            QueryPlaneFallbackReason::ProjectionLagReconcile
        );
        assert_eq!(lagging.telemetry.coverage.missing, 1);
        assert!(
            lagging.telemetry.materialized_count <= 3,
            "hydrate matches plus the gap, not the type: {:?}",
            lagging.telemetry
        );
        let absent = read_orders(&state, "absent").await;
        assert!(absent.entities.is_empty());
        assert_eq!(absent.count, Some(0));
        assert!(
            absent.telemetry.materialized_count <= 1,
            "only the gap is read: {:?}",
            absent.telemetry
        );

        // Once the projection lands, an empty match hydrates nothing at all.
        let response = state
            .get_tenant_entity_state(&tenant, "Order", "ord-01")
            .await
            .unwrap();
        let fields = state.query_projection_fields(&tenant, "Order", &response.state.fields);
        let projected = state.query_projection_state(&response.state);
        QueryPlaneStore::upsert_projection(
            &store,
            tenant.as_str(),
            "Order",
            "ord-01",
            &response.state.status,
            &fields,
            &projected,
            response.state.sequence_nr,
        )
        .await
        .unwrap();
        let caught_up = read_orders(&state, "absent").await;
        assert!(caught_up.entities.is_empty());
        assert_eq!(caught_up.count, Some(0));
        assert_eq!(caught_up.telemetry.coverage.missing, 0);
        assert_eq!(caught_up.telemetry.materialized_count, 0);
        assert_eq!(
            caught_up.telemetry.fallback_reason,
            QueryPlaneFallbackReason::None
        );

        sqlx::query(&format!("DROP SCHEMA \"{name}\" CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
    });
}
