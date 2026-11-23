//! Codex's `exec --json` output: one JSON event per line.
//!
//! Written against Codex CLI 0.156 (`codex-rs/exec/src/exec_events.rs`):
//!
//! - `thread.started {thread_id}` opens the Codex session, which `codex exec
//!   resume <thread_id>` continues. The ID goes back to Codex as an argument,
//!   so one that could read as an option, or holds anything but letters,
//!   digits, `-`, and `_`, is malformed;
//! - `turn.started` begins the turn: the response starts;
//! - `item.completed` with an `agent_message` item carries answer text. Exec
//!   mode reports each message whole when it completes, never token by token,
//!   and a turn may hold several;
//! - other item events report progress: reasoning, tool calls, plans, and
//!   non-fatal warnings;
//! - `turn.completed` and `turn.failed {error: {message}}` end the turn;
//! - top-level `error` events are retry notices such as "Reconnecting... 2/5",
//!   not failures: only `turn.failed` fails a turn.
//!
//! Unknown event and item types are ignored, so newer Codex versions keep
//! working as long as these events keep their meaning.

use serde_json::Value;

use seatline_core::protocol::ErrorCode;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::search::SearchResult;
use seatline_core::turn::Usage;

/// One line of Codex output, reduced to what the adapter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    ThreadStarted(String),
    TurnStarted,
    AgentMessage(String),
    /// A web search started, ran, or finished. Codex `0.171+` may attach
    /// structured search hits on completion; older builds carry none.
    WebSearch(Vec<SearchResult>),
    /// Work in progress, with nothing to show.
    Progress,
    TurnCompleted(Usage),
    /// The turn failed; the message is Codex's own and is never forwarded.
    TurnFailed(String),
    /// Nothing the adapter acts on.
    Ignored,
}

/// A line that is not a Codex event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// Longest thread ID accepted. Codex's are UUIDs.
const MAX_THREAD_ID_LENGTH: usize = 128;

/// Whether `thread_id` is safe to pass back to `codex exec resume`: it can't
/// read as an option, and holds only identifier characters.
pub(super) fn is_thread_id(thread_id: &str) -> bool {
    !thread_id.is_empty()
        && thread_id.len() <= MAX_THREAD_ID_LENGTH
        && !thread_id.starts_with('-')
        && thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn parse(line: &str) -> Result<Line, Malformed> {
    if line.trim().is_empty() {
        return Ok(Line::Ignored);
    }
    let event: Value = serde_json::from_str(line).map_err(|_| Malformed)?;
    let kind = event.get("type").and_then(Value::as_str).ok_or(Malformed)?;
    Ok(match kind {
        "thread.started" => match event.get("thread_id").and_then(Value::as_str) {
            Some(thread_id) if is_thread_id(thread_id) => Line::ThreadStarted(thread_id.to_owned()),
            _ => return Err(Malformed),
        },
        "turn.started" => Line::TurnStarted,
        "turn.completed" => Line::TurnCompleted(Usage {
            input_tokens: event.pointer("/usage/input_tokens").and_then(Value::as_u64),
            output_tokens: event
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64),
            cached_input_tokens: event
                .pointer("/usage/cached_input_tokens")
                .and_then(Value::as_u64),
            cache_write_input_tokens: event
                .pointer("/usage/cache_write_input_tokens")
                .and_then(Value::as_u64),
            reasoning_output_tokens: event
                .pointer("/usage/reasoning_output_tokens")
                .and_then(Value::as_u64),
        }),
        "turn.failed" => Line::TurnFailed(
            event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        "item.started" | "item.updated" | "item.completed" => {
            let item = event
                .get("item")
                .filter(|item| item.is_object())
                .ok_or(Malformed)?;
            let item_type = item.get("type").and_then(Value::as_str).ok_or(Malformed)?;
            match (kind, item_type) {
                ("item.completed", "agent_message") => Line::AgentMessage(
                    item.get("text")
                        .and_then(Value::as_str)
                        .ok_or(Malformed)?
                        .to_owned(),
                ),
                ("item.completed", "web_search") => Line::WebSearch(parse_web_search_results(item)),
                (_, "web_search") => Line::WebSearch(Vec::new()),
                _ => Line::Progress,
            }
        }
        _ => Line::Ignored,
    })
}

/// Upper bound on structured hits parsed from a single `web_search` item.
/// Higher than `MAX_SOURCES_PER_TURN` so later hits cited in the answer can
/// still be enriched with structured metadata before the turn caps emitted
/// sources at `MAX_SOURCES_PER_TURN`.
const MAX_WEB_SEARCH_ITEM_RESULTS: usize = 64;

/// Extracts bounded, SEC-05 validated search results from Codex `0.171+`
/// `WebSearchItem.results` entries (`{url, title, snippet, ...}`).
fn parse_web_search_results(item: &Value) -> Vec<SearchResult> {
    let Some(entries) = item.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let url = entry.get("url").and_then(Value::as_str)?;
            let title = entry.get("title").and_then(Value::as_str).unwrap_or("");
            let snippet = entry.get("snippet").and_then(Value::as_str).unwrap_or("");
            let source_name = entry
                .get("source_name")
                .or_else(|| entry.get("site_name"))
                .and_then(Value::as_str);
            let age = entry
                .get("age")
                .or_else(|| entry.get("published_at"))
                .and_then(Value::as_str);
            SearchResult::new(title, url, snippet, source_name, age)
        })
        .take(MAX_WEB_SEARCH_ITEM_RESULTS)
        .collect()
}

