//! The Codex CLI adapter: discovery and sign-in status (PRO-02), requests and
//! streaming (PRO-03), and cancellation, timeouts, and failures (PRO-04).
//!
//! A request runs `codex exec --json` in a read-only sandbox, from an empty
//! working directory, with the question on stdin. Its JSON lines become
//! runtime updates (see [`output`]). The adapter knows no conversations: a
//! persistent turn reports the Codex thread it runs in as an opaque handle,
//! and continues one it is given with `codex exec resume`.
//!
//! Before a request that asks for it, `codex login status` checks the sign-in, because a
//! signed-out `codex exec` retries the network instead of failing. Only its
//! exit status is read: its output names the account and a masked key.
//!
//! Codex runs in a workspace nobody but its user can change ([`workspace`]),
//! with a minimal environment (SEC-02): the variables every provider gets
//! ([`environment::INHERITED`]), Codex's own settings, and a `PATH` that
//! starts with Codex's directory.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{Cleanup, Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::SearchPath;
use seatline_core::exchange::SessionLoss;
use seatline_core::process::{Event, Exit, Process, ProcessSpec};
use seatline_core::prompt;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, Capability, ErrorCode, ProviderState,
};
use seatline_core::search::{NATIVE_SEARCH_NO_SOURCES, SourceCollector, codex_message_sources};
use seatline_core::stream::{BUSY_LIMIT, LineStream, Output};
use seatline_core::turn::{SessionPolicy, ToolPolicy, Turn as TurnRequest};
use seatline_platform::discovery;
use seatline_platform::environment;
use seatline_platform::forget;
use seatline_platform::layout::Layout;
use seatline_platform::workspace;

pub mod output;

use output::Line;

pub const ID: &str = "codex";

/// The executable the adapter looks for.
const EXECUTABLE: &str = "codex";

/// Codex's own settings, passed on when set: where it keeps its settings,
/// sign-in, and state, and an extra CA certificate for its connections.
pub const CODEX_VARIABLES: &[&str] = &["CODEX_HOME", "CODEX_SQLITE_HOME", "CODEX_CA_CERTIFICATE"];

/// Longest line of Codex output. One line holds a whole answer, which can be
/// long; a line past this ends the request instead of growing memory.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// The adapter's time limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The host's limits for each request.
    pub timeouts: Timeouts,
    /// How long `codex login status` may take before the sign-in counts as
    /// unknown.
    pub probe: Duration,
    /// How long Codex gets to save its session and exit after the turn ended.
    pub finish: Duration,
}

/// Codex answers without token deltas, so a long silence can be a model
/// thinking; five minutes without any progress means it is stuck.
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

/// What this adapter supports. Answers arrive a message at a time, not token
/// by token. Browser context is framed as untrusted reference data in the
/// prompt; attachments are not passed to Codex yet. A chosen model goes to
/// `codex exec --model`. Codex has no stable way to list its models, so the
/// adapter suggests none: any valid model ID is passed on, and Codex reports
/// one it doesn't know as a failed turn.
pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: Capability::Supported,
    continuation: Capability::Supported,
    web_search: Capability::Supported,
    model_selection: Capability::Supported,
    cancellation: Capability::Supported,
    tool_isolation: Capability::Supported,
};

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

/// A tool-free turn was asked for, but the user's own Codex configuration
/// exposes tools the adapter can't switch off.
const TOOL_ISOLATION_UNAVAILABLE: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "TOOL_ISOLATION_UNAVAILABLE",
    retryable: false,
};

const SEARCH_TOOLS_ENABLED: ErrorBody = ErrorBody {
    code: ErrorCode::SearchFailed,
    reason: "NATIVE_SEARCH_CONFIGURATION_UNSAFE",
    retryable: false,
};

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

/// How Codex processes start: where they run, and what environment they get.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    work_dir: PathBuf,
    /// Variables copied from the host's environment.
    inherited: Vec<(OsString, OsString)>,
    /// The host's `PATH`.
    path: Option<OsString>,
}

