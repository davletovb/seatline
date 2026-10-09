//! xAI Grok adapter through one-shot Grok Build headless mode (PRO-09).
//!
//! The adapter intentionally does not run Grok as a persistent ACP/app server.
//! Every turn gets a fresh headless `grok` process, a private working
//! directory and GROK_HOME, and the application's bounded history. Only the
//! existing Grok/X OAuth file is referenced outside that directory.
//!
//! Ordinary turns expose no tools. Grok web search is deliberately not
//! advertised yet because the currently shipped headless CLI does not expose
//! the backend search surface that upstream main documents.
//! The headless stream's init event is verified before any answer text is
//! forwarded, and the private workspace (including Grok's session files and
//! the prompt file) is removed after the child has exited.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::{Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::{CachedSearchPath, SearchPath};
use seatline_core::process::{Event, Exit, Process, ProcessSpec};
use seatline_core::prompt;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, Capability, ErrorCode, ModelOption, ProviderState,
};
use seatline_core::stream::{BUSY_LIMIT, LineStream, Output};
use seatline_core::turn::{Namespace, SessionPolicy, ToolPolicy, Turn as TurnRequest};
use seatline_platform::discovery;
use seatline_platform::environment;
use seatline_platform::forget;
use seatline_platform::layout::Layout;
use seatline_platform::private_fs;
use seatline_platform::workspace;

pub mod output;

use output::Line;

pub const ID: &str = "grok";
const EXECUTABLE: &str = "grok";
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 8 * 1024;
const STATUS_OUTPUT_BYTES: usize = 16 * 1024;
const STATUS_PROBE: Duration = Duration::from_secs(10);
const FINISH_GRACE: Duration = Duration::from_secs(5);
const STALE_WORKSPACE_AFTER: Duration = Duration::from_secs(15 * 60);
const LEGACY_STALE_WORKSPACE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: Capability::Unsupported,
    continuation: Capability::Supported,
    web_search: Capability::Unsupported,
    model_selection: Capability::Supported,
    reasoning_effort: Capability::Unsupported,
    service_tier: Capability::Unsupported,
    cancellation: Capability::Supported,
    tool_isolation: Capability::Supported,
};

pub const TIMEOUTS: Timeouts = Timeouts {
    start: Duration::from_secs(60),
    idle: Duration::from_secs(300),
    max_turn: Duration::MAX,
    stop_grace: Duration::from_secs(2),
};

const NOT_INSTALLED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotFound,
    reason: "EXECUTABLE_NOT_FOUND",
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
const BOUNDARY_VIOLATION: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_BOUNDARY_VIOLATION",
    retryable: false,
};
const AUTH_MODE_REJECTED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderNotAuthenticated,
    reason: "AUTH_REJECTED",
    retryable: false,
};
const PERSISTENT_SESSION_UNSUPPORTED: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "PERSISTENT_SESSION_UNSUPPORTED",
    retryable: false,
};
const MODEL_NOT_SUPPORTED: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "MODEL_NOT_SUPPORTED",
    retryable: false,
};
const SEARCH_UNSUPPORTED: ErrorBody = ErrorBody {
    code: ErrorCode::InvalidRequest,
    reason: "SEARCH_UNSUPPORTED",
    retryable: false,
};
const MODEL_MISMATCH: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "MODEL_MISMATCH",
    retryable: false,
};
const WORKSPACE_MISMATCH: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "WORKSPACE_MISMATCH",
    retryable: false,
};
const TOOLSET_MISMATCH: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "TOOLSET_MISMATCH",
    retryable: false,
};
const SKILLS_MISMATCH: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "SKILLS_MISMATCH",
    retryable: false,
};
const MCP_MISMATCH: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "MCP_MISMATCH",
    retryable: false,
};

/// The agent of a turn, where `{name}` is its name.
const PLAIN_AGENT: &str = r#"---
name: {name}
description: Text-only Grok responder.
promptMode: full
tools: []
discoverSkills: false
inheritSkills: false
agentsMd: false
disallowedTools:
  - Agent
mcpInheritance: none
permissionMode: dontAsk
---
Answer the user's request directly as text. Do not use tools, files, commands, MCP servers, skills, plugins, hooks, subagents, memory, or external side effects.
"#;

