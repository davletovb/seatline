//! Provider-native web-search source normalization (SRCH-01..04).
//!
//! The runtime runs no search service. The authenticated AI provider a turn
//! runs on performs the search, and its provider-specific result format is
//! normalized here before an application sees any source.

use std::collections::HashSet;

use crate::protocol::{ErrorCode, Failure, Source};
use serde::Deserialize;

pub const MAX_SOURCES_PER_TURN: usize = 20;

pub const NATIVE_SEARCH_NO_SOURCES: Failure = Failure {
    code: ErrorCode::SearchFailed,
    reason: "NATIVE_SEARCH_NO_SOURCES",
    retryable: true,
};

const MAX_TITLE_BYTES: usize = 512;
const MAX_URL_BYTES: usize = 4096;
const MAX_SNIPPET_BYTES: usize = 4096;
const MAX_META_BYTES: usize = 256;

/// One provider-native search result before it receives its application's source identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source_name: Option<String>,
    pub age: Option<String>,
}

impl SearchResult {
    /// A result with a usable HTTP(S) URL. Every text field is untrusted
    /// provider output (SEC-05): it becomes bounded single-line plain text
    /// here, before any browser surface or log sees it. A title that is
    /// nothing but markup falls back to the URL's host.
    pub fn new(
        title: &str,
        url: &str,
        snippet: &str,
        source_name: Option<&str>,
        age: Option<&str>,
    ) -> Option<Self> {
        if !valid_source_url(url) {
            return None;
        }
        let title = match display_text(title, MAX_TITLE_BYTES) {
            title if title.is_empty() => bounded(url_host(url), MAX_TITLE_BYTES),
            title => title,
        };
        Some(Self {
            title,
            url: url.to_owned(),
            snippet: display_text(snippet, MAX_SNIPPET_BYTES),
            source_name: source_name
                .map(|value| display_text(value, MAX_META_BYTES))
                .filter(|value| !value.is_empty()),
            age: age
                .map(|value| display_text(value, MAX_META_BYTES))
                .filter(|value| !value.is_empty()),
        })
    }
}

/// Per-turn source identity, deduplication and storage bound shared by every
/// provider adapter.
pub struct SourceCollector {
    backend_id: &'static str,
    seen: HashSet<String>,
    count: usize,
}

impl SourceCollector {
    pub fn new(backend_id: &'static str) -> Self {
        Self {
            backend_id,
            seen: HashSet::new(),
            count: 0,
        }
    }

    pub fn push(&mut self, result: SearchResult) -> Option<Source> {
        if self.count >= MAX_SOURCES_PER_TURN || !self.seen.insert(result.url.clone()) {
            return None;
        }
        self.count += 1;
        Some(Source {
            id: format!("src_{}_{}", self.backend_id, self.count),
            backend_id: self.backend_id.to_owned(),
            title: result.title,
            url: result.url,
            snippet: result.snippet,
            source_name: result.source_name,
            age: result.age,
        })
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn reset(&mut self) {
        self.seen.clear();
        self.count = 0;
    }
}

/// Claude Code 2.1.x returns WebSearch results as a text `tool_result` with
/// `Links: [{...}]`, followed by prose about the results and a reminder to
/// cite them. Parse only that first JSON array; the prose around it stays
/// provider output and is never treated as data.
pub fn claude_tool_result_sources(content: &str) -> Vec<SearchResult> {
    #[derive(Deserialize)]
    struct Link {
        title: String,
        url: String,
        #[serde(default)]
        snippet: String,
    }

    // The first `Links:` followed by a JSON array: the prose after it may
    // mention links too.
    let Some(links) = content.match_indices("Links:").find_map(|(marker, _)| {
        let json = content[marker + "Links:".len()..].trim_start();
        serde_json::Deserializer::from_str(json)
            .into_iter::<Vec<Link>>()
            .next()
            .and_then(Result::ok)
    }) else {
        return Vec::new();
    };
    links
        .into_iter()
        .filter_map(|link| SearchResult::new(&link.title, &link.url, &link.snippet, None, None))
        .take(MAX_SOURCES_PER_TURN)
        .collect()
}

/// Codex exec currently reports only the search query in its `web_search`
/// item. Grounding URLs live in the answer text, so search turns normalize
/// Markdown links and bare HTTP(S) URLs from completed agent messages.
pub fn codex_message_sources(text: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() && results.len() < MAX_SOURCES_PER_TURN {
        let Some(open_rel) = text[offset..].find('[') else {
            break;
        };
        let open = offset + open_rel;
        let Some(close_rel) = text[open + 1..].find(']') else {
            break;
        };
        let close = open + 1 + close_rel;
        if text.as_bytes().get(close + 1) != Some(&b'(') {
            offset = close + 1;
            continue;
        }
        let Some(end_rel) = text[close + 2..].find(')') else {
            break;
        };
        let end = close + 2 + end_rel;
        let title = &text[open + 1..close];
        let url = text[close + 2..end].trim().trim_matches(['<', '>']);
        if seen.insert(url.to_owned()) {
            if let Some(result) = SearchResult::new(title, url, "", None, None) {
                results.push(result);
            }
        }
        offset = end + 1;
    }

    // Some Codex answers cite a URL without Markdown. Keep those too.
    for token in text.split_whitespace() {
        if results.len() >= MAX_SOURCES_PER_TURN {
            break;
        }
        let url = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | ',' | ';' | '.'
            )
        });
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            continue;
        }
        if seen.insert(url.to_owned()) {
            if let Some(result) = SearchResult::new(url, url, "", None, None) {
                results.push(result);
            }
        }
    }
    results
}