impl Launch {
    fn new(work_dir: PathBuf, host: Vec<(OsString, OsString)>) -> Self {
        let path = environment::lookup(&host, "PATH").map(OsStr::to_os_string);
        Self {
            work_dir,
            inherited: environment::inherit(host, CODEX_VARIABLES),
            path,
        }
    }

    /// The workspace, created if needed and checked before every launch:
    /// the path Codex runs in and is pointed at.
    fn workspace(&self) -> std::io::Result<PathBuf> {
        workspace::prepare(&self.work_dir)
    }

    /// `codex` with `args`, in `workspace`, with only its environment.
    fn command<I>(&self, workspace: &Path, executable: &Path, args: I) -> ProcessSpec
    where
        I: IntoIterator,
        I::Item: Into<OsString>,
    {
        ProcessSpec::new(executable)
            .args(args)
            .envs(self.inherited.iter().cloned())
            .env(
                "PATH",
                environment::search_path_for(executable, self.path.as_deref()),
            )
            .current_dir(workspace)
    }
}

/// The Codex CLI adapter.
pub struct Codex {
    search: SearchPath,
    launch: Rc<Launch>,
    limits: Limits,
}

impl Codex {
    /// The adapter of an installed host: the platform lookup rules, and an
    /// empty workspace in the user's own cache directory.
    pub fn installed(layout: &Layout) -> Self {
        let host: Vec<_> = std::env::vars_os().collect();
        Self::new(
            discovery::installed(layout),
            layout.workspace(&host, "codex"),
        )
    }

    /// Looks for `codex` in `search`, and runs it in `work_dir`, which it
    /// creates when needed and refuses if other users could change it, with
    /// variables from the host's environment.
    pub fn new(search: SearchPath, work_dir: PathBuf) -> Self {
        Self {
            search,
            launch: Rc::new(Launch::new(work_dir, std::env::vars_os().collect())),
            limits: LIMITS,
        }
    }

    /// Takes the variables Codex gets from `host` instead of the host's own
    /// environment.
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

    /// Replaces the default time limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    fn executable(&self) -> Option<PathBuf> {
        self.search.find(EXECUTABLE)
    }

    /// Refuses a turn that needs tools switched off while the user's own
    /// configuration exposes some Codex can't switch off. A turn that leaves
    /// the provider's own configuration in charge runs whatever it says.
    fn check_tools(&self, tools: ToolPolicy) -> Result<(), ErrorBody> {
        if tools != ToolPolicy::ProviderDefault && !context_configuration_is_safe(&self.launch) {
            return Err(if tools == ToolPolicy::NativeWebSearch {
                SEARCH_TOOLS_ENABLED
            } else {
                TOOL_ISOLATION_UNAVAILABLE
            });
        }
        Ok(())
    }
}

impl Provider for Codex {
    fn id(&self) -> &str {
        ID
    }

    fn supports_persistent_session(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = CAPABILITIES;
        if !context_configuration_is_safe(&self.launch) {
            capabilities.web_search = Capability::Unsupported;
            capabilities.tool_isolation = Capability::Unsupported;
        }
        capabilities
    }

    fn timeouts(&self) -> Timeouts {
        self.limits.timeouts
    }

