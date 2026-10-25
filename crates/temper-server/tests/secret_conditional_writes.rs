//! Conditional writes for tenant secrets: `If-None-Match: *` creates only,
//! `If-Match: "<etag>"` replaces only an unchanged key, and every successful
//! write answers with the key's new `ETag`.
//!
//! Every case runs against Turso. The same cases run against Postgres when
//! `TEMPER_TEST_POSTGRES_URL` names a disposable database (ignored otherwise;
//! the Postgres rollback-migrations CI job runs them).

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::secrets::vault::SecretsVault;
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const TENANT: &str = "default";
const WRITER: &str = "secret-writer";
const CONCURRENT_WRITERS: usize = 8;

/// A server whose secrets live in a real store. Dropping it drops the store.
struct Backend {
    state: ServerState,
    _temp: Option<tempfile::TempDir>,
    postgres: Option<(sqlx::PgPool, String)>,
}

impl Backend {
    async fn turso(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let db_url = format!("file:{}", temp.path().join("metadata.db").display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create Turso test store");
        let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
        state.data_dir = temp.path().join("data");
        state.set_storage_stack(StorageStack::from_turso(store));
        Self::with_policy(state, Some(temp), None)
    }

    async fn postgres(name: &str) -> Self {
        let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
        let options: PgConnectOptions = url.parse().expect("PostgreSQL URL");
        let pool = PgPoolOptions::new()
            .max_connections(CONCURRENT_WRITERS as u32 + 2)
            .connect_with(options.options([("search_path", "pg_catalog")]))
            .await
            .expect("connect to PostgreSQL");
        let schema_name = format!(
            "secret_cas_{}",
            temper_runtime::scheduler::sim_uuid().simple()
        );
        let schema = PostgresSchema::new(schema_name.clone()).expect("schema name");
        migration::run_migrations_in_schema(&pool, &schema)
            .await
            .expect("migrate PostgreSQL schema");
        let store = PostgresEventStore::with_schema(pool.clone(), schema);
        let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
        let data_dir = tempfile::tempdir().expect("temporary test directory");
        state.data_dir = data_dir.path().join("data");
        state.set_storage_stack(StorageStack::from_postgres(store));
        Self::with_policy(state, Some(data_dir), Some((pool, schema_name)))
    }

    fn with_policy(
        mut state: ServerState,
        temp: Option<tempfile::TempDir>,
        postgres: Option<(sqlx::PgPool, String)>,
    ) -> Self {
        state.secrets_vault = Some(std::sync::Arc::new(SecretsVault::new(&[21_u8; 32])));
        state
            .authz
            .reload_tenant_policies(
                TENANT,
                &format!(
                    r#"
permit(
  principal == Customer::"{WRITER}",
  action == Action::"manage_secrets",
  resource
);
"#
                ),
            )
            .expect("secret policy should parse");
        Self {
            state,
            _temp: temp,
            postgres,
        }
    }

    async fn finish(self) {
        if let Some((pool, schema_name)) = self.postgres {
            sqlx::query(&format!("DROP SCHEMA \"{schema_name}\" CASCADE"))
                .execute(&pool)
                .await
                .expect("drop test schema");
            pool.close().await;
        }
    }

    fn router(&self) -> Router {
        build_router(self.state.clone())
    }

    /// The value the store holds for `key`, read back from the store rather
    /// than from this server's in-memory cache.
    async fn stored_value(&self, key: &str) -> Option<String> {
        let vault = self.state.secrets_vault.as_ref().expect("vault");
        vault.remove_secret(TENANT, key);
        self.state
            .load_tenant_secrets(TENANT)
            .await
            .expect("load secrets from the store");
        vault.get_secret(TENANT, key)
    }
}

fn security_context(id: &str) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: format!("secret-cas-{id}"),
    }
}

fn request(
    method: Method,
    uri: &str,
    body: Option<&str>,
    principal: &str,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = body
        .map(|value| serde_json::json!({ "value": value }).to_string())
        .unwrap_or_default();
    let mut request = builder
        .body(Body::from(body))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            security_context(principal),
        ));
    request
}

/// What a PUT answered: its status, its `ETag` header and its body.
#[derive(Debug)]
struct PutOutcome {
    status: StatusCode,
    etag: Option<String>,
    body: String,
    value: String,
}

