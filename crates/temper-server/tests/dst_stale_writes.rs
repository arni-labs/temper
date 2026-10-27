//! Seeded simulations of writes racing on one record.
//!
//! An external write is decided on the record as it was read (Cedar and the
//! write's other checks) and may land only on that state: the record's actor
//! refuses it when the record changed first (the authorization precondition).
//! Each seed runs several writes on one record at once, in one single-threaded
//! execution against the simulated journal. The cooperative yield points take
//! a seeded number of turns, so the writes reach the record in a different
//! order on each seed, and the same seed replays the same execution.
//!
//! The invariants:
//! - A write its policy allows on the state it lands on lands, however many
//!   writes race it: N allowed moves at once all land, and the count is N.
//! - No write lands on a state its policy refuses: a move allowed only while
//!   the record's project is Open never lands after the record was pointed at
//!   a Closed project, though it was decided while the project was Open.
//! - A write is never answered "decided on a state that changed": the caller
//!   gets the answer for the record as it is. The same holds for field updates
//!   and deletes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::join_all;
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::buggify::install_buggify_context;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Stale" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Tally">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="last" Type="Edm.String"/>
        <Property Name="project_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Project">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="Bump" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Tally"/>
        <Parameter Name="last" Type="Edm.String"/>
      </Action>
      <Action Name="Repoint" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Tally"/>
        <Parameter Name="project_id" Type="Edm.String"/>
      </Action>
      <Action Name="Close" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Project"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Tallies" EntityType="Temper.Stale.Tally"/>
        <EntitySet Name="Projects" EntityType="Temper.Stale.Project"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Bump` counts and writes who bumped last, so every bump changes the fields
/// Cedar reads. Cedar also sees the status of the record's project
/// (`ctx_project_status`), which `Repoint` changes.
const TALLY_IOA: &str = r#"
[automaton]
name = "Tally"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[state]]
name = "last"
type = "string"
initial = ""

[[state]]
name = "project_id"
type = "string"
initial = ""

[[context_entity]]
name = "project"
entity_type = "Project"
id_field = "project_id"

[[action]]
name = "Bump"
kind = "input"
from = ["Open"]
to = "Open"
params = ["last"]
effect = ["count += 1"]

[[action]]
name = "Repoint"
kind = "input"
from = ["Open"]
to = "Open"
params = ["project_id"]
"#;

const PROJECT_IOA: &str = r#"
[automaton]
name = "Project"
states = ["Open", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "Closed"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
"#;

/// Everyone may do everything, except that Mallory may bump a tally only while
/// its project is Open.
const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal == Customer::"mallory", action == Action::"Bump", resource)
unless { resource has ctx_project_status && resource.ctx_project_status == "Open" };
"#;

/// Seeds explored on every run, after the regressions.
const EXPLORED_SEEDS: u64 = 48;

/// Seeds that once broke an invariant, kept forever (none yet).
const REGRESSION_SEEDS: &[u64] = &[];

/// How many allowed moves race on one record.
const RACING_MOVES: usize = 6;

/// How many of Mallory's moves race the record's repoint.
const RACING_ATTACKS: usize = 3;

fn seeds() -> impl Iterator<Item = u64> {
    let explored = (0..EXPLORED_SEEDS).filter(|seed| !REGRESSION_SEEDS.contains(seed));
    REGRESSION_SEEDS.iter().copied().chain(explored)
}

fn customer(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(id, PrincipalKind::Customer, None, None, None, None)
}

fn server(seed: u64, scenario: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let entities = [("Tally", TALLY_IOA), ("Project", PROJECT_IOA)];
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &entities,
    );
    for (entity_type, _) in &entities {
        registry.set_verification_status(
            &TenantId::default(),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-10-27T00:00:00Z".to_string(),
            }),
        );
    }
    let store = SimEventStore::no_faults(seed);
    let name = format!("dst-stale-writes-{scenario}-{seed}");
    let mut state = ServerState::from_registry(ActorSystem::new(&name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    state
}

async fn send(
    state: &ServerState,
    as_who: &str,
    mut request: Request<Body>,
) -> (StatusCode, Value) {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            customer(as_who),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(state: &ServerState, as_who: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, as_who, request).await
}

async fn create(state: &ServerState, set: &str, body: Value) -> Result<(), String> {
    let (status, body) = post(state, "owner", &format!("/tdata/{set}"), body).await;
    if status != StatusCode::CREATED {
        return Err(format!("creating in {set}: {status} {body}"));
    }
    Ok(())
}

async fn act(state: &ServerState, as_who: &str, path: &str, body: Value) -> (StatusCode, Value) {
    post(state, as_who, &format!("/tdata/{path}"), body).await
}

async fn tally(state: &ServerState, id: &str) -> temper_server::EntityState {
    state
        .get_tenant_entity_state(&TenantId::default(), "Tally", id)
        .await
        .expect("the tally should be readable")
        .state
}

/// N allowed moves at once on one record: every one lands, and the count is N.
async fn allowed_moves_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "moves");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_MOVES).map(|i| format!("c{i}")).collect();
    let answers = join_all(who.iter().map(|id| {
        act(
            &state,
            id,
            "Tallies('t1')/Temper.Stale.Bump",
            json!({ "last": id }),
        )
    }))
    .await;

    let refused: Vec<String> = who
        .iter()
        .zip(&answers)
        .filter(|(_, (status, _))| *status != StatusCode::OK)
        .map(|(id, (status, body))| format!("{id}: {status} {body}"))
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "{} of {RACING_MOVES} allowed moves did not land: {}",
            refused.len(),
            refused.join("; ")
        ));
    }
    let record = tally(&state, "t1").await;
    let count = record.counters.get("count").copied().unwrap_or_default();
    if count != RACING_MOVES {
        return Err(format!(
            "{RACING_MOVES} moves landed but the count is {count}"
        ));
    }
    Ok(())
}

/// Mallory's moves, each allowed only while the tally's project is Open, race
/// the owner pointing the tally at a Closed project.
async fn linked_record_attack(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "attack");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Projects", json!({ "Id": "p2" })).await?;
    let (status, body) = act(
        &state,
        "owner",
        "Projects('p2')/Temper.Stale.Close",
        json!({}),
    )
    .await;
    if status != StatusCode::OK {
        return Err(format!("closing p2: {status} {body}"));
    }
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let attempts: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("m{i}")).collect();
    let (repointed, attacks) = tokio::join!(
        act(
            &state,
            "owner",
            "Tallies('t1')/Temper.Stale.Repoint",
            json!({ "project_id": "p2" }),
        ),
        join_all(attempts.iter().map(|last| {
            act(
                &state,
                "mallory",
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": last }),
            )
        })),
    );

    let mut trace = vec![format!("repoint: {}", repointed.0)];
    trace.extend(
        attempts
            .iter()
            .zip(&attacks)
            .map(|(last, (status, _))| format!("{last}: {status}")),
    );
    if repointed.0 != StatusCode::OK {
        return Err(format!(
            "the owner's repoint did not land: {} {}; {}",
            repointed.0,
            repointed.1,
            trace.join(", ")
        ));
    }
    if let Some((last, (status, body))) = attempts
        .iter()
        .zip(&attacks)
        .find(|(_, (status, _))| *status != StatusCode::OK && *status != StatusCode::FORBIDDEN)
    {
        return Err(format!(
            "Mallory's {last} was answered {status} {body}, neither landed nor refused by \
             the policy; {}",
            trace.join(", ")
        ));
    }

    let record = tally(&state, "t1").await;
    let actions: Vec<&str> = record.events.iter().map(|e| e.action.as_str()).collect();
    let Some(repoint_at) = actions.iter().position(|a| *a == "Repoint") else {
        return Err(format!("no Repoint in the record's moves {actions:?}"));
    };
    if actions[repoint_at..].contains(&"Bump") {
        return Err(format!(
            "Mallory's move landed after the tally's project was Closed: moves {actions:?}; {}",
            trace.join(", ")
        ));
    }
    let landed = attacks.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    let bumps = actions.iter().filter(|a| **a == "Bump").count();
    if landed != bumps {
        return Err(format!(
            "{landed} bumps answered OK but {bumps} landed: {actions:?}"
        ));
    }
    Ok(())
}

/// N field updates at once on one record: every one lands.
async fn field_updates_race(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "patch");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_MOVES).map(|i| format!("c{i}")).collect();
    let answers = join_all(who.iter().map(|id| {
        let request = Request::patch("/tdata/Tallies('t1')")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({ "last": id }).to_string()))
            .unwrap();
        send(&state, id, request)
    }))
    .await;

    let refused: Vec<String> = who
        .iter()
        .zip(&answers)
        .filter(|(_, (status, _))| !status.is_success())
        .map(|(id, (status, body))| format!("{id}: {status} {body}"))
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "{} of {RACING_MOVES} allowed field updates did not land: {}",
            refused.len(),
            refused.join("; ")
        ));
    }
    let record = tally(&state, "t1").await;
    let last = record
        .fields
        .get("last")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !who.iter().any(|id| id == last) {
        return Err(format!(
            "after the updates `last` is {last:?}, none of theirs"
        ));
    }
    Ok(())
}

/// The owner deletes a record while others move it: the delete lands.
async fn delete_races_moves(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = server(seed, "delete");
    create(&state, "Projects", json!({ "Id": "p1" })).await?;
    create(&state, "Tallies", json!({ "Id": "t1", "project_id": "p1" })).await?;

    let _faults = install_buggify_context(seed);
    let who: Vec<String> = (0..RACING_ATTACKS).map(|i| format!("c{i}")).collect();
    let delete = Request::delete("/tdata/Tallies('t1')")
        .body(Body::empty())
        .unwrap();
    let (deleted, _moves) = tokio::join!(
        send(&state, "owner", delete),
        join_all(who.iter().map(|id| {
            act(
                &state,
                id,
                "Tallies('t1')/Temper.Stale.Bump",
                json!({ "last": id }),
            )
        })),
    );
    if !deleted.0.is_success() {
        return Err(format!(
            "the owner's delete did not land: {} {}",
            deleted.0, deleted.1
        ));
    }
    Ok(())
}

#[tokio::test]
async fn dst_stale_writes_every_allowed_move_on_a_busy_record_lands() {
    for seed in seeds() {
        if let Err(violation) = allowed_moves_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_a_move_never_lands_on_a_state_its_policy_refuses() {
    for seed in seeds() {
        if let Err(violation) = linked_record_attack(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_every_allowed_field_update_on_a_busy_record_lands() {
    for seed in seeds() {
        if let Err(violation) = field_updates_race(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_stale_writes_a_delete_lands_while_the_record_is_being_moved() {
    for seed in seeds() {
        if let Err(violation) = delete_races_moves(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}
