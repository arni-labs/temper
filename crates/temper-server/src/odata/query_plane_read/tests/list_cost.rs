//! A list read costs in proportion to the rows it returns, not to the type.
//!
//! The shape is an app's first screen: many records with many fields, read
//! newest move first with a status filter and a small `$top`, then followed
//! by `$skiptoken`. Work is measured by the read's own telemetry
//! (`materialized_count`, `candidate_count`), which production reports on
//! every `odata.entity_set_read` span. The answer is checked against an
//! oracle built from the records themselves: same rows, same order, the same
//! continuation, and no row a per-row policy hides.

use super::*;
use chrono::{DateTime, TimeZone, Utc};
use temper_odata::query::types::parse_query_options;

const RECORDS: usize = 600;
const FIELDS: usize = 200;
const TOP: usize = 10;

/// One stored record, as the oracle sees it.
struct Record {
    id: String,
    status: &'static str,
    last_event_at: Option<DateTime<Utc>>,
    secret: bool,
}

fn record(index: usize) -> Record {
    let status = if index % 7 == 0 {
        "Unmade"
    } else if index % 2 == 0 {
        "Draft"
    } else {
        "Created"
    };
    // A permutation of ids over 300 seconds, so ids are not in time order and
    // many records share a second. Nanoseconds cover every precision the
    // kernel's clock serializes: a whole second, milli, micro and nano.
    let second = ((index * 7919) % 300) as i64;
    let nanos = match index % 4 {
        0 => 0,
        1 => 500_000_000,
        2 => 1_000,
        _ => 1,
    };
    let last_event_at = (index % 50 != 1).then(|| {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + chrono::Duration::seconds(second)
            + chrono::Duration::nanoseconds(nanos)
    });
    Record {
        id: format!("rec-{index:03}"),
        status,
        last_event_at,
        secret: index % 3 == 0,
    }
}

async fn seed(store: &TursoEventStore, tenant: &TenantId) -> Vec<Record> {
    let records: Vec<Record> = (0..RECORDS).map(record).collect();
    for (index, record) in records.iter().enumerate() {
        let mut fields = serde_json::Map::new();
        fields.insert("Id".to_string(), serde_json::json!(record.id));
        fields.insert(
            "Secret".to_string(),
            serde_json::json!(if record.secret { "yes" } else { "no" }),
        );
        for field in 0..FIELDS {
            fields.insert(
                format!("f{field:03}"),
                serde_json::json!(format!("value-{index}-{field}")),
            );
        }
        let fields = serde_json::Value::Object(fields);
        // The kernel's own serialization of its clock, as a commit stores it.
        let at = record
            .last_event_at
            .map(|at| serde_json::to_value(at).unwrap())
            .unwrap_or(serde_json::Value::Null);
        let state = serde_json::json!({
            "status": record.status,
            "fields": fields,
            "first_event_at": at,
            "last_event_at": at,
        });
        store
            .upsert_query_projection_with_state(
                tenant.as_str(),
                "Order",
                &record.id,
                record.status,
                &fields,
                &state,
                1,
            )
            .await
            .expect("upsert projection");
    }
    records
}

/// Ids a reader may see, newest move first (unknown times first), ties by id.
fn expected(records: &[Record], hide_secret: bool) -> Vec<String> {
    let mut visible: Vec<&Record> = records
        .iter()
        .filter(|record| record.status != "Unmade" && !(hide_secret && record.secret))
        .collect();
    visible.sort_by(|a, b| match (a.last_event_at, b.last_event_at) {
        (None, None) => a.id.cmp(&b.id),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.id.cmp(&b.id)),
    });
    visible
        .into_iter()
        .map(|record| record.id.clone())
        .collect()
}

fn reader() -> SecurityContext {
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
        correlation_id: "list-cost".to_string(),
        cause: None,
        credential: None,
    }
}

async fn read(
    state: &ServerState,
    security_ctx: &SecurityContext,
    query: &str,
) -> QueryPlaneReadResult {
    let tenant = TenantId::default();
    let query_options = parse_query_options(query).expect("query parses");
    match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state,
        tenant: &tenant,
        security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        // The production defaults: a 100-row default page, 1000 at most.
        budget: QueryPlaneReadBudget {
            default_page_size: 100,
            max_entities: 1000,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("list read should succeed: {query}"),
    }
}

/// Every page of `query`, following continuations; each page's work is
/// checked against `work_per_page` rows.
async fn walk(
    state: &ServerState,
    security_ctx: &SecurityContext,
    query: &str,
    top: usize,
    work_per_page: usize,
) -> Vec<String> {
    let mut ids = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..=RECORDS {
        let page_query = match &token {
            Some(token) => format!("{query}&$top={top}&$skiptoken={token}"),
            None => format!("{query}&$top={top}"),
        };
        let page = read(state, security_ctx, &page_query).await;
        assert!(page.entities.len() <= top, "{page_query}");
        ids.extend(page.entities.iter().map(listed_entity_id));
        assert_work(&page, work_per_page, &page_query);
        match page.next_skiptoken {
            Some(next) => token = Some(next),
            None => return ids,
        }
    }
    panic!("continuations never ended: {query}");
}

