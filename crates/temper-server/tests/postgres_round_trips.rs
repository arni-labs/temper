//! What a move and a filtered list read cost in Postgres round trips.
//!
//! The server talks to Postgres through a wire proxy that counts the client
//! messages that wait for an answer: a simple `Query` ('Q') and a `Sync` ('S')
//! that ends an extended-protocol batch (sqlx's acquire-time ping is a bare
//! Sync). A move writes its event and every row it changes in one statement,
//! and a list read reads one page, so neither cost grows with the number of
//! records of the type.
//!
//! Gated on DATABASE_URL (skips without it, runs in CI); the store works in a
//! schema of its own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::{PostgresEventStore, PostgresSchema};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Panels" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Surface"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="Status" Type="Edm.String"/>
<Property Name="app" Type="Edm.String"/>
<Property Name="kind" Type="Edm.String"/>
<Property Name="body" Type="Edm.String"/>
</EntityType>
<Action Name="Keep" IsBound="true"><Parameter Name="bindingParameter" Type="Panels.Surface"/>
<Parameter Name="app" Type="Edm.String"/><Parameter Name="kind" Type="Edm.String"/></Action>
<Action Name="Edit" IsBound="true"><Parameter Name="bindingParameter" Type="Panels.Surface"/>
<Parameter Name="body" Type="Edm.String"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Surfaces" EntityType="Panels.Surface"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

const SURFACE: &str = r#"
[automaton]
name = "Surface"
states = ["Draft", "Kept"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Kept"]

[[action]]
name = "Keep"
kind = "input"
from = ["Draft"]
to = "Kept"
params = ["app", "kind"]

[[action]]
name = "Edit"
kind = "input"
from = ["Kept"]
params = ["body"]
"#;

const POLICY: &str = r#"permit(principal, action, resource is Surface);"#;

/// The filtered first page an app's view asks for: one app's kept surfaces of
/// one kind.
const LIST: &str = "/tdata/Surfaces?$filter=app%20eq%20'app-3'%20and%20status%20eq%20'Kept'%20and%20kind%20eq%20'react'&$top=1";

/// Counts the client messages that wait for an answer from the server, and
/// keeps each one's description: its type and the statements it runs.
#[derive(Clone, Default)]
struct RoundTrips(Arc<AtomicU64>, Arc<Mutex<Vec<String>>>);

impl RoundTrips {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// The counted messages from `from` on, one line each.
    fn since(&self, from: u64) -> Vec<String> {
        self.1.lock().unwrap()[from as usize..].to_vec()
    }
}

/// Forward every connection on a local port to `upstream`, counting the
/// round trips the client starts.
async fn proxy(upstream: (String, u16)) -> (u16, RoundTrips) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let trips = RoundTrips::default();
    let counted = trips.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let server = TcpStream::connect((upstream.0.as_str(), upstream.1))
                .await
                .unwrap();
            tokio::spawn(relay(client, server, counted.clone()));
        }
    });
    (port, trips)
}

async fn relay(client: TcpStream, server: TcpStream, trips: RoundTrips) {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
    });
    let mut frames = Frames::default();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match client_read.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        // Count before forwarding, so the answer cannot arrive first.
        let waits = frames.push(&chunk[..read]);
        if !waits.is_empty() {
            let mut log = trips.1.lock().unwrap();
            trips.0.fetch_add(waits.len() as u64, Ordering::SeqCst);
            log.extend(waits);
        }
        if server_write.write_all(&chunk[..read]).await.is_err() {
            return;
        }
    }
}

/// The frontend message stream: untyped messages (`[len][body]`) until the
/// startup message, then typed ones (`[type][len][body]`).
#[derive(Default)]
struct Frames {
    pending: Vec<u8>,
    started: bool,
    /// Each prepared statement's text, by name, as its Parse gave it.
    prepared: HashMap<String, String>,
    /// The statements of the extended-protocol batch a Sync will end.
    batch: Vec<String>,
}