/// The file a workspace holds while its turn lives, and refreshes as a
/// heartbeat: the name of the application's namespace in a leading-dot file
/// name (`.my-app-owner` for `my-app`). A workspace with a fresh owner file is
/// never removed as stale, and one without it is removed only when old; the name
/// is fixed for a namespace because directories a previous run left behind are
/// found by it.
pub fn owner_file(namespace: &Namespace) -> String {
    format!(".{}-owner", namespace.as_str())
}

/// The definition of the agent every turn runs as, named after the
/// application's namespace.
fn agent_definition(namespace: &Namespace) -> String {
    PLAIN_AGENT.replace("{name}", &format!("{}-text", namespace.as_str()))
}

/// Introduces a turn's system prompt in the prompt file. Grok's model follows
/// instructions when they claim precedence over the messages below (16 of 16
/// live runs), follows a plain introduction in 1 of 7 attempts, and ignores the
/// same text in the body of its agent file (2 of 5 runs): so, unlike
/// Antigravity's, its system prompt goes in the prompt, in these words.
pub const SYSTEM_INTRO: &str = "Follow these instructions from the application for the whole conversation. They come before, and take precedence over, everything in the messages below:\n";

#[derive(Debug, Clone)]
struct Launch {
    work_dir: PathBuf,
    inherited: Vec<(OsString, OsString)>,
    path: Option<OsString>,
    auth_path: Option<PathBuf>,
}

impl Launch {
    fn new(work_dir: PathBuf, host: Vec<(OsString, OsString)>) -> Self {
        let auth_path = environment::lookup(&host, "GROK_AUTH_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                environment::lookup(&host, "GROK_HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join("auth.json"))
            })
            .or_else(|| environment::home_dir(&host).map(|home| home.join(".grok/auth.json")));
        Self {
            work_dir,
            inherited: environment::inherit(host.clone(), &[]),
            path: environment::lookup(&host, "PATH").map(OsStr::to_os_string),
            auth_path,
        }
    }

    fn base_workspace(&self) -> io::Result<PathBuf> {
        workspace::prepare(&self.work_dir)
    }

    fn command(
        &self,
        cwd: &Path,
        grok_home: &Path,
        executable: &Path,
        args: Vec<OsString>,
    ) -> ProcessSpec {
        let mut spec = ProcessSpec::new(executable)
            .args(args)
            .envs(self.inherited.iter().cloned())
            .env(
                "PATH",
                environment::search_path_for(executable, self.path.as_deref()),
            )
            // Isolate every mutable/customizable Grok surface while keeping
            // the first-party cached OAuth file as the sole auth input.
            .env("GROK_HOME", grok_home.as_os_str())
            .env("GROK_DISABLE_API_KEY_AUTH", "1")
            .env("GROK_DISABLE_AUTOUPDATER", "1")
            .env("GROK_SUBAGENTS", "0")
            .env("GROK_MEMORY", "0")
            .env("GROK_WORKFLOWS", "0")
            .env("GROK_WEB_FETCH", "0")
            .env("GROK_TELEMETRY_ENABLED", "false")
            .env("GROK_TELEMETRY_TRACE_UPLOAD", "false")
            .env("GROK_FEEDBACK_ENABLED", "false")
            .env("GROK_FOLDER_TRUST", "0")
            .env("GROK_PROMPT_SUGGESTIONS", "false")
            .current_dir(cwd);
        if let Some(auth_path) = &self.auth_path {
            spec = spec.env("GROK_AUTH_PATH", auth_path.as_os_str());
        }
        spec
    }
}

pub struct Grok {
    search: CachedSearchPath,
    launch: Rc<Launch>,
    namespace: Namespace,
    timeouts: Timeouts,
    probe_timeout: Duration,
}

impl Grok {
    pub fn installed(layout: &Layout) -> Self {
        let host: Vec<_> = std::env::vars_os().collect();
        Self::new(
            layout.namespace(),
            discovery::installed(layout),
            layout.workspace(&host, "grok"),
        )
    }

