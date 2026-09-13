use super::session::message_tool_use_ids;
use super::*;
use crate::agent::{Delivery, shared_queue};
use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
use crate::components::command::ParsedCommand;
use crate::components::keybindings::{KeybindContext, key as kb};
use crate::components::{ExitRequest, key, test_model};
use crate::selection::{SelectableZone, SelectionState, SelectionZone};
use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use n00n_agent::permissions::PermissionManager;
use n00n_agent::tools::{SessionIdentity, ToolRegistry};
use n00n_agent::{
    ExtractedCommand, ImageMediaType, InterruptPoint, InterruptSource, McpConfigErrors,
    McpPromptArg, McpServerInfo, McpServerStatus, McpSnapshot, McpSnapshotReader, ToolDoneEvent,
    ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use n00n_config::{PermissionsConfig, UiConfig};
use n00n_lua::{HintReader, KeymapReader, LuaCommandReader, PluginHost};
use n00n_providers::{ContentBlock, Effort, Role, TokenUsage};
use n00n_storage::id::SessionRef;
use n00n_storage::sessions::{
    StoredMode, StoredSessionLifecycle, StoredSessionStateSnapshot, StoredStateScope,
    StoredThinking, TranscriptEntry,
};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use ratatui_image::picker::Picker;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;

const PROVIDER_FAILED_ERR: &str = "provider failed";
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

fn set_zone(app: &mut App, zone: SelectionZone, area: Rect) {
    app.zones.push(SelectableZone {
        area,
        zone,
        scroll_info: None,
    });
}

fn build_app(dir: StateDir, writer: Arc<StorageWriter>) -> App {
    build_app_with_mcp(dir, writer, McpSnapshotReader::empty())
}

fn build_app_with_mcp(
    dir: StateDir,
    writer: Arc<StorageWriter>,
    mcp_reader: McpSnapshotReader,
) -> App {
    build_app_with_session(
        dir,
        writer,
        mcp_reader,
        AppSession::new("test-model", "/tmp/test"),
    )
}

fn build_app_with_session(
    dir: StateDir,
    writer: Arc<StorageWriter>,
    mcp_reader: McpSnapshotReader,
    session: AppSession,
) -> App {
    let model = test_model();
    App::new(AppInit {
        model,
        session,
        storage: dir,
        available_models: Arc::new(ArcSwapOption::empty()),
        mcp_reader,
        mcp_config_errors: McpConfigErrors::new(PathBuf::new()),
        lua_command_reader: LuaCommandReader::empty(),
        keymap_reader: KeymapReader::empty(),
        effective_keymap: Arc::new(EffectiveKeymap::default()),
        hint_reader: HintReader::empty(),
        storage_writer: writer,
        ui_config: UiConfig::default(),
        input_history_size: 100,
        retention_budget: RetentionBudget::default(),
        permissions: Arc::new(PermissionManager::new(
            PermissionsConfig {
                rules: vec![],
                ..Default::default()
            },
            PathBuf::from("/tmp"),
        )),
        custom_commands: Arc::from([]),
        picker: Arc::new(Picker::halfblocks()),
    })
}

fn isolated_app() -> App {
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let mut app = build_app(dir, Arc::clone(&writer));
    app.test_state_dir = Some(TestStateDir {
        dir: Some(tmp),
        writer: Some(writer),
    });
    app
}

fn test_app() -> App {
    let mut app = isolated_app();
    let (shared_queue, _rx) = shared_queue::queue();
    app.queue.set_shared(shared_queue);
    app
}

#[test]
fn test_app_cleans_up_isolated_state_directory() {
    let path = {
        let mut app = test_app();
        let path = app.storage.path().to_path_buf();
        app.state
            .session
            .messages
            .push(Message::user("persist before cleanup".into()));
        app.save_session();
        assert!(path.exists());
        path
    };

    assert!(!path.exists());
}

fn tempdir_app() -> (TempDir, StateDir, Arc<StorageWriter>, App) {
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let app = build_app(dir.clone(), Arc::clone(&writer));
    (tmp, dir, writer, app)
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn agent_msg(event: AgentEvent) -> Msg {
    agent_msg_with_run_id(event, 1)
}

fn agent_msg_with_run_id(event: AgentEvent, run_id: u64) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: None,
        run_id,
    }))
}

fn subagent_info(parent_id: &str, name: &str) -> SubagentInfo {
    subagent_info_with_tx(parent_id, name, None)
}

fn subagent_info_with_tx(
    parent_id: &str,
    name: &str,
    answer_tx: Option<flume::Sender<String>>,
) -> SubagentInfo {
    subagent_info_with_channels(parent_id, parent_id, name, answer_tx, None)
}

fn subagent_info_with_channels(
    session_id: &str,
    _parent_id: &str,
    name: &str,
    answer_tx: Option<flume::Sender<String>>,
    prompt_tx: Option<flume::Sender<SubagentPrompt>>,
) -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: session_id.into(),
        name: name.into(),
        prompt: None,
        model: None,
        answer_tx,
        prompt_tx,
    }
}

fn subagent_msg(event: AgentEvent, parent_id: &str, name: Option<&str>) -> Msg {
    subagent_msg_with_run_id(event, parent_id, name, 1)
}

fn subagent_msg_with_run_id(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    run_id: u64,
) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(subagent_info(parent_id, name.unwrap_or_else(|| "Agent"))),
        run_id,
    }))
}

fn subagent_msg_with_prompt(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    prompt: Option<&str>,
) -> Msg {
    let mut info = subagent_info(parent_id, name.unwrap_or_else(|| "Agent"));
    info.prompt = prompt.map(String::from);
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

fn subagent_msg_with_model(event: AgentEvent, parent_id: &str, name: &str, model: &str) -> Msg {
    let mut info = subagent_info(parent_id, name);
    info.model = Some(model.into());
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

#[test]
fn typing_and_submit() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(s) if s.input.message == "hi"));
    assert_eq!(app.status, Status::Streaming);
    assert!(app.input_box.is_empty());
    // Regression check: the bubble has to be on screen the same frame we
    // submit, otherwise it briefly sits one row too high before snapping down.
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
    assert_eq!(app.main_chat().last_message_text(), "hi");
}

#[derive(Clone)]
struct ManualSubmissionClock(Arc<std::sync::Mutex<Instant>>);

impl SubmissionClock for ManualSubmissionClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

impl ManualSubmissionClock {
    fn advance(&self, duration: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += duration;
    }
}

fn install_manual_submission_clock(app: &mut App) -> ManualSubmissionClock {
    let clock = ManualSubmissionClock(Arc::new(std::sync::Mutex::new(Instant::now())));
    app.submission_clock = Arc::new(clock.clone());
    clock
}

#[test]
fn rapid_submissions_keep_fifo_while_first_waits_for_persistence() {
    let mut app = test_app();
    let (shared, receiver) = shared_queue::queue();
    app.queue.set_shared(shared);

    let first = type_and_submit(&mut app, "first");
    let Action::SendMessage(first_dispatch) = first.into_iter().next().unwrap() else {
        panic!("expected first submission dispatch");
    };
    app.handle_submit(Submission {
        text: "second".into(),
        images: Vec::new(),
        control: false,
    });

    assert!(
        receiver
            .poll(n00n_agent::InterruptPoint::ToolComplete)
            .is_none()
    );

    let first_id = first_dispatch.submission_id;
    assert!(
        app.queue
            .mark_submission_ready(first_id, first_dispatch.input)
    );
    let crate::agent::shared_queue::QueueItem::Message { input, .. } = receiver.pop().unwrap()
    else {
        panic!("expected first queued submission");
    };
    assert_eq!(input.message, "first");
    let n00n_agent::ExtractedCommand::Interrupt(second, _) = receiver
        .poll(n00n_agent::InterruptPoint::ToolComplete)
        .unwrap()
    else {
        panic!("expected second queued submission");
    };
    assert_eq!(second.message, "second");
}

#[test]
fn session_api_prompt_is_explicitly_non_paint_gated() {
    let mut app = test_app();
    let outcome = app.submit_background_prompt(crate::app::queue::QueuedMessage {
        text: "background prompt".into(),
        images: Vec::new(),
        control: false,
    });
    let SubmitOutcome::Started(actions) = outcome else {
        panic!("expected background prompt to start");
    };
    let Action::SendMessage(dispatch) = &actions[0] else {
        panic!("expected submission dispatch");
    };

    assert!(!dispatch.paint_required);
    assert_eq!(app.main_chat().message_count(), 0);
}

#[test]
fn session_api_control_prompt_steers_with_control_tag() {
    let mut app = test_app();
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    app.status = Status::Streaming;

    assert!(matches!(
        app.submit_control_prompt(QueuedMessage {
            text: "resume".into(),
            images: Vec::new(),
            control: true,
        }),
        SubmitOutcome::Queued
    ));
    let Some(n00n_agent::ExtractedCommand::Interrupt(input, _)) =
        receiver.poll(n00n_agent::InterruptPoint::ToolComplete)
    else {
        panic!("expected steering interrupt");
    };
    assert_eq!(input.message, "resume");
    assert!(input.control);
}

#[test]
fn background_persistence_failure_is_terminal_without_composer_restore() {
    let mut app = test_app();
    let SubmitOutcome::Started(actions) = app.submit_background_prompt(QueuedMessage {
        text: "background prompt".into(),
        images: Vec::new(),
        control: false,
    }) else {
        panic!("expected background prompt to start");
    };
    let Action::SendMessage(dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected background submission dispatch");
    };
    let submission_id = dispatch.submission_id;
    let gate = Arc::clone(&dispatch.gate);
    assert!(matches!(
        app.submit_background_prompt(QueuedMessage {
            text: "queued after failure".into(),
            images: Vec::new(),
            control: false,
        }),
        SubmitOutcome::Queued
    ));

    app.handle_submission_persistence_failure(&dispatch);

    assert!(gate.is_cancelled());
    assert!(matches!(
        &app.status,
        Status::Error { message, .. } if message == PERSISTENCE_FAILURE_MSG
    ));
    assert_eq!(app.main_chat().last_message_text(), PERSISTENCE_FAILURE_MSG);
    assert!(app.input_box.is_empty());
    assert_eq!(app.queue.text_messages(), vec!["queued after failure"]);
    assert_eq!(submission_id, dispatch.submission_id);
}

#[test]
fn escape_before_dispatch_restores_text_and_exact_images_once() {
    let mut app = test_app();
    install_manual_submission_clock(&mut app);
    app.input_box.set_input("describe");
    with_image(&mut app);

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let Action::SendMessage(dispatch) = &actions[0] else {
        panic!("expected submission dispatch");
    };
    let gate = Arc::clone(&dispatch.gate);
    assert_eq!(app.main_chat().message_count(), 1);

    let cancel_actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(cancel_actions.is_empty());
    assert!(gate.is_cancelled());
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.main_chat().message_count(), 0);
    let restored = app.input_box.submit().unwrap();
    assert_eq!(restored.text, "describe");
    assert_eq!(restored.images.len(), 1);
    assert_eq!(restored.images[0].media_type, ImageMediaType::Png);
    assert_eq!(&*restored.images[0].data, "dGVzdA==");

    app.handle_submission_persistence_failure(dispatch);
    assert_eq!(app.main_chat().message_count(), 0);
    assert!(app.input_box.is_empty());
}

#[test]
fn committed_submission_esc_cancels_agent() {
    let mut app = test_app();
    install_manual_submission_clock(&mut app);
    let actions = type_and_submit(&mut app, "sent");
    let Action::SendMessage(dispatch) = &actions[0] else {
        panic!("expected submission dispatch");
    };
    assert!(dispatch.gate.try_commit());

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[..], [Action::CancelAgent { .. }]));
    assert!(app.input_box.is_empty());
    assert_eq!(app.main_chat().last_message_text(), "Cancelled.");
}

#[test]
fn shell_preamble_restores_once_on_cancel_and_resubmits_once() {
    let mut app = test_app();
    install_manual_submission_clock(&mut app);
    app.shell.push_result(Message::user("shell output".into()));

    let actions = type_and_submit(&mut app, "use context");
    let Action::SendMessage(mut dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected submission dispatch");
    };
    assert!(app.stage_submission_preamble(&mut dispatch));
    assert_eq!(dispatch.input.preamble.len(), 1);
    assert!(app.shell.drain_results().is_empty());

    app.update(Msg::Key(key(KeyCode::Esc)));
    let restored_preamble = app.shell.drain_results();
    assert_eq!(restored_preamble.len(), 1);
    assert_eq!(restored_preamble[0].user_text(), Some("shell output"));
    app.shell.restore_results(restored_preamble);
    let submission = app.input_box.submit().expect("cancel restores submission");
    let actions = app.handle_submit(submission);
    let Action::SendMessage(mut dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected resubmission dispatch");
    };
    assert!(app.stage_submission_preamble(&mut dispatch));
    assert_eq!(dispatch.input.preamble.len(), 1);
    assert_eq!(dispatch.input.preamble[0].user_text(), Some("shell output"));
    assert!(app.shell.drain_results().is_empty());

    app.handle_submission_persistence_failure(&dispatch);
    let restored_after_failure = app.shell.drain_results();
    assert_eq!(restored_after_failure.len(), 1);
    assert_eq!(restored_after_failure[0].user_text(), Some("shell output"));
}
#[test]
fn escape_during_mcp_error_restores_and_resubmits_exactly_once() {
    let mut app = test_app();
    install_manual_submission_clock(&mut app);
    app.shell.push_result(Message::user("mcp context".into()));

    let actions = type_and_submit(&mut app, "review");
    let Action::SendMessage(mut dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected submission dispatch");
    };
    assert!(app.stage_submission_preamble(&mut dispatch));
    app.update(agent_msg(AgentEvent::Error {
        message: "MCP prompt failed".into(),
    }));

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(app.main_chat().message_count(), 0);
    assert_eq!(app.input_box.buffer.value(), "review");
    let restored = app.shell.drain_results();
    assert_eq!(restored.len(), 1);
    app.shell.restore_results(restored);

    let submission = app.input_box.submit().expect("restored submission");
    let actions = app.handle_submit(submission);
    assert_eq!(actions.len(), 1);
    let Action::SendMessage(mut retry) = actions.into_iter().next().unwrap() else {
        panic!("expected retry dispatch");
    };
    assert!(app.stage_submission_preamble(&mut retry));
    assert_eq!(retry.input.preamble.len(), 1);
    assert!(app.shell.drain_results().is_empty());
}

#[test]
fn expired_escape_window_esc_cancels_agent() {
    let mut app = test_app();
    let clock = install_manual_submission_clock(&mut app);
    type_and_submit(&mut app, "too late");
    clock.advance(SUBMISSION_ESCAPE_WINDOW + Duration::from_millis(1));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(matches!(&actions[..], [Action::CancelAgent { .. }]));
    assert!(app.input_box.is_empty());
    assert_eq!(app.main_chat().last_message_text(), "Cancelled.");
    assert_eq!(app.status, Status::Idle);
}

#[test]
fn escape_at_submission_window_boundary_restores() {
    let mut app = test_app();
    let clock = install_manual_submission_clock(&mut app);
    type_and_submit(&mut app, "at boundary");
    clock.advance(SUBMISSION_ESCAPE_WINDOW);

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.input_box.buffer.value(), "at boundary");
}

#[test]
fn escape_after_submission_window_cancels_agent() {
    let mut app = test_app();
    let clock = install_manual_submission_clock(&mut app);
    type_and_submit(&mut app, "after boundary");
    clock.advance(SUBMISSION_ESCAPE_WINDOW + Duration::from_millis(1));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(matches!(&actions[..], [Action::CancelAgent { .. }]));
    assert_eq!(app.status, Status::Idle);
    assert!(app.input_box.is_empty());
}

fn with_text(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
}

fn with_image(app: &mut App) {
    let img = ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="));
    app.input_box.attach_image(img);
}

#[test_case(with_text as fn(&mut App)  ; "clears_text")]
#[test_case(with_image as fn(&mut App) ; "clears_image")]
fn ctrl_c_clears_nonempty_input(setup: fn(&mut App)) {
    let mut app = test_app();
    setup(&mut app);
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.input_box.is_empty());
}

#[test]
fn ctrl_c_quits_when_input_empty() {
    let mut app = test_app();
    app.status = Status::Idle;
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(actions.is_empty());
}

#[test_case(AgentEvent::Done { usage: TokenUsage::default(), num_turns: 1, stop_reason: None, fusion: None }, ExitRequest::Success ; "done_exits_success")]
#[test_case(AgentEvent::Error { message: "boom".into() }, ExitRequest::Error ; "error_exits_error")]
fn exit_on_done_flag_triggers_exit(event: AgentEvent, expected: ExitRequest) {
    let mut app = test_app();
    app.exit_on_done = true;
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(event));
    assert_eq!(app.exit_request, expected);
}

#[test]
fn toggle_mode_state_machine() {
    let tab = |app: &mut App| app.update(Msg::Key(key(KeyCode::Tab)));

    let mut app = test_app();
    assert_eq!(app.state.mode, Mode::Build);

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    let first_path = app.state.plan.path().unwrap().to_path_buf();
    assert!(first_path.to_str().unwrap().contains("plans"));

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(!app.state.plan.is_ready());

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.plan.mark_ready();
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(app.state.plan.is_ready());
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.mode = Mode::Build;
    app.status = Status::Streaming;
    app.run_id = 1;
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);
}

