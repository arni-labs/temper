//! Credentials kept out of a module's error text.
//!
//! A module's error (an ordinary failure, a `retry` reason, a trap) is
//! logged, recorded on its span and in invocation history, and sent to the
//! trigger's `on_failure`. Modules build that text from what they called:
//! the SDK's own HTTP errors name the URL (`HTTP call failed: PUT <url>`),
//! and a presigned object-store URL carries its signature in the query, as
//! a URL can carry a user and password before its host. So every URL in the
//! text keeps its scheme, host and path and loses its user information,
//! query and fragment, once, where the text enters the kernel.

/// What stands in for a URL's query or fragment.
const REDACTED: &str = "<redacted>";

/// `text` with every URL in it stripped of its user information, query and
/// fragment. Text without a URL comes back as it was.
pub fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let scheme_start = rest[..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .last()
            .map_or(at, |(index, _)| index);
        if scheme_start == at {
            // "://" with no scheme before it is not a URL.
            out.push_str(&rest[..at + 3]);
            rest = &rest[at + 3..];
            continue;
        }
        out.push_str(&rest[..scheme_start]);
        let url_end = rest[at + 3..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .map_or(rest.len(), |end| at + 3 + end);
        out.push_str(&redact_one(&rest[scheme_start..url_end], at - scheme_start));
        rest = &rest[url_end..];
    }
    out.push_str(rest);
    out
}

/// One URL, whose scheme ends at `scheme_len`.
fn redact_one(url: &str, scheme_len: usize) -> String {
    let after_scheme = scheme_len + 3;
    let (body, cut) = match url[after_scheme..].find(['?', '#']) {
        Some(at) => (&url[after_scheme..after_scheme + at], true),
        None => (&url[after_scheme..], false),
    };
    let (authority, path) = body.split_at(body.find('/').unwrap_or(body.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut redacted = format!("{}{host}{path}", &url[..after_scheme]);
    if authority.contains('@') {
        redacted.insert_str(after_scheme, &format!("{REDACTED}@"));
    }
    if cut {
        redacted.push('?');
        redacted.push_str(REDACTED);
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::redact_url_credentials;

    #[test]
    fn a_presigned_query_is_cut() {
        assert_eq!(
            redact_url_credentials(
                "HTTP call failed with code -3: PUT https://acct.r2.cloudflarestorage.com/b/k.json?X-Amz-Signature=abc&X-Amz-Credential=AKIA"
            ),
            "HTTP call failed with code -3: PUT https://acct.r2.cloudflarestorage.com/b/k.json?<redacted>"
        );
    }

    #[test]
    fn user_information_is_cut() {
        assert_eq!(
            redact_url_credentials("connect postgres://admin:hunter2@db.internal:5432/app failed"),
            "connect postgres://<redacted>@db.internal:5432/app failed"
        );
        assert_eq!(
            redact_url_credentials("GET https://tok@api.example.com/v1#frag"),
            "GET https://<redacted>@api.example.com/v1?<redacted>"
        );
    }

    #[test]
    fn every_url_in_the_text_is_redacted_and_the_rest_kept() {
        assert_eq!(
            redact_url_credentials(
                "first \"https://a.example/x?k=1\" then https://b.example/y?token=2, done"
            ),
            "first \"https://a.example/x?<redacted>\" then https://b.example/y?<redacted> done"
        );
    }

    #[test]
    fn text_without_credentials_is_unchanged() {
        for text in [
            "r2 put 503",
            "GET https://api.example.com/v1/items failed",
            "a :// on its own",
            "",
        ] {
            assert_eq!(redact_url_credentials(text), text);
        }
    }
}
