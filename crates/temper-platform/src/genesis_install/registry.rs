//! The Genesis registry this process installs apps from.

use std::fmt;

use reqwest::Url;

/// The setting that names this process's Genesis registry.
const REGISTRY_SETTING: &str = "TEMPER_GENESIS_REGISTRY_URL";

/// The setting that holds this process's credential for that registry: a
/// bearer token the registry resolves to a read-only identity. Unset, every
/// registry read is anonymous and only public apps can be fetched.
const TOKEN_SETTING: &str = "TEMPER_GENESIS_REGISTRY_TOKEN";

/// The Genesis registry this process fetches app bundles and App rows from,
/// and the credential it presents there.
///
/// A new install takes its registry from [`configured_registry`], from
/// `TEMPER_GENESIS_REGISTRY_URL`: which host a server takes new app code from
/// is a deployment setting, so an install request may name a registry only to
/// say which one it means, and any other value is refused before a request is
/// made. Restoring an install this server already made uses
/// [`recorded_registry`], the registry that install came from.
///
/// Every request to the registry is built by [`Registry::get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registry {
    base: String,
    credential: Option<Credential>,
}

/// The `TEMPER_GENESIS_REGISTRY_TOKEN` secret. Its `Debug` form never shows it.
#[derive(Clone, PartialEq, Eq)]
struct Credential(String);

impl Credential {
    fn from_setting(raw: &str) -> Option<Self> {
        let token = raw.trim();
        (!token.is_empty()).then(|| Self(token.to_string()))
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(redacted)")
    }
}

impl Registry {
    /// The registry's base URL, without a trailing slash.
    pub(crate) fn as_str(&self) -> &str {
        &self.base
    }

    /// The full URL of `path` on this registry.
    pub(crate) fn url(&self, path: &str) -> String {
        // A path that did not start at the root would extend the host name
        // and send the request, and its credential, somewhere else.
        assert!(
            path.starts_with('/'),
            "registry path must start with '/': {path}"
        );
        format!("{}{path}", self.base)
    }

    /// A GET of `path` on this registry, in `registry_tenant`, carrying this
    /// process's registry credential when one is configured.
    pub(crate) fn get(
        &self,
        path: &str,
        registry_tenant: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let request = registry_client()?
            .get(self.url(path))
            .header("X-Tenant-Id", registry_tenant);
        Ok(match &self.credential {
            Some(Credential(token)) => request.bearer_auth(token),
            None => request,
        })
    }
}

/// The configured Genesis registry, checked against the one `requested` names.
///
/// An empty `requested` means the configured registry. With no registry
/// configured, nothing is fetched.
pub(crate) fn configured_registry(requested: &str) -> Result<Registry, String> {
    let configured = std::env::var(REGISTRY_SETTING).unwrap_or_default();
    let token = std::env::var(TOKEN_SETTING).unwrap_or_default();
    registry_for(&configured, requested, &token)
}

/// The registry a stored install record was made from, to fetch that same
/// install's files again when its cache directory is gone.
///
/// Restoring reproduces what this server already installed, so it does not
/// depend on the current setting: a restart with the setting unset or changed
/// keeps every installed app.
pub(crate) fn recorded_registry(recorded: &str) -> Result<Registry, String> {
    let configured = std::env::var(REGISTRY_SETTING).unwrap_or_default();
    let token = std::env::var(TOKEN_SETTING).unwrap_or_default();
    recorded_registry_for(recorded, &configured, &token)
}

/// The registry `configured` names, as if it were this server's setting.
#[cfg(test)]
pub(crate) fn registry_for_test(configured: &str) -> Registry {
    registry_for(configured, "", "").expect("test registry URL")
}

fn registry_for(configured: &str, requested: &str, token: &str) -> Result<Registry, String> {
    if configured.trim().is_empty() {
        return Err(format!(
            "no Genesis registry is configured on this server; set {REGISTRY_SETTING} to install apps from one"
        ));
    }
    let configured_form =
        comparable_form(configured).map_err(|error| format!("{REGISTRY_SETTING} {error}"))?;
    if !requested.trim().is_empty() {
        let requested_form =
            comparable_form(requested).map_err(|error| format!("registry_url {error}"))?;
        if requested_form != configured_form {
            return Err(format!(
                "registry_url must be this server's Genesis registry ({REGISTRY_SETTING}); leave it out to use that registry"
            ));
        }
    }
    Ok(Registry {
        base: configured.trim().trim_end_matches('/').to_string(),
        credential: Credential::from_setting(token),
    })
}

fn recorded_registry_for(
    recorded: &str,
    configured: &str,
    token: &str,
) -> Result<Registry, String> {
    let recorded_form =
        comparable_form(recorded).map_err(|error| format!("recorded registry_url {error}"))?;
    // The credential was issued by the configured registry. A record made from
    // any other registry is fetched without it, so it never reaches a host
    // that could replay it against the registry that issued it.
    let credential = comparable_form(configured)
        .is_ok_and(|configured_form| configured_form == recorded_form)
        .then(|| Credential::from_setting(token))
        .flatten();
    Ok(Registry {
        base: recorded.trim().trim_end_matches('/').to_string(),
        credential,
    })
}

