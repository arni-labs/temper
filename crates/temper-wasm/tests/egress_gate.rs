//! The `http_call` gate decides on the host each WASM egress request reaches,
//! redirects included. Every test drives `AuthorizedWasmHost` over the real
//! `ProductionWasmHost` (reqwest), wired as the server wires it, against a
//! loopback server that logs each request it receives.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, LOCATION};
use axum::response::IntoResponse;
use temper_wasm::http_stream::HttpRequestHead;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate,
    WasmHost,
};

/// Allows outbound calls to the listed hosts only, like a domain-scoped
/// `http_call` permit.
struct HostsGate(&'static [&'static str]);

impl WasmAuthzGate for HostsGate {
    fn authorize_http_call(
        &self,
        domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        if self.0.contains(&domain) {
            WasmAuthzDecision::Allow
        } else {
            WasmAuthzDecision::Deny(format!("{domain} is not allowed"))
        }
    }

    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

fn ctx() -> WasmAuthzContext {
    WasmAuthzContext {
        tenant: "test-tenant".into(),
        module_name: "egress-module".into(),
        agent_id: None,
        session_id: None,
        entity_type: "Order".into(),
        trigger_action: "Submit".into(),
    }
}

/// The host chain as the server builds it: the same gate checks the guest's
/// calls and every redirect hop.
fn gated_host(allowed: &'static [&'static str]) -> AuthorizedWasmHost {
    let gate: Arc<dyn WasmAuthzGate> = Arc::new(HostsGate(allowed));
    let production =
        ProductionWasmHost::new(BTreeMap::new()).with_redirect_gate(Arc::clone(&gate), ctx());
    AuthorizedWasmHost::new(Arc::new(production), gate, ctx())
}

