//! Authorization-wrapper tests, including ADR-0166 policy forwarding.
use super::*;
use crate::host_trait::SimWasmHost;

/// A gate that denies everything.
struct DenyAllGate;
impl WasmAuthzGate for DenyAllGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }
    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }
}

/// A gate that allows everything.
struct AllowAllGate;
impl WasmAuthzGate for AllowAllGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

fn test_ctx() -> WasmAuthzContext {
    WasmAuthzContext::test_fixture()
}

/// ADR-0166. This wrapper is what dispatch hands to the engine, so the
/// engine's view of the tenant's content decision is whatever this forwards.
/// Both directions matter: not forwarding `true` silently disables the opt-in
/// for every tenant, and not forwarding `false` would leak.
#[test]
fn authorized_host_forwards_the_llm_content_export_decision() {
    use crate::host_trait::ProductionWasmHost;
    use std::collections::BTreeMap;

    let opted_in: Arc<dyn WasmHost> =
        Arc::new(ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(true));
    let wrapped = AuthorizedWasmHost::new(opted_in, Arc::new(AllowAllGate), test_ctx());
    assert!(
        wrapped.exports_llm_content(),
        "an opted-in tenant's decision must survive the authz wrapper"
    );

    let redacted: Arc<dyn WasmHost> =
        Arc::new(ProductionWasmHost::new(BTreeMap::new()).with_llm_content_export(false));
    let wrapped = AuthorizedWasmHost::new(redacted, Arc::new(AllowAllGate), test_ctx());
    assert!(!wrapped.exports_llm_content());
}

