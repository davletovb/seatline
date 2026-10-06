//! Claude Code CLI adapter (PRO-05/06).
//!
//! The adapter discovers the fixed `claude` executable through the shared
//! provider search path, checks `claude auth status`, and drives print mode
//! through stream-json on stdin/stdout. It runs one turn per process, and
//! knows no conversations: a persistent turn reports the native session it
//! runs in as an opaque handle, and resumes one it is given.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{Cleanup, Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::{CachedSearchPath, SearchPath};
use seatline_core::exchange::SessionLoss;
use seatline_core::process::{Event, Exit, Process, ProcessSpec};
use seatline_core::prompt;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, Capability, ErrorCode, ModelOption, ProviderState,
};
use seatline_core::search::{
    NATIVE_SEARCH_NO_SOURCES, SourceCollector, claude_tool_result_sources,
};
use seatline_core::stream::{BUSY_LIMIT, LineStream, Output};
use seatline_core::telemetry::Span;
use seatline_core::turn::{SessionPolicy, ToolPolicy, Turn as TurnRequest};
use seatline_platform::discovery;
use seatline_platform::environment;
use seatline_platform::forget;
use seatline_platform::layout::Layout;
use seatline_platform::workspace;

pub mod output;
mod readiness;

use output::Line;

pub const ID: &str = "claude";
const EXECUTABLE: &str = "claude";

/// Non-secret Claude/Node configuration needed to reproduce a working CLI
/// launch from Chrome's much smaller environment. Proxies are already in
/// [`environment::INHERITED`]; Node reads extra CA certificates only from
/// `NODE_EXTRA_CA_CERTS`.
pub const CLAUDE_VARIABLES: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_GIT_BASH_PATH",
    "NODE_EXTRA_CA_CERTS",
];

/// How much of Claude's stderr a turn keeps, to tell a missing session apart
/// from any other early exit. Never logged or forwarded.
const STDERR_TAIL_BYTES: usize = 4096;

pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeouts: Timeouts,
    pub probe: Duration,
    /// How long Claude may linger after its terminal `result` event.
    pub finish: Duration,
}

pub const LIMITS: Limits = Limits {
    timeouts: Timeouts {
        start: Duration::from_secs(60),
        idle: Duration::from_secs(300),
        max_turn: Duration::MAX,
        stop_grace: Duration::from_secs(2),
    },
    probe: Duration::from_secs(10),
    finish: Duration::from_secs(5),
};

/// Page context reaches Claude as untrusted reference data in the prompt
/// (`provider_prompt`). A context turn is a plain turn: Claude gets no tools
/// at all (`--tools ""`), MCP stays blocked, and the host refuses context with
/// search, so page text can't make Claude act, only inform its answer.
pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: Capability::Supported,
    continuation: Capability::Supported,
    web_search: Capability::Supported,
    model_selection: Capability::Supported,
    reasoning_effort: Capability::Unsupported,
    service_tier: Capability::Unsupported,
    cancellation: Capability::Supported,
    tool_isolation: Capability::Supported,
};

/// Suggested models: Claude Code's documented aliases, which always resolve to
/// the latest model of each family, so the list doesn't go stale. Any other
/// valid model ID (a full model name) is passed on as well.
pub const MODELS: &[ModelOption] = &[
    ModelOption {
        id: Cow::Borrowed("sonnet"),
        label: Cow::Borrowed("Sonnet (latest)"),
    },
    ModelOption {
        id: Cow::Borrowed("opus"),
        label: Cow::Borrowed("Opus (latest)"),
    },
    ModelOption {
        id: Cow::Borrowed("haiku"),
        label: Cow::Borrowed("Haiku (latest)"),
    },
];

const NOT_INSTALLED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotFound,
    reason: "EXECUTABLE_NOT_FOUND",
    retryable: false,
};

const NOT_SIGNED_IN: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotAuthenticated,
    reason: "LOGIN_REQUIRED",
    retryable: false,
};

/// How much of a search turn's message is held back before it counts as the
/// answer rather than narration before a search. Narration is a sentence or
/// two; past this, the answer streams live.
const HELD_TEXT_LIMIT: usize = 600;