    /// The adapter for the application `namespace` names, which names its
    /// agent and the owner file of its workspaces ([`owner_file`]).
    pub fn new(namespace: &Namespace, search: SearchPath, work_dir: PathBuf) -> Self {
        // A hard-killed host cannot run TurnWorkspace::drop. Clear only the
        // Grok directories the application names itself before this adapter
        // starts serving requests, so prompt/context files from an interrupted
        // prior host do not accumulate in the cache.
        if let Ok(base) = workspace::prepare(&work_dir) {
            sweep_stale_workspaces(&base, &owner_file(namespace));
        }
        Self {
            search: CachedSearchPath::new(search),
            launch: Rc::new(Launch::new(work_dir, std::env::vars_os().collect())),
            namespace: namespace.clone(),
            timeouts: TIMEOUTS,
            probe_timeout: STATUS_PROBE,
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
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Bounds readiness/model-catalog checks without changing turn limits.
    #[must_use]
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    fn executable(&self) -> Option<PathBuf> {
        self.search.find(EXECUTABLE)
    }
}

impl Provider for Grok {
    fn readiness_key(&self) -> Option<crate::readiness::Key> {
        self.launch.base_workspace().ok()?;
        crate::readiness::Key::watch(
            &self.executable()?,
            self.launch.auth_path.clone(),
            self.capabilities(),
        )
    }
    fn id(&self) -> &str {
        ID
    }

    fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    fn supports_preparation(&self) -> bool {
        true
    }

    fn invalidate_readiness(&self) {
        self.search.invalidate();
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn status(&self) -> Box<dyn Exchange> {
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::new([
                status_update(
                    Availability::NotFound,
                    Authentication::Unknown,
                    Vec::new(),
                    None,
                ),
                Update::Completed,
            ]));
        };
        let Ok(base) = self.launch.base_workspace() else {
            return Box::new(Scripted::new([
                status_update(
                    Availability::Unavailable,
                    Authentication::Unknown,
                    Vec::new(),
                    None,
                ),
                Update::Completed,
            ]));
        };
        let Ok(workspace) = ProbeWorkspace::create(&base, &owner_file(&self.namespace)) else {
            return Box::new(Scripted::new([
                status_update(
                    Availability::Unavailable,
                    Authentication::Unknown,
                    Vec::new(),
                    None,
                ),
                Update::Completed,
            ]));
        };
        let spec = self.launch.command(
            workspace.path(),
            workspace.grok_home(),
            &executable,
            vec![OsString::from("models")],
        );
        Box::new(match Process::spawn(&spec) {
            Ok(mut process) => {
                process.close_stdin();
                StatusCheck::Probing {
                    process: Box::new(process),
                    workspace: Some(workspace),
                    give_up: private_fs::after(self.probe_timeout),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }
            }
            Err(_) => StatusCheck::Done(VecDeque::from([
                status_update(
                    Availability::Unavailable,
                    Authentication::Unknown,
                    Vec::new(),
                    None,
                ),
                Update::Completed,
            ])),
        })
    }

    fn send(&self, request: TurnRequest) -> Box<dyn Exchange> {
        if request.reasoning_effort.is_some() {
            return Box::new(Scripted::failed(crate::REASONING_EFFORT_UNSUPPORTED));
        }
        if request.service_tier.is_some() {
            return Box::new(Scripted::failed(crate::SERVICE_TIER_UNSUPPORTED));
        }
        if request.session != SessionPolicy::Ephemeral {
            return Box::new(Scripted::failed(PERSISTENT_SESSION_UNSUPPORTED));
        }
        if request.validate().is_err() {
            return Box::new(Scripted::failed(crate::INVALID_TURN));
        }
        if request.tools == ToolPolicy::NativeWebSearch {
            return Box::new(Scripted::failed(SEARCH_UNSUPPORTED));
        }
        if request
            .model
            .as_deref()
            .is_some_and(|model| !model.starts_with("grok-"))
        {
            return Box::new(Scripted::failed(MODEL_NOT_SUPPORTED));
        }
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::failed(NOT_INSTALLED));
        };
        let Ok(base) = self.launch.base_workspace() else {
            return Box::new(Scripted::failed(NO_WORKSPACE));
        };