fn assert_work(page: &QueryPlaneReadResult, rows: usize, query: &str) {
    assert!(
        page.telemetry.materialized_count <= rows,
        "a page of {} rows materialized {} records (bound {rows}): {query}",
        page.entities.len(),
        page.telemetry.materialized_count,
    );
    assert!(
        page.telemetry.candidate_count <= rows,
        "a page of {} rows considered {} candidates (bound {rows}): {query}",
        page.entities.len(),
        page.telemetry.candidate_count,
    );
}

async fn turso_state(name: &str) -> (ServerState, Vec<Record>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("list-cost.db").display()),
        None,
    )
    .await
    .unwrap();
    let state = build_order_state_with_store(name, &store);
    let records = seed(&store, &TenantId::default()).await;
    (state, records, dir)
}

const NEWEST_FIRST: &str = "$filter=status ne 'Unmade'&$orderby=last_event_at desc";

#[tokio::test]
async fn newest_first_page_reads_only_its_rows() {
    let (state, records, _dir) = turso_state("list-cost-newest-first").await;
    let security_ctx = SecurityContext::system();
    let want = expected(&records, false);

    // First page and its continuation: the rows and order of the oracle,
    // each paid for with about one page of records.
    let first = read(&state, &security_ctx, &format!("{NEWEST_FIRST}&$top={TOP}")).await;
    let first_ids: Vec<String> = first.entities.iter().map(listed_entity_id).collect();
    assert_eq!(first_ids, want[..TOP].to_vec());
    let token = first
        .next_skiptoken
        .clone()
        .expect("a truncated page links on");
    let second = read(
        &state,
        &security_ctx,
        &format!("{NEWEST_FIRST}&$top={TOP}&$skiptoken={token}"),
    )
    .await;
    let second_ids: Vec<String> = second.entities.iter().map(listed_entity_id).collect();
    assert_eq!(second_ids, want[TOP..2 * TOP].to_vec());

    // `$count` names the whole filtered set, still from one page of work.
    let counted = read(
        &state,
        &security_ctx,
        &format!("{NEWEST_FIRST}&$top={TOP}&$count=true"),
    )
    .await;
    assert_eq!(counted.count, Some(want.len()));

    // Following every link enumerates the set once, in order.
    let all = walk(&state, &security_ctx, NEWEST_FIRST, 50, RECORDS).await;
    assert_eq!(all, want);

    // A projected page carries only what it selected.
    let projected = read(
        &state,
        &security_ctx,
        &format!("{NEWEST_FIRST}&$top={TOP}&$select=entity_id,f001"),
    )
    .await;
    for row in &projected.entities {
        assert!(row.get("f002").is_none(), "unselected field leaked: {row}");
        assert!(
            row.get("fields").is_none(),
            "unselected fields leaked: {row}"
        );
    }

    // The cost: one page of records per page, not the type.
    let page_work = 2 * (TOP + 1);
    assert_work(&first, page_work, "first page");
    assert_work(&second, page_work, "continuation");
    assert_work(&counted, page_work, "counted page");
    assert_work(&projected, page_work, "projected page");
    walk(&state, &security_ctx, NEWEST_FIRST, 50, 2 * 51).await;
}

#[tokio::test]
async fn row_policy_pages_stay_bounded_and_hide_their_rows() {
    let (state, records, _dir) = turso_state("list-cost-row-policy").await;
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"
            permit(principal, action == Action::"list", resource is Order);
            permit(principal, action == Action::"read", resource is Order);
            forbid(principal, action == Action::"read", resource is Order)
                when { resource.Secret == "yes" };
            "#,
        )
        .expect("install row policy");
    let security_ctx = reader();
    let want = expected(&records, true);

    let first = read(&state, &security_ctx, &format!("{NEWEST_FIRST}&$top={TOP}")).await;
    let first_ids: Vec<String> = first.entities.iter().map(listed_entity_id).collect();
    assert_eq!(first_ids, want[..TOP].to_vec());
    let all = walk(&state, &security_ctx, NEWEST_FIRST, 50, RECORDS).await;
    assert_eq!(all, want, "a hidden row was listed, or a visible one lost");

    // A third of the rows are hidden, so a page reads a few more records than
    // it returns, never the type.
    assert_work(&first, 4 * (TOP + 1), "first page under a row policy");
    walk(&state, &security_ctx, NEWEST_FIRST, 50, 4 * 51).await;
}
