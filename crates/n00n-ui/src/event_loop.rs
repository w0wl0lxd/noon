//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`InputReader`]), so the loop
//! waits on every event source at once and wakes the moment a plugin action,
//! agent event, or keypress arrives instead of sleeping in `event::poll`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::eyre;

use crossterm::event::{
    Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use n00n_agent::command::CustomCommand;
use n00n_agent::permissions::PermissionManager;
use n00n_agent::{
    AgentConfig, CancelToken, McpCommand, McpConfigErrors, McpHandle, mcp,
    tools::{SessionIdentity, truncate_output},
};
use n00n_config::UiConfig;
use n00n_lua::{
    EventHandle, HintReader, KeymapReader, LuaCommandReader, SessionReply, SessionRequest, UiAction,
};
use n00n_providers::Timeouts;
use n00n_providers::provider::{
    Provider, fetch_all_models, from_model_with_openai_options, unconfigured_provider,
};
use n00n_providers::{
    ContentBlock, Message, Model, ModelCatalog, ModelCatalogError, OpenAiOptions,
};
use n00n_storage::StateDir;
use n00n_storage::StorageError;
use n00n_storage::id::{SessionRef, n00nId, n00nIdParseError};
use n00n_storage::sessions::{
    CompactionStateError, RetentionBudget, SessionError, StoredDirectTool, StoredSessionLifecycle,
    StoredSessionStateSnapshot, TranscriptEntry, normalize_title,
};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppSession;
use crate::agent::{
    AgentCommand, AgentHandles, ModelSlot, RevisionAllocator, shared_queue::QueueItem,
};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::{App, AppInit, Msg, QueuedMessage, SubmitOutcome, message_tool_use_ids};
use crate::components::input::Submission;
use crate::components::usage_modal::UsageFetchState;
use crate::components::{
    Action, DisplayMessage, DisplayRole, ExitRequest, Status, SubmissionDispatch,
};
use crate::input::InputReader;
use crate::session_lineage::{LineageError, LineageLimits, LiveSession, SessionLineageGuard};

use crate::color_compat;
use crate::storage_writer::StorageWriter;
use crate::terminal;
use crate::terminal_image;
use ratatui_image::picker::Picker;

const ANIMATION_INTERVAL_MS: u64 = 16;
const IDLE_POLL_INTERVAL_MS: u64 = 100;
const PERIODIC_SAVE_INTERVAL: Duration = Duration::from_secs(1);
/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
/// Max queued matching mouse events coalesced per event-loop turn.
const COALESCE_BUDGET: usize = 64;
/// Max input events handled per wake before yielding to rendering and other channels.
const HANDLE_INPUT_BUDGET: usize = 64;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const STORAGE_WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const TERMINAL_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMPACTION_CHECKPOINT_ATTEMPTS: u8 = 3;
const COMPACTION_SHUTDOWN_TIMEOUT_ERROR: &str = "compaction checkpoint timed out during shutdown";
const STORAGE_WRITER_REFS_ERR: &str =
    "storage writer has outstanding references, skipping graceful shutdown";
const DIRECT_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const DELETE_FOCUSED_ERR: &str = "cannot delete the focused session";
const DELETE_UI_ONLY_ERR: &str = "session deletion is available only from trusted UI controls";
const NOT_LIVE_ERR: &str = "session not live";
const TEAM_TOOL_NAME: &str = "team";
const PAUSED_TEAM_RUN_ID_MAX_BYTES: usize = 256;

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<AppSession>,
    pub focused: usize,
}

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub sessions: Vec<AppSession>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub project_trusted: bool,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub retention_budget: RetentionBudget,
    pub snapshot_timeout: std::time::Duration,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub openai_options: OpenAiOptions,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub ui_action_rx: Option<flume::Receiver<UiAction>>,
    pub lua_event_handle: Option<EventHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Idle => "idle",
        }
    }

    fn needs_shutdown_settle(self) -> bool {
        self == Self::Working
    }
}

fn parse_session_id(id: &str) -> Result<n00nId, String> {
    id.parse().map_err(|e: n00nIdParseError| e.to_string())
}

fn caller_session_id(caller: Option<SessionRef>) -> Result<n00nId, String> {
    caller
        .map(|session| session.id())
        .ok_or_else(|| "session caller identity is required".to_owned())
}

fn authorize_ui_delete(
    caller: Option<&SessionRef>,
    trusted_ui_control: bool,
) -> Result<(), String> {
    if caller.is_some() || !trusted_ui_control {
        return Err(DELETE_UI_ONLY_ERR.to_owned());
    }
    Ok(())
}
fn live_session(session: &AppSession) -> std::result::Result<LiveSession, LineageError> {
    let root_session_id = match (session.meta.parent_id, session.meta.root_session_id) {
        (Some(_), None) => return Err(LineageError::MissingRoot(session.id)),
        (_, Some(root_session_id)) => root_session_id,
        (None, None) => session.id,
    };
    Ok(LiveSession {
        id: session.id,
        root_session_id,
        parent_id: session.meta.parent_id,
        runtime_present: true,
        execution_active: session.meta.lifecycle.is_active(),
    })
}

fn has_restorable_work(session: &AppSession) -> bool {
    !session.meta.queued_messages.is_empty()
        || !session.meta.queued_submissions.is_empty()
        || !session.meta.queued_direct_tools.is_empty()
}
fn cancel_stored_session(session: &mut AppSession) -> bool {
    let had_work = session.meta.lifecycle.is_active()
        || !session.meta.queued_messages.is_empty()
        || !session.meta.queued_submissions.is_empty()
        || !session.meta.queued_direct_tools.is_empty();
    if !had_work {
        return false;
    }
    session.meta.lifecycle = StoredSessionLifecycle::Cancelled;
    session.meta.queued_messages.clear();
    session.meta.queued_submissions.clear();
    session.meta.queued_direct_tools.clear();
    session.meta.direct_paused_team = None;
    session.updated_at = n00n_storage::now_epoch();
    true
}

fn bounded_direct_output(text: &str, config: &AgentConfig) -> String {
    truncate_output(
        text,
        config.max_output_lines,
        config.max_output_bytes.min(DIRECT_OUTPUT_MAX_BYTES),
    )
}

fn delete_sessions_sequentially(
    writer: &Arc<StorageWriter>,
    mut targets: Vec<n00nId>,
    reply_tx: flume::Sender<SessionReply>,
) {
    let Some(target) = targets.pop() else {
        let _ = reply_tx.send(Ok(json!(true)));
        return;
    };
    let next_writer = Arc::clone(writer);
    writer.delete(target, move |result| match result {
        Ok(()) | Err(SessionError::Storage(StorageError::NotFound(_))) => {
            delete_sessions_sequentially(&next_writer, targets, reply_tx);
        }
        Err(error) => {
            let _ = reply_tx.send(Err(error.to_string()));
        }
    });
}

fn resolved_root(
    start: n00nId,
    parents: &HashMap<n00nId, Option<n00nId>>,
) -> std::result::Result<n00nId, LineageError> {
    let mut current = start;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return Err(LineageError::Cycle(current));
        }
        let parent = parents
            .get(&current)
            .ok_or(LineageError::UnknownSession(current))?;
        let Some(parent) = parent else {
            return Ok(current);
        };
        if !parents.contains_key(parent) {
            return Err(LineageError::MissingParent {
                id: current,
                parent: *parent,
            });
        }
        current = *parent;
    }
}

fn session_identity(session: &AppSession) -> std::result::Result<SessionIdentity, LineageError> {
    let live = live_session(session)?;
    let session_id = SessionRef::from(live.id);
    if live.id == live.root_session_id {
        Ok(SessionIdentity::root(session_id))
    } else {
        Ok(SessionIdentity::child(
            session_id,
            SessionRef::from(live.root_session_id),
        ))
    }
}

fn capture_revision_matches(snapshot: &StoredSessionStateSnapshot, revision: u64) -> bool {
    snapshot.state_revision() == Some(revision)
}

fn state_revision_or_initial(
    snapshot: Option<&n00n_storage::sessions::StoredSessionStateSnapshot>,
) -> u64 {
    let Some(snapshot) = snapshot else {
        return 0;
    };
    let Some(revision) = snapshot.state_revision() else {
        return 0;
    };
    revision
}

fn current_state_revision(session: &AppSession) -> u64 {
    state_revision_or_initial(session.meta.state_snapshot.as_ref()).max(session.meta.revision)
}

fn persisted_root_id(session: &AppSession) -> n00nId {
    match session.meta.root_session_id {
        Some(root_id) => root_id,
        None => session.id,
    }
}

fn initial_state_revision(session: &AppSession, root: Option<&AppSession>) -> u64 {
    let session_revision = current_state_revision(session);
    root.map_or(session_revision, current_state_revision)
        .max(session_revision)
}

fn outer_compaction_boundary_revision(transcript: &[TranscriptEntry<Message>]) -> Option<u64> {
    match transcript.first() {
        Some(TranscriptEntry::Compaction { state_revision, .. }) => *state_revision,
        _ => None,
    }
}

fn resume_state_snapshot(
    session: &AppSession,
    referenced_revision: Option<u64>,
) -> std::result::Result<Option<StoredSessionStateSnapshot>, String> {
    let Some(revision) = referenced_revision else {
        return Ok(session.meta.state_snapshot.clone());
    };
    let latest = session.meta.state_snapshot.clone();
    if state_revision_or_initial(latest.as_ref()) >= revision {
        return Ok(latest);
    }
    match session.meta.compaction_state_at(revision) {
        Ok(snapshot) => Ok(Some(snapshot.clone())),
        Err(
            error @ (CompactionStateError::MissingRevision { .. }
            | CompactionStateError::FutureRevision { .. }),
        ) => {
            warn!(
                session_id = %session.id,
                checkpoint_revision = revision,
                %error,
                "compaction state checkpoint unavailable; using latest plugin state"
            );
            Ok(latest)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
fn outer_compaction_state_revision(
    transcript: &[TranscriptEntry<Message>],
) -> std::result::Result<u64, String> {
    match transcript.first() {
        Some(TranscriptEntry::Compaction {
            state_revision: Some(revision),
            ..
        }) => Ok(*revision),
        Some(TranscriptEntry::Compaction { .. }) => {
            Err("compaction boundary has no plugin state revision".to_owned())
        }
        _ => Err("compaction transcript has no outer boundary".to_owned()),
    }
}

fn capture_state_at_revision(
    handle: Option<&EventHandle>,
    session: &AppSession,
    revision: u64,
) -> std::result::Result<StoredSessionStateSnapshot, String> {
    let Some(handle) = handle else {
        let Some(mut snapshot) = session.meta.state_snapshot.clone() else {
            return Ok(StoredSessionStateSnapshot::new(revision));
        };
        snapshot
            .set_state_revision(revision)
            .map_err(|error| error.to_string())?;
        return Ok(snapshot);
    };
    let identity = session_identity(session).map_err(|error| error.to_string())?;
    let snapshot = handle
        .capture_state(&identity, revision)
        .map_err(|error| error.to_string())?;
    if !capture_revision_matches(&snapshot, revision) {
        return Err(format!(
            "captured plugin state revision does not match compaction revision {revision}"
        ));
    }
    Ok(snapshot)
}

fn apply_compaction_checkpoint(
    session: &mut AppSession,
    revision: u64,
    snapshot: StoredSessionStateSnapshot,
) -> std::result::Result<(), String> {
    let next_session_revision = session
        .meta
        .revision
        .checked_add(1)
        .ok_or_else(|| "session revision exhausted".to_owned())?;
    let protected = n00n_storage::sessions::transcript_referenced_revisions(&session.transcript);
    match session
        .meta
        .checkpoint_compaction_state_with_protected(snapshot.clone(), &protected)
    {
        Ok(()) => {}
        Err(
            CompactionStateError::UnsupportedSchemaVersion { .. }
            | CompactionStateError::InvalidEnvelope,
        ) => {
            warn!(session_id = %session.id, checkpoint_revision = revision, "compaction checkpoint metadata is unusable; compacting without an exact checkpoint");
        }
        Err(error) => return Err(error.to_string()),
    }
    if state_revision_or_initial(session.meta.state_snapshot.as_ref()) <= revision {
        session.meta.state_snapshot = Some(snapshot);
    }
    session.meta.revision = next_session_revision;
    session.updated_at = n00n_storage::now_epoch();
    Ok(())
}

#[cfg(test)]
fn prepare_compaction_checkpoint(
    handle: Option<&EventHandle>,
    session: &mut AppSession,
    revision: u64,
) -> std::result::Result<(), String> {
    let snapshot = capture_state_at_revision(handle, session, revision)?;
    apply_compaction_checkpoint(session, revision, snapshot)
}

async fn prepare_compaction_checkpoint_async(
    handle: Option<&EventHandle>,
    session: &mut AppSession,
    revision: u64,
) -> std::result::Result<(), String> {
    let snapshot = match handle {
        Some(handle) => {
            let identity = session_identity(session).map_err(|error| error.to_string())?;
            handle
                .capture_state_async(&identity, revision)
                .await
                .map_err(|error| error.to_string())?
        }
        None => capture_state_at_revision(None, session, revision)?,
    };
    if !capture_revision_matches(&snapshot, revision) {
        return Err(format!(
            "captured plugin state revision does not match compaction revision {revision}"
        ));
    }
    apply_compaction_checkpoint(session, revision, snapshot)
}

fn merge_compaction_metadata(
    target: &mut AppSession,
    checkpoint: &AppSession,
    revision: u64,
) -> std::result::Result<(), String> {
    let snapshot = checkpoint
        .meta
        .compaction_state_at(revision)
        .cloned()
        .map_err(|error| error.to_string())?;
    let protected = n00n_storage::sessions::transcript_referenced_revisions(&target.transcript);
    target
        .meta
        .checkpoint_compaction_state_with_protected(snapshot, &protected)
        .map_err(|error| error.to_string())?;
    if state_revision_or_initial(target.meta.state_snapshot.as_ref())
        <= state_revision_or_initial(checkpoint.meta.state_snapshot.as_ref())
    {
        target
            .meta
            .state_snapshot
            .clone_from(&checkpoint.meta.state_snapshot);
    }
    target.meta.revision = target.meta.revision.max(checkpoint.meta.revision);
    target.updated_at = target.updated_at.max(checkpoint.updated_at);
    Ok(())
}

fn warn_lineage_cleanup<T>(
    result: std::result::Result<T, LineageError>,
    session_id: n00nId,
    action: &'static str,
) {
    if let Err(error) = result {
        warn!(%session_id, %error, action, "session lineage cleanup failed");
    }
}

fn validated_paused_team_payload(payload: &Value) -> Option<Value> {
    if payload.get("paused").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let run_id = payload.get("run_id")?.as_str()?;
    if run_id.is_empty() || run_id.len() > PAUSED_TEAM_RUN_ID_MAX_BYTES {
        return None;
    }
    let mode = match payload.get("mode") {
        Some(mode) => Some(mode.as_str()?),
        None => None,
    };
    if mode.is_some_and(|mode| !matches!(mode, "supervised" | "autonomous" | "swarm")) {
        return None;
    }
    let mut validated = json!({ "paused": true, "run_id": run_id });
    if let Some(mode) = mode {
        validated["mode"] = Value::String(mode.to_owned());
    }
    Some(validated)
}

fn paused_team_payload(content: &str) -> Option<Value> {
    if !content.trim_start().starts_with('{') {
        return None;
    }
    let payload: Value = match serde_json::from_str(content) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(%error, "invalid paused team result; ignoring");
            return None;
        }
    };
    validated_paused_team_payload(&payload)
}

fn direct_paused_team_payload(tool: &str, content: &str) -> Option<Value> {
    if tool == TEAM_TOOL_NAME {
        paused_team_payload(content)
    } else {
        None
    }
}

fn paused_team_run(history: &[Message]) -> Option<Value> {
    let (user_index, last_user) = history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| matches!(message.role, n00n_providers::Role::User))?;
    for block in &last_user.content {
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } = block
        else {
            continue;
        };
        let is_team_result = history[..user_index].iter().rev().any(|message| {
            message
                .tool_uses()
                .any(|(id, name, _)| id == tool_use_id && name == TEAM_TOOL_NAME)
        });
        if !is_team_result {
            continue;
        }

        if let Some(payload) = paused_team_payload(content) {
            return Some(payload);
        }
    }

    None
}

struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    last_status: SessionStatus,
    direct_bootstrap_active: bool,
    pending_compactions: VecDeque<PendingCompaction>,
    deferred_agent_events: VecDeque<Box<n00n_agent::Envelope>>,
}

struct PreparedCompaction {
    revision: u64,
    session: AppSession,
    root: Option<AppSession>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactionPersistStage {
    Ready,
    Capturing,
    PersistingRoot,
    RootDurable,
    PersistingSession,
}

struct PendingCompaction {
    ack: Box<n00n_agent::Envelope>,
    transcript: Option<Arc<Vec<TranscriptEntry<Message>>>>,
    revision: u64,
    prepared: Option<PreparedCompaction>,
    attempts: u8,
    stage: CompactionPersistStage,
}

impl PendingCompaction {
    fn should_retry_after_failure(&mut self) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts < MAX_COMPACTION_CHECKPOINT_ATTEMPTS
    }
}