        let prompt = prompt::render_with_intro(
            SYSTEM_INTRO,
            request.system.as_deref(),
            &request.messages,
            request.tools,
        );
        let workspace = match TurnWorkspace::create(
            &base,
            &prompt,
            &owner_file(&self.namespace),
            &agent_definition(&self.namespace),
        ) {
            Ok(workspace) => workspace,
            Err(_) => return Box::new(Scripted::failed(NO_WORKSPACE)),
        };

        let args = grok_args(&workspace, request.model.as_deref());
        let expected_cwd = workspace.path().to_path_buf();
        let spec = self
            .launch
            .command(workspace.path(), workspace.grok_home(), &executable, args);
        let Ok(mut process) = Process::spawn(&spec) else {
            return Box::new(Scripted::failed(START_FAILED));
        };
        process.close_stdin();

        Box::new(Turn {
            stream: LineStream::new(process, MAX_LINE_BYTES).keeping_stderr_tail(STDERR_TAIL_BYTES),
            workspace: Some(workspace),
            queue: VecDeque::from([Update::Launched]),
            expected_cwd,
            requested_model: request.model,
            initialized: false,
            cancelled: false,
            saw_text: false,
            outcome: None,
            finish_by: None,
            result_at: None,
            done: false,
        })
    }
}

fn status_update(
    availability: Availability,
    authentication: Authentication,
    models: Vec<ModelOption>,
    sign_in: Option<seatline_core::turn::SignInClassification>,
) -> Update {
    Update::Status {
        provider_id: ID.to_owned(),
        status: ProviderState {
            availability,
            authentication,
            capabilities: CAPABILITIES,
            models: Cow::Owned(models),
            sign_in,
            readiness: None,
        },
    }
}

fn parse_models(bytes: &[u8]) -> Vec<ModelOption> {
    let text = String::from_utf8_lossy(bytes);
    let mut models = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches(['*', '-']).trim();
        let Some(id) = trimmed.split_whitespace().next() else {
            continue;
        };
        if !id.starts_with("grok-")
            || !seatline_core::turn::is_model_id(id)
            || id.len() > seatline_core::turn::MAX_MODEL_LABEL_BYTES
            || models.iter().any(|model: &ModelOption| model.id == id)
        {
            continue;
        }
        if models.len() == seatline_core::turn::MAX_MODEL_OPTIONS {
            break;
        }
        models.push(ModelOption {
            id: Cow::Owned(id.to_owned()),
            label: Cow::Owned(id.to_owned()),
        });
    }
    models
}

fn classify_sign_in(
    authentication: Authentication,
    stdout: &[u8],
) -> Option<seatline_core::turn::SignInClassification> {
    let text = String::from_utf8_lossy(stdout).to_ascii_lowercase();
    if text.contains("api key") || text.contains("deployment key") {
        Some(seatline_core::turn::SignInClassification::ApiKey)
    } else if authentication == Authentication::Authenticated {
        Some(seatline_core::turn::SignInClassification::Subscription)
    } else {
        None
    }
}

