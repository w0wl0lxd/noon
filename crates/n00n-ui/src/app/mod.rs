//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc: first esc flashes a hint, second within `flash_duration` cancels/rewinds.
//! `run_id` increments each run so stale events from previous agent runs are ignored.

mod btw;
mod image_paste;
pub(crate) mod mode;
mod mouse;
mod queue;
mod session;
pub(crate) use session::message_tool_use_ids;
pub(crate) mod session_state;
pub(crate) mod shell;
#[cfg(test)]
mod tests;
pub(crate) mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::AppSession;
use crate::agent::RevisionAllocator;
use crate::animation;
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT, transcript_to_display};
use crate::clipboard::ClipboardState;
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, StashOutcome, Submission};
use crate::components::keybindings::{KeybindContext, key};
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::tool_display::format_turn_usage;
use crate::components::usage_modal::{UsageFetchState, UsageModal};
use crate::components::{
    Action, DisplayMessage, DisplayRole, ExitRequest, Overlay, RetryInfo, Status,
    SubmissionDispatch,
};
use crate::image;
use crate::keymap::{self, KeyAction};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use n00n_agent::permissions::PermissionManager;
use n00n_agent::{
    AgentEvent, Envelope, FusionPhase, ImageSource, McpConfigErrors, McpPromptInfo,
    McpSnapshotReader, PreDispatchGate, SubagentInfo, SubagentPrompt, ToolOutput,
};
use n00n_config::UiConfig;
use n00n_lua::{EventHandle, HintReader, KeymapReader, LuaCommandReader};
use n00n_providers::model_registry::{model_registry, set_thinking_and_persist};
use n00n_providers::{Effort, Message, Model, ModelPricing, System, ThinkingConfig};
use n00n_storage::StateDir;
use n00n_storage::input_history::InputHistory;
use n00n_storage::model::persist_model;
use n00n_storage::sessions::{RetentionBudget, StoredTokenUsage};

use crate::storage_writer::StorageWriter;
use ratatui::layout::Position;
use ratatui_image::picker::Picker;

pub(crate) use crate::agent::QueuedMessage;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use queue::{MessageQueue, SubmitOutcome};
pub(crate) use session::session_has_content;
use session_state::SessionState;

const CANCEL_MSG: &str = "Cancelled.";
const PERSISTENCE_FAILURE_MSG: &str = "Could not save the session. Your message was restored.";
pub(super) const SUBMISSION_ESCAPE_WINDOW: Duration = Duration::from_millis(2_500);
/// Bypasses the per-run staleness filter because re-bake replies
/// don't belong to any real agent run.
pub(crate) const RESTORE_RUN_ID: u64 = u64::MAX;
const FLASH_REWIND: &str = "Press esc again to rewind...";
const AUTH_EXPIRED_MSG: &str =
    "Token expired. Run `n00n auth login` in another terminal, then press Enter to retry.";
const FLASH_NO_PLAN: &str = "No plan file";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode requires an Anthropic Opus 4.6+ model (API only)";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const THINKING_CYCLE: [ThinkingConfig; 8] = [
    ThinkingConfig::Off,
    ThinkingConfig::Adaptive,
    ThinkingConfig::Effort(Effort::Minimal),
    ThinkingConfig::Effort(Effort::Low),
    ThinkingConfig::Effort(Effort::Medium),
    ThinkingConfig::Effort(Effort::High),
    ThinkingConfig::Effort(Effort::XHigh),
    ThinkingConfig::Effort(Effort::Max),
];
const WORKFLOW_ON_MSG: &str = "Workflow mode: on";
const WORKFLOW_OFF_MSG: &str = "Workflow mode: off";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const TASK_DONE_DETAIL: &str = "✓ done";
const TASK_ERROR_DETAIL: &str = "✗ error";
const TASK_RUNNING_DETAIL: &str = "◈ running";
const STEERING_UNAVAILABLE_MSG: &str = "This agent is no longer accepting messages";
const STEERING_BUSY_MSG: &str = "This agent is busy; try again in a moment";
const TASK_PANEL_FOOTER: &[(&str, &str)] =
    &[("enter", "open"), ("ctrl+t", "toggle"), ("esc", "close")];

enum SubagentPromptError {
    Finished,
    Disconnected,
    Full(Submission),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskStatus {
    Main,
    Running,
    Done,
    Error,
}

#[derive(Clone)]
pub(super) struct TaskEntry {
    name: String,
    status: TaskStatus,
    usage: Option<String>,
}

impl PickerItem for TaskEntry {
    fn label(&self) -> &str {
        &self.name
    }
    fn suffix(&self) -> Option<&str> {
        self.usage.as_deref()
    }
    fn detail(&self) -> Option<&str> {
        match self.status {
            TaskStatus::Done => Some(TASK_DONE_DETAIL),
            TaskStatus::Error => Some(TASK_ERROR_DETAIL),
            TaskStatus::Running => Some(TASK_RUNNING_DETAIL),
            TaskStatus::Main => None,
        }
    }
    fn is_spinning(&self) -> bool {
        self.status == TaskStatus::Running
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum PendingInput {
    #[default]
    None,
    AuthRetry {
        subagent_id: Option<String>,
    },
    SubagentFollowUp {
        subagent_id: String,
    },
}

trait SubmissionClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemSubmissionClock;

impl SubmissionClock for SystemSubmissionClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
struct PendingSubmission {
    submission_id: u64,
    run_id: u64,
    submitted_at: Instant,
    message: QueuedMessage,
    gate: Arc<PreDispatchGate>,
    preamble: Vec<Message>,
    display_len_before: usize,
}

pub(crate) struct PendingPlanSubmit {
    pub(crate) message: QueuedMessage,
    pub(crate) plan: Option<(String, String)>,
}

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Agent(Box<Envelope>),
}

#[cfg(test)]
const TEST_WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
struct TestStateDir {
    dir: Option<tempfile::TempDir>,
    writer: Option<Arc<StorageWriter>>,
}

#[cfg(test)]
impl Drop for TestStateDir {
    fn drop(&mut self) {
        let Some(writer) = self.writer.take() else {
            return;
        };
        let writer_stopped = match Arc::try_unwrap(writer) {
            Ok(writer) => match writer.wait_for_shutdown(TEST_WRITER_DRAIN_TIMEOUT) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, "test storage writer shutdown failed");
                    false
                }
            },
            Err(writer) => {
                drop(writer);
                false
            }
        };
        if !writer_stopped && let Some(dir) = self.dir.take() {
            drop(dir.keep());
        }
    }
}

pub struct App {
    pub(super) chats: Vec<Chat>,
    pub(super) active_chat: usize,
    pub(super) chat_index: HashMap<String, usize>,
    pub(crate) input_box: InputBox,
    pub(super) command_palette: CommandPalette,
    pub(super) task_picker: ListPicker<TaskEntry>,
    pub(super) task_picker_original: Option<usize>,
    pub(super) theme_picker: ThemePicker,
    pub(super) model_picker: ModelPicker,
    model_picker_reply: Option<flume::Sender<Option<String>>>,
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) help_modal: HelpModal,
    pub(super) usage_modal: UsageModal,
    pub(super) btw_modal: BtwModal,
    pub(super) float_mgr: FloatManager,
    pub(super) search_modal: SearchModal,
    pub(super) file_picker: FilePickerModal,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) plan_form: PlanForm,
    pub(super) status_bar: StatusBar,
    pub status: Status,
    pub(super) fusion_phase: Option<FusionPhase>,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
    pub(crate) queue: MessageQueue,
    pub answer_tx: Option<flume::Sender<String>>,
    pub(crate) cmd_tx: Option<flume::Sender<super::AgentCommand>>,
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    next_submission_id: u64,
    pending_submission: Option<PendingSubmission>,
    pub(crate) pending_plan_submit: Option<PendingPlanSubmit>,
    submission_clock: Arc<dyn SubmissionClock>,
    pub(super) retry_info: Option<RetryInfo>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) scrollbar_drag: Option<mouse::ScrollbarDrag>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,
    last_ctrl_d: Option<Instant>,

    pub(crate) storage: StateDir,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) shared_history: Option<Arc<ArcSwap<Vec<Message>>>>,
    pub(crate) shared_transcript: Option<n00n_agent::SharedTranscript>,
    pub(crate) btw_system: Option<Arc<ArcSwap<System>>>,
    pub(crate) shared_tool_outputs: Option<Arc<Mutex<HashMap<String, ToolOutput>>>>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    pending_save: bool,
    last_save_flush: Option<Instant>,
    pub(crate) retention_budget: RetentionBudget,
    #[cfg(test)]
    session_saves: usize,
    #[cfg(test)]
    test_state_dir: Option<TestStateDir>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) picker: Arc<Picker>,
    pub(crate) lua_event_handle: Option<EventHandle>,
    pub(crate) revision_allocator: Option<Arc<RevisionAllocator>>,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    pub(crate) restore_event_tx: Option<n00n_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
    subagent_prompts: HashMap<String, flume::Sender<SubagentPrompt>>,
}