    fn status(&self) -> Box<dyn Exchange> {
        let capabilities = self.capabilities();
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::new([
                status_update(
                    Availability::NotFound,
                    Authentication::Unknown,
                    capabilities,
                    None,
                ),
                Update::Completed,
            ]));
        };
        Box::new(match probe(&self.launch, &executable) {
            Ok(process) => StatusCheck::Probing {
                process,
                give_up: after(self.limits.probe),
                capabilities,
                output: Vec::new(),
            },
            Err(_) => StatusCheck::Done(VecDeque::from([
                status_update(
                    Availability::Unavailable,
                    Authentication::Unknown,
                    capabilities,
                    None,
                ),
                Update::Completed,
            ])),
        })
    }

    fn cleanup_sessions(&self, sessions: &[String]) -> Cleanup {
        let home = codex_home(&self.launch);
        let workspace = self.launch.work_dir.clone();
        let sessions = sessions.to_vec();
        Cleanup::new(
            move || {
                let Some(home) = home else {
                    return Ok(());
                };
                for session in &sessions {
                    forget_rollouts(&home, &workspace, session)?;
                }
                Ok(())
            },
            || {},
        )
    }

    fn send(&self, request: TurnRequest) -> Box<dyn Exchange> {
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::failed(NOT_INSTALLED));
        };
        if request.validate().is_err() {
            return Box::new(Scripted::failed(crate::INVALID_TURN));
        }
        if let Err(error) = self.check_tools(request.tools) {
            return Box::new(Scripted::failed(error));
        }

        let mut turn = Turn {
            stage: Stage::Done,
            executable,
            launch: Rc::clone(&self.launch),
            prompt: prompt::render(request.system.as_deref(), &request.messages, request.tools),
            resume: request.continuation,
            session_policy: request.session,
            finish_grace: self.limits.finish,
            // Page text and search results are attacker-controlled: a turn
            // that isn't left to the user's own configuration runs answer-only.
            restrict_tools: request.tools != ToolPolicy::ProviderDefault,
            context_turn: request.tools == ToolPolicy::None,
            native_search: request.tools == ToolPolicy::NativeWebSearch,
            model: request.model,
            queue: VecDeque::new(),
            sources: SourceCollector::new(ID),
            cancelled: false,
            thread_id: None,
            started: false,
            messages: 0,
            held: None,
            outcome: None,
            finish_by: None,
        };
        let probe = request
            .check_sign_in
            .then(|| probe(&turn.launch, &turn.executable));
        match probe {
            Some(Ok(process)) => {
                turn.stage = Stage::Probing {
                    process,
                    give_up: after(self.limits.probe),
                };
            }
            // The sign-in can't be checked; the request itself will tell.
            Some(Err(_)) | None => turn.start(),
        }
        Box::new(turn)
    }
}

/// Codex's own directory: `CODEX_HOME`, or `.codex` in the user's home.
fn codex_home(launch: &Launch) -> Option<PathBuf> {
    environment::lookup(&launch.inherited, "CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| environment::home_dir(&launch.inherited).map(|home| home.join(".codex")))
}

fn context_configuration_is_safe(launch: &Launch) -> bool {
    let home = codex_home(launch);
    let Some(home) = home else {
        return true;
    };

    // Plugin caches/install directories and hook files are safe to leave in
    // place because context turns explicitly disable those Codex features at
    // invocation time. User-configured MCP servers are different: Codex
    // exposes them independently of the plugin/apps feature gates, so fail
    // closed until the adapter can disable each effective server deterministically.
    let mut configs = vec![home.join("config.toml")];
    if home.exists() {
        let Ok(entries) = std::fs::read_dir(&home) else {
            return false;
        };
        configs.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| name.ends_with(".config.toml"))
                }),
        );
    }
    configs.into_iter().all(|path| {
        if !path.exists() {
            return true;
        }
        let Ok(config) = std::fs::read_to_string(path) else {
            return false;
        };
        !config.to_ascii_lowercase().contains("mcp_servers")
    })
}

fn status_update(
    availability: Availability,
    authentication: Authentication,
    capabilities: Capabilities,
    sign_in: Option<seatline_core::turn::SignInClassification>,
) -> Update {
    Update::Status {
        provider_id: ID.to_owned(),
        status: ProviderState {
            availability,
            authentication,
            capabilities,
            models: Cow::Borrowed(&[]),
            sign_in,
        },
    }
}