/// Whether `url` is a source link every browser surface accepts (SEC-05).
///
/// This is a strict subset of what the WHATWG URL parser accepts: the
/// extension parses each source again with the browser's `new URL`, and a
/// source counted here toward grounding (`NATIVE_SEARCH_NO_SOURCES`) must
/// never be one the extension then drops. So the host is an ASCII domain name
/// without punycode labels, or a dotted-quad IPv4 address; the port, if any,
/// is a valid one; there are no credentials; and the URL stays within
/// MAX_URL_BYTES after the browser percent-encodes it.
/// `docs/protocol/fixtures/v1-source-urls.json` pins both sides.
pub fn valid_source_url(url: &str) -> bool {
    if url.len() > MAX_URL_BYTES
        || url.chars().any(|character| {
            character.is_control() || character.is_whitespace() || is_invisible(character)
        })
    {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let (host, port) = split_authority(rest);
    valid_host(host) && port.is_none_or(valid_port) && serialized_len(url) <= MAX_URL_BYTES
}

/// The host and port of what follows `scheme://`. A backslash ends the
/// authority, as it does in the browser for `http` and `https`.
fn split_authority(rest: &str) -> (&str, Option<&str>) {
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let authority = &rest[..end];
    match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    }
}

fn valid_host(host: &str) -> bool {
    // One trailing dot is allowed, as in `example.com.`.
    let name = host.strip_suffix('.').unwrap_or(host);
    let labels: Vec<&str> = name.split('.').collect();
    let well_formed = labels.iter().all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            && !label
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xn--"))
    });
    if !well_formed {
        return false;
    }
    // A host whose last label is a number is an IPv4 address to the browser,
    // which refuses one that doesn't parse. Only the dotted-quad form passes.
    let last = labels[labels.len() - 1];
    let numeric = last.bytes().all(|byte| byte.is_ascii_digit())
        || (last
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
            && last[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    !numeric
        || (name.len() == host.len()
            && labels.len() == 4
            && labels.iter().all(|label| {
                (label.len() == 1 || !label.starts_with('0'))
                    && label.bytes().all(|byte| byte.is_ascii_digit())
                    && label.parse::<u8>().is_ok()
            }))
}

fn valid_port(port: &str) -> bool {
    port.is_empty()
        || (port.len() <= 5
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok())
}

/// At most how long the browser's serialization of `url` is: it adds a `/`
/// to an empty path and percent-encodes every non-ASCII byte and a few ASCII
/// characters, each as three characters.
fn serialized_len(url: &str) -> usize {
    1 + url
        .bytes()
        .map(|byte| {
            if !byte.is_ascii() || b"\"<>`{}'^".contains(&byte) {
                3
            } else {
                1
            }
        })
        .sum::<usize>()
}

/// The host of a URL `valid_source_url` accepted.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    split_authority(rest).0
}