impl SessionRuntime {
    fn id(&self) -> n00nId {
        self.app.state.session.id
    }
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCtx {
    storage: StateDir,
    config: AgentConfig,
    ui_config: UiConfig,
    input_history_size: usize,
    retention_budget: RetentionBudget,
    /// Prototype only: every runtime forks its own manager so session
    /// rules stay per-session.
    permissions: Arc<PermissionManager>,
    timeouts: Timeouts,
    openai_options: OpenAiOptions,
    custom_commands: Arc<[CustomCommand]>,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    lua_event_handle: Option<EventHandle>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    available_models: Arc<ArcSwapOption<Vec<String>>>,
    storage_writer: Arc<StorageWriter>,
    picker: Arc<Picker>,
    hydration_requested_roots: Arc<Mutex<HashMap<n00nId, Option<u64>>>>,
    revision_allocators: RefCell<HashMap<n00nId, Arc<RevisionAllocator>>>,
}

impl SpawnCtx {
    fn spawn_runtime(&self, mut session: AppSession) -> Result<SessionRuntime> {
        session.meta.direct_paused_team = session
            .meta
            .direct_paused_team
            .as_ref()
            .and_then(validated_paused_team_payload);
        let resumed = crate::app::session_has_content(&session);
        let direct_bootstrap_active = !session.meta.queued_direct_tools.is_empty();
        let identity = session_identity(&session)
            .map_err(|error| eyre!("invalid session identity: {error}"))?;
        let root_id = persisted_root_id(&session);
        let referenced_revision = outer_compaction_boundary_revision(&session.transcript);
        let root = if root_id == session.id {
            None
        } else {
            match self.storage_writer.latest_snapshot(root_id) {
                Ok(Some(root)) => Some(Arc::unwrap_or_clone(root)),
                Ok(None) => match AppSession::load_with_retention(
                    root_id,
                    &self.storage,
                    self.retention_budget,
                    message_tool_use_ids,
                ) {
                    Ok(root) => Some(root),
                    Err(error) => {
                        warn!(session_id = %session.id, %root_id, %error, "root session state unavailable; using child plugin state");
                        None
                    }
                },
                Err(error) => {
                    warn!(session_id = %session.id, %root_id, %error, "current root session state unavailable; using child plugin state");
                    None
                }
            }
        };
        let session_snapshot = resume_state_snapshot(&session, referenced_revision)
            .map_err(|error| eyre!("failed to select session plugin state: {error}"))?;
        let root_snapshot = match root.as_ref() {
            Some(root) => resume_state_snapshot(root, referenced_revision),
            None => Ok(session_snapshot.clone()),
        }
        .map_err(|error| eyre!("failed to select root plugin state: {error}"))?;
        let initial_state_revision = initial_state_revision(&session, root.as_ref());
        let revision_allocator = Arc::clone(
            self.revision_allocators
                .borrow_mut()
                .entry(root_id)
                .or_insert_with(|| Arc::new(RevisionAllocator::new(initial_state_revision))),
        );
        revision_allocator.observe(initial_state_revision);

        if let Some(handle) = &self.lua_event_handle {
            let root_snapshot_revision = root_snapshot
                .as_ref()
                .and_then(StoredSessionStateSnapshot::state_revision);
            let should_hydrate_root = self
                .hydration_requested_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&root_id)
                .is_none_or(|hydrated_revision| root_snapshot_revision > *hydrated_revision);
            if should_hydrate_root {
                self.hydration_requested_roots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(root_id, root_snapshot_revision);
                let hydration_requested_roots = Arc::clone(&self.hydration_requested_roots);
                let queue_result = handle.hydrate_state_background_with_completion(
                    &SessionIdentity::root(SessionRef::from_id(root_id)),
                    root_snapshot,
                    move |result| {
                        if let Err(error) = result {
                            let mut requests = hydration_requested_roots
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if requests.get(&root_id) == Some(&root_snapshot_revision) {
                                requests.remove(&root_id);
                            }
                            warn!(%root_id, %error, "failed to restore root plugin session state");
                        }
                    },
                );
                if let Err(error) = queue_result {
                    self.hydration_requested_roots
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&root_id);
                    return Err(eyre!("failed to hydrate root plugin state: {error}"));
                }
            }
            if root_id != session.id {
                handle
                    .hydrate_state_background(&identity, session_snapshot)
                    .map_err(|error| eyre!("failed to hydrate plugin session state: {error}"))?;
            }
        }
        let permissions = Arc::new(self.permissions.fork());
        let initial_plan_path = session.meta.plan_path.as_ref().map(PathBuf::from);
        let handles = AgentHandles::spawn(
            &self.model_slot,
            session.messages.clone(),
            session.transcript.clone(),
            initial_state_revision,
            revision_allocator,
            initial_plan_path,
            self.config.clone(),
            self.ui_config.tool_output_lines,
            &permissions,
            Some(identity),
            self.timeouts,
            self.openai_options.clone(),
            self.lua_event_handle.clone(),
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
        );
        let mut app = App::new(AppInit {
            model: self.model_slot.load().model.clone(),
            session,
            storage: self.storage.clone(),
            available_models: Arc::clone(&self.available_models),
            mcp_reader: handles.mcp_reader(),
            mcp_config_errors: handles.mcp_config_errors.clone(),
            lua_command_reader: self.lua_command_reader.clone(),
            keymap_reader: self.keymap_reader.clone(),
            hint_reader: self.hint_reader.clone(),
            storage_writer: Arc::clone(&self.storage_writer),
            ui_config: self.ui_config.clone(),
            input_history_size: self.input_history_size,
            retention_budget: self.retention_budget,
            permissions,
            custom_commands: Arc::clone(&self.custom_commands),
            picker: Arc::clone(&self.picker),
        });
        app.lua_event_handle.clone_from(&self.lua_event_handle);
        handles.apply_to_app(&mut app);
        if resumed {
            restore_session(&mut app, &handles);
        }
        let last_status = SessionStatus::of(&app);
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        Ok(SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
            last_status,
            direct_bootstrap_active,
            pending_compactions: VecDeque::new(),
            deferred_agent_events: VecDeque::new(),
        })
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    lineage: SessionLineageGuard,
    ctx: SpawnCtx,
    input: InputReader,
    pending_input: RefCell<VecDeque<Event>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    ui_action_rx: Option<flume::Receiver<UiAction>>,
    submission_persist_tx: flume::Sender<SubmissionPersistence>,
    submission_persist_rx: flume::Receiver<SubmissionPersistence>,
    compaction_capture_tx: flume::Sender<CompactionCaptureCompletion>,
    compaction_capture_rx: flume::Receiver<CompactionCaptureCompletion>,
    compaction_persist_tx: flume::Sender<CompactionPersistence>,
    compaction_persist_rx: flume::Receiver<CompactionPersistence>,
    state_capture_tx: flume::Sender<StateCaptureCompletion>,
    state_capture_rx: flume::Receiver<StateCaptureCompletion>,
    pending_state_captures: HashSet<n00nId>,
    deferred_state_captures: HashSet<n00nId>,
    post_draw_submissions: Vec<(n00nId, SubmissionDispatch)>,
    last_save: Instant,
    startup_login_slot: Option<Arc<ModelSlot>>,
    model_refresh_generation: Arc<Mutex<u64>>,
    _model_fetch_task: smol::Task<()>,
    /// Set when UI state changed and a fresh frame must be painted. Draws are
    /// gated on this (or active animation) so we don't re-diff the whole
    /// buffer on every idle tick. Resize also sets it.
    dirty: bool,
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
struct SubmissionPersistence {
    session_id: n00nId,
    dispatch: SubmissionDispatch,
    execution_started: bool,
    result: Result<(), SessionError>,
}

struct CompactionCaptureCompletion {
    session_id: n00nId,
    run_id: u64,
    revision: u64,
    result: std::result::Result<PreparedCompaction, String>,
}

struct CompactionPersistence {
    session_id: n00nId,
    run_id: u64,
    revision: u64,
    stage: CompactionPersistStage,
    result: Result<(), SessionError>,
}

struct CapturedState {
    revision: u64,
    result: std::result::Result<StoredSessionStateSnapshot, String>,
}

struct StateCaptureCompletion {
    session_id: n00nId,
    session: CapturedState,
    root: Option<(n00nId, CapturedState)>,
}
fn begin_state_capture(
    pending: &mut HashSet<n00nId>,
    deferred: &mut HashSet<n00nId>,
    session_id: n00nId,
) -> bool {
    if pending.insert(session_id) {
        true
    } else {
        deferred.insert(session_id);
        false
    }
}

enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    Agent(usize, Box<n00n_agent::Envelope>),
    Shell(usize, ShellEvent),
    SubmissionPersisted(SubmissionPersistence),
    CompactionCaptured(Box<CompactionCaptureCompletion>),
    CompactionPersisted(CompactionPersistence),
    StateCaptured(StateCaptureCompletion),
    Warn(String),
}

struct DrainScheduler {
    prefer_input: bool,
}

impl Default for DrainScheduler {
    fn default() -> Self {
        Self { prefer_input: true }
    }
}

impl DrainScheduler {
    fn next<T>(
        &mut self,
        mut input: impl FnMut() -> Option<T>,
        mut other: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        let (is_input, item) = if self.prefer_input {
            input()
                .map(|item| (true, item))
                .or_else(|| other().map(|item| (false, item)))
        } else {
            other()
                .map(|item| (false, item))
                .or_else(|| input().map(|item| (true, item)))
        }?;
        self.prefer_input = !is_input;
        Some(item)
    }
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    generation: Arc<Mutex<u64>>,
    task: smol::Task<()>,
}

fn merge_batch(
    available: &ArcSwapOption<Vec<String>>,
    batch: n00n_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
) {
    for w in batch.warnings {
        let _ = warn_tx.try_send(w);
    }
    if batch.models.is_empty() {
        return;
    }
    let mut merged = available
        .load()
        .as_deref()
        .cloned()
        .unwrap_or_else(Vec::new);
    for spec in &batch.models {
        let well_formed = spec
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty());
        if well_formed && !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    available.store(Some(Arc::new(merged)));
}

fn merge_model_batch(
    available: &ArcSwapOption<Vec<String>>,
    batch: n00n_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
    generation: u64,
    current_generation: &Mutex<u64>,
) -> bool {
    let current = current_generation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *current != generation {
        return false;
    }
    merge_batch(available, batch, warn_tx);
    true
}

fn resolve_model_selection(
    spec: &str,
    discovered: Option<&[String]>,
) -> Result<Model, ModelCatalogError> {
    ModelCatalog::current_with_specs(discovered.into_iter().flatten().cloned()).resolve(spec)
}

fn publish_model_refresh(
    available: &ArcSwapOption<Vec<String>>,
    refreshed: Option<Arc<Vec<String>>>,
    generation: u64,
    current_generation: &Mutex<u64>,
) -> bool {
    let current = current_generation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *current != generation {
        return false;
    }
    available.store(refreshed);
    true
}

fn startup_login_completed(initial_slot: &Arc<ModelSlot>, current_slot: &Arc<ModelSlot>) -> bool {
    !Arc::ptr_eq(initial_slot, current_slot)
}

fn complete_model_fetch_with(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_slot: &Arc<ModelSlot>,
    needs_login: bool,
    create: impl FnOnce(
        &mut Model,
    ) -> std::result::Result<Box<dyn Provider>, n00n_providers::AgentError>,
) {
    if needs_login || !Arc::ptr_eq(&model_slot.load_full(), initial_slot) {
        return;
    }

    let spec = initial_slot.model.spec();
    let mut resolved = match Model::from_spec(&spec) {
        Ok(model) => model,
        Err(error) => {
            warn!(spec = %spec, %error, "failed to resolve model after discovery");
            return;
        }
    };
    let provider = match create(&mut resolved) {
        Ok(provider) => provider,
        Err(error) => {
            warn!(spec = %spec, %error, "failed to create provider after discovery");
            return;
        }
    };
    drop(model_slot.compare_and_swap(
        initial_slot,
        Arc::new(ModelSlot {
            model: resolved,
            provider: Arc::from(provider),
        }),
    ));
}

fn spawn_model_fetch(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    timeouts: Timeouts,
    openai_options: OpenAiOptions,
    needs_login: bool,
) -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let warn_tx_bg = warn_tx.clone();
    let model_slot = Arc::clone(model_slot);
    let initial_slot = model_slot.load_full();
    let generation = Arc::new(Mutex::new(0));
    let fetch_generation = Arc::clone(&generation);
    let task = smol::spawn(async move {
        let warn_tx = warn_tx_bg;
        let done = Box::new(move || {
            complete_model_fetch_with(&model_slot, &initial_slot, needs_login, |model| {
                from_model_with_openai_options(model, timeouts, openai_options)
            });
        });
        fetch_all_models(
            |batch| {
                merge_model_batch(&bg, batch, &warn_tx, 0, &fetch_generation);
            },
            Some(done),
        )
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        generation,
        task,
    }
}