async fn put(
    app: &Router,
    key: &str,
    value: &str,
    principal: &str,
    headers: &[(&str, &str)],
) -> PutOutcome {
    let response = app
        .clone()
        .oneshot(request(
            Method::PUT,
            &format!("/api/tenants/{TENANT}/secrets/{key}"),
            Some(value),
            principal,
            headers,
        ))
        .await
        .expect("secret PUT should run");
    let status = response.status();
    let etag = response
        .headers()
        .get("etag")
        .map(|value| value.to_str().expect("ETag is ASCII").to_string());
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read PUT body");
    PutOutcome {
        status,
        etag,
        body: String::from_utf8_lossy(&body).into_owned(),
        value: value.to_string(),
    }
}

/// The etag the secret listing reports for `key`.
async fn listed_etag(app: &Router, key: &str) -> Option<String> {
    let response = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/tenants/{TENANT}/secrets"),
            None,
            WRITER,
            &[],
        ))
        .await
        .expect("secret list should run");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read list body");
    let listing: serde_json::Value = serde_json::from_slice(&body).expect("list JSON");
    assert!(
        listing["keys"]
            .as_array()
            .expect("keys stay a list of names")
            .iter()
            .any(|name| name == key),
        "the listing still names {key}: {listing}"
    );
    listing["etags"][key].as_str().map(str::to_string)
}

fn written(outcome: &PutOutcome) -> String {
    assert_eq!(
        outcome.status,
        StatusCode::NO_CONTENT,
        "write should succeed: {outcome:?}"
    );
    let etag = outcome.etag.clone().expect("a successful PUT returns ETag");
    assert!(
        etag.len() > 2 && etag.starts_with('"') && etag.ends_with('"'),
        "ETag is a quoted strong validator: {etag}"
    );
    etag
}

fn refused(outcome: &PutOutcome, current_etag: &str) {
    assert_eq!(
        outcome.status,
        StatusCode::PRECONDITION_FAILED,
        "write should be refused: {outcome:?}"
    );
    assert_eq!(outcome.etag, None, "a 412 carries no ETag");
    assert!(
        !outcome.body.contains(current_etag.trim_matches('"')),
        "a 412 never reveals the current etag: {}",
        outcome.body
    );
}

// ---------------------------------------------------------------------------
// Scenarios, each run against every store.
// ---------------------------------------------------------------------------

async fn create_only_refuses_an_existing_key(backend: Backend) {
    let app = backend.router();
    let first = put(&app, "signin", "first", WRITER, &[("if-none-match", "*")]).await;
    let etag = written(&first);
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*etag));

    let second = put(&app, "signin", "second", WRITER, &[("if-none-match", "*")]).await;
    refused(&second, &etag);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("first")
    );
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*etag));
    backend.finish().await;
}

async fn replace_needs_the_current_etag(backend: Backend) {
    let app = backend.router();
    let missing = put(
        &app,
        "signin",
        "nobody",
        WRITER,
        &[("if-match", "\"nope\"")],
    )
    .await;
    refused(&missing, "nope");
    assert_eq!(backend.stored_value("signin").await, None);

    let first = written(&put(&app, "signin", "first", WRITER, &[]).await);
    let second = written(&put(&app, "signin", "second", WRITER, &[("if-match", &first)]).await);
    assert_ne!(first, second, "every write gets a new etag");
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*second));

    let stale = put(&app, "signin", "stale", WRITER, &[("if-match", &first)]).await;
    refused(&stale, &second);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("second")
    );
    backend.finish().await;
}

async fn etag_is_not_derived_from_the_value(backend: Backend) {
    let app = backend.router();
    let a = written(&put(&app, "one", "same-value", WRITER, &[]).await);
    let b = written(&put(&app, "one", "same-value", WRITER, &[]).await);
    let c = written(&put(&app, "two", "same-value", WRITER, &[]).await);
    assert_ne!(a, b, "rewriting the same value changes the etag");
    assert_ne!(a, c, "two keys holding one value have different etags");
    assert_ne!(b, c, "two keys holding one value have different etags");
    backend.finish().await;
}

async fn unconditional_writes_are_unchanged(backend: Backend) {
    let app = backend.router();
    written(&put(&app, "plain", "first", WRITER, &[]).await);
    written(&put(&app, "plain", "second", WRITER, &[]).await);
    assert_eq!(
        backend.stored_value("plain").await.as_deref(),
        Some("second")
    );
    backend.finish().await;
}

async fn authorization_comes_before_the_condition(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "first", WRITER, &[]).await);
    for headers in [[("if-match", "\"nope\"")], [("if-none-match", "*")]] {
        let denied = put(&app, "signin", "intruder", "intruder", &headers).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{denied:?}");
        assert_eq!(denied.etag, None);
        assert!(!denied.body.contains(etag.trim_matches('"')));
    }
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("first")
    );
    backend.finish().await;
}

