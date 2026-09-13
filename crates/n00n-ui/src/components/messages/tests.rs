use super::*;
use crate::components::keybindings::key;
use crate::components::scrollbar::SCROLLBAR_THUMB;
use crate::selection::{Selection, SelectionZone};
use n00n_agent::tools::{BASH_TOOL_NAME, GREP_TOOL_NAME, WRITE_TOOL_NAME};
use n00n_agent::{
    GrepFileEntry, GrepMatchGroup, SnapshotLine, SnapshotSpan, SpanStyle, ToolInput, ToolOutput,
};
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui_image::picker::Picker;
use std::sync::Arc;
use test_case::test_case;

fn test_panel() -> MessagesPanel {
    test_panel_with_config(UiConfig::default())
}

fn test_panel_with_config(ui_config: UiConfig) -> MessagesPanel {
    MessagesPanel::new(ui_config, Arc::new(Picker::halfblocks()))
}

#[test]
fn reduced_motion_config_disables_streaming_reveal() {
    let mut panel = test_panel_with_config(UiConfig {
        reduced_motion: true,
        ..UiConfig::default()
    });

    panel.text_delta("a response long enough to animate with the default typewriter speed");

    assert!(!panel.is_working());
    assert_eq!(
        panel.streaming_text,
        "a response long enough to animate with the default typewriter speed"
    );
}
#[test]
fn highlight_result_drain_is_limited_per_frame() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "ready".into()));
    assert!(!panel.is_animating());
    for id in 0..=MAX_HIGHLIGHT_RESULTS_PER_FRAME as u64 {
        panel
            .hl_worker
            .enqueue_result_for_test(crate::render_worker::RenderResult {
                id,
                lines: Vec::new(),
            });
    }

    panel.drain_highlights();

    assert_eq!(panel.hl_worker.pending_results_for_test(), 1);
    assert!(panel.is_animating());
}

fn snap_line(text: &str) -> SnapshotLine {
    SnapshotLine {
        spans: vec![SnapshotSpan {
            text: text.into(),
            style: SpanStyle::Default,
        }],
    }
}

fn start(id: &str, tool: &str) -> ToolStartEvent {
    ToolStartEvent {
        id: id.into(),
        tool: tool.into(),
        summary: id.into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }
}

fn panel_with_tools(ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = test_panel();
    for &(id, tool) in ids {
        panel.tool_start(start(id, tool));
    }
    panel
}

fn done(id: &str) -> ToolDoneEvent {
    ToolDoneEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    }
}

fn finish_with_live_buf(
    panel: &mut MessagesPanel,
    id: &str,
    text: &str,
    is_error: bool,
) -> Arc<n00n_agent::SharedBuf> {
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line(text)]);
    panel.register_live_buf(id.into(), Arc::clone(&buf));
    let mut ev = start(id, BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_done(ToolDoneEvent {
        is_error,
        ..done(id)
    });
    buf
}

#[test_case(false, ToolStatus::Success ; "success_updates_start_to_success")]
#[test_case(true,  ToolStatus::Error   ; "error_updates_start_to_error")]
fn tool_done_updates_start_status(is_error: bool, expected: ToolStatus) {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "bash"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error,
        annotation: None,
        written_path: None,
    });

    assert_eq!(panel.messages.len(), 1);
    assert!(matches!(&panel.messages[0].role, DisplayRole::Tool(t) if t.status == expected));
    assert!(panel.messages[0].text.contains("output"));
}

#[test_case(
    WRITE_TOOL_NAME,
    ToolOutput::WriteCode { path: "src/main.rs".into(), byte_count: 42, lines: vec!["fn main() {}".into()] },
    Some("42 bytes")
    ; "write_bytes"
)]
#[test_case(
    "grep",
    grep_output(2),
    Some("2 matches in 2 files")
    ; "grep_files"
)]
fn tool_done_sets_annotation(tool: &'static str, output: ToolOutput, expected: Option<&str>) {
    let mut panel = test_panel();
    panel.tool_start(start("t1", tool));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: tool.into(),
        output,
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

#[test_case("line\n".repeat(200).as_str(), Some("2m timeout · 200 lines") ; "merges_start_and_output_annotations")]
#[test_case("ok",                           Some("2m timeout · 1 lines") ; "merges_start_and_short_output")]
fn tool_done_annotation_merge(output: &str, expected: Option<&str>) {
    let mut panel = test_panel();
    let mut event = start("t1", BASH_TOOL_NAME);
    event.annotation = Some("2m timeout".into());
    panel.tool_start(event);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(output.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

fn grep_output(n_files: usize) -> ToolOutput {
    ToolOutput::GrepResult {
        entries: (0..n_files)
            .map(|i| GrepFileEntry {
                path: format!("{i}.rs"),
                groups: vec![GrepMatchGroup::single(1, "")],
            })
            .collect(),
    }
}

#[test]
fn tool_done_grep_shows_matches() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", GREP_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    let text = &panel.messages[0].text;
    assert!(!text.contains('\n'), "grep body should not be in msg.text");
    assert!(panel.messages[0].tool_output.is_some());
}

#[test]
fn tool_start_flushes_streaming_text() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer("partial response");

    panel.tool_start(start("t1", "read"));

    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.messages[0].role, DisplayRole::Assistant);
    assert!(matches!(panel.messages[1].role, DisplayRole::Tool(_)));
}

#[test]
fn thinking_delta_separate_from_text() {
    let mut panel = test_panel();
    panel.thinking_delta("reasoning");
    assert_eq!(panel.streaming_thinking, "reasoning");
    assert!(panel.streaming_text.is_empty());

    panel.text_delta("output");
    assert!(panel.streaming_thinking.is_empty());
    assert_eq!(panel.streaming_text, "output");
    assert_eq!(panel.messages[0].role, DisplayRole::Thinking);
    assert_eq!(panel.messages[0].text, "reasoning");
}

#[test]
fn tool_boundaries_preserve_independently_expandable_thinking_rows() {
    let mut panel = test_panel();
    panel.thinking_delta("A");
    panel.tool_pending("tool1".into(), "bash");
    panel.thinking_delta("B");
    panel.tool_pending("tool2".into(), "bash");
    rebuild(&mut panel);

    let thinking: Vec<_> = panel
        .messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| matches!(msg.role, DisplayRole::Thinking))
        .collect();
    assert_eq!(thinking.len(), 2);
    assert_eq!(thinking[0].1.text, "A");
    assert_eq!(thinking[1].1.text, "B");
    assert!(thinking.iter().all(|(_, msg)| msg.thinking_collapsed));

    let first_idx = thinking[0].0;
    let second_idx = thinking[1].0;
    assert!(panel.try_toggle_cached_thinking(Some(first_idx), 80));
    assert!(!panel.messages[first_idx].thinking_collapsed);
    assert!(panel.messages[second_idx].thinking_collapsed);
    assert!(panel.try_toggle_cached_thinking(Some(second_idx), 80));
    assert!(!panel.messages[first_idx].thinking_collapsed);
    assert!(!panel.messages[second_idx].thinking_collapsed);
}

#[test]
fn transcript_details_toggle_only_changes_persisted_thinking() {
    let mut panel = test_panel();
    panel.thinking_delta("persisted");
    panel.flush();
    panel.thinking_delta("live");

    assert!(panel.toggle_transcript_details());
    assert!(!panel.messages[0].thinking_collapsed);
    assert_eq!(panel.streaming_thinking, "live");
    assert!(!panel.toggle_transcript_details());
    assert!(panel.messages[0].thinking_collapsed);
    assert_eq!(panel.streaming_thinking, "live");
}

#[test]
fn scroll_up_pins_viewport_during_streaming() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);

    panel.scroll(1);
    panel.scroll(1);
    render(&mut panel, 80, 10);
    let pinned = panel.scroll_top;

    panel.text_delta("b\nb\nb\n");
    render(&mut panel, 80, 10);

    assert!(!panel.auto_scroll);
    assert_eq!(panel.scroll_top, pinned);
}

fn render_sel(
    panel: &mut MessagesPanel,
    width: u16,
    height: u16,
    has_selection: bool,
) -> ratatui::Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            panel.view(f, f.area(), has_selection, false);
        })
        .unwrap();
    terminal
}

fn render(panel: &mut MessagesPanel, width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
    render_sel(panel, width, height, false)
}

fn rebuild(panel: &mut MessagesPanel) {
    render(panel, 80, 24);
}

fn render_working(
    panel: &mut MessagesPanel,
    width: u16,
    height: u16,
) -> ratatui::Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            panel.view(f, f.area(), false, true);
        })
        .unwrap();
    terminal
}