fn restore_session(app: &mut App, handles: &AgentHandles) {
    app.permissions
        .load_session_rules(crate::app::session_state::stored_to_rules(
            &app.state.session.meta.session_rules,
        ));
    (*handles
        .tool_outputs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
    .clone_from(&app.state.session.tool_outputs);
    app.restore_display();
    for w in app.state.warnings.drain(..) {
        app.status_bar.flash(w);
    }
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();

        let EventLoopParams {
            mut model,
            mut needs_login,
            commands,
            mut sessions,
            focused,
            mut startup_warnings,
            storage,
            config,
            project_trusted,
            ui_config,
            input_history_size,
            retention_budget,
            snapshot_timeout,
            permissions,
            timeouts,
            openai_options,
            exit_on_done,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            ui_action_rx,
            lua_event_handle,
        } = params;

        PROCESS_WARMUP.call_once(|| {
            std::thread::spawn(crate::highlight::warmup);
            crate::update::spawn_check();
        });
        let storage_writer = Arc::new(StorageWriter::new_with_timeout(
            storage.clone(),
            snapshot_timeout,
        )?);
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start(
            &cwd,
            config.mcp_tool_desc_max_chars,
            project_trusted,
        ));

        let (provider, provider_warning) =
            startup_provider_with(&mut model, needs_login, |model| {
                from_model_with_openai_options(model, timeouts, openai_options.clone())
            });
        if let Some(warning) = provider_warning {
            startup_warnings.push(warning);
            needs_login = true;
        }
        let provider = Arc::from(provider);
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let bg = spawn_model_fetch(&model_slot, timeouts, openai_options.clone(), needs_login);
        let startup_login_slot = needs_login.then(|| model_slot.load_full());

        let picker = Arc::new(terminal_image::picker());

        let runtime_ids: HashSet<_> = sessions.iter().map(|session| session.id).collect();
        let stored = AppSession::list(&cwd.to_string_lossy(), &storage)
            .map_err(|error| eyre!("failed to list stored session lineage: {error}"))?;
        let mut stored_sessions = Vec::new();
        for summary in stored {
            if runtime_ids.contains(&summary.id) {
                continue;
            }
            match AppSession::load_with_retention(
                summary.id,
                &storage,
                retention_budget,
                message_tool_use_ids,
            ) {
                Ok(session) => stored_sessions.push(session),
                Err(error) => startup_warnings.push(format!(
                    "Skipped unreadable stored session {}: {error}",
                    summary.id
                )),
            }
        }
        let parents: HashMap<_, _> = sessions
            .iter()
            .chain(&stored_sessions)
            .map(|session| (session.id, session.meta.parent_id))
            .collect();
        for session in &mut sessions {
            let root = resolved_root(session.id, &parents)
                .map_err(|error| eyre!("invalid live session lineage: {error}"))?;
            if session
                .meta
                .root_session_id
                .is_some_and(|stored| stored != root)
            {
                return Err(eyre!(
                    "invalid live session lineage root for {}",
                    session.id
                ));
            }
            session.meta.root_session_id = (session.meta.parent_id.is_some()).then_some(root);
        }
        let mut live_sessions = sessions
            .iter()
            .map(|session| {
                let mut live = live_session(session)?;
                live.execution_active = has_restorable_work(session);
                Ok(live)
            })
            .collect::<std::result::Result<Vec<_>, LineageError>>()
            .map_err(|error| eyre!("invalid live session lineage: {error}"))?;
        for mut session in stored_sessions {
            let root = match resolved_root(session.id, &parents) {
                Ok(root) => root,
                Err(error) => {
                    startup_warnings.push(format!(
                        "Skipped stored session {} with invalid lineage: {error}",
                        session.id
                    ));
                    continue;
                }
            };
            let migrated_root = session.meta.parent_id.map(|_| root);
            if session.meta.root_session_id != migrated_root {
                session.meta.root_session_id = migrated_root;
                storage_writer.send(Box::new(session.clone()));
            }
            let mut node = live_session(&session)
                .map_err(|error| eyre!("invalid stored session lineage: {error}"))?;
            node.runtime_present = false;
            node.execution_active = false;
            live_sessions.push(node);
        }
        let lineage = SessionLineageGuard::from_live(
            live_sessions,
            LineageLimits {
                max_depth: config.max_depth,
                max_total_descendants: config.max_total_descendants,
                max_active_descendants: config.max_active_descendants,
            },
        )
        .map_err(|error| eyre!("invalid live session lineage: {error}"))?;

        let ctx = SpawnCtx {
            storage,
            config,
            ui_config,
            input_history_size,
            retention_budget,
            permissions,
            timeouts,
            openai_options,
            custom_commands: Arc::from(commands),
            lua_command_reader,
            keymap_reader,
            hint_reader,
            lua_event_handle,
            mcp_handle,
            mcp_config_errors,
            model_slot,
            available_models: bg.available,
            storage_writer,
            picker,
            hydration_requested_roots: Arc::new(Mutex::new(HashMap::new())),
            revision_allocators: RefCell::new(HashMap::new()),
        };

        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .map(|session| ctx.spawn_runtime(session))
            .collect::<Result<Vec<_>>>()?;
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let app = &mut runtimes[focused].app;
        app.exit_on_done = exit_on_done;
        if needs_login {
            app.login_picker.open(app.storage.clone());
        }
        if !ctx.mcp_config_errors.is_empty() {
            let msg = format!("MCP config error: {}", ctx.mcp_config_errors);
            app.flash(msg);
        }
        for w in startup_warnings {
            app.flash(w);
        }
        app.fire_session_focus_autocmd();

        let (submission_persist_tx, submission_persist_rx) = flume::unbounded();
        let (compaction_capture_tx, compaction_capture_rx) = flume::unbounded();
        let (compaction_persist_tx, compaction_persist_rx) = flume::unbounded();
        let (state_capture_tx, state_capture_rx) = flume::unbounded();
        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            lineage,
            ctx,
            input: InputReader::spawn()?,
            pending_input: RefCell::new(VecDeque::new()),
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            ui_action_rx,
            submission_persist_tx,
            submission_persist_rx,
            compaction_capture_tx,
            compaction_capture_rx,
            compaction_persist_tx,
            compaction_persist_rx,
            state_capture_tx,
            state_capture_rx,
            pending_state_captures: HashSet::new(),
            deferred_state_captures: HashSet::new(),
            post_draw_submissions: Vec::new(),
            last_save: Instant::now(),
            startup_login_slot,
            model_refresh_generation: bg.generation,
            _model_fetch_task: bg.task,
            dirty: true,
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }
    fn dispatch_initial_prompt(&mut self, session_id: n00nId, initial_prompt: &mut Option<String>) {
        let Some(index) = self.position(session_id) else {
            warn!(%session_id, "startup session disappeared before initial prompt dispatch");
            initial_prompt.take();
            return;
        };
        let Some(prompt) = initial_prompt.take() else {
            return;
        };
        let submission = Submission {
            text: prompt,
            images: Vec::new(),
            control: false,
        };
        let actions = self.sessions[index].app.handle_submit(submission);
        self.dispatch(index, actions);
    }

    pub(crate) fn run(mut self, mut initial_prompt: Option<String>) -> Result<ShutdownReport> {
        let initial_prompt_session_id = self.sessions[self.focused].id();
        if self.startup_login_slot.is_none() {
            self.dispatch_initial_prompt(initial_prompt_session_id, &mut initial_prompt);
        }
        let result = loop {
            self.tick();
            if let Err(e) = self.drain_channels() {
                break Err(e);
            }
            if self
                .startup_login_slot
                .as_ref()
                .is_some_and(|initial_slot| {
                    startup_login_completed(initial_slot, &self.ctx.model_slot.load_full())
                })
            {
                self.startup_login_slot = None;
                self.dispatch_initial_prompt(initial_prompt_session_id, &mut initial_prompt);
            }
            let should_draw = self.dirty
                || self.sessions[self.focused].app.is_animating()
                || !self.post_draw_submissions.is_empty();
            let app = &mut self.sessions[self.focused].app;
            if should_draw {
                if let Err(e) = draw_then_post_terminal(self.terminal, |f| app.view(f), || {}) {
                    break Err(e.into());
                }
                self.dirty = false;
                self.after_terminal_draw();
            }

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None
                    && !self.pending_state_captures.contains(&rt.id())
            }) {
                // A backgrounded session can finish an `exit_on_done` turn;
                // focus it so shutdown reports its exit code and id.
                self.focused = i;
                break Ok(());
            }

            let timeout = if self.sessions[self.focused].app.is_animating() {
                Duration::from_millis(ANIMATION_INTERVAL_MS)
            } else {
                Duration::from_millis(IDLE_POLL_INTERVAL_MS)
            };
            if let Some(wake) = self.next_wake(timeout)
                && let Err(e) = self.handle_wake(wake)
            {
                break Err(e);
            }
        };
        // Fatal errors still save every session, shut down MCP transports
        // (terminating and reaping their child processes), and drain the
        // storage writer before the process exits.
        let shutdown = self.shutdown();
        match result {
            Ok(()) => shutdown,
            Err(error) => {
                if let Err(shutdown_error) = shutdown {
                    warn!(error = %shutdown_error, "shutdown after fatal error was incomplete");
                }
                Err(error)
            }
        }
    }

    /// Wait for the next event from any source, or time out so animations
    /// and periodic polls keep running. Already-pending input wins before
    /// joining the fair selector.
    fn next_wake(&self, timeout: Duration) -> Option<Wake> {
        self.try_input_wake().or_else(|| self.select_wake(timeout))
    }

    fn try_input_wake(&self) -> Option<Wake> {
        self.pending_input
            .borrow_mut()
            .pop_front()
            .or_else(|| self.input.receiver().try_recv().ok())
            .map(Wake::Input)
    }

    fn select_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new().recv(self.input.receiver(), |res| match res {
            Ok(ev) => Some(Wake::Input(ev)),
            Err(_) => Some(Wake::InputGone),
        });
        if let Some(rx) = self
            .ui_action_rx
            .as_ref()
            .filter(|rx| !rx.is_disconnected())
        {
            sel = sel.recv(rx, |res| res.ok().map(Wake::Ui));
        }
        sel = sel.recv(&self.warn_rx, |res| res.ok().map(Wake::Warn));
        sel = sel.recv(&self.submission_persist_rx, |res| {
            res.ok().map(Wake::SubmissionPersisted)
        });
        sel = sel.recv(&self.compaction_capture_rx, |res| {
            res.ok().map(Box::new).map(Wake::CompactionCaptured)
        });
        sel = sel.recv(&self.compaction_persist_rx, |res| {
            res.ok().map(Wake::CompactionPersisted)
        });
        sel = sel.recv(&self.state_capture_rx, |res| {
            res.ok().map(Wake::StateCaptured)
        });
        for (i, rt) in self.sessions.iter().enumerate() {
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn next_non_input_wake(&self) -> Option<Wake> {
        self.select_non_input_wake(Duration::ZERO)
    }

    fn select_non_input_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new();
        if let Some(rx) = self
            .ui_action_rx
            .as_ref()
            .filter(|rx| !rx.is_disconnected())
        {
            sel = sel.recv(rx, |res| res.ok().map(Wake::Ui));
        }
        sel = sel.recv(&self.warn_rx, |res| res.ok().map(Wake::Warn));
        sel = sel.recv(&self.submission_persist_rx, |res| {
            res.ok().map(Wake::SubmissionPersisted)
        });
        sel = sel.recv(&self.compaction_capture_rx, |res| {
            res.ok().map(Box::new).map(Wake::CompactionCaptured)
        });
        sel = sel.recv(&self.compaction_persist_rx, |res| {
            res.ok().map(Wake::CompactionPersisted)
        });
        sel = sel.recv(&self.state_capture_rx, |res| {
            res.ok().map(Wake::StateCaptured)
        });
        for (i, rt) in self.sessions.iter().enumerate() {
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        self.dirty = true;
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Shell(i, event) => self.sessions[i].app.handle_shell_event(event),
            Wake::SubmissionPersisted(completion) => self.handle_submission_persisted(completion),
            Wake::CompactionCaptured(completion) => self.handle_compaction_captured(*completion),
            Wake::CompactionPersisted(completion) => self.handle_compaction_persisted(completion),
            Wake::StateCaptured(completion) => self.handle_state_captured(completion),
            Wake::Warn(warning) => self.focused_app().flash(warning),
        }
        Ok(())
    }

    fn tick(&mut self) {
        for idx in 0..self.sessions.len() {
            self.retry_compaction_checkpoint(idx);
        }
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            rt.app.float_mgr.tick();
            if i != self.focused {
                continue;
            }
            rt.app.tick_edge_scroll();
            rt.app.tick_error_expiry();
            rt.app.poll_image_paste();
            rt.app.btw_modal.poll();
            rt.app.status_bar.poll_branch_update();
            rt.app.mcp_picker.refresh();
        }
        for rt in &mut self.sessions {
            rt.app.tick_pending_save();
        }
        self.tick_periodic_save();
    }

    fn tick_periodic_save(&mut self) {
        if self.last_save.elapsed() < PERIODIC_SAVE_INTERVAL {
            return;
        }
        // Lua state capture is a drain barrier. Active tools are captured by their terminal event.
        for rt in &mut self.sessions {
            if rt.pending_compactions.is_empty()
                && should_save_periodically(&rt.app.status, rt.app.awaiting_input())
            {
                rt.app.save_session_without_plugin_state_capture();
            }
        }
        self.last_save = Instant::now();
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<n00n_agent::Envelope>) {
        if let n00n_agent::AgentEvent::QueueDrained { generation } = &envelope.event {
            if self.sessions[idx].handles.queue.is_drained(*generation) {
                let id = self.sessions[idx].id();
                if let Err(error) = self.lineage.set_execution_active(id, false) {
                    warn!(session_id = %id, error = %error, "failed to release drained session activity");
                }
                self.sessions[idx]
                    .app
                    .save_session_without_plugin_state_capture();
            }
            return;
        }
        if envelope.run_id != self.sessions[idx].app.run_id {
            let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
            self.dispatch(idx, actions);
            return;
        }
        if !self.sessions[idx].pending_compactions.is_empty() {
            self.sessions[idx].deferred_agent_events.push_back(envelope);
            return;
        }
        if self.sessions[idx].direct_bootstrap_active {
            match &envelope.event {
                n00n_agent::AgentEvent::ToolDone(done) => {
                    let output = done.output.as_text();
                    let meta = &mut self.sessions[idx].app.state.session.meta;
                    meta.direct_paused_team = direct_paused_team_payload(&done.tool, &output);
                    meta.direct_output = Some(bounded_direct_output(&output, &self.ctx.config));
                    meta.direct_output_is_error = done.is_error;
                }
                n00n_agent::AgentEvent::Error { message }
                    if self.sessions[idx]
                        .app
                        .state
                        .session
                        .meta
                        .direct_output
                        .is_none() =>
                {
                    self.sessions[idx].app.state.session.meta.direct_output =
                        Some(bounded_direct_output(message, &self.ctx.config));
                    self.sessions[idx]
                        .app
                        .state
                        .session
                        .meta
                        .direct_output_is_error = true;
                }
                _ => {}
            }
        }
        let lifecycle = match &envelope.event {
            n00n_agent::AgentEvent::Done { .. } => Some(StoredSessionLifecycle::Succeeded),
            n00n_agent::AgentEvent::Error { .. } => Some(StoredSessionLifecycle::Failed),
            n00n_agent::AgentEvent::PermissionRequest { .. }
            | n00n_agent::AgentEvent::AuthRequired
            | n00n_agent::AgentEvent::SubagentInputRequired { .. } => {
                Some(StoredSessionLifecycle::WaitingInput)
            }
            n00n_agent::AgentEvent::ToolStart(_)
            | n00n_agent::AgentEvent::TextDelta { .. }
            | n00n_agent::AgentEvent::ThinkingDelta { .. }
            | n00n_agent::AgentEvent::QueueItemConsumed { .. } => {
                Some(StoredSessionLifecycle::Running)
            }
            _ => None,
        };
        let compaction_revision = match &envelope.event {
            n00n_agent::AgentEvent::CompactionDone { state_revision } => *state_revision,
            _ => None,
        };
        if let Some(revision) = compaction_revision
            && envelope.run_id == self.sessions[idx].app.run_id
            && envelope.subagent.is_none()
        {
            let transcript = self.sessions[idx]
                .app
                .shared_transcript
                .as_ref()
                .map(|transcript| transcript.load_full());
            self.sessions[idx]
                .pending_compactions
                .push_back(PendingCompaction {
                    ack: envelope,
                    transcript,
                    revision,
                    prepared: None,
                    attempts: 0,
                    stage: CompactionPersistStage::Ready,
                });
            self.retry_compaction_checkpoint(idx);
            return;
        }
        let capture = matches!(
            &envelope.event,
            n00n_agent::AgentEvent::Done { .. }
                | n00n_agent::AgentEvent::Error { .. }
                | n00n_agent::AgentEvent::AutoCompactFailed { .. }
                | n00n_agent::AgentEvent::CompactionDone { .. }
        );
        let terminal = matches!(
            lifecycle,
            Some(StoredSessionLifecycle::Succeeded | StoredSessionLifecycle::Failed)
        );
        let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
        if let Some(lifecycle) = lifecycle {
            self.sessions[idx].app.state.session.meta.lifecycle = lifecycle;
            if terminal {
                self.sessions[idx].direct_bootstrap_active = false;
                self.sessions[idx]
                    .app
                    .state
                    .session
                    .meta
                    .queued_direct_tools
                    .clear();
            }
        }
        self.dispatch(idx, actions);
        if terminal {
            self.sessions[idx]
                .app
                .save_session_without_plugin_state_capture();
        }
        if capture && self.sessions[idx].pending_compactions.is_empty() {
            self.schedule_plugin_state_capture(idx);
        }
    }

    fn compaction_snapshots(
        &mut self,
        idx: usize,
    ) -> std::result::Result<(AppSession, Option<AppSession>), String> {
        let session = self.sessions[idx].app.session_snapshot();
        let root_id = persisted_root_id(&session);
        let root = if root_id == session.id {
            None
        } else {
            let root = if let Some(root_idx) = self.position(root_id) {
                self.sessions[root_idx].app.session_snapshot()
            } else if let Some(root) = self
                .ctx
                .storage_writer
                .latest_snapshot(root_id)
                .map_err(|error| error.to_string())?
            {
                Arc::unwrap_or_clone(root)
            } else {
                AppSession::load_with_retention(
                    root_id,
                    &self.ctx.storage,
                    self.ctx.retention_budget,
                    message_tool_use_ids,
                )
                .map_err(|error| error.to_string())?
            };
            Some(root)
        };
        Ok((session, root))
    }

    fn apply_compaction_metadata(
        &mut self,
        idx: usize,
        prepared: &PreparedCompaction,
    ) -> std::result::Result<(), String> {
        if let Some(root) = &prepared.root
            && let Some(root_idx) = self.position(root.id)
        {
            merge_compaction_metadata(
                &mut self.sessions[root_idx].app.state.session,
                root,
                prepared.revision,
            )?;
        }
        merge_compaction_metadata(
            &mut self.sessions[idx].app.state.session,
            &prepared.session,
            prepared.revision,
        )
    }

    fn retry_compaction_checkpoint(&mut self, idx: usize) {
        let Some(mut pending) = self.sessions[idx].pending_compactions.pop_front() else {
            return;
        };
        if matches!(
            pending.stage,
            CompactionPersistStage::Capturing
                | CompactionPersistStage::PersistingRoot
                | CompactionPersistStage::PersistingSession
        ) {
            self.sessions[idx].pending_compactions.push_front(pending);
            return;
        }
        if pending.prepared.is_none() {
            let (mut session, mut root) = match self.compaction_snapshots(idx) {
                Ok(snapshots) => snapshots,
                Err(error) => {
                    self.handle_compaction_failure(idx, pending, &error);
                    return;
                }
            };
            let session_id = self.sessions[idx].id();
            let run_id = pending.ack.run_id;
            let revision = pending.revision;
            pending.stage = CompactionPersistStage::Capturing;
            self.sessions[idx].pending_compactions.push_front(pending);
            let handle = self.ctx.lua_event_handle.clone();
            let completion_tx = self.compaction_capture_tx.clone();
            smol::spawn(async move {
                let result = async {
                    prepare_compaction_checkpoint_async(handle.as_ref(), &mut session, revision)
                        .await?;
                    if let Some(root) = &mut root {
                        prepare_compaction_checkpoint_async(handle.as_ref(), root, revision)
                            .await?;
                    }
                    Ok(PreparedCompaction {
                        revision,
                        session,
                        root,
                    })
                }
                .await;
                let _ = completion_tx
                    .send_async(CompactionCaptureCompletion {
                        session_id,
                        run_id,
                        revision,
                        result,
                    })
                    .await;
            })
            .detach();
            return;
        }
        let Some(prepared) = pending.prepared.as_ref() else {
            self.handle_compaction_failure(idx, pending, "compaction checkpoint was not prepared");
            return;
        };
        let (stage, snapshot) = match pending.stage {
            CompactionPersistStage::Ready => match &prepared.root {
                Some(root) => (CompactionPersistStage::PersistingRoot, root.clone()),
                None => (
                    CompactionPersistStage::PersistingSession,
                    prepared.session.clone(),
                ),
            },
            CompactionPersistStage::RootDurable => (
                CompactionPersistStage::PersistingSession,
                prepared.session.clone(),
            ),
            CompactionPersistStage::Capturing
            | CompactionPersistStage::PersistingRoot
            | CompactionPersistStage::PersistingSession => return,
        };
        let session_id = self.sessions[idx].id();
        let run_id = pending.ack.run_id;
        let revision = pending.revision;
        pending.stage = stage;
        self.sessions[idx].pending_compactions.push_front(pending);
        let completion_tx = self.compaction_persist_tx.clone();
        self.ctx
            .storage_writer
            .persist(Box::new(snapshot), move |result| {
                let _ = completion_tx.send(CompactionPersistence {
                    session_id,
                    run_id,
                    revision,
                    stage,
                    result,
                });
            });
    }

    fn handle_compaction_captured(&mut self, completion: CompactionCaptureCompletion) {
        let Some(idx) = self.position(completion.session_id) else {
            return;
        };
        let Some(mut pending) = self.sessions[idx].pending_compactions.pop_front() else {
            return;
        };
        if pending.ack.run_id != completion.run_id
            || pending.revision != completion.revision
            || pending.stage != CompactionPersistStage::Capturing
        {
            self.sessions[idx].pending_compactions.push_front(pending);
            return;
        }
        match completion.result {
            Ok(prepared) => {
                pending.prepared = Some(prepared);
                pending.stage = CompactionPersistStage::Ready;
                self.sessions[idx].pending_compactions.push_front(pending);
                self.retry_compaction_checkpoint(idx);
            }
            Err(error) => {
                pending.stage = CompactionPersistStage::Ready;
                self.handle_compaction_failure(idx, pending, &error);
            }
        }
    }

    fn handle_compaction_persisted(&mut self, completion: CompactionPersistence) {
        let Some(idx) = self.position(completion.session_id) else {
            return;
        };
        let Some(mut pending) = self.sessions[idx].pending_compactions.pop_front() else {
            return;
        };
        if pending.ack.run_id != completion.run_id
            || pending.revision != completion.revision
            || pending.stage != completion.stage
        {
            self.sessions[idx].pending_compactions.push_front(pending);
            return;
        }
        if let Err(error) = completion.result {
            pending.stage = CompactionPersistStage::Ready;
            self.handle_compaction_failure(idx, pending, &error.to_string());
            return;
        }
        if completion.stage == CompactionPersistStage::PersistingRoot {
            pending.stage = CompactionPersistStage::RootDurable;
            self.sessions[idx].pending_compactions.push_front(pending);
            self.retry_compaction_checkpoint(idx);
            return;
        }
        if let Some(prepared) = &pending.prepared
            && let Err(error) = self.apply_compaction_metadata(idx, prepared)
        {
            warn!(session_id = %completion.session_id, %error, "failed to apply durable compaction checkpoint metadata");
        }
        self.finish_compaction(idx, pending);
    }

    fn handle_compaction_failure(
        &mut self,
        idx: usize,
        mut pending: PendingCompaction,
        error: &str,
    ) {
        if pending.should_retry_after_failure() {
            warn!(
                session_id = %self.sessions[idx].id(),
                attempt = pending.attempts,
                max_attempts = MAX_COMPACTION_CHECKPOINT_ATTEMPTS,
                %error,
                "failed to persist compaction state checkpoint; retrying"
            );
            self.sessions[idx].pending_compactions.push_front(pending);
            return;
        }
        warn!(
            session_id = %self.sessions[idx].id(),
            attempts = pending.attempts,
            %error,
            "failed to persist compaction state checkpoint; releasing compaction acknowledgement"
        );
        if let Some(prepared) = &pending.prepared
            && let Err(merge_error) = self.apply_compaction_metadata(idx, prepared)
        {
            warn!(
                session_id = %self.sessions[idx].id(),
                error = %merge_error,
                "failed to retain compaction checkpoint metadata in memory"
            );
        }
        self.finish_compaction(idx, pending);
    }

    fn finish_compaction(&mut self, idx: usize, pending: PendingCompaction) {
        let transcript = pending
            .transcript
            .map(|snapshot| Arc::new(ArcSwap::from(snapshot)));
        let live_transcript =
            std::mem::replace(&mut self.sessions[idx].app.shared_transcript, transcript);
        let actions = self.sessions[idx].app.update(Msg::Agent(pending.ack));
        self.sessions[idx].app.shared_transcript = live_transcript;
        self.dispatch(idx, actions);
        if matches!(
            self.sessions[idx].app.state.session.meta.lifecycle,
            StoredSessionLifecycle::Succeeded | StoredSessionLifecycle::Failed
        ) {
            self.schedule_plugin_state_capture(idx);
            self.sessions[idx]
                .app
                .save_session_without_plugin_state_capture();
        }
        self.drain_deferred_agent_events(idx);
    }

    fn drain_deferred_agent_events(&mut self, idx: usize) {
        while self.sessions[idx].pending_compactions.is_empty() {
            let Some(envelope) = self.sessions[idx].deferred_agent_events.pop_front() else {
                break;
            };
            self.handle_agent(idx, envelope);
        }
    }

    fn schedule_plugin_state_capture(&mut self, idx: usize) {
        if !self.sessions[idx].app.plugin_state_capture_safe()
            || !self.sessions[idx].pending_compactions.is_empty()
        {
            return;
        }
        let Some(handle) = self.ctx.lua_event_handle.clone() else {
            return;
        };
        let session_id = self.sessions[idx].id();
        if !begin_state_capture(
            &mut self.pending_state_captures,
            &mut self.deferred_state_captures,
            session_id,
        ) {
            return;
        }
        let session_identity = match session_identity(&self.sessions[idx].app.state.session) {
            Ok(identity) => identity,
            Err(error) => {
                self.pending_state_captures.remove(&session_id);
                warn!(%session_id, %error, "failed to identify plugin state capture");
                return;
            }
        };
        let session_revision = match self.sessions[idx].handles.allocate_state_revision() {
            Ok(revision) => revision,
            Err(error) => {
                self.pending_state_captures.remove(&session_id);
                warn!(%session_id, %error, "failed to allocate plugin state revision");
                return;
            }
        };
        let root_id = self.sessions[idx]
            .app
            .state
            .session
            .meta
            .root_session_id
            .unwrap_or_else(|| session_id);
        let root = if root_id == session_id {
            None
        } else {
            match self.sessions[idx].handles.allocate_state_revision() {
                Ok(revision) => Some((
                    root_id,
                    SessionIdentity::root(SessionRef::from_id(root_id)),
                    revision,
                )),
                Err(error) => {
                    self.pending_state_captures.remove(&session_id);
                    warn!(%session_id, %error, "failed to allocate root plugin state revision");
                    return;
                }
            }
        };
        let completion_tx = self.state_capture_tx.clone();
        smol::spawn(async move {
            let session = CapturedState {
                revision: session_revision,
                result: handle
                    .capture_state_async(&session_identity, session_revision)
                    .await
                    .map_err(|error| error.to_string()),
            };
            let root = if let Some((root_id, identity, revision)) = root {
                Some((
                    root_id,
                    CapturedState {
                        revision,
                        result: handle
                            .capture_state_async(&identity, revision)
                            .await
                            .map_err(|error| error.to_string()),
                    },
                ))
            } else {
                None
            };
            let _ = completion_tx
                .send_async(StateCaptureCompletion {
                    session_id,
                    session,
                    root,
                })
                .await;
        })
        .detach();
    }

    fn handle_state_captured(&mut self, completion: StateCaptureCompletion) {
        let StateCaptureCompletion {
            session_id,
            session,
            root,
        } = completion;
        self.pending_state_captures.remove(&session_id);
        if let Some((root_id, captured)) = root {
            if let Some(idx) = self.position(session_id) {
                self.sessions[idx]
                    .handles
                    .observe_state_revision(captured.revision);
            }
            if let Some(root_idx) = self.position(root_id) {
                self.apply_captured_state(root_idx, captured, "root");
                self.sessions[root_idx]
                    .app
                    .save_session_without_plugin_state_capture();
            } else {
                self.persist_stored_root_capture(root_id, captured);
            }
        }
        let Some(idx) = self.position(session_id) else {
            self.deferred_state_captures.remove(&session_id);
            return;
        };
        self.apply_captured_state(idx, session, "session");
        self.sessions[idx]
            .app
            .save_session_without_plugin_state_capture();
        if self.deferred_state_captures.remove(&session_id) {
            self.schedule_plugin_state_capture(idx);
        }
    }

    fn apply_captured_state(&mut self, idx: usize, captured: CapturedState, scope: &'static str) {
        let session_id = self.sessions[idx].id();
        let snapshot = match captured.result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(%session_id, %error, scope, "plugin state capture failed");
                return;
            }
        };
        if !capture_revision_matches(&snapshot, captured.revision) {
            warn!(%session_id, scope, expected = captured.revision, actual = ?snapshot.state_revision(), "plugin state capture returned the wrong revision");
            return;
        }
        let current = state_revision_or_initial(
            self.sessions[idx]
                .app
                .state
                .session
                .meta
                .state_snapshot
                .as_ref(),
        );
        if captured.revision < current {
            return;
        }
        let Some(session_revision) = self.sessions[idx]
            .app
            .state
            .session
            .meta
            .revision
            .checked_add(1)
        else {
            warn!(%session_id, "session revision exhausted while applying plugin state");
            return;
        };
        self.sessions[idx].app.state.session.meta.revision = session_revision;
        self.sessions[idx].app.state.session.meta.state_snapshot = Some(snapshot);
        self.sessions[idx]
            .handles
            .observe_state_revision(captured.revision);
    }

    fn persist_stored_root_capture(&mut self, root_id: n00nId, captured: CapturedState) {
        let snapshot = match captured.result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(%root_id, %error, "root plugin state capture failed");
                return;
            }
        };
        if !capture_revision_matches(&snapshot, captured.revision) {
            warn!(%root_id, expected = captured.revision, actual = ?snapshot.state_revision(), "root plugin state capture returned the wrong revision");
            return;
        }
        let mut root = match self.ctx.storage_writer.latest_snapshot(root_id) {
            Ok(Some(root)) => Arc::unwrap_or_clone(root),
            Ok(None) => match AppSession::load_with_retention(
                root_id,
                &self.ctx.storage,
                self.ctx.retention_budget,
                message_tool_use_ids,
            ) {
                Ok(root) => root,
                Err(error) => {
                    warn!(%root_id, %error, "failed to load root for plugin state capture");
                    return;
                }
            },
            Err(error) => {
                warn!(%root_id, %error, "failed to read root plugin state snapshot");
                return;
            }
        };
        if captured.revision < state_revision_or_initial(root.meta.state_snapshot.as_ref()) {
            return;
        }
        let Some(revision) = root.meta.revision.checked_add(1) else {
            warn!(%root_id, "root session revision exhausted while applying plugin state");
            return;
        };
        root.meta.revision = revision;
        root.meta.state_snapshot = Some(snapshot);
        root.updated_at = n00n_storage::now_epoch();
        self.ctx.storage_writer.send(Box::new(root));
    }

    fn drain_channels(&mut self) -> Result<()> {
        // Leftovers beyond the budget are picked up right after the next draw.
        let mut scheduler = DrainScheduler::default();
        for _ in 0..DRAIN_BUDGET {
            let Some(wake) =
                scheduler.next(|| self.try_input_wake(), || self.next_non_input_wake())
            else {
                break;
            };
            self.handle_wake(wake)?;
        }
        for rt in &mut self.sessions {
            if rt.app.status == Status::Streaming && rt.handles.agent_rx.is_disconnected() {
                rt.app.status = Status::error("agent stopped unexpectedly".into());
                self.dirty = true;
            }
        }

        let slot_model = self.ctx.model_slot.load();
        let spec = slot_model.model.spec();
        for rt in &mut self.sessions {
            if rt.app.state.session.model != spec
                || rt.app.state.model.context_window != slot_model.model.context_window
            {
                rt.app.update_model(&slot_model.model);
                self.dirty = true;
            }
        }
        drop(slot_model);

        self.emit_status_changes();
        Ok(())
    }

    fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::Flash(msg) => {
                self.focused_app().flash(msg);
            }
            UiAction::OpenEditor { path, reply_tx } => {
                let code = self.open_editor(self.focused, &path);
                let _ = reply_tx.send(code);
            }
            UiAction::OpenWin {
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
                close_requested,
            } => {
                let app = self.focused_app();
                app.float_mgr.open_with_close_flag(
                    buf,
                    config,
                    focus,
                    event_tx,
                    cmd_rx,
                    close_requested,
                );
                if focus {
                    app.transition_plan(&crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::PickModel { current, reply_tx } => {
                self.focused_app()
                    .pick_model_for_lua(current.as_deref(), reply_tx);
                self.handle_action(self.focused, Action::RefreshModels);
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
        }
    }

    /// Exits with the editor's status code; `-1` (flashed on the session's
    /// app) when the editor could not be launched.
    fn open_editor(&mut self, idx: usize, path: &std::path::Path) -> i32 {
        let result = match self.input.pause() {
            Ok(_pause) => terminal::open_in_editor(path, self.terminal),
            Err(e) => Err(e),
        };
        match result {
            Ok(code) => code,
            Err(e) => {
                self.sessions[idx].app.flash(e);
                -1
            }
        }
    }

    fn emit_status_changes(&mut self) {
        let Some(handle) = self.ctx.lua_event_handle.as_ref() else {
            return;
        };
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            let status = SessionStatus::of(&rt.app);
            if status == rt.last_status {
                continue;
            }
            rt.last_status = status;
            handle.fire_autocmd(
                "SessionStatusChanged",
                json!({
                    "session_id": rt.id(),
                    "title": rt.app.state.session.title,
                    "status": status.as_str(),
                    "focused": i == self.focused,
                }),
            );
        }
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes.
    #[allow(clippy::too_many_lines)]
    fn handle_session_request(
        &mut self,
        req: SessionRequest,
        reply_tx: flume::Sender<SessionReply>,
    ) {
        match req {
            SessionRequest::List => {
                let storage = self.ctx.storage.clone();
                smol::unblock(move || {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    let reply = AppSession::list(&cwd.to_string_lossy(), &storage)
                        .map_err(|e| e.to_string())
                        .and_then(|list| serde_json::to_value(list).map_err(|e| e.to_string()));
                    let _ = reply_tx.send(reply);
                })
                .detach();
            }
            // Deletes run on the storage writer thread after any queued
            // flushes, so the loop never blocks on disk and a queued save
            // cannot resurrect the files.
            SessionRequest::Delete {
                id,
                caller_id,
                trusted_ui_control,
            } => {
                if let Err(error) = authorize_ui_delete(caller_id.as_ref(), trusted_ui_control) {
                    let _ = reply_tx.send(Err(error));
                    return;
                }
                let id = match parse_session_id(&id) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                        return;
                    }
                };
                let mut targets = match self.lineage.descendants_for_delete(id) {
                    Ok(targets) => targets,
                    Err(LineageError::UnknownSession(_)) => Vec::new(),
                    Err(error) => {
                        let _ = reply_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                targets.push(id);
                let focused_id = self.sessions[self.focused].id();
                if targets.contains(&focused_id) {
                    let _ = reply_tx.send(Err(DELETE_FOCUSED_ERR.into()));
                    return;
                }
                let mut runtime_indices: Vec<_> = targets
                    .iter()
                    .filter_map(|target| self.position(*target))
                    .collect();
                runtime_indices.sort_unstable_by(|left, right| right.cmp(left));
                for index in runtime_indices {
                    let rt = self.remove_runtime(index);
                    let runtime_id = rt.id();
                    rt.app.drop_plugin_state(runtime_id);
                    rt.handles.cancel();
                }
                self.lineage.remove_sessions(&targets);
                targets.reverse();
                delete_sessions_sequentially(&self.ctx.storage_writer, targets, reply_tx);
            }
            SessionRequest::Live => {
                let list: Vec<_> = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        json!({
                            "id": rt.id(),
                            "title": rt.app.state.session.title,
                            "status": SessionStatus::of(&rt.app).as_str(),
                            "updated_at": rt.app.state.session.updated_at,
                            "focused": i == self.focused,
                            "cwd": rt.app.state.session.cwd,
                        })
                    })
                    .collect();
                let _ = reply_tx.send(Ok(json!(list)));
            }
            SessionRequest::Status { id } => {
                let reply = parse_session_id(&id).and_then(|id| {
                    let idx = self
                        .position(id)
                        .ok_or_else(|| format!("{NOT_LIVE_ERR}: {id}"))?;
                    let rt = &self.sessions[idx];
                    let history = rt.handles.history.load();
                    let assistant_output = history.iter().rev().find_map(|message| {
                        matches!(message.role, n00n_providers::Role::Assistant)
                            .then(|| message.first_text_content())
                            .flatten()
                    });
                    let direct_output = rt.app.state.session.meta.direct_output.as_deref();
                    let output = assistant_output.or(direct_output);
                    let direct_error = assistant_output
                        .is_none()
                        .then_some(rt.app.state.session.meta.direct_output_is_error)
                        .filter(|_| direct_output.is_some());
                    let paused_team = paused_team_run(&history).or_else(|| {
                        rt.app
                            .state
                            .session
                            .meta
                            .direct_paused_team
                            .as_ref()
                            .and_then(validated_paused_team_payload)
                    });
                    Ok(json!({
                        "id": rt.id(),
                        "title": rt.app.state.session.title,
                        "status": SessionStatus::of(&rt.app).as_str(),
                        "updated_at": rt.app.state.session.updated_at,
                        "focused": idx == self.focused,
                        "output": output,
                        "is_error": direct_error,
                        "paused_team": paused_team,
                        "cwd": rt.app.state.session.cwd,
                    }))
                });
                let _ = reply_tx.send(reply);
            }
            SessionRequest::Current => {
                let _ = reply_tx.send(Ok(json!(self.sessions[self.focused].id())));
            }
            SessionRequest::New {
                prompt,
                focus,
                parent_id,
                caller_id,
                bootstrap,
            } => {
                let reply = (|| {
                    let caller = caller_session_id(caller_id)?;
                    let explicit_parent = parent_id.as_deref().map(parse_session_id).transpose()?;
                    let execution_active = prompt.is_some() || bootstrap.is_some();
                    let reservation = self
                        .lineage
                        .reserve_new(caller, explicit_parent, execution_active)
                        .map_err(|error| error.to_string())?;
                    let caller_lineage = match self.lineage.lineage(caller) {
                        Ok(lineage) => lineage,
                        Err(error) => {
                            warn_lineage_cleanup(
                                self.lineage.release(reservation),
                                caller,
                                "release reservation",
                            );
                            return Err(error.to_string());
                        }
                    };
                    let mut session = {
                        let slot = self.ctx.model_slot.load();
                        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                        AppSession::new(&slot.model.spec(), &cwd.to_string_lossy())
                    };
                    session.meta.parent_id = Some(caller);
                    session.meta.root_session_id = Some(caller_lineage.root);
                    session.meta.lifecycle = StoredSessionLifecycle::Queued;
                    if let Some(bootstrap) = &bootstrap
                        && let Some(title) = &bootstrap.title
                    {
                        session.title = normalize_title(title);
                    }
                    let runtime = match self.ctx.spawn_runtime(session) {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            warn_lineage_cleanup(
                                self.lineage.release(reservation),
                                caller,
                                "release reservation",
                            );
                            return Err(error.to_string());
                        }
                    };
                    let id = runtime.id();
                    if let Err(error) = self.lineage.commit_new(reservation, id) {
                        runtime.handles.cancel();
                        return Err(error.to_string());
                    }
                    let idx = self.push_runtime(runtime);
                    let start_result = if let Some(bootstrap) = bootstrap {
                        let run_id = {
                            let runtime = &mut self.sessions[idx];
                            runtime.direct_bootstrap_active = true;
                            runtime.app.run_id += 1;
                            runtime.app.status = Status::Streaming;
                            runtime.app.state.session.meta.lifecycle =
                                StoredSessionLifecycle::Bootstrapping;
                            runtime.app.state.session.meta.queued_direct_tools =
                                vec![StoredDirectTool {
                                    tool: bootstrap.tool.clone(),
                                    input: bootstrap.input.clone(),
                                }];
                            runtime.app.run_id
                        };
                        self.sessions[idx]
                            .handles
                            .queue
                            .push(QueueItem::DirectTool {
                                run_id,
                                tool: bootstrap.tool,
                                input: bootstrap.input,
                            });
                        Ok(json!("started"))
                    } else if let Some(prompt) = prompt {
                        self.lineage
                            .set_execution_active(id, false)
                            .map_err(|error| error.to_string())
                            .and_then(|_| self.submit_text(idx, prompt, false, false))
                    } else {
                        self.sessions[idx].app.state.session.meta.lifecycle =
                            StoredSessionLifecycle::Idle;
                        warn_lineage_cleanup(
                            self.lineage.set_execution_active(id, false),
                            id,
                            "clear idle activity",
                        );
                        Ok(json!("idle"))
                    };
                    if let Err(error) = start_result {
                        let runtime = self.remove_runtime(idx);
                        runtime.handles.cancel();
                        warn_lineage_cleanup(
                            self.lineage.rollback_new(id),
                            id,
                            "roll back new session",
                        );
                        return Err(error);
                    }
                    self.sessions[idx]
                        .app
                        .save_session_without_plugin_state_capture();
                    if focus {
                        self.set_focus(idx);
                    }
                    Ok(json!(id))
                })();
                let _ = reply_tx.send(reply);
            }
            SessionRequest::Prompt {
                id,
                text,
                steer,
                control,
                caller_id,
                host_control,
            } => {
                let reply = (|| {
                    let explicit_target = id.as_deref().map(parse_session_id).transpose()?;
                    let target = if host_control {
                        explicit_target
                            .ok_or_else(|| "host control requires a target session".to_owned())?
                    } else {
                        let caller = caller_session_id(caller_id)?;
                        self.lineage
                            .authorize_prompt(caller, explicit_target)
                            .map_err(|error| error.to_string())?
                    };
                    let idx = self
                        .position(target)
                        .ok_or_else(|| format!("{NOT_LIVE_ERR}: {target}"))?;
                    let activated = self
                        .lineage
                        .begin_execution(target)
                        .map_err(|error| error.to_string())?;
                    match self.submit_text(idx, text, steer, control) {
                        Ok(state) => {
                            let meta = &mut self.sessions[idx].app.state.session.meta;
                            meta.lifecycle = StoredSessionLifecycle::Running;
                            meta.direct_paused_team = None;
                            Ok(state)
                        }
                        Err(error) => {
                            if activated {
                                warn_lineage_cleanup(
                                    self.lineage.set_execution_active(target, false),
                                    target,
                                    "roll back prompt activity",
                                );
                            }
                            Err(error)
                        }
                    }
                })();
                let _ = reply_tx.send(reply);
            }
            SessionRequest::Cancel {
                id,
                caller_id,
                host_control,
            } => {
                let reply = (|| {
                    let requested = parse_session_id(&id)?;
                    let target = if host_control {
                        requested
                    } else {
                        let caller = caller_session_id(caller_id)?;
                        self.lineage
                            .authorize_prompt(caller, Some(requested))
                            .map_err(|error| error.to_string())?
                    };
                    let mut targets = self
                        .lineage
                        .descendants_of(target)
                        .map_err(|error| error.to_string())?;
                    targets.push(target);
                    let mut cancelled = false;
                    for session_id in targets {
                        let Some(idx) = self.position(session_id) else {
                            let mut session = match self
                                .ctx
                                .storage_writer
                                .latest_snapshot(session_id)
                                .map_err(|error| error.to_string())?
                            {
                                Some(session) => Arc::unwrap_or_clone(session),
                                None => AppSession::load_with_retention(
                                    session_id,
                                    &self.ctx.storage,
                                    self.ctx.retention_budget,
                                    message_tool_use_ids,
                                )
                                .map_err(|error| error.to_string())?,
                            };
                            if cancel_stored_session(&mut session) {
                                cancelled = true;
                                self.ctx.storage_writer.send(Box::new(session));
                                warn_lineage_cleanup(
                                    self.lineage.set_execution_active(session_id, false),
                                    session_id,
                                    "clear cancelled activity",
                                );
                            }
                            continue;
                        };
                        if SessionStatus::of(&self.sessions[idx].app) != SessionStatus::Idle
                            || self.sessions[idx]
                                .app
                                .state
                                .session
                                .meta
                                .lifecycle
                                .is_active()
                            || !self.sessions[idx].app.queue.is_empty()
                            || has_restorable_work(&self.sessions[idx].app.state.session)
                        {
                            let actions = self.sessions[idx].app.cancel_current_run();
                            self.dispatch(idx, actions);
                            let meta = &mut self.sessions[idx].app.state.session.meta;
                            meta.lifecycle = StoredSessionLifecycle::Cancelled;
                            meta.direct_paused_team = None;
                            self.sessions[idx]
                                .app
                                .save_session_without_plugin_state_capture();
                            cancelled = true;
                        }
                        warn_lineage_cleanup(
                            self.lineage.set_execution_active(session_id, false),
                            session_id,
                            "clear cancelled activity",
                        );
                    }
                    if !cancelled {
                        return Err(format!("session is idle: {target}"));
                    }
                    Ok(json!(true))
                })();
                let _ = reply_tx.send(reply);
            }
            SessionRequest::Focus { id } => {
                let reply = parse_session_id(&id)
                    .and_then(|id| self.focus_session(id))
                    .map(|()| json!(true));
                let _ = reply_tx.send(reply);
            }
            SessionRequest::SetTitle { id, title } => {
                let title = normalize_title(&title);
                let reply = (|| {
                    let id = parse_session_id(&id)?;
                    if let Some(i) = self.position(id) {
                        let app = &mut self.sessions[i].app;
                        app.state.session.title = title;
                        app.save_session_without_plugin_state_capture();
                    } else {
                        let mut session = AppSession::load_with_retention(
                            id,
                            &self.ctx.storage,
                            self.ctx.retention_budget,
                            message_tool_use_ids,
                        )
                        .map_err(|e| e.to_string())?;
                        session.title = title;
                        session.updated_at = n00n_storage::now_epoch();
                        self.ctx.storage_writer.send(Box::new(session));
                    }
                    Ok(json!(true))
                })();
                let _ = reply_tx.send(reply);
            }
        }
    }

    fn submit_text(
        &mut self,
        idx: usize,
        text: String,
        steer: bool,
        control: bool,
    ) -> SessionReply {
        let msg = QueuedMessage {
            text,
            images: Vec::new(),
            control,
        };
        let outcome = if steer {
            self.sessions[idx].app.submit_control_prompt(msg)
        } else {
            self.sessions[idx].app.submit_background_prompt(msg)
        };
        match outcome {
            SubmitOutcome::Started(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("started"))
            }
            SubmitOutcome::Queued => Ok(json!("queued")),
            SubmitOutcome::Rejected(e) => Err(e.into()),
        }
    }

    fn position(&self, id: n00nId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    fn current_revision_for(&self, idx: usize) -> u64 {
        let session = &self.sessions[idx].app.state.session;
        let revision = current_state_revision(session);
        let root_id = persisted_root_id(session);
        if root_id == session.id {
            return revision;
        }
        let root_revision = if let Some(root_idx) = self.position(root_id) {
            current_state_revision(&self.sessions[root_idx].app.state.session)
        } else {
            match self.ctx.storage_writer.latest_snapshot(root_id) {
                Ok(Some(root)) => current_state_revision(&root),
                Ok(None) => match AppSession::load_with_retention(
                    root_id,
                    &self.ctx.storage,
                    self.ctx.retention_budget,
                    message_tool_use_ids,
                ) {
                    Ok(root) => current_state_revision(&root),
                    Err(error) => {
                        warn!(session_id = %session.id, %root_id, %error, "root state revision unavailable; using session revision");
                        return revision;
                    }
                },
                Err(error) => {
                    warn!(session_id = %session.id, %root_id, %error, "current root state revision unavailable; using session revision");
                    return revision;
                }
            }
        };
        revision.max(root_revision)
    }

    /// The single place that removes a runtime: keeps `focused` pointing at
    /// the same session afterwards. The focused runtime itself is never
    /// removable, so `sessions` stays non-empty.
    fn remove_runtime(&mut self, idx: usize) -> SessionRuntime {
        debug_assert_ne!(idx, self.focused);
        let rt = self.sessions.remove(idx);
        self.pending_state_captures.remove(&rt.id());
        self.deferred_state_captures.remove(&rt.id());
        let root_id = persisted_root_id(&rt.app.state.session);
        if !self
            .sessions
            .iter()
            .any(|runtime| persisted_root_id(&runtime.app.state.session) == root_id)
        {
            self.ctx
                .hydration_requested_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&root_id);
            self.ctx.revision_allocators.borrow_mut().remove(&root_id);
        }
        if let Err(error) = self.lineage.remove_runtime(rt.id()) {
            warn!(session_id = %rt.id(), error = %error, "failed to remove session runtime from lineage");
        }
        if idx < self.focused {
            self.focused -= 1;
        }
        rt
    }

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }
    fn set_focus(&mut self, idx: usize) {
        if idx == self.focused {
            return;
        }
        self.schedule_plugin_state_capture(self.focused);
        self.sessions[self.focused]
            .app
            .save_session_without_plugin_state_capture();
        self.focused = idx;
        self.sessions[self.focused].app.fire_session_focus_autocmd();
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: n00nId) -> Result<(), String> {
        if let Some(i) = self.position(id) {
            self.set_focus(i);
            return Ok(());
        }
        let session = match self
            .ctx
            .storage_writer
            .latest_snapshot(id)
            .map_err(|error| format!("Failed to load pending session state: {error}"))?
        {
            Some(session) => Arc::unwrap_or_clone(session),
            None => AppSession::load_with_retention(
                id,
                &self.ctx.storage,
                self.ctx.retention_budget,
                message_tool_use_ids,
            )
            .map_err(|error| format!("Failed to load session: {error}"))?,
        };
        let restore_execution = has_restorable_work(&session);
        let mut live = live_session(&session).map_err(|error| error.to_string())?;
        live.execution_active = false;
        self.lineage
            .activate_runtime(live)
            .map_err(|error| error.to_string())?;
        if restore_execution && let Err(error) = self.lineage.begin_execution(id) {
            warn_lineage_cleanup(
                self.lineage.remove_runtime(id),
                id,
                "roll back restored execution activation",
            );
            return Err(error.to_string());
        }
        let runtime = match self.ctx.spawn_runtime(session) {
            Ok(runtime) => runtime,
            Err(error) => {
                warn_lineage_cleanup(
                    self.lineage.remove_runtime(id),
                    id,
                    "roll back runtime activation",
                );
                return Err(error.to_string());
            }
        };
        let idx = self.push_runtime(runtime);
        self.set_focus(idx);
        Ok(())
    }

    /// Handles a bounded number of coalescer leftovers, retaining excess ahead
    /// of input that is still queued in the reader.
    fn handle_input(&mut self, raw: Event) {
        let leftover = handle_input_bounded(raw, |event| {
            let (msg, leftover) = self.translate(event);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
            }
            leftover
        });
        if let Some(event) = leftover {
            self.pending_input.borrow_mut().push_front(event);
        }
    }

    fn translate(&mut self, raw: Event) -> (Option<Msg>, Option<Event>) {
        match raw {
            Event::Resize(..) => {
                self.dirty = true;
                (None, None)
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => (Some(Msg::Key(key)), None),
            Event::Paste(text) => (Some(Msg::Paste(text)), None),
            Event::Mouse(mouse) => self.translate_mouse(mouse),
            _ => (None, None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> (Option<Msg>, Option<Event>) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll_lines = self.focused_app().ui_config.mouse_scroll_lines;
                let (msg, leftover) = self.aggregate_scroll(mouse, scroll_lines);
                (Some(msg), leftover)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, leftover) = self.coalesce_drag(mouse);
                (Some(Msg::Mouse(drag)), leftover)
            }
            _ => (Some(Msg::Mouse(mouse)), None),
        }
    }

    /// Sums queued scroll events into one delta; the first non-scroll event
    /// drained along the way is returned so it isn't lost.
    fn aggregate_scroll(&self, first: CtMouseEvent, scroll_lines: u32) -> (Msg, Option<Event>) {
        aggregate_scroll(first, scroll_lines, || {
            try_recv_input(self.input.receiver())
        })
    }

    /// Keeps only the newest queued drag position; the first non-drag event
    /// drained along the way is returned so it isn't lost.
    fn coalesce_drag(&self, latest: CtMouseEvent) -> (CtMouseEvent, Option<Event>) {
        coalesce_drag(latest, || try_recv_input(self.input.receiver()))
    }

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::SendMessage(dispatch) if dispatch.paint_required => {
                    self.post_draw_submissions
                        .push((self.sessions[idx].id(), *dispatch));
                }
                Action::SendMessage(dispatch) => {
                    self.handle_action(idx, Action::SendMessage(dispatch));
                }
                action => self.handle_action(idx, action),
            }
        }
    }

    /// The optimistic user bubble must have completed a terminal draw before
    /// persistence or provider dispatch can begin.
    fn after_terminal_draw(&mut self) {
        let painted_session = self.sessions[self.focused].id();
        for (_, dispatch) in
            take_painted_submissions(&mut self.post_draw_submissions, painted_session)
        {
            self.handle_action(self.focused, Action::SendMessage(Box::new(dispatch)));
        }
    }

    fn respawn_agent(
        &mut self,
        idx: usize,
        history: Vec<Message>,
        transcript: Vec<TranscriptEntry<Message>>,
        identity: SessionIdentity,
    ) {
        let initial_state_revision = self.current_revision_for(idx);
        let rt = &mut self.sessions[idx];
        let lua_handle = rt.app.lua_event_handle.clone();
        let permissions = Arc::clone(&rt.app.permissions);
        rt.handles.respawn(
            history,
            transcript,
            initial_state_revision,
            &self.ctx.model_slot,
            self.ctx.config.clone(),
            self.ctx.ui_config.tool_output_lines,
            &permissions,
            &mut rt.app,
            Some(identity),
            lua_handle,
        );
    }

    fn handle_submission_persisted(&mut self, completion: SubmissionPersistence) {
        let Some(idx) = self.position(completion.session_id) else {
            return;
        };
        let rt = &mut self.sessions[idx];
        if completion.result.is_err() {
            rt.app
                .handle_submission_persistence_failure(&completion.dispatch);
            if completion.execution_started && rt.app.queue.is_empty() {
                warn_lineage_cleanup(
                    self.lineage
                        .set_execution_active(completion.session_id, false),
                    completion.session_id,
                    "release failed submission activity",
                );
            }
            return;
        }
        if !rt.app.accepts_submission_persistence(&completion.dispatch) {
            rt.app
                .queue
                .remove_submission(completion.dispatch.submission_id);
            rt.app.save_session_without_plugin_state_capture();
            if completion.execution_started && rt.app.queue.is_empty() {
                warn_lineage_cleanup(
                    self.lineage
                        .set_execution_active(completion.session_id, false),
                    completion.session_id,
                    "release superseded submission activity",
                );
            }
            return;
        }
        let submission_id = completion.dispatch.submission_id;
        if rt
            .app
            .queue
            .mark_submission_ready(submission_id, completion.dispatch.input)
        {
            rt.app.state.session.meta.direct_paused_team = None;
        } else {
            rt.app.queue.remove_submission(submission_id);
            rt.app.save_session_without_plugin_state_capture();
            if completion.execution_started && rt.app.queue.is_empty() {
                warn_lineage_cleanup(
                    self.lineage
                        .set_execution_active(completion.session_id, false),
                    completion.session_id,
                    "release removed submission activity",
                );
            }
        }
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::SendMessage(mut dispatch) => {
                let session_id = self.sessions[idx].id();
                let execution_started = match self.lineage.begin_execution(session_id) {
                    Ok(started) => started,
                    Err(error) => {
                        self.sessions[idx]
                            .app
                            .handle_submission_failure(&dispatch, &error.to_string());
                        return;
                    }
                };
                let rt = &mut self.sessions[idx];
                if !rt.app.stage_submission_preamble(&mut dispatch) {
                    rt.app.queue.remove_submission(dispatch.submission_id);
                    if execution_started {
                        warn_lineage_cleanup(
                            self.lineage.set_execution_active(session_id, false),
                            session_id,
                            "release rejected submission activity",
                        );
                    }
                    return;
                }
                let session_id = rt.app.state.session.id;
                let mut snapshot = rt.app.session_snapshot();
                snapshot.meta.direct_paused_team = None;
                let completion_tx = self.submission_persist_tx.clone();
                self.ctx
                    .storage_writer
                    .persist(Box::new(snapshot), move |result| {
                        let _ = completion_tx.send(SubmissionPersistence {
                            session_id,
                            dispatch: *dispatch,
                            execution_started,
                            result,
                        });
                    });
            }
            Action::CancelAgent { run_id } => {
                let id = self.sessions[idx].id();
                if let Err(error) = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::Cancel { run_id })
                {
                    warn!(session_id = %id, %error, "failed to send agent cancellation");
                }
                self.sessions[idx].app.state.session.meta.lifecycle =
                    StoredSessionLifecycle::Cancelled;
                self.sessions[idx]
                    .app
                    .state
                    .session
                    .meta
                    .queued_direct_tools
                    .clear();
                self.sessions[idx].app.state.session.meta.direct_paused_team = None;
                warn_lineage_cleanup(
                    self.lineage.set_execution_active(id, false),
                    id,
                    "clear keyboard-cancelled activity",
                );
                self.sessions[idx]
                    .app
                    .save_session_without_plugin_state_capture();
            }
            Action::CancelSubagent { tool_use_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::NewSession { previous_id } => {
                let replacement = match live_session(&self.sessions[idx].app.state.session) {
                    Ok(replacement) => replacement,
                    Err(error) => {
                        warn!(session_id = %self.sessions[idx].id(), %error, "invalid reset session lineage");
                        self.sessions[idx].app.status = Status::error(error.to_string());
                        return;
                    }
                };
                let identity = match session_identity(&self.sessions[idx].app.state.session) {
                    Ok(identity) => identity,
                    Err(error) => {
                        warn!(session_id = %replacement.id, %error, "invalid reset session identity");
                        self.sessions[idx].app.status = Status::error(error.to_string());
                        return;
                    }
                };
                if let Err(error) = self.lineage.replace_runtime(previous_id, replacement) {
                    warn!(
                        previous_session_id = %previous_id,
                        replacement_session_id = %replacement.id,
                        %error,
                        "failed to replace reset session lineage"
                    );
                    self.sessions[idx].app.status = Status::error(error.to_string());
                    return;
                }
                self.respawn_agent(idx, Vec::new(), Vec::new(), identity);
                if let Some(pending) = self.sessions[idx].app.pending_plan_submit.take() {
                    let actions = {
                        let app = &mut self.sessions[idx].app;
                        if let Some((content, path)) = pending.plan {
                            app.main_chat()
                                .push(DisplayMessage::plan(content, path.clone()));
                            app.state.plan =
                                crate::app::PlanState::Ready(std::path::PathBuf::from(path));
                        }
                        app.run_id += 1;
                        app.start_from_queue(&pending.message)
                    };
                    self.dispatch(idx, actions);
                }
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                let discovered = self.ctx.available_models.load_full();
                if loaded.model_spec != self.ctx.model_slot.load().model.spec()
                    && let Ok(mut new_model) = resolve_model_selection(
                        &loaded.model_spec,
                        discovered.as_deref().map(Vec::as_slice),
                    )
                    && let Ok(new_provider) = from_model_with_openai_options(
                        &mut new_model,
                        self.ctx.timeouts,
                        self.ctx.openai_options.clone(),
                    )
                {
                    self.sessions[idx].app.usage_slot.store(None);
                    self.ctx.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                let identity = match session_identity(&self.sessions[idx].app.state.session) {
                    Ok(identity) => identity,
                    Err(error) => {
                        warn!(session_id = %self.sessions[idx].id(), %error, "invalid loaded session identity");
                        return;
                    }
                };
                let hydrated =
                    loaded.plugin_state_hydrated || self.sessions[idx].app.hydrate_plugin_state();
                if hydrated {
                    let session = &self.sessions[idx].app.state.session;
                    let root_id = persisted_root_id(session);
                    if root_id == session.id {
                        let revision = session
                            .meta
                            .state_snapshot
                            .as_ref()
                            .and_then(StoredSessionStateSnapshot::state_revision);
                        self.ctx
                            .hydration_requested_roots
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(root_id, revision);
                    }
                }
                self.respawn_agent(idx, loaded.messages, loaded.transcript, identity);
                *self.sessions[idx]
                    .handles
                    .tool_outputs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = loaded.tool_outputs;
            }
            Action::ChangeModel(spec) => self.change_model(&spec),
            Action::RefreshProvider { slug } => self.refresh_provider(&slug),
            Action::AssignTier(spec, tier) => {
                n00n_providers::model_registry::set_and_persist(spec, tier, &self.ctx.storage);
            }
            Action::UnassignTier(spec, tier) => {
                n00n_providers::model_registry::unset_and_persist(&spec, tier, &self.ctx.storage);
            }
            Action::Compact => {
                let rt = &mut self.sessions[idx];
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Compact { run_id });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let rt = &mut self.sessions[idx];
                let (trigger, cancel) = CancelToken::new();
                rt.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    rt.shell_tx.clone(),
                    cancel,
                    &self.ctx.config,
                );
            }
            Action::OpenEditor(path) => {
                self.open_editor(idx, &path);
            }
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                let result = match self.input.pause() {
                    Ok(_pause) => terminal::edit_temp_content(&current_text, self.terminal),
                    Err(e) => Err(e),
                };
                match result {
                    Ok(edited) => self.sessions[idx].app.input_box.set_input(&edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.ctx.model_slot.load();
                self.sessions[idx].app.start_btw(
                    &question,
                    Arc::clone(&slot.provider),
                    slot.model.clone(),
                );
            }
            Action::Suspend => match self.input.pause() {
                Ok(_pause) => terminal::suspend(self.terminal),
                Err(e) => self.sessions[idx].app.flash(e),
            },
            Action::Redraw => {
                if let Err(e) = self.terminal.clear() {
                    self.sessions[idx].app.flash(e.to_string());
                }
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
        }
    }

    fn change_model(&mut self, spec: &str) {
        let discovered = self.ctx.available_models.load_full();
        match resolve_model_selection(spec, discovered.as_deref().map(Vec::as_slice)) {
            Ok(mut new_model) => match from_model_with_openai_options(
                &mut new_model,
                self.ctx.timeouts,
                self.ctx.openai_options.clone(),
            ) {
                Ok(new_provider) => {
                    let app = self.focused_app();
                    app.update_model(&new_model);
                    app.record_recent_model(&new_model.spec());
                    app.usage_slot.store(None);
                    self.ctx.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                Err(e) => {
                    let msg = format!("Failed to create provider: {e}");
                    self.focused_app()
                        .main_chat()
                        .push(DisplayMessage::new(DisplayRole::Error, msg.clone()));
                    self.focused_app().flash(msg);
                }
            },
            Err(e) => {
                let msg = format!("Invalid model: {e}");
                self.focused_app()
                    .main_chat()
                    .push(DisplayMessage::new(DisplayRole::Error, msg.clone()));
                self.focused_app().flash(msg);
            }
        }
    }

    fn refresh_models(&self) {
        let available = Arc::clone(&self.ctx.available_models);
        let refreshed = Arc::new(ArcSwapOption::empty());
        let warn_tx = self.warn_tx.clone();
        let current_generation = Arc::clone(&self.model_refresh_generation);
        let generation = {
            let mut current = current_generation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = current.saturating_add(1);
            *current
        };
        smol::spawn(async move {
            fetch_all_models(
                |batch| {
                    merge_model_batch(&refreshed, batch, &warn_tx, generation, &current_generation);
                },
                None,
            )
            .await;
            publish_model_refresh(
                &available,
                refreshed.load_full(),
                generation,
                &current_generation,
            );
        })
        .detach();
    }

    fn refresh_usage(&mut self) {
        let provider = Arc::clone(&self.ctx.model_slot.load().provider);
        let slot = Arc::clone(&self.focused_app().usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        smol::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    fn refresh_provider(&mut self, slug: &str) {
        let mut model = self.ctx.model_slot.load().model.clone();
        if model.provider.to_string() == slug {
            if let Ok(provider) =
                n00n_providers::provider::from_model(&mut model, self.ctx.timeouts)
            {
                self.focused_app().usage_slot.store(None);
                self.ctx.model_slot.store(Arc::new(ModelSlot {
                    model,
                    provider: Arc::from(provider),
                }));
            }
        } else if let Some(builtin) = n00n_config::providers::builtin_provider(slug) {
            self.change_model(builtin.default_model);
        }
    }
    fn preserve_post_draw_submissions(&mut self) {
        for (session_id, dispatch) in std::mem::take(&mut self.post_draw_submissions)
            .into_iter()
            .rev()
        {
            let Some(idx) = self.position(session_id) else {
                warn!(%session_id, "paint-gated submission lost its session before shutdown");
                continue;
            };
            self.sessions[idx]
                .app
                .preserve_submission_for_shutdown(dispatch);
        }
    }

    fn settle_cancelled_sessions(&mut self) {
        let Some(deadline) = Instant::now().checked_add(AGENT_SHUTDOWN_TIMEOUT) else {
            warn!("shutdown cancellation deadline overflowed");
            return;
        };
        while self
            .sessions
            .iter()
            .any(|runtime| SessionStatus::of(&runtime.app).needs_shutdown_settle())
            && Instant::now() < deadline
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = remaining.min(Duration::from_millis(IDLE_POLL_INTERVAL_MS));
            if let Some(wake) = self.select_non_input_wake(timeout)
                && let Err(error) = self.handle_wake(wake)
            {
                warn!(%error, "failed to settle a cancellation event during shutdown");
            }
        }
    }

    fn settle_state_captures(&mut self) {
        for idx in 0..self.sessions.len() {
            self.schedule_plugin_state_capture(idx);
        }
        let Some(deadline) = Instant::now().checked_add(TERMINAL_CHECKPOINT_TIMEOUT) else {
            warn!("plugin state capture shutdown deadline overflowed");
            return;
        };
        while (!self.pending_state_captures.is_empty()
            || self
                .sessions
                .iter()
                .any(|runtime| !runtime.pending_compactions.is_empty()))
            && Instant::now() < deadline
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = remaining.min(Duration::from_millis(IDLE_POLL_INTERVAL_MS));
            if let Some(wake) = self.select_non_input_wake(timeout)
                && let Err(error) = self.handle_wake(wake)
            {
                warn!(%error, "failed to settle plugin state capture during shutdown");
            }
        }
        let pending_compactions = self
            .sessions
            .iter()
            .map(|runtime| runtime.pending_compactions.len())
            .sum::<usize>();
        if !self.pending_state_captures.is_empty() || pending_compactions != 0 {
            warn!(
                pending_state_captures = self.pending_state_captures.len(),
                pending_compactions, "timed out waiting for state capture during shutdown"
            );
        }
        for idx in 0..self.sessions.len() {
            while let Some(mut pending) = self.sessions[idx].pending_compactions.pop_front() {
                pending.attempts = MAX_COMPACTION_CHECKPOINT_ATTEMPTS;
                pending.stage = CompactionPersistStage::Ready;
                self.handle_compaction_failure(idx, pending, COMPACTION_SHUTDOWN_TIMEOUT_ERROR);
            }
        }
    }

    fn shutdown(mut self) -> Result<ShutdownReport> {
        self.preserve_post_draw_submissions();
        let exit = self.sessions[self.focused].app.exit_request;
        for rt in &self.sessions {
            let _ = rt.handles.cmd_tx.try_send(AgentCommand::CancelAll);
        }
        self.settle_cancelled_sessions();
        self.settle_state_captures();
        self.preserve_post_draw_submissions();
        let mut tabs = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            app.save_session_without_plugin_state_capture();
            // `app` drops at the end of this iteration, closing the
            // channels the agent loop waits on, so `join_all` can finish.
            tabs.push(app.state.session);
            agent_tasks.push(handles.into_task());
        }
        if let Some(ref h) = self.ctx.mcp_handle {
            smol::block_on(h.shutdown());
        }
        crate::agent::join_all(agent_tasks, AGENT_SHUTDOWN_TIMEOUT);
        let storage_result = match Arc::try_unwrap(self.ctx.storage_writer) {
            Ok(writer) => writer
                .shutdown(STORAGE_WRITER_SHUTDOWN_TIMEOUT)
                .map_err(Into::into),
            Err(_) => Err(eyre!(STORAGE_WRITER_REFS_ERR)),
        };
        let report = ShutdownReport {
            exit,
            tabs,
            focused: self.focused,
        };
        storage_result?;
        Ok(report)
    }
}

