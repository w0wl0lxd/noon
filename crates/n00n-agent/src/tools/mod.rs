//! Shared plumbing for tools. The registry itself lives in `registry.rs`; this file
//! holds the helpers every tool leans on: `ToolFilter` to enable/disable per caller,
//! `Deadline` so a parent tool can cap a child's timeout, the walker that skips `.git`,
//! and `sanitize_tool_input` which patches up small JSON mistakes models make (stray
//! quotes, camelCase keys, extra wrappers). Plan mode rejects writes to
//! anything but the plan file before they reach the tool.

pub mod admission;
mod file_tracker;
pub mod grep;
pub mod interpreter_bridge;
pub mod registry;
pub mod schema;
pub mod tool_search;

pub use admission::{AdmissionError, ToolAdmission, ToolAdmissionClass, ToolAdmissionGuard};
pub use file_tracker::FileReadTracker;
pub use registry::{
    ActiveTools, BoxFuture, ExecFuture, HeaderFuture, HeaderResult, ParseError, PermissionScopes,
    RegisteredTool, RegistryError, Tool, ToolAudience, ToolExecResult, ToolInvocation,
    ToolRegistry, ToolSearchResult, ToolSource, ToolsSnapshot,
};

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use humantime::format_duration;
use ignore::WalkBuilder;
use serde_json::Value;
use tracing::warn;

use crate::agent::LoadedInstructions;
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::{AgentConfig, AgentMode, EventSender, SharedBuf};
use n00n_config::{ToolOutputLines, canonical_tool_name};
use n00n_providers::RequestOptions;
use n00n_providers::provider::Provider;
use n00n_providers::{Model, ModelFamily, ModelPricing, ModelTier, OpenAiOptions};
use n00n_storage::id::SessionRef;

pub struct DescriptionContext<'a> {
    pub filter: &'a ToolFilter,
    pub audience: ToolAudience,
    pub workflow: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolFilter {
    #[default]
    All,
    Only(Vec<String>),
    AllExcept(Vec<String>),
}

impl ToolFilter {
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        let name = canonical_tool_name(name);
        match self {
            Self::All => true,
            Self::Only(allowed) => allowed.iter().any(|n| canonical_tool_name(n) == name),
            Self::AllExcept(blocked) => !blocked.iter().any(|n| canonical_tool_name(n) == name),
        }
    }

    #[must_use]
    pub fn including(self, names: impl IntoIterator<Item = String>) -> Self {
        match self {
            Self::Only(mut allowed) => {
                for name in names {
                    if !allowed
                        .iter()
                        .any(|held| canonical_tool_name(held) == canonical_tool_name(&name))
                    {
                        allowed.push(name);
                    }
                }
                Self::Only(allowed)
            }
            other => other,
        }
    }

    #[must_use]
    pub fn excluding(self, names: &[&str]) -> Self {
        if names.is_empty() {
            return self;
        }
        match self {
            Self::All => Self::AllExcept(names.iter().map(|s| (*s).to_owned()).collect()),
            Self::Only(allowed) => Self::Only(
                allowed
                    .into_iter()
                    .filter(|n| {
                        !names
                            .iter()
                            .any(|x| canonical_tool_name(x) == canonical_tool_name(n))
                    })
                    .collect(),
            ),
            Self::AllExcept(mut blocked) => {
                for &n in names {
                    if !blocked
                        .iter()
                        .any(|b| canonical_tool_name(b) == canonical_tool_name(n))
                    {
                        blocked.push(n.to_owned());
                    }
                }
                Self::AllExcept(blocked)
            }
        }
    }

    #[must_use]
    pub fn intersect(self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, other) => other.clone(),
            (current, Self::All) => current,
            (Self::Only(current_allowed), Self::Only(other_allowed)) => {
                let intersection: Vec<String> = current_allowed
                    .into_iter()
                    .filter(|n| {
                        let canonical = canonical_tool_name(n);
                        other_allowed
                            .iter()
                            .any(|o| canonical_tool_name(o) == canonical)
                    })
                    .collect();
                if intersection.is_empty() {
                    Self::Only(vec![]) // No tools allowed
                } else {
                    Self::Only(intersection)
                }
            }
            (Self::AllExcept(current_blocked), Self::AllExcept(other_blocked)) => {
                let union: Vec<String> = current_blocked
                    .into_iter()
                    .chain(other_blocked.iter().cloned())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                Self::AllExcept(union)
            }
            (Self::Only(allowed), Self::AllExcept(blocked)) => Self::Only(
                allowed
                    .into_iter()
                    .filter(|n| {
                        let canonical = canonical_tool_name(n);
                        !blocked.iter().any(|b| canonical_tool_name(b) == canonical)
                    })
                    .collect(),
            ),
            (Self::AllExcept(blocked), Self::Only(allowed)) => Self::Only(
                allowed
                    .iter()
                    .filter(|n| {
                        let canonical = canonical_tool_name(n);
                        !blocked.iter().any(|b| canonical_tool_name(b) == canonical)
                    })
                    .cloned()
                    .collect(),
            ),
        }
    }

    #[must_use]
    pub fn from_config(config: &AgentConfig, model: &Model, extra_exclude: &[&str]) -> Self {
        let base = if config.allowed_tools.is_empty() {
            Self::All
        } else {
            Self::Only(
                config
                    .allowed_tools
                    .iter()
                    .filter(|s| is_builtin_tool(s))
                    .cloned()
                    .collect(),
            )
        };
        let mut exclude: Vec<&str> = extra_exclude.to_vec();
        exclude.extend(capability_exclusions(model));
        if !config.fusion.enabled {
            exclude.push(FUSION_TOOL_NAME);
        }
        exclude.extend(
            config
                .disabled_tools
                .iter()
                .map(std::string::String::as_str),
        );
        base.excluding(&exclude)
    }
}

pub fn filter_definitions(definitions: &mut Value, filter: &ToolFilter) {
    let Some(definitions) = definitions.as_array_mut() else {
        return;
    };
    definitions.retain(|definition| {
        definition
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| filter.matches(name))
    });
}

#[must_use]
pub fn has_definition(definitions: &Value, name: &str) -> bool {
    definitions.as_array().is_some_and(|definitions| {
        definitions.iter().any(|definition| {
            definition
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|candidate| candidate == name)
        })
    })
}

