//! The query shapes an app's screens send, each paid for by its page: a feed
//! of the whole type newest first, a set of statuses, a time range, a count
//! alone, and `$select` with each.

use super::*;

/// A feed's page size.
const PAGE: usize = 50;

/// Ids of the records `keep` admits, oldest first event first (unknown times
/// last), ties by id: the order `$orderby=first_event_at asc` pages in.
fn oldest_first(records: &[Record], keep: impl Fn(&Record) -> bool) -> Vec<String> {
    let mut kept: Vec<&Record> = records.iter().filter(|record| keep(record)).collect();
    kept.sort_by(|a, b| match (a.last_event_at, b.last_event_at) {
        (None, None) => a.id.cmp(&b.id),
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.id.cmp(&b.id)),
    });
    kept.into_iter().map(|record| record.id.clone()).collect()
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

/// The first screen of a feed: every record of the type, newest move first,
/// no filter. It reads its page and the one row that says more follow.
#[tokio::test]
async fn newest_moves_of_the_whole_type_read_one_page() {
    let (state, records, _dir) = turso_state("list-cost-newest-moves").await;
    let ctx = SecurityContext::system();
    let want = newest_first(&records, |_| true);
    let query = "$orderby=last_event_at desc";

    let first = read(&state, &ctx, &format!("{query}&$top={PAGE}")).await;
    assert_eq!(ids(&first), want[..PAGE].to_vec());
    let token = first
        .next_skiptoken
        .clone()
        .expect("a truncated page links on");
    let second = read(
        &state,
        &ctx,
        &format!("{query}&$top={PAGE}&$skiptoken={token}"),
    )
    .await;
    assert_eq!(ids(&second), want[PAGE..2 * PAGE].to_vec());
    let projected = read(
        &state,
        &ctx,
        &format!("{query}&$top={PAGE}&$select=Id,f001"),
    )
    .await;
    assert_eq!(ids(&projected), want[..PAGE].to_vec());
    assert_projected(&projected, &["Id", "f001"]);

    assert_work(&first, PAGE + 1, "first page");
    assert_work(&second, PAGE + 1, "continuation");
    assert_work(&projected, PAGE + 1, "projected page");
    let all = walk(&state, &ctx, query, PAGE, PAGE + 1).await;
    assert_eq!(all, want);
}

/// A board column: a set of statuses, written as `or` or as `in`, counted,
/// projected and paged.
#[tokio::test]
async fn a_status_set_reads_one_page_with_its_count() {
    let (state, records, _dir) = turso_state("list-cost-status-set").await;
    let ctx = SecurityContext::system();
    let want = newest_first(&records, |record| {
        matches!(record.status, "Draft" | "Created")
    });

    for filter in [
        "Status eq 'Draft' or Status eq 'Created'",
        "Status in ('Draft', 'Created')",
    ] {
        let query = format!("$filter={filter}&$orderby=last_event_at desc&$select=Id,f001");
        let page = read(&state, &ctx, &format!("{query}&$top={TOP}&$count=true")).await;
        assert_eq!(ids(&page), want[..TOP].to_vec(), "{filter}");
        assert_eq!(page.count, Some(want.len()), "{filter}");
        assert_projected(&page, &["Id", "f001"]);
        assert_work(&page, TOP + 1, &query);
        let all = walk(&state, &ctx, &query, PAGE, PAGE + 1).await;
        assert_eq!(all, want, "{filter}");
    }
}

/// What moved since a time, newest first, and how many moved since another.
#[tokio::test]
async fn a_time_range_pages_in_the_catalog() {
    let (state, records, _dir) = turso_state("list-cost-time-range").await;
    let ctx = SecurityContext::system();

    let since = at("2026-01-01T00:02:00Z");
    let want = newest_first(&records, |record| {
        record.last_event_at.is_some_and(|time| time > since)
    });
    let query =
        "$filter=last_event_at gt '2026-01-01T00:02:00Z'&$orderby=last_event_at desc&$select=Id";
    let first = read(&state, &ctx, &format!("{query}&$top={TOP}")).await;
    assert_projected(&first, &["Id"]);
    assert_work(&first, TOP + 1, query);
    let all = walk(&state, &ctx, query, PAGE, PAGE + 1).await;
    assert_eq!(all, want);

    // A fractional bound compares as the instant it names.
    let from = at("2026-01-01T00:02:30.5Z");
    let counted = newest_first(&records, |record| {
        record.status != "Unmade" && record.last_event_at.is_some_and(|time| time >= from)
    });
    let query = "$filter=last_event_at ge '2026-01-01T00:02:30.5Z' and Status ne 'Unmade'&$top=0&$count=true";
    let page = read(&state, &ctx, query).await;
    assert!(page.entities.is_empty(), "{query}");
    assert_eq!(page.count, Some(counted.len()), "{query}");
    assert_work(&page, 1, query);
}