enum StatusCheck {
    Probing {
        process: Box<Process>,
        workspace: Option<ProbeWorkspace>,
        give_up: Instant,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Done(VecDeque<Update>),
}

impl Exchange for StatusCheck {
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let busy_until = deadline.max(private_fs::after(BUSY_LIMIT));
        loop {
            match self {
                Self::Done(queue) => return queue.pop_front(),
                Self::Probing {
                    process,
                    workspace,
                    give_up,
                    stdout,
                    stderr,
                } => {
                    if let Some(workspace) = workspace.as_ref() {
                        let _ = workspace.touch();
                    }
                    // A status-of-all request starts every probe up front and
                    // polls providers in sequence. If this probe already
                    // finished while another provider was being polled, drain
                    // that queued exit before applying the timeout.
                    let timed_out = Instant::now() >= *give_up;
                    let poll_until = if timed_out {
                        Instant::now()
                    } else {
                        deadline.min(*give_up)
                    };
                    match process.next_event(poll_until) {
                        Some(Event::Stdout(bytes)) => {
                            keep_head(stdout, &bytes, STATUS_OUTPUT_BYTES)
                        }
                        Some(Event::Stderr(bytes)) => {
                            private_fs::keep_tail(stderr, &bytes, STDERR_TAIL_BYTES)
                        }
                        Some(Event::Exited(exit)) => {
                            let text = String::from_utf8_lossy(stdout);
                            let error = String::from_utf8_lossy(stderr);
                            let authenticated = text.contains("You are logged in with ");
                            let unauthenticated = text.contains("You are not authenticated.")
                                || text.contains("You are using XAI_API_KEY.")
                                || text.contains("using its own API key")
                                || text.contains("deployment key")
                                || output::authentication_failure(&error);
                            let success = exit.status.is_some_and(|status| status.success());
                            let authentication = if authenticated {
                                Authentication::Authenticated
                            } else if unauthenticated {
                                Authentication::Unauthenticated
                            } else {
                                Authentication::Unknown
                            };
                            let availability = if success || unauthenticated {
                                Availability::Available
                            } else {
                                Availability::Unavailable
                            };
                            let models = if success {
                                parse_models(stdout)
                            } else {
                                Vec::new()
                            };
                            let sign_in = classify_sign_in(authentication, stdout);
                            workspace.take();
                            *self = Self::Done(VecDeque::from([
                                status_update(availability, authentication, models, sign_in),
                                Update::Completed,
                            ]));
                        }
                        None if Instant::now() >= *give_up => {
                            process.kill();
                            workspace.take();
                            *self = Self::Done(VecDeque::from([
                                status_update(
                                    Availability::Unavailable,
                                    Authentication::Unknown,
                                    Vec::new(),
                                    None,
                                ),
                                Update::Completed,
                            ]));
                        }
                        None => return None,
                    }
                }
            }
            if Instant::now() >= busy_until {
                return None;
            }
        }
    }

    fn cancel(&mut self, _grace: Duration) {
        if let Self::Probing {
            process, workspace, ..
        } = self
        {
            process.kill();
            workspace.take();
        }
        *self = Self::Done(VecDeque::from([Update::Stopped]));
    }
}

struct ProbeWorkspace {
    path: PathBuf,
    grok_home: PathBuf,
    /// The name of the owner file ([`owner_file`]).
    owner: String,
}

impl ProbeWorkspace {
    fn create(base: &Path, owner: &str) -> io::Result<Self> {
        let path = private_fs::unique_child(base, "status");
        private_fs::create_private_dir(&path)?;
        private_fs::write_private_file(&path.join(owner), b"live")?;
        let grok_home = path.join("grok-home");
        private_fs::create_private_dir(&grok_home)?;
        Ok(Self {
            path,
            grok_home,
            owner: owner.to_owned(),
        })
    }

    fn touch(&self) -> io::Result<()> {
        fs::write(self.path.join(&self.owner), b"live")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn grok_home(&self) -> &Path {
        &self.grok_home
    }
}

impl Drop for ProbeWorkspace {
    fn drop(&mut self) {
        forget::remove_in_background(self.path.clone());
    }
}

struct TurnWorkspace {
    path: PathBuf,
    grok_home: PathBuf,
    prompt: PathBuf,
    agent: PathBuf,
    /// The name of the owner file ([`owner_file`]).
    owner: String,
}

impl TurnWorkspace {
    fn create(base: &Path, prompt: &str, owner: &str, definition: &str) -> io::Result<Self> {
        let path = private_fs::unique_child(base, "turn");
        private_fs::create_private_dir(&path)?;
        // Construct the guard before any sensitive file is written so every
        // later error path removes the partial workspace.
        let workspace = Self {
            grok_home: path.join("grok-home"),
            prompt: path.join("prompt.txt"),
            agent: path.join("agent.md"),
            owner: owner.to_owned(),
            path,
        };
        workspace.initialize(prompt, definition)?;
        Ok(workspace)
    }

    fn initialize(&self, prompt: &str, definition: &str) -> io::Result<()> {
        private_fs::write_private_file(&self.path.join(&self.owner), b"live")?;
        private_fs::create_private_dir(&self.grok_home)?;
        private_fs::write_private_file(&self.prompt, prompt.as_bytes())?;
        private_fs::write_private_file(&self.agent, definition.as_bytes())
    }

