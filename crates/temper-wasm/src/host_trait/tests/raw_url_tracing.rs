//! Keep named URLs behind `telemetry_url` at every host tracing call.
//!
//! This reads Rust tokens, not data flow: a URL copied into a variable whose
//! name lacks "url" still escapes it. `url_for_log` is the engine's existing
//! pre-redacted value. Neither exemption applies to other values in a call.

use proc_macro2::{Delimiter, TokenStream, TokenTree};

fn tracing_macro(name: &str) -> bool {
    matches!(
        name,
        "event"
            | "span"
            | "error"
            | "warn"
            | "info"
            | "debug"
            | "trace"
            | "error_span"
            | "warn_span"
            | "info_span"
            | "debug_span"
            | "trace_span"
    )
}

fn names_url(name: &str) -> bool {
    name.to_ascii_lowercase().contains("url")
}

fn message_has_url(message: &str) -> bool {
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let capture: String = chars.by_ref().take_while(|c| *c != '}').collect();
        let name = capture.split([':', '!']).next().unwrap_or_default().trim();
        if names_url(name) && name != "url_for_log" {
            return true;
        }
    }
    false
}

// Return (raw URL, redacted URL). Only the argument of telemetry_url is
// exempt: `(telemetry_url(url), url)` still contains a raw URL.
fn urls_in_value(tokens: &[TokenTree]) -> (bool, bool) {
    let (mut raw, mut redacted) = (false, false);
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            TokenTree::Ident(name)
                if name == "telemetry_url"
                    && matches!(tokens.get(index + 1), Some(TokenTree::Group(_))) =>
            {
                redacted = true;
                index += 2;
                continue;
            }
            TokenTree::Ident(name) if name == "url_for_log" => redacted = true,
            TokenTree::Ident(name) => raw |= names_url(&name.to_string()),
            TokenTree::Literal(literal) => {
                if let Ok(message) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    raw |= message_has_url(&message.value());
                }
            }
            TokenTree::Group(group) => {
                let (nested_raw, nested_redacted) =
                    urls_in_value(&group.stream().into_iter().collect::<Vec<_>>());
                raw |= nested_raw;
                redacted |= nested_redacted;
            }
            _ => {}
        }
        index += 1;
    }
    (raw, redacted)
}

fn arguments_have_raw_url(tokens: &[TokenTree]) -> bool {
    tokens
        .split(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
        .any(argument_has_raw_url)
}

fn argument_has_raw_url(tokens: &[TokenTree]) -> bool {
    // Tracing also accepts fields wrapped in braces, before the message.
    if let [TokenTree::Group(group)] = tokens
        && group.delimiter() == Delimiter::Brace
    {
        return arguments_have_raw_url(&group.stream().into_iter().collect::<Vec<_>>());
    }
    if let Some(equals) = tokens
        .iter()
        .position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '='))
    {
        let field: String = tokens[..equals].iter().map(ToString::to_string).collect();
        let (raw, redacted) = urls_in_value(&tokens[equals + 1..]);
        // url.path is already parsed separately from the query by the local host.
        raw || (names_url(&field) && field != "url.path" && !redacted)
    } else {
        urls_in_value(tokens).0
    }
}

fn scan_macros(stream: TokenStream, found: &mut Vec<String>) {
    let tokens: Vec<_> = stream.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        let TokenTree::Group(group) = token else {
            continue;
        };
        let name = match (
            index.checked_sub(2).and_then(|i| tokens.get(i)),
            index.checked_sub(1).and_then(|i| tokens.get(i)),
        ) {
            (Some(TokenTree::Ident(name)), Some(TokenTree::Punct(bang)))
                if bang.as_char() == '!' && tracing_macro(&name.to_string()) =>
            {
                Some(name)
            }
            _ => None,
        };
        if let Some(name) = name {
            let arguments: Vec<_> = group.stream().into_iter().collect();
            if arguments_have_raw_url(&arguments) {
                found.push(format!("{name}!{}", group));
            }
        }
        scan_macros(group.stream(), found);
    }
}

fn raw_urls_in_tracing(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    scan_macros(
        source.parse().expect("valid Rust source tokens"),
        &mut found,
    );
    found
}

fn rust_sources(dir: &std::path::Path, into: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source folder") {
        let path = entry.expect("read source entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            if name != "tests" {
                rust_sources(&path, into);
            }
        } else if name.ends_with(".rs")
            && !name.ends_with("_test.rs")
            && !name.ends_with("_tests.rs")
        {
            into.push(path);
        }
    }
}