const AUTH_REJECTED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotAuthenticated,
    reason: "AUTH_REJECTED",
    retryable: false,
};

const RATE_LIMITED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_RATE_LIMITED",
    retryable: true,
};

const UNAVAILABLE: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_UNAVAILABLE",
    retryable: true,
};

/// The normalized error for a failed turn. Codex reports only a message, so
/// the reason comes from the status it names; anything else counts as the
/// service being unavailable. The message itself is never forwarded: it can
/// contain URLs and masked keys.
pub fn turn_failure(message: &str) -> ErrorBody {
    let message = message.to_ascii_lowercase();
    let mentions = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
    if mentions(&[
        "401 unauthorized",
        "403 forbidden",
        "invalid_api_key",
        "incorrect api key",
    ]) {
        AUTH_REJECTED
    } else if mentions(&["429", "rate limit", "usage limit", "quota"]) {
        RATE_LIMITED
    } else {
        UNAVAILABLE
    }
}

/// Version-gated Codex CLI isolation capabilities parsed from `codex --version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VersionCapabilities {
    /// `0.145.0`+ (or `0.0.0` local dev): knows `[agents].enabled = false`.
    pub(super) agents_enabled: bool,
    /// `0.122.0`+ (or `0.0.0` local dev): knows `codex exec --ignore-rules`.
    pub(super) ignore_rules: bool,
}

impl VersionCapabilities {
    /// Fallback when `--version` fails, times out, or is unparseable: fail
    /// closed on `agents.enabled=false` while omitting `--ignore-rules` so an
    /// unknown pre-0.122 binary is never broken by an unrecognized CLI flag.
    pub(super) const FAIL_CLOSED_UNKNOWN: Self = Self {
        agents_enabled: true,
        ignore_rules: false,
    };
}

fn at_least_minor(
    major: u64,
    minor: u64,
    patch: u64,
    has_prerelease: bool,
    required_minor: u64,
) -> bool {
    major > 0
        || minor > required_minor
        || (minor == required_minor && (patch > 0 || !has_prerelease))
}

