//! What a span keeps of an outside call a module makes: its URL with no
//! credentials or keys, and its HTTP status.

/// What a span or a log keeps of a URL a module called: its host and path, with no credentials, query or fragment, and
/// with any key-shaped run in the path taken out. A path can hold a key: some services are reached at
/// `https://host/<64 hex>/build`, and a span must never carry that key.
pub(crate) fn telemetry_url(url: &str) -> String {
    let after_scheme = url.find("://").map(|idx| &url[idx + 3..]).unwrap_or(url);
    // Credentials only belong to the authority; an @ in the path, query or
    // fragment must not move that component into the host we keep.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (authority, path_and_query) = after_scheme.split_at(authority_end);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let path_end = path_and_query
        .find(['?', '#'])
        .unwrap_or(path_and_query.len());
    let path = &path_and_query[..path_end];
    if path.is_empty() {
        authority.to_string()
    } else {
        format!("{authority}{}", without_keys(path))
    }
}

/// A key-shaped run of a path: unreserved characters, at least 20 long with a digit in it, or at least 32 long whatever
/// they are. 20 is below the shortest secret a URL path carries in the services modules call (a Slack webhook's 24). A
/// hyphenated UUID stays: entity and trace ids are what a reader of a span joins on, and an id is no credential. Only
/// hyphenated: 32 bare hex also parses as a UUID, and is what many API keys look like.
fn key_shaped(run: &str) -> bool {
    let long = run.len() >= 32 || (run.len() >= 20 && run.bytes().any(|b| b.is_ascii_digit()));
    let hyphenated_uuid = run.len() == 36 && uuid::Uuid::try_parse(run).is_ok();
    long && !hyphenated_uuid
}

fn without_keys(path: &str) -> String {
    let is_run = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~');
    let mut kept = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find(is_run) {
        let (before, from) = rest.split_at(start);
        kept.push_str(before);
        let (run, after) = from.split_at(from.find(|c| !is_run(c)).unwrap_or(from.len()));
        // Dots in ('...') separate entity-ID components that readers join spans
        // on. Keep those IDs only when no individual component is key-shaped.
        let readable_entity_id = before.ends_with("('")
            && after.starts_with("')")
            && run.split('.').all(|part| !key_shaped(part));
        kept.push_str(if key_shaped(run) && !readable_entity_id {
            "<redacted>"
        } else {
            run
        });
        rest = after;
    }
    kept.push_str(rest);
    kept
}

/// Record an outside call's HTTP status on its span: as `status_code`, and as
/// `http.status_code`, which the trace view reads. A 5xx is the service's
/// failure, so the span is marked an error, though the call returns to the
/// module, which decides what to answer.
pub(super) fn record_http_status(status: impl Into<u64>) {
    let status: u64 = status.into();
    let span = tracing::Span::current();
    span.record("status_code", status);
    span.record("http.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
}
