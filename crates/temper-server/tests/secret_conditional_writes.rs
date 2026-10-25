//! Conditional writes for tenant secrets: `If-None-Match: *` creates only,
//! `If-Match: "<etag>"` replaces only an unchanged key, and every successful
//! write answers with the key's new `ETag`. `If-Value-Sha256: <hex>` replaces
//! a key only while its stored value still has that SHA-256, for a caller that
//! knows the value it was given but not the key's etag.
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
/// May write and read secrets.
const WRITER: &str = "secret-writer";
/// May write secrets but not read them (`manage_secrets` without `access_secret`).
const BLIND_WRITER: &str = "secret-blind-writer";
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
  action in [Action::"manage_secrets", Action::"access_secret"],
  resource
);
permit(
  principal == Customer::"{BLIND_WRITER}",
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
    race_values(app, key, headers, "writer").await
}

/// [`race`], with each writer's value named `<prefix>-<n>`.
async fn race_values(
    app: &Router,
    key: &str,
    headers: Vec<(String, String)>,
    prefix: &str,
) -> Vec<PutOutcome> {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CONCURRENT_WRITERS));
    let mut writers = Vec::new();
    for writer in 0..CONCURRENT_WRITERS {
        let app = app.clone();
        let key = key.to_string();
        let headers = headers.clone();
        let barrier = barrier.clone();
        let value = format!("{prefix}-{writer}");
        writers.push(tokio::spawn(async move {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            barrier.wait().await;
            put(&app, &key, &value, WRITER, &headers).await
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
// If-Value-Sha256: replace only while the stored value is the one named.
// ---------------------------------------------------------------------------

const VALUE_HEADER: &str = "if-value-sha256";

fn sha256_hex(value: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A refusal that also never echoes the hash the caller sent.
fn refused_by_value(outcome: &PutOutcome, current_etag: &str, sent_hash: &str) {
    refused(outcome, current_etag);
    assert!(
        !outcome.body.to_ascii_lowercase().contains(sent_hash),
        "a 412 never names a hash: {}",
        outcome.body
    );
}

async fn value_condition_replaces_only_the_value_it_names(backend: Backend) {
    let app = backend.router();
    let missing_hash = sha256_hex("never-stored");
    let missing = put(
        &app,
        "signin",
        "x",
        WRITER,
        &[(VALUE_HEADER, &missing_hash)],
    )
    .await;
    refused_by_value(&missing, "\"no-etag-yet\"", &missing_hash);
    assert_eq!(backend.stored_value("signin").await, None);

    let first = written(&put(&app, "signin", "given-to-run", WRITER, &[]).await);

    let other_hash = sha256_hex("something-else");
    let wrong = put(
        &app,
        "signin",
        "moved",
        WRITER,
        &[(VALUE_HEADER, &other_hash)],
    )
    .await;
    refused_by_value(&wrong, &first, &other_hash);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("given-to-run")
    );

    // Hex in either case names the same value.
    let given_hash = sha256_hex("given-to-run").to_ascii_uppercase();
    let replaced = written(
        &put(
            &app,
            "signin",
            "moved",
            WRITER,
            &[(VALUE_HEADER, &given_hash)],
        )
        .await,
    );
    assert_ne!(replaced, first, "the replacement gets a new etag");
    assert_eq!(
        listed_etag(&app, "signin").await.as_deref(),
        Some(&*replaced)
    );
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("moved")
    );

    // The value the run was given is gone now, so the same header is refused.
    let again = put(
        &app,
        "signin",
        "moved-twice",
        WRITER,
        &[(VALUE_HEADER, &given_hash)],
    )
    .await;
    refused_by_value(&again, &replaced, &given_hash.to_ascii_lowercase());
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("moved")
    );
    backend.finish().await;
}

async fn value_condition_needs_access_secret(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "given-to-run", WRITER, &[]).await);
    // Right and wrong guesses get the same answer, so a caller who may write
    // but not read learns nothing about the stored value.
    for guess in ["given-to-run", "wrong-guess"] {
        let hash = sha256_hex(guess);
        let denied = put(
            &app,
            "signin",
            "blind",
            BLIND_WRITER,
            &[(VALUE_HEADER, &hash)],
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{guess}: {denied:?}");
        assert_eq!(denied.etag, None);
        assert!(!denied.body.contains(etag.trim_matches('"')));
    }
    // The same caller may still write without the value condition.
    written(&put(&app, "other", "plain", BLIND_WRITER, &[]).await);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("given-to-run")
    );
    backend.finish().await;
}

