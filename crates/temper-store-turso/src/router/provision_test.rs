use super::PROVISIONING;
#[cfg(feature = "cloud")]
use super::Provisioned;
use crate::router::TenantStoreRouter;
use crate::store::WritePriority;

async fn routed(dir: &std::path::Path) -> TenantStoreRouter {
    TenantStoreRouter::new(
        &format!("file:{}", dir.join("platform.db").display()),
        None,
        Some(dir.join("tenants").to_string_lossy().into_owned()),
    )
    .await
    .expect("router")
}

async fn registered(router: &TenantStoreRouter) -> Vec<(String, String)> {
    router
        .load_tenant_registry()
        .await
        .expect("registry")
        .into_iter()
        .map(|row| (row.tenant_id, row.status))
        .collect()
}

#[tokio::test]
async fn a_database_left_behind_refuses_a_create_and_stays() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = routed(dir.path()).await;
    let left = router.local_database_path("alpha").expect("path");
    std::fs::create_dir_all(left.parent().expect("dir")).expect("dir");
    std::fs::write(&left, b"an earlier tenant's").expect("file");

    for _ in 0..2 {
        let refused = router.register_tenant("alpha").await.expect_err("refused");
        assert!(refused.to_string().contains("alpha.db"), "{refused}");
    }
    assert_eq!(std::fs::read(&left).expect("kept"), b"an earlier tenant's");
    assert!(registered(&router).await.is_empty(), "nothing is written");
    assert_eq!(
        router.left_over_database("alpha").await.expect("check"),
        Some(format!("database file {}", left.display()))
    );
    assert_eq!(
        router.left_over_database("beta").await.expect("check"),
        None
    );
}

#[tokio::test]
async fn a_create_that_stopped_part_way_is_finished_by_creating_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = routed(dir.path()).await;
    // The create wrote its row and connected to its new database, then
    // stopped before the row became active.
    router
        .platform
        .write_connection("test", WritePriority::High)
        .await
        .expect("conn")
        .execute(
            "INSERT INTO tenant_registry (tenant_id, turso_db_url, status) VALUES (?1, '', ?2)",
            crate::driver::params!["alpha", PROVISIONING],
        )
        .await
        .expect("row");
    let made = router.local_database_path("alpha").expect("path");
    std::fs::create_dir_all(made.parent().expect("dir")).expect("dir");
    std::fs::write(&made, b"").expect("file");

    assert!(router.list_tenants().await.expect("list").is_empty());
    assert!(
        !router.begin_tenant_removal("alpha").await.expect("delete"),
        "a create not yet ended is not deleted"
    );
    assert_eq!(
        router.left_over_database("alpha").await.expect("check"),
        None
    );

    router.register_tenant("alpha").await.expect("finished");
    assert_eq!(
        registered(&router).await,
        vec![("alpha".to_string(), "active".to_string())]
    );
    assert!(router.register_tenant("alpha").await.is_err(), "taken now");
}

#[cfg(feature = "cloud")]
async fn cloud_router(dir: &std::path::Path, server: &wiremock::MockServer) -> TenantStoreRouter {
    let mut router =
        routed(dir)
            .await
            .with_cloud_config("api-token".to_string(), "acme".to_string(), None);
    router.turso_api_base_url = server.uri();
    router
}

#[cfg(feature = "cloud")]
#[tokio::test]
async fn a_cloud_database_left_behind_refuses_a_create() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/organizations/acme/databases/temper-alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "database": { "hostname": "alpha.db.turso.io" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let router = cloud_router(dir.path(), &server).await;

    let refused = router.register_tenant("alpha").await.expect_err("refused");
    assert!(refused.to_string().contains("temper-alpha"), "{refused}");
    assert!(registered(&router).await.is_empty(), "nothing is written");
    server.verify().await;
}

#[cfg(feature = "cloud")]
#[tokio::test]
async fn a_cloud_database_already_made_is_taken_over_only_by_the_create_that_made_it() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/organizations/acme/databases"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "database already exists"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/organizations/acme/databases/temper-alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "database": { "hostname": "alpha.db.turso.io" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/organizations/acme/databases/temper-alpha/auth/tokens",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "jwt": "token-123"
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let router = cloud_router(dir.path(), &server).await;

    match router
        .provision_cloud_database("alpha", "api-token", "acme", false)
        .await
        .expect("provision")
    {
        Provisioned::LeftOver(left) => assert!(left.contains("temper-alpha"), "{left}"),
        Provisioned::Made { .. } => panic!("a create of a new id took over a database"),
    }
    match router
        .provision_cloud_database("alpha", "api-token", "acme", true)
        .await
        .expect("provision")
    {
        Provisioned::Made { db_url, auth_token } => {
            assert_eq!(db_url, "libsql://alpha.db.turso.io");
            assert_eq!(auth_token.as_deref(), Some("token-123"));
        }
        Provisioned::LeftOver(left) => panic!("a resumed create refused its own: {left}"),
    }
}