/// Fires `CONCURRENT_WRITERS` PUTs at once, each with a distinct value and the
/// same condition, and returns what each one got.
async fn race(app: &Router, key: &str, headers: Vec<(String, String)>) -> Vec<PutOutcome> {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CONCURRENT_WRITERS));
    let mut writers = Vec::new();
    for writer in 0..CONCURRENT_WRITERS {
        let app = app.clone();
        let key = key.to_string();
        let headers = headers.clone();
        let barrier = barrier.clone();
        writers.push(tokio::spawn(async move {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            barrier.wait().await;
            put(&app, &key, &format!("writer-{writer}"), WRITER, &headers).await
        }));
    }
    let mut outcomes = Vec::new();
    for writer in writers {
        outcomes.push(writer.await.expect("writer task"));
    }
    outcomes
}

/// Exactly one writer wins, every other writer gets 412, and the store holds
/// the winner's value under the winner's etag.
async fn assert_one_winner(backend: &Backend, app: &Router, key: &str, outcomes: &[PutOutcome]) {
    let winners: Vec<&PutOutcome> = outcomes
        .iter()
        .filter(|outcome| outcome.status.is_success())
        .collect();
    assert_eq!(winners.len(), 1, "exactly one writer wins: {outcomes:?}");
    let winner = winners[0];
    let etag = written(winner);
    for loser in outcomes
        .iter()
        .filter(|outcome| !outcome.status.is_success())
    {
        refused(loser, &etag);
    }
    assert_eq!(listed_etag(app, key).await.as_deref(), Some(&*etag));
    assert_eq!(
        backend.stored_value(key).await.as_deref(),
        Some(winner.value.as_str())
    );
}

async fn concurrent_creators_have_one_winner(backend: Backend) {
    let app = backend.router();
    let outcomes = race(&app, "signin", vec![("if-none-match".into(), "*".into())]).await;
    assert_one_winner(&backend, &app, "signin", &outcomes).await;
    backend.finish().await;
}

async fn concurrent_replacers_have_one_winner(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "older", WRITER, &[]).await);
    let outcomes = race(&app, "signin", vec![("if-match".into(), etag.clone())]).await;
    assert_one_winner(&backend, &app, "signin", &outcomes).await;
    let after = listed_etag(&app, "signin")
        .await
        .expect("etag after the race");
    assert_ne!(after, etag, "the winning write replaced the etag");
    backend.finish().await;
}

// ---------------------------------------------------------------------------
// Turso
// ---------------------------------------------------------------------------

#[tokio::test]
async fn turso_create_only_refuses_an_existing_key() {
    create_only_refuses_an_existing_key(Backend::turso("cas-create").await).await;
}

#[tokio::test]
async fn turso_replace_needs_the_current_etag() {
    replace_needs_the_current_etag(Backend::turso("cas-replace").await).await;
}

#[tokio::test]
async fn turso_etag_is_not_derived_from_the_value() {
    etag_is_not_derived_from_the_value(Backend::turso("cas-opaque").await).await;
}

#[tokio::test]
async fn turso_unconditional_writes_are_unchanged() {
    unconditional_writes_are_unchanged(Backend::turso("cas-plain").await).await;
}

#[tokio::test]
async fn turso_authorization_comes_before_the_condition() {
    authorization_comes_before_the_condition(Backend::turso("cas-authz").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turso_concurrent_creators_have_one_winner() {
    concurrent_creators_have_one_winner(Backend::turso("cas-race-create").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turso_concurrent_replacers_have_one_winner() {
    concurrent_replacers_have_one_winner(Backend::turso("cas-race-replace").await).await;
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_create_only_refuses_an_existing_key() {
    create_only_refuses_an_existing_key(Backend::postgres("pg-cas-create").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_replace_needs_the_current_etag() {
    replace_needs_the_current_etag(Backend::postgres("pg-cas-replace").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_etag_is_not_derived_from_the_value() {
    etag_is_not_derived_from_the_value(Backend::postgres("pg-cas-opaque").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_unconditional_writes_are_unchanged() {
    unconditional_writes_are_unchanged(Backend::postgres("pg-cas-plain").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_authorization_comes_before_the_condition() {
    authorization_comes_before_the_condition(Backend::postgres("pg-cas-authz").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_concurrent_creators_have_one_winner() {
    concurrent_creators_have_one_winner(Backend::postgres("pg-cas-race-create").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_concurrent_replacers_have_one_winner() {
    concurrent_replacers_have_one_winner(Backend::postgres("pg-cas-race-replace").await).await;
}