#[test]
fn working_state_does_not_overlay_thinking_status_on_chat() {
    let mut panel = test_panel();
    panel.text_delta("hello");
    let terminal = render_working(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("n00n"),
        "streamed response stays in chat: {text:?}"
    );
    assert!(
        !text.contains("thinking..."),
        "thinking status belongs in expandable chat lines: {text:?}"
    );
}

#[test]
fn ctrl_d_to_bottom_re_enables_auto_scroll() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let half = panel.half_page();
    panel.scroll(half);
    render(&mut panel, 80, 10);
    assert!(!panel.auto_scroll);

    panel.scroll(-half);
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
}

#[test]
fn jump_to_bottom_popup_appears_when_scrolled_up() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.jump_to_bottom_popup().is_none());

    panel.scroll(panel.half_page());
    let terminal = render(&mut panel, 80, 10);
    let popup = panel.jump_to_bottom_popup().unwrap();
    assert_eq!(popup.height, JUMP_TO_BOTTOM_POPUP_HEIGHT);
    let text = buffer_text(&terminal);
    assert!(text.contains(JUMP_TO_BOTTOM_TEXT));
    assert!(text.contains(key::CHAT_SCROLL_BOTTOM.label));
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((popup.x, popup.y)).unwrap().symbol(), "╭");
    assert_eq!(
        buffer.cell((popup.right() - 1, popup.y)).unwrap().symbol(),
        "╮"
    );
    assert_eq!(
        buffer.cell((popup.x, popup.bottom() - 1)).unwrap().symbol(),
        "╰"
    );
    assert_eq!(
        buffer
            .cell((popup.right() - 1, popup.bottom() - 1))
            .unwrap()
            .symbol(),
        "╯"
    );

    panel.jump_to_bottom();
    assert!(panel.auto_scroll);
    render(&mut panel, 80, 10);
    assert!(panel.jump_to_bottom_popup().is_none());
}

#[test]
fn unknown_tool_id_is_noop() {
    let mut panel = test_panel();
    panel.tool_output("ghost", "data");
    panel.tool_done(ToolDoneEvent {
        id: "orphan".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert!(panel.messages.is_empty());
}

#[test]
fn in_progress_tracking() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    assert_eq!(panel.in_progress_count(), 2);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 1);

    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "read".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 0);
}

fn has_scrollbar_thumb(terminal: &ratatui::Terminal<TestBackend>) -> bool {
    let buf = terminal.backend().buffer();
    (0..buf.area.height).any(|y| {
        buf.cell((buf.area.width - 1, y))
            .is_some_and(|c: &ratatui::buffer::Cell| c.symbol() == SCROLLBAR_THUMB)
    })
}

#[test_case(40, true  ; "rendered_when_content_overflows")]
#[test_case(1,  false ; "hidden_when_content_fits")]
fn scrollbar_visibility(line_count: usize, expected: bool) {
    let mut panel = test_panel();
    panel
        .streaming_text
        .set_buffer(&"line\n".repeat(line_count));
    let terminal = render(&mut panel, 80, 10);
    assert_eq!(has_scrollbar_thumb(&terminal), expected);
}

fn first_segment_text(panel: &MessagesPanel) -> String {
    panel.cache.segments()[0]
        .lines()
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect()
}

fn seg_text(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect()
}

fn truncation_source_line(panel: &MessagesPanel, tool_id: &str) -> usize {
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some(tool_id))
        .unwrap();
    (0..segment.lines().len())
        .find(|&line| segment.truncation_action(line).is_some())
        .unwrap()
}

fn section_truncation_source_line(
    panel: &MessagesPanel,
    tool_id: &str,
    section: SectionFlags,
) -> usize {
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some(tool_id))
        .unwrap();
    (0..segment.lines().len())
        .find(|&line| segment.truncation_action(line) == Some(section))
        .unwrap()
}

fn msg_status(panel: &MessagesPanel, tool_id: &str) -> ToolStatus {
    panel
        .messages
        .iter()
        .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        .map(|m| match &m.role {
            DisplayRole::Tool(t) => t.status,
            _ => unreachable!(),
        })
        .unwrap()
}

fn has_seg(panel: &MessagesPanel, tool_id: &str) -> bool {
    panel
        .cache
        .segments()
        .iter()
        .any(|s| s.tool_id.as_deref() == Some(tool_id))
}

#[test]
fn events_before_cache_built_render_correctly() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "bash")]);
    panel.tool_output("t1", "early output");
    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("result".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    assert!(seg_text(&panel, "t1").contains("early output"));
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Success);
    assert!(seg_text(&panel, "t2").contains("result"));
}

fn bash_code_start(panel: &mut MessagesPanel, id: &str, code: &str) {
    panel.tool_start(ToolStartEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        summary: code.into(),
        annotation: None,
        input: Some(ToolInput::Code {
            language: "bash".into(),
            code: code.into(),
        }),
        raw_input: None,
        output: None,
        render_header: None,
    });
}

#[test]
fn bash_live_output_with_code_input() {
    let mut panel = test_panel();
    bash_code_start(&mut panel, "t1", "echo hello");
    rebuild(&mut panel);

    panel.tool_output("t1", "streaming");
    assert!(seg_text(&panel, "t1").contains("streaming"));

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("done".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    let text = seg_text(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("done"));
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test_case(true  ; "after_cache_built")]
#[test_case(false ; "before_cache_built")]
fn cancel_in_progress_marks_pending_as_error(cache_built: bool) {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    if cache_built {
        rebuild(&mut panel);
    }

    panel.cancel_in_progress();

    assert_eq!(panel.in_progress_count(), 0);
    assert!(!panel.is_animating());
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Error);
}

#[test]
fn new_tool_after_in_place_update() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    rebuild(&mut panel);
    panel.tool_output("t1", "streaming data");

    panel.tool_start(start("t2", "read"));
    rebuild(&mut panel);

    assert!(seg_text(&panel, "t1").contains("streaming data"));
    assert!(has_seg(&panel, "t2"));
}

#[test]
fn tool_done_after_cancel_in_progress_does_not_underflow() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.cancel_in_progress();
    assert_eq!(panel.in_progress_count(), 0);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("late".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test]
fn selection_freezes_viewport_during_auto_scroll() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
    let scroll_before = panel.scroll_top;
    assert!(scroll_before > 0);

    panel.streaming_text.set_buffer(&"a\n".repeat(35));
    render_sel(&mut panel, 80, 10, true);
    assert_eq!(panel.scroll_top, scroll_before);
    assert!(panel.auto_scroll);

    render_sel(&mut panel, 80, 10, false);
    assert!(panel.scroll_top > scroll_before);
    assert!(panel.auto_scroll);
}

fn seg_search(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .search_text
        .clone()
}

#[test]
fn search_text_grep_result_includes_structured_output() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "grep"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "grep".into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("0.rs:") && text.contains("1.rs:"));
}

#[test]
fn search_text_diff_output_includes_hunks() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "edit"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "edit".into(),
        output: ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "old\n".into(),
            after: "new\n".into(),
            summary: "1 edit".into(),
            telemetry: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("- old") && text.contains("+ new"));
}

#[test]
fn search_text_bash_with_code_input() {
    let mut panel = test_panel();
    bash_code_start(&mut panel, "t1", "echo hello");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("hello".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("hello"));
}

#[test]
fn expanded_compaction_card_renders_stored_tool_output_once() {
    let mut tool = DisplayMessage::new(
        DisplayRole::Tool(Box::new(ToolRole {
            id: "tool-1".into(),
            status: ToolStatus::Success,
            name: Arc::from("bash"),
        })),
        "Run command".into(),
    );
    tool.tool_output = Some(Arc::new(ToolOutput::Plain("reviewer output body".into())));
    let compaction_id = "compaction:0";
    let mut panel = test_panel();
    panel.push(DisplayMessage::compaction(CompactionDisplay {
        id: compaction_id.into(),
        depth: 1,
        message_count: 1,
        summary: Some("summary".into()),
        entries: vec![tool],
    }));
    panel.expanded_compactions.insert(compaction_id.into());
    rebuild(&mut panel);

    let card = first_segment_text(&panel);
    assert!(card.contains("reviewer output body"), "card: {card}");
    assert_eq!(card.matches("reviewer output body").count(), 1);
}