#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/plans/test.md".into()), Some("/tmp/plans/test.md".into()), true  ; "write_matching")]
#[test_case(ToolOutput::Diff { path: "/tmp/plans/test.md".into(), before: String::new(), after: String::new(), summary: String::new(), telemetry: None }, None, true  ; "edit_matching")]
#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/other.rs".into()), Some("/tmp/other.rs".into()), false ; "write_non_matching")]
fn tool_done_transitions_plan_to_ready(
    output: ToolOutput,
    written_path: Option<String>,
    expect_ready: bool,
) {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output,
        is_error: false,
        annotation: None,
        written_path,
    }))));

    assert_eq!(app.state.plan.is_ready(), expect_ready);
}

#[test]
fn altgr_chars_not_swallowed_by_ctrl_handler() {
    let mut app = test_app();
    let altgr_backslash = KeyEvent {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(altgr_backslash));
    assert_eq!(app.input_box.buffer.value(), "hi\\");
}

#[test_case(Status::Idle      ; "idle")]
#[test_case(Status::Streaming ; "streaming")]
fn paste_works_regardless_of_status(status: Status) {
    let mut app = test_app();
    app.status = status;
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), "pasted");
}

#[test_case("a\rb\rc",       "a\nb\nc"       ; "bare_cr")]
#[test_case("a\r\nb\r\nc",   "a\nb\nc"       ; "crlf")]
#[test_case("a\r\nb\rc\nd",  "a\nb\nc\nd"    ; "mixed")]
fn paste_normalizes_line_endings(input: &str, expected: &str) {
    let mut app = test_app();
    app.update(Msg::Paste(input.into()));
    assert_eq!(app.input_box.buffer.value(), expected);
}

#[test]
fn paste_file_path_triggers_image_load() {
    let mut app = test_app();
    app.update(Msg::Paste("file:///tmp/nonexistent.png".into()));
    assert!(!app.image_paste_rx.is_empty());
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn mixed_text_and_image_path_paste_loads_images_and_keeps_text() {
    let mut app = test_app();
    app.update(Msg::Paste(
        "Please compare these:\nfile:///tmp/first.png\nfile:///tmp/second.jpg\nThanks".into(),
    ));

    assert_eq!(app.image_paste_rx.len(), 2);
    assert_eq!(
        app.input_box.buffer.value(),
        "Please compare these:\nThanks"
    );
}

#[test]
fn busy_enter_queues_steering_and_second_chord_promotes_latest() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");

    app.input_box.set_input("steer one");
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    app.input_box.set_input("steer two");
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert_eq!(app.queue.len(), 2);
    assert_eq!(app.queue.panel_entries()[0].text, "↪ steer one");
    assert_eq!(app.queue.panel_entries()[1].text, "↪ steer two");

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.panel_entries()[0].text, "↪ steer one");
    assert_eq!(app.queue.panel_entries()[1].text, "↯ steer two");
}

#[test]
fn busy_tab_queues_multiple_turn_end_messages_without_toggling_mode() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let mode = app.state.mode;

    for text in ["later one", "later two"] {
        app.input_box.set_input(text);
        app.update(Msg::Key(key(KeyCode::Tab)));
    }

    assert_eq!(app.state.mode, mode);
    assert_eq!(app.queue.text_messages(), ["later one", "later two"]);
}

#[test]
fn queue_item_consumed_pushes_deferred_user_message() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    assert_eq!(app.main_chat().message_count(), 1);

    app.queue_and_notify(queued_msg("queued"));
    assert_eq!(
        app.main_chat().message_count(),
        1,
        "queueing while streaming must not render the bubble yet",
    );

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            text: "queued".into(),
            image_count: 0,
            images: Vec::new(),
            control: false,
        },
        app.run_id,
    ));

    assert_eq!(app.main_chat().message_count(), 2);
    assert_eq!(app.main_chat().last_message_text(), "queued");
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
}

#[test]
fn cancel_clears_queue() {
    let mut app = app_with_queued_message();
    cancel_app(&mut app);
    assert!(app.queue.is_empty());
}

#[test_case("/compact" ; "slash_command")]
#[test_case("exit" ; "exit_keyword")]
#[test_case("!ls" ; "shell_prefix")]
fn submit_prompt_never_interprets_text(text: &str) {
    let mut app = test_app();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Started(actions) => {
            assert!(matches!(&actions[0], Action::SendMessage(_)));
        }
        _ => panic!("raw prompt must start the agent"),
    }
}

#[test]
fn submit_prompt_queues_while_streaming() {
    let mut app = test_app();
    app.status = Status::Streaming;
    assert!(matches!(
        app.submit_prompt(queued_msg("hi")),
        SubmitOutcome::Queued
    ));
    assert_eq!(app.queue.len(), 1);
}

#[test_case(test_app as fn() -> App, "   ", queue::EMPTY_PROMPT_ERR ; "blank_text")]
#[test_case(streaming_app_without_queue, "hi", queue::NO_QUEUE_ERR ; "streaming_without_shared_queue")]
fn submit_prompt_rejects(mk: fn() -> App, text: &str, expected: &str) {
    let mut app = mk();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Rejected(e) => assert_eq!(e, expected),
        _ => panic!("expected rejection"),
    }
}

fn streaming_app_without_queue() -> App {
    let mut app = isolated_app();
    app.status = Status::Streaming;
    app
}

fn queued_msg(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.into(),
        images: vec![],
        control: false,
    }
}

fn app_with_queued_message() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
    app
}

fn type_and_submit(app: &mut App, text: &str) -> Vec<Action> {
    for c in text.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn cancel_app(app: &mut App) {
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
}

fn error_app(app: &mut App) {
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
}

fn cmd(name: &str) -> ParsedCommand {
    ParsedCommand {
        name: name.to_string(),
        args: String::new(),
    }
}

fn type_slash(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('/'))));
}

#[test]
fn typing_filters_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Char('z'))));
    assert!(!app.command_palette.is_active());
}

#[test]
fn enter_executes_new_command() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::NewSession { .. }));
    assert!(!app.command_palette.is_active());
}

#[test]
fn ctrl_c_closes_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.command_palette.is_active());
}

#[test]
fn reset_session_clears_plan() {
    let mut app = test_app();
    app.state.token_usage.input = 500;
    app.chats[0].context_size = 1000;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
    app.help_modal.toggle();
    let (_tx, rx) = flume::bounded::<crate::components::btw_modal::BtwEvent>(1);
    app.btw_modal.open("q", rx);
    let previous_id = app.state.session.id;
    let actions = app.reset_session();
    assert!(matches!(
        &actions[0],
        Action::NewSession { previous_id: id } if *id == previous_id
    ));
    assert_ne!(app.state.session.id, previous_id);
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.state.token_usage.input, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
    assert!(app.queue.is_empty());
    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "Main");
    assert_eq!(app.active_chat, 0);
    assert!(app.chat_index.is_empty());
    assert!(app.queue.focus().is_none());
    assert!(!app.help_modal.is_open());
    assert!(!app.btw_modal.is_open());
}

#[test]
fn reset_session_assigns_new_plan_path_in_plan_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("old-plan.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Plan);
    assert!(app.state.plan.path().is_some());
    assert_ne!(app.state.plan.path(), Some(Path::new("old-plan.md")));
}

#[test]
fn reset_session_clears_drafting_plan_in_build_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Drafting(PathBuf::from("leftover.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
}

#[test]
fn load_session_clears_plan() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    app.state
        .session
        .messages
        .push(Message::user("test".into()));
    app.state.session.save(&app.storage).unwrap();
    let id = app.state.session.id;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("old-plan.md"));
    app.load_session(id);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan.path(), None);
}

#[test]
fn picker_load_carries_recursive_transcript_through_recompact_and_save() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let summary = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "first summary".into(),
        }],
        ..Default::default()
    };
    app.state.session.messages = vec![Message::user("summary prompt".into()), summary.clone()];
    app.state.session.transcript = vec![
        TranscriptEntry::Compaction {
            entries: vec![TranscriptEntry::Compaction {
                entries: vec![TranscriptEntry::Message(Message::user("original".into()))],
                generated_summary: None,
                state_revision: None,
            }],
            generated_summary: Some(summary.clone()),
            state_revision: None,
        },
        TranscriptEntry::GeneratedMessage(Message::user("summary prompt".into())),
        TranscriptEntry::GeneratedMessage(summary),
    ];
    app.state.session.save(&app.storage).unwrap();
    let id = app.state.session.id;

    let actions = app.load_session(id);
    let Action::LoadSession(loaded) = actions.into_iter().next().expect("load action") else {
        panic!("expected loaded session action");
    };
    assert!(matches!(
        loaded.transcript.as_slice(),
        [TranscriptEntry::Compaction { entries, .. }, ..]
            if matches!(entries.as_slice(), [TranscriptEntry::Compaction { .. }])
    ));

    let message_mirror = Arc::new(ArcSwap::from_pointee(loaded.messages.clone()));
    let transcript_mirror = Arc::new(ArcSwap::from_pointee(loaded.transcript.clone()));
    let mut history =
        n00n_agent::agent::History::restored_with_transcript(loaded.messages, loaded.transcript)
            .with_mirror(Arc::clone(&message_mirror))
            .with_transcript_mirror(Arc::clone(&transcript_mirror));
    history.push(Message::user("continued".into()));
    history.compact_boundary(
        Message::user("summary prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "second summary".into(),
            }],
            ..Default::default()
        },
        None,
    );
    app.shared_history = Some(message_mirror);
    app.shared_transcript = Some(transcript_mirror);
    app.save_session();
    drain_writer(app, writer);

    let saved = AppSession::load(id, &dir).unwrap();
    assert!(matches!(
        saved.transcript.as_slice(),
        [TranscriptEntry::Compaction { entries, .. }, TranscriptEntry::GeneratedMessage(_), TranscriptEntry::GeneratedMessage(_)]
            if matches!(entries.as_slice(), [TranscriptEntry::Compaction { entries, .. }, TranscriptEntry::GeneratedMessage(_), TranscriptEntry::GeneratedMessage(_), TranscriptEntry::Message(_)] if matches!(entries.as_slice(), [TranscriptEntry::Compaction { .. }]))
    ));
}

#[test]
fn tab_in_palette_completes_command() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Tab)));
    let val = app.input_box.buffer.value();
    assert!(val.starts_with('/'));
}

#[test]
fn ctrl_p_n_navigation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active_chat, 0);

    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);

    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);
}

#[test]
fn subagents_get_descriptive_names() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        "task1",
        Some("first"),
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task2",
        Some("second"),
    ));
    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.chats[1].name, "first");
    assert_eq!(app.chats[2].name, "second");
}

#[test]
fn subagent_prompt_shown_once_and_not_duplicated() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        "task1",
        Some("research"),
        Some("Find all TODO comments"),
    ));
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), "Find all TODO comments");

    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task1",
        Some("research"),
    ));
    app.chats[1].flush();
    assert_eq!(app.chats[1].message_count(), 2);
    assert_eq!(app.chats[1].last_message_text(), "ab");
}

#[test]
fn turn_complete_tracks_usage_and_context_per_chat() {
    let mut app = app_with_subagent();

    let main_usage = TokenUsage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    app.update(agent_msg(AgentEvent::TurnComplete(Box::new(
        TurnCompleteEvent {
            message: Message::default(),
            usage: main_usage,
            model: "test".into(),
            context_size: None,
        },
    ))));

    let sub_usage = TokenUsage {
        input: 200,
        output: 75,
        ..Default::default()
    };
    app.update(subagent_msg(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage: sub_usage,
            model: "test".into(),
            context_size: None,
        })),
        "task1",
        None,
    ));

    assert_eq!(app.state.token_usage.input, 300);
    assert_eq!(app.state.token_usage.output, 125);
    assert_eq!(app.chats[0].token_usage.input, 100);
    assert_eq!(app.chats[1].token_usage.input, 200);
    assert_eq!(app.chats[0].context_size, main_usage.context_tokens());
    assert_eq!(app.chats[1].context_size, sub_usage.context_tokens());
}

#[test]
fn live_compaction_notice_becomes_typed_card() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.shared_transcript = Some(Arc::new(ArcSwap::from_pointee(vec![
        TranscriptEntry::Compaction {
            entries: vec![TranscriptEntry::Message(Message::user("original".into()))],
            generated_summary: None,
            state_revision: None,
        },
    ])));

    app.update(agent_msg(AgentEvent::AutoCompacting));
    assert!(app.main_chat().has_pending_compaction());

    app.update(agent_msg(AgentEvent::CompactionDone {
        state_revision: Some(1),
    }));
    assert!(!app.main_chat().has_pending_compaction());
    assert_eq!(app.main_chat().compaction_card_count(), 1);
}

#[test]
fn subagent_compaction_completion_uses_live_summary_without_touching_main_transcript() {
    let mut app = app_with_subagent();
    let main_transcript = Arc::new(ArcSwap::from_pointee(vec![TranscriptEntry::Message(
        Message::user("main conversation".into()),
    )]));
    app.shared_transcript = Some(Arc::clone(&main_transcript));

    app.update(subagent_msg(
        AgentEvent::AutoCompacting,
        "task1",
        Some("research"),
    ));
    assert!(app.chats[1].has_pending_compaction());
    app.update(subagent_msg(
        AgentEvent::TextDelta {
            text: "subagent summary".into(),
        },
        "task1",
        Some("research"),
    ));
    app.update(subagent_msg(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "subagent summary".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            model: "test".into(),
            context_size: None,
        })),
        "task1",
        Some("research"),
    ));
    app.update(subagent_msg(
        AgentEvent::CompactionDone {
            state_revision: Some(1),
        },
        "task1",
        Some("research"),
    ));

    assert!(!app.chats[1].has_pending_compaction());
    assert_eq!(app.chats[1].compaction_card_count(), 1);
    assert_eq!(
        app.chats[1].message_count(),
        2,
        "the streamed summary must not remain beside its compaction card"
    );
    assert_eq!(
        app.chats[1].last_compaction_summary(),
        Some("subagent summary")
    );
    assert_eq!(app.main_chat().compaction_card_count(), 0);
    assert!(matches!(
        main_transcript.load().as_slice(),
        [TranscriptEntry::Message(message)] if message.user_text() == Some("main conversation")
    ));
}

#[test]
fn turn_complete_accumulates_usage_by_model() {
    let mut app = app_with_subagent();

    app.update(agent_msg(AgentEvent::TurnComplete(Box::new(
        TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage {
                input: 100,
                output: 50,
                cache_read: 10,
                ..Default::default()
            },
            model: "main-model".into(),
            context_size: None,
        },
    ))));
    app.update(subagent_msg(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage {
                input: 200,
                output: 75,
                ..Default::default()
            },
            model: "sub-model".into(),
            context_size: None,
        })),
        "task1",
        None,
    ));

    let by_model = &app.state.session.meta.usage_by_model;
    assert_eq!(by_model.len(), 2);
    let main = &by_model["main-model"];
    assert_eq!(main.input, 100);
    assert_eq!(main.output, 50);
    assert_eq!(main.cache_read, 10);
    let sub = &by_model["sub-model"];
    assert_eq!(sub.input, 200);
    assert_eq!(sub.output, 75);
}
#[test]
fn cancel_resets_all_chats_and_indices() {
    let mut app = app_with_subagent();
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "sub_t1".into(),
            tool: "bash".into(),
            summary: "running".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        None,
    ));

    cancel_app(&mut app);
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chat_index.is_empty());
}

fn finish_subagent(app: &mut App, id: &str, is_error: bool) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: "task".into(),
        output: ToolOutput::Plain("result".into()),
        is_error,
        annotation: None,
        written_path: None,
    }))));
}

fn finish_subagent_task(app: &mut App, is_error: bool) {
    finish_subagent(app, "task1", is_error);
}

#[test]
fn subagent_done_only_in_subagent_chat() {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    assert_ne!(app.chats[0].last_message_role(), Some(&DisplayRole::Done));
}

