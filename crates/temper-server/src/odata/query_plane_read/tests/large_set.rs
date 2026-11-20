//! A set of cards larger than the scan budget, written through the kernel
//! into each store, and the in-memory evaluator's answer for any read of it
//! (ADR-0188).
//!
//! The budget is small (`max_entities` 20, so 210 candidates a read) and the
//! set holds 330 records. Some fields hold values of the wrong JSON type, a
//! JSON null, or no value at all, so a read that SQL answers differently from
//! `query_eval` shows up as a different set of ids or a different count.

use super::*;
use temper_odata::query::types::parse_query_options;

pub(super) const ROWS: usize = 330;
pub(super) const BUDGET: QueryPlaneReadBudget = QueryPlaneReadBudget {
    default_page_size: 20,
    max_entities: 20,
};
const STATUSES: [(&str, Option<&str>); 4] = [
    ("Open", None),
    ("Done", Some("Finish")),
    ("Held", Some("Hold")),
    ("Posted", Some("Post")),
];

const CARD_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Cards" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Card">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="kind" Type="Edm.String" Nullable="false"/>
        <Property Name="title" Type="Edm.String" Nullable="false"/>
        <Property Name="rank" Type="Edm.Int64" Nullable="false"/>
        <Property Name="up" Type="Edm.Int64" Nullable="false"/>
        <Property Name="score" Type="Edm.Decimal" Nullable="false"/>
        <Property Name="pinned" Type="Edm.Boolean" Nullable="false"/>
        <Property Name="said_at" Type="Edm.String" Nullable="false"/>
        <Property Name="note" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Cards" EntityType="Temper.Cards.Card"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const CARD_IOA: &str = r#"
[automaton]
name = "Card"
states = ["Open", "Done", "Held", "Posted"]
initial = "Open"

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Held"

[[action]]
name = "Post"
kind = "input"
from = ["Open"]
to = "Posted"
"#;

/// The card states, registered for the default tenant.
pub(super) fn card_state(system_name: &str) -> ServerState {
    let csdl = parse_csdl(CARD_CSDL).expect("card CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TenantId::default().as_str(),
        csdl,
        CARD_CSDL.to_string(),
        &[("Card", CARD_IOA)],
    );
    ServerState::from_registry(ActorSystem::new(system_name), registry)
}

/// Card `index`'s fields. `title`, `rank`, `score` and `said_at` hold only
/// values of their declared type (or none, for `rank`), so an order by them
/// is well defined; `kind`, `up` and `pinned` also hold values of other JSON
/// types, nulls and nothing.
pub(super) fn card_fields(index: usize) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    let kind = match index % 35 {
        6 => serde_json::json!(5),
        13 => serde_json::Value::Null,
        20 => serde_json::json!(["steer"]),
        27 => serde_json::json!("true"),
        34 => serde_json::json!("x".repeat(2100)),
        other if other % 7 < 3 => serde_json::json!("steer"),
        other if other % 7 < 5 => serde_json::json!("find"),
        _ => serde_json::json!(""),
    };
    if index % 35 != 30 {
        fields.insert("kind".into(), kind);
    }
    let titles = [
        "",
        "Zebra",
        "apple",
        "Apple",
        "émigré",
        "apple pie",
        "b",
        "Ünïcode",
    ];
    fields.insert("title".into(), serde_json::json!(titles[(index * 5) % 8]));
    if !index.is_multiple_of(23) {
        fields.insert(
            "rank".into(),
            serde_json::json!(((index * 7) % 11) as i64 - 2),
        );
    }
    let up = match index % 13 {
        0 => serde_json::json!(0),
        1 => serde_json::json!(0.0),
        2 => serde_json::json!("0"),
        3 => serde_json::Value::Null,
        5 => serde_json::json!(-1),
        _ => serde_json::json!(index % 5),
    };
    if index % 13 != 4 {
        fields.insert("up".into(), up);
    }
    let scores = [0.5, -0.5, 0.1, 0.30000000000000004, 2.0, 1e20, -3.25];
    fields.insert("score".into(), serde_json::json!(scores[index % 7]));
    let pinned = match index % 9 {
        0 => serde_json::json!("true"),
        1..=3 => serde_json::json!(true),
        4 => serde_json::Value::Null,
        _ => serde_json::json!(false),
    };
    if index % 9 != 8 {
        fields.insert("pinned".into(), pinned);
    }
    let said_at = match index % 6 {
        0 => String::new(),
        1 => format!("2026-10-{:02}", 1 + index % 28),
        2 | 3 => format!("2026-11-{:02}", 1 + index % 30),
        4 => "2026-11-01".to_string(),
        _ => format!("2026-12-{:02}T08:00:00Z", 1 + index % 28),
    };
    fields.insert("said_at".into(), serde_json::json!(said_at));
    serde_json::Value::Object(fields)
}