pub struct AppInit {
    pub model: Model,
    pub session: AppSession,
    pub storage: StateDir,
    pub available_models: Arc<ArcSwapOption<Vec<String>>>,
    pub mcp_reader: McpSnapshotReader,
    pub mcp_config_errors: McpConfigErrors,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub storage_writer: Arc<StorageWriter>,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub retention_budget: RetentionBudget,
    pub permissions: Arc<PermissionManager>,
    pub custom_commands: Arc<[n00n_agent::command::CustomCommand]>,
    pub picker: Arc<Picker>,
}

impl App {
    #[must_use]
    pub fn new(init: AppInit) -> Self {
        let AppInit {
            model,
            session,
            storage,
            available_models,
            mcp_reader,
            mcp_config_errors,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            storage_writer,
            ui_config,
            input_history_size,
            retention_budget,
            permissions,
            custom_commands,
            picker,
        } = init;
        animation::set_reduced_motion(ui_config.reduced_motion);
        scrollbar::set_enabled(ui_config.scrollbar);
        storage_writer.register_loaded(&session);
        let state = SessionState::from_session(session, &model, &storage);
        let mut input_box = InputBox::new(InputHistory::load(&storage, input_history_size));
        input_box.set_max_input_lines(ui_config.max_input_lines);
        let mut app = Self {
            chats: vec![Chat::new(
                "Main".into(),
                ui_config.clone(),
                Arc::clone(&picker),
            )],
            picker,
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box,
            command_palette: CommandPalette::new(
                custom_commands,
                mcp_reader.clone(),
                lua_command_reader,
            ),
            task_picker: ListPicker::new(),
            task_picker_original: None,
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            model_picker_reply: None,
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            usage_modal: UsageModal::new(),
            btw_modal: BtwModal::new(ui_config.typewriter_ms_per_char, ui_config.reduced_motion),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            permission_prompt: PermissionPrompt::new(),
            plan_form: PlanForm::new(),
            status_bar: StatusBar::new(ui_config.flash_duration()),
            status: Status::Idle,
            fusion_phase: None,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
            queue: MessageQueue::default(),
            answer_tx: None,
            cmd_tx: None,
            pending_input: PendingInput::None,
            run_id: 0,
            next_submission_id: 0,
            pending_submission: None,
            pending_plan_submit: None,
            submission_clock: Arc::new(SystemSubmissionClock),
            retry_info: None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            scrollbar_drag: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            last_ctrl_d: None,
            storage,
            usage_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            shared_transcript: None,
            btw_system: None,
            shared_tool_outputs: None,
            image_paste_rx: vec![],
            storage_writer,
            pending_save: false,
            last_save_flush: None,
            retention_budget,
            #[cfg(test)]
            session_saves: 0,
            #[cfg(test)]
            test_state_dir: None,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
            lua_event_handle: None,
            revision_allocator: None,
            keymap_reader,
            hint_reader,
            restore_event_tx: None,
            restoring: Arc::new(AtomicBool::new(false)),
            subagent_answers: HashMap::new(),
            subagent_prompts: HashMap::new(),
        };
        app.model_picker
            .set_recents(n00n_storage::model::read_recents(&app.storage));
        app
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[0]
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == 0
    }

    fn plan_form_active(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
    }

    pub(crate) fn update_model(&mut self, model: &Model) {
        let spec_changed = self.state.session.model != model.spec();
        self.state.update_model(model);
        if spec_changed
            && model.supports_thinking()
            && let Some(remembered) = model_registry()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remembered_thinking(&model.spec())
        {
            self.state.thinking = remembered.into();
        }
        persist_model(&self.storage, &self.state.session.model);
    }

    fn set_thinking(&mut self, thinking: ThinkingConfig) {
        self.state.thinking = thinking;
        if self.state.model.supports_thinking() {
            set_thinking_and_persist(self.state.model.spec(), thinking.into(), &self.storage);
        }
    }

    fn cycle_thinking(&mut self) {
        if !self.state.model.supports_thinking() {
            self.flash("Thinking requires a model that supports it".into());
            return;
        }
        let idx = THINKING_CYCLE
            .iter()
            .position(|&c| c == self.state.thinking)
            .unwrap_or_else(|| 0);
        let next = THINKING_CYCLE[(idx + 1) % THINKING_CYCLE.len()];
        self.set_thinking(next);
        self.flash(format!("Thinking: {next}"));
    }

    pub(crate) fn record_recent_model(&mut self, spec: &str) {
        let recents = n00n_storage::model::push_recent(&self.storage, spec);
        self.model_picker.set_recents(recents);
    }