#[test_case(|app: &mut App| finish_subagent_task(app, false), DONE_TEXT,      &DisplayRole::Done  ; "task_success")]
#[test_case(|app: &mut App| finish_subagent_task(app, true),  ERROR_TEXT,     &DisplayRole::Error ; "task_failure")]
#[test_case(cancel_app as fn(&mut App),                       CANCELLED_TEXT, &DisplayRole::Error ; "cancel")]
#[test_case(error_app  as fn(&mut App),                       ERROR_TEXT,     &DisplayRole::Error ; "main_error")]
fn subagent_terminal_marker(
    terminate: fn(&mut App),
    expected_text: &str,
    expected_role: &DisplayRole,
) {
    let mut app = app_with_subagent();
    terminate(&mut app);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

#[test_case(error_app  as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn subagent_already_done_not_double_marked(terminate: fn(&mut App)) {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    let count_before = app.chats[1].message_count();
    terminate(&mut app);
    assert_eq!(app.chats[1].message_count(), count_before);
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case(false, DONE_TEXT,  &DisplayRole::Done  ; "batch_subagent_success")]
#[test_case(true,  ERROR_TEXT, &DisplayRole::Error ; "batch_subagent_failure")]
fn batch_subagent_done_marker(is_error: bool, expected_text: &str, expected_role: &DisplayRole) {
    let mut app = app_with_subagent_id("batch1__0");
    finish_subagent(&mut app, "batch1__0", is_error);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

#[test]
fn completed_subagent_chat_remains_discoverable_by_tool_id() {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    app.chat_index.clear();

    let idx = app
        .chats
        .iter()
        .position(|chat| chat.tool_use_id.as_deref() == Some("task1"));
    assert_eq!(idx, Some(1));
}

#[test]
fn completed_task_card_click_toggles_inline_without_navigating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    app.update(subagent_msg(
        AgentEvent::TextDelta {
            text: "child".into(),
        },
        "task1",
        Some("research"),
    ));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "task1".into(),
        tool: "task".into(),
        output: ToolOutput::Markdown("body line\n".repeat(100).into()),
        is_error: false,
        annotation: None,
        written_path: None,
    }))));

    let mut terminal = Terminal::new(TestBackend::new(80, 80)).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    let area = app.msg_area();
    let collapsed_lines = app.chats[0].total_lines();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        10,
        area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        10,
        area.y,
    ));
    assert_eq!(app.active_chat, 0, "task card click must not navigate");
    terminal.draw(|frame| app.view(frame)).unwrap();
    assert!(
        app.chats[0].total_lines() > collapsed_lines,
        "task card click must expand inline"
    );

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        10,
        area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        10,
        area.y,
    ));
    assert_eq!(
        app.active_chat, 0,
        "repeated task card click must not navigate"
    );
    terminal.draw(|frame| app.view(frame)).unwrap();
    assert_eq!(app.chats[0].total_lines(), collapsed_lines);
}

fn open_tasks_picker(app: &mut App) {
    for c in "/tasks".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
}
#[test]
fn agent_picker_exposes_names_models_and_status() {
    let mut app = app_with_subagent();
    app.chats[1].model_id = Some("openai/test-model".into());
    open_tasks_picker(&mut app);

    let main = app.task_picker.item(0).unwrap();
    assert_eq!(main.label(), "Main chat");
    assert_eq!(main.suffix(), Some("main session"));
    let agent = app.task_picker.item(1).unwrap();
    assert_eq!(agent.label(), "Agent: research");
    assert_eq!(agent.suffix(), Some("openai/test-model"));
    assert_eq!(agent.detail(), Some(TASK_RUNNING_DETAIL));
}

#[test]
fn ctrl_x_toggles_tasks_picker() {
    let mut app = test_app();
    app.update(Msg::Key(kb::TASKS.to_key_event()));
    assert!(app.task_picker.is_open());
    app.update(Msg::Key(kb::TASKS.to_key_event()));
    assert!(!app.task_picker.is_open());
}

fn app_with_subagent_id(id: &str) -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "x".into() },
        id,
        Some("research"),
    ));
    app
}

fn app_with_subagent() -> App {
    app_with_subagent_id("task1")
}

fn select_subagent_preview(app: &mut App) {
    open_tasks_picker(app);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.resolve_render_chat(), 1);
    assert_eq!(app.active_chat, 0);
}

fn render_chat(app: &mut App, chat: usize, area: Rect) {
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    terminal
        .draw(|frame| app.chats[chat].view(frame, area, false, false))
        .unwrap();
}

#[test]
fn task_picker_preview_selection_uses_rendered_chat_scroll() {
    let mut app = app_with_subagent();
    app.chats[0].restore_scroll(2, false);
    app.chats[1].restore_scroll(9, false);
    select_subagent_preview(&mut app);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));

    let (start, _) = app.selection_state.as_ref().unwrap().sel().normalized();
    assert_eq!(start.row, 14);
}

#[test]
fn task_picker_preview_copy_uses_rendered_chat() {
    let mut app = app_with_subagent();
    app.chats[1].flush();
    let area = Rect::new(0, 0, 79, 20);
    render_chat(&mut app, 1, area);
    select_subagent_preview(&mut app);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 78, 0));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 78, 0));

    assert!(app.status_bar.flash_text().is_some());
    assert_eq!(app.active_chat, 0);
}

#[test]
fn nested_task_preview_click_stays_inline_and_picker_enter_navigates() {
    let mut app = app_with_subagent();
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "task2".into(),
            tool: "task".into(),
            summary: "nested".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        None,
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta {
            text: "nested".into(),
        },
        "task2",
        Some("nested"),
    ));
    let area = Rect::new(0, 0, 79, 20);
    render_chat(&mut app, 1, area);
    let tool_row = (area.y..area.bottom())
        .find(|&row| app.chats[1].tool_id_at(row, area) == Some("task2"))
        .unwrap();
    select_subagent_preview(&mut app);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        1,
        tool_row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        1,
        tool_row,
    ));

    assert_eq!(
        app.active_chat, 0,
        "nested task card click must not navigate"
    );

    app.update(Msg::Key(kb::TASKS.to_key_event()));
    assert!(!app.task_picker.is_open());
    app.update(Msg::Key(kb::TASKS.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(
        app.active_chat, 2,
        "Ctrl+X and Enter must navigate explicitly"
    );
}

#[test]
fn task_picker_preview_scrollbar_uses_rendered_chat() {
    let mut app = app_with_subagent();
    for i in 0..40 {
        app.chats[1].push_user_message(format!("preview line {i}"));
    }
    let area = Rect::new(0, 0, 80, 10);
    render_chat(&mut app, 1, area);
    app.chats[1].scroll_to_top();
    select_subagent_preview(&mut app);
    let info = app.chats[1].scroll_info(area.height).unwrap();
    app.zones.push(SelectableZone {
        area,
        zone: SelectionZone::Messages,
        scroll_info: Some(info),
    });
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.right() - 1,
        area.bottom() - 1,
    ));

    assert_eq!(app.chats[0].scroll_top(), u16::MAX);
    assert!(app.chats[1].scroll_top() > 0);
}
#[test]
fn picker_escape_restores_chat() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);

    open_tasks_picker(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.task_picker.is_open());
    assert_eq!(app.active_chat, 0);
}

#[test]
fn picker_enter_stays_at_navigated() {
    let mut app = app_with_subagent();
    open_tasks_picker(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.task_picker.is_open());
    assert_eq!(app.active_chat, 1);
}

const OVERLAY_BLOCKED_KEYS: &[KeyEvent] = &[
    kb::NEXT_CHAT.to_key_event(),
    kb::PREV_CHAT.to_key_event(),
    kb::SCROLL_HALF_UP.to_key_event(),
    kb::SCROLL_HALF_DOWN.to_key_event(),
    kb::HELP.to_key_event(),
];

fn open_help(app: &mut App) {
    app.help_modal.toggle();
}

fn open_search(app: &mut App) {
    app.search_modal.open(0, true);
}

fn focus_queue(app: &mut App) {
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
}

#[test_case(open_tasks_picker as fn(&mut App) ; "task_picker")]
#[test_case(open_help                         ; "help_modal")]
#[test_case(open_search                       ; "search_modal")]
#[test_case(focus_queue                       ; "queue_focus")]
fn overlay_blocks_ctrl_shortcuts(setup: fn(&mut App)) {
    let mut app = app_with_subagent();
    setup(&mut app);
    let before = app.active_chat;
    let scroll_before = app.chats[app.active_chat].scroll_top();

    for k in OVERLAY_BLOCKED_KEYS {
        app.update(Msg::Key(*k));
    }

    assert_eq!(
        app.active_chat, before,
        "active_chat changed through overlay"
    );
    assert_eq!(
        app.chats[app.active_chat].scroll_top(),
        scroll_before,
        "scroll changed through overlay"
    );
}

#[test]
fn at_mention_opens_file_picker_and_esc_leaves_literal() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('@'))));
    assert!(app.file_picker.is_open());
    assert_eq!(app.input_box.buffer.value(), "");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.file_picker.is_open());
    assert_eq!(app.input_box.buffer.value(), "@");
}

#[test]
fn at_mention_does_not_open_mid_word() {
    let mut app = test_app();
    for c in "em".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Char('@'))));
    assert!(!app.file_picker.is_open());
    assert_eq!(app.input_box.buffer.value(), "em@");
}

#[test]
fn ctrl_s_stashes_and_restores_draft() {
    let mut app = test_app();
    for c in "draft".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(kb::STASH.to_key_event()));
    assert!(app.input_box.is_empty());
    assert_eq!(app.status_bar.flash_text(), Some("Draft stashed"));

    app.update(Msg::Key(kb::STASH.to_key_event()));
    assert_eq!(app.input_box.buffer.value(), "draft");
    assert_eq!(app.status_bar.flash_text(), Some("Draft restored"));
}

#[test]
fn ctrl_d_flashes_then_exits_on_second_press() {
    let mut app = test_app();
    let actions = app.update(Msg::Key(kb::DELETE.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Press Ctrl+D again to exit")
    );
    assert_eq!(app.exit_request, ExitRequest::None);

    app.update(Msg::Key(kb::DELETE.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
}

#[test]
fn ctrl_d_deletes_char_forward_with_text() {
    let mut app = test_app();
    for c in "ab".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Home)));

    let actions = app.update(Msg::Key(kb::DELETE.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.input_box.buffer.value(), "b");
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test]
fn unbound_ctrl_chords_never_insert_text() {
    let mut app = test_app();
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('m'),
        KeyModifiers::CONTROL,
    )));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )));
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn ctrl_r_history_search_end_to_end() {
    let mut app = test_app();
    type_and_submit(&mut app, "first prompt");
    type_and_submit(&mut app, "second prompt");

    app.update(Msg::Key(kb::HISTORY_SEARCH.to_key_event()));
    assert!(app.input_box.history_search_active());

    for c in "fir".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.input_box.buffer.value(), "first prompt");

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.input_box.history_search_active());
    assert_eq!(app.input_box.buffer.value(), "first prompt");
}

#[test]
fn ctrl_c_during_history_search_aborts_search_not_app() {
    let mut app = test_app();
    for c in "wip".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(kb::HISTORY_SEARCH.to_key_event()));
    assert!(app.input_box.history_search_active());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.input_box.history_search_active());
    assert_eq!(app.input_box.buffer.value(), "wip");
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::CancelAgent { .. }))
    );
}

#[test]
fn ctrl_c_during_search_while_streaming_keeps_agent() {
    let mut app = streaming_app_without_queue();
    app.update(Msg::Key(kb::HISTORY_SEARCH.to_key_event()));
    assert!(app.input_box.history_search_active());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.input_box.history_search_active());
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::CancelAgent { .. })),
        "Ctrl+C must abort the search, not the running agent"
    );
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn kitty_shifted_codepoint_still_matches_shifted_binds() {
    // REPORT_ALTERNATE_KEYS folds Shift into the codepoint and clears the
    // flag: Alt+Shift+G arrives as Char('G')+ALT, Ctrl+Shift+C as
    // Char('C')+CONTROL. Both must resolve to the shifted binding.
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(kb::CHAT_SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('G'),
        KeyModifiers::ALT,
    )));
    assert!(
        app.chats[0].auto_scroll(),
        "Alt+Shift+G (folded) should jump to bottom"
    );

    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.input_box.buffer.value(), "hi");
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test]
fn super_enter_submits_like_plain_enter() {
    let mut app = test_app();
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER)));
    assert_eq!(
        app.input_box.buffer.value(),
        "",
        "Super+Enter should submit"
    );
}

#[test]
fn ctrl_d_arm_resets_when_typing_between_presses() {
    let mut app = test_app();
    app.update(Msg::Key(kb::DELETE.to_key_event()));
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Press Ctrl+D again to exit")
    );
    // Intervening real input must disarm the double-press window.
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.update(Msg::Key(kb::DELETE.to_key_event()));
    assert_eq!(
        app.exit_request,
        ExitRequest::None,
        "intervening typing must reset the Ctrl+D exit arm"
    );
}

#[test]
fn ctrl_underscore_undoes_composer_edit() {
    let mut app = test_app();
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('_'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn shift_enter_inserts_newline() {
    let mut app = test_app();
    for c in "ab".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    assert_eq!(app.input_box.buffer.value(), "ab\n");
}

#[test]
fn compact_command_sets_streaming() {
    let mut app = test_app();
    let actions = app.execute_command(cmd("/compact"));
    assert!(matches!(&actions[0], Action::Compact));
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn compact_during_streaming_queues_item() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.execute_command(cmd("/compact"));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "/compact");
}

#[test]
fn cancel_clears_pending_input() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    cancel_app(&mut app);
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn scroll_disables_auto_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
    assert!(!app.chats[0].auto_scroll());
}

#[test]
fn scroll_outside_msg_area_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 25,
        delta: 3,
    });
    assert!(app.chats[0].auto_scroll());
}

#[test]
fn scroll_shortcuts_toggle_auto_scroll() {
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(kb::CHAT_SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    app.update(Msg::Key(kb::CHAT_SCROLL_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll());
}

#[test]
fn mouse_drag_updates_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 10));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel().normalized();
    assert_eq!(end.row, 10);
    assert_eq!(end.col, 20);
}

#[test]
fn mouse_drag_clamps_to_area() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        100,
        50,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel().normalized();
    assert_eq!(end.col, 78);
    assert_eq!(end.row, 19, "clamped to area bottom");
    assert!(
        app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "outside area triggers edge scroll"
    );
}

#[test_case(Rect::new(0, 2, 80, 20), (10, 12), (10, 1),  Some(EDGE_SCROLL_LINES)  ; "top_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (10, 22), Some(-EDGE_SCROLL_LINES) ; "bottom_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (20, 15), None                     ; "middle_no_scroll")]
fn edge_scroll_direction(zone: Rect, down: (u16, u16), drag: (u16, u16), expected: Option<i32>) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        down.0,
        down.1,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        drag.0,
        drag.1,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let edge_dir = match state {
        SelectionState::Dragging { edge_scroll, .. } => edge_scroll.as_ref().map(|es| es.dir),
        SelectionState::PendingCopy { .. } => None,
    };
    assert_eq!(edge_dir, expected);
}

#[test]
fn mouse_up_clears_edge_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 2, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 1));
    let state = app.selection_state.as_ref().unwrap();
    assert!(state.is_pending_copy());
}

#[test]
fn esc_cancels_flushes_and_fails_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta {
        text: "partial".into(),
    }));
    for id in ["t1", "t2"] {
        app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            tool: "bash".into(),
            summary: "running".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        }))));
    }
    render_chat(&mut app, 0, Rect::new(0, 0, 80, 20));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(
        app.chats[0].tool_status("t1"),
        Some(crate::components::ToolStatus::Error)
    );
    assert_eq!(
        app.chats[0].tool_status("t2"),
        Some(crate::components::ToolStatus::Error)
    );

    app.update(agent_msg_with_run_id(
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: "t1".into(),
            tool: "bash".into(),
            output: ToolOutput::Plain("late".into()),
            is_error: false,
            annotation: None,
            written_path: None,
        })),
        1,
    ));
    render_chat(&mut app, 0, Rect::new(0, 0, 80, 20));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(
        app.chats[0].tool_status("t1"),
        Some(crate::components::ToolStatus::Error),
        "stale success replaced cancellation"
    );
}

#[test]
fn double_esc_idle_opens_rewind_picker() {
    let mut app = test_app();
    type_and_submit(&mut app, "hello");
    app.status = Status::Idle;
    app.run_id = 1;
    app.state
        .session
        .messages
        .push(Message::user("hello".into()));

    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.rewind_picker.is_open());
}

#[test]
fn double_esc_idle_no_user_turns_flashes_error() {
    let mut app = test_app();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.rewind_picker.is_open());
}

#[test]
fn ctrl_c_while_streaming_cancels_instead_of_quitting() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_ne!(app.exit_request, ExitRequest::Success);
}

#[test]
fn streaming_status_keeps_app_animating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
        fusion: None,
    }));
    assert!(!app.is_animating());

    app.status = Status::Streaming;
    assert!(app.is_animating());
}

#[test]
fn edge_scroll_makes_app_animating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
        fusion: None,
    }));
    assert!(!app.is_animating());
    let zone = Rect::new(0, 2, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.is_animating());
}

#[test]
fn empty_click_clears_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 5));
    assert!(app.selection_state.is_none());
}

fn make_pending_copy(app: &mut App) {
    set_zone(app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 10));
}

fn send_key(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('a'))));
}

fn send_scroll(app: &mut App) {
    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
}

#[test_case(send_key as fn(&mut App) ; "key")]
fn interrupt_clears_dragging_but_preserves_pending_copy(interrupt: fn(&mut App)) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    interrupt(&mut app);
    assert!(app.selection_state.is_none(), "clears dragging");

    make_pending_copy(&mut app);
    interrupt(&mut app);
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "preserves pending copy"
    );
}

#[test]
fn scroll_preserves_dragging_and_updates_cursor() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));

    send_scroll(&mut app);

    assert!(
        matches!(
            app.selection_state.as_ref().unwrap(),
            SelectionState::Dragging { .. }
        ),
        "scroll keeps dragging"
    );

    make_pending_copy(&mut app);
    send_scroll(&mut app);
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "scroll preserves pending copy"
    );
}