#[test]
fn expanded_compaction_card_caps_stored_tool_output() {
    let mut tool = DisplayMessage::new(
        DisplayRole::Tool(Box::new(ToolRole {
            id: "tool-1".into(),
            status: ToolStatus::Success,
            name: Arc::from("bash"),
        })),
        "Run command".into(),
    );
    tool.tool_output = Some(Arc::new(ToolOutput::Plain(
        "line 0\nline 1\nline 2\nline 3\nline 4\nline 5".into(),
    )));
    let compaction_id = "compaction:0";
    let mut ui_config = UiConfig::default();
    ui_config.tool_output_lines.bash = 2;
    let mut panel = test_panel_with_config(ui_config);
    panel.push(DisplayMessage::compaction(CompactionDisplay {
        id: compaction_id.into(),
        depth: 1,
        message_count: 1,
        summary: Some("summary".into()),
        entries: vec![tool],
    }));
    panel.expanded_compactions.insert(compaction_id.into());
    rebuild(&mut panel);

    let card = first_segment_text(&panel);
    assert!(card.contains("line 0"), "card: {card}");
    assert!(card.contains("line 1"), "card: {card}");
    assert!(!card.contains("line 2"), "card: {card}");
    assert!(!card.contains("line 5"), "card: {card}");
}

#[test]
fn nested_compaction_cards_expand_collapse_at_narrow_width() {
    let inner = DisplayMessage::compaction(CompactionDisplay {
        id: "compaction:0.0".into(),
        depth: 2,
        message_count: 1,
        summary: Some("inner summary".into()),
        entries: vec![DisplayMessage::new(
            DisplayRole::User,
            "original request".into(),
        )],
    });
    let outer = DisplayMessage::compaction(CompactionDisplay {
        id: "compaction:0".into(),
        depth: 1,
        message_count: 2,
        summary: Some("outer summary with narrow wrapping".into()),
        entries: vec![
            inner,
            DisplayMessage::new(DisplayRole::Assistant, "inner summary".into()),
        ],
    });
    let mut panel = test_panel();
    panel.push(outer);
    let area = Rect::new(0, 0, 24, 20);

    let collapsed = buffer_text(&render(&mut panel, area.width, area.height));
    let collapsed_card = first_segment_text(&panel);
    assert!(collapsed_card.contains("depth 1"), "card: {collapsed_card}");
    assert!(
        collapsed_card.contains("2 messages"),
        "card: {collapsed_card}"
    );
    assert!(
        collapsed_card.contains("outer summary"),
        "card: {collapsed_card}"
    );
    assert!(
        !collapsed_card.contains("depth 2"),
        "card: {collapsed_card}"
    );
    assert!(
        collapsed.contains("wrapping"),
        "rendered buffer: {collapsed}"
    );

    let outer_row = panel.cache.segments()[0]
        .compaction_action_row("compaction:0", panel.viewport_width)
        .unwrap();
    assert!(panel.toggle_expansion_at(outer_row, area));
    let expanded = buffer_text(&render(&mut panel, area.width, area.height));
    let expanded_card = first_segment_text(&panel);
    assert!(expanded_card.contains("depth 2"), "card: {expanded_card}");
    assert!(
        !expanded_card.contains("original request"),
        "card: {expanded_card}"
    );
    assert!(expanded.contains("inner"), "rendered buffer: {expanded}");

    let inner_row = panel.cache.segments()[0]
        .compaction_action_row("compaction:0.0", panel.viewport_width)
        .unwrap();
    assert!(
        panel.toggle_expansion_at(inner_row, area),
        "inner_row={inner_row} scroll_top={} width={}",
        panel.scroll_top,
        panel.viewport_width
    );
    render(&mut panel, area.width, area.height);
    let nested_card = first_segment_text(&panel);
    assert!(
        nested_card.contains("original request"),
        "card: {nested_card}"
    );

    assert!(panel.toggle_expansion_at(outer_row, area));
    render(&mut panel, area.width, area.height);
    let recollapsed_card = first_segment_text(&panel);
    assert!(
        !recollapsed_card.contains("depth 2"),
        "card: {recollapsed_card}"
    );
}

#[test]
fn assistant_markdown_renders_copy_action_and_raw_payload() {
    let markdown = "# Result\n\n- one\n- two";
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::Assistant, markdown.into()));
    let terminal = render(&mut panel, 48, 10);
    let text = buffer_text(&terminal);

    assert!(text.contains("[copy]"), "rendered buffer: {text}");
    assert!(text.contains("Result"), "rendered buffer: {text}");
    assert_eq!(
        panel.copy_at(0, 47, Rect::new(0, 0, 48, 10)),
        Some((markdown.into(), "markdown")),
    );
}

#[test]
fn fenced_code_copy_action_returns_code_without_fence() {
    let markdown = "```rust\nfn main() {}\n```";
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::Assistant, markdown.into()));
    render(&mut panel, 48, 10);

    assert_eq!(
        panel.copy_at(0, 47, Rect::new(0, 0, 48, 10)),
        Some(("fn main() {}".into(), "code")),
    );
    assert_eq!(panel.copy_at(0, 20, Rect::new(0, 0, 48, 10)), None);
}

#[test]
fn user_message_renders_as_compact_card() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    let terminal = render(&mut panel, 32, 8);
    let text = buffer_text(&terminal);

    assert!(!text.contains('╭'), "rendered buffer: {text}");
    assert!(!text.contains('╰'), "rendered buffer: {text}");
    assert!(text.contains("│ You   hello"), "rendered buffer: {text}");
    assert_eq!(panel.total_lines(), 1);
}

#[test]
fn copy_hit_target_requires_exact_visible_label() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "answer".into()));
    render(&mut panel, 48, 8);
    let area = Rect::new(0, 0, 48, 8);

    assert_eq!(
        panel.copy_at(0, 42, area),
        Some(("answer".into(), "markdown"))
    );
    assert_eq!(panel.copy_at(0, 41, area), None);
}

#[test]
fn copy_label_does_not_replace_narrow_assistant_text() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "answer".into()));
    let area = Rect::new(0, 0, 5, 8);
    let text = buffer_text(&render(&mut panel, area.width, area.height));

    assert!(text.contains("ans"), "rendered buffer: {text}");
    assert!(text.contains("wer"), "rendered buffer: {text}");
    assert!(!text.contains("[copy]"), "rendered buffer: {text}");
    assert_eq!(panel.copy_at(0, 0, area), None);
}

#[test]
fn copy_label_does_not_replace_full_width_assistant_text() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "abcdefghijkl".into(),
    ));
    let text = buffer_text(&render(&mut panel, 12, 8));

    assert!(text.contains("abcdefghijk"), "rendered buffer: {text}");
    assert!(text.contains("l "), "rendered buffer: {text}");
    assert!(!text.contains("[copy]"), "rendered buffer: {text}");
}

#[test]
fn partially_scrolled_user_card_does_not_replace_content_with_top_border() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "first\nsecond\nthird".into(),
    ));
    render(&mut panel, 24, 2);
    panel.set_scroll_top(1);
    let text = buffer_text(&render(&mut panel, 24, 2));

    assert!(text.contains("│ second"), "rendered buffer: {text}");
    assert!(!text.contains('╭'), "rendered buffer: {text}");
}

#[test]
fn user_markdown_uses_card_content_width() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "---".into()));
    render(&mut panel, 12, 8);

    assert_eq!(panel.cache.get(0).unwrap().lines()[0].width(), 6);
}

#[test]
fn restored_timed_and_legacy_thinking_use_distinct_labels() {
    let mut timed = DisplayMessage::new(DisplayRole::Thinking, "timed".into());
    timed.thinking_collapsed = true;
    timed.annotation = Some("4s".into());
    let mut legacy = DisplayMessage::new(DisplayRole::Thinking, "legacy".into());
    legacy.thinking_collapsed = true;
    let mut panel = test_panel();
    panel.load_messages(vec![timed, legacy]);
    let text = buffer_text(&render(&mut panel, 80, 10));

    assert!(text.contains("Thought for 4s"), "timed disclosure: {text}");
    assert!(
        text.contains("Thinking · 1 lines"),
        "legacy disclosure: {text}"
    );
}

#[test]
fn search_text_includes_role_prefix() {
    let md = "# Heading\n\nSome **bold** text";
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, md.into()));
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "hmm".into()));
    rebuild(&mut panel);
    let texts = panel.segment_search_texts();
    assert_eq!(texts[0], "you> hello");
    assert_eq!(texts[2], format!("n00n> {md}"));
    assert_eq!(texts[4], "thinking> hmm");
}

