//! ADR-0183: `principal in` answered from `GroupMembership` records.
//!
//! The real path: agent specs bootstrapped into a tenant on a Turso file,
//! memberships written through the normal dispatch path, decisions from the
//! tenant's Cedar, and a second process on the same database that fills the
//! membership set from the stored records before its first check.

use std::collections::{BTreeMap, HashMap};

use temper_authz::groups::group_membership_id;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_store_turso::TursoEventStore;

const TENANT: &str = "default";
const APP: &str = r#"KatagamiApp::"a1""#;
const POLICY: &str = r#"permit(principal, action == Action::"read", resource is KatagamiApp) when { principal in resource };"#;

async fn booted(db_url: &str) -> PlatformState {
    let turso = TursoEventStore::new(db_url, None)
        .await
        .expect("open the membership database");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_turso(turso));
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, TENANT, false, &cache);
    state
        .server
        .authz
        .reload_tenant_policies(TENANT, POLICY)
        .expect("tenant policy parses");
    state
}

async fn act(state: &PlatformState, member: &str, action: &str) {
    let id = group_membership_id(APP, member).expect("group is a Cedar uid");
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "GroupMembership",
            &id,
            action,
            serde_json::json!({ "group": APP, "member": member }),
            &AgentContext::system(),
        )
        .await
        .unwrap_or_else(|error| panic!("{action} for {member}: {error}"));
    assert!(
        response.success,
        "{action} for {member}: {:?}",
        response.error
    );
}

fn reads(state: &PlatformState, who: &str) -> bool {
    let person =
        SecurityContext::from_verified_jwt(who, PrincipalKind::Customer, None, None, None, None);
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("a1"))]);
    state
        .server
        .authz
        .authorize_for_tenant(TENANT, &person, "read", "KatagamiApp", &attrs)
        .is_allowed()
}

#[tokio::test]
async fn a_membership_record_decides_principal_in_now_and_after_a_restart() {
    let db_url = format!(
        "file:/tmp/temper-group-membership-{}.db",
        uuid::Uuid::new_v4()
    );
    let state = booted(&db_url).await;
    let tenant = TenantId::new(TENANT);

    act(&state, "alice", "StartMembership").await;
    assert!(
        !reads(&state, "alice"),
        "before the tenant is filled nobody is a member"
    );
    state
        .server
        .fill_group_memberships(&tenant)
        .await
        .expect("fill");
    assert!(reads(&state, "alice"), "an active member reads the app");
    assert!(!reads(&state, "bob"), "a non-member does not");

    act(&state, "bob", "StartMembership").await;
    assert!(
        reads(&state, "bob"),
        "a join counts from the transition that commits it"
    );
    act(&state, "bob", "EndMembership").await;
    assert!(!reads(&state, "bob"), "an ended membership counts no more");

    // A record whose id is not sha256(group, member) makes nobody a member.
    let forged = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "GroupMembership",
            "forged-id",
            "StartMembership",
            serde_json::json!({ "group": APP, "member": "mallory" }),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch");
    assert!(forged.success);
    assert!(!reads(&state, "mallory"));

    let restarted = booted(&db_url).await;
    assert!(
        !reads(&restarted, "alice"),
        "a new process answers no until it has read the records"
    );
    restarted
        .server
        .fill_group_memberships(&tenant)
        .await
        .expect("fill after restart");
    assert!(
        reads(&restarted, "alice"),
        "alice's membership was read back"
    );
    assert!(
        !reads(&restarted, "bob"),
        "bob's ended membership stays ended"
    );
    assert!(!reads(&restarted, "mallory"));
}