const START_FAILED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_UNAVAILABLE",
    retryable: false,
};

const NO_WORKSPACE: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "WORKSPACE_UNAVAILABLE",
    retryable: false,
};

const PROCESS_EXITED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROCESS_EXITED",
    retryable: true,
};

const MALFORMED_OUTPUT: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "MALFORMED_PROVIDER_OUTPUT",
    retryable: false,
};

/// The session a turn was asked to resume doesn't exist.
const UNKNOWN_SESSION: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "UNKNOWN_SESSION",
    retryable: false,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    work_dir: PathBuf,
    inherited: Vec<(OsString, OsString)>,
    path: Option<OsString>,
}

impl Launch {
    fn new(work_dir: PathBuf, host: Vec<(OsString, OsString)>) -> Self {
        let path = environment::lookup(&host, "PATH").map(OsStr::to_os_string);
        Self {
            work_dir,
            inherited: environment::inherit(host, CLAUDE_VARIABLES),
            path,
        }
    }

    fn workspace(&self) -> std::io::Result<PathBuf> {
        workspace::prepare(&self.work_dir)
    }

    fn command<I>(&self, workspace: &Path, executable: &Path, args: I) -> ProcessSpec
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        ProcessSpec::new(executable)
            .args(args)
            .envs(self.inherited.iter().cloned())
            .env("DISABLE_AUTOUPDATER", "1")
            .env(
                "PATH",
                environment::search_path_for(executable, self.path.as_deref()),
            )
            .current_dir(workspace)
    }
}

pub struct Claude {
    search: CachedSearchPath,
    launch: Rc<Launch>,
    limits: Limits,
    account_file: readiness::AccountFile,
}

impl Claude {
    pub fn installed(layout: &Layout) -> Self {
        let host: Vec<_> = std::env::vars_os().collect();
        Self::new(
            discovery::installed(layout),
            layout.workspace(&host, "claude"),
        )
    }

    pub fn new(search: SearchPath, work_dir: PathBuf) -> Self {
        Self {
            search: CachedSearchPath::new(search),
            launch: Rc::new(Launch::new(work_dir, std::env::vars_os().collect())),
            limits: LIMITS,
            account_file: readiness::AccountFile::default(),
        }
    }

    #[must_use]
    pub fn with_environment<I>(mut self, host: I) -> Self
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        self.launch = Rc::new(Launch::new(
            self.launch.work_dir.clone(),
            host.into_iter().collect(),
        ));
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    fn executable(&self) -> Option<PathBuf> {
        self.search.find(EXECUTABLE)
    }
}

impl Provider for Claude {
    fn readiness_key(&self) -> Option<crate::readiness::Key> {
        self.launch.workspace().ok()?;
        let files: Vec<_> = claude_config_dir(&self.launch)
            .into_iter()
            .flat_map(|dir| {
                [
                    dir.join(".credentials.json"),
                    dir.join("settings.json"),
                    dir.join("settings.local.json"),
                    dir,
                ]
            })
            .collect();
        let key = crate::readiness::Key::watch(&self.executable()?, files, self.capabilities())?;
        let account_path = environment::lookup(&self.launch.inherited, "CLAUDE_CONFIG_DIR")
            .map(|dir| PathBuf::from(dir).join(".claude.json"))
            .or_else(|| {
                environment::home_dir(&self.launch.inherited).map(|home| home.join(".claude.json"))
            });
        if let Some(path) = account_path {
            self.account_file.watch(key, path)
        } else {
            Some(key)
        }
    }
    fn id(&self) -> &str {
        ID
    }

    fn timeouts(&self) -> Timeouts {
        self.limits.timeouts
    }

    fn supports_persistent_session(&self) -> bool {
        true
    }

    fn supports_preparation(&self) -> bool {
        true
    }

