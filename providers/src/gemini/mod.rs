//! Google Gemini adapter through Antigravity CLI ('agy') (PRO-08).
//!
//! Each turn is a one-shot Antigravity run in a private workspace with a
//! workspace-local agent. Ordinary turns have no tools; Web turns allow only
//! 'search_web'. The application sends bounded conversation history every turn
//! instead of depending on Antigravity's native continuation state, and the
//! adapter removes the Antigravity transcript once the child has exited.

use seatline_core::work::{Permit, Worker};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::{Cleanup, Exchange, Provider, Scripted, Timeouts, Update};
use seatline_core::discovery::{CachedSearchPath, SearchPath};
use seatline_core::process::{Event, Exit, Process, ProcessSpec};
use seatline_core::prompt;
use seatline_core::protocol::Failure as ErrorBody;
use seatline_core::protocol::{
    Authentication, Availability, Capabilities, Capability, ErrorCode, ModelOption, ProviderState,
};
use seatline_core::search::{NATIVE_SEARCH_NO_SOURCES, SourceCollector, codex_message_sources};
use seatline_core::stream::{BUSY_LIMIT, LineStream, Output};
use seatline_core::turn::{
    Namespace, SessionPolicy, ToolPolicy, Turn as TurnRequest, is_cleanup_group,
};
use seatline_platform::discovery;
use seatline_platform::environment;
use seatline_platform::forget;
use seatline_platform::layout::Layout;
use seatline_platform::private_fs;
use seatline_platform::workspace;

pub mod output;

use output::Line;

pub const ID: &str = "gemini";
const EXECUTABLE: &str = "agy";
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 8 * 1024;
const STATUS_OUTPUT_BYTES: usize = 16 * 1024;
const STATUS_PROBE: Duration = Duration::from_secs(10);
const FINISH_GRACE: Duration = Duration::from_secs(5);

pub const CAPABILITIES: Capabilities = Capabilities {
    streaming: Capability::Supported,
    continuation: Capability::Supported,
    web_search: Capability::Supported,
    model_selection: Capability::Supported,
    reasoning_effort: Capability::Unsupported,
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
const AGENT_NOT_USED: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_AGENT_NOT_USED",
    retryable: false,
};
const PERMISSIONS_TOO_OPEN: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "PROVIDER_PERMISSIONS_TOO_OPEN",
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

/// The agent of a turn with no tools, where `{name}` is its name.
const PLAIN_AGENT_DEFINITION: &str = r#"---
name: {name}
description: Text-only Gemini responder with no local or external tools.
tools: []
mainAgent: true
subagent: false
inheritCustomizations: false
inheritMcp: false
commandExecutionPolicy: "off"
mcpServers: []
skills: []
plugins: []
rules: []
agents: []
hooks: []
---
# System Prompt
Answer the user's request directly as text. Do not use tools, subagents, files, commands, browsers, MCP servers, skills, plugins, hooks, or external side effects.
"#;

/// The agent of a web-search turn, where `{name}` is its name.
const SEARCH_AGENT_DEFINITION: &str = r#"---
name: {name}
description: Gemini web responder allowed to use only Antigravity's native web search.
tools:
  - search_web
mainAgent: true
subagent: false
inheritCustomizations: false
inheritMcp: false
commandExecutionPolicy: "off"
mcpServers: []
skills: []
plugins: []
rules: []
agents: []
hooks: []
---
# System Prompt
Use search_web when answering. Do not use any other tool, subagent, file, command, browser automation, MCP server, skill, plugin, hook, or external side effect. Cite the pages you use as Markdown links.
"#;

/// The names of the workspace-local agents a turn runs as: the application's
/// namespace, then `-text` for a turn with no tools and `-search` for a web
/// turn. Antigravity reports the agent it ran, and the adapter refuses a turn
/// that ran as any other.
pub fn agent_name(namespace: &Namespace, search: bool) -> String {
    format!(
        "{}-{}",
        namespace.as_str(),
        if search { "search" } else { "text" }
    )
}

/// The definition of the agent named `name`, for a turn that may search or
/// not. The application's system prompt, if it has one, ends the agent's own:
/// Antigravity treats the agent file as the system prompt it runs under, and
/// treats the same text in a user message as an attempt to override it.
fn agent_definition(name: &str, search: bool, system: Option<&str>) -> String {
    let template = if search {
        SEARCH_AGENT_DEFINITION
    } else {
        PLAIN_AGENT_DEFINITION
    };
    let mut definition = template.replace("{name}", name);
    if let Some(system) = system.filter(|system| !system.is_empty()) {
        definition.push('\n');
        definition.push_str(system);
        definition.push('\n');
    }
    definition
}

type PendingCleanups = Rc<std::cell::RefCell<HashMap<String, Vec<String>>>>;

#[derive(Debug, Clone)]
struct Launch {
    work_dir: PathBuf,
    inherited: Vec<(OsString, OsString)>,
    path: Option<OsString>,
    home: Option<PathBuf>,
}

impl Launch {
    fn new(work_dir: PathBuf, host: Vec<(OsString, OsString)>) -> Self {
        Self {
            work_dir,
            inherited: environment::inherit(host.clone(), &[]),
            path: environment::lookup(&host, "PATH").map(OsStr::to_os_string),
            home: environment::home_dir(&host),
        }
    }

    fn base_workspace(&self) -> io::Result<PathBuf> {
        workspace::prepare(&self.work_dir)
    }

    fn command(&self, cwd: &Path, executable: &Path, args: Vec<OsString>) -> ProcessSpec {
        ProcessSpec::new(executable)
            .args(args)
            .envs(self.inherited.iter().cloned())
            .env(
                "PATH",
                environment::search_path_for(executable, self.path.as_deref()),
            )
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .current_dir(cwd)
    }
}