/// `codex login status`: exit status 0 means signed in, 1 signed out.
fn probe(launch: &Launch, executable: &Path) -> std::io::Result<Process> {
    let workspace = launch.workspace()?;
    let mut process = Process::spawn(&launch.command(&workspace, executable, ["login", "status"]))?;
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

/// Removes Codex's saved sessions of `thread` that Codex wrote for this application:
/// `sessions/YYYY/MM/DD/rollout-…-<thread>.jsonl` and
/// `archived_sessions/rollout-…-<thread>.jsonl` files whose `session_meta`
/// names the thread and records the application's workspace as where it ran. Codex's
/// own state database is left alone.
fn forget_rollouts(home: &Path, workspace: &Path, thread: &str) -> io::Result<()> {
    let suffix = format!("-{thread}.jsonl");
    // Each directory with how many levels of directories may lie below it.
    let mut pending = vec![
        (home.join("sessions"), 3),
        (home.join("archived_sessions"), 0),
    ];
    while let Some((dir, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            entries => entries?,
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                if depth > 0 {
                    pending.push((path, depth - 1));
                }
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
                && rollout_written_for_workspace(&path, thread, workspace)
            {
                forget::remove(&path)?;
            }
        }
    }
    Ok(())
}

fn rollout_written_for_workspace(path: &Path, thread: &str, workspace: &Path) -> bool {
    forget::head_lines(path).is_some_and(|lines| {
        lines
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| {
                record.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
            })
            .and_then(|record| record.get("payload").cloned())
            .is_some_and(|meta| {
                meta.get("id").and_then(serde_json::Value::as_str) == Some(thread)
                    && meta
                        .get("cwd")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|cwd| forget::same_directory(cwd, workspace))
            })
    })
}

/// The `codex exec` command line for one turn. The question itself goes on
/// stdin (`-`). `--model` is a global `exec` option, so it applies to
/// `resume` too; it is one `--model=<id>` argument, so the ID can never be
/// read as an option of its own.
#[cfg(test)]
fn exec_args(
    workspace: &Path,
    restrict_tools: bool,
    context_turn: bool,
    native_search: bool,
    resume: Option<&str>,
    model: Option<&str>,
) -> Vec<OsString> {
    exec_args_for_session(
        workspace,
        seatline_core::turn::SessionPolicy::Persistent,
        restrict_tools,
        context_turn,
        native_search,
        resume,
        model,
    )
}

fn exec_args_for_session(
    workspace: &Path,
    session_policy: seatline_core::turn::SessionPolicy,
    restrict_tools: bool,
    context_turn: bool,
    native_search: bool,
    resume: Option<&str>,
    model: Option<&str>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
    ]
    .map(OsString::from)
    .into();
    if session_policy == seatline_core::turn::SessionPolicy::Ephemeral {
        args.push("--ephemeral".into());
    }
    if restrict_tools {
        // Page text is attacker-controlled. A context turn is deliberately
        // answer-only: no local shell/image tools, apps/plugins/hooks,
        // web search, orchestrator MCP, or subagents. User-configured MCP
        // servers are refused before this point because they are not all
        // controlled by those feature gates.
        for setting in [
            "features.shell_tool=false",
            "features.view_image=false",
            "features.apps=false",
            "features.plugins=false",
            "features.hooks=false",
            "features.multi_agent=false",
            "features.multi_agent_v2=false",
            "features.standalone_web_search=false",
            "orchestrator.mcp.enabled=false",
        ] {
            args.extend(["-c".into(), setting.into()]);
        }
        if context_turn {
            // Older Codex builds also honor these gates. Keep them for
            // attacker-controlled page-context turns as defense in depth.
            for setting in [
                "features.web_search_request=false",
                "features.web_search_cached=false",
            ] {
                args.extend(["-c".into(), setting.into()]);
            }
        }
    }
    // Search is explicit. Plain and context turns never inherit
    // Codex's cached-search default.
    args.extend([
        "-c".into(),
        if native_search && !context_turn {
            "web_search=\"live\"".into()
        } else {
            "web_search=\"disabled\"".into()
        },
    ]);
    if let Some(model) = model {
        args.push(format!("--model={model}").into());
    }
    args.push("-C".into());
    args.push(workspace.as_os_str().to_owned());
    if let Some(thread_id) = resume {
        args.extend(["resume", thread_id].map(OsString::from));
    }
    args.push("-".into());
    args
}