async fn value_condition_refuses_malformed_or_combined_headers(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "given-to-run", WRITER, &[]).await);
    let hash = sha256_hex("given-to-run");
    let short = &hash[..63];
    let long = format!("{hash}0");
    let not_hex = format!("{}g", &hash[..63]);
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![(VALUE_HEADER, short)],
        vec![(VALUE_HEADER, &long)],
        vec![(VALUE_HEADER, &not_hex)],
        vec![(VALUE_HEADER, "")],
        vec![(VALUE_HEADER, &hash), ("if-match", &etag)],
        vec![(VALUE_HEADER, &hash), ("if-none-match", "*")],
        vec![(VALUE_HEADER, &hash), (VALUE_HEADER, &hash)],
    ];
    for headers in cases {
        let outcome = put(&app, "signin", "refused", WRITER, &headers).await;
        assert_eq!(
            outcome.status,
            StatusCode::BAD_REQUEST,
            "{headers:?}: {outcome:?}"
        );
        assert_eq!(outcome.etag, None);
    }
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("given-to-run")
    );
    backend.finish().await;
}

/// Writers that all read the same value race to replace it. Whoever reads
/// before the winner's write lands and updates after it must still lose.
async fn concurrent_value_replacers_have_one_winner(backend: Backend) {
    let app = backend.router();
    written(&put(&app, "signin", "round-start", WRITER, &[]).await);
    for round in 0..5 {
        let current = backend
            .stored_value("signin")
            .await
            .expect("a value before each round");
        let headers = vec![(VALUE_HEADER.to_string(), sha256_hex(&current))];
        let outcomes = race_values(&app, "signin", headers, &format!("round-{round}")).await;
        assert_one_winner(&backend, &app, "signin", &outcomes).await;
    }
    backend.finish().await;
}

/// Postgres only: a writer lands between the conditional PUT's read and its
/// update. The test holds the row with its own uncommitted update, waits until
/// the PUT is queued behind it, then commits. The PUT read the old value, so
/// its hash matched, but the row changed before its update and it must lose.
async fn value_condition_loses_to_a_write_between_read_and_update(backend: Backend) {
    let app = backend.router();
    written(&put(&app, "signin", "given-to-run", WRITER, &[]).await);
    let (pool, schema) = backend.postgres.clone().expect("Postgres backend");
    let vault = backend.state.secrets_vault.clone().expect("vault");
    let (ciphertext, nonce) = vault.encrypt(b"landed-between").expect("encrypt");

    let mut landing = pool.begin().await.expect("begin the landing write");
    let updated = sqlx::query(&format!(
        "UPDATE \"{schema}\".tenant_secrets SET ciphertext = $1, nonce = $2, \
         version_id = 'landed', updated_at = now() WHERE tenant = $3 AND key_name = $4"
    ))
    .bind(&ciphertext)
    .bind(&nonce)
    .bind(TENANT)
    .bind("signin")
    .execute(&mut *landing)
    .await
    .expect("hold the row");
    assert_eq!(updated.rows_affected(), 1);

    let hash = sha256_hex("given-to-run");
    let racer = {
        let app = app.clone();
        let hash = hash.clone();
        tokio::spawn(async move {
            put(
                &app,
                "signin",
                "from-the-run",
                WRITER,
                &[(VALUE_HEADER, &hash)],
            )
            .await
        })
    };

    // Wait until the PUT waits on the row lock the landing write holds.
    let mut queued = false;
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .expect("read lock waits");
        if waiting > 0 {
            queued = true;
            break;
        }
        assert!(
            !racer.is_finished(),
            "the PUT finished without queuing behind the row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(queued, "the PUT never queued behind the landing write");
    landing.commit().await.expect("land the write");

    let outcome = racer.await.expect("racer task");
    refused_by_value(&outcome, "landed", &hash);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("landed-between")
    );
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

#[tokio::test]
async fn turso_value_condition_replaces_only_the_value_it_names() {
    value_condition_replaces_only_the_value_it_names(Backend::turso("value-replace").await).await;
}

#[tokio::test]
async fn turso_value_condition_needs_access_secret() {
    value_condition_needs_access_secret(Backend::turso("value-authz").await).await;
}

#[tokio::test]
async fn turso_value_condition_refuses_malformed_or_combined_headers() {
    value_condition_refuses_malformed_or_combined_headers(Backend::turso("value-malformed").await)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turso_concurrent_value_replacers_have_one_winner() {
    concurrent_value_replacers_have_one_winner(Backend::turso("value-race").await).await;
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

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_value_condition_replaces_only_the_value_it_names() {
    value_condition_replaces_only_the_value_it_names(Backend::postgres("pg-value-replace").await)
        .await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_value_condition_needs_access_secret() {
    value_condition_needs_access_secret(Backend::postgres("pg-value-authz").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_value_condition_refuses_malformed_or_combined_headers() {
    value_condition_refuses_malformed_or_combined_headers(
        Backend::postgres("pg-value-malformed").await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_concurrent_value_replacers_have_one_winner() {
    concurrent_value_replacers_have_one_winner(Backend::postgres("pg-value-race").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_value_condition_loses_to_a_write_between_read_and_update() {
    value_condition_loses_to_a_write_between_read_and_update(
        Backend::postgres("pg-value-between").await,
    )
    .await;
}