#[test]
fn new_mouse_down_replaces_pending_copy_with_dragging() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 15));
    assert!(matches!(
        app.selection_state.as_ref().unwrap(),
        SelectionState::Dragging { .. }
    ));
}

#[test]
fn pending_copy_ignores_drag_and_tick() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 50, 50));
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());

    app.tick_edge_scroll();
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());
}

#[test]
fn pending_copy_not_animating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
        fusion: None,
    }));
    make_pending_copy(&mut app);
    assert!(!app.is_animating());
}

#[test]
fn edge_scroll_direction_switches_on_drag_reversal() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));

    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir > 0,
            "scrolling up (positive dir)"
        );
    } else {
        panic!("expected Dragging");
    }

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 16));
    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir < 0,
            "scrolling down after reversal"
        );
    } else {
        panic!("expected Dragging");
    }
}

#[test]
fn drag_back_into_area_clears_edge_scroll() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    assert!(
        !app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "dragging back into area must stop edge scroll"
    );
}

#[test]
fn mouse_down_outside_all_zones_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 40, 10));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 15));
    assert!(
        app.selection_state.is_none(),
        "click outside zones must not create selection"
    );
}

#[test_case(true  ; "non_empty")]
#[test_case(false ; "empty")]
fn queue_command_sets_focus(has_queue: bool) {
    let mut app = if has_queue {
        app_with_queued_message()
    } else {
        test_app()
    };
    app.execute_command(cmd("/queue"));
    assert_eq!(app.queue.focus().is_some(), has_queue);
}

#[test]
fn queue_boundary_clamps() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.queue.focus(), Some(0), "up at top clamps");
    app.queue.set_focus_at(1);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.queue.focus(), Some(1), "down at bottom clamps");
}

#[test]
fn queue_enter_edits_selected_in_place_without_duplication() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.text_messages(), ["second"]);
    assert_eq!(app.input_box.buffer.value(), "queued");
    app.input_box.set_input("edited");
    app.update(Msg::Key(key(KeyCode::Tab)));

    assert_eq!(app.queue.text_messages(), ["edited", "second"]);
    assert_eq!(app.queue.len(), 2);
}

#[test]
fn ctrl_c_cancels_queue_edit_and_restores_original_message() {
    let mut app = app_with_queued_message();
    let image = ImageSource::new(ImageMediaType::Png, Arc::from("b3JpZ2luYWw="));
    assert!(app.queue_steering(QueuedMessage {
        text: "original".into(),
        images: vec![image],
        control: true,
    }));
    app.queue_and_notify(queued_msg("after"));
    app.queue.set_focus_at(1);

    app.update(Msg::Key(key(KeyCode::Enter)));
    app.input_box.set_input("replacement");
    app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert_eq!(app.queue.text_messages(), ["queued", "original", "after"]);
    assert!(app.queue.editing().is_none());
    assert!(app.input_box.is_empty());
    let queued = app.queue.queued_inputs();
    let (input, delivery) = &queued[1];
    assert_eq!(input.message, "original");
    assert_eq!(*delivery, Delivery::Steering);
    assert!(input.control);
    assert_eq!(input.images.len(), 1);
    assert_eq!(input.images[0].media_type, ImageMediaType::Png);
    assert_eq!(&*input.images[0].data, "b3JpZ2luYWw=");
}

#[test]
fn queue_delete_removes_selected_visible_item() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(1);

    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.text_messages(), ["queued"]);
    assert_eq!(app.queue.focus(), Some(0));
}

#[test]
fn queue_esc_unfocuses_without_removing() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.queue.focus().is_none());
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn ctrl_q_pops_front() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
    assert!(app.queue.focus().is_none(), "unfocused stays unfocused");

    app.queue_and_notify(queued_msg("third"));
    app.queue.set_focus_at(1);
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(
        app.queue.focus(),
        Some(0),
        "focus adjusted when item removed"
    );
}

#[test_case(cancel_app as fn(&mut App) ; "cancel")]
#[test_case(error_app as fn(&mut App)  ; "error")]
fn clears_queue_focus_on_terminate(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);
    terminate(&mut app);
    assert!(app.queue.focus().is_none());
}

#[test]
fn stale_events_ignored_after_run_id_increment() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    let current_run = app.run_id;
    let actions = type_and_submit(&mut app, "new prompt");
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.input.message == "new prompt"));
    let active_run = app.run_id;

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "stale text".into(),
        },
        current_run,
    ));
    assert_eq!(app.chats[0].last_message_text(), "new prompt");

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "new text".into(),
        },
        active_run,
    ));
    app.chats[0].flush();
    assert_eq!(app.chats[0].last_message_text(), "new text");
}

#[test]
fn active_main_fusion_phase_is_visible() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::FusionPhase {
        phase: n00n_agent::FusionPhase::Executing,
        label: Some("brief label".into()),
    }));
    assert_eq!(
        app.main_chat().last_message_text(),
        "Executing: brief label"
    );
}

#[test]
fn stale_fusion_phase_is_ignored() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 2;
    let count_before = app.main_chat().message_count();
    app.update(agent_msg_with_run_id(
        AgentEvent::FusionPhase {
            phase: n00n_agent::FusionPhase::Reviewing,
            label: None,
        },
        1,
    ));
    assert_eq!(app.main_chat().message_count(), count_before);
}

#[test]
fn stale_done_does_not_drain_queue() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    app.queue_and_notify(queued_msg("next"));

    app.update(agent_msg_with_run_id(
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            stop_reason: None,
            fusion: None,
        },
        1,
    ));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.status, Status::Idle);
}

#[test]
fn mouse_down_in_input_creates_input_zone_selection() {
    let mut app = test_app();
    let input = Rect::new(0, 15, 80, 5);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 15));
    set_zone(&mut app, SelectionZone::Input, input);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 16));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Input);
    assert_eq!(state.sel().area, input);
}

#[test]
fn resolve_or_create_chat_sets_model_id_and_annotation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "hi".into() },
        "task1",
        "research",
        "anthropic/claude-sonnet-4-20250514",
    ));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(
        app.chats[1].model_id.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test]
fn help_toggles_modal() {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());
    app.update(Msg::Key(kb::HELP.to_key_event()));
    assert!(app.help_modal.is_open());
    app.execute_command(cmd("/help"));
    assert!(!app.help_modal.is_open());
}

#[test]
fn help_modal_consumes_keys_and_esc_closes() {
    let mut app = test_app();
    app.update(Msg::Key(kb::HELP.to_key_event()));

    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(app.input_box.buffer.value(), "");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.help_modal.is_open());
}

#[test_case(
    |_: &mut App| {},
    &[KeybindContext::General, KeybindContext::Editing],
    &[KeybindContext::Streaming]
    ; "idle"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; },
    &[KeybindContext::General, KeybindContext::Streaming, KeybindContext::Editing],
    &[]
    ; "streaming"
)]
#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    &[KeybindContext::FormInput],
    &[KeybindContext::Editing]
    ; "plan_form"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; app.run_id = 1; app.queue_and_notify(queued_msg("q")); app.queue.set_focus_at(0); },
    &[KeybindContext::QueueFocus],
    &[KeybindContext::Editing]
    ; "queue_focus"
)]
#[test_case(
    |app: &mut App| { open_tasks_picker(app); },
    &[KeybindContext::TaskPicker],
    &[KeybindContext::Editing]
    ; "task_picker"
)]
#[test_case(
    |app: &mut App| {
        app.state.session.messages.push(Message::user("test".into()));
        app.open_rewind_picker();
    },
    &[KeybindContext::RewindPicker],
    &[KeybindContext::Editing]
    ; "rewind_picker"
)]
fn active_contexts(setup: fn(&mut App), expected: &[KeybindContext], absent: &[KeybindContext]) {
    let mut app = test_app();
    setup(&mut app);
    let contexts = app.active_keybind_contexts();
    for ctx in expected {
        assert!(contexts.contains(ctx), "{ctx:?} should be present");
    }
    for ctx in absent {
        assert!(!contexts.contains(ctx), "{ctx:?} should be absent");
    }
}

#[test]
fn submit_exit_quits() {
    let mut app = test_app();
    let actions = app.handle_submit(Submission {
        text: "exit".into(),
        images: vec![],
        control: false,
    });
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(actions.is_empty());
}

#[test]
fn session_has_content_covers_each_branch() {
    let mut session = AppSession::new("test-model", "/tmp/test");
    assert!(!session_has_content(&session));

    session.meta.input_draft = Some("draft".into());
    assert!(session_has_content(&session));
    session.meta.input_draft = None;

    session.meta.queued_messages = vec!["queued".into()];
    assert!(session_has_content(&session));
    session.meta.queued_messages.clear();

    session.meta.lifecycle = StoredSessionLifecycle::Cancelled;
    assert!(session_has_content(&session));
    session.meta.lifecycle = StoredSessionLifecycle::Idle;

    session.meta.mode = Some(StoredMode::Plan);
    assert!(session_has_content(&session));
    session.meta.mode = Some(StoredMode::Build);

    session.subagent_messages.insert("child".into(), vec![]);
    assert!(session_has_content(&session));
    session.subagent_messages.clear();

    session.messages.push(Message::user("hello".into()));
    assert!(session_has_content(&session));
}

#[test]
fn unchanged_session_snapshot_keeps_revision_and_updated_at() {
    let mut app = test_app();
    let first = app.session_snapshot();
    let second = app.session_snapshot();

    assert_eq!(second.meta.revision, first.meta.revision);
    assert_eq!(second.updated_at, first.updated_at);
}

#[test]
fn session_snapshot_advances_revision_for_semantic_changes() {
    let mut app = test_app();
    let mut revision = app.session_snapshot().meta.revision;
    let mut assert_changed = |app: &mut App, change| {
        let next = app.session_snapshot().meta.revision;
        assert!(next > revision, "{change} did not advance revision");
        revision = next;
    };

    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(vec![Message::user(
        "history".into(),
    )])));
    assert_changed(&mut app, "history");

    app.shared_transcript = Some(Arc::new(ArcSwap::from_pointee(vec![
        TranscriptEntry::Message(Message::user("transcript".into())),
    ])));
    assert_changed(&mut app, "transcript");

    app.shared_tool_outputs = Some(Arc::new(Mutex::new(HashMap::from([(
        "tool".into(),
        ToolOutput::TodoList(Vec::new()),
    )]))));
    assert_changed(&mut app, "tool output");

    app.state.token_usage.input = 1;
    app.state.context_size = 2;
    assert_changed(&mut app, "usage");

    app.permissions
        .add_session_rule(n00n_config::PermissionRule {
            tool: n00n_config::ToolKey::native("bash"),
            scope: Some("cargo test".into()),
            effect: n00n_config::Effect::Allow,
        });
    assert_changed(&mut app, "permission");

    app.input_box.set_input("draft");
    assert_changed(&mut app, "draft");

    app.queue_and_notify(queued_msg("queued"));
    assert_changed(&mut app, "queue");

    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta {
            text: "child".into(),
        },
        "task1",
        Some("research"),
    ));
    assert_changed(&mut app, "subagent");

    app.state.session.title.push_str(" changed");
    assert_changed(&mut app, "title");
}

#[test]
fn save_session_syncs_ephemeral_content_into_meta() {
    let mut app = test_app();
    app.save_session();
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.save_session();
    assert!(session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.save_session();
    assert!(app.state.session.meta.input_draft.is_none());
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Tab)));
    app.save_session();
    assert_eq!(app.state.session.meta.mode, Some(StoredMode::Plan));
    assert!(session_has_content(&app.state.session));

    let mut queued = app_with_queued_message();
    queued.save_session();
    let session = &queued.state.session;
    assert!(session.messages.is_empty());
    assert!(session.meta.input_draft.is_none());
    assert_eq!(session.meta.mode, Some(StoredMode::Build));
    assert_eq!(session.meta.queued_messages, vec!["queued".to_string()]);
    assert!(session_has_content(session));
}

fn drain_writer(app: App, writer: Arc<StorageWriter>) {
    drop(app);
    Arc::try_unwrap(writer)
        .ok()
        .expect("app must hold the only other writer reference")
        .shutdown(WRITER_DRAIN_TIMEOUT)
        .unwrap();
}

#[test]
fn checkpoint_session_is_durable_before_returning() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let session_id = app.state.session.id;
    app.state
        .session
        .messages
        .push(Message::user("checkpoint".into()));

    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();

    let loaded = AppSession::load(session_id, &dir).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.messages).unwrap(),
        serde_json::to_value(&app.state.session.messages).unwrap()
    );
    drain_writer(app, writer);
}

#[test]
fn save_session_captures_plugin_state_snapshot() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let handle = host.event_handle().unwrap();
    let session_id = app.state.session.id;
    let mut snapshot = StoredSessionStateSnapshot::new(3);
    snapshot
        .set_plugin_state(
            "todo_write",
            1,
            StoredStateScope::Root,
            serde_json::json!({"todos": [{"content": "ship", "status": "in_progress"}]}),
        )
        .unwrap();
    app.state.session.meta.state_snapshot = Some(snapshot);
    app.lua_event_handle = Some(handle);
    app.hydrate_plugin_state();
    app.state
        .session
        .messages
        .push(Message::user("persist state".into()));

    app.save_session();
    assert!(app.state.session.meta.state_snapshot.is_some());
    drain_writer(app, writer);

    let loaded = AppSession::load(session_id, &dir).unwrap();
    let payload = loaded
        .meta
        .state_snapshot
        .as_ref()
        .unwrap()
        .plugin_payload_for_apply("todo_write", 1, StoredStateScope::Root)
        .unwrap();
    assert_eq!(
        payload,
        Some(&serde_json::json!({"todos": [{"content": "ship", "status": "in_progress"}]}))
    );
}

#[test]
fn save_session_without_plugin_capture_keeps_live_state_snapshot() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    app.lua_event_handle = host.event_handle();
    app.state.session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(3));
    app.hydrate_plugin_state();
    app.state
        .session
        .messages
        .push(Message::user("persist without capture".into()));
    let session_id = app.state.session.id;

    app.save_session_without_plugin_state_capture();

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(3)
    );
    drain_writer(app, writer);
    assert_eq!(
        AppSession::load(session_id, &dir)
            .unwrap()
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(3)
    );
}

#[test]
fn save_session_does_not_overtake_pending_compaction_revision() {
    let (_tmp, _dir, writer, mut app) = tempdir_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    app.lua_event_handle = host.event_handle();
    app.state.session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(3));
    app.hydrate_plugin_state();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::AutoCompacting));
    let revision_before_save = app
        .state
        .session
        .meta
        .state_snapshot
        .as_ref()
        .and_then(StoredSessionStateSnapshot::state_revision);

    app.save_session();

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        revision_before_save
    );
    drain_writer(app, writer);
}

#[test]
fn apply_loaded_session_hydrates_plugin_state_snapshot() {
    let mut app = test_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let handle = host.event_handle().unwrap();
    app.lua_event_handle = Some(handle.clone());
    let mut loaded = AppSession::new("test-model", "/tmp/test");
    loaded.messages.push(Message::user("restore state".into()));
    let loaded_id = loaded.id;
    let mut snapshot = StoredSessionStateSnapshot::new(9);
    snapshot
        .set_plugin_state(
            "todo_write",
            1,
            StoredStateScope::Root,
            serde_json::json!({"todos": [{"content": "resume", "status": "pending"}]}),
        )
        .unwrap();
    loaded.meta.state_snapshot = Some(snapshot);
    let model = app.state.model.clone();

    app.apply_loaded_session(loaded, &model);

    let identity = SessionIdentity::root(SessionRef::from_id(loaded_id));
    let captured = handle.capture_state(&identity, 10).unwrap();
    assert_eq!(
        captured
            .plugin_payload_for_apply("todo_write", 1, StoredStateScope::Root)
            .unwrap(),
        Some(&serde_json::json!({"todos": [{"content": "resume", "status": "pending"}]}))
    );
}

#[test]
fn apply_loaded_session_uses_boundary_checkpoint_when_latest_snapshot_is_older() {
    let mut app = test_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let handle = host.event_handle().unwrap();
    app.lua_event_handle = Some(handle.clone());
    let mut loaded = AppSession::new("test-model", "/tmp/test");
    loaded.transcript = vec![TranscriptEntry::Compaction {
        entries: Vec::new(),
        generated_summary: None,
        state_revision: Some(7),
    }];
    let mut checkpoint = StoredSessionStateSnapshot::new(7);
    checkpoint
        .set_plugin_state(
            "todo_write",
            1,
            StoredStateScope::Root,
            serde_json::json!({"todos": [{"content": "boundary", "status": "pending"}]}),
        )
        .unwrap();
    loaded.meta.checkpoint_compaction_state(checkpoint).unwrap();
    loaded.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(6));
    let loaded_id = loaded.id;
    let model = app.state.model.clone();

    app.apply_loaded_session(loaded, &model);

    let identity = SessionIdentity::root(SessionRef::from_id(loaded_id));
    let captured = handle.capture_state(&identity, 8).unwrap();
    assert_eq!(
        captured
            .plugin_payload_for_apply("todo_write", 1, StoredStateScope::Root)
            .unwrap(),
        Some(&serde_json::json!({"todos": [{"content": "boundary", "status": "pending"}]}))
    );
}