#[test_case(&["short", &"x".repeat(200)], 80, 4 ; "long_line_wraps")]
#[test_case(&["", "a", ""],                 40, 3 ; "empty_lines_count_as_one")]
#[test_case(&[&"a".repeat(80)],              80, 1 ; "exactly_width_no_wrap")]
#[test_case(&[&"a".repeat(81)],              80, 2 ; "one_over_width_wraps")]
#[test_case(&["hello", "world"],              0, 2 ; "zero_width_returns_line_count")]
#[test_case(&["aaaa bbbb cccc dddd"],         10, 2 ; "word_boundary_wrap")]
#[test_case(&["aaaaaa bbbbbbbbb"],            10, 2 ; "word_straddles_boundary")]
fn wrapped_line_count_cases(input: &[&str], width: u16, expected: u16) {
    let lines: Vec<Line<'static>> = input
        .iter()
        .map(|s| Line::from(Span::raw(s.to_string())))
        .collect();
    assert_eq!(wrapped_line_count(&lines, width), expected);
}

#[test]
fn update_tool_model_sets_annotation() {
    let mut panel = panel_with_tools(&[("t1", "task"), ("t2", "bash")]);
    rebuild(&mut panel);

    panel.update_tool_model("t1", "anthropic/claude-sonnet-4-20250514");

    let msg = &panel.messages[0];
    assert_eq!(
        msg.annotation.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test]
fn scroll_clamps_to_max_scroll() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(&"a\n".repeat(15));
    render(&mut panel, 80, 10);
    let max = panel.max_scroll();

    panel.scroll(-3);
    assert_eq!(panel.scroll_top, max);
}

#[test_case("bash", 1, 1 ; "known_tool_creates_message")]
#[test_case("nonexistent_tool", 1, 1 ; "unknown_tool_accepted")]
fn tool_pending(tool: &str, expected_msgs: usize, expected_in_progress: usize) {
    let mut panel = test_panel();
    panel.tool_pending("t1".into(), tool);
    assert_eq!(panel.messages.len(), expected_msgs);
    assert_eq!(panel.in_progress_count(), expected_in_progress);
}

#[test]
fn tool_start_upgrades_pending_in_place() {
    let mut panel = test_panel();
    panel.tool_pending("t1".into(), "bash");
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);

    let mut event = start("t1", BASH_TOOL_NAME);
    event.annotation = Some("note".into());
    panel.tool_start(event);

    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);
    assert_eq!(panel.messages[0].text, "t1");
    assert_eq!(panel.messages[0].annotation.as_deref(), Some("note"));
}

#[test]
fn stream_reset_clears_streaming_and_fails_tools() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    panel.streaming_thinking.set_buffer("partial thinking");
    panel.streaming_text.set_buffer("partial text");
    rebuild(&mut panel);

    panel.stream_reset();

    assert!(panel.streaming_thinking.is_empty());
    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Error);
}

#[test]
fn failed_compaction_clears_the_partially_streamed_summary() {
    let mut panel = test_panel();
    panel.messages.push(DisplayMessage::compaction_pending());
    panel.streaming_thinking.set_buffer("partial thinking");
    panel.streaming_text.set_buffer("partial summary");
    panel.thinking_collapsed = true;

    assert!(panel.fail_pending_compaction("stream ended early"));

    assert!(panel.streaming_thinking.is_empty());
    assert!(panel.streaming_text.is_empty());
    assert!(!panel.thinking_collapsed);
    assert!(panel.thinking_started.is_none());
}

const N00N_PREFIX_LEN: u16 = 6;

fn make_sel(area: Rect, anchor: (u32, u16), cursor: (u32, u16)) -> Selection {
    let mut sel = Selection::start(
        area.y + u16::try_from(anchor.0).unwrap_or_else(|_| u16::MAX),
        anchor.1,
        area,
        SelectionZone::Messages,
        0,
    );
    sel.update(
        area.y + u16::try_from(cursor.0).unwrap_or_else(|_| u16::MAX),
        cursor.1,
        0,
    );
    sel
}

fn panel_with_msgs(texts: &[&str], width: u16, height: u16) -> MessagesPanel {
    let mut panel = test_panel();
    for &text in texts {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, width, height);
    panel
}

#[test]
fn extract_partial_column_selection() {
    let panel = panel_with_msgs(&["Hello world"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let world_start = N00N_PREFIX_LEN + u16::try_from("Hello ".len()).unwrap_or_else(|_| u16::MAX);
    let sel = make_sel(area, (0, world_start), (0, world_start + 4));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text, "world");
}

#[test]
fn extract_user_partial_selection_accounts_for_frame_inset() {
    const USER_PREFIX_LEN: u16 = 6;
    const FRAME_INSET: u16 = 2;
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "hello world".into()));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let text_start = FRAME_INSET + USER_PREFIX_LEN;
    let sel = make_sel(area, (0, text_start), (0, text_start + 4));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text, "hello");
}

#[test]
fn extract_skips_out_of_range_segments() {
    let panel = panel_with_msgs(&["seg0", "seg1", "seg2"], 80, 24);
    let heights = panel.segment_heights();
    let total: u16 = heights.iter().sum();
    let mid = total / 2;
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (u32::from(mid), 0), (u32::from(mid), 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("seg1"));
    assert!(!text.contains("seg0"));
    assert!(!text.contains("seg2"));
}

#[test]
fn extract_off_screen_rows_via_temp_buffer() {
    let mut panel = test_panel();
    let text = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    panel.push(DisplayMessage::new(DisplayRole::Assistant, text));
    render(&mut panel, 80, 5);

    let total: u16 = panel.segment_heights().iter().sum();
    assert!(total > 5, "content must exceed viewport");
    let sel_area = Rect::new(0, 0, 80, total);
    let sel = make_sel(sel_area, (1, 0), (u32::from(total - 1), 79));

    let extracted = panel.extract_selection_text(&sel, sel_area);
    assert!(!extracted.contains("line 0"), "first line excluded");
    assert!(extracted.contains("line 1") && extracted.contains("line 19"));
}

#[test]
fn extract_mixed_fully_enclosed_and_partial() {
    let panel = panel_with_msgs(&["full segment", "partial here"], 80, 24);
    let heights = panel.segment_heights();
    let area = Rect::new(0, 0, 80, 24);
    let seg1_start = heights[0] + heights[1];
    let sel = make_sel(area, (0, 0), (u32::from(seg1_start), N00N_PREFIX_LEN + 6));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("full segment"));
    assert!(text.contains("partial"));
}

#[test_case(&["line-0\nline-1\nline-2\nline-3"], "line-0", "line-3" ; "single_segment")]
#[test_case(&["seg-A-text", "seg-B-text"],      "seg-A-text", "seg-B-text" ; "across_segments")]
fn extract_partial_col_symmetric(msgs: &[&str], expect_start: &str, expect_end: &str) {
    let mut panel = test_panel();
    for &text in msgs {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let down = make_sel(area, (0, N00N_PREFIX_LEN), (u32::from(total - 1), 79));
    let up = make_sel(area, (u32::from(total - 1), 79), (0, N00N_PREFIX_LEN));
    let text_down = panel.extract_selection_text(&down, area);
    let text_up = panel.extract_selection_text(&up, area);
    assert!(text_down.contains(expect_start));
    assert!(text_down.contains(expect_end));
    assert_eq!(text_down, text_up, "direction should not affect result");
}

#[test_case("```\n{L}\n```", (0, 1)  ; "wrapped_code_block")]
#[test_case("short\n{L}",   (0, 0)  ; "wrapped_long_line")]
fn extract_wrapped_no_soft_breaks(template: &str, anchor: (u32, u16)) {
    let long = "x".repeat(200);
    let msg = template.replace("{L}", &long);
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(DisplayRole::Assistant, msg));
    render(&mut panel, 40, 30);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 40, 30);
    let sel = make_sel(area, anchor, (u32::from(total - 1), 39));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        text.contains(&long),
        "wrapped line must be copied without newlines: {text:?}"
    );
}

#[test]
fn extract_fully_selected_message_copies_raw_text() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "some **markdown** text".into(),
    ));
    render(&mut panel, 80, 24);

    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (0, 0), (u32::from(total - 1), 79));
    let text = panel.extract_selection_text(&sel, area);

    assert_eq!(text, "some **markdown** text");
}

#[test]
fn extract_fully_selected_tool_copies_raw_output() {
    let mut panel = test_panel();
    let table = "| a | b |\n|---|---|\n| 1 | 2 |";
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Markdown(table.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (0, 0), (u32::from(total - 1), 79));
    let text = panel.extract_selection_text(&sel, area);

    assert!(
        text.contains("| a | b |"),
        "expected raw markdown table, got: {text:?}"
    );
    assert!(
        !text.contains('─'),
        "copied text should not contain rendered table borders: {text:?}"
    );
}

#[test]
fn extract_partial_last_line_truncated() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "first\nABCDEFGHIJKLMNOP".into(),
    ));
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let last_row = u32::from(total - 1);
    let sel = make_sel(area, (0, 0), (last_row, 3));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text.lines().last().unwrap(), "ABCD");
}