/// One gate for every definitions builder (main loop, headless, Lua): a model
/// without vision never learns `view_image` exists.
#[must_use]
pub fn capability_exclusions(model: &Model) -> &'static [&'static str] {
    if model.supports_vision() {
        &[]
    } else {
        &[VIEW_IMAGE_TOOL_NAME]
    }
}

/// Deferred tools that should be active by default so new features are not
/// silently disabled. `view_image` is still filtered out for non-vision models.
#[must_use]
pub fn default_active_tools() -> ActiveTools {
    let mut active = ActiveTools::default();
    active.names.insert(AGENT_CONTROL_TOOL_NAME.to_owned());
    active.names.insert(BATCH_TOOL_NAME.to_owned());
    active.names.insert(VIEW_IMAGE_TOOL_NAME.to_owned());
    active
}

/// Build the initial main-agent tool definitions through the production filter path.
#[must_use]
pub fn runtime_tool_definitions(
    registry: &ToolRegistry,
    vars: &crate::template::Vars,
    config: &AgentConfig,
    model: &Model,
    excluded_tools: &[&str],
    workflow: bool,
) -> (Value, ToolFilter) {
    let filter = ToolFilter::from_config(config, model, excluded_tools);
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow,
    };
    let definitions = registry.definitions_active(
        vars,
        &ctx,
        model.supports_tool_examples(),
        &default_active_tools(),
    );
    (definitions, filter)
}

/// A tool is enabled unless named in `disabled_tools` (config, or the raw
/// list a Lua caller holds, e.g. `n00n.api.get_tools`).
#[must_use]
pub fn is_tool_enabled(disabled_tools: &[String], name: &str) -> bool {
    let canonical = canonical_tool_name(name);
    !disabled_tools
        .iter()
        .any(|disabled| canonical_tool_name(disabled) == canonical)
}

pub const AGENT_CONTROL_TOOL_NAME: &str = "control_agent";
pub const BATCH_TOOL_NAME: &str = "run_batch";
pub const BASH_TOOL_NAME: &str = "run_shell";
pub const CODE_EXECUTION_TOOL_NAME: &str = "run_python";
pub const EDIT_TOOL_NAME: &str = "edit_file";
pub const FUSION_TOOL_NAME: &str = "delegate_fusion";
pub const GLOB_TOOL_NAME: &str = "search_files";
pub const GREP_TOOL_NAME: &str = "search_code";
pub const MULTIEDIT_TOOL_NAME: &str = "edit_file_bulk";
pub const QUESTION_TOOL_NAME: &str = "ask_user";
pub const READ_TOOL_NAME: &str = "read_file";
pub const TASK_TOOL_NAME: &str = "run_task";
pub const TODOWRITE_TOOL_NAME: &str = "update_todo";
pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";
pub const WRITE_TOOL_NAME: &str = "write_file";

pub use n00n_config::TOOL_ALIASES;

pub(crate) const PLAN_WRITE_RESTRICTED: &str = "write restricted to plan file in plan mode";
pub(crate) const DEADLINE_EXCEEDED: &str = "timeout exceeded";

#[derive(Clone, Copy, Debug, Default)]
pub enum Deadline {
    #[default]
    None,
    At(Instant),
}

impl Deadline {
    #[must_use]
    pub fn after(duration: Duration) -> Self {
        Self::At(Instant::now() + duration)
    }

    /// Checks if the deadline has been exceeded.
    ///
    /// # Errors
    ///
    /// Returns a `String` error if the deadline has been exceeded.
    pub fn check(self) -> Result<(), String> {
        match self {
            Self::At(instant) if instant.saturating_duration_since(Instant::now()).is_zero() => {
                Err(DEADLINE_EXCEEDED.into())
            }
            Self::None | Self::At(_) => Ok(()),
        }
    }

    /// Caps the timeout to the deadline if set.
    ///
    /// # Errors
    ///
    /// Returns a `String` error if the deadline is in the past.
    pub fn cap_timeout(self, timeout_secs: u64) -> Result<u64, String> {
        match self {
            Self::None => Ok(timeout_secs),
            Self::At(instant) => {
                let remaining = instant.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(DEADLINE_EXCEEDED.into())
                } else {
                    Ok(timeout_secs.min(remaining.as_secs().max(1)))
                }
            }
        }
    }
}

#[must_use]
pub fn timeout_annotation(secs: u64) -> String {
    let d = Duration::from_secs(secs);
    let formatted: String = format_duration(d)
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    format!("{formatted} timeout")
}

pub type LocalToolFn = Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>;
pub type LocalTools = Arc<HashMap<String, LocalToolFn>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    session_id: SessionRef,
    root_session_id: SessionRef,
}

impl SessionIdentity {
    #[must_use]
    pub fn root(session_id: SessionRef) -> Self {
        Self {
            root_session_id: session_id.clone(),
            session_id,
        }
    }

    #[must_use]
    pub fn child(session_id: SessionRef, root_session_id: SessionRef) -> Self {
        Self {
            session_id,
            root_session_id,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionRef {
        &self.session_id
    }

    #[must_use]
    pub fn root_session_id(&self) -> &SessionRef {
        &self.root_session_id
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub provider: Arc<dyn Provider>,
    pub model: Arc<Model>,
    pub event_tx: EventSender,
    pub mode: Arc<AgentMode>,
    pub tool_use_id: Option<String>,
    pub user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    pub loaded_instructions: LoadedInstructions,
    pub cancel: CancelToken,
    pub mcp: Option<McpSession>,
    pub deadline: Deadline,
    pub config: Arc<AgentConfig>,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: n00n_providers::Timeouts,
    pub openai_options: OpenAiOptions,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub opts: RequestOptions,
    pub subagent_cancels: Arc<CancelMap<String>>,
    /// Immutable session and root identity of the agent executing this tool.
    pub identity: Option<SessionIdentity>,
    pub registry: Arc<ToolRegistry>,
    /// Stable identity used by registry-scoped per-agent admission. Child
    /// sessions receive their own scope, while nested calls in one agent share it.
    pub admission_scope: Arc<str>,
    pub tool_filter: ToolFilter,
    pub workflow: bool,
    pub audience: ToolAudience,
    pub local_tools: LocalTools,
    pub active_skill_policy: Option<crate::skill_policy::ActiveSkillPolicy>,
    /// Streams a dispatched child's live bufs and annotations back to the
    /// caller (`n00n.agent.call_tool` with `on_live_buf`/`on_annotation`).
    /// Never inherited: `to_tool_context` clears it, and each caller sets
    /// it for its own call only.
    pub live_sink: Option<flume::Sender<ToolLive>>,
}

/// Live progress of a dispatched child tool, streamed while it runs.
pub enum ToolLive {
    Buf(Arc<SharedBuf>),
    Annotation(String),
}

pub(crate) fn resolve_path(path: &str) -> Result<String, String> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.join(rest).to_string_lossy().into_owned()
    } else if path == "~" {
        let home = HOME.as_deref().ok_or("cannot expand ~: HOME not set")?;
        home.to_string_lossy().into_owned()
    } else {
        path.to_string()
    };

    if Path::new(&expanded).is_relative() {
        let cwd = env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
        Ok(cwd.join(&expanded).to_string_lossy().into_owned())
    } else {
        Ok(expanded)
    }
}

/// Resolves a search path, using the current directory if None.
///
/// # Errors
///
/// Returns a `String` error if the path cannot be resolved or the current directory cannot be accessed.
pub fn resolve_search_path(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(p) => resolve_path(p),
        None => env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cwd error: {e}")),
    }
}