    fn invalidate_readiness(&self) {
        self.search.invalidate();
        self.account_file.invalidate();
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn status(&self) -> Box<dyn Exchange> {
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::new([
                status_update(Availability::NotFound, Authentication::Unknown),
                Update::Completed,
            ]));
        };
        Box::new(match probe(&self.launch, &executable) {
            Ok(process) => StatusCheck::Probing {
                process,
                give_up: after(self.limits.probe),
                output: Vec::new(),
            },
            Err(_) => StatusCheck::Done(VecDeque::from([
                status_update(Availability::Unavailable, Authentication::Unknown),
                Update::Completed,
            ])),
        })
    }

    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        let config = claude_config_dir(&self.launch);
        let workspace = self.launch.work_dir.clone();
        let sessions = sessions.to_vec();
        Cleanup::new(
            move || {
                let Some(config) = config else {
                    return Ok(());
                };
                for session in &sessions {
                    forget_transcript(&config, &workspace, session)?;
                }
                Ok(())
            },
            || {},
        )
    }

    fn send(&self, request: TurnRequest) -> Box<dyn Exchange> {
        if request.reasoning_effort.is_some() {
            return Box::new(Scripted::failed(crate::REASONING_EFFORT_UNSUPPORTED));
        }
        if request.service_tier.is_some() {
            return Box::new(Scripted::failed(crate::SERVICE_TIER_UNSUPPORTED));
        }
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::failed(NOT_INSTALLED));
        };
        if request.validate().is_err() {
            return Box::new(Scripted::failed(crate::INVALID_TURN));
        }

        let mut turn = Turn {
            stage: Stage::Done,
            executable,
            launch: Rc::clone(&self.launch),
            prompt: prompt::render(request.system.as_deref(), &request.messages, request.tools),
            resume: request.continuation,
            session_policy: request.session,
            reported_session: None,
            finish_grace: self.limits.finish,
            model: request.model,
            native_search: request.tools == ToolPolicy::NativeWebSearch,
            queue: VecDeque::new(),
            sources: SourceCollector::new(ID),
            web_search_uses: HashSet::new(),
            cancelled: false,
            started: false,
            saw_delta: false,
            messages: 0,
            break_before_text: false,
            held: String::new(),
            live: false,
            outcome: None,
            finish_by: None,
            probe_span: None,
        };
        let probe_began = request.check_sign_in.then(Instant::now);
        let probe = request
            .check_sign_in
            .then(|| probe(&turn.launch, &turn.executable));
        match probe {
            Some(Ok(process)) => {
                turn.probe_span = probe_began.map(Span::begin);
                turn.stage = Stage::Probing {
                    process,
                    give_up: after(self.limits.probe),
                };
            }
            Some(Err(_)) | None => turn.start(),
        }
        Box::new(turn)
    }
}

fn status_update(availability: Availability, authentication: Authentication) -> Update {
    Update::Status {
        provider_id: ID.to_owned(),
        status: ProviderState {
            availability,
            authentication,
            capabilities: CAPABILITIES,
            models: Cow::Borrowed(MODELS),
            sign_in: (authentication == Authentication::Authenticated)
                .then_some(seatline_core::turn::SignInClassification::Unknown),
            readiness: None,
        },
    }
}

fn probe(launch: &Launch, executable: &Path) -> std::io::Result<Process> {
    let workspace = launch.workspace()?;
    let mut process = Process::spawn(&launch.command(&workspace, executable, ["auth", "status"]))?;
    process.close_stdin();
    Ok(process)
}

fn signed_in(exit: &Exit) -> Authentication {
    match exit.status.and_then(|status| status.code()) {
        Some(0) => Authentication::Authenticated,
        Some(1) => Authentication::Unauthenticated,
        _ => Authentication::Unknown,
    }
}

fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}

/// Claude Code's own directory: `CLAUDE_CONFIG_DIR`, or `.claude` in the
/// user's home.
fn claude_config_dir(launch: &Launch) -> Option<PathBuf> {
    environment::lookup(&launch.inherited, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| environment::home_dir(&launch.inherited).map(|home| home.join(".claude")))
}