/// A server on 127.0.0.1 that logs `METHOD host/path auth=<bool> bytes=<n>` for
/// every request. `/start` answers 302 to this server under the name
/// `localhost`, `/again` 307 to `/landed`, and `/loop` 302 to itself.
async fn logging_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&log);
    let elsewhere = format!("http://localhost:{}/landed", address.port());
    let app = Router::new().fallback(move |request: Request| {
        let seen = Arc::clone(&seen);
        let elsewhere = elsewhere.clone();
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let headers = request.headers().clone();
            let body = axum::body::to_bytes(request.into_body(), 1024)
                .await
                .unwrap();
            seen.lock().unwrap().push(format!(
                "{method} {}{path} auth={} bytes={}",
                headers[HOST].to_str().unwrap(),
                headers.contains_key(AUTHORIZATION),
                body.len()
            ));
            match path.as_str() {
                "/start" => (StatusCode::FOUND, [(LOCATION, elsewhere)]).into_response(),
                "/again" => {
                    (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/landed")]).into_response()
                }
                "/loop" => (StatusCode::FOUND, [(LOCATION, "/loop")]).into_response(),
                _ if headers.contains_key("connect-protocol-version") => {
                    // One Connect data frame carrying the JSON string "landed".
                    let mut frame = vec![0, 0, 0, 0, 8];
                    frame.extend_from_slice(br#""landed""#);
                    frame.into_response()
                }
                _ => "landed".into_response(),
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, log)
}

fn take(log: &Mutex<Vec<String>>) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

fn stream_head(method: &str, url: &str) -> HttpRequestHead {
    HttpRequestHead {
        method: method.into(),
        url: url.into(),
        headers: vec![("Authorization".into(), "Bearer key".into())],
    }
}

/// An '@' after the authority, or after a backslash (which the URL parser
/// reads as '/'), once made the gate check `localhost` while the call went to
/// 127.0.0.1. The gate now sees 127.0.0.1 on all four entry points.
#[tokio::test]
async fn gate_checks_the_host_each_outbound_call_reaches() {
    let (address, log) = logging_server().await;
    let host = gated_host(&["localhost"]);
    for url in [
        format!("http://{address}/@localhost"),
        format!("http://{address}/q?@localhost"),
        format!("http://{address}/f#@localhost"),
        format!("http://{address}\\@localhost"),
    ] {
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
                host.http_stream_begin_outbound(stream_head("GET", &url))
                    .await
                    .map(drop),
            ),
        ];
        for (operation, result) in results {
            assert_eq!(
                result,
                Err(format!(
                    "authorization denied for {operation} to 127.0.0.1: 127.0.0.1 is not allowed"
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
    assert_eq!(take(&log), Vec::<String>::new(), "a refused call was sent");

    // The same server answers when the gate allows the host it listens on.
    let allowed = gated_host(&["127.0.0.1"]);
    assert_eq!(
        allowed
            .http_call("GET", &format!("http://{address}/@localhost"), &[], "")
            .await,
        Ok((200, "landed".into()))
    );
    assert_eq!(take(&log).len(), 1);
}

/// A redirect to a host the gate allows is followed, as reqwest's own policy
/// did: 302 resends as GET without the body, 307 resends the request as it
/// was, and credentials stay behind when the host changes.
#[tokio::test]
async fn redirects_to_an_allowed_host_are_followed() {
    let (address, log) = logging_server().await;
    let host = gated_host(&["127.0.0.1", "localhost"]);
    let port = address.port();
    let key = [("Authorization".to_owned(), "Bearer key".to_owned())];
    let start = format!("http://{address}/start");

    assert_eq!(
        host.http_call("GET", &start, &key, "").await,
        Ok((200, "landed".into()))
    );
    assert_eq!(
        take(&log),
        [
            format!("GET 127.0.0.1:{port}/start auth=true bytes=0"),
            format!("GET localhost:{port}/landed auth=false bytes=0"),
        ]
    );

    assert_eq!(
        host.http_call_binary("POST", &format!("http://{address}/again"), &key, b"kept")
            .await,
        Ok((200, b"landed".to_vec()))
    );
    assert_eq!(
        take(&log),
        [
            format!("POST 127.0.0.1:{port}/again auth=true bytes=4"),
            format!("POST 127.0.0.1:{port}/landed auth=true bytes=4"),
        ]
    );

    assert_eq!(
        host.connect_call(&start, &key, "{}").await,
        Ok(vec![r#""landed""#.to_owned()])
    );
    assert_eq!(
        take(&log),
        [
            format!("POST 127.0.0.1:{port}/start auth=true bytes=7"),
            format!("GET localhost:{port}/landed auth=false bytes=0"),
        ]
    );

    let handles = host
        .http_stream_begin_outbound(stream_head("GET", &start))
        .await
        .unwrap();
    host.http_stream_close(handles.request_body).await.unwrap();
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(
        take(&log),
        [
            format!("GET 127.0.0.1:{port}/start auth=true bytes=0"),
            format!("GET localhost:{port}/landed auth=false bytes=0"),
        ]
    );

    assert_eq!(
        host.http_call("GET", &format!("http://{address}/loop"), &[], "")
            .await,
        Err("http_call refused: more than 10 redirects".into())
    );
    assert_eq!(take(&log).len(), 11, "the first request and 10 hops");
}

/// A redirect to a host the gate does not allow is denied before it is sent:
/// `/start` points at `localhost`, and nothing reaches it.
#[tokio::test]
async fn redirects_to_a_disallowed_host_are_denied_before_they_are_sent() {
    let (address, log) = logging_server().await;
    let host = gated_host(&["127.0.0.1"]);
    let start = format!("http://{address}/start");
    let denied = |operation: &str| {
        format!(
            "authorization denied for {operation} redirect to localhost: localhost is not allowed"
        )
    };

    assert_eq!(
        host.http_call("GET", &start, &[], "").await,
        Err(denied("http_call"))
    );
    assert_eq!(
        host.http_call_binary("GET", &start, &[], &[]).await,
        Err(denied("http_call_binary"))
    );
    assert_eq!(
        host.connect_call(&start, &[], "{}").await,
        Err(denied("connect_call"))
    );
    let handles = host
        .http_stream_begin_outbound(stream_head("GET", &start))
        .await
        .unwrap();
    host.http_stream_close(handles.request_body).await.unwrap();
    let head = host
        .http_stream_response_head(handles.response_body)
        .await
        .unwrap();
    assert_eq!(head.status, 0);
    assert_eq!(
        head.headers,
        [(
            "x-temper-stream-error".to_owned(),
            denied("http_stream_begin_outbound")
        )]
    );
    let requests = take(&log);
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert!(
        requests.iter().all(|request| request.contains("/start ")),
        "a denied hop was sent: {requests:?}"
    );

    // A host with no gate attached follows nothing: the 3xx goes back.
    let unattached = AuthorizedWasmHost::new(
        Arc::new(ProductionWasmHost::new(BTreeMap::new())),
        Arc::new(HostsGate(&["127.0.0.1"])),
        ctx(),
    );
    assert_eq!(
        unattached.http_call("GET", &start, &[], "").await,
        Ok((302, String::new()))
    );
    assert_eq!(take(&log).len(), 1);
}