#[test]
fn reload_persists_session_with_content_to_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.state
        .session
        .messages
        .push(Message::user("hello".into()));
    let actions = app.execute_command(cmd("/reload"));
    assert_eq!(app.exit_request, ExitRequest::Reload);
    assert!(actions.is_empty());
    let id = app.state.session.id;
    drain_writer(app, writer);

    assert_eq!(AppSession::load(id, &dir).unwrap().messages.len(), 1);
}

#[test]
fn rewind_snapshot_survives_crash_restore() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.state.session.messages = vec![
        Message::user("first".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "reply".into(),
            }],
            ..Default::default()
        },
        Message::user("second".into()),
    ];
    let id = app.state.session.id;
    let before = app.state.session.meta.revision;
    app.rewind_to(&crate::components::rewind_picker::RewindEntry {
        turn_index: 2,
        prompt_preview: "2: second".into(),
        prompt_text: "second".into(),
    });
    assert!(app.state.session.meta.revision > before);
    drain_writer(app, writer);

    let restored = AppSession::load(id, &dir).unwrap();
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(restored.meta.input_draft.as_deref(), Some("second"));
}

#[test]
fn loaded_metadata_consumption_survives_crash_restore() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mut seed = AppSession::new("test-model", "/tmp/test");
    seed.meta.input_draft = Some("draft before crash".into());
    seed.meta.queued_messages = vec!["queued before crash".into()];
    seed.messages.push(Message::user("history".into()));
    let id = seed.id;
    seed.save(&dir).unwrap();

    let (shared, _receiver) = shared_queue::queue();
    app.queue.set_shared(shared);
    let previous_revision = seed.meta.revision;
    app.apply_loaded_session(seed, &test_model());
    assert!(app.state.session.meta.revision > previous_revision);
    drain_writer(app, writer);

    let restored = AppSession::load(id, &dir).unwrap();
    assert_eq!(
        restored.meta.input_draft.as_deref(),
        Some("draft before crash")
    );
    assert_eq!(restored.meta.queued_messages, vec!["queued before crash"]);
}

#[test]
fn turn_error_preserves_queued_prompt_in_memory_and_after_restart() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let (shared, _receiver) = shared_queue::queue();
    app.queue.set_shared(shared);
    app.status = Status::Streaming;
    app.run_id = 1;

    assert!(matches!(
        app.submit_prompt(QueuedMessage {
            text: "queued through turn error".into(),
            images: Vec::new(),
            control: false,
        }),
        SubmitOutcome::Queued
    ));
    app.queue.set_focus();
    let Some((_, edited, _)) = app.queue.take_focused_for_edit() else {
        panic!("queued prompt must enter edit mode");
    };
    app.input_box.set_submission(Submission {
        text: edited.text,
        images: edited.images,
        control: edited.control,
    });
    assert!(app.queue.text_messages().is_empty());

    app.update(agent_msg(AgentEvent::Error {
        message: PROVIDER_FAILED_ERR.into(),
    }));

    assert_eq!(app.queue.text_messages(), vec!["queued through turn error"]);
    assert!(app.input_box.is_empty());
    app.save_session();
    let session_id = app.state.session.id;
    drain_writer(app, writer);

    let saved = AppSession::load(session_id, &dir).expect("saved session loads");
    assert_eq!(
        saved.meta.queued_messages,
        vec!["queued through turn error"]
    );
    assert_eq!(saved.meta.queued_submissions.len(), 1);

    let writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let mut restarted = build_app(dir, Arc::clone(&writer));
    let (shared, receiver) = shared_queue::queue();
    restarted.queue.set_shared(shared);
    restarted.apply_loaded_session(saved, &test_model());

    let Some(shared_queue::QueueItem::Message { input, .. }) = receiver.pop() else {
        panic!("queued prompt must be restored after turn error");
    };
    assert_eq!(input.message, "queued through turn error");
    drain_writer(restarted, writer);
}

#[test]
fn draw_failure_pending_submission_restores_fifo_images_and_control_after_restart() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let (shared, receiver) = shared_queue::queue();
    app.queue.set_shared(shared);

    app.input_box.set_input("first with image");
    with_image(&mut app);
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let Action::SendMessage(dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected paint-gated submission");
    };
    assert!(
        receiver.pop().is_none(),
        "paint gate must block provider dispatch"
    );

    assert!(matches!(
        app.submit_control_prompt(QueuedMessage {
            text: "second control in fifo".into(),
            images: Vec::new(),
            control: true,
        }),
        SubmitOutcome::Queued
    ));
    app.preserve_submission_for_shutdown(*dispatch);
    app.save_session();
    let session_id = app.state.session.id;
    drop(app);
    Arc::try_unwrap(writer)
        .ok()
        .expect("test owns the storage writer")
        .shutdown(WRITER_DRAIN_TIMEOUT)
        .unwrap();

    let writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let mut restarted = build_app(dir.clone(), Arc::clone(&writer));
    let (shared, receiver) = shared_queue::queue();
    restarted.queue.set_shared(shared);
    restarted.apply_loaded_session(
        AppSession::load(session_id, &dir).expect("saved session loads"),
        &test_model(),
    );

    let Some(shared_queue::QueueItem::Message {
        text,
        input,
        delivery,
        ..
    }) = receiver.pop()
    else {
        panic!("first pending message must be restored");
    };
    assert_eq!(text, "first with image");
    assert_eq!(input.images.len(), 1);
    assert_eq!(&*input.images[0].data, "dGVzdA==");
    assert_eq!(delivery, shared_queue::Delivery::TurnEnd);
    let ExtractedCommand::Interrupt(input, _) =
        InterruptSource::poll(&receiver, InterruptPoint::Safe)
            .expect("steering control must be restorable at a safe boundary")
    else {
        panic!("second queued message must restore as a steering interrupt");
    };
    assert_eq!(input.message, "second control in fifo");
    assert!(input.control);
    assert!(receiver.pop().is_none());
    assert!(InterruptSource::poll(&receiver, InterruptPoint::Safe).is_none());

    drop(restarted);
    Arc::try_unwrap(writer)
        .ok()
        .expect("test owns the restarted storage writer")
        .shutdown(WRITER_DRAIN_TIMEOUT)
        .unwrap();
}

#[test]
#[allow(deprecated)]
fn mcp_prompt_draw_failure_survives_restart_without_text_fallback() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mcp_reader = McpSnapshotReader::from_snapshot(McpSnapshot {
        prompts: vec![n00n_agent::McpPromptInfo {
            display_name: "myserver:review".into(),
            qualified_name: "myserver/review".into(),
            description: "Review a change".into(),
            arguments: vec![McpPromptArg {
                name: "diff".into(),
                description: "The diff".into(),
                required: true,
            }],
        }],
        ..Default::default()
    });
    app.command_palette = crate::components::command::CommandPalette::new(
        Arc::from([]),
        mcp_reader.clone(),
        LuaCommandReader::empty(),
    );
    let (shared, _receiver) = shared_queue::queue();
    app.queue.set_shared(shared);
    app.state.thinking = n00n_providers::ThinkingConfig::Effort(Effort::High);
    app.state.fast = true;
    app.state.workflow = true;
    let actions = app.execute_command(ParsedCommand {
        name: "/myserver:review".into(),
        args: "important diff".into(),
    });
    let Action::SendMessage(dispatch) = actions.into_iter().next().unwrap() else {
        panic!("expected MCP prompt dispatch");
    };
    assert_eq!(dispatch.input.message, "/myserver:review important diff");
    let prompt = dispatch
        .input
        .prompt
        .as_ref()
        .expect("MCP prompt reference");
    assert_eq!(prompt.qualified_name, "myserver/review");
    assert_eq!(
        prompt.arguments.get("diff").map(String::as_str),
        Some("important diff")
    );
    assert_eq!(
        dispatch.input.thinking,
        n00n_providers::ThinkingConfig::Effort(Effort::High)
    );
    assert!(dispatch.input.fast);
    assert!(dispatch.input.workflow);

    app.preserve_submission_for_shutdown(*dispatch);
    app.save_session();
    let session_id = app.state.session.id;
    drop(app);
    Arc::try_unwrap(writer)
        .ok()
        .expect("test owns the storage writer")
        .shutdown(WRITER_DRAIN_TIMEOUT)
        .unwrap();

    let writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let mut restarted = build_app_with_mcp(dir.clone(), Arc::clone(&writer), mcp_reader);
    let (shared, receiver) = shared_queue::queue();
    restarted.queue.set_shared(shared);
    restarted.apply_loaded_session(
        AppSession::load(session_id, &dir).expect("saved session loads"),
        &test_model(),
    );

    let Some(shared_queue::QueueItem::Message { input, .. }) = receiver.pop() else {
        panic!("MCP prompt must be restored");
    };
    let prompt = input
        .prompt
        .expect("restored MCP prompt must use get_prompt");
    assert_eq!(prompt.qualified_name, "myserver/review");
    assert_eq!(
        prompt.arguments.get("diff").map(String::as_str),
        Some("important diff")
    );
    assert_eq!(
        input.thinking,
        n00n_providers::ThinkingConfig::Effort(Effort::High)
    );
    assert!(input.fast);
    assert!(input.workflow);
    assert_eq!(input.message, "/myserver:review important diff");

    drop(restarted);
    Arc::try_unwrap(writer)
        .ok()
        .expect("test owns the restarted storage writer")
        .shutdown(WRITER_DRAIN_TIMEOUT)
        .unwrap();
}

#[test]
fn reload_leaves_empty_session_unpersisted_on_disk() {
    let (tmp, _dir, writer, mut app) = tempdir_app();
    app.execute_command(cmd("/reload"));
    drain_writer(app, writer);

    let sessions_dir = tmp.path().join(n00n_storage::sessions::SESSIONS_DIR);
    let entries = std::fs::read_dir(&sessions_dir).map_or(0, std::iter::Iterator::count);
    assert_eq!(entries, 0);
}

#[test]
fn yolo_toggle() {
    let mut app = test_app();
    assert!(!app.permissions.is_yolo());
    app.execute_command(cmd("/yolo"));
    assert!(app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("enabled"), "flash={flash:?}");
    app.execute_command(cmd("/yolo"));
    assert!(!app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("disabled"), "flash={flash:?}");
}

#[test]
fn usage_command_toggles_modal() {
    let mut app = test_app();
    assert!(!app.usage_modal.is_open());
    let open_actions = app.execute_command(cmd("/usage"));
    assert!(app.usage_modal.is_open());
    assert!(
        open_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "opening should request a quota refresh"
    );
    let close_actions = app.execute_command(cmd("/usage"));
    assert!(!app.usage_modal.is_open());
    assert!(
        !close_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "closing should not trigger a refresh"
    );
}

#[test]
fn ctrl_r_refreshes_usage_while_modal_open() {
    let mut app = test_app();
    app.execute_command(cmd("/usage"));
    assert!(app.usage_modal.is_open());

    let actions = app.update(Msg::Key(kb::REFRESH.to_key_event()));
    assert!(
        actions.iter().any(|a| matches!(a, Action::RefreshUsage)),
        "Ctrl+R should emit RefreshUsage"
    );
    assert!(app.usage_modal.is_open(), "modal should stay open");
}

#[test]
fn cd_command_behavior() {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/cd".into(),
        args: "/tmp".into(),
    });
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd /tmp"), "flash={flash:?}");
    // Use `canonicalize_clean` (resolves symlinks like the OS does) rather
    // than `absolute` which preserves symlinks. On macOS `/tmp` is a symlink
    // to `/private/tmp`; production `cmd_cd` reads back `current_dir()` which
    // returns the resolved form, so the test expectation must match.
    let resolved = n00n_storage::paths::canonicalize_clean(Path::new("/tmp"));
    assert_eq!(app.state.session.cwd, resolved.to_string_lossy());

    app.execute_command(ParsedCommand {
        name: "/cd".into(),
        args: "/nonexistent_path_12345".into(),
    });
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd: "), "error flash={flash:?}");
}

#[test]
fn typed_slash_command_executes() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/help");
    assert!(actions.is_empty());
    assert!(app.help_modal.is_open());
}

#[test]
fn slash_noncommand_sends_as_prompt() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/nonexistent");
    assert!(app.status_bar.flash_text().is_none());
    assert!(actions.iter().any(|a| matches!(a, Action::SendMessage(..))));
}

fn build_rewind_app() -> App {
    let mut app = test_app();

    app.state.session.messages = vec![
        Message::user("first prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "response 1".into(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
            ..Default::default()
        },
        Message::user("second prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response 2".into(),
            }],
            ..Default::default()
        },
        Message::user("third prompt".into()),
    ];
    app.state
        .session
        .tool_outputs
        .insert("tool-1".into(), ToolOutput::Plain("output".into()));
    app
}

#[test]
fn rewind_to_middle_truncates_and_populates_input() {
    let mut app = build_rewind_app();
    app.state.context_size = 100_000;
    let old_run_id = app.run_id;
    let entry = crate::components::rewind_picker::RewindEntry {
        turn_index: 2,
        prompt_preview: "2: second".into(),
        prompt_text: "second prompt".into(),
    };
    let actions = app.rewind_to(&entry);

    assert_eq!(app.state.session.messages.len(), 2);
    assert!(app.state.session.tool_outputs.contains_key("tool-1"));
    assert_eq!(app.input_box.buffer.value(), "second prompt");
    assert_eq!(app.run_id, old_run_id + 1);
    let expected_ctx = n00n_agent::agent::estimate_message_tokens(
        &app.state.session.messages,
        &app.state.model.id,
    );
    assert_eq!(app.state.context_size, expected_ctx);
    assert_eq!(app.chats[0].context_size, expected_ctx);

    let Action::LoadSession(ref loaded) = actions[0] else {
        panic!("expected LoadSession");
    };
    assert_eq!(loaded.messages.len(), 2);
    assert!(loaded.tool_outputs.contains_key("tool-1"));
}

#[test]
fn rewind_truncates_active_tail_inside_recursive_transcript() {
    let mut app = test_app();
    let assistant = |text: &str| Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        ..Default::default()
    };
    let summary = assistant("summary");
    let kept_reply = assistant("kept reply");
    let removed_reply = assistant("removed reply");
    app.state.session.messages = vec![
        Message::user("summary prompt".into()),
        summary.clone(),
        Message::user("keep".into()),
        kept_reply.clone(),
        Message::user("remove".into()),
        removed_reply.clone(),
    ];
    app.state.session.transcript = vec![
        TranscriptEntry::Compaction {
            entries: vec![TranscriptEntry::Compaction {
                entries: vec![TranscriptEntry::Message(Message::user("oldest".into()))],
                generated_summary: None,
                state_revision: None,
            }],
            generated_summary: Some(summary.clone()),
            state_revision: Some(7),
        },
        TranscriptEntry::GeneratedMessage(Message::user("summary prompt".into())),
        TranscriptEntry::GeneratedMessage(summary),
        TranscriptEntry::Message(Message::user("keep".into())),
        TranscriptEntry::Message(kept_reply),
        TranscriptEntry::Message(Message::user("remove".into())),
        TranscriptEntry::Message(removed_reply),
    ];
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let handle = host.event_handle().unwrap();
    app.lua_event_handle = Some(handle.clone());
    let mut checkpoint = StoredSessionStateSnapshot::new(7);
    checkpoint
        .set_plugin_state(
            "todo_write",
            1,
            StoredStateScope::Root,
            serde_json::json!({"value": "boundary"}),
        )
        .unwrap();
    app.state
        .session
        .meta
        .checkpoint_compaction_state(checkpoint)
        .unwrap();
    let mut future = StoredSessionStateSnapshot::new(9);
    future
        .set_plugin_state(
            "todo_write",
            1,
            StoredStateScope::Root,
            serde_json::json!({"value": "future"}),
        )
        .unwrap();
    app.state.session.meta.state_snapshot = Some(future);
    app.hydrate_plugin_state();
    let actions = app.rewind_to(&crate::components::rewind_picker::RewindEntry {
        turn_index: 4,
        prompt_preview: "remove".into(),
        prompt_text: "remove".into(),
    });

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(7)
    );
    let identity = SessionIdentity::root(SessionRef::from_id(app.state.session.id));
    let captured = handle.capture_state(&identity, 10).unwrap();
    assert_eq!(
        captured
            .plugin_payload_for_apply("todo_write", 1, StoredStateScope::Root)
            .unwrap(),
        Some(&serde_json::json!({"value": "boundary"}))
    );
    assert!(matches!(
        app.state.session.transcript.as_slice(),
        [TranscriptEntry::Compaction { entries, .. }, TranscriptEntry::GeneratedMessage(_), TranscriptEntry::GeneratedMessage(_), TranscriptEntry::Message(_), TranscriptEntry::Message(_)]
            if matches!(entries.as_slice(), [TranscriptEntry::Compaction { .. }])
    ));
    let Action::LoadSession(loaded) = &actions[0] else {
        panic!("expected loaded session action");
    };
    let mut restored = n00n_agent::agent::History::restored_with_transcript(
        loaded.messages.clone(),
        loaded.transcript.clone(),
    );
    restored.compact_boundary(
        Message::user("summary prompt".into()),
        assistant("new summary"),
        None,
    );
    let TranscriptEntry::Compaction { entries, .. } = &restored.transcript()[0] else {
        panic!("expected recursive compaction");
    };
    assert!(!entries.iter().any(|entry| {
        matches!(entry, TranscriptEntry::Message(message) if message.user_text().is_some_and(|text| text.contains("remove")))
    }));
}