pub struct Gemini {
    search: CachedSearchPath,
    launch: Rc<Launch>,
    namespace: Namespace,
    /// The application's cleanup records, kept across restarts.
    cleanup_dir: Option<PathBuf>,
    timeouts: Timeouts,
    probe_timeout: Duration,
    pending_cleanups: PendingCleanups,
    cleanup_worker: Option<Arc<Worker>>,
}

impl Gemini {
    pub fn installed(layout: &Layout) -> Self {
        let host: Vec<_> = std::env::vars_os().collect();
        let mut gemini = Self::new(
            layout.namespace(),
            discovery::installed(layout),
            layout.workspace(&host, "antigravity"),
        );
        gemini.cleanup_dir = layout.data_dir().map(|dir| dir.join("gemini-cleanups"));
        gemini
    }

    /// The adapter for the application `namespace` names, which gives its
    /// agents their names ([`agent_name`]).
    pub fn new(namespace: &Namespace, search: SearchPath, work_dir: PathBuf) -> Self {
        let cleanup_dir = Some(work_dir.with_extension("cleanups"));
        Self {
            search: CachedSearchPath::new(search),
            launch: Rc::new(Launch::new(work_dir, std::env::vars_os().collect())),
            namespace: namespace.clone(),
            cleanup_dir,
            timeouts: TIMEOUTS,
            probe_timeout: STATUS_PROBE,
            pending_cleanups: Rc::new(std::cell::RefCell::new(HashMap::new())),
            cleanup_worker: None,
        }
    }