    fn touch(&self) -> io::Result<()> {
        fs::write(self.path.join(&self.owner), b"live")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn grok_home(&self) -> &Path {
        &self.grok_home
    }
}

impl Drop for TurnWorkspace {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            forget::remove_in_background(self.path.clone());
        }
    }
}

fn grok_args(workspace: &TurnWorkspace, model: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--prompt-file"),
        workspace.prompt.as_os_str().to_os_string(),
        OsString::from("--verbatim"),
        OsString::from("--output-format"),
        OsString::from("streaming-messages-json"),
        OsString::from("--include-partial-messages"),
        OsString::from("--agent"),
        workspace.agent.as_os_str().to_os_string(),
        OsString::from("--no-subagents"),
        OsString::from("--no-auto-update"),
        OsString::from("--max-turns"),
        OsString::from("8"),
        OsString::from("--permission-mode"),
        OsString::from("dontAsk"),
        OsString::from("--disallowed-tools"),
        OsString::from("Agent,search_tool,use_tool"),
        // Shipped Grok always offers the MCP search/use umbrellas unless
        // explicitly denied. Clamp the built-in surface to web_search, then
        // disable that hosted tool too: the resulting turn is text-only.
        OsString::from("--tools"),
        OsString::from("web_search"),
        OsString::from("--disable-web-search"),
    ];
    if let Some(model) = model {
        args.extend([OsString::from("--model"), OsString::from(model)]);
    }
    args
}

struct Turn {
    // Process first: on Drop it dies before the private cwd is removed.
    stream: LineStream,
    workspace: Option<TurnWorkspace>,
    queue: VecDeque<Update>,
    expected_cwd: PathBuf,
    requested_model: Option<String>,
    initialized: bool,
    cancelled: bool,
    saw_text: bool,
    outcome: Option<Result<(), ErrorBody>>,
    finish_by: Option<Instant>,
    /// When Grok's final result was read: for telemetry.
    result_at: Option<Instant>,
    done: bool,
}

impl Turn {
    fn fail(&mut self, error: ErrorBody) {
        self.queue.clear();
        self.outcome = Some(Err(error));
        self.finish_by = None;
        self.stream.cancel(Duration::ZERO);
    }