fn panel_with_long_tool(line_count: usize) -> MessagesPanel {
    let body = (0..line_count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = test_panel();
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        summary: "cmd".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(body.into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_truncated_tool() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(seg_text(&panel, "t1").contains("›"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("›"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("›"));
}

#[test]
fn narrow_truncated_tool_keeps_chevron_click_target() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 24, 12);
    render(&mut panel, area.width, area.height);

    assert!(seg_text(&panel, "t1").contains('›'));
    assert!(panel.handle_click(area.y, area));

    render(&mut panel, area.width, area.height);
    assert!(!seg_text(&panel, "t1").contains('›'));
}

#[test]
fn expand_truncated_tool_does_not_auto_scroll() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    let before_scroll = panel.scroll_top;
    assert!(
        panel.auto_scroll,
        "auto_scroll should be on when content fits"
    );

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!panel.auto_scroll, "expanding should pause auto-scroll");
    assert_eq!(panel.scroll_top, before_scroll, "viewport should not jump");
}

#[test]
fn extract_selection_copies_visible_content_only() {
    let panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let sel = make_sel(area, (0, 0), (u32::from(total - 1), 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        !text.contains("line 50"),
        "truncated line should not be copied"
    );
}

#[test]
fn toggle_returns_false_for_non_expandable() {
    let mut panel = panel_with_long_tool(3);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.toggle_expansion_at(area.y, area));
}

fn panel_with_grep_tool(match_count: usize) -> MessagesPanel {
    let entries = vec![GrepFileEntry {
        path: "src/main.rs".into(),
        groups: (1..=match_count)
            .map(|i| GrepMatchGroup::single(i, format!("match_{i}")))
            .collect(),
    }];
    let mut panel = test_panel();
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        summary: "grep pattern".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: GREP_TOOL_NAME.into(),
        output: ToolOutput::GrepResult { entries },
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_grep_tool() {
    let mut panel = panel_with_grep_tool(8);
    let area = Rect::new(0, 0, 80, 24);
    assert!(seg_text(&panel, "t1").contains("›"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("›"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("›"));
}

fn buffer_text(terminal: &ratatui::Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.push('\n');
    }
    text
}

#[test]
fn streaming_with_cached_segments_shows_end_on_auto_scroll() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "a\n".repeat(20).trim().into(),
    ));
    panel.streaming_text.set_buffer(
        &(0..50)
            .map(|i| format!("stream_{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let terminal = render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let screen = buffer_text(&terminal);
    assert!(screen.contains("stream_49"), "should show end");
    assert!(!screen.contains("stream_0 "), "should not show beginning");
}

#[test]
fn auto_scroll_approaches_bottom_smoothly() {
    let mut panel = test_panel();
    panel.streaming_text.set_buffer(
        &(0..50)
            .map(|i| format!("stream_{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    panel.scroll_top = 0;
    panel.auto_scroll = true;

    let mut terminal = render(&mut panel, 80, 10);
    let first = panel.scroll_top;
    assert!(
        first > 0 && first < 40,
        "should not jump straight to bottom"
    );

    for _ in 0..12 {
        terminal
            .draw(|f| panel.view(f, f.area(), false, panel.is_working()))
            .unwrap();
    }
    assert_eq!(
        panel.scroll_top, 40,
        "should reach bottom after a few frames"
    );
    assert!(panel.auto_scroll);
}

#[test]
fn streaming_content_height_is_cached() {
    use crate::components::streaming_content::StreamingContent;
    use ratatui::style::Style;

    let mut sc = StreamingContent::new("", Style::default(), Style::default(), 0);
    sc.set_buffer("this is a very long line that definitely needs to wrap when the width is only forty characters\nshort");
    let first = sc.height(80);
    let second = sc.height(80);
    assert_eq!(first, second);

    let narrow = sc.height(40);
    assert!(narrow > first, "width change should recompute height");
}

#[test]
fn search_text_includes_truncated_bash_output() {
    let full_output = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = test_panel();
    bash_code_start(&mut panel, "t1", "echo lines");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain(full_output.clone().into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);
    assert!(seg_search(&panel, "t1").contains(&full_output));
}

fn instruction_blocks() -> Vec<InstructionBlock> {
    vec![InstructionBlock {
        path: "agents.md".into(),
        content: "follow style guide".into(),
    }]
}

fn read_code_with_instructions(blocks: Vec<InstructionBlock>) -> ToolOutput {
    ToolOutput::ReadCode {
        path: "file.rs".into(),
        start_line: 1,
        lines: vec!["fn main() {}".into()],
        total_lines: 1,
        instructions: Some(blocks),
    }
}

fn prev_segment_is_spacer(panel: &MessagesPanel, _tool_id: &str) -> bool {
    let idx = panel.cache.find_instructions("t1").unwrap();
    panel.cache.get(idx - 1).unwrap().tool_id.is_none()
}

#[test]
fn instruction_segment_has_spacer_before_it() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    assert!(prev_segment_is_spacer(&panel, "t1__inst"));
}

fn seg_line_count(panel: &MessagesPanel, _tool_id: &str) -> usize {
    let idx = panel.cache.find_instructions("t1").unwrap();
    panel.cache.get(idx).unwrap().lines().len()
}

#[test]
fn toggle_instruction_segment_expands_and_collapses() {
    let mut panel = test_panel();
    let blocks = vec![InstructionBlock {
        path: "agents.md".into(),
        content: "x\n".repeat(100),
    }];
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(blocks),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    rebuild(&mut panel);

    let inst_id = "t1__inst";
    let collapsed = seg_line_count(&panel, inst_id);

    panel.toggle_expansion(inst_id);
    assert!(seg_line_count(&panel, inst_id) > collapsed);

    panel.toggle_expansion(inst_id);
    assert_eq!(seg_line_count(&panel, inst_id), collapsed);
}

#[test]
fn handle_click_returns_nothing_when_no_segment_at_row() {
    let mut panel = test_panel();
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(23, area));
}

#[test]
fn tool_id_at_returns_clicked_tool() {
    let mut panel = test_panel();
    panel.tool_start(start("task1", "task"));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert_eq!(panel.tool_id_at(area.y, area), Some("task1"));
}

#[test]
fn task_tool_id_at_only_routes_header() {
    let mut panel = test_panel();
    panel.tool_start(start("task1", "task"));
    panel.tool_done(ToolDoneEvent {
        id: "task1".into(),
        tool: "task".into(),
        output: ToolOutput::Markdown(
            "body line
"
            .repeat(100)
            .into(),
        ),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    render(&mut panel, 80, 80);
    let area = Rect::new(0, 0, 80, 80);

    assert_eq!(panel.tool_id_at(0, area), Some("task1"));
    assert_eq!(panel.tool_id_at(1, area), None);
    let truncation_row =
        u16::try_from(truncation_source_line(&panel, "task1")).unwrap_or_else(|_| u16::MAX);
    assert_eq!(panel.tool_id_at(truncation_row, area), None);
    assert!(panel.handle_click(truncation_row, area));
    assert!(!seg_text(&panel, "task1").contains("›"));
}

#[test]
fn row_truncation_actions_expand_only_their_section_in_mixed_states() {
    const SCRIPT: SectionFlags = SectionFlags {
        script: true,
        output: false,
        details: false,
    };
    const OUTPUT: SectionFlags = SectionFlags {
        script: false,
        output: true,
        details: false,
    };

    fn panel_with_truncated_script_and_output() -> MessagesPanel {
        let mut panel = test_panel();
        bash_code_start(&mut panel, "t1", &"script line\n".repeat(200));
        panel.tool_done(ToolDoneEvent {
            id: "t1".into(),
            tool: BASH_TOOL_NAME.into(),
            output: ToolOutput::ReadCode {
                path: "output.rs".into(),
                start_line: 1,
                lines: vec!["output line".into(); 200],
                total_lines: 200,
                instructions: None,
            },
            is_error: false,
            annotation: None,
            written_path: None,
        });
        render(&mut panel, 120, 240);
        panel
    }

    let area = Rect::new(0, 0, 120, 240);
    let mut output_first = panel_with_truncated_script_and_output();
    let output_row = u16::try_from(section_truncation_source_line(&output_first, "t1", OUTPUT))
        .unwrap_or_else(|_| u16::MAX);
    assert!(output_first.handle_click(output_row, area));
    assert_eq!(output_first.expanded_tools.get("t1"), Some(&OUTPUT));
    let script_row = u16::try_from(section_truncation_source_line(&output_first, "t1", SCRIPT))
        .unwrap_or_else(|_| u16::MAX);
    assert!(output_first.handle_click(script_row, area));
    assert_eq!(
        output_first.expanded_tools.get("t1"),
        Some(&SectionFlags {
            script: true,
            output: true,
            details: false,
        })
    );

    let mut script_first = panel_with_truncated_script_and_output();
    let script_row = u16::try_from(section_truncation_source_line(&script_first, "t1", SCRIPT))
        .unwrap_or_else(|_| u16::MAX);
    assert!(script_first.handle_click(script_row, area));
    assert_eq!(script_first.expanded_tools.get("t1"), Some(&SCRIPT));
    let output_row = u16::try_from(section_truncation_source_line(&script_first, "t1", OUTPUT))
        .unwrap_or_else(|_| u16::MAX);
    assert!(script_first.handle_click(output_row, area));
    assert_eq!(
        script_first.expanded_tools.get("t1"),
        Some(&SectionFlags {
            script: true,
            output: true,
            details: false,
        })
    );
}

#[test]
fn snapshot_tool_native_truncation_expands_but_snapshot_row_routes_lua() {
    let mut panel = test_panel();
    panel.tool_start(ToolStartEvent {
        input: Some(ToolInput::Code {
            language: "bash".into(),
            code: "echo line
"
            .repeat(200),
        }),
        ..start("t1", BASH_TOOL_NAME)
    });
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("lua snapshot row")])),
        None,
    );
    panel.tool_done(done("t1"));
    render(&mut panel, 80, 80);
    let area = Rect::new(0, 0, 80, 80);
    let truncation_line = truncation_source_line(&panel, "t1");
    assert!(
        panel.cache.segments()[0].lines()[truncation_line]
            .spans
            .iter()
            .any(|span| span.content.contains('›'))
    );

    let mut renamed_lines = panel.cache.segments()[0].lines().to_vec();
    renamed_lines[truncation_line] = Line::from("  wording changed");
    panel.cache.segments_mut()[0].set_lines(renamed_lines);

    assert!(panel.handle_click(
        u16::try_from(truncation_line).unwrap_or_else(|_| u16::MAX),
        area
    ));
    assert!(panel.lua_clicks.is_empty());
    assert!(!seg_text(&panel, "t1").contains("›"));

    render(&mut panel, 80, 240);
    let snapshot_row = u16::try_from(
        panel.cache.segments()[0]
            .lines()
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("lua snapshot row"))
            })
            .unwrap(),
    )
    .unwrap_or_else(|_| u16::MAX);
    assert!(panel.handle_click(snapshot_row, Rect::new(0, 0, 80, 240)));
    assert_eq!(panel.lua_clicks.get("t1"), Some(&vec![1]));
}