#[test]
fn the_check_for_raw_urls_catches_each_way_of_logging_one() {
    for bad in [
        r#"tracing::warn!(url = %url, "failed");"#,
        r#"tracing::warn!(%url, "failed");"#,
        r#"warn!(?request.url, "failed");"#,
        r#"tracing::event!(tracing::Level::WARN, url = %url);"#,
        r#"tracing::warn_span!("x", url = %url);"#,
        r#"tracing::info_span!("x", http.url = %request.url);"#,
        "tracing::warn!(\n target = ?request.url,\n \"failed\"\n);",
        "tracing::warn!(\n %url,\n \"failed\"\n);",
        r#"tracing::error!("call to {url} failed");"#,
        r#"tracing::error!("call to {request_url:?} failed");"#,
        r#"tracing::warn!(url=%destination, "failed");"#,
        r#"tracing::warn!(url = destination, "failed");"#,
        r#"tracing::warn!("call failed: {}", request.url);"#,
        r#"tracing::warn! { %url, "failed" }"#,
        r#"tracing::warn![%url, "failed"];"#,
    ] {
        assert_eq!(raw_urls_in_tracing(bad).len(), 1, "{bad}");
    }
    for name in [
        "event",
        "span",
        "error",
        "warn",
        "info",
        "debug",
        "trace",
        "error_span",
        "warn_span",
        "info_span",
        "debug_span",
        "trace_span",
    ] {
        let prefix = match name {
            "event" => "tracing::Level::WARN, ",
            "span" => "tracing::Level::WARN, \"x\", ",
            name if name.ends_with("_span") => "\"x\", ",
            _ => "",
        };
        let bad = format!("tracing::{name}! ({prefix}url=%url);");
        assert_eq!(raw_urls_in_tracing(&bad).len(), 1, "{bad}");
    }
}

#[test]
fn redaction_is_checked_per_value() {
    for bad in [
        r#"tracing::warn!(raw_url = %url, safe_url = %telemetry_url(url), "failed");"#,
        r#"tracing::warn!(safe_url = %telemetry_url(url), raw_url = %url, "failed");"#,
        "tracing::warn!(safe_url = %telemetry_url(\n &request.url\n),\n raw_url = %url);",
        r#"tracing::warn!(safe_url = %telemetry_url(url), "failed {url}");"#,
        r#"tracing::warn!(safe_url = %url_for_log, %url, "failed");"#,
        r#"tracing::warn!(value = ?(telemetry_url(url), url), "failed");"#,
        r#"tracing::warn!(url = %url_for_logger, "failed");"#,
        r#"tracing::warn!(url.path = %url, "failed");"#,
        r#"tracing::event!(tracing::Level::WARN, { raw_url = %url, safe_url = %telemetry_url(url) });"#,
        r#"tracing::warn!({ raw_url = %url, safe_url = %telemetry_url(url) }, "failed");"#,
        r#"tracing::warn!(%url, "failed"); tracing::info!(url = %telemetry_url(url), "safe");"#,
        r#"tracing::warn!(%url, "failed"); // telemetry_url(url)"#,
    ] {
        assert_eq!(raw_urls_in_tracing(bad).len(), 1, "{bad}");
    }
    for good in [
        r#"tracing::warn!(url = %telemetry_url(url), "failed");"#,
        "tracing::warn!(url = %telemetry_url(\n &request.url\n), \"failed\");",
        r#"tracing::info_span!("x", http.url = %telemetry_url(&request.url));"#,
        r#"tracing::warn!(url = %url_for_log, "failed");"#,
        r#"tracing::warn!(status = status, "failed");"#,
        r#"tracing::event!(tracing::Level::WARN, { url = %telemetry_url(url) });"#,
        r#"tracing::warn!({ url = %telemetry_url(url) }, "failed");"#,
        r#"tracing::info_span!("x", url.path = %path_for_span);"#,
        r#"tracing::warn!("failed {}", telemetry_url(url));"#,
        r#"tracing::warn!("literal {{url}}");"#,
        r#"let url = format!("{url}");"#,
        r#"// tracing::warn!(%url, "failed");"#,
    ] {
        assert!(raw_urls_in_tracing(good).is_empty(), "{good}");
    }
}

#[test]
fn no_host_code_gives_tracing_a_raw_url() {
    let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_sources(&crate_root.join("src"), &mut sources);
    rust_sources(
        &crate_root.join("../temper-server/src/state/dispatch"),
        &mut sources,
    );
    assert!(
        sources.len() > 10,
        "the check must read the host's code, and found {} files",
        sources.len()
    );
    let found: Vec<String> = sources
        .iter()
        .flat_map(|path| {
            let source = std::fs::read_to_string(path).expect("read source");
            raw_urls_in_tracing(&source)
                .into_iter()
                .map(move |call| format!("{}: {call}", path.display()))
        })
        .collect();
    assert_eq!(
        found,
        Vec::<String>::new(),
        "use telemetry_url: a URL can hold a key"
    );
}
