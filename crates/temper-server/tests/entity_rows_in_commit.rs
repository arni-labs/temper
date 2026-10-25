//! A move is one commit on every shipped stack whose query plane is its
//! event store: the event, the record's query-plane row and its timer row
//! land in one transaction. A stack whose query plane is something else
//! still has the server write the rows after the commit.
//!
//! Turso and the tenant router are covered by `dst_commit_outcome` and the
//! Turso store tests. Here: the stack rule on every builder, and, against
//! PostgreSQL when `TEMPER_TEST_POSTGRES_URL` names a disposable database,
//! that a move's event, catalog row and timer row share one transaction.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::QueryPlaneStore;
use temper_server::{ServerState, StorageStack};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;

const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Open", "Busy"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Busy"

[[action]]
name = "Release"
kind = "input"
from = ["Busy"]
to = "Open"

[[action]]
name = "Expire"
kind = "internal"
from = ["Busy"]
to = "Open"

[[state_timeout]]
state = "Busy"
after_seconds = 86400
on_timeout = "Expire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A lazy pool: building a stack and asking about it opens no connection.
fn lazy_postgres() -> PostgresEventStore {
    let pool = sqlx::PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("a lazy pool parses its URL");
    PostgresEventStore::new(pool)
}

#[tokio::test]
async fn a_stack_commits_its_rows_only_when_its_query_plane_is_its_event_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("stack.db").display());
    let turso = TursoEventStore::new(&url, None).await.expect("turso store");

    let stack = StorageStack::from_turso(turso.clone());
    assert!(stack.entity_rows_in_commit(), "turso commits its own rows");

    let postgres = StorageStack::from_postgres(lazy_postgres());
    assert!(
        postgres.entity_rows_in_commit(),
        "postgres commits its own rows"
    );

    // Another query plane, or none: the commit's rows are not the ones the
    // stack reads, so the server writes them after the commit.
    for mut replaced in [StorageStack::from_turso(turso), postgres] {
        let other: Arc<dyn QueryPlaneStore> = Arc::new(
            TursoEventStore::new(
                &format!("file:{}", dir.path().join("other.db").display()),
                None,
            )
            .await
            .expect("other store"),
        );
        replaced.query_plane = Some(other);
        assert!(!replaced.entity_rows_in_commit(), "a different query plane");
        replaced.query_plane = None;
        assert!(!replaced.entity_rows_in_commit(), "no query plane");
    }
}

/// The transaction that last wrote each row: the event's, the catalog row's,
/// and the timer row's.
async fn writers(
    pool: &sqlx::PgPool,
    schema: &PostgresSchema,
    id: &str,
    sequence_nr: i64,
) -> (String, String, Option<String>) {
    let event: String = sqlx::query_scalar(&schema.qualify_sql(
        "SELECT xmin::text FROM {schema}events \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = $1 AND sequence_nr = $2",
    ))
    .bind(id)
    .bind(sequence_nr)
    .fetch_one(pool)
    .await
    .expect("the event row");
    let catalog: String = sqlx::query_scalar(&schema.qualify_sql(
        "SELECT xmin::text FROM {schema}entity_catalog \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = $1 AND sequence_nr = $2",
    ))
    .bind(id)
    .bind(sequence_nr)
    .fetch_one(pool)
    .await
    .expect("the catalog row at the event's sequence");
    let timer: Option<String> = sqlx::query_scalar(&schema.qualify_sql(
        "SELECT xmin::text FROM {schema}state_timers \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = $1",
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("the timer row reads");
    (event, catalog, timer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn a_move_on_postgres_commits_its_event_catalog_and_timer_rows_together() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    let schema = PostgresSchema::new(format!(
        "one_commit_{}",
        temper_runtime::scheduler::sim_uuid().simple()
    ))
    .expect("schema name");
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .expect("migrate");

    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("Job", JOB_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("one-commit"), registry);
    state.set_storage_stack(StorageStack::from_postgres(
        PostgresEventStore::with_schema(pool.clone(), schema.clone()),
    ));
    let tenant = TenantId::default();
    let caller = AgentContext::for_service("one-commit-test");
    state
        .get_or_create_tenant_entity(&tenant, "Job", "j-1", serde_json::json!({ "Id": "j-1" }))
        .await
        .expect("the job is created");

    // Entering the timed state writes the catalog row and arms the timer row,
    // in the event's own transaction.
    let held = state
        .dispatch_tenant_action(
            &tenant,
            "Job",
            "j-1",
            "Hold",
            serde_json::json!({}),
            &caller,
        )
        .await
        .expect("hold dispatches");
    assert!(held.success, "{:?}", held.error);
    let (event, catalog, timer) =
        writers(&pool, &schema, "j-1", held.state.sequence_nr as i64).await;

    // Leaving it removes the timer row in the event's transaction.
    let released = state
        .dispatch_tenant_action(
            &tenant,
            "Job",
            "j-1",
            "Release",
            serde_json::json!({}),
            &caller,
        )
        .await
        .expect("release dispatches");
    assert!(released.success, "{:?}", released.error);
    let (released_event, released_catalog, released_timer) =
        writers(&pool, &schema, "j-1", released.state.sequence_nr as i64).await;

    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .expect("drop schema");
    pool.close().await;

    assert_eq!(
        catalog, event,
        "the hold's catalog row is its event's commit"
    );
    assert_eq!(
        timer.as_deref(),
        Some(event.as_str()),
        "the hold's timer row is its event's commit"
    );
    assert_eq!(
        released_catalog, released_event,
        "the release's catalog row is its event's commit"
    );
    assert_eq!(released_timer, None, "the release removed the timer row");
}