#[test]
fn rewind_to_first_turn_clears_everything() {
    let mut app = build_rewind_app();
    app.state.context_size = 100_000;
    app.state.token_usage.input = 500;
    app.state.token_usage.output = 200;
    let entry = crate::components::rewind_picker::RewindEntry {
        turn_index: 0,
        prompt_preview: "1: first".into(),
        prompt_text: "first prompt".into(),
    };
    let actions = app.rewind_to(&entry);

    assert!(app.state.session.messages.is_empty());
    assert!(!app.state.session.tool_outputs.contains_key("tool-1"));
    assert_eq!(app.state.token_usage.input, 500);
    assert_eq!(app.state.token_usage.output, 200);
    assert_eq!(app.state.context_size, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert!(matches!(&actions[0], Action::LoadSession(_)));
}

#[test_case(Duration::ZERO,          true  ; "keeps_fresh_error")]
#[test_case(Duration::from_mins(1), false ; "clears_stale_error")]
fn tick_error_expiry(age: Duration, expect_error: bool) {
    let mut app = test_app();
    app.status = Status::Error {
        message: "fail".into(),
        since: Instant::now().checked_sub(age).unwrap(),
    };
    app.tick_error_expiry();
    assert_eq!(matches!(app.status, Status::Error { .. }), expect_error);
}

#[test]
fn retry_clears_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));
    assert_eq!(app.chats[0].in_progress_count(), 1);

    app.update(agent_msg(AgentEvent::Retry {
        attempt: 1,
        message: "overloaded".into(),
        delay_ms: 1000,
    }));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(app.retry_info.is_some());
}

#[test]
fn retry_clears_subagent_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::ToolPending {
            id: "st1".into(),
            name: "bash".into(),
        },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].in_progress_count(), 1);

    app.update(subagent_msg(
        AgentEvent::Retry {
            attempt: 1,
            message: "overloaded".into(),
            delay_ms: 1000,
        },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.retry_info.is_none());
}

fn auth_retry_enter(app: &mut App) -> Vec<Action> {
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn auth_retry_type_then_enter(app: &mut App) -> Vec<Action> {
    type_and_submit(app, "ignored")
}

#[test_case(auth_retry_enter          ; "bare_enter")]
#[test_case(auth_retry_type_then_enter ; "typed_text_then_enter")]
fn auth_retry_sends_empty_answer(submit: fn(&mut App) -> Vec<Action>) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(AgentEvent::AuthRequired));
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: None }
    ));

    let actions = submit(&mut app);
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "");
}

#[test]
fn typing_in_running_subagent_routes_prompt_to_that_agent() {
    let (mut app, _answer_rx, _main_rx) = app_with_subagent_tx("task1");
    let (prompt_tx, prompt_rx) = flume::unbounded();
    app.subagent_prompts.insert("task1".into(), prompt_tx);
    app.active_chat = 1;

    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    let prompt = prompt_rx.try_recv().unwrap();
    assert_eq!(prompt.text, "hi");
    assert_eq!(app.chats[1].last_message_text(), "hi");
    assert_eq!(app.chats[1].last_message_role(), Some(&DisplayRole::User));
}

#[test]
fn typing_in_read_only_subagent_flashes_explanation() {
    let mut app = app_with_active_subagent();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(app.status_bar.flash_text(), Some(STEERING_UNAVAILABLE_MSG));
}
fn app_with_subagent_tx(id: &str) -> (App, flume::Receiver<String>, flume::Receiver<String>) {
    let (sub_tx, sub_rx) = flume::unbounded();
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::TextDelta { text: "x".into() },
        subagent: Some(subagent_info_with_tx(id, "research", Some(sub_tx))),
        run_id: 1,
    })));
    (app, sub_rx, main_rx)
}

fn app_with_steerable_subagent(id: &str) -> (App, flume::Receiver<SubagentPrompt>) {
    let (prompt_tx, prompt_rx) = flume::bounded(2);
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::TextDelta { text: "x".into() },
        subagent: Some(subagent_info_with_channels(
            id,
            "task-group",
            "research",
            None,
            Some(prompt_tx),
        )),
        run_id: 1,
    })));
    app.active_chat = app.chat_index[id];
    (app, prompt_rx)
}

#[test]
fn concurrent_children_with_one_parent_have_independent_chats_cancels_and_persistence() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    for (session_id, name) in [("child-a", "research"), ("child-b", "build")] {
        app.update(Msg::Agent(Box::new(Envelope {
            event: AgentEvent::TextDelta { text: name.into() },
            subagent: Some(subagent_info_with_channels(
                session_id,
                "workflow-group",
                name,
                None,
                None,
            )),
            run_id: 1,
        })));
    }

    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.chat_index.get("child-a"), Some(&1));
    assert_eq!(app.chat_index.get("child-b"), Some(&2));
    app.save_session();
    assert_eq!(
        app.state
            .session
            .meta
            .subagents
            .iter()
            .map(|agent| agent.tool_use_id.as_str())
            .collect::<Vec<_>>(),
        ["child-a", "child-b"]
    );

    app.active_chat = app.chat_index["child-b"];
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(matches!(
        &actions[..],
        [Action::CancelSubagent { tool_use_id }] if tool_use_id == "child-b"
    ));
    assert!(!app.chats[app.chat_index["child-a"]].is_finished());
    assert!(app.chats[app.chat_index["child-b"]].is_finished());
}

#[test]
fn expanded_subagent_chat_sends_typed_steering() {
    let (mut app, prompt_rx) = app_with_steerable_subagent("child-a");
    for c in "expand".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    let prompt = prompt_rx.try_recv().unwrap();
    assert_eq!(prompt.text, "expand");
    assert_eq!(app.chats[1].last_message_role(), Some(&DisplayRole::User));
    assert_eq!(app.chats[1].last_message_text(), "expand");
}

#[test]
fn esc_in_subagent_cancels_then_returns_to_main() {
    let (mut app, _prompt_rx) = app_with_steerable_subagent("child-a");

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[..], [Action::CancelSubagent { .. }]));
    assert!(app.chats[1].is_finished());

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert_eq!(app.active_chat, 0);
}

#[test]
fn expanded_subagent_chat_sends_pasted_steering() {
    let (mut app, prompt_rx) = app_with_steerable_subagent("child-a");
    app.update(Msg::Paste("pasted steering".into()));
    assert_eq!(app.input_box.buffer.value(), "pasted steering");
    app.update(Msg::Key(key(KeyCode::Enter)));

    let prompt = prompt_rx.try_recv().unwrap();
    assert_eq!(prompt.text, "pasted steering");
    assert_eq!(app.chats[1].last_message_text(), "pasted steering");
}

#[test]
fn left_in_expanded_subagent_chat_returns_to_main_without_cancelling() {
    let (mut app, prompt_rx) = app_with_steerable_subagent("child-a");

    let actions = app.update(Msg::Key(key(KeyCode::Left)));

    assert!(actions.is_empty());
    assert_eq!(app.active_chat, 0);
    assert!(!app.chats[1].is_finished());
    assert!(prompt_rx.try_recv().is_err());
}

#[test]
fn subagent_history_error_marks_chat_failed_and_closes_steering() {
    let (mut app, prompt_rx) = app_with_steerable_subagent("child-a");
    app.update(agent_msg(AgentEvent::SubagentHistory {
        tool_use_id: "child-a".into(),
        messages: vec![],
        is_error: true,
    }));

    assert!(app.chats[1].is_failed());
    assert_eq!(app.chats[1].last_message_text(), ERROR_TEXT);
    assert!(!app.subagent_prompts.contains_key("child-a"));
    assert!(prompt_rx.try_recv().is_err());
}

#[test]
fn auth_required_in_subagent_shows_in_both_chats() {
    let mut app = app_with_subagent_id("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    assert_eq!(app.chats[1].last_message_text(), AUTH_EXPIRED_MSG);
    assert_eq!(app.chats[0].last_message_text(), AUTH_EXPIRED_MSG);
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: Some(ref id) } if id == "sub1"
    ));
}

#[test]
fn auth_retry_in_subagent_routes_to_subagent_channel() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(sub_rx.try_recv().unwrap(), "");
    assert!(main_rx.try_recv().is_err());
}

#[test]
fn cancel_clears_subagent_auth_retry() {
    let (mut app, sub_rx, _main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    cancel_app(&mut app);

    assert_eq!(app.pending_input, PendingInput::None);
    assert!(sub_rx.try_recv().is_err());
}

#[test]
fn stale_auth_required_after_cancel_is_dropped() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 2;
    let count_before = app.chats[0].message_count();
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::AuthRequired,
        subagent: None,
        run_id: 1,
    })));
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(app.chats[0].message_count(), count_before);
}

#[test]
fn send_to_agent_unknown_subagent_does_not_fallback_to_main() {
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);

    app.pending_input = PendingInput::AuthRetry {
        subagent_id: Some("nonexistent".into()),
    };
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(main_rx.try_recv().is_err());
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test_case(42, false ; "restores_scroll_position")]
#[test_case(0,  true  ; "restores_auto_scroll")]
fn search_escape_restores_scroll(scroll_top: u16, auto_scroll: bool) {
    let mut app = test_app();
    app.active_chat().restore_scroll(scroll_top, auto_scroll);

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.search_modal.is_open());
    assert_eq!(app.active_chat().scroll_top(), scroll_top);
    assert_eq!(app.active_chat().auto_scroll(), auto_scroll);
}

#[test]
fn mcp_command_opens_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/mcp"));
    assert!(app.mcp_picker.is_open());
}

#[test]
#[allow(deprecated)]
fn mcp_toggle_dispatches_action() {
    let mut app = test_app();
    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "test-srv".into(),
                transport_kind: "stdio",
                tool_count: 2,
                prompt_count: 0,
                status: McpServerStatus::Running,
                config_path: PathBuf::from("/tmp/config.toml"),
                url: None,
            }],
            prompts: vec![],
            pids: Vec::new(),
            generation: 0,
        }),
        McpConfigErrors::new(PathBuf::new()),
    );
    app.execute_command(cmd("/mcp"));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        &actions[0],
        Action::ToggleMcp(name, false) if name == "test-srv"
    ));
}

#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    ""
    ; "consumed_by_plan_form"
)]
#[test_case(
    |app: &mut App| { open_tasks_picker(app); },
    ""
    ; "routed_to_open_picker"
)]
#[test_case(
    |app: &mut App| { app.update(Msg::Key(kb::SEARCH.to_key_event())); },
    ""
    ; "routed_to_search_modal"
)]
#[test_case(
    |_: &mut App| {},
    "pasted"
    ; "falls_through_to_input"
)]
fn paste_routing(setup: fn(&mut App), expected_input: &str) {
    let mut app = test_app();
    setup(&mut app);
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), expected_input);
}

#[test_case(PlanState::None,                                       true  ; "no_plan")]
#[test_case(PlanState::Drafting(PathBuf::from("/tmp/plan.md")),     false ; "plan_drafting")]
#[test_case(PlanState::Ready(PathBuf::from("/tmp/plan.md")),       false ; "plan_ready")]
fn open_editor(plan: PlanState, expect_flash: bool) {
    let mut app = test_app();
    let plan_path = plan.path().map(Path::to_path_buf);
    app.state.plan = plan;
    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    if expect_flash {
        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_NO_PLAN);
        assert!(!app.plan_form.is_visible());
    } else {
        let expected = plan_path.unwrap();
        assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == &expected));
        assert!(!app.plan_form.is_visible());
    }
}

#[test]
fn edit_input_opens_editor_for_input() {
    let mut app = test_app();
    app.input_box.buffer.insert_text("hello");
    let actions = app.update(Msg::Key(kb::EDIT_INPUT.to_key_event()));
    assert!(matches!(&actions[..], [Action::EditInputInEditor]));
}

#[test]
fn alt_o_alias_opens_editor_for_input() {
    let mut app = test_app();
    app.input_box.buffer.insert_text("hello");
    let alt_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);
    let actions = app.update(Msg::Key(alt_o));
    assert!(matches!(&actions[..], [Action::EditInputInEditor]));
}

#[test]
fn btw_empty_flashes_error() {
    let mut app = test_app();
    let actions = app.execute_command(ParsedCommand {
        name: "/btw".into(),
        args: String::new(),
    });
    assert!(actions.is_empty());
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        "Usage: /btw <question>"
    );
}

#[test]
fn btw_with_question_returns_action() {
    let mut app = test_app();
    let actions = app.execute_command(ParsedCommand {
        name: "/btw".into(),
        args: "what is rust?".into(),
    });
    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}
#[test]
fn btw_modal_key_routing_and_animation() {
    let mut app = test_app();
    let (_tx, rx) = flume::bounded(1);
    app.btw_modal.open("test", rx);

    assert!(app.btw_modal.is_animating());

    let actions = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(actions.is_empty());
    assert!(app.btw_modal.is_open());
    assert_eq!(app.input_box.buffer.value(), "");

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.btw_modal.is_open());
    assert!(!app.btw_modal.is_animating());
}

#[test]
fn overlay_zone_click_gating() {
    let mut app = test_app();
    let msg = Rect::new(0, 0, 80, 15);
    let overlay = Rect::new(10, 3, 60, 10);
    set_zone(&mut app, SelectionZone::Messages, msg);
    set_zone(&mut app, SelectionZone::Overlay, overlay);
    app.help_modal.toggle();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 1));
    assert!(app.selection_state.is_none());

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 20, 5));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Overlay);
}

fn streaming_app_with_history() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let history = vec![
        Message::user("hello".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            ..Default::default()
        },
    ];
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(history)));
    app
}

#[test_case(
    AgentEvent::Done { usage: TokenUsage::default(), num_turns: 1, stop_reason: None, fusion: None } ; "stale_done_saves_session"
)]
#[test_case(
    AgentEvent::Error { message: "timeout".into() } ; "stale_error_saves_session"
)]
fn stale_terminal_event_after_cancel_saves_session(event: AgentEvent) {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);
    assert_ne!(app.run_id, old_run_id);
    assert!(app.state.session.messages.is_empty());

    app.update(agent_msg_with_run_id(event, old_run_id));
    assert_eq!(app.state.session.messages.len(), 2);
}

#[test]
fn stale_non_terminal_event_does_not_save_session() {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);

    app.update(agent_msg_with_run_id(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::user(String::new()),
            usage: TokenUsage::default(),
            model: "mock".into(),
            context_size: None,
        })),
        old_run_id,
    ));
    assert!(app.state.session.messages.is_empty());
}

#[test]
fn error_event_matching_run_id_saves_session() {
    let mut app = streaming_app_with_history();
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    assert_eq!(app.state.session.messages.len(), 2);
}

// --- Plan form integration tests ---
fn done_event() -> Msg {
    agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
        fusion: None,
    })
}

fn implement_msg(parallel: bool) -> String {
    if parallel {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`. {IMPLEMENT_PARALLEL_HINT}")
    } else {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`.")
    }
}
fn plan_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    app
}

#[test_case(Mode::Plan,  true  ; "plan_mode_tooldone_opens_form")]
#[test_case(Mode::Build, false ; "build_mode_tooldone_no_form")]
fn tool_done_write_opens_plan_form(mode: Mode, expect_form: bool) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = mode;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to /tmp/plans/test.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("/tmp/plans/test.md".into()),
    }))));
    assert_eq!(app.plan_form.is_visible(), expect_form);
    if expect_form {
        assert!(app.state.plan.is_ready());
    }
}

#[test]
fn done_event_does_not_open_plan_form() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("test-plan.md"));
    app.update(done_event());
    assert!(!app.plan_form.is_visible());
}

#[test]
fn re_edit_keeps_plan_form_visible() {
    let mut app = plan_app();
    assert!(app.state.plan.is_ready());
    assert!(app.plan_form.is_visible());

    // Agent edits the plan again (second write to same path) — idempotent, stays Ready
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 50 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    assert!(matches!(app.state.plan, PlanState::Ready(_)));
    assert!(app.plan_form.is_visible());
}

#[test_case(1, Mode::Build, true,  false ; "clear_and_implement")]
#[test_case(2, Mode::Build, false, true  ; "implement_keeps_context")]
fn plan_form_menu_options(
    downs: usize,
    expected_mode: Mode,
    has_new_session: bool,
    has_send_message: bool,
) {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    for _ in 0..downs {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.state.mode, expected_mode);
    if has_new_session {
        assert_eq!(app.state.plan, PlanState::None);
    } else {
        assert!(matches!(app.state.plan, PlanState::Ready(_)));
    }
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::NewSession { .. })),
        has_new_session
    );
    let expected_msg = implement_msg(PlanForm::new().parallel());
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.input.message == expected_msg)),
        has_send_message
    );
    if !has_send_message {
        let pending = app
            .pending_plan_submit
            .as_ref()
            .expect("pending plan submit");
        assert_eq!(pending.message.text, expected_msg);
    }
}