/// Removes Claude Code's saved files for `session` when Claude wrote them for
/// this application: `projects/<project>/<session>.jsonl` transcripts that name
/// the session and record the application's workspace as where they ran, the directory
/// beside each, and the session's own `session-env`, `tasks`, and
/// `file-history` directories. The transcripts, which prove the session is
/// the application's, go last, so a removal that fails can be retried.
fn forget_transcript(config: &Path, workspace: &Path, session: &str) -> io::Result<()> {
    let projects = match std::fs::read_dir(config.join("projects")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        projects => projects?,
    };
    let mut transcripts = Vec::new();
    for project in projects {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let transcript = project.path().join(format!("{session}.jsonl"));
        if transcript_written_for_workspace(&transcript, session, workspace) {
            transcripts.push((transcript, project.path().join(session)));
        }
    }
    if transcripts.is_empty() {
        return Ok(());
    }
    for dir in ["session-env", "tasks", "file-history"] {
        forget::remove(&config.join(dir).join(session))?;
    }
    for (_, beside) in &transcripts {
        forget::remove(beside)?;
    }
    for (transcript, _) in &transcripts {
        forget::remove(transcript)?;
    }
    Ok(())
}

/// Whether the first record of `transcript` that says where it ran names
/// `session` and the application's `workspace`.
fn transcript_written_for_workspace(transcript: &Path, session: &str, workspace: &Path) -> bool {
    forget::head_lines(transcript).is_some_and(|lines| {
        lines
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record.get("cwd").is_some())
            .is_some_and(|record| {
                record.get("sessionId").and_then(serde_json::Value::as_str) == Some(session)
                    && record
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|cwd| forget::same_directory(cwd, workspace))
            })
    })
}

enum StatusCheck {
    Probing {
        process: Process,
        give_up: Instant,
        output: Vec<u8>,
    },
    Done(VecDeque<Update>),
}

impl Exchange for StatusCheck {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let busy_until = deadline.max(after(BUSY_LIMIT));
        loop {
            let authentication = match self {
                Self::Done(updates) => return updates.pop_front(),
                Self::Probing {
                    process, give_up, ..
                } if Instant::now() >= *give_up => {
                    process.kill();
                    Authentication::Unknown
                }
                Self::Probing {
                    process,
                    give_up,
                    output,
                } => match process.next_event(deadline.min(*give_up)) {
                    Some(Event::Exited(exit)) => signed_in(&exit),
                    Some(Event::Stdout(bytes)) => {
                        crate::keep_bounded_output(output, &bytes, 4096);
                        if Instant::now() >= busy_until {
                            return None;
                        }
                        continue;
                    }
                    Some(Event::Stderr(_)) => {
                        if Instant::now() >= busy_until {
                            return None;
                        }
                        continue;
                    }
                    None if Instant::now() >= *give_up => continue,
                    None => return None,
                },
            };
            let mut update = status_update(Availability::Available, authentication);
            if let (Self::Probing { output, .. }, Update::Status { status, .. }) =
                (&self, &mut update)
            {
                if authentication == Authentication::Authenticated {
                    status.sign_in = Some(classify_sign_in(output));
                }
            }
            *self = Self::Done(VecDeque::from([update, Update::Completed]));
        }
    }

    fn cancel(&mut self, _grace: Duration) {
        *self = Self::Done(VecDeque::from([Update::Stopped]));
    }
}

fn classify_sign_in(output: &[u8]) -> seatline_core::turn::SignInClassification {
    use seatline_core::turn::SignInClassification;
    let parsed = serde_json::from_slice::<serde_json::Value>(output).ok();
    match parsed
        .as_ref()
        .and_then(|value| value["authMethod"].as_str())
    {
        Some("claude.ai") => SignInClassification::Subscription,
        Some("api_key" | "api_key_helper") => SignInClassification::ApiKey,
        Some("third_party") => SignInClassification::Cloud,
        _ => SignInClassification::Unknown,
    }
}

/// The `claude -p` command line for one turn; the question goes on stdin.
/// A model is one `--model=<id>` argument, so the ID can never be read as an
/// option of its own.
#[cfg(test)]
fn claude_args(resume: Option<&str>, model: Option<&str>, native_search: bool) -> Vec<OsString> {
    claude_args_for_session(
        resume,
        model,
        native_search,
        seatline_core::turn::SessionPolicy::Persistent,
    )
}

