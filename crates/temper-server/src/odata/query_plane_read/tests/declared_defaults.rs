use super::*;

#[tokio::test]
async fn indexed_sort_with_declared_defaults_stays_bounded_when_values_are_present() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("indexed-default.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = build_order_state("indexed-declared-default");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let spec = format!(
        "{ORDER_IOA}\n[[state]]\nname = \"Currency\"\ntype = \"string\"\ninitial = \"USD\"\n"
    );
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", &spec)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    // More records than the read-source scan budget below. Every sort value
    // is indexed, so declaring an initial value must not force a full scan.
    for index in 0..11 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("ord-{index:02}"),
            serde_json::json!({"Currency": if index == 0 { "EUR" } else { "USD" }}),
            1,
        )
        .await;
    }
    let security = SecurityContext::system();
    let options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "Currency".into(),
            direction: OrderDirection::Asc,
        }]),
        top: Some(1),
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Currency".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("EUR".into()))),
        }),
        ..QueryOptions::default()
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
        panic!(
            "complete indexed sort failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], "ord-00");
    assert_eq!(result.count, None);
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    let mut options = options;
    options.filter = Some(FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Currency".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("USD".into()))),
    });
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
        panic!(
            "default-matching candidate page failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], "ord-01");
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
}