/// The first `limit` characters of `text` on one line.
fn excerpt(text: &str, limit: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

/// The NUL-terminated strings at the start of `body`.
fn cstrings(body: &[u8], count: usize) -> Vec<String> {
    body.split(|byte| *byte == 0)
        .take(count)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

const SSL_REQUEST: u32 = 80_877_103;
const GSS_REQUEST: u32 = 80_877_104;

impl Frames {
    /// Take in `bytes` and describe each complete message among them that
    /// waits for an answer.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut waits = Vec::new();
        loop {
            let header = if self.started { 5 } else { 8 };
            if self.pending.len() < header {
                return waits;
            }
            let at = usize::from(self.started);
            let length = u32::from_be_bytes(self.pending[at..at + 4].try_into().unwrap()) as usize;
            assert!(length >= 4, "a frontend message length counts itself");
            if self.pending.len() < at + length {
                return waits;
            }
            if self.started {
                let body = &self.pending[5..at + length];
                match self.pending[0] {
                    b'Q' => waits.push(format!("Q {}", excerpt(&cstrings(body, 1)[0], 120))),
                    b'P' => {
                        let parts = cstrings(body, 2);
                        let text = excerpt(&parts[1], 120);
                        self.prepared.insert(parts[0].clone(), text.clone());
                        self.batch.push(format!("Parse {:?}: {text}", parts[0]));
                    }
                    b'B' => {
                        let name = cstrings(body, 2).remove(1);
                        let text = self.prepared.get(&name).cloned().unwrap_or_default();
                        self.batch.push(format!("Bind {name:?}: {text}"));
                    }
                    b'S' => {
                        let batch = std::mem::take(&mut self.batch);
                        waits.push(if batch.is_empty() {
                            "S (bare Sync: the pool's ping)".to_string()
                        } else {
                            format!("S [{}]", batch.join(" ; "))
                        });
                    }
                    _ => {}
                }
            } else {
                let code = u32::from_be_bytes(self.pending[4..8].try_into().unwrap());
                self.started = !matches!(code, SSL_REQUEST | GSS_REQUEST);
            }
            self.pending.drain(..at + length);
        }
    }
}

async fn server(url: &str, schema: &PostgresSchema) -> (ServerState, RoundTrips) {
    let options: PgConnectOptions = url.parse().unwrap();
    let upstream = (options.get_host().to_string(), options.get_port());
    let (port, trips) = proxy(upstream).await;
    // One connection: every statement goes over it in turn, so the count is
    // the requests' own and nothing opens a connection mid-measurement.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(
            options
                .host("127.0.0.1")
                .port(port)
                .ssl_mode(PgSslMode::Disable),
        )
        .await
        .unwrap();
    temper_store_postgres::migration::run_migrations_in_schema(&pool, schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool, schema.clone());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Surface", SURFACE)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("pg-round-trips"), registry);
    // Writes through the router wait for the spec to verify; this one is
    // marked verified, as the other router tests do.
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Surface",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-11-02T00:00:00Z".to_string(),
        }),
    );
    let mut stack = StorageStack::from_postgres(store);
    // No background writer shares the connection inside a counted window.
    // Trajectory entries are written in the background, off the request path,
    // so the sink is off. The snapshot queue writes a record only at its
    // 100-event boundary (`TEMPER_SNAPSHOT_INTERVAL`), which no record here
    // nears. The OTS outbox reads its queue once when the stack is attached;
    // `quiet` waits that out before the first record is made.
    stack.trajectory = None;
    state.set_storage_stack(stack);
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), POLICY)
        .unwrap();
    (state, trips)
}

fn authenticated(mut request: Request<Body>) -> Request<Body> {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "round-trips".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "postgres-round-trips".to_string(),
        cause: None,
        credential: None,
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

/// Wait until the connection has been idle for half a second, so no earlier
/// statement is still in flight when a window opens.
async fn quiet(trips: &RoundTrips) {
    let mut last = trips.now();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = trips.now();
        if now == last {
            return;
        }
        last = now;
    }
    panic!("the connection never went idle");
}