fn claude_args_for_session(
    resume: Option<&str>,
    model: Option<&str>,
    native_search: bool,
    session_policy: seatline_core::turn::SessionPolicy,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        "default",
        // The adapter is conversational, not an agent. Plain turns expose no
        // built-in tools. Search turns expose and auto-approve only WebSearch;
        // WebFetch remains unavailable because it can fetch arbitrary URLs
        // from the user's machine.
        "--tools",
        if native_search { "WebSearch" } else { "" },
        "--strict-mcp-config",
        "--disallowedTools",
        "mcp__*",
    ]
    .map(OsString::from)
    .into();
    if native_search {
        args.extend(["--allowedTools", "WebSearch"].map(OsString::from));
    }
    if session_policy == seatline_core::turn::SessionPolicy::Ephemeral {
        args.push("--no-session-persistence".into());
    }
    if let Some(model) = model {
        args.push(format!("--model={model}").into());
    }
    if let Some(session) = resume {
        args.extend([OsString::from("--resume"), OsString::from(session)]);
    }
    args
}

struct Turn {
    stage: Stage,
    executable: PathBuf,
    launch: Rc<Launch>,
    prompt: String,
    /// The Claude session this run resumes.
    resume: Option<String>,
    session_policy: SessionPolicy,
    /// The session last reported, so a later result naming another one is
    /// reported again.
    reported_session: Option<String>,
    finish_grace: Duration,
    /// The model to answer with, or `None` for Claude's own default.
    model: Option<String>,
    native_search: bool,
    queue: VecDeque<Update>,
    sources: SourceCollector,
    /// WebSearch tool-use IDs observed in this run. Only matching tool_result
    /// blocks are allowed to create sources.
    web_search_uses: HashSet<String>,
    cancelled: bool,
    /// This Claude run sent `init`.
    started: bool,
    saw_delta: bool,
    messages: usize,
    break_before_text: bool,
    /// In a search turn, the text of the message being streamed, held until
    /// it's clear it isn't narration before a search ("Let me look that
    /// up"): a tool call starting in the same message drops it; the message
    /// ending, or the text growing past HELD_TEXT_LIMIT, shows it.
    held: String,
    /// The message being streamed has shown its text; the rest streams live.
    live: bool,
    outcome: Option<Result<(), ErrorBody>>,
    finish_by: Option<Instant>,
    /// The sign-in probe, when the request asked for one: for telemetry.
    probe_span: Option<Span>,
}

enum Stage {
    Probing { process: Process, give_up: Instant },
    Running(LineStream),
    Done,
}

