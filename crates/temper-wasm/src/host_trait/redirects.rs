//! Redirects for WASM egress, followed one gated hop at a time.
//!
//! The egress clients follow no redirects themselves (`http_clients.rs`),
//! because the `http_call` gate sees only the URL the guest names. The host
//! follows them here instead and asks the same gate about every hop before it
//! sends it, so a 3xx can only lead to a host the guest could call directly.

use std::sync::Arc;

use reqwest::header::{self, HeaderMap};
use reqwest::{Method, StatusCode, Url};

use crate::authorized_host::{WasmAuthzGate, authorize_outbound};
use crate::types::WasmAuthzContext;

use super::internal_http;

/// Most redirects one call follows: reqwest's own default, which is what guest
/// calls followed before each hop went through the gate.
const REDIRECT_HOP_BUDGET: usize = 10;

/// Why an outbound call ended without a response.
pub(super) enum OutboundError {
    /// The transport failed.
    Http(reqwest::Error),
    /// A redirect hop was refused: the gate denied it or the budget ran out.
    Refused(String),
}

impl OutboundError {
    /// The message the guest sees: a refusal as it stands, a transport error
    /// as the calling path words it.
    pub(super) fn into_message(self, transport: impl FnOnce(reqwest::Error) -> String) -> String {
        match self {
            Self::Http(error) => transport(error),
            Self::Refused(reason) => reason,
        }
    }
}

impl std::fmt::Display for OutboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::Refused(reason) => f.write_str(reason),
        }
    }
}

/// What following a redirect needs from the host. Cloned into the task that
/// sends a streaming request.
#[derive(Clone)]
pub(super) struct Redirects {
    /// The gate and context the host's `AuthorizedWasmHost` uses. Without
    /// them no redirect is followed: the 3xx goes back to the guest.
    pub(super) gate: Option<(Arc<dyn WasmAuthzGate>, WasmAuthzContext)>,
    /// The internal Temper API, which redirects neither leave nor enter: its
    /// capabilities are bound to the one request the guest made.
    pub(super) internal_api_base_url: Option<String>,
}

impl Redirects {
    /// Send the request `builder` holds and follow its redirects, each hop
    /// checked by the gate before it is sent.
    pub(super) async fn send(
        &self,
        client: &reqwest::Client,
        builder: reqwest::RequestBuilder,
        operation: &str,
    ) -> Result<reqwest::Response, OutboundError> {
        let request = builder.build().map_err(OutboundError::Http)?;
        let (gate, ctx) = match &self.gate {
            Some((gate, ctx)) if !self.is_internal(request.url()) => (gate, ctx),
            _ => return client.execute(request).await.map_err(OutboundError::Http),
        };
        let mut hop = Hop::of(&request);
        let mut response = client.execute(request).await.map_err(OutboundError::Http)?;
        let mut budget = REDIRECT_HOP_BUDGET;
        while let Some(next) = hop.next(&response) {
            if self.is_internal(&next.url) {
                break;
            }
            if budget == 0 {
                return Err(OutboundError::Refused(format!(
                    "{operation} refused: more than {REDIRECT_HOP_BUDGET} redirects"
                )));
            }
            budget -= 1;
            authorize_outbound(
                gate.as_ref(),
                ctx,
                &format!("{operation} redirect"),
                next.method.as_str(),
                next.url.as_str(),
            )
            .map_err(OutboundError::Refused)?;
            response = client
                .execute(next.request())
                .await
                .map_err(OutboundError::Http)?;
            hop = next;
        }
        Ok(response)
    }

    fn is_internal(&self, url: &Url) -> bool {
        internal_http::is_internal_url(
            url.as_str(),
            self.internal_api_base_url.iter().map(String::as_str),
        )
    }
}

/// One request of a redirect chain, kept so the next hop can be built from it.
struct Hop {
    method: Method,
    url: Url,
    headers: HeaderMap,
    /// The body, when it can be sent again.
    body: Option<bytes::Bytes>,
    /// The body was a stream, which cannot be sent again.
    streamed: bool,
}

impl Hop {
    fn of(request: &reqwest::Request) -> Self {
        let body = request.body().map(reqwest::Body::as_bytes);
        Self {
            method: request.method().clone(),
            url: request.url().clone(),
            headers: request.headers().clone(),
            body: body.flatten().map(bytes::Bytes::copy_from_slice),
            streamed: body.is_some_and(|bytes| bytes.is_none()),
        }
    }

    /// The request a redirect `response` asks for, following the rules
    /// reqwest's own policy used; `None` when it is not one to follow.
    fn next(&self, response: &reqwest::Response) -> Option<Self> {
        // 301, 302 and 303 resend as GET without the body; 307 and 308 resend
        // the request as it was, which a streamed body cannot be.
        let keeps_request = match response.status() {
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => false,
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT if !self.streamed => {
                true
            }
            _ => return None,
        };
        let location = response.headers().get(header::LOCATION)?.to_str().ok()?;
        let mut next = Self {
            method: self.method.clone(),
            url: self.url.join(location).ok()?,
            headers: self.headers.clone(),
            body: self.body.clone(),
            streamed: self.streamed,
        };
        if !keeps_request {
            if next.method != Method::GET && next.method != Method::HEAD {
                next.method = Method::GET;
            }
            next.body = None;
            next.streamed = false;
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_ENCODING,
                header::TRANSFER_ENCODING,
            ] {
                next.headers.remove(name);
            }
        }
        if next.url.host_str() != self.url.host_str()
            || next.url.port_or_known_default() != self.url.port_or_known_default()
        {
            for name in [
                header::AUTHORIZATION,
                header::COOKIE,
                header::PROXY_AUTHORIZATION,
                header::WWW_AUTHENTICATE,
            ] {
                next.headers.remove(name);
            }
            next.headers.remove("cookie2");
        }
        Some(next)
    }

    fn request(&self) -> reqwest::Request {
        let mut request = reqwest::Request::new(self.method.clone(), self.url.clone());
        *request.headers_mut() = self.headers.clone();
        *request.body_mut() = self.body.clone().map(reqwest::Body::from);
        request
    }
}