/// Card `index`'s id and status.
pub(super) fn card(index: usize) -> (String, &'static str) {
    (
        format!("card-{index:03}"),
        STATUSES[index % STATUSES.len()].0,
    )
}

/// Write every card through the kernel; the records as the evaluator reads
/// them.
pub(super) async fn seed_cards(state: &ServerState) -> Vec<serde_json::Value> {
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("large-set");
    let mut bodies = Vec::with_capacity(ROWS);
    for index in 0..ROWS {
        let (id, status) = card(index);
        let fields = card_fields(index);
        state
            .get_or_create_tenant_entity(&tenant, "Card", &id, fields.clone())
            .await
            .expect("create card");
        if let Some(action) = STATUSES[index % STATUSES.len()].1 {
            let moved = state
                .dispatch_tenant_action(&tenant, "Card", &id, action, serde_json::json!({}), &agent)
                .await
                .expect("move card");
            assert!(moved.success, "{id} moves to {status}");
        }
        bodies.push(serde_json::json!({"entity_id": id, "status": status, "fields": fields}));
    }
    bodies
}

/// The cards on the simulation store, which has no query plane: every read
/// takes the in-memory path.
pub(super) async fn sim_cards(system_name: &str) -> (ServerState, Vec<serde_json::Value>) {
    let mut state = card_state(system_name);
    state.set_storage_stack(StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7),
        None,
    ));
    let bodies = seed_cards(&state).await;
    (state, bodies)
}

/// Statuses the reader's policy hides: half of the cards.
pub(super) const HIDDEN_STATUSES: [&str; 2] = ["Done", "Held"];

/// Install a policy that lets a reader list cards and read every card
/// except those in [`HIDDEN_STATUSES`], so each row is judged on its own;
/// the reader's context.
pub(super) fn install_reader(state: &ServerState) -> SecurityContext {
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"
            permit(principal, action == Action::"list", resource is Card);
            permit(principal, action == Action::"read", resource is Card);
            forbid(principal, action == Action::"read", resource is Card)
                when { resource.status == "Done" || resource.status == "Held" };
            "#,
        )
        .expect("install the reader's policy");
    SecurityContext {
        principal: temper_authz::Principal {
            id: "reader".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "large-set-reader".to_string(),
        cause: None,
        credential: None,
    }
}

/// The cards the reader may see.
pub(super) fn visible(bodies: &[serde_json::Value]) -> Vec<serde_json::Value> {
    bodies
        .iter()
        .filter(|body| {
            let status = body["status"].as_str().unwrap_or_default();
            !HIDDEN_STATUSES.contains(&status)
        })
        .cloned()
        .collect()
}

/// The cards on a Turso store in a temporary file.
pub(super) async fn turso_cards(
    system_name: &str,
) -> (ServerState, Vec<serde_json::Value>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("cards.db").display()),
        None,
    )
    .await
    .expect("turso store");
    let mut state = card_state(system_name);
    state.set_storage_stack(StorageStack::from_turso(store));
    let bodies = seed_cards(&state).await;
    (state, bodies, dir)
}

/// The cards on Postgres, in a schema of their own that [`Self::drop`]
/// removes.
pub(super) struct PostgresCards {
    pub(super) state: ServerState,
    pub(super) bodies: Vec<serde_json::Value>,
    pool: sqlx::PgPool,
    schema: String,
}