    /// Override the shared bounded filesystem pool, for isolated runtimes or
    /// fault-injection tests. A turn reserves its cleanup before launch.
    #[must_use]
    pub fn with_cleanup_worker(mut self, worker: Arc<Worker>) -> Self {
        self.cleanup_worker = Some(worker);
        self
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

    /// Overrides the durable cleanup-record directory (used by isolated tests).
    #[must_use]
    pub fn with_cleanup_dir(mut self, cleanup_dir: PathBuf) -> Self {
        self.cleanup_dir = Some(cleanup_dir);
        self
    }

    fn executable(&self) -> Option<PathBuf> {
        self.search.find(EXECUTABLE)
    }
}

impl Provider for Gemini {
    fn readiness_key(&self) -> Option<crate::readiness::Key> {
        self.launch.base_workspace().ok()?;
        let files = self.launch.home.clone().into_iter().flat_map(|home| {
            [
                home.join(".gemini/oauth_creds.json"),
                home.join(".gemini/settings.json"),
                home.join(".gemini/antigravity-cli/auth.json"),
                home.join(".gemini/antigravity-cli/config.json"),
            ]
        });
        crate::readiness::Key::watch(&self.executable()?, files, self.capabilities())
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
        let Ok(workspace) = self.launch.base_workspace() else {
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
        let spec = self
            .launch
            .command(&workspace, &executable, vec![OsString::from("models")]);
        Box::new(match Process::spawn(&spec) {
            Ok(mut process) => {
                process.close_stdin();
                StatusCheck::Probing {
                    process,
                    give_up: private_fs::after(self.probe_timeout),
                    stdout: Vec::new(),
                    stderr_tail: Vec::new(),
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
        if request.session != SessionPolicy::Ephemeral {
            return Box::new(Scripted::failed(PERSISTENT_SESSION_UNSUPPORTED));
        }
        if request.validate().is_err() {
            return Box::new(Scripted::failed(crate::INVALID_TURN));
        }
        if request
            .model
            .as_deref()
            .is_some_and(|model| !model.starts_with("gemini-"))
        {
            return Box::new(Scripted::failed(MODEL_NOT_SUPPORTED));
        }
        let native_search = request.tools == ToolPolicy::NativeWebSearch;
        let Some(executable) = self.executable() else {
            return Box::new(Scripted::failed(NOT_INSTALLED));
        };
        let Ok(base) = self.launch.base_workspace() else {
            return Box::new(Scripted::failed(NO_WORKSPACE));
        };
        let agent = agent_name(&self.namespace, native_search);
        let worker = match self.cleanup_worker.as_deref() {
            Some(worker) => worker,
            None => match Worker::cleanup() {
                Ok(worker) => worker,
                Err(_) => return Box::new(Scripted::failed(CLEANUP_BACKLOG_FULL)),
            },
        };
        let cleanup_permit = match worker.reserve() {
            Ok(permit) => permit,
            Err(_) => return Box::new(Scripted::failed(CLEANUP_BACKLOG_FULL)),
        };
        let workspace =
            match TurnWorkspace::create(&base, &agent, native_search, request.system.as_deref()) {
                Ok(workspace) => workspace,
                Err(_) => return Box::new(Scripted::failed(NO_WORKSPACE)),
            };

        let cleanup_group = request
            .cleanup_group
            .clone()
            .unwrap_or_else(|| UNGROUPED.to_owned());
        // Persist before launch, away from the hub. A second reservation covers
        // initialization; the original permit remains reserved for deletion.
        let (marker, persistence) = match self.cleanup_dir.clone() {
            Some(dir) => {
                let marker = match workspace_marker(&dir, &cleanup_group, workspace.path()) {
                    Ok(marker) => marker,
                    Err(_) => return Box::new(Scripted::failed(CLEANUP_FAILED)),
                };
                let permit = match worker.reserve() {
                    Ok(permit) => permit,
                    Err(_) => return Box::new(Scripted::failed(CLEANUP_BACKLOG_FULL)),
                };
                let path = workspace.path().to_owned();
                let group = cleanup_group.clone();
                let result = match permit
                    .submit(move || record_workspace(&dir, &group, &path).map(|_| ()))
                {
                    Ok(result) => result,
                    Err(_) => return Box::new(Scripted::failed(CLEANUP_FAILED)),
                };
                (Some(marker), Some(result))
            }
            None => (None, None),
        };
        // The system prompt is in the agent, not in the prompt.
        let prompt = prompt::render(None, &request.messages, request.tools);
        let input = serde_json::json!({
            "event": "user",
            "message": { "content": prompt }
        })
        .to_string()
            + "
";
        let args = agy_args(&agent, request.model.as_deref());
        let spec = self.launch.command(workspace.path(), &executable, args);
        Box::new(Turn {
            stream: None,
            pending_launch: Some(PendingLaunch {
                persistence,
                spec,
                input,
            }),
            cleanup_permit: Some(cleanup_permit),
            cleanup_result: None,
            terminal: None,
            marker,
            workspace: Some(workspace),
            home: self.launch.home.clone(),
            cleanup_dir: self.cleanup_dir.clone(),
            pending_cleanups: Rc::clone(&self.pending_cleanups),
            queue: VecDeque::new(),
            cleanup_group,
            expected_agent: agent,
            initialized: false,
            antigravity_conversation: None,
            cancelled: false,
            native_search,
            searched: false,
            saw_delta: false,
            answer: String::new(),
            held: String::new(),
            held_step_done: false,
            answer_steps: HashSet::new(),
            answer_step: None,
            step_text: String::new(),
            messages: 0,
            sources: SourceCollector::new(ID),
            outcome: None,
            finish_by: None,
            done: false,
        })
    }

    fn cleanup_group(&self, group: &str) -> Cleanup {
        if !is_cleanup_group(group) {
            return Cleanup::nothing();
        }
        let memory_ids = self
            .pending_cleanups
            .borrow()
            .get(group)
            .cloned()
            .unwrap_or_default();
        let home = self.launch.home.clone();
        let cleanup_dir = self.cleanup_dir.clone();
        let pending = Rc::clone(&self.pending_cleanups);
        let workspace_base = self.launch.work_dir.clone();
        let group = group.to_owned();
        let work_group = group.clone();
        Cleanup::new(
            move || {
                let mut first_error = None;
                if let Some(dir) = cleanup_dir.as_deref() {
                    let group_dir = cleanup_conversation_dir(dir, &work_group)?;
                    match fs::read_dir(&group_dir) {
                        Ok(entries) => {
                            let base = fs::canonicalize(workspace::prepare(&workspace_base)?)?;
                            for entry in entries {
                                let entry = match entry {
                                    Ok(entry) => entry,
                                    Err(error) => {
                                        first_error.get_or_insert(error);
                                        continue;
                                    }
                                };
                                let kind = match entry.file_type() {
                                    Ok(kind) => kind,
                                    Err(error) => {
                                        first_error.get_or_insert(error);
                                        continue;
                                    }
                                };
                                if !kind.is_file()
                                    || !entry
                                        .file_name()
                                        .to_string_lossy()
                                        .starts_with("workspace-")
                                {
                                    continue;
                                }
                                let result = (|| {
                                    let bytes = fs::read(entry.path())?;
                                    let workspace: PathBuf = match serde_json::from_slice(&bytes) {
                                        Ok(path) => path,
                                        Err(error) => {
                                            quarantine_marker(dir, &work_group, &entry.path())?;
                                            return Err(io::Error::other(error));
                                        }
                                    };
                                    if let Err(error) =
                                        validate_cleanup_workspace(&workspace, &base)
                                    {
                                        // Keep invalid evidence outside the active group. It
                                        // must never block other markers or touch that path.
                                        if matches!(
                                            error.kind(),
                                            io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                                        ) {
                                            quarantine_marker(dir, &work_group, &entry.path())?;
                                        }
                                        return Err(error);
                                    }
                                    clean_runtime(
                                        home.as_deref(),
                                        Some(dir),
                                        &work_group,
                                        &workspace,
                                        None,
                                    )?;
                                    forget::remove(&workspace)?;
                                    forget::remove(&entry.path())
                                })();
                                if let Err(error) = result {
                                    first_error.get_or_insert(error);
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
                let mut ids = memory_ids;
                if let Some(dir) = cleanup_dir.as_deref() {
                    ids.extend(read_pending_cleanup_ids(dir, &work_group)?);
                }
                ids.sort();
                ids.dedup();
                if !ids.is_empty() {
                    let home = home.as_deref().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "home directory unavailable for Antigravity transcript cleanup",
                        )
                    })?;
                    for id in &ids {
                        match remove_antigravity_transcript(home, id) {
                            Ok(()) => {
                                if let Some(dir) = cleanup_dir.as_deref() {
                                    if let Err(error) =
                                        forget_cleanup_id_record(dir, &work_group, id)
                                    {
                                        first_error.get_or_insert(error);
                                    }
                                }
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                }
                if let Some(error) = first_error {
                    return Err(error);
                }
                if let Some(dir) = cleanup_dir.as_deref() {
                    forget_cleanup_record(dir, &work_group)?;
                }
                Ok(())
            },
            move || {
                pending.borrow_mut().remove(&group);
            },
        )
    }
}

fn validate_cleanup_workspace(path: &Path, canonical_base: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid cleanup workspace"))?;
    if !path
        .file_name()
        .is_some_and(|s| s.to_string_lossy().starts_with("turn-"))
        || fs::canonicalize(parent)? != canonical_base
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup workspace outside app scope",
        ));
    }
    // Validate the stored form too, but only after proving it names our base.
    // Canonicalizing both existing parents handles Windows case, separators
    // and verbatim prefixes without changing the string used in transcripts.
    workspace::prepare(parent)?;
    Ok(())
}

fn quarantine_marker(base: &Path, group: &str, marker: &Path) -> io::Result<()> {
    let dir = base.join(".quarantine").join(group);
    private_fs::create_private_dir(&dir)?;
    let name = marker
        .file_name()
        .ok_or_else(|| io::Error::other("invalid marker"))?;
    fs::rename(marker, dir.join(name))
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
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let (id, label) = line.split_once('\t')?;
            let id = id.trim();
            let label = label.trim();
            (id.starts_with("gemini-")
                && seatline_core::turn::is_model_id(id)
                && !label.is_empty()
                && label.chars().count() <= seatline_core::turn::MAX_MODEL_LABEL_BYTES)
                .then(|| ModelOption {
                    id: Cow::Owned(id.to_owned()),
                    label: Cow::Owned(label.to_owned()),
                })
        })
        .take(seatline_core::turn::MAX_MODEL_OPTIONS)
        .collect()
}

enum StatusCheck {
    Probing {
        process: Process,
        give_up: Instant,
        stdout: Vec<u8>,
        stderr_tail: Vec<u8>,
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
                    give_up,
                    stdout,
                    stderr_tail,
                } => {
                    // Even after the nominal deadline, first consume an exit
                    // that is already queued. This avoids killing a probe that
                    // finished while another provider status check was polled.
                    let timed_out = Instant::now() >= *give_up;
                    let poll_until = if timed_out {
                        Instant::now()
                    } else {
                        deadline.min(*give_up)
                    };
                    match process.next_event(poll_until) {
                        Some(Event::Stdout(bytes)) => {
                            crate::keep_bounded_output(stdout, &bytes, STATUS_OUTPUT_BYTES);
                        }
                        Some(Event::Stderr(bytes)) => {
                            private_fs::keep_tail(stderr_tail, &bytes, STDERR_TAIL_BYTES);
                        }
                        Some(Event::Exited(exit)) => {
                            let success = exit.status.is_some_and(|status| status.success());
                            let authentication = if success {
                                Authentication::Authenticated
                            } else if output::authentication_failure(&String::from_utf8_lossy(
                                stderr_tail,
                            )) {
                                Authentication::Unauthenticated
                            } else {
                                Authentication::Unknown
                            };
                            let availability =
                                if success || authentication == Authentication::Unauthenticated {
                                    Availability::Available
                                } else {
                                    Availability::Unavailable
                                };
                            let models = if success {
                                parse_models(stdout)
                            } else {
                                Vec::new()
                            };
                            let sign_in = (authentication == Authentication::Authenticated)
                                .then_some(seatline_core::turn::SignInClassification::Cloud);
                            *self = Self::Done(VecDeque::from([
                                status_update(availability, authentication, models, sign_in),
                                Update::Completed,
                            ]));
                        }
                        None if Instant::now() >= *give_up => {
                            process.kill();
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
        if let Self::Probing { process, .. } = self {
            process.kill();
        }
        *self = Self::Done(VecDeque::from([Update::Stopped]));
    }
}

struct TurnWorkspace {
    path: PathBuf,
}

impl TurnWorkspace {
    fn create(base: &Path, agent: &str, search: bool, system: Option<&str>) -> io::Result<Self> {
        let path = private_fs::unique_child(base, "turn");
        private_fs::create_private_dir(&path)?;
        let definition = agent_definition(agent, search, system);
        let agent_dir = path.join(".agents/agents").join(agent);
        private_fs::create_private_dir(&agent_dir)?;
        private_fs::write_private_file(&agent_dir.join("agent.md"), definition.as_bytes())?;
        let hooks_dir = path.join(".agents");
        private_fs::create_private_dir(&hooks_dir)?;
        private_fs::write_private_file(&hooks_dir.join("hooks.json"), b"{}\n")?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TurnWorkspace {
    fn drop(&mut self) {
        let _ = forget::remove(&self.path);
    }
}

struct PendingLaunch {
    persistence: Option<Receiver<io::Result<()>>>,
    spec: ProcessSpec,
    input: String,
}

struct Turn {
    // Keep the process before the workspace so dropping a live turn stops the
    // child before its cwd is removed.
    stream: Option<LineStream>,
    pending_launch: Option<PendingLaunch>,
    cleanup_permit: Option<Permit>,
    cleanup_result: Option<Receiver<io::Result<()>>>,
    terminal: Option<Update>,
    marker: Option<PathBuf>,
    workspace: Option<TurnWorkspace>,
    home: Option<PathBuf>,
    cleanup_dir: Option<PathBuf>,
    pending_cleanups: PendingCleanups,
    queue: VecDeque<Update>,
    /// Groups this turn's cleanup records with the others of its group.
    cleanup_group: String,
    expected_agent: String,
    initialized: bool,
    antigravity_conversation: Option<String>,
    cancelled: bool,
    native_search: bool,
    searched: bool,
    saw_delta: bool,
    answer: String,
    /// One Antigravity agent-response step held until the next step reveals
    /// whether it was search narration.
    held: String,
    held_step_done: bool,
    /// Indices of the agent-response steps seen, so a later update that
    /// doesn't name its type still counts as answer text only for them.
    answer_steps: HashSet<u64>,
    /// The agent-response step being written, and its text so far.
    answer_step: Option<u64>,
    step_text: String,
    messages: usize,
    sources: SourceCollector,
    outcome: Option<Result<(), ErrorBody>>,
    finish_by: Option<Instant>,
    done: bool,
}

impl Turn {
    /// Do not launch until the crash-recovery marker is durable. Startup errors
    /// use the same deletion boundary as normal turns, including a child that
    /// started but could not accept its first input.
    fn launch(&mut self, deadline: Instant) -> bool {
        let Some(launch) = self.pending_launch.take() else {
            return true;
        };
        if let Some(result) = &launch.persistence {
            match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(())) => {}
                Err(RecvTimeoutError::Timeout) => {
                    self.pending_launch = Some(launch);
                    return false;
                }
                _ => {
                    self.terminal = Some(Update::Failed(CLEANUP_FAILED));
                    self.cleanup_runtime();
                    return true;
                }
            }
        }
        let mut process = match Process::spawn(&launch.spec) {
            Ok(process) => process,
            Err(_) => {
                self.terminal = Some(Update::Failed(START_FAILED));
                self.cleanup_runtime();
                return true;
            }
        };
        let failed = process.write(launch.input.as_bytes()).is_err();
        process.close_stdin();
        self.stream =
            Some(LineStream::new(process, MAX_LINE_BYTES).keeping_stderr_tail(STDERR_TAIL_BYTES));
        if failed {
            self.terminal = Some(Update::Failed(START_FAILED));
            self.cleanup_runtime();
        } else {
            self.queue.push_back(Update::Launched);
        }
        true
    }

    fn fail(&mut self, error: ErrorBody) {
        self.queue.clear();
        self.outcome = Some(Err(error));
        self.finish_by = Some(Instant::now());
    }

    fn register_cleanup_id(&mut self, id: String) {
        {
            let mut pending = self.pending_cleanups.borrow_mut();
            let ids = pending.entry(self.cleanup_group.clone()).or_default();
            if !ids.iter().any(|candidate| candidate == &id) {
                ids.push(id.clone());
            }
        }
        if let Some(dir) = self.cleanup_dir.as_deref() {
            let _ = record_pending_cleanup_id(dir, &self.cleanup_group, &id);
        }
    }

    fn clear_cleanup_ids(&mut self) {
        if let Some(id) = &self.antigravity_conversation {
            let mut pending = self.pending_cleanups.borrow_mut();
            if let Some(ids) = pending.get_mut(&self.cleanup_group) {
                ids.retain(|candidate| candidate != id);
                if ids.is_empty() {
                    pending.remove(&self.cleanup_group);
                }
            }
        }
    }

    fn on_line(&mut self, line: &str) {
        if self.outcome.is_some() || self.cancelled {
            return;
        }
        match output::parse(line) {
            Err(_) => self.fail(MALFORMED_OUTPUT),
            Ok(Line::Init {
                conversation_id,
                permission_mode,
                agent,
            }) => {
                if self.initialized {
                    return self.fail(BOUNDARY_VIOLATION);
                }

                // Capture and persist the provider transcript ID before any
                // boundary check. Even an unsafe init can already have written
                // the prompt/page context to brain/<id>.
                self.antigravity_conversation = Some(conversation_id.clone());
                self.register_cleanup_id(conversation_id);

                if agent != self.expected_agent {
                    return self.fail(AGENT_NOT_USED);
                }
                if !matches!(
                    permission_mode.as_str(),
                    "request-review" | "proceed-in-sandbox" | "strict"
                ) {
                    return self.fail(PERMISSIONS_TOO_OPEN);
                }
                self.initialized = true;
                self.queue.push_back(Update::Started);
            }
            Ok(Line::AgentDelta { index, text, done }) => {
                if let Some(index) = index {
                    self.answer_steps.insert(index);
                }
                self.answer_delta(index, text, done);
            }
            Ok(Line::Untyped { index, text, done }) => {
                if index.is_some_and(|index| self.answer_steps.contains(&index)) {
                    self.answer_delta(index, text, done);
                } else if !self.initialized {
                    self.fail(MALFORMED_OUTPUT);
                } else {
                    self.queue.push_back(Update::Activity);
                }
            }
            Ok(Line::OtherStep { .. }) => {
                // The prompt echoed back, a system message, or an unclassified
                // step: not answer text, and nothing the adapter lets act. The
                // prompt is echoed before or after `init`, so either is fine.
                self.queue.push_back(Update::Activity);
            }
            Ok(Line::Tool(tool)) => {
                if !self.initialized {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if self.native_search && tool == "search_web" {
                    // Text immediately before any search is narration, not
                    // durable answer/history.
                    self.held.clear();
                    self.held_step_done = false;
                    self.searched = true;
                    self.queue.push_back(Update::Activity);
                } else {
                    self.fail(BOUNDARY_VIOLATION);
                }
            }
            Ok(Line::Subagent | Line::UnexpectedStep) => self.fail(BOUNDARY_VIOLATION),
            Ok(Line::ResultSuccess {
                conversation_id,
                response,
                usage,
            }) => {
                if !self.initialized {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if conversation_id
                    .as_deref()
                    .is_some_and(|id| self.antigravity_conversation.as_deref() != Some(id))
                {
                    return self.fail(MALFORMED_OUTPUT);
                }
                if self.native_search {
                    self.flush_held();
                }
                if !self.saw_delta && (!self.native_search || self.searched) && !response.is_empty()
                {
                    if self.native_search {
                        self.show_search_message(response);
                    } else {
                        self.show_text(response);
                    }
                }
                if self.native_search && self.searched {
                    for result in codex_message_sources(&self.answer) {
                        if let Some(source) = self.sources.push(result) {
                            self.queue.push_back(Update::Source(source));
                        }
                    }
                }
                if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                    self.queue.push_back(Update::Usage(usage));
                }
                let outcome = if self.native_search && (!self.searched || self.sources.count() == 0)
                {
                    Err(NATIVE_SEARCH_NO_SOURCES)
                } else {
                    Ok(())
                };
                self.outcome = Some(outcome);
                self.finish_by = Some(private_fs::after(FINISH_GRACE));
            }
            Ok(Line::ResultFailed(error)) => self.fail(error),
            Ok(Line::Ignored) => {}
        }
    }

    /// Answer text from agent-response step `index`.
    fn answer_delta(&mut self, index: Option<u64>, text: String, done: bool) {
        if !self.initialized {
            return self.fail(MALFORMED_OUTPUT);
        }
        if index != self.answer_step {
            self.answer_step = index;
            self.step_text.clear();
        }
        // A DONE update normally carries the last fragment; one that repeats
        // the step's whole text shows only what's new, never a second copy.
        let text = if done && !self.step_text.is_empty() && text.starts_with(&self.step_text) {
            text[self.step_text.len()..].to_owned()
        } else {
            text
        };
        if done {
            self.answer_step = None;
            self.step_text.clear();
        } else {
            self.step_text.push_str(&text);
        }
        if !self.native_search {
            self.show_text(text);
        } else {
            // Each response step is held until the next step. If a search
            // follows, it was narration and is dropped. If a second response
            // starts, the previous one was answer text.
            if self.held_step_done {
                self.flush_held();
            }
            self.held.push_str(&text);
            self.held_step_done = done;
        }
    }

    fn flush_held(&mut self) {
        self.held_step_done = false;
        if self.held.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.held);
        self.show_search_message(text);
    }

    fn show_search_message(&mut self, mut text: String) {
        if text.is_empty() {
            return;
        }
        if self.messages > 0 && !self.answer.is_empty() {
            text.insert_str(0, "\n\n");
        }
        self.messages += 1;
        self.show_text(text);
    }

    fn show_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.saw_delta = true;
        self.answer.push_str(&text);
        self.queue.push_back(Update::Delta(text));
    }

    /// The process is gone before this job can scan or remove its transcript.
    /// Keep the terminal boundary pending until deletion succeeds or fails.
    fn cleanup_runtime(&mut self) {
        let launched = self.stream.is_some();
        self.stream.take();
        let persistence = self
            .pending_launch
            .take()
            .and_then(|launch| launch.persistence);
        let Some(workspace) = self.workspace.take() else {
            return;
        };
        let home = self.home.clone();
        let dir = self.cleanup_dir.clone();
        let group = self.cleanup_group.clone();
        let id = self.antigravity_conversation.clone();
        let marker = self.marker.take();
        if let Some(permit) = self.cleanup_permit.take() {
            self.cleanup_result = permit
                .submit(move || {
                    // A cancelled/dropped pre-launch turn must wait for its
                    // marker job before deleting that marker and workspace.
                    if let Some(result) = persistence {
                        let _ = result.recv();
                    }
                    let result = if launched {
                        clean_runtime(
                            home.as_deref(),
                            dir.as_deref(),
                            &group,
                            workspace.path(),
                            id.as_deref(),
                        )
                    } else {
                        Ok(())
                    };
                    if result.is_ok() {
                        forget::remove(workspace.path())?;
                        if let Some(marker) = marker {
                            forget::remove(&marker)?;
                        }
                    }
                    result
                })
                .ok();
        }
        if self.cleanup_result.is_none() {
            self.terminal = Some(Update::Failed(CLEANUP_FAILED));
        }
    }

    fn ended(&mut self, exit: &Exit) -> Update {
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
                let stderr = self
                    .stream
                    .as_ref()
                    .map_or(&[][..], LineStream::stderr_tail);
                let failure = output::provider_failure(&String::from_utf8_lossy(stderr));
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
    fn next(&mut self, deadline: Instant) -> Option<Update> {
        let busy_until = deadline.max(private_fs::after(BUSY_LIMIT));
        loop {
            if let Some(update) = self.queue.pop_front() {
                return Some(update);
            }
            if let Some(result) = &self.cleanup_result {
                match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(result) => {
                        self.cleanup_result = None;
                        self.done = true;
                        if result.is_ok() {
                            self.clear_cleanup_ids();
                        }
                        return Some(if result.is_ok() {
                            self.terminal.take().unwrap_or(Update::Stopped)
                        } else {
                            Update::Failed(CLEANUP_FAILED)
                        });
                    }
                    Err(RecvTimeoutError::Timeout) => return None,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.cleanup_result = None;
                        self.done = true;
                        return Some(Update::Failed(CLEANUP_FAILED));
                    }
                }
            }
            if self.terminal.is_some() {
                self.done = true;
                return self.terminal.take();
            }
            if self.done || Instant::now() >= busy_until {
                return None;
            }
            if self.pending_launch.is_some() {
                if !self.launch(deadline) {
                    return None;
                }
                continue;
            }
            let Some(stream) = self.stream.as_mut() else {
                self.done = true;
                return Some(Update::Failed(CLEANUP_FAILED));
            };
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
                    let update = self.ended(&exit);
                    self.terminal = Some(update);
                    self.cleanup_runtime();
                    continue;
                }
                Some(Output::Error(_)) => {
                    self.outcome = Some(Err(MALFORMED_OUTPUT));
                    let update = if self.cancelled {
                        Update::Stopped
                    } else {
                        Update::Failed(MALFORMED_OUTPUT)
                    };
                    self.terminal = Some(update);
                    self.cleanup_runtime();
                    continue;
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
        if self.pending_launch.is_some() {
            self.terminal = Some(Update::Stopped);
            self.cleanup_runtime();
        }
        if let Some(stream) = &mut self.stream {
            stream.cancel(grace);
        }
        // Cancellation during cleanup must still await its deletion boundary.
        if self.cleanup_result.is_some() {
            self.terminal = Some(Update::Stopped);
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if self.workspace.is_some() {
            self.cleanup_runtime();
        }
    }
}

const CLEANUP_FAILED: ErrorBody = ErrorBody {
    code: ErrorCode::InternalError,
    reason: seatline_core::protocol::CLEANUP_FAILED,
    retryable: true,
};
const CLEANUP_BACKLOG_FULL: ErrorBody = ErrorBody {
    code: ErrorCode::ProviderFailed,
    reason: "CLEANUP_BACKLOG_FULL",
    retryable: true,
};

fn write_cleanup_record(path: &Path, bytes: &[u8]) -> io::Result<()> {
    private_fs::write_private_file(path, bytes)?;
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

fn record_workspace(base: &Path, group: &str, workspace: &Path) -> io::Result<PathBuf> {
    let marker = workspace_marker(base, group, workspace)?;
    private_fs::create_private_dir(marker.parent().unwrap())?;
    write_cleanup_record(&marker, &serde_json::to_vec(workspace)?)?;
    Ok(marker)
}

fn workspace_marker(base: &Path, group: &str, workspace: &Path) -> io::Result<PathBuf> {
    let name = workspace
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::other("invalid workspace"))?;
    let dir = cleanup_conversation_dir(base, group)?;
    Ok(dir.join(format!("workspace-{name}.json")))
}

fn clean_runtime(
    home: Option<&Path>,
    dir: Option<&Path>,
    group: &str,
    workspace: &Path,
    known: Option<&str>,
) -> io::Result<()> {
    let home = home.ok_or_else(|| io::Error::other("home unavailable for cleanup"))?;
    let ids = match known {
        Some(id) => vec![id.to_owned()],
        None => antigravity_transcripts_for_workspace(home, workspace)?,
    };
    let mut first_error = None;
    for id in ids {
        if let Some(dir) = dir {
            record_pending_cleanup_id(dir, group, &id)?;
        }
        if let Err(error) = remove_antigravity_transcript(home, &id) {
            first_error.get_or_insert(error);
        } else if let Some(dir) = dir {
            forget_cleanup_id_record(dir, group, &id)?;
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn agy_args(agent: &str, model: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--input-format"),
        OsString::from("stream-json"),
        OsString::from("--output-format"),
        OsString::from("stream-json"),
        OsString::from("--print-timeout"),
        OsString::from("300s"),
        OsString::from("--sandbox"),
        OsString::from("--agent"),
        OsString::from(agent),
    ];
    if let Some(model) = model {
        args.extend([OsString::from("--model"), OsString::from(model)]);
    }
    args
}

fn remove_antigravity_transcript(home: &Path, id: &str) -> io::Result<()> {
    if !output::is_conversation_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe Antigravity conversation id",
        ));
    }
    // Antigravity keeps a turn in two places: the transcript under `brain`, and
    // the conversation itself, prompt included, in a SQLite database of the
    // same ID under `conversations` (with the files SQLite keeps beside it
    // while it writes). Each is removed even when another can't be, and the
    // first failure is what the retry hears of.
    let root = home.join(".gemini").join("antigravity-cli");
    let mut first_error = None;
    for path in [
        root.join("brain").join(id),
        root.join("conversations").join(format!("{id}.db")),
        root.join("conversations").join(format!("{id}.db-wal")),
        root.join("conversations").join(format!("{id}.db-shm")),
        root.join("conversations").join(format!("{id}.db-journal")),
    ] {
        if let Err(error) = forget::remove(&path) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// The cleanup group of turns whose application names none.
const UNGROUPED: &str = "ungrouped";
const TRANSCRIPT_SCAN_BUDGET: usize = 512;
const TRANSCRIPT_SCAN_BYTES: usize = 64 * 1024;

fn cleanup_conversation_dir(base: &Path, conversation_id: &str) -> io::Result<PathBuf> {
    if !is_cleanup_group(conversation_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe cleanup group",
        ));
    }
    Ok(base.join(conversation_id))
}

fn record_pending_cleanup_id(base: &Path, conversation_id: &str, id: &str) -> io::Result<()> {
    if !output::is_conversation_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe Antigravity conversation id",
        ));
    }
    let dir = cleanup_conversation_dir(base, conversation_id)?;
    private_fs::create_private_dir(&dir)?;
    write_cleanup_record(&dir.join(id), b"pending\n")
}

fn read_pending_cleanup_ids(base: &Path, conversation_id: &str) -> io::Result<Vec<String>> {
    let dir = cleanup_conversation_dir(base, conversation_id)?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if output::is_conversation_id(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn forget_cleanup_id_record(base: &Path, conversation_id: &str, id: &str) -> io::Result<()> {
    if !output::is_conversation_id(id) {
        return Ok(());
    }
    let dir = cleanup_conversation_dir(base, conversation_id)?;
    forget::remove(&dir.join(id))
}

fn forget_cleanup_record(base: &Path, conversation_id: &str) -> io::Result<()> {
    let dir = cleanup_conversation_dir(base, conversation_id)?;
    forget::remove(&dir)
}

fn antigravity_brain(home: &Path) -> PathBuf {
    home.join(".gemini").join("antigravity-cli").join("brain")
}

/// Finds only transcripts that prove they came from this turn's unique
/// private workspace. This is the fallback when cancellation/malformed
/// output prevents the adapter from consuming Antigravity's init event.
fn antigravity_transcripts_for_workspace(home: &Path, workspace: &Path) -> io::Result<Vec<String>> {
    let brain = antigravity_brain(home);
    let entries = match fs::read_dir(&brain) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let raw = workspace.to_string_lossy().into_owned();
    let escaped = serde_json::to_string(&raw)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned();
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !output::is_conversation_id(&id) {
            continue;
        }
        let mut budget = TRANSCRIPT_SCAN_BUDGET;
        if transcript_tree_mentions(&entry.path(), &raw, &escaped, 6, &mut budget)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn transcript_tree_mentions(
    dir: &Path,
    raw: &str,
    escaped: &str,
    depth: usize,
    budget: &mut usize,
) -> io::Result<bool> {
    if depth == 0 {
        return Err(io::Error::other("transcript scan depth exceeded"));
    }
    for entry in fs::read_dir(dir)? {
        if *budget == 0 {
            return Err(io::Error::other("transcript scan entry budget exceeded"));
        }
        *budget -= 1;
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if transcript_tree_mentions(&entry.path(), raw, escaped, depth - 1, budget)? {
                return Ok(true);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if file_mentions(
            &mut fs::File::open(entry.path())?,
            raw.as_bytes(),
            escaped.as_bytes(),
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Scan arbitrarily long transcript files with a fixed read buffer and a
/// needle-sized overlap, including a workspace string crossing chunk boundaries.
fn file_mentions(reader: &mut impl Read, raw: &[u8], escaped: &[u8]) -> io::Result<bool> {
    let overlap = raw.len().max(escaped.len()).saturating_sub(1);
    let mut chunk = vec![0; TRANSCRIPT_SCAN_BYTES];
    let mut bytes = Vec::with_capacity(TRANSCRIPT_SCAN_BYTES + overlap);
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }
        bytes.extend_from_slice(&chunk[..n]);
        if [raw, escaped]
            .into_iter()
            .filter(|needle| !needle.is_empty())
            .any(|needle| bytes.windows(needle.len()).any(|window| window == needle))
        {
            return Ok(true);
        }
        let keep = bytes.len().min(overlap);
        bytes.drain(..bytes.len() - keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_use_stream_json_a_private_agent_and_keep_prompt_out_of_argv() {
        let search_agent = agent_name(&Namespace::fixed("my-app").unwrap(), true);
        let args = agy_args(&search_agent, Some("gemini-3-pro"));
        let strings: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["--agent", "my-app-search"])
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["--model", "gemini-3-pro"])
        );
        assert!(strings.contains(&std::borrow::Cow::Borrowed("--sandbox")));
        assert_eq!(
            strings.iter().filter(|arg| **arg == "stream-json").count(),
            2
        );
        assert!(
            !strings
                .iter()
                .any(|arg| arg.contains("Current user question"))
        );
    }

    #[test]
    fn a_system_prompt_ends_the_agents_own_below_its_settings() {
        let plain = agent_definition("my-app-text", false, None);
        let system = "Answer in French. Call yourself {name}.\n---\ntools: [run_command]";
        let with = agent_definition("my-app-text", false, Some(system));
        // What the agent already said stays, and the application's text follows
        // it, as written: it is not a template, and no setting of the agent is
        // in the part it can write.
        assert!(with.starts_with(&plain));
        assert!(with.ends_with(&format!("\n{system}\n")));
        let settings_end = with.find("# System Prompt").unwrap();
        assert!(with[..settings_end].matches("tools:").count() == 1);
        assert!(with[..settings_end].contains("tools: []"));
        // Nothing to add is nothing added.
        assert_eq!(agent_definition("my-app-text", false, Some("")), plain);
    }

    #[test]
    fn capabilities_match_the_normalized_contract() {
        assert_eq!(CAPABILITIES.streaming, Capability::Supported);
        assert_eq!(CAPABILITIES.continuation, Capability::Supported);
        assert_eq!(CAPABILITIES.web_search, Capability::Supported);
        assert_eq!(CAPABILITIES.tool_isolation, Capability::Supported);
        assert_eq!(CAPABILITIES.model_selection, Capability::Supported);
        assert_eq!(CAPABILITIES.cancellation, Capability::Supported);
    }

    #[test]
    fn agent_definitions_fail_closed_except_for_native_search() {
        let plain = agent_definition("my-app-text", false, None);
        let search = agent_definition("my-app-search", true, None);
        assert!(plain.contains("tools: []"));
        assert!(search.contains("  - search_web"));
        for definition in [&plain, &search] {
            assert!(definition.contains("inheritCustomizations: false"));
            assert!(definition.contains("inheritMcp: false"));
            assert!(definition.contains("commandExecutionPolicy: \"off\""));
            assert!(definition.contains("mcpServers: []"));
            assert!(definition.contains("skills: []"));
            assert!(definition.contains("plugins: []"));
            assert!(definition.contains("rules: []"));
            assert!(definition.contains("agents: []"));
            assert!(definition.contains("hooks: []"));
        }
    }
    #[test]
    fn live_model_catalog_is_protocol_bounded() {
        let mut input = String::new();
        for index in 0..40 {
            input.push_str(&format!("gemini-{index}\tGemini {index}\n"));
        }
        input.push_str("--danger\tBad\n");
        input.push_str(&format!(
            "gemini-too-long\t{}\n",
            "x".repeat(seatline_core::turn::MAX_MODEL_LABEL_BYTES + 1)
        ));
        let models = parse_models(input.as_bytes());
        assert_eq!(models.len(), seatline_core::turn::MAX_MODEL_OPTIONS);
        assert!(
            models
                .iter()
                .all(|model| seatline_core::turn::is_model_id(&model.id))
        );
        assert!(
            models
                .iter()
                .all(|model| model.label.chars().count()
                    <= seatline_core::turn::MAX_MODEL_LABEL_BYTES)
        );
    }
    #[test]
    fn workspace_matching_reads_past_the_old_size_limit_and_across_chunks() {
        let needle = b"/private/app/turn-012345";
        let mut bytes = vec![b'x'; 2 * 1024 * 1024 + TRANSCRIPT_SCAN_BYTES - 5];
        bytes.extend_from_slice(needle);
        assert!(file_mentions(&mut bytes.as_slice(), needle, b"escaped").unwrap());
        assert!(!file_mentions(&mut b"other workspace".as_slice(), needle, b"escaped").unwrap());
    }

    #[test]
    fn a_scan_that_exhausted_its_budget_reports_failure_instead_of_claiming_deletion() {
        let root = std::env::temp_dir().join(format!(
            "seatline-scan-{}",
            private_fs::unique_child(std::path::Path::new(""), "test").display()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("transcript"), "other app").unwrap();
        assert!(transcript_tree_mentions(&root, "needle", "escaped", 6, &mut 0).is_err());
        assert!(transcript_tree_mentions(&root, "needle", "escaped", 0, &mut 10).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