impl Turn {
    fn start(&mut self) {
        self.probe_over();
        let Ok(workspace) = self.launch.workspace() else {
            return self.end(Update::Failed(NO_WORKSPACE));
        };
        let args = claude_args_for_session(
            self.resume.as_deref(),
            self.model.as_deref(),
            self.native_search,
            self.session_policy,
        );
        let input = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": &self.prompt}]
            }
        })
        .to_string()
            + "\n";
        match Process::spawn(&self.launch.command(&workspace, &self.executable, args)) {
            Ok(mut process) => {
                let _ = process.write(input.as_bytes());
                process.close_stdin();
                self.prompt.clear();
                self.queue.push_back(Update::Launched);
                self.stage = Stage::Running(
                    LineStream::new(process, MAX_LINE_BYTES).keeping_stderr_tail(STDERR_TAIL_BYTES),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.end(Update::Failed(NOT_INSTALLED))
            }
            Err(_) => self.end(Update::Failed(START_FAILED)),
        }
    }

    fn end(&mut self, update: Update) {
        self.probe_over();
        self.queue.push_back(update);
        self.stage = Stage::Done;
    }

    /// The sign-in probe, if there was one, is over: it exited, timed out, or
    /// the request ended while it ran.
    fn probe_over(&mut self) {
        if let Some(span) = self.probe_span.as_mut() {
            span.finish(Instant::now());
        }
    }

    fn on_line(&mut self, line: &str) {
        if self.cancelled || self.outcome.is_some() {
            return;
        }
        match output::parse(line) {
            Err(_) => self.end(Update::Failed(MALFORMED_OUTPUT)),
            Ok(Line::Init(session)) => {
                if self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                self.started = true;
                if self.session_policy == SessionPolicy::Persistent {
                    self.reported_session = Some(session.clone());
                    self.queue.push_back(Update::Session(session));
                }
                self.queue.push_back(Update::Started);
            }
            Ok(Line::ToolEvents(events)) => {
                if !self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                for event in events {
                    match event {
                        output::ToolEvent::WebSearchUse(id) => {
                            if self.native_search {
                                self.web_search_uses.insert(id);
                            }
                            self.queue.push_back(Update::Activity);
                        }
                        output::ToolEvent::ToolResult {
                            tool_use_id,
                            content,
                        } if self.web_search_uses.remove(&tool_use_id) => {
                            for result in claude_tool_result_sources(&content) {
                                if let Some(source) = self.sources.push(result) {
                                    self.queue.push_back(Update::Source(source));
                                }
                            }
                            self.queue.push_back(Update::Activity);
                        }
                        output::ToolEvent::ToolResult { .. } => {
                            self.queue.push_back(Update::Activity);
                        }
                    }
                }
            }
            Ok(Line::MessageStart) => {
                if !self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                self.flush_held();
                self.live = false;
                if self.messages > 0 {
                    self.break_before_text = true;
                }
                self.messages += 1;
            }
            Ok(Line::TextDelta(text)) => {
                if !self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                if self.native_search && !self.live {
                    self.held.push_str(&text);
                    if self.held.len() > HELD_TEXT_LIMIT {
                        // Too long to be narration: it's the answer.
                        self.flush_held();
                    }
                } else {
                    self.show_text(text);
                }
            }
            Ok(Line::ToolUseStart) => {
                if !self.live {
                    self.held.clear();
                }
                self.queue.push_back(Update::Activity);
            }
            Ok(Line::MessageStop) => {
                self.flush_held();
                self.queue.push_back(Update::Activity);
            }
            Ok(Line::ResultSuccess {
                session_id,
                text,
                usage,
            }) => {
                if !self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                // A resumed or forked session can end up under another ID: say so,
                // so the application follows it.
                if let Some(session) = session_id {
                    if self.session_policy == SessionPolicy::Persistent
                        && self.reported_session.as_deref() != Some(session.as_str())
                    {
                        self.reported_session = Some(session.clone());
                        self.queue.push_back(Update::Session(session));
                    }
                }
                self.flush_held();
                if !self.saw_delta && !text.is_empty() {
                    self.saw_delta = true;
                    self.queue.push_back(Update::Delta(text));
                }
                if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                    self.queue.push_back(Update::Usage(usage));
                }
                self.turn_ended(if self.native_search && self.sources.count() == 0 {
                    Err(NATIVE_SEARCH_NO_SOURCES)
                } else {
                    Ok(())
                });
            }
            Ok(Line::ResultFailed(error)) => self.turn_ended(Err(error)),
            Ok(Line::Ignored) => {}
        }
    }

    /// Shows held text, if any, and streams the rest of its message live.
    fn flush_held(&mut self) {
        if !self.held.is_empty() {
            let text = std::mem::take(&mut self.held);
            self.show_text(text);
            self.live = true;
        }
    }

    fn show_text(&mut self, mut text: String) {
        if text.is_empty() {
            return;
        }
        if self.messages == 0 {
            self.messages = 1;
        }
        if self.break_before_text && self.saw_delta {
            text.insert_str(0, "\n\n");
        }
        self.break_before_text = false;
        self.saw_delta = true;
        self.queue.push_back(Update::Delta(text));
    }

    fn turn_ended(&mut self, outcome: Result<(), ErrorBody>) {
        self.outcome = Some(outcome);
        self.finish_by = Some(after(self.finish_grace));
    }

    /// Claude exited. `session_gone` says it reported, on stderr before `init`,
    /// that the session it was asked to resume doesn't exist.
    fn ended(&mut self, exit: &Exit, session_gone: bool) {
        if self.cancelled {
            return self.end(Update::Stopped);
        }
        let lost = self.resume.is_some()
            && !self.saw_delta
            && match self.outcome {
                Some(Err(error)) => error.reason == UNKNOWN_SESSION.reason,
                Some(Ok(())) => false,
                None => session_gone && exit.status.is_some_and(|status| !status.success()),
            };
        if lost {
            self.queue
                .push_back(Update::SessionLost(SessionLoss::Confirmed));
            return self.end(Update::Failed(UNKNOWN_SESSION));
        }
        let update = self.exited(exit);
        self.end(update)
    }

    fn exited(&mut self, exit: &Exit) -> Update {
        match self.outcome.take() {
            Some(Ok(())) => Update::Completed,
            Some(Err(error)) => Update::Failed(error),
            None if exit.status.is_some_and(|status| status.success()) => {
                Update::Failed(MALFORMED_OUTPUT)
            }
            None => Update::Failed(PROCESS_EXITED),
        }
    }
}