static CWD: LazyLock<Option<PathBuf>> = LazyLock::new(|| env::current_dir().ok());
static HOME: LazyLock<Option<PathBuf>> = LazyLock::new(n00n_storage::paths::home);

pub(crate) fn relative_path(path: &str) -> String {
    let p = Path::new(path);
    if let Some(cwd) = CWD.as_deref()
        && let Ok(rel) = p.strip_prefix(cwd)
    {
        return format_rel("", ".", rel);
    }
    if let Some(home) = HOME.as_deref()
        && let Ok(rel) = p.strip_prefix(home)
    {
        return format_rel("~/", "~", rel);
    }
    path.to_string()
}

fn format_rel(prefix: &str, fallback: &str, rel: &Path) -> String {
    let s = rel.to_string_lossy();
    if s.is_empty() {
        fallback.into()
    } else {
        format!("{prefix}{s}")
    }
}

/// Convenience wrapper that always respects gitignore.
///
/// # Errors
///
/// Returns a `String` error if the root path is invalid.
pub fn walk_builder(root: &str, patterns: &[&str]) -> Result<WalkBuilder, String> {
    walk_builder_opts(root, patterns, true)
}

/// `.git` is always excluded, even when `gitignore` is false.
///
/// # Errors
/// Returns an error if the glob pattern is invalid.
pub fn walk_builder_opts(
    root: &str,
    patterns: &[&str],
    gitignore: bool,
) -> Result<WalkBuilder, String> {
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    ob.add("!.git")
        .map_err(|e| format!("invalid glob pattern for .git: {e}"))?;

    for p in patterns {
        ob.add(p)
            .map_err(|e| format!("invalid glob pattern: {e}"))?;
    }

    let overrides = ob
        .build()
        .map_err(|e| format!("invalid glob pattern: {e}"))?;

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false).overrides(overrides);
    if !gitignore {
        wb.ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false);
    }
    Ok(wb)
}

/// Expected during a directory walk: an unreadable path or a file that
/// vanished mid-walk. Anything else is a genuine I/O failure.
#[must_use]
pub fn is_expected_walk_io_error(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::PermissionDenied | ErrorKind::NotFound)
}

/// Shared classifier for every caller that iterates an
/// `ignore::Walk`/`WalkParallel` (grep, glob, Lua `fs` helpers). Expected
/// conditions should log at `debug!`; everything else stays at `warn!`.
///
/// `ignore::Error::Loop` carries no `io::Error`, so it needs its own arm rather
/// than falling through to [`is_expected_walk_io_error`].
#[must_use]
pub fn is_expected_walk_error(err: &ignore::Error) -> bool {
    match err {
        ignore::Error::Loop { .. } => true,
        ignore::Error::Io(e) => is_expected_walk_io_error(e.kind()),
        ignore::Error::WithLineNumber { err, .. }
        | ignore::Error::WithPath { err, .. }
        | ignore::Error::WithDepth { err, .. } => is_expected_walk_error(err),
        // An aggregate is only expected when every part of it is: one unexpected
        // member downgraded to `debug!` would hide a genuine I/O failure. This
        // also widens classification for a `Partial` of two-or-more expected
        // kinds together (e.g. `[PermissionDenied, NotFound]`) to `debug!`,
        // where `err.io_error()` previously returned `None` for len() != 1 and
        // left the whole aggregate at `warn!`.
        ignore::Error::Partial(errs) => !errs.is_empty() && errs.iter().all(is_expected_walk_error),
        _ => false,
    }
}

#[must_use]
pub fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| SystemTime::UNIX_EPOCH)
}

pub(crate) fn truncate_bytes(line: &str, max_bytes: usize) -> String {
    if line.len() > max_bytes {
        let boundary = line.floor_char_boundary(max_bytes);
        format!("{}...", &line[..boundary])
    } else {
        line.to_owned()
    }
}

#[must_use]
pub fn truncate_output(text: &str, max_lines: usize, max_bytes: usize) -> String {
    truncate_output_impl(text, max_lines, max_bytes, true)
}

/// [`truncate_output`] without the truncation warning. For callers that bound
/// many small pieces (per line/item) and report the truncation once themselves.
#[must_use]
pub fn truncate_output_quiet(text: &str, max_lines: usize, max_bytes: usize) -> String {
    truncate_output_impl(text, max_lines, max_bytes, false)
}