/// A host that does not answer must redact. Any future `WasmHost` that forgets
/// to implement the method inherits this, so the default is the whole
/// protection for that host.
#[test]
fn wasm_host_defaults_to_redacting_llm_content() {
    struct MinimalHost;
    #[async_trait]
    impl WasmHost for MinimalHost {
        async fn http_call(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: &str,
        ) -> Result<(u16, String), String> {
            Ok((200, String::new()))
        }
        async fn http_call_binary(
            &self,
            _method: &str,
            _url: &str,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> Result<(u16, Vec<u8>), String> {
            Ok((200, Vec::new()))
        }
        fn get_secret(&self, _key: &str) -> Result<String, String> {
            Err("no secrets".to_string())
        }
        fn log(&self, _level: &str, _message: &str) {}
    }
    assert!(
        !MinimalHost.exports_llm_content(),
        "the WasmHost default must be redact; a host that does not opt in must \
         never export a tenant's LLM content"
    );
}

#[tokio::test]
async fn deny_gate_blocks_http_call() {
    let inner = Arc::new(SimWasmHost::new());
    let gate = Arc::new(DenyAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

    let result = host
        .http_call("POST", "https://api.stripe.com/v1/charges", &[], "")
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("authorization denied"));
}

#[tokio::test]
async fn deny_gate_blocks_secret_access() {
    let inner = Arc::new(SimWasmHost::new().with_secret("STRIPE_API_KEY", "sk-test"));
    let gate = Arc::new(DenyAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

    let result = host.get_secret("STRIPE_API_KEY");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("authorization denied"));
}

#[tokio::test]
async fn allow_gate_delegates_http_call() {
    let inner = Arc::new(SimWasmHost::new());
    let gate = Arc::new(AllowAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

    let result = host
        .http_call("GET", "https://api.stripe.com/v1/charges", &[], "")
        .await;
    assert!(result.is_ok());
    let (status, _body) = result.unwrap();
    assert_eq!(status, 200);
}

#[tokio::test]
async fn allow_gate_delegates_secret_access() {
    let inner = Arc::new(SimWasmHost::new().with_secret("KEY", "val"));
    let gate = Arc::new(AllowAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

    let result = host.get_secret("KEY");
    assert_eq!(result, Ok("val".into()));
}

#[test]
fn allow_gate_delegates_evaluate_spec() {
    let ioa_source = "[automaton]\nname = \"Issue\"";
    let ioa_hash = format!("{:x}", ioa_source.len());
    let inner = Arc::new(SimWasmHost::new().with_spec_eval_response(
        &ioa_hash,
        "Reassign",
        r#"{"success":true,"new_state":"InProgress"}"#,
    ));
    let gate = Arc::new(AllowAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());

    let result = host.evaluate_spec(ioa_source, "Backlog", "Reassign", "{}");
    assert!(
        result.is_ok(),
        "evaluate_spec should delegate to inner host"
    );
    assert!(
        result.unwrap_or_default().contains(r#""success":true"#),
        "expected canned evaluate_spec response from inner host"
    );
}

#[test]
fn logging_always_allowed() {
    let inner = Arc::new(SimWasmHost::new());
    let gate = Arc::new(DenyAllGate);
    let host = AuthorizedWasmHost::new(inner, gate, test_ctx());
    // Should not panic
    host.log("info", "test message");
}

#[test]
fn extract_domain_https() {
    assert_eq!(
        extract_domain("https://api.stripe.com/v1/charges").as_deref(),
        Some("api.stripe.com")
    );
}

#[test]
fn extract_domain_http() {
    assert_eq!(
        extract_domain("http://localhost:8080/api").as_deref(),
        Some("localhost")
    );
}

#[test]
fn extract_domain_with_port() {
    assert_eq!(
        extract_domain("https://example.com:443/path").as_deref(),
        Some("example.com")
    );
}

#[test]
fn extract_domain_refuses_a_url_without_a_scheme() {
    assert_eq!(extract_domain("api.stripe.com/path"), None);
}

#[test]
fn extract_domain_bare() {
    assert_eq!(
        extract_domain("https://example.com").as_deref(),
        Some("example.com")
    );
}

#[test]
fn extract_domain_ip() {
    assert_eq!(
        extract_domain("http://127.0.0.1:3000/api").as_deref(),
        Some("127.0.0.1")
    );
}

#[test]
fn extract_domain_refuses_userinfo() {
    assert_eq!(
        extract_domain("https://attacker:pass@localhost/exploit"),
        None
    );
    assert_eq!(extract_domain("https://localhost@api.stripe.com/"), None);
}

/// Allows outbound calls to one host only, like a domain-scoped `http_call`
/// permit.
struct OneHostGate(&'static str);
impl WasmAuthzGate for OneHostGate {
    fn authorize_http_call(
        &self,
        domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        if domain == self.0 {
            WasmAuthzDecision::Allow
        } else {
            WasmAuthzDecision::Deny(format!("{domain} is not {}", self.0))
        }
    }
    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

/// A server on 127.0.0.1 that counts the requests it answers. `/start` is not
/// counted: it answers 302 to this same server under the name `localhost`.
async fn counting_server() -> (
    std::net::SocketAddr,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use axum::Router;
    use axum::http::{StatusCode, header::LOCATION};
    use axum::routing::any;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let elsewhere = format!("http://localhost:{}/after-redirect", address.port());
    let app = Router::new()
        .route(
            "/start",
            any(move || {
                let location = elsewhere.clone();
                async move { (StatusCode::FOUND, [(LOCATION, location)]) }
            }),
        )
        .fallback(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            async { "reached" }
        });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, hits, server)
}

fn production_host(gate: OneHostGate) -> AuthorizedWasmHost {
    use crate::host_trait::ProductionWasmHost;
    use std::collections::BTreeMap;

    AuthorizedWasmHost::new(
        Arc::new(ProductionWasmHost::new(BTreeMap::new())),
        Arc::new(gate),
        test_ctx(),
    )
}

/// The gate decides on the host the request is sent to. An '@' after the
/// authority, or after a backslash (which the URL parser reads as '/'), once
/// made the gate check `localhost` while the call went to 127.0.0.1.
#[tokio::test]
async fn gate_checks_the_host_each_outbound_call_reaches() {
    use crate::http_stream::HttpRequestHead;
    use std::sync::atomic::Ordering;

    let (address, hits, server) = counting_server().await;
    let host = production_host(OneHostGate("localhost"));
    for url in [
        format!("http://{address}/@localhost"),
        format!("http://{address}/q?@localhost"),
        format!("http://{address}/f#@localhost"),
        format!("http://{address}\\@localhost"),
    ] {
        let stream = HttpRequestHead {
            method: "GET".into(),
            url: url.clone(),
            headers: Vec::new(),
        };
        let results = [
            (
                "http_call",
                host.http_call("GET", &url, &[], "").await.map(drop),
            ),
            (
                "http_call_binary",
                host.http_call_binary("GET", &url, &[], &[]).await.map(drop),
            ),
            (
                "connect_call",
                host.connect_call(&url, &[], "{}").await.map(drop),
            ),
            (
                "http_stream_begin_outbound",
                host.http_stream_begin_outbound(stream).await.map(drop),
            ),
        ];
        for (operation, result) in results {
            assert_eq!(
                result,
                Err(format!(
                    "authorization denied for {operation} to 127.0.0.1: 127.0.0.1 is not localhost"
                )),
                "{url}"
            );
        }
    }
    for url in [
        format!("http://localhost@{address}/"),
        format!("localhost:{}/", address.port()),
    ] {
        assert_eq!(
            host.http_call("GET", &url, &[], "").await,
            Err("http_call refused: the URL must name a host and carry no userinfo".into()),
            "{url}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a refused call was sent");

    // The same server answers when the gate allows the host it listens on.
    let allowed = production_host(OneHostGate("127.0.0.1"));
    assert_eq!(
        allowed
            .http_call("GET", &format!("http://{address}/@localhost"), &[], "")
            .await,
        Ok((200, "reached".into()))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

/// A 3xx from an allowed host goes back to the guest; the next hop, if any,
/// is a new call the gate checks. `/start` points at `localhost`, a host this
/// gate does not allow.
#[tokio::test]
async fn redirects_from_an_allowed_host_are_returned_not_followed() {
    use crate::http_stream::HttpRequestHead;
    use std::sync::atomic::Ordering;

    let (address, hits, server) = counting_server().await;
    let host = production_host(OneHostGate("127.0.0.1"));
    let start = format!("http://{address}/start");

    let (status, _) = host.http_call("GET", &start, &[], "").await.unwrap();
    assert_eq!(status, 302);
    let (status, _) = host
        .http_call_binary("GET", &start, &[], &[])
        .await
        .unwrap();
    assert_eq!(status, 302);
    let error = host.connect_call(&start, &[], "{}").await.unwrap_err();
    assert!(error.contains("HTTP 302"), "{error}");
    let handles = host
        .http_stream_begin_outbound(HttpRequestHead {
            method: "GET".into(),
            url: start,
            headers: Vec::new(),
        })
        .await
        .unwrap();
    host.http_stream_close(handles.request_body).await.unwrap();
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 302);

    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a redirect was followed past the gate"
    );
    server.abort();
}