    pub(crate) fn pick_model_for_lua(
        &mut self,
        current: Option<&str>,
        reply: flume::Sender<Option<String>>,
    ) {
        self.model_picker_reply = Some(reply);
        self.model_picker.open(current.unwrap_or_else(|| ""));
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn fire_session_autocmd(&self, event: &str, mut data: serde_json::Value) {
        if let Some(ref handle) = self.lua_event_handle {
            if let Some(map) = data.as_object_mut() {
                map.insert(
                    "session_id".into(),
                    serde_json::Value::String(self.state.session.id.to_string()),
                );
            }
            handle.fire_autocmd(event, data);
        }
    }

    pub fn tick_error_expiry(&mut self) {
        if self.status.is_error_expired() {
            self.status = Status::Idle;
        }
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    fn clear_selection_unless_pending_copy(&mut self) {
        if !self
            .selection_state
            .as_ref()
            .is_some_and(super::selection::SelectionState::is_pending_copy)
        {
            self.selection_state = None;
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if text.is_empty() {
                    if self.is_main_chat() && self.image_paste_rx.is_empty() {
                        self.start_image_paste();
                    }
                } else {
                    let mut text_lines = Vec::new();
                    if self.is_main_chat() {
                        for line in text.split('\n') {
                            if let Some((path, mt)) = image::try_parse_image_path(line) {
                                self.start_file_image_paste(path, mt);
                            } else {
                                text_lines.push(line);
                            }
                        }
                    } else {
                        text_lines.push(&text);
                    }
                    let text = text_lines.join("\n");
                    if !text.is_empty() {
                        self.route_text_paste(&text);
                    }
                }
                vec![]
            }
            Msg::Mouse(event) => {
                self.handle_mouse(event);
                vec![]
            }
            Msg::Scroll { column, row, delta } => {
                let drag_zone = self.selection_state.as_ref().and_then(|s| match s {
                    SelectionState::Dragging { sel, .. } => Some(sel.zone),
                    SelectionState::PendingCopy { .. } => None,
                });
                self.handle_scroll(column, row, delta);
                if let Some(zone) = self.zone_at(row, column) {
                    if drag_zone == Some(zone.zone)
                        && matches!(zone.zone, SelectionZone::Messages | SelectionZone::Input)
                    {
                        let scroll = self.scroll_offset(zone.zone);
                        if let Some(SelectionState::Dragging { sel, .. }) =
                            &mut self.selection_state
                        {
                            sel.update(row, column, scroll);
                        }
                    } else {
                        self.clear_selection_unless_pending_copy();
                    }
                } else {
                    self.clear_selection_unless_pending_copy();
                }
                vec![]
            }
            Msg::Agent(envelope) => self.handle_agent_event(*envelope),
        }
    }

    fn send_answer(&self, answer: String) {
        if let Some(tx) = &self.answer_tx {
            let _ = tx.try_send(answer);
        }
    }

    fn send_to_agent(&self, subagent_id: Option<&str>, answer: String) {
        if let Some(id) = subagent_id {
            if let Some(tx) = self.subagent_answers.get(id) {
                let _ = tx.try_send(answer);
            }
            // If the target subagent has finished, its answer channel is gone;
            // do not fall back to the main agent's answer channel.
            return;
        }
        self.send_answer(answer);
    }

    fn send_subagent_prompt(
        &mut self,
        subagent_id: &str,
        sub: Submission,
    ) -> Result<(), SubagentPromptError> {
        let Some(&idx) = self.chat_index.get(subagent_id) else {
            return Err(SubagentPromptError::Disconnected);
        };
        if self.chats[idx].is_finished() {
            self.subagent_prompts.remove(subagent_id);
            return Err(SubagentPromptError::Finished);
        }
        let Some(tx) = self.subagent_prompts.get(subagent_id) else {
            return Err(SubagentPromptError::Disconnected);
        };
        let prompt = SubagentPrompt {
            text: sub.text.clone(),
            images: sub.images.clone(),
        };
        match tx.try_send(prompt) {
            Ok(()) => {
                self.chats[idx].show_user_message_with_images(sub.text.clone(), sub.images);
                Ok(())
            }
            Err(flume::TrySendError::Full(_)) => Err(SubagentPromptError::Full(sub)),
            Err(flume::TrySendError::Disconnected(_)) => {
                self.subagent_prompts.remove(subagent_id);
                Err(SubagentPromptError::Disconnected)
            }
        }
    }

    fn handle_subagent_prompt_result(&mut self, subagent_id: &str, sub: Submission) {
        match self.send_subagent_prompt(subagent_id, sub) {
            Ok(()) => {}
            Err(SubagentPromptError::Full(sub)) => {
                self.flash(STEERING_BUSY_MSG.into());
                self.input_box.set_submission(sub);
            }
            Err(SubagentPromptError::Finished | SubagentPromptError::Disconnected) => {
                self.flash(STEERING_UNAVAILABLE_MSG.into());
            }
        }
    }

    fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        if self.btw_modal.is_open() {
            self.btw_modal.scroll(delta);
            return;
        }
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return;
        }
        if self.usage_modal.is_open() {
            self.usage_modal.scroll(delta);
            return;
        }
        let pos = Position::new(column, row);
        if self.float_mgr.scroll_at(pos, delta) {
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.is_open() {
                    if $picker.contains(pos) {
                        $picker.scroll(delta);
                    }
                    return;
                }
            };
        }
        try_picker!(self.rewind_picker);
        try_picker!(self.task_picker);
        try_picker!(self.model_picker);
        try_picker!(self.file_picker);
        if let Some(zone) = self.zone_at(row, column) {
            self.scroll_zone(zone.zone, delta);
        }
    }

    fn open_tasks(&mut self) {
        let entries: Vec<TaskEntry> = self
            .chats
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let status = if i == 0 {
                    TaskStatus::Main
                } else if c.is_failed() {
                    TaskStatus::Error
                } else if c.is_finished() {
                    TaskStatus::Done
                } else {
                    TaskStatus::Running
                };
                let usage = if i == 0 {
                    Some("main session".to_owned())
                } else {
                    let model = c.model_id.as_deref().unwrap_or_else(|| "model pending");
                    let tokens = c.token_usage.total_input() + c.token_usage.output;
                    Some(if tokens > 0 {
                        format!("{model} · {tokens} tokens")
                    } else {
                        model.to_owned()
                    })
                };
                TaskEntry {
                    name: if i == 0 {
                        "Main chat".to_owned()
                    } else {
                        format!("Agent: {}", c.name)
                    },
                    status,
                    usage,
                }
            })
            .collect();
        self.task_picker_original = Some(self.active_chat);
        self.task_picker.set_footer(TASK_PANEL_FOOTER);
        self.task_picker.open(entries, " Agents & Teams ");
        self.task_picker.select(self.active_chat);
    }

    fn dispatch_overlay(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if self.permission_prompt.is_open() {
            if let Some(answer) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let encoded = answer.encode();
                self.permission_prompt.close();
                self.send_to_agent(subagent_id.as_deref(), encoded);
            }
            return Some(vec![]);
        }

        // plan_form is non-modal: Passthrough falls through to the rest of dispatch
        if self.plan_form_active() {
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return Some(self.handle_plan_form_action(action));
            }
        }

        if self.help_modal.is_open() {
            self.help_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.usage_modal.is_open() {
            if key::REFRESH.matches(key) {
                return Some(vec![Action::RefreshUsage]);
            }
            self.usage_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.btw_modal.is_open() {
            self.btw_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.float_mgr.handle_key(key) {
            return Some(vec![]);
        }

        if self.search_modal.is_open() {
            match self.search_modal.handle_key(key) {
                SearchAction::Consumed => {
                    let chat = &mut self.chats[self.active_chat];
                    let texts = chat.segment_search_texts();
                    self.search_modal.update_matches(&texts);
                    sync_search_highlight(&self.search_modal, chat);
                }
                SearchAction::Navigate => {
                    sync_search_highlight(&self.search_modal, &mut self.chats[self.active_chat]);
                }
                SearchAction::Select(idx) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.scroll_to_segment(idx);
                    chat.set_highlight_segment(None);
                    self.search_modal.close();
                }
                SearchAction::Close(saved) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.set_highlight_segment(None);
                    if let Some((top, auto)) = saved {
                        chat.restore_scroll(top, auto);
                    }
                    self.search_modal.close();
                }
            }
            return Some(vec![]);
        }

        if self.file_picker.is_open() {
            return Some(match self.file_picker.handle_key(key) {
                FilePickerModalAction::Consumed => vec![],
                FilePickerModalAction::Select(path) => {
                    self.file_picker.close();
                    if let InputAction::PaletteSync(val) =
                        self.input_box.handle_paste_with_spaces(&path)
                    {
                        self.command_palette.sync(&val);
                    }
                    vec![]
                }
                FilePickerModalAction::Close => {
                    let was_at = self.file_picker.take_at_mention();
                    self.file_picker.close();
                    if was_at {
                        self.input_box.buffer.push_char('@');
                        self.command_palette.sync(&self.input_box.buffer.value());
                    }
                    vec![]
                }
            });
        }

        if self.queue.focus().is_some() {
            match key.code {
                KeyCode::Up => self.queue.move_focus_up(),
                KeyCode::Down => self.queue.move_focus_down(),
                KeyCode::Enter => {
                    if let Some((_, msg, _)) = self.queue.take_focused_for_edit() {
                        self.input_box.set_submission(Submission {
                            text: msg.text,
                            images: msg.images,
                            control: msg.control,
                        });
                    }
                }
                KeyCode::Delete | KeyCode::Backspace => self.queue.remove_focused(),
                KeyCode::Esc => self.queue.unfocus(),
                _ if key::QUIT.matches(key) => self.queue.unfocus(),
                _ if key::POP_QUEUE.matches(key) => {
                    self.queue.remove(0);
                }
                _ => {}
            }
            return Some(vec![]);
        }

        if self.task_picker.is_open() {
            if key::TASKS.matches(key) {
                self.task_picker.close();
                return Some(vec![]);
            }
            return Some(match self.task_picker.handle_key(key) {
                PickerAction::Consumed | PickerAction::Toggle(..) => vec![],
                PickerAction::Select(idx, _) => {
                    self.task_picker_original = None;
                    self.active_chat = idx;
                    vec![]
                }
                PickerAction::Close => {
                    self.active_chat = self.task_picker_original.take().unwrap_or_else(|| 0);
                    vec![]
                }
            });
        }

        if self.rewind_picker.is_open() {
            return Some(match self.rewind_picker.handle_key(key) {
                RewindPickerAction::Consumed | RewindPickerAction::Close => vec![],
                RewindPickerAction::Select(entry) => self.rewind_to(&entry),
            });
        }

        if self.theme_picker.is_open() {
            return Some(match self.theme_picker.handle_key(key) {
                ThemePickerAction::Consumed | ThemePickerAction::Closed => vec![],
            });
        }

        if self.model_picker.is_open() {
            return Some(match self.model_picker.handle_key(key) {
                ModelPickerAction::Consumed => vec![],
                ModelPickerAction::Select(spec) => {
                    if let Some(reply) = self.model_picker_reply.take() {
                        let _ = reply.send(Some(spec));
                        vec![]
                    } else {
                        vec![Action::ChangeModel(spec)]
                    }
                }
                ModelPickerAction::AssignTier(spec, tier) => {
                    vec![Action::AssignTier(spec, tier)]
                }
                ModelPickerAction::UnassignTier(spec, tier) => {
                    vec![Action::UnassignTier(spec, tier)]
                }
                ModelPickerAction::CycleThinking => {
                    self.cycle_thinking();
                    vec![]
                }
                ModelPickerAction::Close => {
                    if let Some(reply) = self.model_picker_reply.take() {
                        let _ = reply.send(None);
                    }
                    vec![]
                }
            });
        }

        if self.login_picker.is_open() {
            return Some(match self.login_picker.handle_key(key) {
                LoginPickerAction::Consumed | LoginPickerAction::Close => vec![],
                LoginPickerAction::Authenticated { model_spec } => {
                    vec![Action::ChangeModel(model_spec), Action::RefreshModels]
                }
                LoginPickerAction::Configured { slug } => {
                    vec![Action::RefreshProvider { slug }, Action::RefreshModels]
                }
            });
        }

        if self.mcp_picker.is_open() {
            return Some(match self.mcp_picker.handle_key(key) {
                McpPickerAction::Consumed | McpPickerAction::Close => vec![],
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                } => {
                    vec![Action::ToggleMcp(server_name, enabled)]
                }
            });
        }

        None
    }

    /// Active keymap contexts, most specific first. Overlays consume their
    /// keys before this stack is consulted.
    fn context_stack(&self) -> Vec<KeybindContext> {
        let mut stack = Vec::with_capacity(5);
        if self.input_box.history_search_active() {
            stack.push(KeybindContext::HistorySearch);
        }
        if !self.is_main_chat() {
            stack.push(KeybindContext::SubagentChat);
        }
        if self.status == Status::Streaming {
            stack.push(KeybindContext::Streaming);
        }
        stack.push(KeybindContext::Editing);
        stack.push(KeybindContext::General);
        stack
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();

        if key::SUSPEND.matches(key) && cfg!(unix) {
            return vec![Action::Suspend];
        }

        if let Some(actions) = self.dispatch_overlay(key) {
            return actions;
        }

        if !(self.status == Status::Streaming && is_streaming_stop_key(key))
            && self.dispatch_override(key)
        {
            return vec![];
        }

        if let Some(action) = keymap::resolve(&self.context_stack(), key) {
            return self.perform(action, key);
        }

        if keymap::reaches_composer(&key) {
            return self.handle_composer_key(key);
        }

        vec![]
    }

    /// Unbound plain keys (printable input, `@` trigger) reach the composer.
    /// Modified chords that matched no binding are dead by design.
    fn handle_composer_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Any real keypress breaks pending double-press confirmations.
        self.last_esc = None;
        self.last_ctrl_d = None;
        match self.input_box.handle_key(key) {
            InputAction::PaletteSync(val) if self.is_main_chat() => {
                self.command_palette.sync(&val);
            }
            InputAction::OpenFilePicker if self.is_main_chat() => {
                self.file_picker.open_via_at(&self.state.session.cwd);
            }
            _ => {}
        }
        vec![]
    }

    fn dispatch_override(&self, key: KeyEvent) -> bool {
        let snap = self.keymap_reader.load();
        let stroke = keymap::KeyStroke::normalize(key);
        for entry in &snap.entries {
            // Lua spells shift either as `<C-S-t>` (flag) or `<C-T>`
            // (uppercase codepoint) — normalize both sides before compare.
            let want = keymap::KeyStroke::normalize_parts(entry.key, entry.modifiers);
            if want.code == stroke.code
                && want.modifiers == stroke.modifiers
                && let Some(ref handle) = self.lua_event_handle
                && handle.run_keybind_callback(entry.id)
            {
                return true;
            }
        }
        false
    }

    fn perform(&mut self, action: KeyAction, key: KeyEvent) -> Vec<Action> {
        if !matches!(action, KeyAction::Escape | KeyAction::SubagentEscape) {
            self.last_esc = None;
        }
        if action != KeyAction::ExitOrDeleteChar {
            self.last_ctrl_d = None;
        }
        let streaming = self.status == Status::Streaming;

        // The completion popup intercepts navigation/submit keys while open.
        if self.is_main_chat() && !self.input_box.history_search_active() {
            match self
                .command_palette
                .handle_key(key, &self.input_box.buffer.value())
            {
                CommandAction::Consumed => return vec![],
                CommandAction::Execute(cmd) => return self.execute_command(cmd),
                CommandAction::Complete(text) => {
                    self.command_palette.sync(&text);
                    self.input_box.set_input(&text);
                    self.input_box.buffer.move_to_end();
                    return vec![];
                }
                CommandAction::Passthrough => {}
            }
        }

        match action {
            KeyAction::QuitOrCancel => {
                self.command_palette.close();
                if self.input_box.history_search_active() {
                    self.input_box.history_search_cancel();
                    return vec![];
                }
                if self.queue.cancel_editing() {
                    self.input_box.discard();
                    return vec![];
                }
                if !self.is_main_chat() || self.input_box.is_empty() {
                    if streaming {
                        return self.handle_cancel();
                    }
                    return self.quit();
                }
                self.input_box.discard();
                vec![]
            }
            KeyAction::HelpToggle => {
                self.help_modal.toggle();
                vec![]
            }
            KeyAction::Redraw => vec![Action::Redraw],
            KeyAction::Suspend => vec![Action::Suspend],
            KeyAction::ChatPrev => {
                self.active_chat = self.active_chat.saturating_sub(1);
                vec![]
            }
            KeyAction::ChatNext => {
                self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
                vec![]
            }
            KeyAction::ScrollHalfUp => {
                let half = self.chats[self.active_chat].half_page();
                self.active_chat().scroll(half);
                vec![]
            }
            KeyAction::ScrollHalfDown => {
                let half = self.chats[self.active_chat].half_page();
                self.active_chat().scroll(-half);
                vec![]
            }
            KeyAction::ScrollPageUp => {
                let page = self.chats[self.active_chat].half_page() * 2;
                self.active_chat().scroll(page);
                vec![]
            }
            KeyAction::ScrollPageDown => {
                let page = self.chats[self.active_chat].half_page() * 2;
                self.active_chat().scroll(-page);
                vec![]
            }
            KeyAction::ScrollTop => {
                self.active_chat().scroll_to_top();
                vec![]
            }
            KeyAction::ScrollBottom => {
                self.active_chat().jump_to_bottom();
                vec![]
            }
            KeyAction::SearchOpen => {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
                vec![]
            }
            KeyAction::TranscriptDetails => {
                let visible = self.active_chat().toggle_transcript_details();
                self.flash(
                    if visible {
                        "Transcript details shown"
                    } else {
                        "Transcript details hidden"
                    }
                    .into(),
                );
                vec![]
            }
            KeyAction::ThinkingCycle => {
                self.cycle_thinking();
                vec![]
            }
            KeyAction::TasksOpen => {
                self.open_tasks();
                vec![]
            }
            KeyAction::PlanToggle => {
                if self.state.mode == Mode::Plan && self.state.plan.is_ready() {
                    self.plan_form.toggle();
                }
                vec![]
            }
            KeyAction::EditorOpenInput => {
                // The editor seeds from the live buffer — restore the
                // pre-search draft first so a shown match isn't edited.
                self.input_box.history_search_cancel();
                vec![Action::EditInputInEditor]
            }
            KeyAction::EditorOpenPlan => {
                if let Some(p) = self.state.plan.path() {
                    vec![Action::OpenEditor(p.to_path_buf())]
                } else {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            }
            KeyAction::CopySelection => {
                if let Some(SelectionState::Dragging { sel, .. }) = self.selection_state.take()
                    && !sel.is_empty()
                {
                    self.selection_state = Some(SelectionState::PendingCopy { sel });
                }
                vec![]
            }
            KeyAction::ImagePaste => {
                if self.image_paste_rx.is_empty() {
                    self.start_image_paste();
                }
                vec![]
            }
            KeyAction::QueuePop => {
                self.queue.remove(0);
                vec![]
            }
            KeyAction::Submit => {
                if self.input_box.char_before_cursor_is_backslash() {
                    self.input_box.continue_line();
                    return vec![];
                }
                match self.input_box.submit() {
                    Some(sub) => self.handle_submit(sub),
                    None => self.handle_submit(Submission::empty()),
                }
            }
            KeyAction::TabOrMode => {
                if self.is_bash_input() || !self.is_main_chat() {
                    return vec![];
                }
                if streaming && !self.input_box.is_empty() {
                    if let Some(sub) = self.input_box.submit() {
                        let msg = sub.into();
                        if self.queue.editing().is_some() {
                            self.replace_edited(msg);
                        } else {
                            self.queue_and_notify(msg);
                        }
                    }
                    vec![]
                } else {
                    self.toggle_mode()
                }
            }
            KeyAction::Escape => {
                if self.try_restore_pending_submission() {
                    return vec![];
                }
                if let Some(t) = self.last_esc.take()
                    && t.elapsed() < self.status_bar.flash_duration
                {
                    self.open_rewind_picker()
                } else {
                    self.last_esc = Some(Instant::now());
                    self.status_bar.flash(FLASH_REWIND.into());
                    vec![]
                }
            }
            KeyAction::ExitOrDeleteChar => {
                if !self.input_box.buffer.value().is_empty() {
                    return self.run_edit(KeyAction::DeleteCharForward);
                }
                if let Some(t) = self.last_ctrl_d.take()
                    && t.elapsed() < self.status_bar.flash_duration
                {
                    return self.quit();
                }
                self.last_ctrl_d = Some(Instant::now());
                self.status_bar.flash("Press Ctrl+D again to exit".into());
                vec![]
            }
            KeyAction::StashToggle => {
                match self.input_box.stash_toggle() {
                    StashOutcome::Stashed => self.status_bar.flash("Draft stashed".into()),
                    StashOutcome::Restored => {
                        self.command_palette.sync(&self.input_box.buffer.value());
                        self.status_bar.flash("Draft restored".into());
                    }
                    StashOutcome::NothingToStash => {
                        self.status_bar.flash("Nothing to stash".into());
                    }
                    StashOutcome::Occupied => self
                        .status_bar
                        .flash("Stash already occupied — restore it first".into()),
                }
                vec![]
            }
            KeyAction::HistorySearch => {
                self.input_box.start_history_search();
                vec![]
            }
            KeyAction::CancelAgent => {
                if self.try_restore_pending_submission() {
                    return vec![];
                }
                if self.is_main_chat() {
                    self.handle_cancel()
                } else {
                    self.handle_subagent_cancel()
                }
            }
            KeyAction::SubagentBack => {
                self.active_chat = 0;
                vec![]
            }
            KeyAction::SubagentEscape => {
                if self.chats[self.active_chat].is_finished() {
                    self.active_chat = 0;
                    return vec![];
                }
                self.handle_subagent_cancel()
            }
            KeyAction::HistorySearchAccept => {
                self.input_box.history_search_accept();
                vec![]
            }
            KeyAction::HistorySearchCancel => {
                self.input_box.history_search_cancel();
                vec![]
            }
            KeyAction::HistorySearchOlder => {
                self.input_box.history_search_older();
                vec![]
            }
            KeyAction::HistorySearchNewer => {
                self.input_box.history_search_newer();
                vec![]
            }
            KeyAction::HistorySearchBackspace => {
                self.input_box.history_search_backspace();
                vec![]
            }
            KeyAction::InputUp
            | KeyAction::InputDown
            | KeyAction::CharLeft
            | KeyAction::CharRight
            | KeyAction::WordLeft
            | KeyAction::WordRight
            | KeyAction::LineStart
            | KeyAction::LineEnd
            | KeyAction::DeleteCharBack
            | KeyAction::DeleteCharForward
            | KeyAction::DeleteWordBack
            | KeyAction::DeleteWordForward
            | KeyAction::KillLineEnd
            | KeyAction::KillLineStart
            | KeyAction::Yank
            | KeyAction::YankPop
            | KeyAction::Undo
            | KeyAction::Redo
            | KeyAction::Newline => self.run_edit(action),
        }
    }

    /// Forward a resolved editing action into the composer and keep the
    /// command palette in sync with buffer changes.
    fn run_edit(&mut self, action: KeyAction) -> Vec<Action> {
        if let InputAction::PaletteSync(val) = self.input_box.edit_action(action)
            && self.is_main_chat()
        {
            self.command_palette.sync(&val);
        }
        vec![]
    }

    fn quit(&mut self) -> Vec<Action> {
        self.quit_with(ExitRequest::Success)
    }

    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_session_without_plugin_state_capture();
        self.save_input_history();
        self.exit_request = req;
        vec![]
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        match std::mem::take(&mut self.pending_input) {
            PendingInput::AuthRetry { subagent_id } => {
                self.send_to_agent(subagent_id.as_deref(), String::new());
                return vec![];
            }
            PendingInput::SubagentFollowUp { subagent_id } => {
                self.handle_subagent_prompt_result(&subagent_id, sub);
                return vec![];
            }
            PendingInput::None => {}
        }
        if !self.is_main_chat() {
            if sub.is_empty() {
                return vec![];
            }
            let Some(tool_use_id) = self.chats[self.active_chat].tool_use_id.clone() else {
                return vec![];
            };
            self.handle_subagent_prompt_result(&tool_use_id, sub);
            return vec![];
        }
        if sub.is_empty() {
            if self.status == Status::Streaming {
                self.queue.promote_latest_steering();
            }
            return vec![];
        }
        if !self.is_main_chat() {
            let subagent_id = self
                .chat_index
                .iter()
                .find(|&(_, &idx)| idx == self.active_chat)
                .map(|(id, _)| id.clone());
            if let Some(subagent_id) = subagent_id
                && self.send_subagent_prompt(&subagent_id, sub).is_err()
            {
                self.flash(STEERING_UNAVAILABLE_MSG.into());
            }
            return vec![];
        }
        if sub.text.trim() == "exit" {
            return self.quit();
        }

        if let Some(prefix) = shell::parse_shell_prefix(&sub.text) {
            let cmd = prefix.command.trim();
            if cmd == "cd" || cmd.starts_with("cd ") {
                self.flash("Only /cd can change the working directory".into());
            }
            let id = self.shell.next_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().show_user_message(display);
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        if self.status == Status::Streaming {
            let msg = sub.into();
            if self.queue.editing().is_some() {
                self.replace_edited(msg);
            } else {
                self.queue_steering(msg);
            }
            return vec![];
        }
        self.submit_or_queue(sub.into())
    }

    fn try_restore_pending_submission(&mut self) -> bool {
        if !matches!(self.status, Status::Streaming | Status::Error { .. }) {
            return false;
        }
        let Some(pending) = self.pending_submission.as_ref() else {
            return false;
        };
        let failed_before_dispatch =
            matches!(self.status, Status::Error { .. }) && !pending.gate.is_committed();
        if pending.run_id != self.run_id
            || (!failed_before_dispatch
                && self
                    .submission_clock
                    .now()
                    .saturating_duration_since(pending.submitted_at)
                    > SUBMISSION_ESCAPE_WINDOW)
            || !self.input_box.is_empty()
        {
            return false;
        }
        if !pending.gate.try_cancel() {
            self.pending_submission = None;
            return false;
        }
        let submission_id = pending.submission_id;
        self.restore_pending_submission(submission_id, pending.run_id)
    }

    fn restore_pending_submission(&mut self, submission_id: u64, run_id: u64) -> bool {
        let Some(pending) = self.pending_submission.take() else {
            return false;
        };
        if pending.submission_id != submission_id
            || pending.run_id != run_id
            || self.run_id != run_id
        {
            self.pending_submission = Some(pending);
            return false;
        }

        self.queue.remove_submission(submission_id);
        self.shell.restore_results(pending.preamble);
        self.main_chat()
            .truncate_messages(pending.display_len_before);
        self.input_box.set_submission(Submission {
            text: pending.message.text,
            images: pending.message.images,
            control: pending.message.control,
        });
        self.run_id += 1;
        self.status = Status::Idle;
        self.retry_info = None;
        self.last_esc = None;
        self.status_bar.clear_flash();
        true
    }

    pub(crate) fn handle_submission_persistence_failure(&mut self, dispatch: &SubmissionDispatch) {
        self.handle_submission_failure(dispatch, PERSISTENCE_FAILURE_MSG);
    }

    pub(crate) fn handle_submission_failure(
        &mut self,
        dispatch: &SubmissionDispatch,
        message: &str,
    ) {
        self.queue.remove_submission(dispatch.submission_id);
        if dispatch.paint_required {
            let Some(pending) = self.pending_submission.as_ref() else {
                return;
            };
            if pending.submission_id != dispatch.submission_id
                || pending.run_id != dispatch.run_id
                || !dispatch.gate.try_cancel()
            {
                return;
            }
            if self.restore_pending_submission(dispatch.submission_id, dispatch.run_id) {
                self.flash(message.into());
            }
            return;
        }

        let relevant_run = dispatch.run_id == self.run_id;
        let _ = dispatch.gate.try_cancel();
        if relevant_run && self.status == Status::Streaming {
            self.status = Status::error(message.into());
            self.main_chat()
                .push(DisplayMessage::new(DisplayRole::Error, message.into()));
            self.fire_session_autocmd("TurnError", serde_json::json!({ "message": message }));
        }
    }
    pub(crate) fn preserve_submission_for_shutdown(&mut self, dispatch: SubmissionDispatch) {
        if !dispatch.paint_required {
            return;
        }
        let Some(pending) = self.pending_submission.as_ref() else {
            return;
        };
        if pending.submission_id != dispatch.submission_id
            || pending.run_id != dispatch.run_id
            || !Arc::ptr_eq(&pending.gate, &dispatch.gate)
        {
            return;
        }
        self.queue.preserve_submission(dispatch);
    }

    pub(crate) fn stage_submission_preamble(&mut self, dispatch: &mut SubmissionDispatch) -> bool {
        if !dispatch.paint_required {
            if dispatch.gate.is_cancelled() {
                return false;
            }
            dispatch.input.preamble = self.shell.drain_results();
            return true;
        }
        let Some(pending) = self.pending_submission.as_mut() else {
            return false;
        };
        if pending.submission_id != dispatch.submission_id
            || pending.run_id != dispatch.run_id
            || self.run_id != dispatch.run_id
            || !Arc::ptr_eq(&pending.gate, &dispatch.gate)
            || dispatch.gate.is_cancelled()
        {
            return false;
        }
        let preamble = self.shell.drain_results();
        dispatch.input.preamble.clone_from(&preamble);
        pending.preamble = preamble;
        true
    }

    pub(crate) fn accepts_submission_persistence(&self, dispatch: &SubmissionDispatch) -> bool {
        if dispatch.run_id != self.run_id || dispatch.gate.is_cancelled() {
            return false;
        }
        if !dispatch.paint_required {
            return true;
        }
        self.pending_submission.as_ref().is_some_and(|pending| {
            pending.submission_id == dispatch.submission_id
                && pending.run_id == dispatch.run_id
                && Arc::ptr_eq(&pending.gate, &dispatch.gate)
        })
    }

    pub(crate) fn cancel_current_run(&mut self) -> Vec<Action> {
        self.handle_cancel()
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        if let Some(pending) = self.pending_submission.take()
            && pending.gate.try_cancel()
        {
            self.shell.restore_results(pending.preamble);
        }
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.retry_info = None;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.finish_subagents(&DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.subagent_prompts.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.cancel_in_progress();
        }
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        self.queue.clear();
        self.status = Status::Idle;
        self.fusion_phase = None;
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    fn handle_subagent_cancel(&mut self) -> Vec<Action> {
        let Some(tool_use_id) = self.chats[self.active_chat].tool_use_id.clone() else {
            return vec![];
        };

        self.chats[self.active_chat].flush();
        self.chats[self.active_chat].cancel_in_progress();
        self.chats[self.active_chat].mark_finished(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.remove(&tool_use_id);
        self.subagent_prompts.remove(&tool_use_id);
        if matches!(
            self.pending_input,
            PendingInput::SubagentFollowUp { ref subagent_id } if subagent_id == &tool_use_id
        ) {
            self.pending_input = PendingInput::None;
        }
        vec![Action::CancelSubagent { tool_use_id }]
    }

    fn handle_agent_event(&mut self, envelope: Envelope) -> Vec<Action> {
        if envelope.run_id == RESTORE_RUN_ID {
            let (id, snapshot, theme_gen, is_header) = match envelope.event {
                AgentEvent::ToolSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, false),
                AgentEvent::ToolHeaderSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, true),
                _ => return vec![],
            };
            for chat in &mut self.chats {
                if is_header {
                    chat.tool_header_snapshot(&id, snapshot.clone(), theme_gen);
                } else {
                    chat.tool_snapshot(&id, snapshot.clone(), theme_gen);
                }
            }
            return vec![];
        }
        if envelope.run_id != self.run_id {
            // Stale run_id after cancel: agent updates shared_history before sending
            // Done/Error, so this is the first moment the full conversation is available.
            match envelope.event {
                AgentEvent::Done { .. } | AgentEvent::Error { .. } => {
                    self.save_session_without_plugin_state_capture();
                }
                AgentEvent::SubagentHistory {
                    tool_use_id,
                    messages,
                    ..
                } => {
                    self.state
                        .session
                        .subagent_messages
                        .insert(tool_use_id, messages);
                    self.save_session_coalesced();
                }
                _ => {}
            }
            return vec![];
        }

        if let AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
            is_error,
        } = envelope.event
        {
            if let Some(&sub_idx) = self.chat_index.get(tool_use_id.as_str()) {
                let (role, text) = if is_error {
                    (DisplayRole::Error, ERROR_TEXT)
                } else {
                    (DisplayRole::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(role, text);
            }
            self.subagent_answers.remove(&tool_use_id);
            self.subagent_prompts.remove(&tool_use_id);
            self.state
                .session
                .subagent_messages
                .insert(tool_use_id, messages);
            self.save_session_coalesced();
            return vec![];
        }

        let subagent_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());
        let subagent_name = envelope.subagent.as_ref().map(|s| s.name.clone());

        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent),
            None => 0,
        };

        if matches!(envelope.event, AgentEvent::CompactionDone { .. }) && chat_idx == 0 {
            self.chats[chat_idx].flush();
            if let Some(shared_transcript) = &self.shared_transcript {
                let transcript = shared_transcript.load();
                let tool_outputs = self.shared_tool_outputs.as_ref().map_or_else(
                    || self.state.session.tool_outputs.clone(),
                    |outputs| {
                        outputs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                    },
                );
                let (display, _) = transcript_to_display(
                    &transcript,
                    &tool_outputs,
                    &self.ui_config.tool_output_lines,
                );
                self.chats[chat_idx].load_messages(display);
            }
        }

        if let AgentEvent::ToolStart(ref e) = envelope.event {
            self.fire_session_autocmd(
                "ToolStart",
                serde_json::json!({
                    "id": e.id,
                    "tool": e.tool,
                    "summary": e.summary,
                    "subagent": subagent_name.as_deref(),
                }),
            );
        }

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            self.fire_session_autocmd(
                "ToolDone",
                serde_json::json!({
                    "id": e.id,
                    "tool": e.tool,
                    "is_error": e.is_error,
                    "subagent": subagent_name.as_deref(),
                }),
            );
            if self.state.mode == Mode::Plan
                && self.state.plan.path().is_some_and(|pp| e.wrote_to(pp))
            {
                self.transition_plan(&PlanTrigger::WriteDone);
            }
            if let Some(ref outputs) = self.shared_tool_outputs {
                outputs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(e.id.clone(), e.output.clone());
            }
            if let Some(&sub_idx) = self.chat_index.get(&e.id) {
                let (role, text) = if e.is_error {
                    (DisplayRole::Error, ERROR_TEXT)
                } else {
                    (DisplayRole::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(role, text);
                self.subagent_answers.remove(&e.id);
                self.subagent_prompts.remove(&e.id);
            }
            self.save_session_coalesced();
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = envelope.event
        {
            self.chats[chat_idx].stream_reset();
            if chat_idx == 0 {
                self.retry_info = Some(RetryInfo {
                    attempt,
                    message,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                });
            }
            return vec![];
        }

        self.retry_info = None;

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            self.chats[chat_idx].token_usage += tc.usage;
            *self
                .state
                .session
                .meta
                .usage_by_model
                .entry(tc.model.clone())
                .or_default() += tc.usage.into();
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.chats[chat_idx].context_size = ctx_size;
            if chat_idx == 0 {
                self.state.context_size = ctx_size;
            }
            let pricing = match Model::from_spec(&tc.model) {
                Ok(model) => model.pricing,
                Err(error) => {
                    tracing::warn!(model = %tc.model, %error, "omitting turn cost for unresolved model");
                    ModelPricing::default()
                }
            };
            let formatted = format_turn_usage(&tc.usage, &pricing, self.state.fast);
            self.chats[chat_idx].set_pending_turn_usage(formatted);
        }

        if chat_idx == 0 {
            match &envelope.event {
                AgentEvent::FusionPhase { phase, .. } => {
                    self.fusion_phase = match phase {
                        FusionPhase::Idle
                        | FusionPhase::Complete
                        | FusionPhase::Cancelled
                        | FusionPhase::Failed => None,
                        phase => Some(*phase),
                    };
                }
                AgentEvent::Done { .. } | AgentEvent::Error { .. } => {
                    self.fusion_phase = None;
                }
                _ => {}
            }

            if let AgentEvent::Done {
                fusion: Some(stats),
                ..
            } = &envelope.event
            {
                let stored = self
                    .state
                    .session
                    .meta
                    .fusion
                    .get_or_insert_with(Default::default);
                stored.lead_cost += stats.lead_cost;
                stored.sidekick_cost += stats.sidekick_cost;
                stored.lead_usage += StoredTokenUsage {
                    input: stats.lead_usage.input,
                    output: stats.lead_usage.output,
                    cache_creation: stats.lead_usage.cache_creation,
                    cache_read: stats.lead_usage.cache_read,
                };
                stored.sidekick_usage += StoredTokenUsage {
                    input: stats.sidekick_usage.input,
                    output: stats.sidekick_usage.output,
                    cache_creation: stats.sidekick_usage.cache_creation,
                    cache_read: stats.sidekick_usage.cache_read,
                };
                stored.delegation_count += stats.delegation_count;
                stored.compact_count += stats.compact_count;
                stored.final_lane = stats.final_lane.as_str().into();
            }
        }

        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if let ChatEventResult::QueueItemConsumed {
            text,
            image_count,
            images,
            control,
        } = result
        {
            if chat_idx == 0 {
                self.on_queue_item_consumed(&text, image_count, images, control);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest {
            id: _,
            tool,
            scopes,
        } = result
        {
            self.permission_prompt
                .open(tool, scopes, subagent_id.clone());
            return vec![];
        }

        if let ChatEventResult::AuthRequired = result {
            self.chats[chat_idx].push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
            if chat_idx != 0 {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    AUTH_EXPIRED_MSG.into(),
                ));
            }
            self.pending_input = PendingInput::AuthRetry { subagent_id };
            return vec![];
        }

        if let ChatEventResult::SubagentInputRequired = result {
            if let Some(id) = subagent_id {
                self.pending_input = PendingInput::SubagentFollowUp { subagent_id: id };
                self.chats[chat_idx].push(DisplayMessage::new(
                    DisplayRole::Assistant,
                    "Waiting for your follow-up...".into(),
                ));
            }
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.status_bar.clear_flash();
                    self.save_session_without_plugin_state_capture();
                    self.chat_index.clear();
                    self.subagent_answers.clear();
                    self.subagent_prompts.clear();
                    self.status = Status::Idle;
                    self.fire_session_autocmd("TurnEnd", serde_json::json!({}));
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message.clone());
                    if self.queue.cancel_editing() {
                        self.input_box.discard();
                    }
                    self.queue.unfocus();
                    self.status_bar.clear_flash();
                    self.save_session_without_plugin_state_capture();
                    self.subagent_answers.clear();
                    self.subagent_prompts.clear();
                    self.finish_subagents(&DisplayRole::Error, ERROR_TEXT);
                    self.chats[chat_idx]
                        .push(DisplayMessage::new(DisplayRole::Error, message.clone()));
                    for chat in &mut self.chats {
                        chat.fail_in_progress_with_message(message.as_str());
                    }
                    self.fire_session_autocmd(
                        "TurnError",
                        serde_json::json!({ "message": message }),
                    );
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::AuthRequired
                | ChatEventResult::SubagentInputRequired
                | ChatEventResult::PermissionRequest { .. }
                | ChatEventResult::QueueItemConsumed { .. } => unreachable!(),
                ChatEventResult::Continue => {}
            }
        }
        vec![]
    }
    fn resolve_or_create_chat(&mut self, subagent: &SubagentInfo) -> usize {
        let id = &subagent.parent_tool_use_id;
        if let Some(&idx) = self.chat_index.get(id.as_str()) {
            return idx;
        }
        let idx = self.chats.len();
        self.chat_index.insert(id.clone(), idx);
        if let Some(ref tx) = subagent.answer_tx {
            self.subagent_answers.insert(id.clone(), tx.clone());
        }
        if let Some(ref tx) = subagent.prompt_tx {
            self.subagent_prompts.insert(id.clone(), tx.clone());
        }
        self.chats[0].update_tool_summary(&subagent.parent_tool_use_id, &subagent.name);
        if let Some(ref model) = subagent.model {
            self.chats[0].update_tool_model(&subagent.parent_tool_use_id, model);
        }
        let mut chat = Chat::new(
            subagent.name.clone(),
            self.ui_config.clone(),
            Arc::clone(&self.picker),
        );
        chat.set_restore_channel(self.lua_event_handle.clone(), self.restore_event_tx.clone());
        chat.tool_use_id = Some(id.clone());
        chat.model_id.clone_from(&subagent.model);
        if let Some(ref prompt) = subagent.prompt {
            chat.push_user_message(prompt);
        }
        self.chats.push(chat);
        idx
    }

    fn execute_command(&mut self, cmd: ParsedCommand) -> Vec<Action> {
        self.input_box.discard();
        match cmd.name.as_str() {
            "/tasks" => {
                self.open_tasks();
                vec![]
            }
            "/compact" => {
                if self.status == Status::Streaming {
                    self.queue_compact();
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact]
            }
            "/help" => {
                self.help_modal.toggle();
                vec![]
            }
            "/usage" => {
                self.usage_modal.toggle();
                if self.usage_modal.is_open() {
                    vec![Action::RefreshUsage]
                } else {
                    vec![]
                }
            }
            "/btw" => {
                let question = cmd.args.trim().to_string();
                if question.is_empty() {
                    self.flash("Usage: /btw <question>".into());
                    vec![]
                } else {
                    vec![Action::Btw(question)]
                }
            }
            "/new" => self.reset_session(),
            "/queue" => {
                self.queue.set_focus();
                vec![]
            }
            "/model" => {
                self.model_picker_reply = None;
                self.model_picker.open(&self.state.model.spec());
                vec![Action::RefreshModels]
            }
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/login" => {
                self.login_picker.open(self.storage.clone());
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
            "/yolo" => {
                let enabled = self.permissions.toggle_yolo();
                let msg = if enabled {
                    "YOLO mode enabled"
                } else {
                    "YOLO mode disabled"
                };
                self.flash(msg.into());
                vec![]
            }
            "/thinking" => {
                if !self.state.model.supports_thinking() {
                    self.flash("Thinking requires a model that supports it".into());
                    return vec![];
                }
                match ThinkingConfig::parse(cmd.args.trim(), self.state.thinking) {
                    Ok(thinking) => {
                        self.set_thinking(thinking);
                        self.flash(format!("Thinking: {thinking}"));
                    }
                    Err(msg) => self.flash(msg.into()),
                }
                vec![]
            }
            "/fast" => {
                if !self.state.model.supports_fast() {
                    self.flash(FAST_UNSUPPORTED_MSG.into());
                    return vec![];
                }
                self.state.fast = !self.state.fast;
                self.flash(
                    if self.state.fast {
                        FAST_ON_MSG
                    } else {
                        FAST_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            "/workflow" => {
                self.state.workflow = !self.state.workflow;
                self.flash(
                    if self.state.workflow {
                        WORKFLOW_ON_MSG
                    } else {
                        WORKFLOW_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            "/exit" => self.quit(),
            "/reload" => self.quit_with(ExitRequest::Reload),
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        let Some(handle) = &self.lua_event_handle else {
            return;
        };
        handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
            Some(session::plugin_state_identity(&self.state.session)),
        );
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(prompt) = self.command_palette.find_mcp_prompt(name) else {
            return Vec::new();
        };
        let prompt = prompt.clone();
        let arguments = Self::parse_prompt_args(&prompt, args);
        let missing: Vec<_> = prompt
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.contains_key(&a.name))
            .map(|a| format!("<{}>", a.name))
            .collect();
        if !missing.is_empty() {
            self.flash(format!("Usage: {} {}", name, missing.join(" ")));
            return vec![];
        }

        let prompt_ref = n00n_agent::McpPromptRef {
            qualified_name: prompt.qualified_name,
            arguments,
        };
        let display_text = if args.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: display_text.clone(),
            images: Vec::new(),
            control: false,
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            vec![]
        } else {
            self.run_id += 1;
            self.start_submission(
                &QueuedMessage {
                    text: display_text,
                    images: Vec::new(),
                    control: false,
                },
                input,
                true,
            )
        }
    }

    fn parse_prompt_args(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut remaining = args.trim();
        if remaining.is_empty() || prompt.arguments.is_empty() {
            return result;
        }
        let last_idx = prompt.arguments.len() - 1;
        for (i, arg) in prompt.arguments.iter().enumerate() {
            if remaining.is_empty() {
                break;
            }
            if i == last_idx {
                result.insert(arg.name.clone(), remaining.to_string());
            } else if let Some((word, rest)) = remaining.split_once(char::is_whitespace) {
                result.insert(arg.name.clone(), word.to_string());
                remaining = rest.trim_start();
            } else {
                result.insert(arg.name.clone(), remaining.to_string());
                break;
            }
        }
        result
    }

    fn execute_custom_command(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(cmd) = self.command_palette.find_custom_command(name) else {
            self.flash(format!("Unknown command: {name}"));
            return vec![];
        };
        self.submit_or_queue(QueuedMessage {
            text: cmd.render(args),
            images: Vec::new(),
            control: false,
        })
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        let path = if args.is_empty() {
            n00n_storage::paths::home().unwrap_or_else(Default::default)
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = n00n_storage::paths::home().unwrap_or_else(Default::default);
                    if rest.is_empty() {
                        home
                    } else {
                        home.join(rest.trim_start_matches('/'))
                    }
                }
                None => PathBuf::from(args),
            }
        };
        match std::env::set_current_dir(&path) {
            Ok(()) => {
                if let Ok(canonical) = std::env::current_dir() {
                    self.state.session.cwd = canonical.to_string_lossy().into_owned();
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()));
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }

    fn overlays(&self) -> [&dyn Overlay; 13] {
        [
            &self.help_modal,
            &self.usage_modal,
            &self.btw_modal,
            &self.float_mgr,
            &self.search_modal,
            &self.file_picker,
            &self.task_picker,
            &self.rewind_picker,
            &self.theme_picker,
            &self.model_picker,
            &self.login_picker,
            &self.mcp_picker,
            &self.permission_prompt,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 13] {
        [
            &mut self.help_modal,
            &mut self.usage_modal,
            &mut self.btw_modal,
            &mut self.float_mgr,
            &mut self.search_modal,
            &mut self.file_picker,
            &mut self.task_picker,
            &mut self.rewind_picker,
            &mut self.theme_picker,
            &mut self.model_picker,
            &mut self.login_picker,
            &mut self.mcp_picker,
            &mut self.permission_prompt,
        ]
    }

    #[must_use]
    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    /// True when the agent is parked on user input: a permission prompt or an
    /// auth retry. Drives the `needs_input` session status.
    pub(crate) fn awaiting_input(&self) -> bool {
        self.permission_prompt.is_open() || self.pending_input != PendingInput::None
    }

    #[must_use]
    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
    }
    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.status == Status::Streaming
            || !self.image_paste_rx.is_empty()
            || self.btw_modal.is_animating()
            || self.file_picker.is_loading()
            || self.float_mgr.is_open()
            || self
                .selection_state
                .as_ref()
                .is_some_and(super::selection::SelectionState::is_edge_scrolling)
            || self.restoring.load(Ordering::Relaxed)
            || self.chats.iter().any(super::chat::Chat::is_animating)
    }
    fn finish_subagents(&mut self, role: &DisplayRole, text: &str) {
        for &sub_idx in self.chat_index.values() {
            self.chats[sub_idx].mark_finished(role.clone(), text);
        }
        self.chat_index.clear();
        self.subagent_answers.clear();
        self.subagent_prompts.clear();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    fn route_text_paste(&mut self, text: &str) {
        if self.plan_form_active() {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.float_mgr.handle_paste(text) {
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            let chat = &mut self.chats[self.active_chat];
            let texts = chat.segment_search_texts();
            self.search_modal.update_matches(&texts);
            sync_search_highlight(&self.search_modal, chat);
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.handle_paste(text) {
                    return;
                }
            };
        }
        try_picker!(self.file_picker);
        try_picker!(self.task_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.theme_picker);
        try_picker!(self.model_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.login_picker);
        if let InputAction::PaletteSync(val) = self.input_box.handle_paste(text)
            && self.is_main_chat()
        {
            self.command_palette.sync(&val);
        }
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_form.hide();
                vec![]
            }
            PlanFormAction::OpenEditor => {
                if let Some(p) = self.state.plan.path() {
                    vec![Action::OpenEditor(p.to_path_buf())]
                } else {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            }
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
        }
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        let parallel = self.plan_form.parallel();
        self.plan_form.reset();
        let plan_snapshot = match &self.state.plan {
            PlanState::Ready(p) => Some((
                std::fs::read_to_string(p).unwrap_or_else(|_| String::default()),
                p.display().to_string(),
            )),
            _ => None,
        };

        self.state.mode = Mode::Build;

        let text = if let Some((content, path_str)) = plan_snapshot.as_ref() {
            let text = if parallel {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}")
            } else {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`.")
            };
            if !clear_context {
                self.main_chat()
                    .push(DisplayMessage::plan(content.clone(), path_str.clone()));
            }
            text
        } else {
            format!("{IMPLEMENT_MSG_PREFIX}.")
        };

        let msg = QueuedMessage {
            text,
            images: vec![],
            control: false,
        };

        if clear_context {
            self.pending_plan_submit = Some(PendingPlanSubmit {
                message: msg,
                plan: plan_snapshot,
            });
            self.reset_session()
        } else {
            self.run_id += 1;
            self.start_from_queue(&msg)
        }
    }
}

fn is_streaming_stop_key(key: KeyEvent) -> bool {
    key::QUIT.matches(key) || key.code == KeyCode::Esc
}

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}