#[test]
fn handle_click_on_done_tool_records_click_row() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t1").map(Vec::len), Some(1));
}

#[test]
fn running_live_tool_native_truncation_expands_but_header_routes_lua() {
    const SCRIPT: SectionFlags = SectionFlags {
        script: true,
        output: false,
        details: false,
    };

    let mut panel = test_panel();
    bash_code_start(&mut panel, "t1", &"echo line\n".repeat(200));
    let live = Arc::new(n00n_agent::SharedBuf::new());
    live.set_lines(vec![snap_line("streaming")]);
    panel.register_live_buf("t1".into(), live);
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("streaming")])),
        None,
    );
    let (handle, probe) = n00n_lua::test_support::probed_event_handle();
    panel.lua_event_handle = Some(handle);
    render(&mut panel, 80, 240);
    let area = Rect::new(0, 0, 80, 240);
    let truncation_row = u16::try_from(section_truncation_source_line(&panel, "t1", SCRIPT))
        .unwrap_or_else(|_| u16::MAX);

    assert!(panel.handle_click(truncation_row, area));
    assert_eq!(panel.expanded_tools.get("t1"), Some(&SCRIPT));
    assert_eq!(probe.try_recv(), None);

    assert!(panel.handle_click(area.y, area));
    assert_eq!(probe.try_recv(), Some(("click", vec![])));
}

#[test]
fn handle_click_on_running_tool_forwards_live_without_recording() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    let live = Arc::new(n00n_agent::SharedBuf::new());
    live.set_lines(vec![snap_line("streaming")]);
    panel.register_live_buf("t1".into(), live);
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("streaming")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert!(panel.lua_clicks.is_empty());
    assert!(
        !panel.auto_scroll,
        "clicking a running tool should pause auto-scroll"
    );
}

#[test]
fn handle_click_on_running_live_tool_before_first_snapshot_forwards_header_click() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "task"));
    panel.register_live_buf("t1".into(), Arc::new(n00n_agent::SharedBuf::new()));
    let (handle, probe) = n00n_lua::test_support::probed_event_handle();
    panel.lua_event_handle = Some(handle);
    render(&mut panel, 80, 24);

    assert!(panel.handle_click(0, Rect::new(0, 0, 80, 24)));
    assert_eq!(probe.try_recv(), Some(("click", vec![])));
    assert!(!panel.auto_scroll);
}

#[test]
fn running_tool_without_live_buffer_does_not_swallow_click() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", "task"));
    render(&mut panel, 80, 24);

    assert!(!panel.handle_click(0, Rect::new(0, 0, 80, 24)));
}

#[test]
fn handle_click_on_done_tool_pauses_auto_scroll() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.auto_scroll, "auto_scroll should be on before click");
    assert!(panel.handle_click(area.y, area));
    assert!(
        !panel.auto_scroll,
        "clicking a finished tool should pause auto-scroll"
    );
}

#[test]
fn handle_click_returns_toggled_for_truncated_tool_without_snapshot() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
}

#[test]
fn handle_click_non_tool_segment_returns_nothing() {
    let mut panel = test_panel();
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "user message".into(),
    ));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(area.y, area));
}

#[test]
fn tool_done_removes_live_buf_and_snapshots_dirty() {
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("dirty content")]);

    let mut panel = test_panel();
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "dirty content"
    );
}

/// The handler's buf must supersede the `start` preview: the UI keeps only
/// the last registered buf per `tool_use_id`.
#[test]
fn second_register_live_buf_replaces_first() {
    let preview = Arc::new(n00n_agent::SharedBuf::new());
    preview.set_lines(vec![snap_line("preview")]);
    let handler = Arc::new(n00n_agent::SharedBuf::new());
    handler.set_lines(vec![snap_line("handler")]);

    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&preview));
    panel.register_live_buf("t1".into(), Arc::clone(&handler));
    panel.poll_live_bufs();

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "handler"
    );
}

/// Every finished-tool click on a watched buf carries the full recorded
/// click list as a restore fallback: the runtime serves it warm when it
/// can and restores otherwise, so the UI never guesses runtime state.
#[test_case(false ; "success")]
#[test_case(true ; "error_finish")]
fn handle_click_on_watched_tool_sends_click_with_fallback(is_error: bool) {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "body", is_error);
    assert!(panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t1"].clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(probe.try_recv(), Some(("click_fallback", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_moves_live_buf_to_watched_polled_but_not_animating() {
    let mut panel = test_panel();
    let buf = finish_with_live_buf(&mut panel, "t1", "before", false);
    assert!(panel.watching("t1"));
    assert!(
        !panel.is_animating(),
        "watched bufs must not keep the UI animating"
    );

    buf.set_lines(vec![snap_line("after")]);
    panel.poll_live_bufs();
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after"
    );
}

#[test]
fn live_snapshot_update_preserves_manual_scroll_anchor_at_narrow_width() {
    let mut panel = test_panel();
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("short")]);
    panel.tool_start(start("t1", "codegraph"));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.poll_live_bufs();
    for i in 0..20 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i}"),
        ));
    }
    render(&mut panel, 18, 8);
    panel.restore_scroll(8, false);

    let old_scroll = panel.scroll_top();
    let old_height = panel.segment_heights()[0];
    buf.set_lines(
        (1..=6)
            .map(|i| snap_line(&format!("result {i} wraps across the narrow viewport")))
            .collect(),
    );
    panel.poll_live_bufs();
    let new_height = panel.segment_heights()[0];

    assert!(new_height > old_height);
    assert_eq!(
        panel.scroll_top(),
        old_scroll + (new_height - old_height),
        "content below a live card must remain visually anchored"
    );
    assert!(!panel.auto_scroll());
}