fn truncate_output_impl(text: &str, max_lines: usize, max_bytes: usize, emit_warn: bool) -> String {
    const TRUNCATED_MARKER: &str = "[truncated]";
    if max_bytes == 0 || max_lines == 0 {
        return String::new();
    }
    let mut lines = text.lines();
    let mut result = String::new();
    let mut truncated = false;

    for _ in 0..max_lines {
        let Some(line) = lines.next() else { break };
        let separator = usize::from(!result.is_empty());
        if result
            .len()
            .saturating_add(separator)
            .saturating_add(line.len())
            > max_bytes
        {
            let remaining = max_bytes.saturating_sub(result.len().saturating_add(separator));
            if separator != 0 && remaining > 0 {
                result.push('\n');
            }
            result.push_str(&line[..line.floor_char_boundary(remaining)]);
            truncated = true;
            break;
        }
        if separator != 0 {
            result.push('\n');
        }
        result.push_str(line);
    }

    if !truncated && lines.next().is_some() {
        truncated = true;
    }

    if truncated {
        let suffix = if result.is_empty() {
            TRUNCATED_MARKER.to_owned()
        } else {
            format!("\n{TRUNCATED_MARKER}")
        };
        if suffix.len() >= max_bytes {
            result.clear();
            suffix[..suffix.floor_char_boundary(max_bytes)].clone_into(&mut result);
        } else {
            let content_limit = max_bytes - suffix.len();
            result.truncate(result.floor_char_boundary(content_limit));
            result.push_str(&suffix);
        }
        if emit_warn {
            warn!(
                tool = "truncate_output",
                path = "",
                original_bytes = text.len(),
                truncated_bytes = result.len(),
                max_bytes,
                max_lines,
                "truncated tool output"
            );
        }
    }
    result
}

#[must_use]
pub fn is_builtin_tool(name: &str) -> bool {
    let canonical = canonical_tool_name(name);
    n00n_config::DEFAULT_BUILTINS
        .iter()
        .any(|builtin| canonical_tool_name(builtin) == canonical)
        || n00n_config::EDIT_SUB_TOOLS
            .iter()
            .any(|builtin| canonical_tool_name(builtin) == canonical)
        || n00n_config::TOOL_ALIASES
            .iter()
            .any(|(_, alias_canonical)| *alias_canonical == canonical)
}

#[must_use]
pub fn all_builtin_tool_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = n00n_config::DEFAULT_BUILTINS
        .iter()
        .chain(n00n_config::EDIT_SUB_TOOLS.iter())
        .map(|builtin| canonical_tool_name(builtin))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

use n00n_providers::{Message, ProviderEvent, StreamResponse, System};

struct NullProvider;

impl Provider for NullProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a System,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, crate::AgentError>> {
        Box::pin(async {
            Err(crate::AgentError::Config {
                message: "NullProvider cannot stream".to_string(),
            })
        })
    }

    fn list_models(
        &self,
    ) -> BoxFuture<'_, Result<Vec<n00n_providers::ModelInfo>, crate::AgentError>> {
        Box::pin(async { Ok(vec![]) })
    }
}

const FALLBACK_CONTEXT_WINDOW: u32 = 200_000;

fn fallback_model() -> Model {
    Model {
        id: "anthropic/claude-3-haiku-20240307".into(),
        provider: "anthropic".into(),
        tier: ModelTier::Medium,
        family: ModelFamily::Claude,
        supports_tool_examples_override: None,
        supports_thinking_override: None,
        supports_vision_override: None,
        supports_files_override: None,
        pricing: ModelPricing::ZERO,
        max_output_tokens: None,
        context_window: FALLBACK_CONTEXT_WINDOW,
        thinking_dialect: None,
        thinking_fields: None,
        body_override: None,
    }
}

/// Context for code execution tools.
pub fn interpreter_ctx(
    mode: &AgentMode,
    event_tx: &EventSender,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    registry: Arc<ToolRegistry>,
) -> ToolContext {
    static PROVIDER: LazyLock<Arc<dyn Provider>> = LazyLock::new(|| Arc::new(NullProvider));
    let model = Arc::new(
        Model::from_spec("anthropic/claude-sonnet-4-20250514")
            .or_else(|_| Model::from_spec("anthropic/claude-3-5-sonnet-20241022"))
            .or_else(|_| Model::from_spec("anthropic/claude-3-haiku-20240307"))
            .unwrap_or_else(|_| fallback_model()),
    );
    ToolContext {
        provider: Arc::clone(&PROVIDER),
        model,
        event_tx: event_tx.clone(),
        mode: Arc::new(mode.clone()),
        tool_use_id: None,
        user_response_rx,
        loaded_instructions: LoadedInstructions::new(),
        cancel,
        mcp: None,
        deadline: Deadline::None,
        config: Arc::new(AgentConfig::default()),
        tool_output_lines: ToolOutputLines::default(),
        permissions,
        timeouts: n00n_providers::Timeouts::default(),
        openai_options: OpenAiOptions::default(),
        file_tracker,
        prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
        opts: RequestOptions::default(),
        subagent_cancels: Arc::new(CancelMap::new()),
        identity: None,
        registry,
        admission_scope: crate::tools::ToolAdmission::new_scope(),
        tool_filter: ToolFilter::All,
        workflow: false,
        audience: ToolAudience::MAIN,
        local_tools: LocalTools::default(),
        active_skill_policy: None,
        live_sink: None,
    }
}