/// A tab's badge: how many records match, with no rows.
#[tokio::test]
async fn a_count_alone_reads_no_records() {
    let (state, records, _dir) = turso_state("list-cost-count-alone").await;
    let ctx = SecurityContext::system();
    type Keep = fn(&Record) -> bool;
    let cases: [(&str, Keep); 3] = [
        ("Status eq 'Draft'", |record| record.status == "Draft"),
        ("Place eq 'place-2'", |record| record.place == "place-2"),
        ("Status ne 'Unmade' and Place eq 'place-3'", |record| {
            record.status != "Unmade" && record.place == "place-3"
        }),
    ];
    for (filter, keep) in cases {
        let query = format!("$filter={filter}&$top=0&$count=true");
        let page = read(&state, &ctx, &query).await;
        assert!(page.entities.is_empty(), "{query}");
        assert_eq!(
            page.count,
            Some(records.iter().filter(|record| keep(record)).count()),
            "{query}"
        );
        assert_work(&page, 1, &query);
    }
}

/// The other record time pages the same way, oldest first.
#[tokio::test]
async fn oldest_first_records_page_by_their_first_event() {
    let (state, records, _dir) = turso_state("list-cost-oldest-first").await;
    let ctx = SecurityContext::system();
    let want = oldest_first(&records, |_| true);
    let query = "$orderby=first_event_at asc&$select=Id,first_event_at";
    let first = read(&state, &ctx, &format!("{query}&$top={PAGE}")).await;
    assert_eq!(ids(&first), want[..PAGE].to_vec());
    assert_projected(&first, &["Id", "first_event_at"]);
    // The projected time is the one a whole-record read lists.
    let whole = read(
        &state,
        &ctx,
        &format!("$orderby=first_event_at asc&$top={PAGE}"),
    )
    .await;
    for (row, full) in first.entities.iter().zip(&whole.entities) {
        assert_eq!(row["first_event_at"], full["first_event_at"], "{row}");
    }
    let all = walk(&state, &ctx, query, PAGE, PAGE + 1).await;
    assert_eq!(all, want);
}

/// An app's runs list: a filter on a field every record shares, newest first
/// by an app time field, a few fields selected, ten rows. Read by an operator
/// every row is allowed to, then by a reader a row policy hides some from.
#[tokio::test]
async fn a_field_ordered_page_reads_its_rows_whatever_the_filter_keeps() {
    let (state, records, _dir) = turso_state("list-cost-field-order").await;
    let query = format!(
        "$filter=CustomerId eq '{CUSTOMER}'&$orderby=CreatedAt desc\
         &$select=entity_id,status,CreatedAt,f001&$top={TOP}"
    );
    let newest = |hide_secret: bool| {
        let mut kept: Vec<&Record> = records
            .iter()
            .filter(|record| !(hide_secret && record.secret))
            .collect();
        kept.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        kept.into_iter()
            .take(TOP)
            .map(|record| record.id.clone())
            .collect::<Vec<_>>()
    };

    let page = read(&state, &SecurityContext::system(), &query).await;
    assert_eq!(ids(&page), newest(false));
    assert_projected(&page, &["entity_id", "status", "CreatedAt", "f001"]);
    assert_work(&page, TOP + 1, &query);

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
    let page = read(&state, &reader(), &query).await;
    assert_eq!(ids(&page), newest(true));
    // A third of the rows are hidden, so the page reads a few more records
    // than it returns, never the type.
    assert_work(&page, 4 * (TOP + 1), &query);
}
