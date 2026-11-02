//! A server over a store that outlives it, and how the tests move and read.

use super::*;

pub(super) const WITHIN: Duration = Duration::from_secs(20);

/// The log line the kernel leaves each time it stops a timer.
pub(super) const STOPPED: &str = "state timer stopped: nothing names its caller";

#[derive(Clone, Default)]
pub(super) struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    /// This test process's log. One subscriber is installed for the whole
    /// process, the first time a test asks, so tests that share a process
    /// (plain `cargo test`) share it too. The timer tasks that log run on any
    /// of the runtime's threads, which a per-test scoped subscriber would not
    /// see. Each test reads only the records it made (`stops`).
    pub(super) fn install() -> Self {
        static LOG: std::sync::OnceLock<CapturedLog> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let logs = Self::default();
            let writer = logs.clone();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .json()
                    .without_time()
                    .with_writer(move || writer.clone())
                    .finish(),
            )
            .expect("nothing else installs a subscriber in this test");
            logs
        })
        .clone()
    }

    /// The times a timer of record `id` was stopped.
    pub(super) fn stops(&self, id: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["fields"]["message"] == STOPPED && event["fields"]["entity_id"] == id
            })
            .count()
    }
}

/// Timer rows committed with the journal's events, as on Turso.
pub(super) struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// A started server over `store`, with Shipment's timer at `after` seconds.
pub(super) async fn server(name: &str, store: &SimEventStore, after: u32) -> ServerState {
    server_over(name, store, after, None).await
}

/// [`server`], reading and writing its journal through `events` when given.
pub(super) async fn server_over(
    name: &str,
    store: &SimEventStore,
    after: u32,
    events: Option<temper_server::storage::BoxedEventStore>,
) -> ServerState {
    let shipment = SHIPMENT_IOA.replace("AFTER", &after.to_string());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[
            ("Line", LINE_IOA),
            ("Shipment", shipment.as_str()),
            ("Check", CHECK_IOA),
            ("Poller", POLLER_IOA),
            ("Asker", ASKER_IOA),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    let mut storage = StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal();
    if let Some(events) = events {
        storage.events = events;
    }
    state.set_storage_stack(storage);
    state.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("the module compiles");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(&TenantId::default(), "trigger_principal_echo", &hash);
    state.start_runtime().await;
    state
}

pub(super) fn as_who(security_ctx: SecurityContext) -> AgentContext {
    AgentContext {
        security_ctx: Some(security_ctx),
        ..AgentContext::default()
    }
}

pub(super) fn person(id: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Customer,
        None,
        None,
        None,
        None,
    ))
}

pub(super) fn agent_for(person: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude-code"),
        Some(person),
        Some("contributor"),
        None,
    ))
}

/// Make `action` on `entity_type` `id` as `by`, creating the record first.
pub(super) async fn act(state: &ServerState, entity_type: &str, id: &str, action: &str, by: &AgentContext) {
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, entity_type, id, json!({}))
        .await
        .expect("the record is created");
    let response = state
        .dispatch_tenant_action(&tenant, entity_type, id, action, json!({}), by)
        .await
        .expect("the move is dispatched");
    assert!(
        response.success,
        "{action} on {entity_type} {id}: {:?}",
        response.error
    );
}

/// The record as an ordinary read returns it.
pub(super) async fn read(state: &ServerState, set: &str, id: &str) -> Value {
    let mut request = Request::builder()
        .method("GET")
        .uri(format!("/tdata/{set}('{id}')"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("reader", "person", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Wait until `holds` holds of the record, or fail saying what it read.
pub(super) async fn until(
    state: &ServerState,
    set: &str,
    id: &str,
    what: &str,
    holds: fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + WITHIN; // determinism-ok: real-time test
    loop {
        let record = read(state, set, id).await;
        if holds(&record) {
            return record;
        }
        assert!(
            Instant::now() < deadline, // determinism-ok: real-time test
            "{set} {id} never {what}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

pub(super) fn is_stopped(record: &Value) -> bool {
    record["timer_needs_person"].is_string()
}

/// Run `first` on a server of its own, stopped afterward as a process exit
/// would stop it, timers and all, and return what it returned.
pub(super) fn on_a_server_that_stops<T, F, Fut>(store: &SimEventStore, after: u32, first: F) -> T
where
    T: Send + 'static,
    F: FnOnce(ServerState) -> Fut + Send + 'static,
    Fut: Future<Output = T>,
{
    let store = store.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let answer = runtime.block_on(async move {
            let state = server("first", &store, after).await;
            first(state).await
        });
        drop(runtime);
        answer
    })
    .join()
    .expect("the first server ran")
}