/// Minimal `ToolContext` for CLI one-shot tool execution (e.g. `n00n index`).
/// Allows everything, sends events to a dummy channel, uses no model.
#[must_use]
pub fn cli_tool_ctx() -> ToolContext {
    let (tx, _rx) = flume::unbounded::<crate::Envelope>();
    let event_tx = crate::EventSender::new(tx, 0);
    interpreter_ctx(
        &AgentMode::Build,
        &event_tx,
        CancelToken::none(),
        Arc::new(PermissionManager::new(
            n00n_config::PermissionsConfig {
                default: n00n_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        )),
        Arc::new(FileReadTracker::new()),
        None,
        Arc::clone(ToolRegistry::global_arc()),
    )
}

pub mod test_support {
    use std::borrow::Cow;

    use crate::{Envelope, EventSender, ToolOutput};

    use super::{
        AgentMode, Arc, CancelToken, DescriptionContext, FileReadTracker, LazyLock,
        PermissionManager, SessionIdentity, ToolContext, ToolRegistry, Value, interpreter_ctx,
        registry,
    };

    pub const GUARDED_TOOL_NAME: &str = "guarded_mock";

    pub struct GuardedMock;

    struct GuardedInvocation;

    impl registry::ToolInvocation for GuardedInvocation {
        fn start_header(&self) -> registry::HeaderFuture {
            registry::HeaderFuture::Ready(registry::HeaderResult::plain("mock".into()))
        }
        fn permission_scopes(&self) -> registry::BoxFuture<'_, Option<registry::PermissionScopes>> {
            Box::pin(std::future::ready(Some(
                registry::PermissionScopes::single("guarded".into()),
            )))
        }
        fn execute(self: Box<Self>, _ctx: &ToolContext) -> registry::ExecFuture<'_> {
            Box::pin(async {
                registry::ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain("ok".into())))
            })
        }
    }

    impl registry::Tool for GuardedMock {
        fn name(&self) -> &str {
            GUARDED_TOOL_NAME
        }
        fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
            "guarded mock".into()
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        }
        fn parse(
            &self,
            _input: &Value,
        ) -> Result<Box<dyn registry::ToolInvocation>, registry::ParseError> {
            Ok(Box::new(GuardedInvocation))
        }
    }

    static TEST_PERMISSIONS: LazyLock<Arc<PermissionManager>> = LazyLock::new(|| {
        Arc::new(PermissionManager::new(
            n00n_config::PermissionsConfig {
                default: n00n_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp"),
        ))
    });

    pub fn stub_ctx_with(
        mode: &AgentMode,
        event_tx: Option<&EventSender>,
        tool_use_id: Option<&str>,
    ) -> ToolContext {
        let fallback_tx;
        let event_tx = if let Some(tx) = event_tx {
            tx
        } else {
            fallback_tx = EventSender::new(flume::unbounded::<Envelope>().0, 0);
            &fallback_tx
        };
        let mut ctx = interpreter_ctx(
            mode,
            event_tx,
            CancelToken::none(),
            Arc::clone(&TEST_PERMISSIONS),
            Arc::new(FileReadTracker::new()),
            None,
            Arc::new(ToolRegistry::new()),
        );
        ctx.tool_use_id = tool_use_id.map(String::from);
        ctx.identity = Some(SessionIdentity::root(
            n00n_storage::id::SessionRef::generate(),
        ));
        ctx
    }

    #[must_use]
    pub fn stub_ctx(mode: &AgentMode) -> ToolContext {
        stub_ctx_with(mode, None, None)
    }

    #[cfg(test)]
    pub(crate) fn stub_ctx_with_permissions(
        mode: &AgentMode,
        permissions: Arc<PermissionManager>,
    ) -> ToolContext {
        let (tx, _rx) = flume::unbounded::<crate::Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let mut ctx = interpreter_ctx(
            mode,
            &event_tx,
            CancelToken::none(),
            permissions,
            Arc::new(FileReadTracker::new()),
            None,
            Arc::new(ToolRegistry::new()),
        );
        ctx.tool_use_id = None;
        ctx
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const LINE_LIMIT: usize = 500;

    #[test]
    fn root_session_identity_uses_one_id() {
        let session_id = n00n_storage::id::SessionRef::generate();
        let identity = SessionIdentity::root(session_id.clone());

        assert_eq!(identity.session_id(), &session_id);
        assert_eq!(identity.root_session_id(), &session_id);
    }

    #[test]
    fn descendant_session_identities_inherit_root_and_remain_distinct() {
        let root = SessionIdentity::root(n00n_storage::id::SessionRef::generate());
        let child = SessionIdentity::child(
            n00n_storage::id::SessionRef::generate(),
            root.root_session_id().clone(),
        );
        let grandchild = SessionIdentity::child(
            n00n_storage::id::SessionRef::generate(),
            child.root_session_id().clone(),
        );

        assert_ne!(child.session_id(), root.session_id());
        assert_ne!(grandchild.session_id(), child.session_id());
        assert_eq!(child.root_session_id(), root.root_session_id());
        assert_eq!(grandchild.root_session_id(), root.root_session_id());
    }

    #[test_case(true  ; "vision_model_keeps_view_image")]
    #[test_case(false ; "text_only_model_loses_view_image")]
    fn from_config_gates_view_image_on_vision(vision: bool) {
        let mut model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        model.supports_vision_override = Some(vision);
        let filter = ToolFilter::from_config(&AgentConfig::default(), &model, &[]);
        assert_eq!(filter.matches(VIEW_IMAGE_TOOL_NAME), vision);
        assert!(
            filter.matches(READ_TOOL_NAME),
            "unrelated tools stay enabled"
        );
    }

    #[test]
    fn from_config_hides_fusion_tool_until_enabled() {
        let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
        let mut config = AgentConfig::default();
        let disabled = ToolFilter::from_config(&config, &model, &[]);
        assert!(!disabled.matches(FUSION_TOOL_NAME));

        config.fusion.enabled = true;
        let enabled = ToolFilter::from_config(&config, &model, &[]);
        assert!(enabled.matches(FUSION_TOOL_NAME));
    }
    #[test_case(30,  "30s timeout"   ; "seconds_only")]
    #[test_case(120, "2m timeout"    ; "minutes_only")]
    #[test_case(90,  "1m30s timeout" ; "mixed")]
    fn timeout_annotation_cases(secs: u64, expected: &str) {
        assert_eq!(timeout_annotation(secs), expected);
    }

    #[test_case(Deadline::None,                          120, 120 ; "none_passes_through")]
    #[test_case(Deadline::after(Duration::from_mins(1)), 10,  10  ; "requested_under_remaining")]
    fn cap_timeout_ok(deadline: Deadline, requested: u64, expected: u64) {
        assert_eq!(deadline.cap_timeout(requested).unwrap(), expected);
    }

    #[test]
    fn cap_timeout_clamps_to_remaining() {
        let clamped = Deadline::after(Duration::from_hours(1))
            .cap_timeout(7200)
            .unwrap();
        assert!(clamped <= 3600, "expected <= 3600, got {clamped}");
    }

    #[test]
    fn cap_timeout_expired() {
        let expired = Deadline::At(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert_eq!(expired.cap_timeout(120).unwrap_err(), DEADLINE_EXCEEDED);
    }

    #[test_case("short",                            "short"                             ; "short_passthrough")]
    #[test_case(&"x".repeat(LINE_LIMIT),       &"x".repeat(LINE_LIMIT)        ; "exact_boundary")]
    #[test_case(&"x".repeat(LINE_LIMIT + 500), &format!("{}...", "x".repeat(LINE_LIMIT)) ; "long_truncated")]
    #[test_case(&format!("{}\u{1F600}", "a".repeat(LINE_LIMIT - 1)), &format!("{}...", "a".repeat(LINE_LIMIT - 1)) ; "multibyte_char_boundary")]
    fn truncate_bytes_cases(input: &str, expected: &str) {
        let result = truncate_bytes(input, LINE_LIMIT);
        assert_eq!(result, expected);
    }

    #[test_case("abcdefghij\nklmno", 2, 15, "abc\n[truncated]" ; "short_byte_limit_keeps_marker")]
    #[test_case("one\ntwo", 0, 10, "" ; "zero_line_limit")]
    #[test_case("one", 10, 0, "" ; "zero_byte_limit")]
    fn truncate_output_edge_cases(input: &str, max_lines: usize, max_bytes: usize, expected: &str) {
        assert_eq!(truncate_output(input, max_lines, max_bytes), expected);
    }

    #[test]
    fn truncate_output_respects_line_and_byte_limits() {
        const MAX_LINES: usize = 2000;
        const MAX_BYTES: usize = 50 * 1024;

        let many_lines: String = (0..MAX_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_output(&many_lines, MAX_LINES, MAX_BYTES);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() <= MAX_BYTES);

        let many_bytes = "x".repeat(MAX_BYTES + 1000);
        let result = truncate_output(&many_bytes, MAX_LINES, MAX_BYTES);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() <= MAX_BYTES);
    }

    #[test]
    fn grep_search_finds_filters_and_skips_binary() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world\ngoodbye world").unwrap();
        fs::write(dir.path().join("b.rs"), "hello rust").unwrap();
        fs::write(dir.path().join("bin.dat"), b"hello \x00 binary").unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();

        let mut params = grep::GrepParams::new("hello".into());
        params.path = Some(dir_str.clone());
        let (_, entries) = grep::grep_search(&params).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"b.rs"));
        assert!(!paths.contains(&"bin.dat"));

        let mut params = grep::GrepParams::new("hello".into());
        params.path = Some(dir_str.clone());
        params.include = Some("*.rs".into());
        let (_, entries) = grep::grep_search(&params).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "b.rs");

        let mut params = grep::GrepParams::new("zzzznotfound".into());
        params.path = Some(dir_str);
        let (_, entries) = grep::grep_search(&params).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn tool_filter_intersect_preserves_restrictions() {
        // Test that caller restrictions are preserved during Fusion lane switches
        let caller_filter = ToolFilter::Only(vec!["read".to_string(), "grep".to_string()]);
        let model_filter = ToolFilter::All;
        let result = caller_filter.intersect(&model_filter);
        assert_eq!(
            result,
            ToolFilter::Only(vec!["read".to_string(), "grep".to_string()])
        );

        // Test intersection of two Only filters
        let filter1 = ToolFilter::Only(vec![
            "read".to_string(),
            "grep".to_string(),
            "write".to_string(),
        ]);
        let filter2 = ToolFilter::Only(vec!["read".to_string(), "bash".to_string()]);
        let result = filter1.intersect(&filter2);
        assert_eq!(result, ToolFilter::Only(vec!["read".to_string()]));

        // Test Only intersected with AllExcept
        let only_filter = ToolFilter::Only(vec![
            "read".to_string(),
            "grep".to_string(),
            "bash".to_string(),
        ]);
        let except_filter = ToolFilter::AllExcept(vec!["bash".to_string()]);
        let result = only_filter.intersect(&except_filter);
        assert_eq!(
            result,
            ToolFilter::Only(vec!["read".to_string(), "grep".to_string()])
        );

        // Test AllExcept intersected with Only
        let except_filter = ToolFilter::AllExcept(vec!["bash".to_string()]);
        let only_filter = ToolFilter::Only(vec![
            "read".to_string(),
            "grep".to_string(),
            "bash".to_string(),
        ]);
        let result = except_filter.intersect(&only_filter);
        assert_eq!(
            result,
            ToolFilter::Only(vec!["read".to_string(), "grep".to_string()])
        );

        // Test AllExcept intersected with AllExcept (union of blocked)
        let except1 = ToolFilter::AllExcept(vec!["bash".to_string()]);
        let except2 = ToolFilter::AllExcept(vec!["write".to_string()]);
        let result = except1.intersect(&except2);
        assert!(matches!(result, ToolFilter::AllExcept(blocked) if blocked.len() == 2));

        // Test empty intersection
        let only1 = ToolFilter::Only(vec!["read".to_string()]);
        let only2 = ToolFilter::Only(vec!["bash".to_string()]);
        let result = only1.intersect(&only2);
        assert_eq!(result, ToolFilter::Only(vec![]));
    }

    #[test]
    fn grep_search_single_file_preserves_filename() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("demo.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let mut params = grep::GrepParams::new("fn main".into());
        params.path = Some(file.to_string_lossy().into());
        let (_, entries) = grep::grep_search(&params).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "demo.rs");
    }

    #[test]
    fn grep_search_invalid_regex_returns_error() {
        let dir = TempDir::new().unwrap();
        let mut params = grep::GrepParams::new("[invalid".into());
        params.path = Some(dir.path().to_string_lossy().into());
        let err = grep::grep_search(&params).unwrap_err();
        assert!(err.contains(grep::INVALID_REGEX), "got: {err}");
    }

    #[test_case("(?!foo)",   "look-around" ; "negative_lookahead")]
    #[test_case("(?<!foo)",  "look-around" ; "negative_lookbehind")]
    #[test_case("(a)\\1",    "backreference" ; "backreference")]
    fn grep_search_unsupported_pcre_construct_names_it(pattern: &str, expected: &str) {
        let dir = TempDir::new().unwrap();
        let mut params = grep::GrepParams::new(pattern.into());
        params.path = Some(dir.path().to_string_lossy().into());
        let err = grep::grep_search(&params).unwrap_err();
        assert!(err.contains(expected), "got: {err}");
    }

    #[test]
    fn grep_search_multiline_groups_spanning_lines() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("span.rs"), "fn foo() {\n    bar\n}\n").unwrap();

        let mut params = grep::GrepParams::new("(?s)foo.*\\n}".into());
        params.path = Some(dir.path().to_string_lossy().into());
        let (_, entries) = grep::grep_search(&params).unwrap();
        assert_eq!(entries.len(), 1);
        let lines = &entries[0].groups[0].lines;
        assert!(lines.iter().any(|l| l.text.contains("foo") && l.is_match));
    }

    #[test]
    fn grep_search_context_lines_surround_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("ctx.rs"),
            "l1\nl2\nA\nl4\nl5\nl6\nl7\nl8\nB\nl10\n",
        )
        .unwrap();

        let mut params = grep::GrepParams::new("A|B".into());
        params.path = Some(dir.path().to_string_lossy().into());
        params.context_before = 1;
        params.context_after = 1;
        let (_, entries) = grep::grep_search(&params).unwrap();
        assert_eq!(entries[0].groups.len(), 2);

        let g0 = &entries[0].groups[0].lines;
        assert!(g0.iter().any(|l| l.text == "l2" && !l.is_match));
        assert!(g0.iter().any(|l| l.text == "A" && l.is_match));

        let g1 = &entries[0].groups[1].lines;
        assert!(g1.iter().any(|l| l.text == "B" && l.is_match));
        assert!(g1.iter().any(|l| l.text == "l10" && !l.is_match));
    }

    #[test]
    fn grep_search_parallel_stable_under_repeated_calls() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let tied_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for i in 0..20u32 {
            let path = root.join(format!("f{i:03}.rs"));
            fs::write(&path, format!("needle {i}\n")).unwrap();
            let f = File::options().write(true).open(&path).unwrap();
            f.set_modified(tied_mtime).unwrap();
        }
        let path_str = root.to_string_lossy().to_string();

        let mut reference: Option<Vec<(String, usize, bool)>> = None;
        for _ in 0..20 {
            let mut params = grep::GrepParams::new("needle".into());
            params.path = Some(path_str.clone());
            params.limit = 1000;
            let (_, entries) = grep::grep_search(&params).unwrap();

            let flat: Vec<(String, usize, bool)> = entries
                .iter()
                .flat_map(|e| {
                    e.groups.iter().flat_map(|g| {
                        g.lines
                            .iter()
                            .map(|l| (e.path.clone(), l.line_nr, l.is_match))
                    })
                })
                .collect();
            match &reference {
                None => reference = Some(flat),
                Some(prev) => assert_eq!(flat, *prev),
            }
        }
    }

    #[test]
    fn grep_search_limit_truncates_groups_after_sort() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        for i in 0..10u32 {
            fs::write(root.join(format!("m_{i}.rs")), "hit\n").unwrap();
        }

        let mut params = grep::GrepParams::new("hit".into());
        params.path = Some(root.to_string_lossy().into());
        params.limit = 3;
        let (_, entries) = grep::grep_search(&params).unwrap();

        let total_groups: usize = entries.iter().map(|e| e.groups.len()).sum();
        assert_eq!(total_groups, 3);
    }

    #[test]
    fn walk_builder_excludes_dot_git_shows_dotfiles_and_filters_globs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/config"), "stuff").unwrap();
        fs::write(root.join(".git/objects/abc123"), "blob").unwrap();
        fs::write(root.join(".env"), "SECRET=42").unwrap();
        fs::write(root.join("lib.rs"), "pub fn foo() {}").unwrap();
        fs::write(root.join("main.py"), "print('hi')").unwrap();

        let root_str = root.to_string_lossy();
        let collect = |patterns: &[&str]| -> Vec<String> {
            walk_builder(&root_str, patterns)
                .unwrap()
                .build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .map(|e| {
                    e.path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        };

        let all = collect(&[]);
        assert!(all.contains(&"lib.rs".into()));
        assert!(all.contains(&".env".into()), "dotfiles must be shown");
        assert!(
            !all.iter().any(|p| p.starts_with(".git")),
            ".git must be excluded"
        );

        let rs_only = collect(&["*.rs"]);
        assert!(rs_only.contains(&"lib.rs".into()));
        assert!(!rs_only.contains(&"main.py".into()), "glob must filter");
        assert!(!rs_only.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn relative_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = n00n_storage::paths::home().unwrap();

        let cases: &[(&str, &str)] = &[
            (&format!("{}/src/main.rs", cwd.display()), "src/main.rs"),
            (&cwd.to_string_lossy(), "."),
            (
                &format!("{}/.config/something.toml", home.display()),
                "~/.config/something.toml",
            ),
            ("/etc/hosts", "/etc/hosts"),
        ];
        for (input, expected) in cases {
            assert_eq!(relative_path(input), *expected, "input: {input}");
        }

        let no_partial = format!("{}sibling/file.txt", home.display());
        assert_eq!(relative_path(&no_partial), no_partial);
    }

    #[test]
    fn resolve_path_cases() {
        let cwd = env::current_dir().unwrap();
        let home = n00n_storage::paths::home().unwrap();

        assert_eq!(
            resolve_path("~/foo/bar").unwrap(),
            home.join("foo/bar").to_string_lossy()
        );
        assert_eq!(resolve_path("~").unwrap(), home.to_string_lossy());
        assert_eq!(
            resolve_path("src/main.rs").unwrap(),
            cwd.join("src/main.rs").to_string_lossy()
        );

        // `/etc/hosts` is absolute on Unix (passed through unchanged) but
        // root-relative on Windows (no drive prefix, so `is_relative()` is
        // true and it gets joined with the current drive root, producing
        // e.g. `C:\etc\hosts`).
        #[cfg(windows)]
        {
            let expected = cwd.ancestors().last().unwrap().join("etc").join("hosts");
            assert_eq!(PathBuf::from(resolve_path("/etc/hosts").unwrap()), expected);
        }
        #[cfg(not(windows))]
        assert_eq!(resolve_path("/etc/hosts").unwrap(), "/etc/hosts");
    }

    #[test]
    fn walk_builder_opts_gitignore_false_includes_ignored() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("test.log"), "log data").unwrap();
        fs::write(root.join("test.txt"), "text data").unwrap();

        let root_str = root.to_string_lossy();

        let collect = |wb: WalkBuilder| -> Vec<String> {
            wb.build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .map(|e| e.into_path().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };

        let with_ignored = collect(walk_builder_opts(&root_str, &[], false).unwrap());
        assert!(
            with_ignored.iter().any(|p| p.ends_with("test.log")),
            "gitignore=false should include test.log, got: {with_ignored:?}"
        );
        assert!(
            with_ignored.iter().any(|p| p.ends_with("test.txt")),
            "gitignore=false should include test.txt, got: {with_ignored:?}"
        );

        let without_ignored = collect(walk_builder_opts(&root_str, &[], true).unwrap());
        assert!(
            !without_ignored.iter().any(|p| p.ends_with("test.log")),
            "gitignore=true should exclude test.log, got: {without_ignored:?}"
        );
        assert!(
            without_ignored.iter().any(|p| p.ends_with("test.txt")),
            "gitignore=true should include test.txt, got: {without_ignored:?}"
        );

        assert!(
            !with_ignored.iter().any(|p| p.contains(".git/")),
            ".git/ must be excluded even with gitignore=false, got: {with_ignored:?}"
        );
    }

    #[test]
    fn walk_builder_invalid_pattern_returns_error() {
        let tmp = TempDir::new().unwrap();
        let root_str = tmp.path().to_string_lossy();
        let err = walk_builder(&root_str, &["["]).unwrap_err();
        assert!(
            err.contains("invalid glob pattern"),
            "expected 'invalid glob pattern', got: {err}"
        );
    }

    #[test_case(ErrorKind::PermissionDenied, true  ; "permission_denied_is_expected")]
    #[test_case(ErrorKind::NotFound,         true  ; "not_found_is_expected")]
    #[test_case(ErrorKind::Other,            false ; "other_io_error_is_unexpected")]
    #[test_case(ErrorKind::TimedOut,         false ; "timed_out_is_unexpected")]
    fn expected_walk_io_error_cases(kind: ErrorKind, expected: bool) {
        assert_eq!(is_expected_walk_io_error(kind), expected);
    }

    #[test]
    fn expected_walk_error_classifies_permission_denied_io() {
        let err = ignore::Error::Io(std::io::Error::from(ErrorKind::PermissionDenied));
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_classifies_not_found_io() {
        let err = ignore::Error::Io(std::io::Error::from(ErrorKind::NotFound));
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_rejects_unexpected_io_kind() {
        let err = ignore::Error::Io(std::io::Error::from(ErrorKind::Other));
        assert!(!is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_classifies_symlink_loop() {
        let err = ignore::Error::Loop {
            ancestor: PathBuf::from("/a"),
            child: PathBuf::from("/a/b/a"),
        };
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_classifies_loop_wrapped_with_depth() {
        let err = ignore::Error::WithDepth {
            depth: 3,
            err: Box::new(ignore::Error::Loop {
                ancestor: PathBuf::from("/a"),
                child: PathBuf::from("/a/b/a"),
            }),
        };
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_classifies_permission_denied_wrapped_with_path() {
        let err = ignore::Error::WithPath {
            path: PathBuf::from("/secret"),
            err: Box::new(ignore::Error::Io(std::io::Error::from(
                ErrorKind::PermissionDenied,
            ))),
        };
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_classifies_a_wholly_expected_partial() {
        let err = ignore::Error::Partial(vec![
            ignore::Error::Loop {
                ancestor: PathBuf::from("/a"),
                child: PathBuf::from("/a/b/a"),
            },
            ignore::Error::Io(std::io::Error::from(ErrorKind::PermissionDenied)),
        ]);
        assert!(is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_rejects_a_partial_hiding_an_unexpected_member() {
        let err = ignore::Error::Partial(vec![
            ignore::Error::Loop {
                ancestor: PathBuf::from("/a"),
                child: PathBuf::from("/a/b/a"),
            },
            ignore::Error::Io(std::io::Error::from(ErrorKind::TimedOut)),
        ]);
        assert!(!is_expected_walk_error(&err));
    }

    #[test]
    fn expected_walk_error_rejects_an_empty_partial() {
        assert!(!is_expected_walk_error(&ignore::Error::Partial(vec![])));
    }

    #[test]
    fn expected_walk_error_rejects_glob_error() {
        let err = ignore::Error::Glob {
            glob: None,
            err: "bad glob".into(),
        };
        assert!(!is_expected_walk_error(&err));
    }

    #[test]
    fn all_builtin_names_no_duplicates() {
        let names = all_builtin_tool_names();
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name), "duplicate builtin tool name: {name}");
        }
    }

    #[test]
    fn default_active_tools_include_deferred_tools() {
        let active = default_active_tools();
        for name in [
            AGENT_CONTROL_TOOL_NAME,
            BATCH_TOOL_NAME,
            VIEW_IMAGE_TOOL_NAME,
        ] {
            assert!(active.names.contains(name), "missing default tool: {name}");
        }
    }

    fn parse_lua_tool_aliases(lua_src: &str) -> BTreeMap<String, String> {
        let table_start = lua_src
            .find("local TOOL_ALIASES = {")
            .expect("policy.lua must define local TOOL_ALIASES = { ... }");
        let brace_start = table_start
            + lua_src[table_start..]
                .find('{')
                .expect("missing opening brace for TOOL_ALIASES table");
        let body_start = brace_start + 1;
        let body_end = body_start
            + lua_src[body_start..]
                .find('}')
                .expect("missing closing brace for TOOL_ALIASES table");
        let body = &lua_src[body_start..body_end];

        let mut aliases = BTreeMap::new();
        for raw_line in body.lines() {
            let line = raw_line.trim().trim_end_matches(',');
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').unwrap_or_else(|| {
                panic!("unparseable TOOL_ALIASES line in policy.lua: {raw_line:?}")
            });
            let value = value
                .trim()
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or_else(|| {
                    panic!("unparseable TOOL_ALIASES value in policy.lua: {raw_line:?}")
                });
            aliases.insert(key.trim().to_string(), value.to_string());
        }
        aliases
    }

    #[test]
    fn lua_policy_aliases_exactly_match_rust_aliases() {
        let lua_src = include_str!("../../../../plugins/lib/n00n/policy.lua");
        let lua_aliases = parse_lua_tool_aliases(lua_src);

        let rust_aliases: BTreeMap<String, String> = TOOL_ALIASES
            .iter()
            .map(|(alias, canonical)| (alias.to_string(), canonical.to_string()))
            .collect();

        assert_eq!(
            lua_aliases, rust_aliases,
            "plugins/lib/n00n/policy.lua TOOL_ALIASES has drifted from n00n_config::TOOL_ALIASES"
        );
    }
}