impl PostgresCards {
    /// Write the cards into a new schema on `database_url`.
    pub(super) async fn seed(database_url: &str, system_name: &str) -> Self {
        let pool = sqlx::PgPool::connect(database_url)
            .await
            .expect("connect to postgres");
        let schema = format!("cards_{}", sim_uuid().simple());
        let named = temper_store_postgres::PostgresSchema::new(schema.clone()).expect("schema");
        temper_store_postgres::migration::run_migrations_in_schema(&pool, &named)
            .await
            .expect("migrations");
        let store = temper_store_postgres::PostgresEventStore::with_schema(pool.clone(), named);
        let mut state = card_state(system_name);
        state.set_storage_stack(StorageStack::from_postgres(store));
        let bodies = seed_cards(&state).await;
        Self {
            state,
            bodies,
            pool,
            schema,
        }
    }

    /// Drop the schema.
    pub(super) async fn drop(self) {
        sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", self.schema))
            .execute(&self.pool)
            .await
            .expect("drop schema");
    }
}

/// One read of the cards, or a panic naming why it was refused.
pub(super) async fn read(
    state: &ServerState,
    security_ctx: &SecurityContext,
    query: &str,
) -> QueryPlaneReadResult {
    let query_options = parse_query_options(query).expect("query parses");
    let tenant = TenantId::default();
    match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state,
        tenant: &tenant,
        security_ctx,
        entity_type: "Card",
        entity_set_name: "Cards",
        query_options: &query_options,
        budget: BUDGET,
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("{query}: read failed"),
    }
}

/// Every id a read lists, following its continuation to the last page.
pub(super) async fn walk(
    state: &ServerState,
    security_ctx: &SecurityContext,
    query: &str,
) -> Vec<String> {
    let mut ids = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page_query = match &token {
            Some(token) => format!("{query}&$skiptoken={token}"),
            None => query.to_string(),
        };
        let result = read(state, security_ctx, &page_query).await;
        assert!(result.entities.len() <= BUDGET.max_entities);
        ids.extend(result.entities.iter().map(listed_entity_id));
        match result.next_skiptoken {
            Some(next) => token = Some(next),
            None => break,
        }
        assert!(
            ids.len() <= ROWS,
            "{query}: the continuation did not move on"
        );
    }
    ids
}

/// The ids `query_eval` keeps for `query`'s filter, in its order with the id
/// last, and how many: the answer every store must give.
pub(super) fn in_memory(bodies: &[serde_json::Value], query: &str) -> (Vec<String>, usize) {
    let parsed = parse_query_options(query).expect("query parses");
    let options = QueryOptions {
        filter: parsed.filter,
        orderby: Some(super::super::pagination::canonical_pagination_order(
            parsed.orderby.as_deref(),
        )),
        count: Some(true),
        ..QueryOptions::default()
    };
    let (kept, count) = crate::query_eval::apply_query_options(bodies.to_vec(), &options);
    let ids = kept.iter().map(listed_entity_id).collect();
    (ids, count.unwrap_or_default())
}

/// Walk `query` and count it, and check both against the evaluator.
pub(super) async fn assert_matches_memory(
    state: &ServerState,
    security_ctx: &SecurityContext,
    bodies: &[serde_json::Value],
    query: &str,
) {
    let (want, want_count) = in_memory(bodies, query);
    let ids = walk(state, security_ctx, &format!("{query}&$top=20")).await;
    if query.contains("$orderby") {
        assert_eq!(
            ids, want,
            "{query}: every row once, in the evaluator's order"
        );
    } else {
        let mut sorted = ids.clone();
        sorted.sort();
        let mut want_sorted = want.clone();
        want_sorted.sort();
        assert_eq!(sorted, want_sorted, "{query}: every row once");
        assert_eq!(ids.len(), want.len(), "{query}: no row twice");
    }
    let counted = read(state, security_ctx, &format!("{query}&$top=0&$count=true")).await;
    assert_eq!(counted.count, Some(want_count), "{query}: the count");
}