impl Exchange for Turn {
    fn probe_span(&self) -> Option<Span> {
        self.probe_span
    }

    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let busy_until = deadline.max(after(BUSY_LIMIT));
        loop {
            if let Some(update) = self.queue.pop_front() {
                return Some(update);
            }
            if Instant::now() >= busy_until {
                return None;
            }
            match &mut self.stage {
                Stage::Done => return None,
                Stage::Probing { process, give_up } if Instant::now() >= *give_up => {
                    process.kill();
                    self.start();
                }
                Stage::Probing { process, give_up } => {
                    match process.next_event(deadline.min(*give_up)) {
                        Some(Event::Exited(exit)) => match signed_in(&exit) {
                            Authentication::Unauthenticated => {
                                self.end(Update::Failed(NOT_SIGNED_IN));
                            }
                            _ => self.start(),
                        },
                        Some(Event::Stdout(_) | Event::Stderr(_)) => {}
                        None if Instant::now() >= *give_up => {}
                        None => return None,
                    }
                }
                Stage::Running(stream) => {
                    // Checked first, so output that keeps coming after the
                    // turn ended can't put it off.
                    if self
                        .finish_by
                        .is_some_and(|finish_by| Instant::now() >= finish_by)
                    {
                        self.finish_by = None;
                        stream.cancel(Duration::ZERO);
                    }
                    let wait = self
                        .finish_by
                        .map_or(deadline, |finish_by| deadline.min(finish_by));
                    match stream.next(wait) {
                        Some(Output::Line(line)) => self.on_line(&line),
                        Some(Output::Final(exit) | Output::Stopped(exit)) => {
                            let session_gone = !self.started
                                && output::names_unknown_session(&String::from_utf8_lossy(
                                    stream.stderr_tail(),
                                ));
                            self.ended(&exit, session_gone);
                        }
                        Some(Output::Error(_)) => {
                            let update = if self.cancelled {
                                Update::Stopped
                            } else {
                                Update::Failed(MALFORMED_OUTPUT)
                            };
                            self.end(update);
                        }
                        None if self
                            .finish_by
                            .is_some_and(|finish_by| Instant::now() >= finish_by) => {}
                        None => return None,
                    }
                }
            }
        }
    }

    fn cancel(&mut self, grace: Duration) {
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        let finished = self.queue.iter().any(Update::is_terminal);
        self.queue.clear();
        match &mut self.stage {
            Stage::Running(stream) => stream.cancel(grace),
            Stage::Probing { .. } => self.end(Update::Stopped),
            Stage::Done if finished => self.queue.push_back(Update::Stopped),
            Stage::Done => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_one_argument_before_the_session() {
        let args = claude_args(Some("session-1"), Some("sonnet"), false);
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert!(args.contains(&"--model=sonnet"));
        assert_eq!(&args[args.len() - 2..], ["--resume", "session-1"]);
        let default = claude_args(None, None, false);
        assert!(
            !default
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("--model"))
        );
    }

    #[test]
    fn ephemeral_turns_disable_claude_session_persistence() {
        let args = claude_args_for_session(
            None,
            None,
            false,
            seatline_core::turn::SessionPolicy::Ephemeral,
        );
        assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
    }

    #[test]
    fn suggested_models_are_valid_model_ids() {
        assert_eq!(CAPABILITIES.model_selection, Capability::Supported);
        assert!(!std::hint::black_box(MODELS).is_empty());
        for model in MODELS {
            assert!(seatline_core::turn::is_model_id(&model.id), "{}", model.id);
        }
    }
}
