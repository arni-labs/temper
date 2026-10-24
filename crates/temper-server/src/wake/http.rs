//! The control plane's end of the reports, over HTTP.

use std::time::Duration;

use super::{WakeReport, WakeSink};

/// How long one report may take. A request the control plane never answered
/// would stay in flight for good, and the due time it carries would never be
/// sent again; one that times out is sent again at the next change or wake.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// POSTs each report as JSON to the control plane's address with the
/// process's pass as a bearer token.
pub struct HttpWakeSink {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl HttpWakeSink {
    /// Reports to `url`, authorized by `token`.
    pub fn new(url: String, token: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .map_err(|error| format!("wake report client: {error}"))?;
        Ok(Self { client, url, token })
    }
}

impl std::fmt::Debug for HttpWakeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpWakeSink")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl WakeSink for HttpWakeSink {
    async fn send(&self, report: &WakeReport, traceparent: Option<&str>) -> Result<(), String> {
        let mut request = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(report);
        if let Some(traceparent) = traceparent {
            request = request.header("traceparent", traceparent);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("control plane answered {}", response.status()))
        }
    }
}