/// The `provider.status` check.
enum StatusCheck {
    Probing {
        process: Process,
        give_up: Instant,
        capabilities: Capabilities,
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
                // Checked first, so output that keeps coming can't put it off.
                Self::Probing {
                    process,
                    give_up,
                    capabilities: _,
                    output: _,
                } if Instant::now() >= *give_up => {
                    process.kill();
                    Authentication::Unknown
                }
                Self::Probing {
                    process,
                    give_up,
                    capabilities: _,
                    output,
                } => {
                    match process.next_event(deadline.min(*give_up)) {
                        Some(Event::Exited(exit)) => signed_in(&exit),
                        // Keep only a bounded probe prefix for billing-mode
                        // classification; it is never logged or forwarded.
                        Some(Event::Stdout(bytes) | Event::Stderr(bytes)) => {
                            crate::keep_bounded_output(output, &bytes, STATUS_OUTPUT_BYTES);
                            if Instant::now() >= busy_until {
                                return None;
                            }
                            continue;
                        }
                        None if Instant::now() >= *give_up => continue,
                        None => return None,
                    }
                }
            };
            let (capabilities, sign_in) = match self {
                Self::Probing {
                    capabilities,
                    output,
                    ..
                } => (*capabilities, classify_sign_in(authentication, output)),
                Self::Done(_) => unreachable!("handled above"),
            };
            *self = Self::Done(VecDeque::from([
                status_update(
                    Availability::Available,
                    authentication,
                    capabilities,
                    sign_in,
                ),
                Update::Completed,
            ]));
        }
    }

    fn cancel(&mut self, _grace: Duration) {
        *self = Self::Done(VecDeque::from([Update::Stopped]));
    }
}

const STATUS_OUTPUT_BYTES: usize = 4096;

fn classify_sign_in(
    authentication: Authentication,
    output: &[u8],
) -> Option<seatline_core::turn::SignInClassification> {
    if authentication != Authentication::Authenticated {
        return None;
    }
    let text = String::from_utf8_lossy(output).to_ascii_lowercase();
    Some(if text.contains("api key") {
        seatline_core::turn::SignInClassification::ApiKey
    } else if text.contains("chatgpt") || text.contains("subscription") {
        seatline_core::turn::SignInClassification::Subscription
    } else {
        seatline_core::turn::SignInClassification::Unknown
    })
}

/// One `conversation.send`: the sign-in probe, then the `codex exec` turn.
struct Turn {
    stage: Stage,
    executable: PathBuf,
    launch: Rc<Launch>,
    prompt: String,
    /// The Codex thread to resume.
    resume: Option<String>,
    session_policy: SessionPolicy,
    finish_grace: Duration,
    /// Browser context or native search requires all unrelated Codex tool
    /// surfaces to be disabled.
    restrict_tools: bool,
    /// Whether untrusted browser context is attached to this turn.
    context_turn: bool,
    /// Whether this same Codex turn may use its authenticated web search.
    native_search: bool,
    /// The model to answer with, or `None` for Codex's own default.
    model: Option<String>,
    /// Updates produced but not yet returned.
    queue: VecDeque<Update>,
    sources: SourceCollector,
    cancelled: bool,
    thread_id: Option<String>,
    started: bool,
    messages: usize,
    /// In a search turn, the latest message, held until Codex's next event
    /// shows whether a web search follows it. One that does is narration
    /// ("I'll look that up"), not answer, and is dropped.
    held: Option<String>,
    /// How the turn ended, once Codex said so.
    outcome: Option<Result<(), ErrorBody>>,
    /// When to stop waiting for Codex to exit after the turn ended.
    finish_by: Option<Instant>,
}

enum Stage {
    Probing {
        process: Process,
        give_up: Instant,
    },
    Running(LineStream),
    /// Finished: any process has been dropped, which reaps it.
    Done,
}