/// A registry base URL in the form two spellings of one registry share:
/// scheme and host lower-cased, a default port dropped, no trailing slash.
fn comparable_form(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw.trim()).map_err(|error| format!("is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http:// or https:// URL".to_string());
    }
    // Request paths are appended to the base URL, so a query or fragment
    // would change which path is requested.
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or fragment".to_string());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// HTTP client for registry requests.
///
/// It does not follow redirects, so every response comes from the configured
/// registry itself and the credential is sent nowhere else.
fn registry_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build Genesis registry client: {error}"))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const CONFIGURED: &str = "https://genesis.example.test";
    const TOKEN: &str = "registry-test-token";
    const APP_ROW: &str = "/tdata/Apps('app-acme-notes')";

    /// The `Authorization` header of each request `server` received.
    async fn authorization_headers(server: &MockServer) -> Vec<Option<String>> {
        server
            .received_requests()
            .await
            .expect("request recording")
            .iter()
            .map(|request| {
                request
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().expect("header text").to_string())
            })
            .collect()
    }

    async fn ok_registry() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn an_empty_request_uses_the_configured_registry() {
        let registry = registry_for(" https://genesis.example.test/ ", "", "").expect("configured");
        assert_eq!(registry.as_str(), CONFIGURED);
    }

    #[test]
    fn another_spelling_of_the_configured_registry_is_the_same_registry() {
        for requested in [
            "https://genesis.example.test",
            "https://genesis.example.test/",
            "HTTPS://Genesis.Example.Test",
            "https://genesis.example.test:443",
        ] {
            let registry = registry_for(CONFIGURED, requested, "").expect(requested);
            assert_eq!(registry.as_str(), CONFIGURED, "{requested}");
        }
    }

    #[test]
    fn a_registry_other_than_the_configured_one_is_refused() {
        for requested in [
            "https://other.example.test",
            "http://genesis.example.test",
            "https://genesis.example.test:8443",
            "https://genesis.example.test/other",
            "http://127.0.0.1:18081",
            "https://genesis.example.test/?x=1",
            "https://genesis.example.test#x",
            "file:///etc",
            "not a url",
        ] {
            let error = registry_for(CONFIGURED, requested, TOKEN).expect_err(requested);
            assert!(error.contains("registry_url"), "{requested}: {error}");
        }
    }

    #[test]
    fn a_stored_install_is_fetched_again_from_the_registry_it_came_from() {
        let registry =
            recorded_registry_for(" http://127.0.0.1:18081/ ", "", "").expect("recorded");
        assert_eq!(registry.as_str(), "http://127.0.0.1:18081");
        for recorded in ["", "file:///etc", "https://genesis.example.test/?x=1"] {
            assert!(
                recorded_registry_for(recorded, CONFIGURED, TOKEN).is_err(),
                "{recorded}"
            );
        }
    }

    #[test]
    fn without_a_configured_registry_nothing_is_fetched() {
        for requested in ["", CONFIGURED] {
            let error = registry_for("", requested, TOKEN).expect_err("unset");
            assert!(error.contains(REGISTRY_SETTING), "{error}");
        }
    }

    #[tokio::test]
    async fn registry_reads_carry_the_configured_credential() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(APP_ROW))
            .and(header("X-Tenant-Id", "default"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        let registry = registry_for(&server.uri(), "", &format!(" {TOKEN} ")).expect("registry");

        let response = registry
            .get(APP_ROW, "default")
            .expect("request")
            .send()
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 200);
        server.verify().await;
    }

    #[tokio::test]
    async fn without_a_credential_registry_reads_are_anonymous() {
        let server = ok_registry().await;
        for token in ["", "   "] {
            registry_for(&server.uri(), "", token)
                .expect("registry")
                .get(APP_ROW, "default")
                .expect("request")
                .send()
                .await
                .expect("response");
        }
        assert_eq!(authorization_headers(&server).await, vec![None, None]);
    }

    #[tokio::test]
    async fn a_stored_install_from_the_configured_registry_is_fetched_with_its_credential() {
        let server = ok_registry().await;
        let configured = format!("{}/", server.uri().to_uppercase());
        recorded_registry_for(&server.uri(), &configured, TOKEN)
            .expect("recorded")
            .get(APP_ROW, "default")
            .expect("request")
            .send()
            .await
            .expect("response");
        assert_eq!(
            authorization_headers(&server).await,
            vec![Some(format!("Bearer {TOKEN}"))]
        );
    }

    #[tokio::test]
    async fn the_credential_is_never_sent_to_another_registry() {
        let server = ok_registry().await;
        for configured in [CONFIGURED, "", "not a url"] {
            recorded_registry_for(&server.uri(), configured, TOKEN)
                .expect("recorded")
                .get(APP_ROW, "default")
                .expect("request")
                .send()
                .await
                .expect("response");
        }
        assert_eq!(authorization_headers(&server).await, vec![None, None, None]);
    }

    #[test]
    fn the_credential_never_appears_in_debug_output() {
        let registry = registry_for(CONFIGURED, "", TOKEN).expect("registry");
        let shown = format!("{registry:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    #[should_panic(expected = "registry path must start with '/'")]
    fn a_path_that_would_change_the_host_is_refused() {
        registry_for(CONFIGURED, "", TOKEN)
            .expect("registry")
            .url(".attacker.test/x");
    }

    #[tokio::test]
    async fn registry_reads_do_not_follow_redirects() {
        let registry = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/genesis/apps/acme/notes/versions/x/bundle"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", elsewhere.uri())),
            )
            .mount(&registry)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(0)
            .mount(&elsewhere)
            .await;

        let response = registry_for(&registry.uri(), "", TOKEN)
            .expect("registry")
            .get("/api/genesis/apps/acme/notes/versions/x/bundle", "default")
            .expect("request")
            .send()
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 302);
        elsewhere.verify().await;
    }
}