/// Parses `codex --version` output into version-gated isolation capabilities.
/// Returns `None` when the output does not name a parseable version, so
/// callers can fall back to [`VersionCapabilities::FAIL_CLOSED_UNKNOWN`]
/// without caching an unknown result.
///
/// Pre-`0.145` Codex releases lacked `AgentsToml.enabled` and flattened unknown
/// keys under `[agents]` into `BTreeMap<String, AgentRoleToml>`, rejecting
/// `-c agents.enabled=false` with `expected struct AgentRoleToml in agents`.
/// Pre-`0.122` releases lacked `codex exec --ignore-rules`.
pub(super) fn parse_version_capabilities(output: &[u8]) -> Option<VersionCapabilities> {
    let text = std::str::from_utf8(output).ok()?;
    for token in text.split_whitespace() {
        let token = token.strip_prefix('v').unwrap_or(token);
        let (core, has_prerelease) = match token.find(['-', '+']) {
            Some(index) => (&token[..index], token.as_bytes()[index] == b'-'),
            None => (token, false),
        };
        let mut parts = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(major), Ok(minor), Ok(patch)) = (
            major.parse::<u64>(),
            minor.parse::<u64>(),
            patch.parse::<u64>(),
        ) {
            if (major, minor, patch) == (0, 0, 0) {
                return Some(VersionCapabilities {
                    agents_enabled: true,
                    ignore_rules: true,
                });
            }
            return Some(VersionCapabilities {
                agents_enabled: at_least_minor(major, minor, patch, has_prerelease, 145),
                ignore_rules: at_least_minor(major, minor, patch, has_prerelease, 122),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `codex exec --json` output captured from Codex CLI 0.156.1 against a
    /// local stand-in for the Responses API.
    const SUCCESS: &str = include_str!("fixtures/success.jsonl");
    const RESUMED: &str = include_str!("fixtures/resumed.jsonl");
    const FAILED_500: &str = include_str!("fixtures/failed-500.jsonl");
    const FAILED_401: &str = include_str!("fixtures/failed-401.jsonl");
    const FAILED_429: &str = include_str!("fixtures/failed-429.jsonl");
    const RECONNECTING: &str = include_str!("fixtures/reconnecting.jsonl");

    fn parse_all(output: &str) -> Vec<Line> {
        output
            .lines()
            .map(|line| parse(line).expect(line))
            .collect()
    }

    #[test]
    fn a_successful_turn() {
        let lines = parse_all(SUCCESS);
        assert!(matches!(&lines[0], Line::ThreadStarted(id) if id.len() == 36));
        assert_eq!(
            lines[1..],
            [
                // A warning item before the turn: progress, not a failure.
                Line::Progress,
                Line::TurnStarted,
                Line::AgentMessage("Hello from the mock — ünïcödé ✓".to_owned()),
                Line::TurnCompleted(Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(7),
                    cached_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    reasoning_output_tokens: Some(0),
                }),
            ]
        );
    }

    #[test]
    fn web_search_items_extract_structured_results_when_present_and_filter_hostile_urls() {
        let line = r#"{"type":"item.completed","item":{"id":"search_1","type":"web_search","query":"rust","action":{"type":"search","query":"rust"}}}"#;
        assert_eq!(parse(line), Ok(Line::WebSearch(Vec::new())));
        let started = r#"{"type":"item.started","item":{"id":"search_1","type":"web_search","query":"","action":{"type":"other"}}}"#;
        assert_eq!(parse(started), Ok(Line::WebSearch(Vec::new())));

        let structured = r#"{"type":"item.completed","item":{"id":"search_2","type":"web_search","query":"rust","action":{"type":"search","query":"rust"},"results":[{"type":"text_result","url":"https://example.com/rust","title":"Rust <b>Docs</b>","snippet":"Systems language","site_name":"Example","published_at":"2026-11-20"},{"type":"text_result","url":"javascript:alert(1)","title":"Bad"},{"type":"text_result","url":"https://user:pass@example.com/secret","title":"Creds"}]}}"#;
        assert_eq!(
            parse(structured),
            Ok(Line::WebSearch(vec![SearchResult {
                title: "Rust Docs".to_owned(),
                url: "https://example.com/rust".to_owned(),
                snippet: "Systems language".to_owned(),
                source_name: Some("Example".to_owned()),
                age: Some("2026-11-20".to_owned()),
            }]))
        );
    }
    #[test]
    fn a_resumed_turn_reports_the_same_thread() {
        let (first, resumed) = (parse_all(SUCCESS), parse_all(RESUMED));
        assert_eq!(first[0], resumed[0]);
        assert_eq!(
            resumed.last(),
            Some(&Line::TurnCompleted(Usage {
                input_tokens: Some(24),
                output_tokens: Some(14),
                cached_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                reasoning_output_tokens: Some(0),
            }))
        );
    }

    #[test]
    fn each_usage_count_lands_in_its_own_field() {
        // Distinct numbers, which the all-zero fixtures cannot tell apart.
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":800,"cache_write_input_tokens":50,"output_tokens":300,"reasoning_output_tokens":250}}"#;
        assert_eq!(
            parse(line),
            Ok(Line::TurnCompleted(Usage {
                input_tokens: Some(1000),
                output_tokens: Some(300),
                cached_input_tokens: Some(800),
                cache_write_input_tokens: Some(50),
                reasoning_output_tokens: Some(250),
            }))
        );
        // A Codex that reports only the totals leaves the rest unknown, not zero.
        let older = r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":6}}"#;
        assert_eq!(
            parse(older),
            Ok(Line::TurnCompleted(Usage {
                input_tokens: Some(5),
                output_tokens: Some(6),
                ..Usage::default()
            }))
        );
    }

    #[test]
    fn failed_turns_map_to_normalized_errors() {
        for (output, reason) in [
            (FAILED_500, "PROVIDER_UNAVAILABLE"),
            (FAILED_401, "AUTH_REJECTED"),
            (FAILED_429, "PROVIDER_RATE_LIMITED"),
        ] {
            let lines = parse_all(output);
            let Some(Line::TurnFailed(message)) = lines.last() else {
                panic!("expected a failed turn: {lines:?}");
            };
            assert_eq!(turn_failure(message).reason, reason, "{message}");
            // The top-level error before it is only a notice.
            assert_eq!(lines[lines.len() - 2], Line::Ignored);
        }
    }

    #[test]
    fn retry_notices_are_not_failures() {
        let lines = parse_all(RECONNECTING);
        assert!(
            lines
                .iter()
                .all(|line| !matches!(line, Line::TurnFailed(_)))
        );
        assert!(lines.contains(&Line::TurnStarted));
    }

    #[test]
    fn every_item_but_a_completed_agent_message_is_progress() {
        for line in [
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"a\n","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"reasoning","text":"Thinking"}}"#,
            r#"{"type":"item.updated","item":{"id":"item_3","type":"todo_list","items":[]}}"#,
            r#"{"type":"item.started","item":{"id":"item_4","type":"agent_message","text":"partial"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_5","type":"future_item_type"}}"#,
        ] {
            assert_eq!(parse(line), Ok(Line::Progress), "{line}");
        }
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert_eq!(
            parse(r#"{"type":"thread.archived","id":1}"#),
            Ok(Line::Ignored)
        );
        assert_eq!(parse(""), Ok(Line::Ignored));
    }

    #[test]
    fn lines_that_are_not_codex_events_are_malformed() {
        for line in [
            "{not json",
            "[]",
            r#""thread.started""#,
            r#"{"thread_id":"x"}"#,
            r#"{"type":7}"#,
            r#"{"type":"thread.started"}"#,
            r#"{"type":"thread.started","thread_id":""}"#,
            r#"{"type":"thread.started","thread_id":7}"#,
            r#"{"type":"item.completed"}"#,
            r#"{"type":"item.completed","item":"x"}"#,
            r#"{"type":"item.completed","item":{"id":"i"}}"#,
            r#"{"type":"item.completed","item":{"id":"i","type":"agent_message"}}"#,
            r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":7}}"#,
        ] {
            assert_eq!(parse(line), Err(Malformed), "{line}");
        }
    }

    #[test]
    fn thread_ids_that_could_change_the_resume_command_are_malformed() {
        // The thread ID becomes `codex exec resume <thread_id>`: it must not
        // read as an option or carry anything but an identifier.
        for thread_id in [
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            "-",
            "a b",
            "a;b",
            "$(id)",
            "../thread",
            "thread\n",
            "thrëad",
            &"a".repeat(MAX_THREAD_ID_LENGTH + 1),
        ] {
            let line = serde_json::json!({"type": "thread.started", "thread_id": thread_id});
            assert_eq!(parse(&line.to_string()), Err(Malformed), "{thread_id:?}");
        }
        for thread_id in [
            "0199a213-81c0-7800-8aa1-bbab2a035a53",
            "thread_1",
            "a",
            &"a".repeat(MAX_THREAD_ID_LENGTH),
        ] {
            let line = serde_json::json!({"type": "thread.started", "thread_id": thread_id});
            assert_eq!(
                parse(&line.to_string()),
                Ok(Line::ThreadStarted(thread_id.to_owned())),
                "{thread_id:?}"
            );
        }
    }

    #[test]
    fn a_failure_message_is_never_part_of_the_error() {
        let secret = "unexpected status 401 Unauthorized: Incorrect API key provided: sk-abc***xyz";
        let error = turn_failure(secret);
        assert_eq!(error.code, ErrorCode::ProviderNotAuthenticated);
    }

    #[test]
    fn codex_version_gates_agents_enabled_and_ignore_rules() {
        for version in [
            b"codex-cli 0.145.0\n".as_slice(),
            b"codex-cli 0.145.1\n",
            b"codex-cli 0.149.1\n",
            b"codex-cli 0.156.1\n",
            b"codex-cli 0.171.0\n",
            b"codex-cli 0.173.0-alpha.4\n",
            b"codex 1.0.0\n",
            b"codex-cli v0.171.0+build.1\n",
            b"codex-cli 0.0.0\n",
        ] {
            assert_eq!(
                parse_version_capabilities(version),
                Some(VersionCapabilities {
                    agents_enabled: true,
                    ignore_rules: true,
                }),
                "{}",
                String::from_utf8_lossy(version)
            );
        }
        for version in [
            b"codex-cli 0.122.0\n".as_slice(),
            b"codex-cli 0.135.0\n",
            b"codex-cli 0.142.0\n",
            b"codex-cli 0.144.6\n",
            b"codex-cli 0.145.0-alpha.1\n",
        ] {
            assert_eq!(
                parse_version_capabilities(version),
                Some(VersionCapabilities {
                    agents_enabled: false,
                    ignore_rules: true,
                }),
                "{}",
                String::from_utf8_lossy(version)
            );
        }
        for version in [
            b"codex-cli 0.121.0\n".as_slice(),
            b"codex-cli 0.122.0-alpha.1\n",
        ] {
            assert_eq!(
                parse_version_capabilities(version),
                Some(VersionCapabilities {
                    agents_enabled: false,
                    ignore_rules: false,
                }),
                "{}",
                String::from_utf8_lossy(version)
            );
        }
        for unparseable in [b"".as_slice(), b"codex-cli\n", b"not-a-version\n", b"\xff"] {
            assert_eq!(parse_version_capabilities(unparseable), None);
        }
    }
}
