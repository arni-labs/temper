//! Composite writes must preserve the same trusted-input boundary as OData.
use super::*;
use temper_authz::{PrincipalKind, SecurityContext};

fn owned_state(store: Option<SimEventStore>) -> ServerState {
    let child = r#"
[automaton]
name="Child"
states=["Draft","Active"]
initial="Draft"
strict_action_params=true
[[state]]
name="Name"
type="string"
initial=""
[[action]]
name="Create"
from=["Draft"]
to="Active"
params=[{name="Name",type="string",source="authenticated_subject"}]
"#;
    let specs = BTreeMap::from([
        ("Parent".into(), PARENT_IOA.into()),
        ("Child".into(), child.into()),
    ]);
    let system = ActorSystem::new("composite-authenticated-params");
    let csdl = parse_csdl(COMPOSITE_CSDL).unwrap();
    let state = match store {
        Some(store) => ServerState::with_storage_stack(
            system,
            csdl,
            COMPOSITE_CSDL.into(),
            specs,
            StorageStack::from_sim(store, None),
        ),
        None => ServerState::with_specs(system, csdl, COMPOSITE_CSDL.into(), specs),
    }
    .unwrap();
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(principal is Agent, action == Action::"Create", resource is Child)
when { context has actingFor && context.actingFor == "alice" };
"#,
        )
        .unwrap();
    state
}

#[tokio::test]
async fn authenticated_parameters_bind_before_atomic_and_fallback_composite_writes() {
    for seed in 0..16 {
        for durable in [false, true] {
            let store = durable.then(|| SimEventStore::no_faults(seed));
            let state = owned_state(store);
            let tenant = TenantId::default();
            let ctx = AgentContext {
                security_ctx: Some(SecurityContext::from_verified_jwt(
                    "caller",
                    PrincipalKind::Agent,
                    Some("test"),
                    Some("alice"),
                    None,
                    None,
                )),
                ..Default::default()
            };
            for params in [json!({"Name":"bob"}), json!({"Name":"alice"})] {
                let result = state.apply_composite_integration_result(
                    &tenant,"Parent","parent","CreateChild",
                    &json!({"sub_writes":[{"entity_type":"Child","entity_id":"child","action":"Create","params":params}]}),&ctx,
                ).await;
                assert!(
                    result.is_err(),
                    "seed {seed}, durable={durable}: caller override accepted"
                );
                assert!(!state.ensure_entity_loaded(&tenant, "Child", "child").await);
            }
            assert!(state.apply_composite_integration_result(
                &tenant,"Parent","parent","CreateChild",
                &json!({"sub_writes":[{"entity_type":"Child","entity_id":"child","action":"Create","params":{}}]}),&ctx,
            ).await.unwrap(),"seed {seed}, durable={durable}");
            let child = state
                .get_tenant_entity_state(&tenant, "Child", "child")
                .await
                .unwrap();
            assert_eq!(
                child.state.fields["Name"], "alice",
                "seed {seed}, durable={durable}"
            );
        }
    }
}
