//! Claude Code CLI adapter (PRO-05/06).
//!
//! The adapter discovers the fixed `claude` executable through the shared
//! provider search path, checks `claude auth status`, and drives print mode
//! through stream-json on stdin/stdout. It runs one turn per process, and
//! knows no conversations: a persistent turn reports the native session it
//! runs in as an opaque handle, and resumes one it is given.
//!
//! A turn that succeeded and keeps no session ends when Claude prints its final
//! result, not when Claude exits: the process spends about half a second more
//! uploading its own usage analytics and removing its own session files, and is left to do
//! that in the background ([`seatline_core::process::Reaper`]).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{Cleanup, Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::{CachedSearchPath, FileStamp, SearchPath};
use seatline_core::exchange::SessionLoss;
use seatline_core::process::{Event, Exit, Process, ProcessSpec, Reaper};
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
use seatline_core::turn::{ReasoningEffort, SessionPolicy, ToolPolicy, Turn as TurnRequest};
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
    /// How long Claude may linger after its terminal `result` event: for a
    /// turn that keeps a session, how long the turn waits for it to save the
    /// session and leave; for one that keeps none, how long it is left to leave
    /// on its own, in the background, before it is stopped.
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
///
/// An explicit reasoning effort goes to `claude --effort`, for `low` to `max`;
/// Claude has no `none`, so that one is refused (see [`effort_level`]). Whether
/// the installed CLI and the chosen model accept a level is theirs to say.
pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: Capability::Supported,
    continuation: Capability::Supported,
    web_search: Capability::Supported,
    model_selection: Capability::Supported,
    reasoning_effort: Capability::Supported,
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

/// Whether turns are started without the user's own customizations, and what
/// has been learned about the installed Claude since.
///
/// An owner who asks for it has every turn that gives Claude no tools started
/// with `--safe-mode`: the user's hooks, plugins, skills and `CLAUDE.md` are not
/// loaded, which would otherwise cost time at every start and has no place in a
/// turn an application wrote. A turn that leaves the provider's own
/// configuration in charge ([`ToolPolicy::ProviderDefault`]) keeps them. Claude
/// still reads its settings for authentication and network configuration, and
/// still takes the model and effort a turn names.
///
/// A Claude from before the option answers `unknown option` and runs nothing.
/// The turn is then started again without it, once, and that executable is not
/// tried with it again until it changes.
struct Isolation {
    enabled: bool,
    /// The executable that did not know `--safe-mode`, as it was then.
    unsupported: RefCell<Option<(PathBuf, FileStamp)>>,
}

impl Isolation {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            unsupported: RefCell::new(None),
        }
    }

    /// Whether a turn with `tools`, run by `executable`, starts in safe mode.
    fn applies(&self, executable: &Path, tools: ToolPolicy) -> bool {
        self.enabled && tools != ToolPolicy::ProviderDefault && !self.is_unsupported(executable)
    }

    fn is_unsupported(&self, executable: &Path) -> bool {
        self.unsupported
            .borrow()
            .as_ref()
            .is_some_and(|(path, stamp)| {
                // Replaced or upgraded since: it may know the option now.
                path == executable
                    && FileStamp::read(executable).ok().flatten().as_ref() == Some(stamp)
            })
    }

    fn remember_unsupported(&self, executable: &Path) {
        if let Ok(Some(stamp)) = FileStamp::read(executable) {
            *self.unsupported.borrow_mut() = Some((executable.to_path_buf(), stamp));
        }
    }

    fn forget(&self) {
        self.unsupported.borrow_mut().take();
    }
}

pub struct Claude {
    search: CachedSearchPath,
    launch: Rc<Launch>,
    limits: Limits,
    account_file: readiness::AccountFile,
    isolation: Rc<Isolation>,
    /// Waits for the processes of finished turns that keep no session: the
    /// process-wide one, unless a test asks for its own.
    reaper: Reaper,
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
            isolation: Rc::new(Isolation::new(false)),
            reaper: Reaper::shared(),
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