    fn on_line(&mut self, line: &str) {
        if self.outcome.is_some() || self.cancelled {
            return;
        }
        match output::parse(line) {
            Err(_) => self.fail(MALFORMED_OUTPUT),
            Ok(Line::Init {
                api_key_source,
                model,
                cwd,
                tools,
                skills,
                all_mcp_disabled,
            }) => {
                if self.initialized {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if api_key_source != "oauth" {
                    return self.fail(AUTH_MODE_REJECTED);
                }
                if !model_matches(self.requested_model.as_deref(), &model) {
                    return self.fail(MODEL_MISMATCH);
                }
                if !forget::same_directory(&cwd, &self.expected_cwd) {
                    return self.fail(WORKSPACE_MISMATCH);
                }
                if !tools.is_empty() {
                    return self.fail(TOOLSET_MISMATCH);
                }
                if !skills.is_empty() {
                    return self.fail(SKILLS_MISMATCH);
                }
                if !all_mcp_disabled {
                    return self.fail(MCP_MISMATCH);
                }
                self.initialized = true;
                self.queue.push_back(Update::Started);
            }
            Ok(Line::Assistant {
                text,
                activity,
                forbidden_tool,
            }) => {
                if !self.initialized {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if forbidden_tool {
                    return self.fail(BOUNDARY_VIOLATION);
                }
                if !text.is_empty() {
                    self.saw_text = true;
                    self.queue.push_back(Update::Delta(text));
                } else if activity {
                    self.queue.push_back(Update::Activity);
                }
            }
            Ok(Line::ResultSuccess { text }) => {
                if !self.initialized {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if !self.saw_text && !text.is_empty() {
                    self.queue.push_back(Update::Delta(text));
                }
                self.result_at.get_or_insert_with(Instant::now);
                self.outcome = Some(Ok(()));
                self.finish_by = Some(private_fs::after(FINISH_GRACE));
            }
            Ok(Line::ResultFailed(error)) => {
                self.result_at.get_or_insert_with(Instant::now);
                self.fail(error)
            }
            Ok(Line::Activity) if self.initialized => self.queue.push_back(Update::Activity),
            Ok(Line::Activity | Line::Ignored) => {}
        }
    }

    fn ended(&mut self, exit: &Exit) -> Update {
        self.workspace.take();
        if self.cancelled {
            return Update::Stopped;
        }
        match self.outcome.take() {
            Some(Ok(())) => Update::Completed,
            Some(Err(error)) => Update::Failed(error),
            None if exit.status.is_some_and(|status| status.success()) => {
                Update::Failed(MALFORMED_OUTPUT)
            }
            None => {
                let failure =
                    output::provider_failure(&String::from_utf8_lossy(self.stream.stderr_tail()));
                if failure.reason == "PROVIDER_UNAVAILABLE" {
                    Update::Failed(PROCESS_EXITED)
                } else {
                    Update::Failed(failure)
                }
            }
        }
    }
}

impl Exchange for Turn {
    fn result_at(&self) -> Option<Instant> {
        self.result_at
    }

    fn next(&mut self, deadline: Instant) -> Option<Update> {
        if let Some(workspace) = self.workspace.as_ref() {
            let _ = workspace.touch();
        }
        let busy_until = deadline.max(private_fs::after(BUSY_LIMIT));
        loop {
            if let Some(update) = self.queue.pop_front() {
                return Some(update);
            }
            if self.done || Instant::now() >= busy_until {
                return None;
            }
            if self
                .finish_by
                .is_some_and(|finish_by| Instant::now() >= finish_by)
            {
                self.finish_by = None;
                self.stream.cancel(Duration::ZERO);
            }
            let wait = self
                .finish_by
                .map_or(deadline, |finish_by| deadline.min(finish_by));
            match self.stream.next(wait) {
                Some(Output::Line(line)) => self.on_line(&line),
                Some(Output::Final(exit) | Output::Stopped(exit)) => {
                    let update = self.ended(&exit);
                    self.done = true;
                    return Some(update);
                }
                Some(Output::Error(_)) => {
                    self.workspace.take();
                    self.done = true;
                    return Some(if self.cancelled {
                        Update::Stopped
                    } else {
                        Update::Failed(MALFORMED_OUTPUT)
                    });
                }
                None if self
                    .finish_by
                    .is_some_and(|finish_by| Instant::now() >= finish_by) => {}
                None => return None,
            }
        }
    }

    fn cancel(&mut self, grace: Duration) {
        if self.cancelled || self.done {
            return;
        }
        self.cancelled = true;
        self.queue.clear();
        self.finish_by = None;
        self.stream.cancel(grace);
    }
}

fn model_matches(requested: Option<&str>, actual: &str) -> bool {
    if !actual.starts_with("grok-") {
        return false;
    }
    requested.is_none_or(|requested| {
        actual == requested
            || actual
                .strip_prefix(requested)
                .and_then(|suffix| suffix.as_bytes().first())
                .is_some_and(|byte| matches!(*byte, b'-' | b'.' | b'@' | b':'))
    })
}

fn sweep_stale_workspaces(base: &Path, owner_file: &str) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !(name.starts_with("turn-") || name.starts_with("status-"))
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        let owner = path.join(owner_file);
        let stale = age_at_least(&owner, STALE_WORKSPACE_AFTER)
            || (!owner.exists() && age_at_least(&path, LEGACY_STALE_WORKSPACE_AFTER));
        if stale {
            forget::remove_in_background(path);
        }
    }
}

fn age_at_least(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed >= age)
}