#[test]
fn live_snapshot_update_keeps_auto_scroll_at_bottom_without_intermediate_churn() {
    let mut panel = test_panel();
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(
        (1..=6)
            .map(|i| snap_line(&format!("initial result {i} wraps at narrow widths")))
            .collect(),
    );
    panel.tool_start(start("t1", "codegraph"));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    for _ in 0..20 {
        render(&mut panel, 18, 6);
    }
    assert!(panel.auto_scroll());

    let old_scroll = panel.scroll_top();
    let old_height = panel.segment_heights()[0];
    buf.set_lines(
        (1..=10)
            .map(|i| snap_line(&format!("updated result {i} wraps at narrow widths")))
            .collect(),
    );
    panel.poll_live_bufs();
    let new_height = panel.segment_heights()[0];

    assert!(new_height > old_height);
    assert_eq!(panel.scroll_top(), old_scroll + (new_height - old_height));
    assert!(panel.auto_scroll());
}

#[test]
fn live_snapshot_reflows_when_viewport_becomes_narrow_without_republication() {
    let mut panel = test_panel();
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line(
        "a complete explore result line that must reflow when the viewport narrows",
    )]);
    panel.tool_start(start("t1", "codegraph"));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    render(&mut panel, 80, 20);
    let wide_height = panel.segment_heights()[0];
    assert!(buf.read_if_dirty().is_none());

    render(&mut panel, 16, 20);
    let narrow_height = panel.segment_heights()[0];

    assert!(narrow_height > wide_height);
    assert!(
        buf.read_if_dirty().is_none(),
        "resize must not republish the Lua buffer"
    );
}

#[test]
fn watched_fifo_evicts_oldest_which_stops_polling_and_restores_with_recorded_clicks() {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t0", "before", false);

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t0").map(Vec::len), Some(1));
    assert_eq!(
        probe.try_recv(),
        Some(("click_fallback", panel.lua_clicks["t0"].clone()))
    );

    for i in 1..=WARM_TOOL_CAP {
        finish_with_live_buf(&mut panel, &format!("t{i}"), "body", false);
    }
    assert_eq!(panel.watched_bufs.len(), WARM_TOOL_CAP);
    assert!(!panel.watching("t0"));

    buf.set_lines(vec![snap_line("after-eviction")]);
    panel.poll_live_bufs();
    let msg = panel.find_tool_msg_mut("t0").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "before",
        "evicted buf must no longer be polled"
    );

    render(&mut panel, 80, 24);
    panel.scroll_to_top();
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t0"].clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(probe.try_recv(), Some(("restore", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_without_live_buf_is_not_watched_and_click_restores() {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    let mut ev = start("t1", BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("body")])),
        None,
    );
    panel.tool_done(done("t1"));
    assert!(!panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// The stale-run_id filter drops `ToolDone` events after a cancel, so the
/// cancel path itself must retire live bufs: no `is_animating` pin, and
/// the tool stays clickable through the warm path.
#[test]
fn cancel_in_progress_retires_live_buf_to_watched() {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("body")]);
    let mut ev = start("t1", BASH_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    panel.cancel_in_progress();
    assert!(
        !panel.is_animating(),
        "cancel must not leak live bufs that pin animation"
    );
    assert!(panel.watching("t1"));

    buf.set_lines(vec![snap_line("after-cancel")]);
    panel.poll_live_bufs();
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after-cancel"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(probe.try_recv(), Some(("click", vec![])));
    assert_eq!(probe.try_recv(), None);
}

/// A restore reply supersedes the old live view: the buf must stop
/// being watched so its stale content can't overwrite the fresh
/// snapshot, and later clicks must go through restore.
#[test]
fn restore_reply_stops_watching_buf() {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rebaked")])),
        Some(baked_gen),
    );
    assert!(!panel.watching("t1"));

    buf.set_lines(vec![snap_line("stale-mutation")]);
    panel.poll_live_bufs();
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "rebaked",
        "unwatched buf must not overwrite the restored snapshot"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// Requesting a rebake already stops watching: clicks inside the
/// request/reply window must restore (with the new theme) instead of
/// mutating the old-theme buf.
#[test]
fn rebake_request_stops_watching_buf() {
    let (eh, probe) = n00n_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = test_panel();
    panel.set_restore_channel(Some(eh), Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let next_gen = panel.snapshot_gen_of("t1").unwrap() + 1;
    panel.rebake_stale_snapshots(next_gen);
    assert!(!panel.watching("t1"));
    assert_eq!(probe.try_recv(), Some(("restore", vec![])));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn live_buf_streams_across_clean_polls() {
    let buf = Arc::new(n00n_agent::SharedBuf::new());
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    buf.append(snap_line("first"));
    panel.poll_live_bufs();
    panel.poll_live_bufs();

    buf.append(snap_line("second"));
    panel.poll_live_bufs();

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    let snapshot = msg.render_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.lines.len(), 2);
}

#[test]
fn tool_done_without_live_buf_preserves_existing_snapshot() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("pre-existing")])),
        None,
    );
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "pre-existing"
    );
}

#[test]
fn tool_done_clean_live_buf_does_not_snapshot() {
    let buf = Arc::new(n00n_agent::SharedBuf::new());

    let mut panel = test_panel();
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(
        msg.render_snapshot.is_none(),
        "clean (never-written) live buf should not produce a snapshot"
    );
}

const REQUEST_RECORDED_MSG: &str = "a fired re-bake records the requested generation";
const NOT_RESTAMPED_MSG: &str =
    "the re-bake walk must not optimistically stamp the displayed generation";
const NO_REQUEST_MSG: &str = "snapshot-free message must not trigger a re-bake request";
const SUPERSEDED_DROP_MSG: &str =
    "a re-bake reply older than the applied generation must be dropped (monotonic)";

fn bash_tool_with_snapshot(id: &str) -> MessagesPanel {
    let mut panel = test_panel();
    panel.tool_start(start(id, BASH_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: id.into(),
        tool: BASH_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
    });
    panel.tool_snapshot(
        id,
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    panel
}

fn rendered_snapshot() -> BufferSnapshot {
    BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")]))
}

#[test]
fn rebake_walk_requests_without_stamping_displayed_generation() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({ "command": "echo" })));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "plain".into()));
    panel.set_restore_channel(
        Some(n00n_lua::EventHandle::disconnected_for_test()),
        Some(test_event_sender()),
    );

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    let next_gen = baked_gen + 1;
    panel.rebake_stale_snapshots(next_gen);

    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(baked_gen),
        "{NOT_RESTAMPED_MSG}"
    );
    assert_eq!(
        panel.rebake_requested_gen("t1"),
        Some(next_gen),
        "{REQUEST_RECORDED_MSG}"
    );
    assert_eq!(panel.messages[1].snapshot_theme_gen, 0, "{NO_REQUEST_MSG}");
}

#[test]
fn superseded_rebake_reply_is_dropped() {
    let mut panel = bash_tool_with_snapshot("t1");
    let baked = panel.snapshot_gen_of("t1").unwrap();
    let newer = baked + 3;
    panel.tool_snapshot("t1", rendered_snapshot(), Some(newer));
    panel.tool_snapshot("t1", rendered_snapshot(), Some(baked + 1));
    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(newer),
        "{SUPERSEDED_DROP_MSG}"
    );
}

fn test_event_sender() -> n00n_agent::EventSender {
    let (tx, _rx) = flume::unbounded();
    n00n_agent::EventSender::new(tx, 0)
}

const RAW_INPUT_SET_MSG: &str = "tool_raw_input must be set from event payload";
const HEADER_GEN_MSG: &str = "header snapshot must stamp the provided generation";
const LIVE_PANEL_GEN_MSG: &str = "live snapshot (None gen) must stamp with panel theme_generation";
const REBAKE_NOOP_MSG: &str = "rebake without channel must be a no-op (no requested gen)";

#[test_case(false ; "fresh_start")]
#[test_case(true  ; "upgrade_from_pending")]
fn tool_start_propagates_raw_input(pre_pending: bool) {
    let mut panel = test_panel();
    if pre_pending {
        panel.tool_pending("t1".into(), BASH_TOOL_NAME);
    }
    let mut event = start("t1", BASH_TOOL_NAME);
    event.raw_input = Some(serde_json::json!({"command": "echo"}));
    panel.tool_start(event);

    let raw = panel
        .find_tool_msg_mut("t1")
        .unwrap()
        .tool_raw_input
        .as_ref();
    assert!(raw.is_some(), "{RAW_INPUT_SET_MSG}");
    assert_eq!(
        raw.unwrap().as_ref(),
        &serde_json::json!({"command": "echo"}),
        "{RAW_INPUT_SET_MSG}"
    );
}

#[test]
fn header_snapshot_stamps_gen_on_top_level() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_header_snapshot("t1", rendered_snapshot(), Some(5));

    assert_eq!(panel.snapshot_gen_of("t1"), Some(5), "{HEADER_GEN_MSG}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(msg.render_header.is_some(), "render_header must be set");
}