fn draw_then_post_terminal<B>(
    terminal: &mut ratatui::Terminal<B>,
    draw: impl FnOnce(&mut ratatui::Frame<'_>),
    after_draw: impl FnOnce(),
) -> Result<(), B::Error>
where
    B: ratatui::backend::Backend,
{
    terminal.draw(|f| {
        draw(f);
        color_compat::downgrade_if_needed(f.buffer_mut());
    })?;
    after_draw();
    Ok(())
}

fn take_painted_submissions<T>(
    pending: &mut Vec<(n00nId, T)>,
    painted_session: n00nId,
) -> Vec<(n00nId, T)> {
    let submissions = std::mem::take(pending);
    let mut ready = Vec::new();
    for (session_id, submission) in submissions {
        if session_id == painted_session {
            ready.push((session_id, submission));
        } else {
            pending.push((session_id, submission));
        }
    }
    ready
}

fn should_save_periodically(status: &Status, awaiting_input: bool) -> bool {
    *status == Status::Streaming && !awaiting_input
}

fn startup_provider_with(
    model: &mut Model,
    needs_login: bool,
    create: impl FnOnce(&mut Model) -> Result<Box<dyn Provider>, n00n_providers::AgentError>,
) -> (Box<dyn Provider>, Option<String>) {
    if needs_login {
        return (unconfigured_provider(), None);
    }
    match create(model) {
        Ok(provider) => (provider, None),
        Err(error) => (
            unconfigured_provider(),
            Some(format!("Failed to create provider: {error}")),
        ),
    }
}

#[allow(clippy::manual_ok_err)]
fn try_recv_input(rx: &flume::Receiver<Event>) -> Option<Event> {
    match rx.try_recv() {
        Ok(event) => Some(event),
        Err(flume::TryRecvError::Empty | flume::TryRecvError::Disconnected) => None,
    }
}

fn handle_input_bounded(
    first: Event,
    mut handle: impl FnMut(Event) -> Option<Event>,
) -> Option<Event> {
    let mut pending = Some(first);
    for _ in 0..HANDLE_INPUT_BUDGET {
        let Some(event) = pending.take() else {
            break;
        };
        pending = handle(event);
    }
    pending
}

fn aggregate_scroll(
    first: CtMouseEvent,
    scroll_lines: u32,
    mut next_event: impl FnMut() -> Option<Event>,
) -> (Msg, Option<Event>) {
    let mut delta = scroll_delta(first.kind, scroll_lines);
    for _ in 0..COALESCE_BUDGET {
        let Some(next) = next_event() else {
            break;
        };
        match next {
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                delta += scroll_delta(mouse.kind, scroll_lines);
            }
            other => {
                return (
                    Msg::Scroll {
                        column: first.column,
                        row: first.row,
                        delta,
                    },
                    Some(other),
                );
            }
        }
    }
    (
        Msg::Scroll {
            column: first.column,
            row: first.row,
            delta,
        },
        None,
    )
}