fn keep_head(head: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    if head.len() >= limit {
        return;
    }
    let remaining = limit - head.len();
    head.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_arguments_keep_prompt_text_out_of_argv() {
        let workspace = TurnWorkspace {
            path: PathBuf::from("/tmp/my-app-grok"),
            grok_home: PathBuf::from("/tmp/my-app-grok/grok-home"),
            prompt: PathBuf::from("/tmp/my-app-grok/prompt.txt"),
            agent: PathBuf::from("/tmp/my-app-grok/agent.md"),
            owner: owner_file(&Namespace::fixed("my-app").unwrap()),
        };
        let args = grok_args(&workspace, Some("grok-4.6"));
        let rendered: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert!(
            rendered
                .windows(2)
                .any(|pair| pair == ["--tools", "web_search"])
        );
        assert!(
            rendered
                .windows(2)
                .any(|pair| pair == ["--model", "grok-4.6"])
        );
        assert!(rendered.contains(&std::borrow::Cow::Borrowed("streaming-messages-json")));
        assert!(
            !rendered
                .iter()
                .any(|arg| arg.contains("Current user question"))
        );
        std::mem::forget(workspace);
    }

    #[test]
    fn the_system_prompt_is_introduced_with_a_claim_of_precedence() {
        // A claim Antigravity read as an injection, and Grok follows: which is
        // why this adapter words its introduction for itself.
        assert!(SYSTEM_INTRO.contains("take precedence over"));
        assert_ne!(SYSTEM_INTRO, seatline_core::prompt::SYSTEM_INTRO);
    }

    #[test]
    fn the_owner_file_is_named_after_the_namespace() {
        assert_eq!(
            owner_file(&Namespace::fixed("my-app").unwrap()),
            ".my-app-owner"
        );
    }

    #[test]
    fn capabilities_match_the_provider_contract() {
        assert_eq!(CAPABILITIES.streaming, Capability::Unsupported);
        assert_eq!(CAPABILITIES.continuation, Capability::Supported);
        assert_eq!(CAPABILITIES.web_search, Capability::Unsupported);
        assert_eq!(CAPABILITIES.tool_isolation, Capability::Supported);
        assert_eq!(CAPABILITIES.model_selection, Capability::Supported);
        assert_eq!(CAPABILITIES.cancellation, Capability::Supported);
    }

    #[test]
    fn agent_profile_and_cli_clamps_are_text_only() {
        let agent = agent_definition(&Namespace::fixed("my-app").unwrap());
        assert!(agent.contains("name: my-app-text\n"));
        assert!(agent.contains("tools: []"));
        assert!(agent.contains("promptMode: full"));
        assert!(agent.contains("discoverSkills: false"));
        assert!(agent.contains("inheritSkills: false"));
        assert!(agent.contains("agentsMd: false"));
        assert!(agent.contains("mcpInheritance: none"));
        assert!(agent.contains("permissionMode: dontAsk"));
        assert!(agent.contains("  - Agent"));

        let workspace = TurnWorkspace {
            path: PathBuf::from("/tmp/my-app-grok"),
            grok_home: PathBuf::from("/tmp/my-app-grok/grok-home"),
            prompt: PathBuf::from("/tmp/my-app-grok/prompt.txt"),
            agent: PathBuf::from("/tmp/my-app-grok/agent.md"),
            owner: owner_file(&Namespace::fixed("my-app").unwrap()),
        };
        let args = grok_args(&workspace, None);
        let rendered: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert!(
            rendered
                .windows(2)
                .any(|pair| pair == ["--agent", "/tmp/my-app-grok/agent.md"])
        );
        assert!(
            rendered
                .windows(2)
                .any(|pair| pair == ["--disallowed-tools", "Agent,search_tool,use_tool"])
        );
        assert!(rendered.contains(&std::borrow::Cow::Borrowed("--disable-web-search")));
        std::mem::forget(workspace);
    }

    #[test]
    fn model_aliases_may_resolve_to_versioned_ids() {
        assert!(model_matches(Some("grok-4"), "grok-4-0709"));
        assert!(model_matches(Some("grok-4.6"), "grok-4.6"));
        assert!(!model_matches(Some("grok-4"), "grok-40"));
        assert!(!model_matches(Some("grok-4"), "claude-grok-4"));
    }
    #[test]
    fn live_model_catalog_is_protocol_bounded() {
        let mut input = String::new();
        for index in 0..40 {
            input.push_str(&format!("* grok-{index} (available)\n"));
        }
        input.push_str("* grok bad\n");
        input.push_str("* --danger\n");
        let models = parse_models(input.as_bytes());
        assert_eq!(models.len(), seatline_core::turn::MAX_MODEL_OPTIONS);
        assert!(
            models
                .iter()
                .all(|model| seatline_core::turn::is_model_id(&model.id))
        );
    }
}