#[test]
fn clear_and_implement_defers_submission_until_new_session() {
    let mut app = plan_app();
    assert!(app.state.plan.is_ready());
    let old_session_id = app.state.session.id;

    let actions = app.implement_plan(true);

    assert!(matches!(&actions[..], [Action::NewSession { .. }]));
    assert_ne!(app.state.session.id, old_session_id);
    let pending = app
        .pending_plan_submit
        .as_ref()
        .expect("pending plan submit");
    assert!(pending.plan.is_some());
    assert_eq!(
        pending.message.text,
        implement_msg(PlanForm::new().parallel())
    );
    assert!(app.queue.is_empty());
    assert_eq!(app.main_chat().message_count(), 0);
}

#[test]
fn plan_form_implement_toggled_parallel() {
    let mut app = plan_app();
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let expected_msg = implement_msg(!PlanForm::new().parallel());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.input.message == expected_msg))
    );
}

#[test]
fn plan_form_open_editor() {
    let mut app = plan_app();

    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    assert!(app.plan_form.is_visible());
    assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == Path::new("test-plan.md")));
}

fn rewrite_plan(app: &mut App) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 99 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
}

fn dismiss_plan_esc(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Esc)));
}

#[test]
fn rewrite_does_not_reopen_after_dismiss() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());
    dismiss_plan_esc(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());

    rewrite_plan(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());
}

#[test]
fn plan_toggle_toggles_plan_form_in_plan_mode() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(app.plan_form.is_visible());
}

#[test]
fn plan_toggle_noop_when_plan_not_ready() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());
}

#[test]
fn override_shadows_builtin_ctrl_when_no_overlay_open() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::HELP.code,
        modifiers: kb::HELP.modifiers,
        desc: "plugin help override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 1,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.keymap_reader = reader;
    assert!(!app.help_modal.is_open());

    let actions = app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(actions.is_empty());
    assert!(
        !app.help_modal.is_open(),
        "override must consume the key before the built-in HELP handler runs"
    );
}
#[test]
fn override_shadows_quit_builtin() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::QUIT.code,
        modifiers: kb::QUIT.modifiers,
        desc: "plugin quit override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 3,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.status = Status::Idle;
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(actions.is_empty());
    assert_eq!(
        app.exit_request,
        ExitRequest::None,
        "override must consume Ctrl+C before the built-in quit handler runs"
    );
}

#[test]
fn override_matches_shifted_key_in_both_terminal_shapes() {
    // Lua spells shift two ways: `<C-T>` stores Char('T')+CONTROL (shift in
    // the codepoint), `<C-S-t>` stores Char('t')+CONTROL|SHIFT. Kitty
    // REPORT_ALTERNATE_KEYS also delivers the folded shape, so dispatch
    // must normalize both sides or shifted Lua binds go dead there.
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::Char('T'),
        modifiers: KeyModifiers::CONTROL,
        desc: "plugin shifted override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 7,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.keymap_reader = reader;

    // Folded shape: shifted codepoint, SHIFT flag cleared.
    let actions = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('T'),
        KeyModifiers::CONTROL,
    )));
    assert!(actions.is_empty());
    assert!(
        probe.try_recv().is_some(),
        "folded <C-T> event must reach the Lua keybind callback"
    );

    // Flagged shape: lowercase + explicit SHIFT, same binding.
    let actions = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )));
    assert!(actions.is_empty());
    assert!(
        probe.try_recv().is_some(),
        "flagged Ctrl+Shift+T event must reach the Lua keybind callback"
    );
}

#[test]
fn override_shadows_tab_mode_toggle() {
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::Tab,
        modifiers: KeyModifiers::NONE,
        desc: "plugin tab override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 4,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    let initial_mode = app.state.mode;
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(key(KeyCode::Tab)));

    assert!(actions.is_empty());
    assert_eq!(
        app.state.mode, initial_mode,
        "override must consume Tab before the built-in mode toggle runs"
    );
}

#[test]
fn override_shadows_esc_builtin() {
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        desc: "plugin esc override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 5,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(actions.is_empty());
    assert!(
        app.last_esc.is_none(),
        "override must consume Esc before the built-in esc handler runs"
    );
}

#[cfg(unix)]
#[test]
fn override_does_not_shadow_suspend() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::SUSPEND.code,
        modifiers: kb::SUSPEND.modifiers,
        desc: "plugin suspend override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 6,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(kb::SUSPEND.to_key_event()));

    assert!(
        actions.iter().any(|a| matches!(a, Action::Suspend)),
        "suspend is non-remappable: override must not shadow Ctrl+Z"
    );
}

#[test]
fn builtin_runs_when_no_override() {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(app.help_modal.is_open());
}
#[test]
fn overlay_wins_over_override_when_plan_form_open() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::PLAN_TOGGLE.code,
        modifiers: kb::PLAN_TOGGLE.modifiers,
        desc: "plugin plan override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 2,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = plan_app();
    app.keymap_reader = reader;
    assert!(app.plan_form.is_visible());
    assert!(app.lua_event_handle.is_none());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());
}

#[test]
fn streaming_cancel_wins_over_quit_override() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::QUIT.code,
        modifiers: kb::QUIT.modifiers,
        desc: "plugin quit override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 7,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.status = Status::Streaming;
    app.run_id = 1;
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Ctrl+C is overridden"
    );
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test]
fn dead_host_override_falls_back_to_builtin() {
    let entry = n00n_lua::KeymapEntry {
        key: kb::HELP.code,
        modifiers: kb::HELP.modifiers,
        desc: "plugin help override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 8,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    app.lua_event_handle = Some(n00n_lua::EventHandle::disconnected_for_test());
    app.keymap_reader = reader;
    assert!(!app.help_modal.is_open());

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(
        app.help_modal.is_open(),
        "dead lua host must fall back to the built-in HELP handler"
    );
}

#[test]
fn streaming_cancel_wins_over_esc_override() {
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        desc: "plugin esc override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 9,
    };
    let reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let mut app = test_app();
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.status = Status::Streaming;
    app.run_id = 1;
    app.last_esc = Some(Instant::now());
    app.keymap_reader = reader;

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Esc is overridden"
    );
    assert_eq!(app.status, Status::Idle);
}

/// Build a test app whose effective keymap merges `keymap.toml` contents
/// over the compiled-in defaults. Fixtures should be clean: parse and
/// merge warnings fail the test.
fn app_with_keymap(source: &str) -> App {
    let (user, parse_warnings) = crate::keymap::file::parse(source, Path::new("test.toml"));
    assert!(parse_warnings.is_empty(), "{parse_warnings:?}");
    let (effective, merge_warnings) = EffectiveKeymap::build(&user);
    assert!(merge_warnings.is_empty(), "{merge_warnings:?}");
    let mut app = test_app();
    app.effective_keymap = Arc::new(effective);
    app
}

#[test]
fn user_keymap_rebound_key_dispatches_action() {
    let mut app = app_with_keymap("[general]\nhelp = \"f2\"");
    assert!(!app.help_modal.is_open());

    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);

    assert!(app.help_modal.is_open());
}

#[test]
fn user_keymap_replaced_key_no_longer_dispatches() {
    let mut app = app_with_keymap("[general]\nhelp = \"f2\"");

    press(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);

    assert!(
        !app.help_modal.is_open(),
        "ctrl-h must be dead once help is rebound to f2"
    );
}

#[test]
fn user_keymap_empty_list_unbinds() {
    let mut app = app_with_keymap("[general]\ntasks = []");

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    assert!(
        !app.task_picker.is_open(),
        "ctrl-t must be dead once tasks is unbound"
    );
}

#[test]
fn lua_override_still_shadows_user_keymap() {
    let mut app = app_with_keymap("[general]\nhelp = \"f2\"");
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::F(2),
        modifiers: KeyModifiers::NONE,
        desc: "plugin f2 override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 10,
    };
    app.keymap_reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);

    let actions = app.update(Msg::Key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)));

    assert!(actions.is_empty());
    assert!(
        !app.help_modal.is_open(),
        "a Lua bind must shadow the user keymap, not just the defaults"
    );
}

#[test]
fn streaming_stop_key_follows_user_rebind() {
    // `quit` moved to ctrl-x: during streaming the new key keeps the
    // bypass-Lua privilege that protects the interrupt, and a Lua bind on
    // it must not swallow the cancel.
    let mut app = app_with_keymap("[general]\nquit = \"ctrl-x\"");
    let entry = n00n_lua::KeymapEntry {
        key: KeyCode::Char('x'),
        modifiers: KeyModifiers::CONTROL,
        desc: "plugin ctrl-x override".into(),
        plugin: std::sync::Arc::from("test-plugin"),
        id: 11,
    };
    app.keymap_reader = n00n_lua::test_support::keymap_reader_with(vec![entry]);
    let (handle, _probe) = n00n_lua::test_support::probed_event_handle();
    app.lua_event_handle = Some(handle);
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
    )));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "a rebound quit key must still cancel the stream over a Lua bind"
    );
}

#[cfg(unix)]
#[test]
fn suspend_stays_reserved_with_user_keymap() {
    // `suspend` and `ctrl-z` are rejected at parse time, so the file can
    // never shadow the suspend check in handle_key.
    let (user, warnings) = crate::keymap::file::parse(
        "[general]\nsuspend = \"ctrl-x\"\nquit = \"ctrl-z\"",
        Path::new("test.toml"),
    );
    assert_eq!(warnings.len(), 2);
    let (effective, _) = EffectiveKeymap::build(&user);
    let mut app = test_app();
    app.effective_keymap = Arc::new(effective);

    let actions = press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    assert!(
        actions.is_empty(),
        "suspend was rejected, ctrl-x is unbound"
    );

    let actions = press(&mut app, kb::SUSPEND.code, kb::SUSPEND.modifiers);
    assert!(
        actions.iter().any(|a| matches!(a, Action::Suspend)),
        "ctrl-z must still suspend"
    );
}

#[test]
fn reset_session_closes_plan_form() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.reset_session();
    assert!(!app.plan_form.is_visible());
}

#[test]
fn ctrl_c_closes_overlay_instead_of_quitting() {
    let mut app = test_app();
    app.help_modal.toggle();
    assert!(app.help_modal.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.help_modal.is_open());
    assert!(actions.is_empty());
}

#[test]
fn bash_prefix_overrides_mode() {
    let mut app = test_app();

    app.input_box.set_input("! ls");
    assert_eq!(&*app.mode_label().0, "[BASH]");

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(
        app.state.mode,
        Mode::Build,
        "tab must not toggle while bash prefix present"
    );

    app.input_box.set_input("ls");
    assert_eq!(&*app.mode_label().0, "[BUILD]");
}

#[test]
fn thinking_toggle_cycles_off_adaptive() {
    let mut app = test_app();
    assert_eq!(app.state.thinking, ThinkingConfig::Off);

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
}

#[test]
fn thinking_explicit_args() {
    let mut app = test_app();

    app.execute_command(ParsedCommand {
        name: "/thinking".into(),
        args: "8192".into(),
    });
    assert_eq!(app.state.thinking, ThinkingConfig::Budget(8192));

    app.execute_command(ParsedCommand {
        name: "/thinking".into(),
        args: "high".into(),
    });
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::High));
}

#[test]
fn thinking_unsupported_model_flashes_error() {
    let mut app = test_app();
    app.state.model.supports_thinking_override = Some(false);

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
    assert!(app.status_bar.flash_text().is_some());
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Action> {
    app.handle_key(KeyEvent::new(code, modifiers))
}

#[test]
fn alt_t_cycles_thinking() {
    let mut app = test_app();
    assert_eq!(app.state.thinking, ThinkingConfig::Off);

    press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);

    press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::Minimal));
}

#[test]
fn ctrl_shift_t_still_cycles_thinking() {
    let mut app = test_app();
    press(
        &mut app,
        KeyCode::Char('t'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);
}

#[test]
fn alt_i_toggles_transcript_details() {
    let mut app = test_app();
    press(&mut app, KeyCode::Char('i'), KeyModifiers::ALT);
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Transcript details hidden")
    );
}

#[test]
fn thinking_change_persists_model_memory() {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/thinking".into(),
        args: "high".into(),
    });
    let raw = std::fs::read_to_string(app.storage.path().join("model-thinking")).unwrap();
    assert!(raw.contains("anthropic/test-model"), "memory file: {raw}");
    assert!(raw.contains("high"), "memory file: {raw}");
}

#[test]
fn update_model_applies_remembered_thinking() {
    let mut app = test_app();
    let mut model = test_model();
    model.id = "remembered-model".into();
    model.supports_thinking_override = Some(true);
    n00n_providers::model_registry::set_thinking_and_persist(
        model.spec(),
        n00n_storage::sessions::StoredThinking::Effort {
            level: n00n_storage::sessions::Effort::XHigh,
        },
        &app.storage,
    );

    app.update_model(&model);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::XHigh));
}

#[test]
fn same_spec_update_keeps_session_thinking() {
    let mut app = test_app();
    n00n_providers::model_registry::set_thinking_and_persist(
        app.state.model.spec(),
        n00n_storage::sessions::StoredThinking::Effort {
            level: n00n_storage::sessions::Effort::Max,
        },
        &app.storage,
    );
    app.state.thinking = ThinkingConfig::Effort(Effort::Low);

    let mut same_spec = test_model();
    same_spec.context_window = 999_999;
    app.update_model(&same_spec);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::Low));
}

#[test]
fn update_model_without_memory_keeps_thinking() {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/thinking".into(),
        args: "low".into(),
    });

    let model = n00n_providers::Model::from_spec("anthropic/claude-opus-4-8").unwrap();
    app.update_model(&model);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort(Effort::Low));
}

#[test]
fn thinking_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("test-model", "/tmp/test");
    session.meta.thinking = Some(StoredThinking::Budget { tokens: 4096 });

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert_eq!(state.thinking, ThinkingConfig::Budget(4096));
}

fn set_opus_model(app: &mut App) {
    app.state.model = n00n_providers::Model::from_spec("anthropic/claude-opus-4-8").unwrap();
}

#[test]
fn fast_toggle_on_off_on_opus() {
    let mut app = test_app();
    set_opus_model(&mut app);
    assert!(!app.state.fast);
    app.execute_command(cmd("/fast"));
    assert!(app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_ON_MSG));

    app.execute_command(cmd("/fast"));
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test]
fn workflow_toggle_flows_into_agent_input() {
    let mut app = test_app();
    let msg = QueuedMessage {
        text: "hi".into(),
        images: Vec::new(),
        control: false,
    };
    assert!(!app.build_agent_input(&msg).workflow);

    app.execute_command(cmd("/workflow"));
    assert!(app.build_agent_input(&msg).workflow);
    assert_eq!(app.status_bar.flash_text(), Some(WORKFLOW_ON_MSG));

    app.execute_command(cmd("/workflow"));
    assert!(!app.build_agent_input(&msg).workflow);
    assert_eq!(app.status_bar.flash_text(), Some(WORKFLOW_OFF_MSG));
}

/// Workflow sessions have synthetic ids that no `ToolDone` matches, so
/// `SubagentHistory` is what finishes their chat.
#[test]
fn subagent_history_finishes_workflow_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        "session-abc",
        Some("researcher"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert!(!app.chats[1].is_finished());

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            tool_use_id: "session-abc".into(),
            messages: vec![],
            is_error: false,
        },
        1,
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case("anthropic/claude-sonnet-4-5" ; "non_opus_anthropic")]
#[test_case("openai/gpt-5.5" ; "non_anthropic")]
fn fast_flashes_error_on_ineligible_model(spec: &str) {
    let mut app = test_app();
    app.state.model = n00n_providers::Model::from_spec(spec).unwrap();

    app.execute_command(cmd("/fast"));
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_UNSUPPORTED_MSG));
}

#[test]
fn fast_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("anthropic/claude-opus-4-8", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert!(state.fast);
}

#[test]
fn fast_normalized_off_when_restored_onto_ineligible_model() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    // Saved as fast=true, but sonnet cannot do fast mode, so restoring must drop
    // it to false or the UI would show a phantom [fast] badge.
    let mut session = AppSession::new("anthropic/claude-sonnet-4-5", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert!(!state.fast);
}

#[test]
fn update_model_to_ineligible_resets_fast() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.state.fast = true;

    let sonnet = n00n_providers::Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
    app.state.update_model(&sonnet);
    assert!(!app.state.fast);
}

#[test]
fn agent_error_creates_synthetic_tool_done_with_message() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        tool: "bash".into(),
        summary: "echo hello".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    assert_eq!(app.main_chat().in_progress_count(), 1);

    let error_msg = "Provider is overloaded";
    app.update(agent_msg(AgentEvent::Error {
        message: error_msg.into(),
    }));

    assert_eq!(app.main_chat().in_progress_count(), 0);
    let text = app.main_chat().last_message_text();
    assert!(
        text.contains(error_msg),
        "tool output should contain error: {text}"
    );
}

#[test]
fn error_event_adds_copyable_message_to_main_chat() {
    let mut app = test_app();
    app.run_id = 1;
    app.status = Status::Streaming;

    let error_msg = "Provider is overloaded";
    app.update(agent_msg(AgentEvent::Error {
        message: error_msg.into(),
    }));

    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::Error)
    );
    assert!(app.main_chat().last_message_text().contains(error_msg));
}