    /// Starts the turns that give Claude no tools without the user's own hooks,
    /// plugins, skills and `CLAUDE.md` (`claude --safe-mode`). Off unless asked
    /// for. A turn with [`ToolPolicy::ProviderDefault`] keeps them, and so does
    /// every turn on a Claude that does not know the option.
    #[must_use]
    pub fn with_isolated_launch(mut self, enabled: bool) -> Self {
        self.isolation = Rc::new(Isolation::new(enabled));
        self
    }

    /// How many finished turns' processes this adapter alone may leave to exit
    /// in the background at once, instead of sharing the process-wide bound
    /// ([`seatline_core::process::SHARED_REAPER_CAPACITY`]). With none, every
    /// turn waits for its own process.
    #[must_use]
    pub fn with_background_exits(mut self, capacity: usize) -> Self {
        self.reaper = Reaper::new(capacity);
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

    fn refuses_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        effort_level(effort).is_none()
    }

    fn invalidate_readiness(&self) {
        self.search.invalidate();
        self.account_file.invalidate();
        self.isolation.forget();
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
        if request
            .reasoning_effort
            .is_some_and(|effort| self.refuses_reasoning_effort(effort))
        {
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
        let isolate = self.isolation.applies(&executable, request.tools);

        let mut turn = Turn {
            stage: Stage::Done,
            executable,
            launch: Rc::clone(&self.launch),
            prompt: prompt::render(request.system.as_deref(), &request.messages, request.tools),
            resume: request.continuation,
            session_policy: request.session,
            reported_session: None,
            finish_grace: self.limits.finish,
            stop_grace: self.limits.timeouts.stop_grace,
            reaper: self.reaper.clone(),
            model: request.model,
            reasoning_effort: request.reasoning_effort,
            isolation: Rc::clone(&self.isolation),
            isolate,
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
            result_at: None,
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
            Some(Err(_)) | None => turn.start(false),
        }
        Box::new(turn)
    }
}

/// The level `claude --effort` takes for `effort`, or `None` when Claude has no
/// such level. Claude's lowest is `low`, and a value it does not know is not an
/// error but a warning, after which it uses its own default: so `none` must
/// never be passed on, or an explicit choice would be silently replaced.
/// Written out level by level, so a new `ReasoningEffort` has to be decided
/// here rather than reaching the command line by accident.
fn effort_level(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::Xhigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
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
        None,
        false,
    )
}