impl Turn {
    /// Starts `codex exec` and hands it the question.
    fn start(&mut self) {
        let Ok(workspace) = self.launch.workspace() else {
            return self.end(Update::Failed(NO_WORKSPACE));
        };
        let args = exec_args_for_session(
            &workspace,
            self.session_policy,
            self.restrict_tools,
            self.context_turn,
            self.native_search,
            self.resume.as_deref(),
            self.model.as_deref(),
        );
        match Process::spawn(&self.launch.command(&workspace, &self.executable, args)) {
            Ok(mut process) => {
                // The question goes on stdin: it can exceed the size one
                // argument may have, and no shell ever sees it.
                let _ = process.write(std::mem::take(&mut self.prompt).as_bytes());
                process.close_stdin();
                self.queue.push_back(Update::Launched);
                self.stage = Stage::Running(LineStream::new(process, MAX_LINE_BYTES));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.end(Update::Failed(NOT_INSTALLED))
            }
            Err(_) => self.end(Update::Failed(START_FAILED)),
        }
    }

    /// Queues the terminal update and drops any process, which reaps it.
    fn end(&mut self, update: Update) {
        self.queue.push_back(update);
        self.stage = Stage::Done;
    }

    /// Acts on one line of Codex output.
    fn on_line(&mut self, line: &str) {
        if self.cancelled || self.outcome.is_some() {
            return;
        }
        match output::parse(line) {
            Err(_) => self.end(Update::Failed(MALFORMED_OUTPUT)),
            Ok(Line::ThreadStarted(thread_id)) => self.thread_id = Some(thread_id),
            Ok(Line::TurnStarted) => self.turn_started(),
            Ok(Line::AgentMessage(text)) => {
                if !self.started {
                    return self.end(Update::Failed(MALFORMED_OUTPUT));
                }
                if self.native_search {
                    self.flush_held();
                    self.held = Some(text);
                } else {
                    self.show_message(text);
                }
            }
            Ok(Line::WebSearch) => {
                // What came before a search was narration.
                self.held = None;
                self.queue.push_back(Update::Activity);
            }
            Ok(Line::Progress) => self.queue.push_back(Update::Activity),
            Ok(Line::TurnCompleted(_)) if !self.started => {
                self.end(Update::Failed(MALFORMED_OUTPUT))
            }
            Ok(Line::TurnCompleted(usage)) => {
                self.flush_held();
                if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                    self.queue.push_back(Update::Usage(usage));
                }
                self.turn_ended(Ok(()));
            }
            Ok(Line::TurnFailed(message)) => {
                self.flush_held();
                self.turn_ended(Err(output::turn_failure(&message)));
            }
            Ok(Line::Ignored) => {}
        }
    }

    fn flush_held(&mut self) {
        if let Some(text) = self.held.take() {
            self.show_message(text);
        }
    }

    /// One message of the answer. In a search turn, the links it cites are
    /// its sources.
    fn show_message(&mut self, text: String) {
        if self.native_search {
            for result in codex_message_sources(&text) {
                if let Some(source) = self.sources.push(result) {
                    self.queue.push_back(Update::Source(source));
                }
            }
        }
        if !text.is_empty() {
            // Later messages continue the answer after a blank line.
            let text = if self.messages == 0 {
                text
            } else {
                format!("\n\n{text}")
            };
            self.messages += 1;
            self.queue.push_back(Update::Delta(text));
        }
    }

    fn turn_started(&mut self) {
        if self.started {
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            return self.end(Update::Failed(MALFORMED_OUTPUT));
        };
        self.started = true;
        if self.session_policy == SessionPolicy::Persistent {
            self.queue.push_back(Update::Session(thread_id));
        }
        self.queue.push_back(Update::Started);
    }

    fn turn_ended(&mut self, outcome: Result<(), ErrorBody>) {
        let outcome = if outcome.is_ok() && self.native_search && self.sources.count() == 0 {
            Err(NATIVE_SEARCH_NO_SOURCES)
        } else {
            outcome
        };
        self.outcome = Some(outcome);
        self.finish_by = Some(after(self.finish_grace));
    }

    /// The terminal update once the process has exited.
    fn exited(&mut self, exit: &Exit) -> Update {
        match self.outcome.take() {
            Some(Ok(())) => Update::Completed,
            Some(Err(error)) => Update::Failed(error),
            // The process ended without finishing the turn.
            None if exit.status.is_some_and(|status| status.success()) => {
                Update::Failed(MALFORMED_OUTPUT)
            }
            None => Update::Failed(PROCESS_EXITED),
        }
    }
}