/// Send `request` and return its status and JSON body, and the round trips it
/// cost.
async fn send(
    state: &ServerState,
    trips: &RoundTrips,
    request: Request<Body>,
) -> (StatusCode, serde_json::Value, u64) {
    quiet(trips).await;
    let before = trips.now();
    let response = build_router(state.clone())
        .oneshot(authenticated(request))
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    let cost = trips.now() - before;
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body, cost)
}

async fn edit(state: &ServerState, trips: &RoundTrips, id: &str, body: &str) -> u64 {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/tdata/Surfaces('{id}')/Panels.Edit"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "body": body }).to_string()))
        .unwrap();
    let (status, response, cost) = send(state, trips, request).await;
    assert_eq!(status, StatusCode::OK, "Edit {id}: {response}");
    cost
}

async fn list(state: &ServerState, trips: &RoundTrips) -> u64 {
    let request = Request::builder().uri(LIST).body(Body::empty()).unwrap();
    let (status, body, cost) = send(state, trips, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page = body["value"].as_array().expect("a page of surfaces");
    assert_eq!(page.len(), 1, "{body}");
    assert_eq!(page[0]["app"], "app-3", "{body}");
    assert_eq!(page[0]["kind"], "react", "{body}");
    cost
}

/// Make surfaces `from..to`, kept, spread over ten apps and two kinds: the
/// tens alternate between the kinds, so every app holds both.
async fn keep_surfaces(state: &ServerState, from: usize, to: usize) {
    let tenant = TenantId::default();
    let agent = temper_server::request_context::AgentContext::for_service("round-trips");
    for i in from..to {
        let id = format!("s-{i:03}");
        state
            .get_or_create_tenant_entity(&tenant, "Surface", &id, json!({}))
            .await
            .unwrap();
        let kind = if (i / 10) % 2 == 0 { "react" } else { "html" };
        let kept = state
            .dispatch_tenant_action(
                &tenant,
                "Surface",
                &id,
                "Keep",
                json!({ "app": format!("app-{}", i % 10), "kind": kind }),
                &agent,
            )
            .await
            .unwrap();
        assert!(kept.success, "Keep {id}: {:?}", kept.error);
    }
}

/// On main a Postgres move wrote its event, then its catalog row, field-index
/// rows and delta mark statement by statement after the commit, and every list
/// read asked Postgres which records lacked a catalog row, 32 ids a query, so
/// both costs grew with the record and the type.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_and_a_filtered_page_cost_a_fixed_number_of_round_trips() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let schema = PostgresSchema::new(format!("round_trips_{}", sim_uuid().simple())).unwrap();
    let (state, trips) = server(&url, &schema).await;
    tokio::time::timeout(Duration::from_secs(300), async {
        quiet(&trips).await;
        keep_surfaces(&state, 0, 40).await;
        // Warm up: the first use of each statement prepares it.
        edit(&state, &trips, "s-003", "first").await;
        edit(&state, &trips, "s-003", "second").await;
        quiet(&trips).await;
        let from = trips.now();
        let move_cost = edit(&state, &trips, "s-003", "third").await;
        let messages = trips.since(from);
        eprintln!("warm move messages:\n  {}", messages.join("\n  "));
        assert!(
            move_cost <= 2,
            "a warm move costs {move_cost} round trips, want at most 2: {messages:#?}"
        );

        list(&state, &trips).await;
        let at_40 = list(&state, &trips).await;
        keep_surfaces(&state, 40, 300).await;
        list(&state, &trips).await;
        let at_300 = list(&state, &trips).await;
        eprintln!("round trips: move {move_cost}, list at 40 {at_40}, at 300 {at_300}");
        assert_eq!(
            at_40, at_300,
            "a filtered first page costs the same at 40 and 300 records"
        );
        assert!(
            at_300 <= 4,
            "a filtered first page costs {at_300} round trips, want at most 4"
        );
    })
    .await
    .expect("the workload finishes");
    drop(state);
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
}
