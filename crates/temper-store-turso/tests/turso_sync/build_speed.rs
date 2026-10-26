//! Scratch measurement only: where building a long change log spends its time.

use std::time::Instant;

use super::support::*;

const N: i64 = 20_000;

async fn timed(label: &str, sql: &str) -> String {
    let Some(server) = SyncServer::start() else {
        return String::new();
    };
    let (_dir, path) = machine();
    let store = server.open(&path).await;
    let copy = store.sync_copy().expect("copy").clone();
    write(&store, "pushed", 0).await;
    push(&store).await;
    if let Some(setup) = sql.split_once(";;").map(|(setup, _)| setup) {
        copy.execute_local(setup).await.expect("setup");
    }
    let body = sql.split_once(";;").map_or(sql, |(_, body)| body);
    let started = Instant::now();
    let changed = copy.execute_local(body).await.expect(label);
    let took = started.elapsed();
    let line = format!(
        "{label}: {changed} rows in {took:?}, {:.0} rows/s",
        changed as f64 / took.as_secs_f64()
    );
    eprintln!("{line}");
    line
}

#[tokio::test]
async fn where_a_long_log_build_spends_its_time() {
    let digits = "WITH d(x) AS (VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)), \
                  t(x) AS (VALUES (0),(1)) ";
    let lines = vec![
        timed(
            "cdc_series_100b",
            &format!(
                "INSERT INTO turso_cdc (change_id, change_time, change_type, table_name, id, before, after, updates) \
                 SELECT value - {N}, 0, 1, 'events', value, NULL, randomblob(100), NULL FROM generate_series(1, {N})"
            ),
        )
        .await,
        timed(
            "cdc_series_zeroblob",
            &format!(
                "INSERT INTO turso_cdc (change_id, change_time, change_type, table_name, id, before, after, updates) \
                 SELECT value - {N}, 0, 1, 'events', value, NULL, zeroblob(100), NULL FROM generate_series(1, {N})"
            ),
        )
        .await,
        timed(
            "cdc_cte_cross_join",
            &format!(
                "INSERT INTO turso_cdc (change_id, change_time, change_type, table_name, id, before, after, updates) \
                 {digits}SELECT -(t.x*10000 + a.x*1000 + b.x*100 + c.x*10 + e.x), 0, 1, 'events', 1, NULL, randomblob(100), NULL \
                 FROM t, d a, d b, d c, d e"
            ),
        )
        .await,
        timed(
            "plain_table_series",
            &format!(
                "CREATE TABLE bulk(id INTEGER PRIMARY KEY, after BLOB);;\
                 INSERT INTO bulk SELECT value, randomblob(100) FROM generate_series(1, {N})"
            ),
        )
        .await,
        timed(
            "series_count_only",
            &format!(
                "CREATE TABLE cnt(n INTEGER);;\
                 INSERT INTO cnt SELECT count(*) FROM generate_series(1, {N})"
            ),
        )
        .await,
    ];
    panic!("measurements:\n{}", lines.join("\n"));
}