/// Untrusted text as bounded, single-line plain text: markup tags are
/// removed, a few common HTML entities decoded, and control, bidirectional
/// override and invisible characters dropped, so nothing can reorder or hide
/// what's shown. Whitespace collapses to single spaces. The result is at
/// most `limit` bytes and ends on a character boundary.
pub fn display_text(value: &str, limit: usize) -> String {
    // Enough input to fill `limit` after stripping, without scanning an
    // unbounded provider string.
    let scan = &value[..floor_boundary(value, limit.saturating_mul(8).max(1024))];
    let decoded = decode_entities(&strip_tags(scan));
    let mut out = String::with_capacity(decoded.len().min(limit));
    let mut pending_space = false;
    for character in decoded.chars() {
        if is_invisible(character) {
            continue;
        }
        if character.is_whitespace() || character.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        let needed = character.len_utf8() + usize::from(pending_space);
        if out.len() + needed > limit {
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(character);
    }
    out
}

/// Characters that change how surrounding text is displayed without being
/// visible themselves: bidirectional controls and zero-width spaces.
/// Joiners stay, since emoji sequences need them.
fn is_invisible(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Removes `<tag ...>`, `</tag>` and `<!...>` runs. A `<` that doesn't open a
/// tag (as in `a < b`) stays text.
fn strip_tags(value: &str) -> String {
    const MAX_TAG_BYTES: usize = 1024;
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let opens_tag = after
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_alphabetic() || next == '/' || next == '!');
        let close = after
            .char_indices()
            .take_while(|&(index, character)| index <= MAX_TAG_BYTES && character != '<')
            .find(|&(_, character)| character == '>')
            .map(|(index, _)| index);
        match close {
            Some(close) if opens_tag => {
                // A tag separates words the way a line break would.
                out.push(' ');
                rest = &after[close + 1..];
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entities(value: &str) -> String {
    const ENTITIES: [(&str, &str); 7] = [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ];
    if !value.contains('&') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    'text: while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        for (entity, text) in ENTITIES {
            if let Some(after) = rest.strip_prefix(entity) {
                out.push_str(text);
                rest = after;
                continue 'text;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn floor_boundary(value: &str, limit: usize) -> usize {
    if value.len() <= limit {
        return value.len();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn bounded(value: &str, limit: usize) -> String {
    value[..floor_boundary(value, limit)].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_urls_require_http_authority_and_no_credentials() {
        for valid in ["https://example.com/", "http://example.test/path?q=1#x"] {
            assert!(valid_source_url(valid), "{valid}");
        }
        for invalid in [
            "https://",
            "file:///tmp/x",
            "javascript:alert(1)",
            "https://user@example.com/",
            "https://example.com/has space",
        ] {
            assert!(!valid_source_url(invalid), "{invalid}");
        }
    }

    #[test]
    fn result_text_is_bounded_single_line_plain_text() {
        let result = SearchResult::new(
            "  <script>alert(1)</script>Hostile\n<b>result</b>\u{202E}gnp.exe\u{0007} &amp; more  ",
            "https://example.com/hostile",
            &format!("<p>Line one</p>\r\n\tline&nbsp;two {}", "x".repeat(20_000)),
            Some("\u{200B}Example <i>News</i>"),
            Some("2 days\nago"),
        )
        .unwrap();
        assert_eq!(result.title, "alert(1) Hostile result gnp.exe & more");
        assert!(result.snippet.starts_with("Line one line two xxx"));
        assert!(result.snippet.len() <= MAX_SNIPPET_BYTES);
        assert_eq!(result.source_name.as_deref(), Some("Example News"));
        assert_eq!(result.age.as_deref(), Some("2 days ago"));
        for text in [&result.title, &result.snippet] {
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() || is_invisible(c) || c == '<'),
                "{text:?}"
            );
        }
    }

    #[test]
    fn display_text_keeps_ordinary_text_and_character_boundaries() {
        assert_eq!(display_text("a < b and c > d", 64), "a < b and c > d");
        assert_eq!(display_text("&lt;b&gt; is literal", 64), "<b> is literal");
        assert_eq!(
            display_text("family \u{1F468}\u{200D}\u{1F469}", 64),
            "family \u{1F468}\u{200D}\u{1F469}"
        );
        assert_eq!(
            display_text("\u{00e9}\u{00e9}\u{00e9}", 5),
            "\u{00e9}\u{00e9}"
        );
        assert_eq!(display_text("<br><br>", 64), "");
        assert_eq!(display_text("unterminated <tag", 64), "unterminated <tag");
    }

    #[test]
    fn a_markup_only_title_falls_back_to_the_host() {
        let result = SearchResult::new(
            "<img src=x>",
            "https://news.example.org/a?b",
            "",
            None,
            None,
        )
        .unwrap();
        assert_eq!(result.title, "news.example.org");
    }

    #[test]
    fn a_url_the_browser_would_encode_past_the_limit_is_refused() {
        // 2 bytes each, 6 once the browser percent-encodes them.
        let long = format!("https://example.com/{}", "\u{00fc}".repeat(700));
        assert!(long.len() <= MAX_URL_BYTES);
        assert!(!valid_source_url(&long));
        let fits = format!("https://example.com/{}", "\u{00fc}".repeat(600));
        assert!(valid_source_url(&fits));
    }

    #[test]
    fn a_search_with_only_malformed_urls_has_no_sources() {
        let results = claude_tool_result_sources(
            "Links: [{\"title\":\"A\",\"url\":\"https://:443/path\"},{\"title\":\"B\",\"url\":\"https://example.com:99999/\"},{\"title\":\"C\",\"url\":\"https://999.1.1.1/\"}]",
        );
        assert!(results.is_empty());
    }

    #[test]
    fn claude_links_are_read_before_the_prose_that_follows_them() {
        // The shape Claude Code 2.1.236 prints: prose and a reminder after
        // the array, which may mention links again.
        let results = claude_tool_result_sources(
            "Web search results for query: \"rust release\"\n\nLinks: [{\"title\":\"Rust\",\"url\":\"https://www.rust-lang.org/\"},{\"title\":\"Blog\",\"url\":\"https://blog.rust-lang.org/\"}]\n\nRust 1.90 was released with these Links: see above.\n\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.",
        );
        let urls: Vec<_> = results.iter().map(|result| result.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://www.rust-lang.org/", "https://blog.rust-lang.org/"]
        );
    }

    #[test]
    fn claude_links_payload_is_normalized() {
        let results = claude_tool_result_sources(
            "Web search results for query: \"rust\"\n\nLinks: [{\"title\":\"Rust\",\"url\":\"https://www.rust-lang.org/\"},{\"title\":\"Bad\",\"url\":\"https://\"}]",
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn codex_extracts_markdown_and_bare_urls() {
        let results = codex_message_sources(
            "See [Rust blog](https://blog.rust-lang.org/2026/09/01/release.html) and https://example.com/more.",
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust blog");
        assert_eq!(results[1].url, "https://example.com/more");
    }

    #[test]
    fn collector_deduplicates_and_caps_sources() {
        let mut collector = SourceCollector::new("test");
        for index in 0..(MAX_SOURCES_PER_TURN + 5) {
            let result = SearchResult::new(
                &format!("Result {index}"),
                &format!("https://example.com/{index}"),
                "",
                None,
                None,
            )
            .unwrap();
            let _ = collector.push(result);
        }
        assert_eq!(collector.count(), MAX_SOURCES_PER_TURN);
        assert!(
            collector
                .push(SearchResult::new("Again", "https://example.com/1", "", None, None).unwrap())
                .is_none()
        );
        collector.reset();
        assert_eq!(collector.count(), 0);
    }
}