impl Exchange for Turn {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let busy_until = deadline.max(after(BUSY_LIMIT));
        loop {
            if let Some(update) = self.queue.pop_front() {
                return Some(update);
            }
            // Output that keeps coming without an update, such as lines the
            // adapter ignores, gives the caller its turn back.
            if Instant::now() >= busy_until {
                return None;
            }
            match &mut self.stage {
                Stage::Done => return None,
                // Checked first, so output that keeps coming can't put it off.
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
                        // The probe's output names the account: never read.
                        Some(Event::Stdout(_) | Event::Stderr(_)) => {}
                        // Its time is up: the arm above stops it.
                        None if Instant::now() >= *give_up => {}
                        None => return None,
                    }
                }
                Stage::Running(stream) => {
                    let wait = self
                        .finish_by
                        .map_or(deadline, |finish_by| deadline.min(finish_by));
                    match stream.next(wait) {
                        Some(Output::Line(line)) => self.on_line(&line),
                        Some(Output::Final(exit) | Output::Stopped(exit)) => {
                            let lost = !self.cancelled && !self.started && self.resume.is_some();
                            let update = if self.cancelled {
                                Update::Stopped
                            } else {
                                self.exited(&exit)
                            };
                            if lost && matches!(update, Update::Failed(_)) {
                                // A resumed run that ended before the turn began:
                                // the thread may be gone, or Codex may have crashed.
                                self.queue
                                    .push_back(Update::SessionLost(SessionLoss::Suspected));
                            }
                            self.end(update);
                        }
                        Some(Output::Error(_)) => {
                            let update = if self.cancelled {
                                Update::Stopped
                            } else {
                                Update::Failed(MALFORMED_OUTPUT)
                            };
                            self.end(update);
                        }
                        None => match self.finish_by {
                            // The turn is over but Codex hasn't exited.
                            Some(finish_by) if Instant::now() >= finish_by => {
                                self.finish_by = None;
                                stream.cancel(Duration::ZERO);
                            }
                            _ => return None,
                        },
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
            // The probe is harmless to kill, and nothing has started.
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
    fn a_model_is_one_argument_that_applies_to_resumed_threads_too() {
        let workspace = Path::new("/tmp/my-app-workspace");
        let args = exec_args(
            workspace,
            false,
            false,
            false,
            Some("thread-1"),
            Some("gpt-5-codex"),
        );
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        let model = args
            .iter()
            .position(|arg| *arg == "--model=gpt-5-codex")
            .unwrap();
        let resume = args.iter().position(|arg| *arg == "resume").unwrap();
        assert!(model < resume, "--model is an exec option: {args:?}");
        assert_eq!(&args[resume..], ["resume", "thread-1", "-"]);

        let default = exec_args(workspace, true, false, false, None, None);
        assert!(
            !default
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("--model"))
        );
        assert_eq!(default.last().unwrap(), "-");
    }

    #[test]
    fn ephemeral_turns_disable_codex_session_persistence() {
        let args = exec_args_for_session(
            Path::new("/tmp/my-app-workspace"),
            seatline_core::turn::SessionPolicy::Ephemeral,
            false,
            false,
            false,
            None,
            None,
        );
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
    }

    #[test]
    fn codex_s_directory_comes_first_on_its_path() {
        let (codex, inherited) = if cfg!(unix) {
            ("/opt/codex/bin/codex", "/usr/bin:/bin")
        } else {
            (r"C:\npm\codex.cmd", r"C:\Windows;C:\bin")
        };
        let path = environment::search_path_for(Path::new(codex), Some(OsStr::new(inherited)));
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], Path::new(codex).parent().unwrap());
        assert_eq!(dirs.len(), 3);
        let alone = environment::search_path_for(Path::new(codex), None);
        assert_eq!(
            std::env::split_paths(&alone).collect::<Vec<_>>(),
            [Path::new(codex).parent().unwrap()]
        );
    }
}
