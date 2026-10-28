//! Observability contract tests for WASM dispatch attribution.

/// Blank out `//` line comments so a commented-out call cannot satisfy a source
/// contract. Byte offsets are preserved (comment bodies become spaces) so every
/// ordering assertion still compares positions in the original file. String
/// literals are tracked, so a `"http://..."` in code is not mistaken for a
/// comment and does not blank the rest of its line.
///
/// This is a heuristic, not a lexer: char literals (`'/'`) and raw strings
/// (`r#"..."#`) are not modelled. Both scanned files are free of them today, and
/// every assertion built on this asserts *presence*, so a mis-scan produces a
/// loud failure rather than a silent pass.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut escaped = false;
        let mut comment_at = None;
        for (i, &b) in bytes.iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            match b {
                b'\\' if in_string => escaped = true,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => {
                    comment_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        match comment_at {
            Some(at) => {
                out.push_str(&line[..at]);
                for ch in line[at..].chars() {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[test]
fn comment_stripper_ignores_slashes_inside_string_literals() {
    let src = "let url = \"http://example.com/a\"; // real comment\nlet keep = 1;\n";
    let stripped = strip_line_comments(src);
    assert_eq!(stripped.len(), src.len(), "offsets must be preserved");
    assert!(
        stripped.contains("http://example.com/a"),
        "a URL in a string literal must survive: {stripped:?}"
    );
    assert!(
        !stripped.contains("real comment"),
        "the comment must be blanked"
    );
    assert!(stripped.contains("let keep = 1;"));
}