#[test]
fn live_snapshot_uses_panel_generation() {
    let mut panel = test_panel();
    panel.tool_start(start("t1", BASH_TOOL_NAME));
    panel.tool_snapshot("t1", rendered_snapshot(), None);

    assert_eq!(panel.snapshot_gen_of("t1"), Some(0), "{LIVE_PANEL_GEN_MSG}");
}

#[test]
fn rebake_without_channel_is_noop() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({"command": "echo"})));
    let baked_gen = panel.snapshot_gen_of("t1").unwrap();

    panel.rebake_stale_snapshots(baked_gen + 1);

    assert!(
        panel.rebake_requested_gen("t1").is_none(),
        "{REBAKE_NOOP_MSG}"
    );
}

#[test]
fn hidden_config_still_streams_thinking_live() {
    let mut panel = test_panel_with_config(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
    panel
        .streaming_thinking
        .set_buffer("line one\nline two\nline three");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(text.contains("line one"), "live reasoning: {text}");
    assert!(text.contains("line three"), "live reasoning: {text}");
    assert!(!text.contains("›"), "live reasoning: {text}");
}

#[test]
fn live_streaming_thinking_does_not_toggle_on_click() {
    let mut panel = test_panel_with_config(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
    panel.streaming_thinking.set_buffer("secret reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, 80, 10);
    assert!(!panel.handle_click(0, area));
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("secret reasoning"),
        "expanded view should show reasoning; got: {text}"
    );
    assert!(!text.contains("›"), "live reasoning: {text}");
}

#[test]
fn hide_keeps_cached_thinking_as_indicator() {
    let mut panel = test_panel_with_config(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
    panel.thinking_delta("reasoning here");
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thought for"),
        "completed thinking should retain its measured duration; got: {text}"
    );
    assert!(
        text.contains("1 lines"),
        "footer always shows the line count; got: {text}"
    );
    assert!(
        text.contains("›"),
        "footer should hint click-to-expand; got: {text}"
    );
    assert!(
        !text.contains("reasoning here"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn streaming_thinking_is_expanded_then_collapses_on_completion() {
    let mut panel = test_panel();
    panel.thinking_delta("visible reasoning");
    panel.streaming_thinking.set_buffer("visible reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("visible reasoning"),
        "streaming reasoning stays expanded; got: {text}"
    );

    panel.flush();
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(text.contains("Thought for"), "completed disclosure: {text}");
    assert!(
        !text.contains("visible reasoning"),
        "collapsed reasoning: {text}"
    );
}

#[test]
fn hide_cached_thinking_persists_as_indicator() {
    let mut panel = test_panel_with_config(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
    let lines: Vec<String> = (1..=7).map(|n| format!("cached line {n}")).collect();
    panel.thinking_delta(&lines.join("\n"));
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thought for"),
        "completed thinking should retain its measured duration; got: {text}"
    );
    assert!(text.contains("7 lines"), "footer line count; got: {text}");
    assert!(
        text.contains("›"),
        "footer should hint click-to-expand; got: {text}"
    );
    assert!(
        !text.contains("cached line 7"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
    assert!(
        !text.contains("cached line 1"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn hide_cached_thinking_click_expands() {
    let mut panel = test_panel_with_config(UiConfig {
        show_thinking: false,
        ..UiConfig::default()
    });
    panel.thinking_delta("hidden cached reasoning");
    panel.flush();
    let area = Rect::new(0, 0, 80, 12);
    render(&mut panel, 80, 12);
    assert!(
        panel.handle_click(0, area),
        "clicking persisted thinking should toggle expand"
    );
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("hidden cached reasoning"),
        "expanded view shows full reasoning; got: {text}"
    );
    assert!(
        !text.contains("›"),
        "footer should disappear when expanded; got: {text}"
    );
}

#[test]
fn stream_reset_keeps_new_thinking_expanded() {
    let mut panel = test_panel();
    panel.thinking_delta("stale reasoning");
    panel.stream_reset();
    panel.thinking_delta("fresh reasoning");
    panel.streaming_thinking.set_buffer("fresh reasoning");

    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("fresh reasoning"),
        "new streaming reasoning should be visible; got: {text}"
    );
    assert!(
        !text.contains("stale reasoning"),
        "reset must clear stale text"
    );
}
fn mixed_messages(n: usize) -> Vec<DisplayMessage> {
    (0..n)
        .map(|i| {
            if i % 3 == 0 {
                let mut m = DisplayMessage::new(
                    DisplayRole::Tool(Box::new(ToolRole {
                        id: format!("t{i}"),
                        status: ToolStatus::Success,
                        name: Arc::from("bash"),
                    })),
                    format!("tool {i}"),
                );
                m.tool_raw_input = Some(Arc::new(serde_json::json!({ "command": "echo" })));
                m.tool_output = Some(Arc::new(ToolOutput::Plain(format!("out {i}").into())));
                m
            } else if i % 2 == 0 {
                DisplayMessage::new(DisplayRole::User, format!("user {i}"))
            } else {
                DisplayMessage::new(DisplayRole::Assistant, format!("assistant {i}"))
            }
        })
        .collect()
}

fn assert_cache_equal(a: &MessagesPanel, b: &MessagesPanel) {
    assert_eq!(a.message_count(), b.message_count(), "message_count");
    assert_eq!(a.cache.len(), b.cache.len(), "segment count");
    assert_eq!(a.cache.msg_count(), b.cache.msg_count(), "built msg_count");
    assert_eq!(
        a.cache.search_texts(),
        b.cache.search_texts(),
        "search_texts"
    );
    assert_eq!(
        a.cache.total_height(80),
        b.cache.total_height(80),
        "total_height"
    );
    let ai: Vec<_> = a.cache.segments().iter().map(|s| s.msg_index).collect();
    let bi: Vec<_> = b.cache.segments().iter().map(|s| s.msg_index).collect();
    assert_eq!(ai, bi, "msg_index backlinks");
    let at: Vec<_> = a
        .cache
        .segments()
        .iter()
        .map(|s| s.tool_id.clone())
        .collect();
    let bt: Vec<_> = b
        .cache
        .segments()
        .iter()
        .map(|s| s.tool_id.clone())
        .collect();
    assert_eq!(at, bt, "tool_ids");
}

#[test_case(0, 4, 0, &[] ; "empty")]
#[test_case(3, 4, 3, &[] ; "below_batch_all_initial")]
#[test_case(4, 4, 4, &[] ; "exactly_batch_all_initial")]
#[test_case(5, 4, 4, &[1] ; "one_over_batch_one_backlog")]
#[test_case(10, 4, 4, &[4, 2] ; "two_batches")]
#[test_case(12, 4, 4, &[4, 4] ; "even_split")]
fn restore_plan_splits_recent_first(total: usize, batch: usize, initial: usize, batches: &[usize]) {
    let plan = restore_plan(total, batch);
    assert_eq!(plan.initial, initial);
    assert_eq!(plan.prepend_batches, batches);
}

#[test]
fn restore_plan_clamps_zero_batch_size() {
    let plan = restore_plan(7, 0);
    assert_eq!(plan.initial, 1);
    assert_eq!(plan.prepend_batches, vec![1, 1, 1, 1, 1, 1]);
}

#[test]
fn incremental_restore_matches_full_load() {
    let msgs = mixed_messages(40);
    let batch = 7;

    let mut full = test_panel();
    full.load_messages(msgs.clone());
    render(&mut full, 80, 24);

    let mut incr = test_panel();
    incr.begin_restore(msgs, batch);
    render(&mut incr, 80, 24);
    while incr.is_restoring() {
        render(&mut incr, 80, 24);
    }

    assert_cache_equal(&full, &incr);
}

#[test]
fn incremental_restore_first_frame_shows_only_recent() {
    let msgs = mixed_messages(21);
    let mut panel = test_panel();
    panel.begin_restore(msgs, 7);
    render(&mut panel, 80, 24);
    assert_eq!(
        panel.message_count(),
        7,
        "first frame renders only the recent batch"
    );
    assert!(panel.is_restoring(), "older history still pending");
    while panel.is_restoring() {
        render(&mut panel, 80, 24);
    }
    assert_eq!(
        panel.message_count(),
        21,
        "full history loaded after backfill"
    );
    assert!(!panel.is_restoring());
}

#[test]
fn incremental_restore_below_batch_loads_all_at_once() {
    let msgs = mixed_messages(3);
    let mut panel = test_panel();
    panel.begin_restore(msgs, 7);
    render(&mut panel, 80, 24);
    assert_eq!(panel.message_count(), 3);
    assert!(
        !panel.is_restoring(),
        "no backlog when total fits one batch"
    );
}