fn coalesce_drag(
    mut latest: CtMouseEvent,
    mut next_event: impl FnMut() -> Option<Event>,
) -> (CtMouseEvent, Option<Event>) {
    for _ in 0..COALESCE_BUDGET {
        let Some(next) = next_event() else {
            break;
        };
        match next {
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) =>
            {
                latest = mouse;
            }
            other => return (latest, Some(other)),
        }
    }
    (latest, None)
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    let lines = crate::cast::u32_to_isize(lines);
    let n = i32::try_from(lines).unwrap_or_else(|_| i32::MAX);
    if kind == MouseEventKind::ScrollUp {
        n
    } else {
        -n
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COALESCE_BUDGET, CompactionPersistStage, DELETE_UI_ONLY_ERR, DIRECT_OUTPUT_MAX_BYTES,
        DRAIN_BUDGET, DrainScheduler, HANDLE_INPUT_BUDGET, MAX_COMPACTION_CHECKPOINT_ATTEMPTS, Msg,
        PAUSED_TEAM_RUN_ID_MAX_BYTES, PendingCompaction, SessionStatus, TEAM_TOOL_NAME,
        TERMINAL_CHECKPOINT_TIMEOUT, aggregate_scroll, authorize_ui_delete, begin_state_capture,
        bounded_direct_output, cancel_stored_session, capture_revision_matches, coalesce_drag,
        complete_model_fetch_with, direct_paused_team_payload, draw_then_post_terminal,
        handle_input_bounded, initial_state_revision, merge_compaction_metadata, merge_model_batch,
        outer_compaction_state_revision, paused_team_payload, paused_team_run,
        prepare_compaction_checkpoint, publish_model_refresh, resolve_model_selection,
        resume_state_snapshot, should_save_periodically, startup_login_completed,
        startup_provider_with, take_painted_submissions, try_recv_input,
        validated_paused_team_payload,
    };
    use crate::{AppSession, agent::ModelSlot, components::Status};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use n00n_agent::{AgentConfig, AgentEvent, Envelope};
    use n00n_providers::{
        AgentError, ContentBlock, Message, Model, ModelCatalog, ModelCatalogError, Role,
        provider::{ModelBatch, unconfigured_provider},
    };
    use n00n_storage::{
        StateDir,
        id::{SessionRef, n00nId},
        sessions::{
            StoredDelivery, StoredDirectTool, StoredQueuedMessage, StoredSessionLifecycle,
            StoredSessionStateSnapshot, StoredStateScope, TranscriptEntry,
        },
    };
    use ratatui::{
        Terminal,
        backend::{Backend, ClearType, TestBackend, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
        widgets::Paragraph,
    };
    use std::{
        cell::Cell as Counter,
        collections::HashSet,
        io,
        sync::{Arc, Mutex},
    };
    use tempfile::TempDir;
    use test_case::test_case;

    #[test_case(5, 5, true; "matching revision")]
    #[test_case(4, 5, false; "mismatched revision")]
    fn stored_root_capture_revision_must_match(
        snapshot_revision: u64,
        captured_revision: u64,
        expected: bool,
    ) {
        let snapshot = StoredSessionStateSnapshot::new(snapshot_revision);

        assert_eq!(
            capture_revision_matches(&snapshot, captured_revision),
            expected
        );
    }

    #[test]
    fn state_capture_requests_are_coalesced() {
        let session_id = n00nId::generate();
        let mut pending = HashSet::new();
        let mut deferred = HashSet::new();

        assert!(begin_state_capture(&mut pending, &mut deferred, session_id));
        assert!(!begin_state_capture(
            &mut pending,
            &mut deferred,
            session_id
        ));
        assert_eq!(pending.len(), 1);
        assert_eq!(deferred, HashSet::from([session_id]));
    }

    #[test]
    fn compaction_checkpoint_retry_budget_is_bounded() {
        let mut pending = PendingCompaction {
            ack: Box::new(Envelope {
                event: AgentEvent::CompactionDone {
                    state_revision: Some(1),
                },
                subagent: None,
                run_id: 1,
            }),
            transcript: None,
            revision: 1,
            prepared: None,
            attempts: 0,
            stage: CompactionPersistStage::Ready,
        };

        for _ in 1..MAX_COMPACTION_CHECKPOINT_ATTEMPTS {
            assert!(pending.should_retry_after_failure());
        }
        assert!(!pending.should_retry_after_failure());
    }

    #[test]
    fn shutdown_settle_ignores_sessions_waiting_for_input() {
        assert!(SessionStatus::Working.needs_shutdown_settle());
        assert!(!SessionStatus::NeedsInput.needs_shutdown_settle());
        assert!(!SessionStatus::Idle.needs_shutdown_settle());
    }

    #[test]
    fn older_model_refresh_cannot_overwrite_newer_catalog() {
        let available = ArcSwapOption::from(Some(Arc::new(vec!["initial/model".into()])));
        let current_generation = Mutex::new(2);
        let newer = Some(Arc::new(vec!["newer/model".into()]));
        let older = Some(Arc::new(vec!["older/model".into()]));

        assert!(publish_model_refresh(
            &available,
            newer,
            2,
            &current_generation
        ));
        assert!(!publish_model_refresh(
            &available,
            older,
            1,
            &current_generation
        ));
        assert_eq!(
            available.load().as_deref().unwrap().as_slice(),
            ["newer/model"]
        );
    }

    #[test]
    fn startup_model_batch_cannot_merge_after_manual_refresh() {
        let available = ArcSwapOption::from(Some(Arc::new(vec!["manual/model".into()])));
        let generation = Mutex::new(1);
        let (warn_tx, warn_rx) = flume::unbounded();
        let batch = ModelBatch {
            models: vec!["startup/model".into()],
            warnings: vec!["stale warning".into()],
        };

        assert!(!merge_model_batch(
            &available,
            batch,
            &warn_tx,
            0,
            &generation
        ));
        assert_eq!(
            available.load().as_deref().unwrap().as_slice(),
            ["manual/model"]
        );
        assert!(warn_rx.try_recv().is_err());
    }
    #[test_case("unconfigured-nested/vendor/deepseek-v4-flash"; "discovered_nested_model_spec")]
    fn discovered_nested_model_spec_is_catalogued_for_selection(spec: &str) {
        ModelCatalog::from_specs([spec.to_string()])
            .with_alias("nested", spec)
            .unwrap();
        let result = resolve_model_selection(spec, Some(&[spec.to_string()]));
        assert!(matches!(result, Err(ModelCatalogError::Unavailable(_))));
    }

    fn compaction_entry(
        entries: Vec<TranscriptEntry<Message>>,
        revision: u64,
    ) -> TranscriptEntry<Message> {
        TranscriptEntry::Compaction {
            entries,
            generated_summary: None,
            state_revision: Some(revision),
        }
    }

    #[test]
    fn compaction_checkpoint_persists_exact_outer_revision_and_latest_state() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let writer = crate::storage_writer::StorageWriter::new(dir.clone()).unwrap();
        let mut session = AppSession::new("model", "/project");
        session.transcript = vec![compaction_entry(Vec::new(), 7)];
        let mut previous = StoredSessionStateSnapshot::new(4);
        previous
            .set_plugin_state(
                "plugin",
                1,
                StoredStateScope::Session,
                serde_json::json!({"kept": true}),
            )
            .unwrap();
        session.meta.state_snapshot = Some(previous);
        session.meta.revision = 2;

        let revision = outer_compaction_state_revision(&session.transcript).unwrap();
        prepare_compaction_checkpoint(None, &mut session, revision).unwrap();
        writer
            .persist_and_wait(Box::new(session.clone()), TERMINAL_CHECKPOINT_TIMEOUT)
            .unwrap();

        let durable = AppSession::load(session.id, &dir).unwrap();
        assert_eq!(durable.meta.revision, 3);
        assert_eq!(
            durable
                .meta
                .compaction_state_at(7)
                .unwrap()
                .state_revision(),
            Some(7)
        );
        assert_eq!(
            durable
                .meta
                .state_snapshot
                .as_ref()
                .and_then(StoredSessionStateSnapshot::state_revision),
            Some(7)
        );
        assert_eq!(
            durable
                .meta
                .state_snapshot
                .as_ref()
                .unwrap()
                .plugin_payload_for_apply("plugin", 1, StoredStateScope::Session)
                .unwrap(),
            Some(&serde_json::json!({"kept": true}))
        );
        writer.shutdown(TERMINAL_CHECKPOINT_TIMEOUT).unwrap();
    }

    #[test]
    fn failed_compaction_capture_leaves_prior_durable_checkpoint_authoritative() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let writer = crate::storage_writer::StorageWriter::new(dir.clone()).unwrap();
        let mut session = AppSession::new("model", "/project");
        let first = StoredSessionStateSnapshot::new(1);
        session
            .meta
            .checkpoint_compaction_state(first.clone())
            .unwrap();
        session.meta.state_snapshot = Some(first);
        writer
            .persist_and_wait(Box::new(session.clone()), TERMINAL_CHECKPOINT_TIMEOUT)
            .unwrap();
        session.transcript = vec![compaction_entry(Vec::new(), 2)];
        let before_meta = serde_json::to_value(&session.meta).unwrap();
        let before_updated_at = session.updated_at;
        let dead = n00n_lua::EventHandle::disconnected_for_test();

        assert!(prepare_compaction_checkpoint(Some(&dead), &mut session, 2).is_err());
        assert_eq!(serde_json::to_value(&session.meta).unwrap(), before_meta);
        assert_eq!(session.updated_at, before_updated_at);
        let durable = AppSession::load(session.id, &dir).unwrap();
        assert_eq!(
            durable
                .meta
                .compaction_state_at(1)
                .unwrap()
                .state_revision(),
            Some(1)
        );
        assert!(durable.meta.compaction_state_at(2).is_err());
        writer.shutdown(TERMINAL_CHECKPOINT_TIMEOUT).unwrap();
    }

    #[test]
    fn revision_exhaustion_leaves_compaction_checkpoint_unchanged() {
        let mut session = AppSession::new("model", "/project");
        let previous = StoredSessionStateSnapshot::new(1);
        session
            .meta
            .checkpoint_compaction_state(previous.clone())
            .unwrap();
        session.meta.state_snapshot = Some(previous);
        session.meta.revision = u64::MAX;
        let before_meta = serde_json::to_value(&session.meta).unwrap();
        let before_updated_at = session.updated_at;

        assert!(prepare_compaction_checkpoint(None, &mut session, u64::MAX).is_err());
        assert_eq!(serde_json::to_value(&session.meta).unwrap(), before_meta);
        assert_eq!(session.updated_at, before_updated_at);
    }

    #[test]
    fn recursive_compactions_persist_two_exact_state_revisions() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let writer = crate::storage_writer::StorageWriter::new(dir.clone()).unwrap();
        let mut session = AppSession::new("model", "/project");
        session.transcript = vec![compaction_entry(Vec::new(), 3)];
        prepare_compaction_checkpoint(None, &mut session, 3).unwrap();
        writer
            .persist_and_wait(Box::new(session.clone()), TERMINAL_CHECKPOINT_TIMEOUT)
            .unwrap();

        let inner = session.transcript.clone();
        session.transcript = vec![compaction_entry(inner, 9)];
        let revision = outer_compaction_state_revision(&session.transcript).unwrap();
        prepare_compaction_checkpoint(None, &mut session, revision).unwrap();
        writer
            .persist_and_wait(Box::new(session.clone()), TERMINAL_CHECKPOINT_TIMEOUT)
            .unwrap();

        let durable = AppSession::load(session.id, &dir).unwrap();
        assert_eq!(
            durable
                .meta
                .compaction_state_at(3)
                .unwrap()
                .state_revision(),
            Some(3)
        );
        assert_eq!(
            durable
                .meta
                .compaction_state_at(9)
                .unwrap()
                .state_revision(),
            Some(9)
        );
        assert_eq!(
            durable
                .meta
                .state_snapshot
                .as_ref()
                .and_then(StoredSessionStateSnapshot::state_revision),
            Some(9)
        );
        writer.shutdown(TERMINAL_CHECKPOINT_TIMEOUT).unwrap();
    }

    #[test]
    fn resume_selects_latest_snapshot_after_outer_checkpoint() {
        let mut session = AppSession::new("model", "/project");
        let mut checkpoint = StoredSessionStateSnapshot::new(3);
        checkpoint
            .set_plugin_state(
                "plugin",
                1,
                StoredStateScope::Session,
                serde_json::json!({"value": "checkpoint"}),
            )
            .unwrap();
        session
            .meta
            .checkpoint_compaction_state(checkpoint)
            .unwrap();
        let mut latest = StoredSessionStateSnapshot::new(9);
        latest
            .set_plugin_state(
                "plugin",
                1,
                StoredStateScope::Session,
                serde_json::json!({"value": "latest"}),
            )
            .unwrap();
        session.meta.state_snapshot = Some(latest);
        session.meta.revision = 9;

        let selected = resume_state_snapshot(&session, Some(3)).unwrap().unwrap();

        assert_eq!(selected.state_revision(), Some(9));
        assert_eq!(
            selected
                .plugin_payload_for_apply("plugin", 1, StoredStateScope::Session)
                .unwrap(),
            Some(&serde_json::json!({"value": "latest"}))
        );
    }

    #[test]
    fn legacy_compaction_resume_falls_back_to_latest_snapshot() {
        let mut session = AppSession::new("model", "/project");
        session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(6));
        session.transcript = vec![TranscriptEntry::Compaction {
            entries: Vec::new(),
            generated_summary: None,
            state_revision: None,
        }];

        let selected = resume_state_snapshot(&session, None).unwrap().unwrap();

        assert_eq!(selected.state_revision(), Some(6));
    }

    #[test]
    fn missing_or_future_checkpoint_falls_back_to_latest_snapshot() {
        let mut session = AppSession::new("model", "/project");
        session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(5));
        session.meta.revision = 5;

        let missing = resume_state_snapshot(&session, Some(4)).unwrap().unwrap();
        let future = resume_state_snapshot(&session, Some(6)).unwrap().unwrap();

        assert_eq!(missing.state_revision(), Some(5));
        assert_eq!(future.state_revision(), Some(5));
    }

    #[test]
    fn malformed_referenced_checkpoint_fails_closed() {
        let mut session = AppSession::new("model", "/project");
        session.meta = serde_json::from_value(serde_json::json!({
            "revision": 5,
            "state_snapshot": {"schema_version": 1, "state_revision": 3},
            "compaction_state_checkpoints": {
                "schema_version": 1,
                "checkpoints": [{"invalid": true}]
            }
        }))
        .unwrap();

        let error = resume_state_snapshot(&session, Some(4)).unwrap_err();

        assert_eq!(error, "compaction-state checkpoint envelope is malformed");
    }

    #[test]
    fn initial_revision_uses_session_snapshot_meta_and_root_current_revision() {
        let mut child = AppSession::new("model", "/project");
        child.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(8));
        child.meta.revision = 10;
        let mut root = AppSession::new("model", "/project");
        root.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(12));
        root.meta.revision = 11;

        assert_eq!(initial_state_revision(&child, None), 10);
        assert_eq!(initial_state_revision(&child, Some(&root)), 12);
    }

    #[test]
    fn prepared_root_and_child_checkpoints_are_idempotent_across_retry() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let writer = crate::storage_writer::StorageWriter::new(dir.clone()).unwrap();
        let mut root = AppSession::new("model", "/project");
        let mut child = AppSession::new("model", "/project");
        child.meta.root_session_id = Some(root.id);
        child.transcript = vec![compaction_entry(Vec::new(), 7)];
        prepare_compaction_checkpoint(None, &mut root, 7).unwrap();
        prepare_compaction_checkpoint(None, &mut child, 7).unwrap();

        for checkpoint in [&root, &root, &child, &child] {
            writer
                .persist_and_wait(Box::new(checkpoint.clone()), TERMINAL_CHECKPOINT_TIMEOUT)
                .unwrap();
        }

        assert_eq!(
            AppSession::load(root.id, &dir)
                .unwrap()
                .meta
                .compaction_state_at(7)
                .unwrap()
                .state_revision(),
            Some(7)
        );
        assert_eq!(
            AppSession::load(child.id, &dir)
                .unwrap()
                .meta
                .compaction_state_at(7)
                .unwrap()
                .state_revision(),
            Some(7)
        );
        writer.shutdown(TERMINAL_CHECKPOINT_TIMEOUT).unwrap();
    }

    #[test]
    fn merging_delayed_checkpoint_preserves_newer_session_content() {
        let mut current = AppSession::new("model", "/project");
        current.messages = vec![Message::user("after checkpoint".into())];
        current.transcript = vec![TranscriptEntry::Message(Message::user(
            "after checkpoint".into(),
        ))];
        current.meta.revision = 10;
        let mut checkpoint = current.clone();
        checkpoint.messages.clear();
        checkpoint.transcript = vec![compaction_entry(Vec::new(), 7)];
        checkpoint.meta.revision = 8;
        prepare_compaction_checkpoint(None, &mut checkpoint, 7).unwrap();

        merge_compaction_metadata(&mut current, &checkpoint, 7).unwrap();

        assert_eq!(current.messages.len(), 1);
        assert!(matches!(
            current.transcript.as_slice(),
            [TranscriptEntry::Message(_)]
        ));
        assert_eq!(
            current
                .meta
                .compaction_state_at(7)
                .unwrap()
                .state_revision(),
            Some(7)
        );
        assert_eq!(current.meta.revision, 10);
    }

    #[test]
    fn root_checkpoint_for_child_is_stored_at_matching_revision() {
        let mut child = AppSession::new("model", "/project");
        let mut root = AppSession::new("model", "/project");

        prepare_compaction_checkpoint(None, &mut child, 7).unwrap();
        prepare_compaction_checkpoint(None, &mut root, 7).unwrap();

        assert_eq!(
            child.meta.compaction_state_at(7).unwrap().state_revision(),
            Some(7)
        );
        assert_eq!(
            root.meta.compaction_state_at(7).unwrap().state_revision(),
            Some(7)
        );
    }

    #[test]
    fn startup_provider_failure_preserves_model_and_requests_login_once() {
        let mut model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let original_spec = model.spec();
        let calls = Counter::new(0);

        let (_, warning) = startup_provider_with(&mut model, false, |_| {
            calls.set(calls.get() + 1);
            Err(AgentError::Config {
                message: "missing credentials".into(),
            })
        });

        assert_eq!(calls.get(), 1);
        assert_eq!(model.spec(), original_spec);
        assert_eq!(
            warning.as_deref(),
            Some("Failed to create provider: missing credentials")
        );
    }
    #[test]
    fn startup_provider_skips_construction_while_login_is_required() {
        let mut model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let calls = Counter::new(0);

        let (_, warning) = startup_provider_with(&mut model, true, |_| {
            calls.set(calls.get() + 1);
            Err(AgentError::Config {
                message: "must not run".into(),
            })
        });

        assert_eq!(calls.get(), 0);
        assert!(warning.is_none());
    }

    #[test_case(true; "login required")]
    fn model_fetch_completion_preserves_unconfigured_provider(needs_login: bool) {
        let calls = Counter::new(0);
        let initial = Arc::new(ModelSlot {
            model: Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap(),
            provider: Arc::from(unconfigured_provider()),
        });
        let model_slot = Arc::new(ArcSwap::from(Arc::clone(&initial)));

        complete_model_fetch_with(&model_slot, &initial, needs_login, |_| {
            calls.set(calls.get() + 1);
            Ok(unconfigured_provider())
        });

        assert_eq!(calls.get(), 0);
        assert!(Arc::ptr_eq(&model_slot.load_full(), &initial));
    }

    #[test]
    fn startup_login_completes_only_after_provider_slot_replacement() {
        let initial = Arc::new(ModelSlot {
            model: Model::from_spec("codex/gpt-5.6-sol").unwrap(),
            provider: Arc::from(unconfigured_provider()),
        });
        let replacement = Arc::new(ModelSlot {
            model: initial.model.clone(),
            provider: Arc::from(unconfigured_provider()),
        });

        assert!(!startup_login_completed(&initial, &initial));
        assert!(startup_login_completed(&initial, &replacement));
    }

    #[test]
    fn model_fetch_completion_does_not_overwrite_concurrent_model_change() {
        let initial = Arc::new(ModelSlot {
            model: Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap(),
            provider: Arc::from(unconfigured_provider()),
        });
        let replacement = Arc::new(ModelSlot {
            model: Model::from_spec("openai/gpt-4o").unwrap(),
            provider: Arc::from(unconfigured_provider()),
        });
        let model_slot = Arc::new(ArcSwap::from(Arc::clone(&initial)));
        let slot_during_create = Arc::clone(&model_slot);
        let replacement_during_create = Arc::clone(&replacement);

        complete_model_fetch_with(&model_slot, &initial, false, move |_| {
            slot_during_create.store(replacement_during_create);
            Ok(unconfigured_provider())
        });

        assert!(Arc::ptr_eq(&model_slot.load_full(), &replacement));
    }

    #[test]
    fn delete_allows_only_ui_callbacks_without_agent_identity() {
        assert!(authorize_ui_delete(None, true).is_ok());
        assert_eq!(
            authorize_ui_delete(None, false)
                .as_ref()
                .map_err(String::as_str),
            Err(DELETE_UI_ONLY_ERR)
        );
        let caller = SessionRef::generate();
        assert_eq!(
            authorize_ui_delete(Some(&caller), true)
                .as_ref()
                .map_err(String::as_str),
            Err(DELETE_UI_ONLY_ERR)
        );
    }

    #[test]
    fn paused_team_run_requires_matching_team_tool_call() {
        let tool_result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".into(),
                content: r#"{"paused":true,"run_id":"run-1"}"#.into(),
                is_error: true,
            }],
            ..Default::default()
        };
        assert!(paused_team_run(std::slice::from_ref(&tool_result)).is_none());

        let tool_call = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: TEAM_TOOL_NAME.into(),
                input: serde_json::json!({}),
            }],
            ..Default::default()
        };
        let paused = paused_team_run(&[tool_call, tool_result]).expect("paused team payload");
        assert_eq!(paused["run_id"], "run-1");
    }

    #[test]
    fn paused_team_run_ignores_malformed_team_json() {
        let tool_call = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: TEAM_TOOL_NAME.into(),
                input: serde_json::json!({}),
            }],
            ..Default::default()
        };
        let tool_result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".into(),
                content: r#"{"paused":true,"run_id":"#.into(),
                is_error: true,
            }],
            ..Default::default()
        };
        assert!(paused_team_run(&[tool_call, tool_result]).is_none());
    }

    #[test]
    fn paused_team_payload_keeps_only_valid_resume_fields() {
        let paused = paused_team_payload(
            r#"{"paused":true,"run_id":"run-1","mode":"swarm","output":"large"}"#,
        )
        .expect("paused team payload");
        assert_eq!(
            paused,
            serde_json::json!({"paused": true, "run_id": "run-1", "mode": "swarm"})
        );
        assert!(paused_team_payload(r#"{"paused":false,"run_id":"run-1"}"#).is_none());
        assert!(paused_team_payload(r#"{"paused":true,"run_id":""}"#).is_none());
        assert!(
            paused_team_payload(r#"{"paused":true,"run_id":"run-1","mode":"invalid"}"#).is_none()
        );
        let oversized = serde_json::json!({
            "paused": true,
            "run_id": "x".repeat(PAUSED_TEAM_RUN_ID_MAX_BYTES + 1),
        });
        assert!(validated_paused_team_payload(&oversized).is_none());
        assert!(validated_paused_team_payload(&serde_json::json!({"run_id": "run-1"})).is_none());
    }

    #[test]
    fn direct_paused_team_payload_requires_team_tool_event() {
        let output = format!(
            r#"{{"paused":true,"run_id":"run-1","output":"{}"}}"#,
            "x".repeat(DIRECT_OUTPUT_MAX_BYTES)
        );

        assert_eq!(
            direct_paused_team_payload(TEAM_TOOL_NAME, &output),
            Some(serde_json::json!({"paused": true, "run_id": "run-1"}))
        );
        assert!(direct_paused_team_payload("task", &output).is_none());
    }

    #[test]
    fn bounded_direct_output_respects_session_record_limits() {
        let config = AgentConfig {
            max_output_lines: 2,
            max_output_bytes: 24,
            ..AgentConfig::default()
        };

        let output = bounded_direct_output("αβγδεζηθ\nsecond\nthird", &config);

        assert!(output.len() <= config.max_output_bytes);
        assert!(output.lines().count() <= config.max_output_lines);
        assert!(output.contains("[truncated]"));
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());

        let unbounded_config = AgentConfig {
            max_output_lines: usize::MAX,
            max_output_bytes: usize::MAX,
            ..AgentConfig::default()
        };
        let capped =
            bounded_direct_output(&"x".repeat(DIRECT_OUTPUT_MAX_BYTES + 1), &unbounded_config);
        assert!(capped.len() <= DIRECT_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn cancel_stored_session_clears_all_persisted_work() {
        let mut session = AppSession::new("model", "/project");
        session.meta.lifecycle = StoredSessionLifecycle::Running;
        session.meta.queued_messages = vec!["legacy".into()];
        session.meta.queued_submissions = vec![StoredQueuedMessage {
            text: "queued".into(),
            images: Vec::new(),
            mode: None,
            plan_path: None,
            thinking: None,
            fast: false,
            workflow: false,
            control: false,
            delivery: StoredDelivery::TurnEnd,
            prompt: None,
        }];
        session.meta.queued_direct_tools = vec![StoredDirectTool {
            tool: "task".into(),
            input: serde_json::json!({}),
        }];
        session.meta.direct_paused_team = Some(serde_json::json!({
            "paused": true,
            "run_id": "run-1",
        }));

        assert!(cancel_stored_session(&mut session));
        assert_eq!(session.meta.lifecycle, StoredSessionLifecycle::Cancelled);
        assert!(session.meta.queued_messages.is_empty());
        assert!(session.meta.queued_submissions.is_empty());
        assert!(session.meta.queued_direct_tools.is_empty());
        assert!(session.meta.direct_paused_team.is_none());

        let mut inactive = AppSession::new("model", "/project");
        inactive.meta.lifecycle = StoredSessionLifecycle::Succeeded;
        assert!(!cancel_stored_session(&mut inactive));
        assert_eq!(inactive.meta.lifecycle, StoredSessionLifecycle::Succeeded);
        inactive.meta.queued_messages = vec!["pending".into()];
        assert!(cancel_stored_session(&mut inactive));
        assert!(inactive.meta.queued_messages.is_empty());
    }

    struct FailingBackend(TestBackend);

    fn infallible<T>(result: Result<T, std::convert::Infallible>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => match error {},
        }
    }

    impl Backend for FailingBackend {
        type Error = io::Error;
        fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Err(io::Error::other("deterministic draw failure"))
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            infallible(self.0.hide_cursor());
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            infallible(self.0.show_cursor());
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(infallible(self.0.get_cursor_position()))
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            infallible(self.0.set_cursor_position(position));
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            infallible(self.0.clear());
            Ok(())
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            infallible(self.0.clear_region(clear_type));
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(infallible(self.0.size()))
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(infallible(self.0.window_size()))
        }

        fn flush(&mut self) -> io::Result<()> {
            infallible(self.0.flush());
            Ok(())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Source {
        Input(usize),
        Agent(usize),
    }

    #[test]
    fn periodic_save_only_checkpoints_active_sessions() {
        assert!(!should_save_periodically(&Status::Idle, false));
        assert!(!should_save_periodically(
            &Status::error("failed".into()),
            false
        ));
        assert!(should_save_periodically(&Status::Streaming, false));
        assert!(!should_save_periodically(&Status::Streaming, true));
    }

    #[test]
    fn painted_submission_waits_for_its_session_after_focus_switch() {
        let first = n00nId::generate();
        let second = n00nId::generate();
        let mut pending = vec![(first, "first"), (second, "second")];

        let released = take_painted_submissions(&mut pending, second);

        assert_eq!(released, vec![(second, "second")]);
        assert_eq!(pending, vec![(first, "first")]);
        assert_eq!(
            take_painted_submissions(&mut pending, first),
            vec![(first, "first")]
        );
    }

    #[test]
    fn post_draw_hook_runs_after_terminal_buffer_is_painted() {
        let mut terminal = Terminal::new(TestBackend::new(20, 1)).expect("test terminal");
        let painted = std::cell::Cell::new(false);
        let persistence_started = std::cell::Cell::new(false);

        draw_then_post_terminal(
            &mut terminal,
            |frame| {
                frame.render_widget(Paragraph::new("bubble"), frame.area());
                painted.set(true);
            },
            || {
                persistence_started.set(true);
                assert!(painted.get());
            },
        )
        .expect("draw succeeds");

        assert!(persistence_started.get());
        assert_eq!(
            terminal.backend().buffer().cell((0, 0)).unwrap().symbol(),
            "b"
        );
    }

    #[test]
    fn terminal_draw_failure_does_not_release_post_draw_work() {
        let mut terminal =
            Terminal::new(FailingBackend(TestBackend::new(20, 1))).expect("test terminal");
        let post_draw_ran = std::cell::Cell::new(false);

        let result = draw_then_post_terminal(
            &mut terminal,
            |frame| frame.render_widget(Paragraph::new("bubble"), frame.area()),
            || post_draw_ran.set(true),
        );

        assert!(result.is_err());
        assert!(!post_draw_ran.get());
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn scroll_coalescing_is_bounded_under_continuous_input() {
        let first = mouse_event(MouseEventKind::ScrollUp, 1, 1);
        let mut consumed = 0;

        let (message, leftover) = aggregate_scroll(first, 1, || {
            consumed += 1;
            Some(Event::Mouse(mouse_event(MouseEventKind::ScrollUp, 1, 1)))
        });

        assert_eq!(consumed, COALESCE_BUDGET);
        let Msg::Scroll { column, row, delta } = message else {
            panic!("scroll events must produce a scroll message");
        };
        assert_eq!((column, row), (1, 1));
        assert_eq!(
            delta,
            i32::try_from(COALESCE_BUDGET + 1).expect("budget fits in i32")
        );
        assert!(leftover.is_none());
    }

    #[test]
    fn drag_coalescing_is_bounded_under_continuous_input() {
        let mut row = 0;

        let (latest, leftover) = coalesce_drag(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 0),
            || {
                row += 1;
                Some(Event::Mouse(mouse_event(
                    MouseEventKind::Drag(MouseButton::Left),
                    1,
                    row,
                )))
            },
        );

        assert_eq!(
            row,
            u16::try_from(COALESCE_BUDGET).expect("budget fits in u16")
        );
        assert_eq!(latest.row, row);
        assert!(leftover.is_none());
    }

    #[test]
    fn drag_coalescing_leaves_excess_events_queued() {
        let (tx, rx) = flume::unbounded();
        for row in 1..=u16::try_from(COALESCE_BUDGET + 1).expect("budget fits in u16") {
            tx.send(Event::Mouse(mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                1,
                row,
            )))
            .expect("receiver remains connected");
        }

        let (latest, leftover) = coalesce_drag(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 0),
            || try_recv_input(&rx),
        );

        assert_eq!(
            latest.row,
            u16::try_from(COALESCE_BUDGET).expect("budget fits in u16")
        );
        assert!(leftover.is_none());
        assert_eq!(rx.len(), 1);
        assert_eq!(
            rx.try_recv().expect("excess drag remains queued"),
            Event::Mouse(mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                1,
                u16::try_from(COALESCE_BUDGET + 1).expect("budget fits in u16"),
            ))
        );
    }

    #[test]
    fn coalescers_return_first_nonmatching_event_without_reordering() {
        let (scroll_tx, scroll_rx) = flume::unbounded();
        let key = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let trailing_scroll = Event::Mouse(mouse_event(MouseEventKind::ScrollDown, 2, 2));
        scroll_tx
            .send(Event::Mouse(mouse_event(MouseEventKind::ScrollUp, 1, 1)))
            .expect("receiver remains connected");
        scroll_tx
            .send(key.clone())
            .expect("receiver remains connected");
        scroll_tx
            .send(trailing_scroll.clone())
            .expect("receiver remains connected");

        let (scroll, leftover) =
            aggregate_scroll(mouse_event(MouseEventKind::ScrollUp, 0, 0), 1, || {
                try_recv_input(&scroll_rx)
            });

        let Msg::Scroll { column, row, delta } = scroll else {
            panic!("scroll events must produce a scroll message");
        };
        assert_eq!((column, row, delta), (0, 0, 2));
        assert_eq!(leftover, Some(key));
        assert_eq!(
            scroll_rx
                .try_recv()
                .expect("trailing scroll remains queued"),
            trailing_scroll
        );
    }
    #[test]
    fn alternating_coalescer_leftovers_are_bounded_and_ordered() {
        let (tx, rx) = flume::unbounded();
        for row in 1..=u16::try_from(HANDLE_INPUT_BUDGET + 1).expect("budget fits in u16") {
            let kind = if row % 2 == 0 {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::Drag(MouseButton::Left)
            };
            tx.send(Event::Mouse(mouse_event(kind, 0, row)))
                .expect("receiver remains connected");
        }
        let mut handled_rows = Vec::new();

        let retained = handle_input_bounded(
            Event::Mouse(mouse_event(MouseEventKind::ScrollUp, 0, 0)),
            |event| {
                let Event::Mouse(mouse) = event else {
                    panic!("test input must be mouse events");
                };
                handled_rows.push(mouse.row);
                match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        aggregate_scroll(mouse, 1, || try_recv_input(&rx)).1
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        coalesce_drag(mouse, || try_recv_input(&rx)).1
                    }
                    _ => panic!("test input must alternate scroll and drag events"),
                }
            },
        )
        .expect("the bounded handler retains its excess event");

        assert_eq!(handled_rows.len(), HANDLE_INPUT_BUDGET);
        assert_eq!(handled_rows.first(), Some(&0));
        assert_eq!(
            handled_rows.last(),
            Some(&u16::try_from(HANDLE_INPUT_BUDGET - 1).expect("budget fits in u16"))
        );
        assert_eq!(
            retained,
            Event::Mouse(mouse_event(
                MouseEventKind::ScrollUp,
                0,
                u16::try_from(HANDLE_INPUT_BUDGET).expect("budget fits in u16")
            ))
        );
        assert_eq!(
            rx.try_recv().expect("later input remains queued"),
            Event::Mouse(mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                0,
                u16::try_from(HANDLE_INPUT_BUDGET + 1).expect("budget fits in u16")
            ))
        );
    }

    #[test]
    fn drain_prioritizes_input_and_preserves_fair_bounded_progress() {
        let (input_tx, input_rx) = flume::unbounded();
        let (agent_tx, agent_rx) = flume::unbounded();
        for i in 0..DRAIN_BUDGET {
            input_tx.send(i).expect("input receiver remains connected");
            agent_tx.send(i).expect("agent receiver remains connected");
        }

        let mut scheduler = DrainScheduler::default();
        let drained: Vec<_> = (0..DRAIN_BUDGET)
            .filter_map(|_| {
                scheduler.next(
                    || input_rx.try_recv().ok().map(Source::Input),
                    || agent_rx.try_recv().ok().map(Source::Agent),
                )
            })
            .collect();

        assert_eq!(drained.first(), Some(&Source::Input(0)));
        assert_eq!(
            drained
                .iter()
                .filter(|source| matches!(source, Source::Input(_)))
                .count(),
            DRAIN_BUDGET / 2
        );
        assert_eq!(
            drained
                .iter()
                .filter(|source| matches!(source, Source::Agent(_)))
                .count(),
            DRAIN_BUDGET / 2
        );
        assert_eq!(input_rx.len() + agent_rx.len(), DRAIN_BUDGET);
    }
}