#[test]
fn ctrl_c_denies_permission_prompt() {
    let mut app = test_app();
    app.permission_prompt.open(
        n00n_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );
    assert!(app.permission_prompt.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.permission_prompt.is_open());
    assert!(actions.is_empty());
}

const TEST_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 40,
};
const SPLIT_EXTENT: u16 = 8;

fn open_split_window(app: &mut App, dir: n00n_lua::Split) {
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    let config = n00n_lua::FloatConfig {
        width: n00n_lua::Dimension::Abs(SPLIT_EXTENT),
        height: n00n_lua::Dimension::Abs(SPLIT_EXTENT),
        border: n00n_lua::Border::None,
        split: dir,
        ..n00n_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<n00n_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<n00n_lua::WinCommand>(8);
    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
}

#[test]
fn focused_lua_window_receives_app_key_input() {
    let mut app = test_app();
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    let (event_tx, event_rx) = flume::bounded(8);
    let (_cmd_tx, cmd_rx) = flume::bounded(8);
    app.float_mgr.open(
        buf,
        n00n_lua::FloatConfig::default(),
        true,
        event_tx,
        cmd_rx,
    );

    let actions = app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

    assert!(actions.is_empty());
    assert!(matches!(
        event_rx.recv_timeout(Duration::from_secs(1)),
        Ok(n00n_lua::WinEvent::Key { key }) if key == "enter"
    ));
}

#[test]
fn lua_panel_click_is_consumed_before_underlying_chat_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, TEST_AREA);
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.append(n00n_agent::SnapshotLine { spans: vec![] });
    let config = n00n_lua::FloatConfig {
        height: n00n_lua::Dimension::Abs(3),
        border: n00n_lua::Border::Rounded,
        split: n00n_lua::Split::Panel,
        ..n00n_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded(8);
    let (_cmd_tx, cmd_rx) = flume::bounded(8);
    app.float_mgr.open(buf, config, false, event_tx, cmd_rx);
    let backend = ratatui::backend::TestBackend::new(80, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 34));
    assert!(app.selection_state.is_none());
}

#[test]
fn below_split_reserves_bottom_and_suppresses_input() {
    let mut app = test_app();
    let (msg_before, _b, _s, input_before, splits_before) = app.layout_geometry(TEST_AREA);
    assert!(
        splits_before.rect(n00n_lua::Split::Below).is_none(),
        "no split open yet"
    );
    assert!(input_before.height > 0, "input box visible before split");

    open_split_window(&mut app, n00n_lua::Split::Below);
    let (msg_after, _bottom, _s, input_after, splits_after) = app.layout_geometry(TEST_AREA);

    let band = splits_after
        .rect(n00n_lua::Split::Below)
        .expect("below split should reserve a bottom band");
    assert_eq!(
        band.height, SPLIT_EXTENT,
        "below band reserves the requested rows",
    );
    assert!(
        msg_after.height < msg_before.height,
        "chat must shrink to make room for the below split",
    );
    assert_eq!(
        input_after.height, 0,
        "input box is suppressed under a below split"
    );
}
/// `carve` already tests the per-direction geometry; this pins the app wiring:
/// a split shrinks the chat while the full-width status bar stays put. Below is
/// tested separately since it also hides the input box.
#[test_case(n00n_lua::Split::Above ; "above")]
#[test_case(n00n_lua::Split::Left ; "left")]
#[test_case(n00n_lua::Split::Right ; "right")]
fn non_below_split_reserves_band_and_keeps_status_full_width(dir: n00n_lua::Split) {
    let mut app = test_app();
    let (msg_before, _b, _s, _i, _sp) = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, dir);
    let (msg_after, _bottom, status_after, _input, splits) = app.layout_geometry(TEST_AREA);

    assert!(splits.rect(dir).is_some(), "split must reserve a band");
    assert!(
        msg_after.area() < msg_before.area(),
        "chat must shrink to make room for the split",
    );
    assert_eq!(
        status_after.width, TEST_AREA.width,
        "status bar stays full width regardless of the split",
    );
}

#[test]
fn closing_split_restores_layout() {
    let mut app = test_app();
    let before = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, n00n_lua::Split::Below);
    app.float_mgr.close_all();

    let after = app.layout_geometry(TEST_AREA);
    assert_eq!(after, before, "closing the split restores the layout");
}

#[test]
fn permission_prompt_takes_bottom_precedence_over_below_split() {
    let mut app = test_app();
    open_split_window(&mut app, n00n_lua::Split::Below);
    open_split_window(&mut app, n00n_lua::Split::Left);
    open_split_window(&mut app, n00n_lua::Split::Above);
    app.permission_prompt.open(
        n00n_config::ToolKey::native("bash"),
        vec!["ls".into()],
        None,
    );

    let (_msg, _bottom, _status, _input, splits) = app.layout_geometry(TEST_AREA);
    assert!(
        splits.rect(n00n_lua::Split::Below).is_none(),
        "below split must yield the bottom area to an open permission prompt",
    );
    assert!(
        splits.rect(n00n_lua::Split::Left).is_some(),
        "the prompt must leave a left split untouched",
    );
    assert!(
        splits.rect(n00n_lua::Split::Above).is_some(),
        "the prompt must leave an above split untouched",
    );
}

fn app_with_active_subagent() -> App {
    let mut app = app_with_subagent();
    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);
    app
}

#[test]
fn esc_in_subagent_cancels_subagent() {
    let mut app = app_with_active_subagent();
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == "task1"
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), CANCELLED_TEXT);
}

#[test]
fn esc_in_main_chat_with_active_subagent_no_cancel() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert!(!matches!(&actions[0], Action::CancelSubagent { .. }));
}

#[test]
fn cancel_subagent_removes_answer_sender() {
    let (mut app, _sub_rx, _main_rx) = app_with_subagent_tx("task1");
    assert!(!app.subagent_answers.is_empty());
    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.subagent_answers.contains_key("task1"));
}
#[test]
fn multiple_subagents_cancel_one_other_unaffected() {
    let mut app = app_with_subagent_id("task1");
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "y".into() },
        "task2",
        Some("build"),
    ));
    assert_eq!(app.chats.len(), 3);

    app.active_chat = app.chat_index["task2"];
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == "task2"
    ));
    let task1_idx = app.chat_index["task1"];
    assert!(!app.chats[task1_idx].is_finished());
    assert!(app.chats[app.active_chat].is_finished());
}

#[test]
fn esc_in_finished_subagent_returns_to_main() {
    let mut app = app_with_active_subagent();
    finish_subagent_task(&mut app, false);
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert_eq!(app.active_chat, 0);
}

#[test]
fn subagent_cancel_then_navigate_back_main_unaffected() {
    let mut app = app_with_active_subagent();
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.chats[1].is_finished());

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);
    assert_eq!(app.status, Status::Streaming);
    assert!(!app.chats[0].is_finished());
}

const COMPLETION_BURST: usize = 50;
const RETAINED_SUBAGENTS: usize = 2;
const RETAINED_TOOL_OUTPUTS: usize = 2;
const GROWN_SUBAGENTS: usize = 6;

fn subagent_tool_use_id(index: usize) -> String {
    format!("task-{index:03}")
}

fn nested_tool_use_id(index: usize) -> String {
    format!("task-{index:03}-inner")
}

fn tool_use_turn(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: serde_json::json!({}),
        }],
        ..Default::default()
    }
}

/// One assistant turn per subagent, so the session records the ids in the
/// order they ran. Retention ranks by that order.
fn push_subagent_turns(app: &mut App, count: usize) {
    push_subagent_turns_from(app, 0, count);
}

fn push_subagent_turns_from(app: &mut App, start: usize, count: usize) {
    for index in start..start + count {
        let id = subagent_tool_use_id(index);
        let nested = nested_tool_use_id(index);
        app.state.session.messages.push(tool_use_turn(&id, "task"));
        app.state.session.subagent_messages.insert(
            id.clone(),
            vec![
                Message::user(format!("{id} history")),
                tool_use_turn(&nested, "bash"),
            ],
        );
        app.state
            .session
            .tool_outputs
            .insert(id.clone(), ToolOutput::Plain(format!("{id} output").into()));
        app.state.session.tool_outputs.insert(
            nested.clone(),
            ToolOutput::Plain(format!("{nested} output").into()),
        );
    }
}

#[test]
fn burst_of_completions_coalesces_into_one_save() {
    let mut app = test_app();
    app.state
        .session
        .messages
        .push(Message::user("work".into()));
    app.save_session();
    let baseline = app.session_saves;

    for index in 0..COMPLETION_BURST {
        let id = subagent_tool_use_id(index);
        app.state
            .session
            .subagent_messages
            .insert(id.clone(), vec![Message::user(format!("{id} history"))]);
        app.save_session_coalesced();
    }

    assert_eq!(
        app.session_saves, baseline,
        "a burst inside the coalescing window must not snapshot per event"
    );
    assert!(app.pending_save);
}

#[test]
fn active_save_does_not_wait_for_plugin_state_capture() {
    let mut app = test_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    app.lua_event_handle = host.event_handle();
    app.state.session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(3));
    app.hydrate_plugin_state();
    app.state
        .session
        .messages
        .push(Message::user("active work".into()));
    app.status = Status::Streaming;

    app.save_session();

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(3),
        "an active save must reuse the last checkpoint instead of entering the Lua drain barrier"
    );
}

#[test]
fn terminal_save_reuses_plugin_state_until_event_loop_capture_completes() {
    let mut app = test_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    app.lua_event_handle = host.event_handle();
    app.state.session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(3));
    app.hydrate_plugin_state();
    app.state
        .session
        .messages
        .push(Message::user("completed work".into()));
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
        fusion: None,
    }));

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(3),
        "a terminal save must not enter the Lua drain barrier on the UI thread"
    );
}

#[test]
fn completion_saves_do_not_capture_plugin_state() {
    let mut app = test_app();
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    app.lua_event_handle = host.event_handle();
    app.state.session.meta.state_snapshot = Some(StoredSessionStateSnapshot::new(3));
    app.hydrate_plugin_state();
    app.state
        .session
        .messages
        .push(Message::user("parallel work".into()));

    app.save_session_coalesced();
    app.pending_save = true;
    app.last_save_flush = None;
    app.tick_pending_save();

    assert_eq!(
        app.state
            .session
            .meta
            .state_snapshot
            .as_ref()
            .and_then(StoredSessionStateSnapshot::state_revision),
        Some(3),
        "completion saves must not wait for sibling tools or question windows to drain"
    );
}

#[test]
fn deferred_save_lands_once_the_window_elapses() {
    let mut app = test_app();
    app.state
        .session
        .messages
        .push(Message::user("work".into()));
    app.save_session();
    for index in 0..COMPLETION_BURST {
        let id = subagent_tool_use_id(index);
        app.state
            .session
            .subagent_messages
            .insert(id.clone(), vec![Message::user(format!("{id} history"))]);
        app.save_session_coalesced();
    }
    let deferred = app.session_saves;

    app.last_save_flush = None;
    app.tick_pending_save();

    assert_eq!(app.session_saves, deferred + 1);
    assert!(!app.pending_save);
}

#[test]
fn coalesced_burst_still_persists_every_completion() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let session_id = app.state.session.id;
    app.state
        .session
        .messages
        .push(Message::user("work".into()));
    app.save_session();
    for index in 0..COMPLETION_BURST {
        let id = subagent_tool_use_id(index);
        app.state
            .session
            .subagent_messages
            .insert(id.clone(), vec![Message::user(format!("{id} history"))]);
        app.save_session_coalesced();
    }
    assert!(app.session_saves < COMPLETION_BURST);

    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();

    let loaded = AppSession::load(session_id, &dir).unwrap();
    assert_eq!(loaded.subagent_messages.len(), COMPLETION_BURST);
    drain_writer(app, writer);
}

#[test]
fn retention_budget_bounds_live_session_and_keeps_the_log_complete() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let session_id = app.state.session.id;
    app.retention_budget = RetentionBudget {
        tool_outputs: RETAINED_TOOL_OUTPUTS,
        subagent_histories: RETAINED_SUBAGENTS,
    };
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);

    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();

    assert_eq!(
        app.state.session.subagent_messages.len(),
        RETAINED_SUBAGENTS
    );
    assert_eq!(app.state.session.tool_outputs.len(), RETAINED_TOOL_OUTPUTS);
    let loaded = AppSession::load(session_id, &dir).unwrap();
    assert_eq!(loaded.subagent_messages.len(), GROWN_SUBAGENTS);
    assert_eq!(loaded.tool_outputs.len(), GROWN_SUBAGENTS * 2);
    drain_writer(app, writer);
}

#[test]
fn restarted_session_evicts_records_that_were_already_durable() {
    let (tmp, dir, writer, mut app) = tempdir_app();
    let session_id = app.state.session.id;
    let budget = RetentionBudget {
        tool_outputs: RETAINED_TOOL_OUTPUTS,
        subagent_histories: RETAINED_SUBAGENTS,
    };
    app.retention_budget = budget;
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);
    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();
    drain_writer(app, writer);

    let loaded =
        AppSession::load_with_retention(session_id, &dir, budget, message_tool_use_ids).unwrap();
    let restarted_writer = Arc::new(StorageWriter::new(dir.clone()).unwrap());
    let mut restarted = build_app_with_session(
        dir,
        Arc::clone(&restarted_writer),
        McpSnapshotReader::empty(),
        loaded,
    );
    restarted.retention_budget = budget;
    push_subagent_turns_from(&mut restarted, GROWN_SUBAGENTS, 1);

    restarted.save_session();

    assert_eq!(
        restarted.state.session.subagent_messages.len(),
        RETAINED_SUBAGENTS
    );
    assert_eq!(
        restarted.state.session.tool_outputs.len(),
        RETAINED_TOOL_OUTPUTS
    );
    drain_writer(restarted, restarted_writer);
    drop(tmp);
}

/// A rewind rebuilds subagent tabs from the live session, so an evicted history
/// and the tool outputs it renders have to come back off the log.
#[test]
fn evicted_subagent_history_loads_back_from_the_log() {
    let (_tmp, _dir, writer, mut app) = tempdir_app();
    app.retention_budget = RetentionBudget {
        tool_outputs: RETAINED_TOOL_OUTPUTS,
        subagent_histories: RETAINED_SUBAGENTS,
    };
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);
    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();
    let evicted = subagent_tool_use_id(0);
    let nested = nested_tool_use_id(0);
    assert!(
        app.state
            .session
            .evicted_subagent_messages()
            .contains(&evicted)
    );
    assert!(!app.state.session.tool_outputs.contains_key(&nested));

    let source = app.subagent_display_source(&evicted).unwrap();

    assert_eq!(source.messages.len(), 2);
    assert!(source.tool_outputs.contains_key(&nested));
    drain_writer(app, writer);
}

/// A rewind redraws the main transcript from the live session, so outputs the
/// budget evicted have to come back off the log or the turns render empty.
#[test]
fn rewound_transcript_reloads_evicted_tool_outputs() {
    let (_tmp, _dir, writer, mut app) = tempdir_app();
    app.retention_budget = RetentionBudget {
        tool_outputs: RETAINED_TOOL_OUTPUTS,
        subagent_histories: RETAINED_SUBAGENTS,
    };
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);
    app.checkpoint_session(WRITER_DRAIN_TIMEOUT).unwrap();
    let evicted = subagent_tool_use_id(0);
    assert!(!app.state.session.tool_outputs.contains_key(&evicted));

    app.restore_display();
    let referenced = app
        .state
        .session
        .displayed_tool_use_ids(&message_tool_use_ids);
    let rendered = app.display_tool_outputs(referenced.into_iter());

    assert!(
        rendered.contains_key(&evicted),
        "a rewind has to render the evicted output, not a blank result"
    );
    assert!(
        !app.state.session.tool_outputs.contains_key(&evicted),
        "reading an output back for one frame must not undo the eviction"
    );
    drain_writer(app, writer);
}

/// The common path renders straight off the live map, so nothing is cloned and
/// the log is never touched.
#[test]
fn display_tool_outputs_borrows_when_nothing_is_evicted() {
    let (_tmp, _dir, writer, mut app) = tempdir_app();
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);
    let referenced = app
        .state
        .session
        .displayed_tool_use_ids(&message_tool_use_ids);

    let rendered = app.display_tool_outputs(referenced.into_iter());

    assert!(matches!(rendered, Cow::Borrowed(_)));
    drain_writer(app, writer);
}

#[test]
fn retention_never_evicts_records_the_writer_has_not_persisted() {
    let mut app = test_app();
    app.retention_budget = RetentionBudget {
        tool_outputs: RETAINED_TOOL_OUTPUTS,
        subagent_histories: RETAINED_SUBAGENTS,
    };
    push_subagent_turns(&mut app, GROWN_SUBAGENTS);

    let mut eviction =
        app.state
            .session
            .retention_eviction_candidates(app.retention_budget, |message| {
                message
                    .tool_uses()
                    .map(|(id, _, _)| id.to_owned())
                    .collect()
            });
    assert!(!eviction.is_empty());
    app.storage_writer
        .retain_durable(app.state.session.id, &mut eviction);

    assert!(
        eviction.is_empty(),
        "nothing is durable before the first write, so nothing may be evicted"
    );
}