fn claude_args_for_session(
    resume: Option<&str>,
    model: Option<&str>,
    native_search: bool,
    session_policy: seatline_core::turn::SessionPolicy,
    effort: Option<ReasoningEffort>,
    isolate: bool,
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
    if isolate {
        // Leaves out what the user's own setup would load at every start:
        // hooks, plugins, skills and CLAUDE.md. Settings that carry
        // authentication or a proxy still apply.
        args.push("--safe-mode".into());
    }
    if session_policy == seatline_core::turn::SessionPolicy::Ephemeral {
        args.push("--no-session-persistence".into());
    }
    if let Some(model) = model {
        args.push(format!("--model={model}").into());
    }
    if let Some(level) = effort.and_then(effort_level) {
        // One argument, like the model: the level can never be read as an
        // option of its own. `send` has already refused a level Claude lacks.
        args.push(format!("--effort={level}").into());
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
    stop_grace: Duration,
    reaper: Reaper,
    /// The model to answer with, or `None` for Claude's own default.
    model: Option<String>,
    /// The effort to answer with, or `None` for Claude's own default.
    reasoning_effort: Option<ReasoningEffort>,
    isolation: Rc<Isolation>,
    /// Whether the run in progress was started with `--safe-mode`. The question
    /// is kept until Claude starts, so that a Claude that does not know the
    /// option can be asked again without it.
    isolate: bool,
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
    /// When Claude's final `result` was read: for telemetry.
    result_at: Option<Instant>,
    /// The sign-in probe, when the request asked for one: for telemetry.
    probe_span: Option<Span>,
}

enum Stage {
    Probing { process: Process, give_up: Instant },
    Running(LineStream),
    Done,
}

impl Turn {
    /// Starts `claude -p` and hands it the question. `relaunch` says an earlier
    /// run of this turn was rejected before it began, so the application has
    /// already heard that a process was launched.
    fn start(&mut self, relaunch: bool) {
        self.probe_over();
        let Ok(workspace) = self.launch.workspace() else {
            return self.end(Update::Failed(NO_WORKSPACE));
        };
        let args = claude_args_for_session(
            self.resume.as_deref(),
            self.model.as_deref(),
            self.native_search,
            self.session_policy,
            self.reasoning_effort,
            self.isolate,
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
                if !self.isolate {
                    self.prompt.clear();
                }
                if !relaunch {
                    self.queue.push_back(Update::Launched);
                }
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
                self.prompt = String::new();
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
        self.result_at.get_or_insert_with(Instant::now);
        self.outcome = Some(outcome);
        self.finish_by = Some(after(self.finish_grace));
    }

    /// A turn that succeeded and keeps no session has nothing left to wait for
    /// once Claude has said its last word: the answer and its usage are in
    /// hand, and there is no transcript for the next turn to resume. What Claude
    /// does between its `result` and its exit is its own wrap-up, about half a
    /// second of which is uploading its own usage analytics, and the application should not
    /// wait through it. So the process is left to leave on its own, in the
    /// background, and the turn completes now.
    ///
    /// The process is not stopped to save that time. A signal is answered by
    /// the same wrap-up and takes as long, and a kill skips it and leaves the
    /// session bookkeeping Claude removes on a normal exit behind in the user's
    /// own `~/.claude`. It is waited for off the turn's path, for the same
    /// `finish` grace the turn would have waited, and then stopped.
    ///
    /// A turn that keeps a session waits for the process as before: Claude is
    /// still writing the transcript the next turn resumes. So does a failed one,
    /// and one the reaper has no room for. A cancelled turn is already stopping
    /// its process, which the stream refuses to release.
    fn release_finished(&mut self) {
        if !matches!(self.outcome, Some(Ok(()))) || self.session_policy != SessionPolicy::Ephemeral
        {
            return;
        }
        match std::mem::replace(&mut self.stage, Stage::Done) {
            Stage::Running(stream) => {
                match stream.release(&self.reaper, self.finish_grace, self.stop_grace) {
                    Ok(()) => self.queue.push_back(Update::Completed),
                    Err(stream) => self.stage = Stage::Running(*stream),
                }
            }
            other => self.stage = other,
        }
    }

    /// Claude exited. `session_gone` says it reported, on stderr before `init`,
    /// that the session it was asked to resume doesn't exist; `effort_unknown`
    /// that it does not know `--effort`, which a Claude from before the option
    /// reports instead of starting. Nothing ran in either case.
    fn ended(&mut self, exit: &Exit, session_gone: bool, effort_unknown: bool) {
        if self.cancelled {
            return self.end(Update::Stopped);
        }
        if effort_unknown {
            return self.end(Update::Failed(crate::REASONING_EFFORT_UNSUPPORTED));
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

    fn result_at(&self) -> Option<Instant> {
        self.result_at
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
                    self.start(false);
                }
                Stage::Probing { process, give_up } => {
                    match process.next_event(deadline.min(*give_up)) {
                        Some(Event::Exited(exit)) => match signed_in(&exit) {
                            Authentication::Unauthenticated => {
                                self.end(Update::Failed(NOT_SIGNED_IN));
                            }
                            _ => self.start(false),
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
                        Some(Output::Line(line)) => {
                            self.on_line(&line);
                            self.release_finished();
                        }
                        Some(Output::Final(exit) | Output::Stopped(exit)) => {
                            let stderr = String::from_utf8_lossy(stream.stderr_tail());
                            let session_gone =
                                !self.started && output::names_unknown_session(&stderr);
                            let effort_unknown = !self.started
                                && self.reasoning_effort.is_some()
                                && output::names_unknown_option(&stderr, "--effort");
                            let safe_mode_unknown = !self.started
                                && self.isolate
                                && !self.cancelled
                                && output::names_unknown_option(&stderr, "--safe-mode");
                            if safe_mode_unknown {
                                // Nothing ran. Ask again without the option, once,
                                // and do not offer it to this executable again.
                                self.isolation.remember_unsupported(&self.executable);
                                self.isolate = false;
                                self.start(true);
                                continue;
                            }
                            self.ended(&exit, session_gone, effort_unknown);
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
    fn an_effort_is_one_argument_after_the_model_and_before_the_session() {
        let args = claude_args_for_session(
            Some("session-1"),
            Some("sonnet"),
            false,
            SessionPolicy::Persistent,
            Some(ReasoningEffort::Xhigh),
            false,
        );
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        let model = args
            .iter()
            .position(|arg| *arg == "--model=sonnet")
            .unwrap();
        assert_eq!(args[model + 1], "--effort=xhigh", "{args:?}");
        assert_eq!(&args[args.len() - 2..], ["--resume", "session-1"]);
        assert_eq!(args.iter().filter(|arg| arg.contains("effort")).count(), 1);

        // No choice, no option: Claude keeps its own default.
        let default = claude_args(None, None, false);
        assert!(
            !default
                .iter()
                .any(|arg| arg.to_string_lossy().contains("effort"))
        );
    }

    #[test]
    fn every_effort_has_a_claude_level_except_none() {
        for (effort, level) in [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Xhigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            assert_eq!(effort_level(effort), Some(level));
            // The word a client sends is the word Claude takes.
            assert_eq!(effort.as_str(), level);
        }
        // Claude would only warn about it, and answer with its default.
        assert_eq!(effort_level(ReasoningEffort::None), None);
        let args = claude_args_for_session(
            None,
            None,
            false,
            SessionPolicy::Ephemeral,
            Some(ReasoningEffort::None),
            false,
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("effort")),
            "a level Claude lacks is never passed on"
        );
    }

    #[test]
    fn ephemeral_turns_disable_claude_session_persistence() {
        let args = claude_args_for_session(
            None,
            None,
            false,
            seatline_core::turn::SessionPolicy::Ephemeral,
            None,
            false,
        );
        assert!(args.iter().any(|arg| arg == "--no-session-persistence"));
    }

    #[test]
    fn safe_mode_is_asked_for_only_when_a_turn_is_isolated() {
        let flag = |isolate| {
            claude_args_for_session(
                Some("session-1"),
                Some("sonnet"),
                false,
                SessionPolicy::Ephemeral,
                Some(ReasoningEffort::Low),
                isolate,
            )
            .iter()
            .any(|arg| arg == "--safe-mode")
        };
        assert!(flag(true));
        assert!(!flag(false));
        // Still the safe stream-json arguments, with or without it.
        let args = claude_args_for_session(None, None, true, SessionPolicy::Persistent, None, true);
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        for expected in ["--strict-mcp-config", "--safe-mode", "WebSearch"] {
            assert!(args.contains(&expected), "{expected}: {args:?}");
        }
    }

    #[test]
    fn only_an_enabled_isolation_that_the_executable_knows_applies_to_a_turn_without_tools() {
        let executable = std::env::current_exe().unwrap();
        let off = Isolation::new(false);
        let on = Isolation::new(true);
        for tools in [ToolPolicy::None, ToolPolicy::NativeWebSearch] {
            assert!(!off.applies(&executable, tools));
            assert!(on.applies(&executable, tools));
        }
        // The provider's own configuration is what such a turn asked for.
        assert!(!on.applies(&executable, ToolPolicy::ProviderDefault));
        // An executable that did not know the option is left alone, until it
        // changes or what was learned is forgotten.
        on.remember_unsupported(&executable);
        assert!(!on.applies(&executable, ToolPolicy::None));
        assert!(on.applies(&executable.with_extension("other"), ToolPolicy::None));
        on.forget();
        assert!(on.applies(&executable, ToolPolicy::None));
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
