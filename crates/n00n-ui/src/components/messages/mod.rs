mod render;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

use self::render::RenderCursor;
use self::segment::{MessageAction, Segment, SegmentCache, Surface};

pub(crate) use self::segment::wrapped_line_count;

use super::tool_display::{
    RenderCtx, ToolLines, append_annotation, append_right_info, assistant_style,
    build_instructions_lines, build_tool_lines, control_style, done_style, error_style,
    format_timestamp_now, thinking_style, truncate_to_header, user_style,
};
use super::{
    CompactionDisplay, DisplayMessage, DisplayMetadata, DisplayRole, ToolRole, ToolStatus,
    apply_scroll_delta, code_view::SectionFlags,
};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{hr_line, plain_lines, text_to_lines, truncate_output};
use crate::mascot::Mascot;
use crate::render_worker::RenderWorker;
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use n00n_config::{ToolOutputLines, UiConfig};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use super::scrollbar::{ScrollInfo, render_vertical_scrollbar};
use super::streaming_content::StreamingContent;
use n00n_agent::tools::TASK_TOOL_NAME;
use n00n_agent::{
    BufferSnapshot, EventSender, InstructionBlock, NO_FILES_FOUND, SharedBuf, ToolDoneEvent,
    ToolOutput, ToolStartEvent,
};
use n00n_lua::{EventHandle, WARM_TOOL_CAP};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui_image::picker::Picker;

pub(crate) const JUMP_TO_BOTTOM_TEXT: &str = "↓ Bottom";
const JUMP_TO_BOTTOM_KEY_GAP: &str = "  ";
const JUMP_TO_BOTTOM_THRESHOLD: u16 = 1;
const JUMP_TO_BOTTOM_POPUP_HEIGHT: u16 = 3;
const JUMP_TO_BOTTOM_POPUP_BOTTOM_MARGIN: u16 = 1;
const MAX_HIGHLIGHT_RESULTS_PER_FRAME: usize = 8;
const MAX_HIGHLIGHT_RETRIES_PER_FRAME: usize = 8;
pub(super) const COPY_LABEL_WIDTH: u16 = 6;

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

pub struct MessagesPanel {
    pub(crate) messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll_top: u16,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    cache: SegmentCache,
    last_total_lines: u16,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    mascot: Mascot,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    expanded_compactions: HashSet<String>,
    /// Per-tool log of post-completion click rows, replayed on restore.
    lua_clicks: HashMap<String, Vec<usize>>,
    live_bufs: HashMap<String, Arc<SharedBuf>>,
    /// Bufs of finished tools we keep polling so runtime-side warm
    /// clicks stay visible. Purely local: every finished-tool click
    /// carries a restore fallback, so we never track the runtime's
    /// warm cache.
    watched_bufs: VecDeque<(String, Arc<SharedBuf>)>,
    tool_output_lines: ToolOutputLines,
    lua_event_handle: Option<EventHandle>,
    restore_event_tx: Option<EventSender>,
    show_thinking: bool,
    thinking_collapsed: bool,
    thinking_started: Option<Instant>,
    /// One re-bake per tool per generation; `snapshot_theme_gen`
    /// only bumps when colors actually land.
    rebake_requested: HashMap<String, u64>,
    prompt_progress: Option<PromptProgress>,
    jump_to_bottom_popup: Option<Rect>,
    /// Older display messages waiting to be prepended in batches so a long
    /// session resumes without blocking the first paint on full rendering.
    restore_backlog: Vec<DisplayMessage>,
    /// Per-frame prepend sizes, drained front-to-back by `drain_restore_backlog`.
    restore_batches: VecDeque<usize>,
    picker: Arc<Picker>,
}

/// Incremental restore plan: show the `initial` (most recent) messages first,
/// then prepend the remaining older messages in `prepend_batches`-sized chunks
/// (drained front-to-back, each chunk taken from the end of the backlog so
/// older history lands just above what is already rendered).
struct RestorePlan {
    initial: usize,
    prepend_batches: Vec<usize>,
}

fn restore_plan(total: usize, batch_size: usize) -> RestorePlan {
    let batch_size = batch_size.max(1);
    let initial = total.min(batch_size);
    let mut backlog = total - initial;
    let mut prepend_batches = Vec::new();
    while backlog > 0 {
        let take = backlog.min(batch_size);
        prepend_batches.push(take);
        backlog -= take;
    }
    RestorePlan {
        initial,
        prepend_batches,
    }
}

impl MessagesPanel {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(ui_config: UiConfig, picker: Arc<Picker>) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        let reduced_motion = ui_config.reduced_motion;
        let mut panel = Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll_top: u16::MAX,
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            cache: SegmentCache::new(),
            last_total_lines: 0,
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            mascot: Mascot::new(ui_config.mascot),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            expanded_compactions: HashSet::new(),
            lua_clicks: HashMap::new(),
            live_bufs: HashMap::new(),
            watched_bufs: VecDeque::new(),
            tool_output_lines: ui_config.tool_output_lines,
            lua_event_handle: None,
            restore_event_tx: None,
            show_thinking: ui_config.show_thinking,
            thinking_collapsed: false,
            thinking_started: None,
            rebake_requested: HashMap::new(),
            prompt_progress: None,
            jump_to_bottom_popup: None,
            restore_backlog: Vec::new(),
            restore_batches: VecDeque::new(),
            picker,
        };
        panel.streaming_thinking.set_reduced_motion(reduced_motion);
        panel.streaming_text.set_reduced_motion(reduced_motion);
        panel
    }

    pub fn set_restore_channel(
        &mut self,
        event_handle: Option<EventHandle>,
        event_tx: Option<EventSender>,
    ) {
        self.lua_event_handle = event_handle;
        self.restore_event_tx = event_tx;
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages.push(msg);
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.prompt_progress = None;
        self.cache.invalidate_from_msg_count();
    }

    pub fn set_pending_compaction_summary(&mut self, summary: String) {
        if let Some(message) = self.messages.iter_mut().rev().find(|message| {
            matches!(
                message.metadata.as_ref(),
                Some(DisplayMetadata::CompactionPending)
            )
        }) {
            message.annotation = Some(summary);
        }
    }

    pub fn complete_pending_compaction(&mut self) -> bool {
        let Some(message_index) = self.messages.iter().rposition(|message| {
            matches!(
                message.metadata.as_ref(),
                Some(DisplayMetadata::CompactionPending)
            )
        }) else {
            return false;
        };
        let streamed_summary = self.streaming_text.take_all();
        self.streaming_thinking.clear();
        self.thinking_collapsed = false;
        self.thinking_started = None;
        let summary = self.messages[message_index]
            .annotation
            .take()
            .or_else(|| (!streamed_summary.is_empty()).then_some(streamed_summary));
        self.messages[message_index] = DisplayMessage::compaction(CompactionDisplay {
            id: format!("live-compaction:{message_index}"),
            depth: 1,
            message_count: 0,
            summary,
            entries: Vec::new(),
        });
        self.cache.invalidate_from_msg_count();
        true
    }

    pub fn fail_pending_compaction(&mut self, reason: &str) -> bool {
        let Some(message_index) = self.messages.iter().rposition(|message| {
            matches!(
                message.metadata.as_ref(),
                Some(DisplayMetadata::CompactionPending)
            )
        }) else {
            return false;
        };
        // A provider that streamed deltas before erroring has already filled these
        // buffers; leaving them splices the failed summary into the agent's next
        // response.
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.thinking_collapsed = false;
        self.thinking_started = None;
        self.messages[message_index] = DisplayMessage::new(
            DisplayRole::Assistant,
            format!("Auto-compaction failed: {reason}. Continuing without compacting."),
        );
        self.cache.invalidate_from_msg_count();
        true
    }

    pub fn load_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        if !self.show_thinking {
            for msg in &mut msgs {
                if matches!(msg.role, DisplayRole::Thinking) {
                    msg.thinking_collapsed = true;
                }
            }
        }
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.expanded_compactions.clear();
        self.lua_clicks.clear();
        self.live_bufs.clear();
        self.watched_bufs.clear();
        self.rebake_requested.clear();
        self.highlight_segment = None;
        self.thinking_collapsed = false;
        self.thinking_started = None;
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.restore_backlog.clear();
        self.restore_batches.clear();
    }

    /// Splits `msgs` into an initial recent batch (rendered immediately) and an
    /// older backlog that is prepended a few messages per frame so resuming a
    /// long session does not block startup on a full render.
    pub fn begin_restore(&mut self, msgs: Vec<DisplayMessage>, batch_size: usize) {
        let plan = restore_plan(msgs.len(), batch_size);
        let mut all = msgs;
        let initial = all.split_off(all.len() - plan.initial);
        self.load_messages(initial);
        self.restore_backlog = all;
        self.restore_batches = plan.prepend_batches.into();
    }

    pub fn is_restoring(&self) -> bool {
        !self.restore_batches.is_empty()
    }

    fn drain_restore_backlog(&mut self) {
        if self.restore_batches.is_empty() || self.cache.segments().is_empty() {
            return;
        }
        let Some(take) = self.restore_batches.pop_front() else {
            return;
        };
        let take = take.min(self.restore_backlog.len());
        if take == 0 {
            return;
        }
        let split = self.restore_backlog.len() - take;
        let batch = self.restore_backlog.split_off(split);
        self.prepend_messages(batch);
    }

    /// Prepends `msgs` (older history) in front of the existing messages,
    /// building their render segments up front and shifting the cache so the
    /// already-rendered recent tail stays intact.
    pub fn prepend_messages(&mut self, mut msgs: Vec<DisplayMessage>) {
        let n = msgs.len();
        if n == 0 {
            return;
        }
        if !self.show_thinking {
            for msg in &mut msgs {
                if matches!(msg.role, DisplayRole::Thinking) {
                    msg.thinking_collapsed = true;
                }
            }
        }
        let mut built = Vec::new();
        for (i, msg) in msgs.iter().enumerate() {
            if !built.is_empty() {
                built.push(Segment::spacer());
            }
            built.extend(self.build_segments_for_msg(msg, i));
        }
        if !built.is_empty() && !self.cache.segments().is_empty() {
            built.push(Segment::spacer());
        }
        let added_height = built
            .iter()
            .map(|seg| u32::from(seg.height(self.viewport_width)))
            .sum::<u32>();
        self.cache.prepend(built, n);
        self.messages.splice(0..0, msgs);
        if !self.auto_scroll {
            self.scroll_top = self
                .scroll_top
                .saturating_add(crate::cast::u32_to_u16(added_height));
        }
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.thinking_started.get_or_insert_with(Instant::now);
        self.thinking_collapsed = false;
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        // Visible assistant text is a model-response boundary: preserve the
        // preceding reasoning as its own disclosure before streaming it.
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    /// Toggles all persisted reasoning disclosures in this transcript.
    ///
    /// This deliberately leaves live reasoning alone and only changes cached
    /// `DisplayRole::Thinking` rows, so opaque provider reasoning—which never
    /// reaches the display model—cannot be revealed.
    pub fn toggle_transcript_details(&mut self) -> bool {
        let expanded = self
            .messages
            .iter()
            .any(|msg| matches!(msg.role, DisplayRole::Thinking) && msg.thinking_collapsed);
        self.show_thinking = expanded;
        for msg in &mut self.messages {
            if matches!(msg.role, DisplayRole::Thinking) {
                msg.thinking_collapsed = !expanded;
            }
        }
        self.cache.invalidate_from_msg_count();
        expanded
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now());
        self.messages.push(msg);
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_raw_input = event.raw_input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.annotation = event.annotation;
            msg.render_header = event.render_header;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_raw_input = event.raw_input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.annotation = event.annotation;
        msg.render_header = event.render_header;
        msg.timestamp = Some(format_timestamp_now());
        self.messages.push(msg);
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or_else(|| "");
        truncate_to_header(&mut msg.text);
        let truncated = truncate_output(content, self.tool_output_lines.get(tool_name));
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        self.retire_live_buf(&event.id);
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        truncate_to_header(&mut msg.text);
        let done_annotation = event
            .annotation
            .as_deref()
            .map(str::to_owned)
            .or_else(|| event.output.annotation());
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }
        if event.tool.as_ref() == TASK_TOOL_NAME {
            append_annotation(&mut msg.annotation, "ctrl+t to view session");
        }

        match &event.output {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text)
                if msg.render_snapshot.is_none() =>
            {
                let tr = truncate_output(&text.text, self.tool_output_lines.get(&event.tool));
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            _ => {}
        }
        msg.tool_output = Some(Arc::new(event.output));
        msg.live_output = None;
        self.rebuild_tool_segment(&event.id);
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(tool_id, |msg| summary.clone_into(&mut msg.text));
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.update_tool(tool_id, |msg| append_annotation(&mut msg.annotation, model));
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, false, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.store_snapshot(tool_id, snapshot, true, theme_gen);
    }

    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let Some(idx) = self
            .messages
            .iter()
            .rposition(|m| matches!(m.role, DisplayRole::Tool(_)))
        else {
            return;
        };
        self.messages[idx].turn_usage = Some(usage);
        let DisplayRole::Tool(t) = &self.messages[idx].role else {
            unreachable!()
        };
        let id = t.id.clone();
        self.rebuild_tool_segment(&id);
    }

    fn upsert_instruction_segment(
        &mut self,
        parent_id: &str,
        blocks: &[InstructionBlock],
        parent_idx: usize,
    ) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let exp = self
            .expanded_tools
            .get(&inst_id)
            .copied()
            .unwrap_or_else(SectionFlags::default);
        let tl = build_instructions_lines(blocks, self.viewport_width, exp.output);

        if let Some(seg_idx) = self.cache.find_instructions(parent_id) {
            if let Some(seg) = self.cache.get_mut(seg_idx) {
                seg.search_text.clone_from(&tl.search_text);
                seg.update_with_reuse(tl, &self.hl_worker);
            }
        } else {
            let mut seg = Segment::with_instructions(parent_id.to_owned());
            seg.search_text.clone_from(&tl.search_text);
            seg.apply_highlight(tl, &self.hl_worker);
            self.cache.insert(parent_idx + 1, Segment::spacer());
            self.cache.insert(parent_idx + 2, seg);
        }
    }

    fn update_tool(&mut self, tool_id: &str, update_msg: impl FnOnce(&mut DisplayMessage)) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        update_msg(msg);
        self.rebuild_tool_segment(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.thinking_collapsed = false;
        self.thinking_started = None;
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: &str) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                id,
                tool,
                output: ToolOutput::Plain(message.to_string().into()),
                is_error: true,
                annotation: None,
                written_path: None,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            // The stale-run_id filter drops these tools' ToolDone events,
            // so retire their live bufs here: keeps them clickable via
            // the warm path and stops them pinning `is_animating`.
            self.retire_live_buf(id);
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    pub fn is_working(&self) -> bool {
        self.in_progress_count() > 0
            || self.streaming_thinking.is_animating()
            || self.streaming_text.is_animating()
            || !self.live_bufs.is_empty()
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_else(SectionFlags::default);
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        entry.script = !entry.script;
        entry.output = !entry.output;
        self.rebuild_expanded_tool(&tool_id);
        self.auto_scroll = false;
        true
    }

    fn segment_position(&self, tool_id: &str) -> (Option<u32>, u16) {
        let Some(index) = self.cache.find_by_tool_id(tool_id) else {
            return (None, 0);
        };
        let start = self.cache.segments()[..index]
            .iter()
            .map(|seg| u32::from(seg.height(self.viewport_width)))
            .sum();
        (
            Some(start),
            self.cache.segments()[index].height(self.viewport_width),
        )
    }

    fn shift_scroll_for_height_change(&mut self, old_height: u16, new_height: u16) {
        let delta = i32::from(new_height) - i32::from(old_height);
        self.scroll_top = if delta >= 0 {
            self.scroll_top
                .saturating_add(u16::try_from(delta).unwrap_or_else(|_| u16::MAX))
        } else {
            self.scroll_top
                .saturating_sub(u16::try_from(delta.unsigned_abs()).unwrap_or_else(|_| u16::MAX))
        };
    }

    fn preserve_anchor(&mut self, old_start: Option<u32>, old_height: u16, tool_id: &str) {
        let Some(old_start) = old_start else {
            return;
        };
        let (_, new_height) = self.segment_position(tool_id);
        if old_start < u32::from(self.scroll_top) {
            self.shift_scroll_for_height_change(old_height, new_height);
        }
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages.last().map_or("", |m| m.text.as_str())
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn rebake_requested_gen(&self, tool_id: &str) -> Option<u64> {
        self.rebake_requested.get(tool_id).copied()
    }

    #[cfg(test)]
    pub fn snapshot_gen_of(&self, tool_id: &str) -> Option<u64> {
        self.current_snapshot_gen(tool_id)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    pub fn set_prompt_progress(&mut self, progress: Option<PromptProgress>) {
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_top = apply_scroll_delta(self.scroll_top, delta).min(self.max_scroll());
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.auto_scroll = false;
    }

    pub fn enable_auto_scroll(&mut self) {
        self.auto_scroll = true;
    }

    pub fn jump_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_top = self.max_scroll();
    }

    pub fn jump_to_bottom_popup(&self) -> Option<Rect> {
        self.jump_to_bottom_popup
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        let width = self.viewport_width;
        let offset = crate::cast::u32_to_u16(
            self.cache
                .segments()
                .iter()
                .take(segment_index)
                .map(|s| u32::from(s.height(width)))
                .sum::<u32>(),
        );
        self.scroll_top = offset.min(self.max_scroll());
        self.auto_scroll = false;
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.scroll_top = scroll_top;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        i32::from(self.viewport_height) / 2
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn copy_at(&self, row: u16, col: u16, area: Rect) -> Option<(String, &'static str)> {
        if area.height == 0 {
            return None;
        }
        let doc_row = u32::from(row.saturating_sub(area.y)) + u32::from(self.scroll_top);
        let (index, segment, segment_start) =
            self.cache.segment_at_row(doc_row, self.viewport_width)?;
        if doc_row != segment_start
            || segment.surface() != Surface::Assistant
            || !segment.has_copy_label_room(self.viewport_width, COPY_LABEL_WIDTH)
        {
            return None;
        }
        let copy_width = COPY_LABEL_WIDTH.min(area.width);
        let copy_area = Rect::new(area.right().saturating_sub(copy_width), row, copy_width, 1);
        if !copy_area.contains(ratatui::layout::Position::new(col, row)) {
            return None;
        }
        let (text, label) = self.cache.get(index)?.copy_payload()?;
        Some((text.to_owned(), label))
    }

    pub fn tool_id_at(&self, row: u16, area: Rect) -> Option<&str> {
        if area.height == 0 {
            return None;
        }
        let doc_row = u32::from(row.saturating_sub(area.y)) + u32::from(self.scroll_top);
        let (_, segment, segment_start) =
            self.cache.segment_at_row(doc_row, self.viewport_width)?;
        let rel = u16::try_from(doc_row - segment_start).ok()?;
        (segment.source_line_at(rel, self.viewport_width) == Some(0))
            .then_some(segment.tool_id.as_deref())
            .flatten()
    }

    pub fn handle_click(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        let doc_row = u32::from(row.saturating_sub(area.y)) + u32::from(self.scroll_top);
        let width = self.viewport_width;
        // Both fallbacks toggle thinking: a row past the cached segments
        // belongs to the still-streaming indicator, and a segment without a
        // tool_id is a finished message's text.
        let Some((_, seg, seg_start)) = self.cache.segment_at_row(doc_row, width) else {
            return self.try_toggle_collapsed_thinking(doc_row, width);
        };
        let rel = u16::try_from(doc_row - seg_start).unwrap_or_else(|_| u16::MAX);
        let source_line = seg.source_line_at(rel, width);
        if let Some(MessageAction::ToggleCompaction(id)) = source_line
            .and_then(|line| seg.message_action(line))
            .cloned()
        {
            return self.toggle_compaction(&id);
        }
        let Some(tool_id) = seg.tool_id.clone() else {
            let msg_idx = seg.msg_index;
            return self.try_toggle_cached_thinking(msg_idx, width);
        };
        let truncation = seg.truncation;
        if let Some(action) = source_line.and_then(|line| seg.truncation_action(line)) {
            return self.toggle_tool_section(&tool_id, action);
        }

        if self.tool_in_progress(&tool_id) && self.live_bufs.contains_key(&tool_id) {
            self.auto_scroll = false;
            let buf_row = if self.has_snapshot(&tool_id) {
                source_line.map_or(0, |line| seg.buf_row(line))
            } else {
                0
            };
            if let Some(handle) = &self.lua_event_handle {
                handle.request_click(tool_id.clone(), buf_row);
            }
            return true;
        }

        if self.has_snapshot(&tool_id) {
            self.auto_scroll = false;
            let buf_row = source_line.map_or(0, |line| seg.buf_row(line));
            // Recorded even when the warm path serves the click: theme
            // rebake and session restore replay the full sequence.
            self.lua_clicks
                .entry(tool_id.clone())
                .or_default()
                .push(buf_row);
            let item = self.lua_restore_item(&tool_id).map(|mut item| {
                item.clicks.clone_from(&self.lua_clicks[&tool_id]);
                item
            });
            let (Some(eh), Some(tx)) =
                (self.lua_event_handle.clone(), self.restore_event_tx.clone())
            else {
                return true;
            };
            // Watching the buf means a runtime-side warm click would be
            // visible here, so try the fast path; the fallback item lets
            // the runtime degrade to restore+replay if its cache is cold.
            // Without the buf only a fresh restore can show the result.
            match (self.watching(&tool_id), item) {
                (true, Some(item)) => {
                    eh.request_click_with_fallback(tool_id.clone(), buf_row, item, tx);
                }
                (true, None) => eh.request_click(tool_id.clone(), buf_row),
                (false, Some(item)) => eh.request_restore(item, tx),
                (false, None) => {}
            }
            return true;
        }

        let exp = self
            .expanded_tools
            .get(&tool_id)
            .copied()
            .unwrap_or_else(SectionFlags::default);
        if !truncation.any() && !exp.any() {
            return false;
        }
        self.toggle_tool_expansion(&tool_id, truncation)
    }

    pub fn on_mouse(&mut self, column: u16, row: u16) {
        self.mascot.on_mouse(column, row);
    }

    fn toggle_compaction(&mut self, id: &str) -> bool {
        if !self.expanded_compactions.remove(id) {
            self.expanded_compactions.insert(id.to_owned());
        }
        self.cache.invalidate_from_msg_count();
        self.rebuild_line_cache();
        self.auto_scroll = false;
        true
    }

    fn toggle_tool_section(&mut self, tool_id: &str, section: SectionFlags) -> bool {
        if !section.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let (old_start, old_height) = self.segment_position(&tool_id);
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if section.script {
            entry.script = !entry.script;
        } else if section.output {
            entry.output = !entry.output;
        } else {
            entry.details = !entry.details;
        }
        self.rebuild_expanded_tool(&tool_id);
        self.preserve_anchor(old_start, old_height, &tool_id);
        self.auto_scroll = false;
        true
    }

    fn toggle_tool_expansion(&mut self, tool_id: &str, truncation: SectionFlags) -> bool {
        let tool_id = tool_id.to_owned();
        let (old_start, old_height) = self.segment_position(&tool_id);
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if truncation.output || entry.output {
            entry.output = !entry.output;
        } else if truncation.details || entry.details {
            entry.details = !entry.details;
        } else if truncation.script || entry.script {
            entry.script = !entry.script;
        } else {
            return false;
        }
        self.rebuild_expanded_tool(&tool_id);
        self.preserve_anchor(old_start, old_height, &tool_id);
        self.auto_scroll = false;
        true
    }

    #[cfg(test)]
    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        self.handle_click(row, area)
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(parent_idx) = self.cache.find_by_tool_id(parent_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks, parent_idx);
            }
        } else {
            self.rebuild_tool_segment(tool_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        msg.tool_output.as_deref()?.owned_instructions()
    }

    pub fn is_animating(&self) -> bool {
        self.in_progress_count() > 0
            || self.streaming_thinking.is_animating()
            || self.streaming_text.is_animating()
            || self.show_idle_splash()
            || self.accent.is_animating()
            || !self.live_bufs.is_empty()
            || self.streaming_thinking_collapsed()
            || self.is_restoring()
            || self.hl_worker.has_pending_results()
            || self
                .cache
                .segments()
                .iter()
                .any(Segment::has_pending_highlight)
    }

    fn streaming_thinking_collapsed(&self) -> bool {
        self.thinking_collapsed && !self.streaming_thinking.is_empty()
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool, _is_working: bool) {
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let theme_changed = self.theme_generation != theme_gen;
        let width_changed = self.viewport_width != width || theme_changed;
        if width_changed {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
        }
        if theme_changed {
            self.rebake_stale_snapshots(theme_gen);
        }

        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            let theme = theme::current();
            if self.mascot.enabled() && area.height > 18 {
                self.mascot.tick(area);
                self.mascot.render(area, frame.buffer_mut(), &theme, accent);
            } else {
                self.idle_splash.render(area, frame.buffer_mut(), accent);
            }
            return;
        }

        if width_changed {
            self.cache.invalidate_from_msg_count();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
            );
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }
        self.drain_highlights();
        self.poll_live_bufs();
        if !has_selection {
            self.drain_restore_backlog();
        }
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 {
            self.update_spinners();
        }

        let cached_count = self.cache.len();
        let spacer_lines: [Line<'static>; 1] = [Line::default()];
        let mut streaming_heights: Vec<u16> = Vec::new();

        let thinking_collapsed = self.streaming_thinking_collapsed();
        let collapsed_thinking_lines = if thinking_collapsed {
            Self::build_streaming_collapsed_lines(&self.streaming_thinking)
        } else {
            Vec::new()
        };

        if thinking_collapsed {
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights
                .push(u16::try_from(collapsed_thinking_lines.len()).unwrap_or_else(|_| u16::MAX));
        } else if !self.streaming_thinking.is_empty() {
            let h = self.streaming_thinking.height(width);
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights.push(h);
        }

        if !self.streaming_text.is_empty() {
            let h = self.streaming_text.height(width);
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights.push(h);
        }

        let cached_height = self.cache.total_height(width);
        let streaming_sum: u32 = streaming_heights.iter().map(|&h| u32::from(h)).sum();
        let total_lines: u16 = crate::cast::u32_to_u16(cached_height + streaming_sum);
        self.last_total_lines = total_lines;
        let max_scroll = total_lines.saturating_sub(self.viewport_height);
        self.scroll_top = self.scroll_top.min(max_scroll);
        if !has_selection {
            if self.scroll_top >= max_scroll {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                let diff = max_scroll.saturating_sub(self.scroll_top);
                if diff > 0 {
                    let step = diff.div_ceil(4).max(1);
                    self.scroll_top = self.scroll_top.saturating_add(step).min(max_scroll);
                }
            }
        }

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(self.scroll_top, viewport);

        for (i, seg) in self.cache.segments().iter().enumerate() {
            if cursor.past_bottom() {
                break;
            }
            let h = seg.height(width);
            let highlight = self.highlight_segment == Some(i);
            if let Some(term_img) = seg.image() {
                cursor.render_image(&term_img.protocol, h, seg.surface(), frame);
            } else {
                cursor.render(seg.lines(), h, None, seg.surface(), highlight, frame);
            }
        }

        let mut height_idx = 0usize;
        let streamed: [(&StreamingContent, bool); 2] = [
            (&self.streaming_thinking, thinking_collapsed),
            (&self.streaming_text, false),
        ];
        for (sc, collapsed) in streamed {
            if sc.is_empty() || height_idx >= streaming_heights.len() || cursor.past_bottom() {
                continue;
            }
            if cached_count > 0 || height_idx > 0 {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                cursor.render(&spacer_lines, h, None, Surface::Plain, false, frame);
            }
            if height_idx < streaming_heights.len() {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                if collapsed {
                    cursor.render(
                        &collapsed_thinking_lines,
                        h,
                        None,
                        Surface::Plain,
                        false,
                        frame,
                    );
                } else {
                    cursor.render(sc.cached_lines(), h, None, Surface::Plain, false, frame);
                }
            }
        }

        if let Some(pp) = self.prompt_progress
            && pp.total > 0
        {
            let ratio = f64::from(pp.processed) / f64::from(pp.total);
            let bar_width = crate::cast::f64_to_u16((f64::from(width) * 0.1).round());
            let label = " Processing ";
            let label_width = u16::try_from(label.len()).unwrap_or_else(|_| u16::MAX);
            let total_width = label_width + bar_width;
            let bar_x = area.x + width.saturating_sub(total_width);
            let bar_y = area.y + area.height.saturating_sub(1);
            let bar_area = Rect::new(bar_x, bar_y, total_width, 1);
            crate::components::progress_bar::render(
                frame,
                bar_area,
                &crate::components::progress_bar::ProgressBarConfig {
                    ratio,
                    style: theme::current().progress_bar,
                    cache_ratio: f64::from(pp.cache) / f64::from(pp.total),
                    cache_style: Style::new().fg(Color::Green),
                    label: Some(label),
                    label_style: Some(theme::current().tool_dim),
                    bar_width,
                },
            );
        }

        if total_lines > area.height {
            let is_active = self.in_progress_count() > 0
                || !self.streaming_text.is_empty()
                || !self.streaming_thinking.is_empty()
                || !self.live_bufs.is_empty();
            let style = is_active.then_some(theme::current().spinner);
            render_vertical_scrollbar(frame, area, total_lines, self.scroll_top, style);
        }

        self.jump_to_bottom_popup = None;
        let distance = max_scroll.saturating_sub(self.scroll_top);
        let show_popup = max_scroll > 0 && !self.auto_scroll && distance > JUMP_TO_BOTTOM_THRESHOLD;
        if show_popup {
            self.render_jump_to_bottom_popup(frame, viewport);
        }
    }

    fn render_jump_to_bottom_popup(&mut self, frame: &mut Frame, area: Rect) {
        let text_style = theme::current().accent;
        let keybind_style = theme::current().keybind_key;
        let line = Line::from(vec![
            Span::styled(JUMP_TO_BOTTOM_TEXT, text_style),
            Span::raw(JUMP_TO_BOTTOM_KEY_GAP),
            Span::styled(key::CHAT_SCROLL_BOTTOM.label, keybind_style),
        ]);
        let text_width = u16::try_from(line.width()).unwrap_or_else(|_| u16::MAX);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::current().panel_border)
            .padding(Padding::horizontal(1))
            .style(Style::new().bg(theme::current().background));
        let dummy_area = Rect::new(0, 0, u16::MAX, JUMP_TO_BOTTOM_POPUP_HEIGHT);
        let chrome_width = u16::MAX - block.inner(dummy_area).width;
        let width = text_width.saturating_add(chrome_width);

        let min_height = JUMP_TO_BOTTOM_POPUP_HEIGHT + JUMP_TO_BOTTOM_POPUP_BOTTOM_MARGIN;
        if text_width == 0 || width > area.width || area.height < min_height {
            return;
        }
        let x = area.x + (area.width - width) / 2;
        let y = area
            .bottom()
            .saturating_sub(JUMP_TO_BOTTOM_POPUP_HEIGHT + JUMP_TO_BOTTOM_POPUP_BOTTOM_MARGIN);
        let popup_area = Rect::new(x, y, width, JUMP_TO_BOTTOM_POPUP_HEIGHT);
        self.jump_to_bottom_popup = Some(popup_area);

        let inner = block.inner(popup_area);
        frame.render_widget(Clear, popup_area);
        frame.render_widget(block, popup_area);
        frame.render_widget(Paragraph::new(line), inner);
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines.saturating_sub(self.viewport_height)
    }

    pub fn scroll_top(&self) -> u16 {
        self.scroll_top
    }

    pub fn total_lines(&self) -> u16 {
        self.last_total_lines
    }

    pub fn scroll_info(&self, viewport_height: u16) -> Option<ScrollInfo> {
        let content_len = self.last_total_lines;
        if content_len > viewport_height {
            let max_scroll = content_len.saturating_sub(viewport_height);
            Some(ScrollInfo {
                content_len,
                position: self.scroll_top.min(max_scroll),
            })
        } else {
            None
        }
    }

    pub fn set_scroll_top(&mut self, y: u16) {
        self.scroll_top = y;
        self.auto_scroll = false;
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        let width = self.viewport_width;
        self.cache
            .segments()
            .iter()
            .map(|s| s.height(width))
            .collect()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.cache.search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn tool_in_progress(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rev()
            .find_map(|m| match &m.role {
                DisplayRole::Tool(t) if t.id == tool_id => Some(t.status),
                _ => None,
            })
            .is_some_and(|s| s == ToolStatus::InProgress)
    }

    fn watching(&self, tool_id: &str) -> bool {
        self.watched_bufs.iter().any(|(id, _)| id == tool_id)
    }

    fn stop_watching(&mut self, tool_id: &str) {
        self.watched_bufs.retain(|(id, _)| id != tool_id);
    }

    /// Moves a finished tool's live buf to the watched set, flushing any
    /// last dirty lines. Called on completion and on cancellation, so
    /// `live_bufs` never leaks entries that keep `is_animating` true.
    fn retire_live_buf(&mut self, id: &str) {
        let Some(buf) = self.live_bufs.remove(id) else {
            return;
        };
        if let Some(lines) = buf.read_if_dirty() {
            self.store_snapshot(id, BufferSnapshot::from_arc(lines), false, None);
        }
        self.watched_bufs.push_back((id.to_owned(), buf));
        if self.watched_bufs.len() > WARM_TOOL_CAP {
            self.watched_bufs.pop_front();
        }
    }

    fn has_snapshot(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .is_some_and(|m| m.render_snapshot.is_some())
    }

    fn lua_restore_item(&self, tool_id: &str) -> Option<n00n_lua::RestoreItem> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        crate::chat::restore_item_for(msg, self.tool_output_lines, self.theme_generation)
    }

    /// Re-restores every snapshot still painted with old-theme colors.
    /// Replies carry a generation so stale ones can't overwrite fresher colors.
    fn rebake_stale_snapshots(&mut self, current_gen: u64) {
        let (Some(eh), Some(tx)) = (self.lua_event_handle.clone(), self.restore_event_tx.clone())
        else {
            return;
        };
        self.rebake_requested.retain(|_, g| *g >= current_gen);
        let tol = self.tool_output_lines;
        let mut requested = Vec::new();
        for msg in &self.messages {
            let DisplayRole::Tool(role) = &msg.role else {
                continue;
            };
            if !self.should_request_rebake(
                &role.id,
                msg.snapshot_is_stale(current_gen),
                current_gen,
            ) {
                continue;
            }
            if let Some(mut item) = crate::chat::restore_item_for(msg, tol, current_gen) {
                if let Some(clicks) = self.lua_clicks.get(&role.id) {
                    item.clicks.clone_from(clicks);
                }
                eh.request_restore(item, tx.clone());
                requested.push(role.id.clone());
            }
        }
        for id in requested {
            // The watched buf still carries old-theme lines; clicks in
            // the rebake window must go through restore, not warm.
            self.stop_watching(&id);
            self.rebake_requested.insert(id, current_gen);
        }
    }

    fn should_request_rebake(&self, tool_id: &str, stale: bool, current_gen: u64) -> bool {
        stale && self.rebake_requested.get(tool_id) != Some(&current_gen)
    }

    /// Live snapshots (`None`) get the panel's current generation.
    /// Re-bake replies are monotonic: drop if something newer landed.
    fn resolve_snapshot_gen(&self, tool_id: &str, incoming: Option<u64>) -> Option<u64> {
        let Some(incoming_gen) = incoming else {
            return Some(self.theme_generation);
        };
        match self.current_snapshot_gen(tool_id) {
            Some(applied) if applied > incoming_gen => None,
            _ => Some(incoming_gen),
        }
    }

    fn current_snapshot_gen(&self, tool_id: &str) -> Option<u64> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .map(|m| m.snapshot_theme_gen)
    }

    fn store_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        is_header: bool,
        theme_gen: Option<u64>,
    ) {
        let (old_start, old_height) = self.segment_position(tool_id);
        let anchor_auto_scroll = self.auto_scroll && self.last_total_lines > self.viewport_height;
        if theme_gen.is_some() {
            // A generation only comes with restore replies. The restore
            // superseded the old live view (and evicted the runtime's
            // warm handle), so its buf must not overwrite this snapshot.
            self.stop_watching(tool_id);
        }
        let Some(applied_gen) = self.resolve_snapshot_gen(tool_id, theme_gen) else {
            return;
        };
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            if is_header {
                msg.text = snapshot.first_line_text();
                msg.render_header = Some(snapshot);
            } else {
                msg.render_snapshot = Some(snapshot);
            }
            msg.snapshot_theme_gen = applied_gen;
            self.rebuild_tool_segment(tool_id);
            let (_, new_height) = self.segment_position(tool_id);
            if anchor_auto_scroll {
                self.shift_scroll_for_height_change(old_height, new_height);
            } else {
                self.preserve_anchor(old_start, old_height, tool_id);
            }
        }
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            started_at: self.started_at,
            width: self.viewport_width,
            tool_output_lines: &self.tool_output_lines,
        }
    }

    pub fn register_live_buf(&mut self, id: String, body: Arc<SharedBuf>) {
        self.live_bufs.insert(id, body);
    }

    fn poll_live_bufs(&mut self) {
        let dirty: Vec<_> = self
            .live_bufs
            .iter()
            .chain(self.watched_bufs.iter().map(|(id, buf)| (id, buf)))
            .filter_map(|(id, buf)| buf.read_if_dirty().map(|lines| (id.clone(), lines)))
            .collect();
        for (tool_id, lines) in dirty {
            self.store_snapshot(&tool_id, BufferSnapshot::from_arc(lines), false, None);
        }
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: SectionFlags,
    ) -> ToolLines {
        let content_width = Surface::Tool.content_width(rctx.width);
        let tool_ctx = RenderCtx {
            started_at: rctx.started_at,
            width: content_width,
            tool_output_lines: rctx.tool_output_lines,
        };
        let mut tl = build_tool_lines(msg, status, &tool_ctx, exp);
        if let Some(ts) = &msg.timestamp
            && !tl.lines.is_empty()
        {
            append_right_info(
                &mut tl.lines[0],
                msg.turn_usage.as_deref(),
                Some(ts),
                content_width,
            );
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.thinking_collapsed = true;
        msg.annotation = self
            .thinking_started
            .take()
            .map(|started| format_thinking_duration(started.elapsed()));
        self.thinking_collapsed = false;
        self.messages.push(msg);
    }

    fn build_streaming_collapsed_lines(
        streaming_thinking: &StreamingContent,
    ) -> Vec<Line<'static>> {
        thinking_indicator(streaming_thinking.line_count(), None)
    }

    fn build_cached_thinking_indicator(text: &str, duration: Option<&str>) -> Vec<Line<'static>> {
        thinking_indicator(logical_line_count(text), duration)
    }

    fn try_toggle_collapsed_thinking(&mut self, doc_row: u32, width: u16) -> bool {
        if !self.streaming_thinking_collapsed() {
            return false;
        }
        let cached_height = self.cache.total_height(width);
        let spacer = u32::from(self.cache.len() > 0);
        let thinking_start = cached_height + spacer;
        let height =
            u32::try_from(Self::build_streaming_collapsed_lines(&self.streaming_thinking).len())
                .unwrap_or_else(|_| u32::MAX);
        if doc_row >= thinking_start && doc_row < thinking_start + height {
            self.thinking_collapsed = false;
            self.auto_scroll = false;
            return true;
        }
        false
    }

    fn try_toggle_cached_thinking(&mut self, msg_idx: Option<usize>, width: u16) -> bool {
        let Some(idx) = msg_idx else { return false };
        let Some(msg) = self.messages.get_mut(idx) else {
            return false;
        };
        if !matches!(msg.role, DisplayRole::Thinking) {
            return false;
        }
        msg.thinking_collapsed = !msg.thinking_collapsed;
        self.rebuild_thinking_segment(idx, width);
        self.auto_scroll = false;
        true
    }

    fn rebuild_thinking_segment(&mut self, msg_idx: usize, width: u16) {
        let Some((text, collapsed, duration)) = self
            .messages
            .get(msg_idx)
            .map(|m| (m.text.clone(), m.thinking_collapsed, m.annotation.clone()))
        else {
            return;
        };
        let lines = if collapsed {
            Self::build_cached_thinking_indicator(&text, duration.as_deref())
        } else {
            let style = thinking_style();
            text_to_lines(
                &text,
                style.prefix,
                style.text_style,
                style.prefix_style,
                width,
                None,
            )
        };
        let search_text = format!("thinking> {text}");
        let seg_idx = self
            .cache
            .segments()
            .iter()
            .position(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none());
        let Some(seg_idx) = seg_idx else { return };
        if let Some(seg) = self.cache.get_mut(seg_idx) {
            seg.set_lines(lines);
            seg.search_text = search_text;
        }
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            seg.update_spinners(&spinner_span);
        }
    }

    fn drain_highlights(&mut self) {
        for _ in 0..MAX_HIGHLIGHT_RESULTS_PER_FRAME {
            let Some(result) = self.hl_worker.try_recv() else {
                break;
            };
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(result.lines);
            }
        }
        let mut attempted = 0;
        for segment in self.cache.segments_mut() {
            if attempted >= MAX_HIGHLIGHT_RETRIES_PER_FRAME {
                break;
            }
            attempted += usize::from(segment.retry_highlight(&self.hl_worker));
        }
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            unreachable!()
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_else(SectionFlags::default);
        let rctx = self.rctx();
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp);

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(n00n_agent::ToolOutput::owned_instructions);

        if let Some(seg) = self.cache.get_mut(seg_idx) {
            seg.search_text.clone_from(&tl.search_text);
            seg.update_with_reuse(tl, &self.hl_worker);
        }

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx);
        }
    }

    fn build_segments_for_msg(&self, msg: &DisplayMessage, msg_index: usize) -> Vec<Segment> {
        if let Some(DisplayMetadata::Compaction(compaction)) = &msg.metadata {
            return vec![build_compaction_segment(
                compaction,
                msg_index,
                &self.expanded_compactions,
                &self.tool_output_lines,
            )];
        }
        if let DisplayRole::Tool(t) = &msg.role {
            let exp = self
                .expanded_tools
                .get(&t.id)
                .copied()
                .unwrap_or_else(SectionFlags::default);
            let status = t.status;
            let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
            let id = t.id.clone();
            let mut seg = Segment::with_tool(id.clone());
            seg.search_text.clone_from(&tl.search_text);
            seg.apply_highlight(tl, &self.hl_worker);
            let mut out = vec![seg];
            let blocks = msg
                .tool_output
                .as_deref()
                .and_then(n00n_agent::ToolOutput::owned_instructions);
            if let Some(blocks) = blocks
                && !blocks.is_empty()
            {
                let inst_id = segment::instruction_id(&id);
                let exp = self
                    .expanded_tools
                    .get(&inst_id)
                    .copied()
                    .unwrap_or_else(SectionFlags::default);
                let tl = build_instructions_lines(&blocks, self.viewport_width, exp.output);
                let mut inst_seg = Segment::with_instructions(id);
                inst_seg.search_text.clone_from(&tl.search_text);
                inst_seg.apply_highlight(tl, &self.hl_worker);
                out.push(Segment::spacer());
                out.push(inst_seg);
            }
            return out;
        }
        if matches!(&msg.role, DisplayRole::Thinking) && msg.thinking_collapsed {
            let text = msg.text.clone();
            let lines = Self::build_cached_thinking_indicator(&text, msg.annotation.as_deref());
            let search_text = format!("thinking> {text}");
            return vec![Segment::with_lines(
                lines,
                search_text,
                Some(text),
                0,
                Some(msg_index),
            )];
        }
        let style = match &msg.role {
            DisplayRole::User => user_style(),
            DisplayRole::Assistant => assistant_style(),
            DisplayRole::Thinking => thinking_style(),
            DisplayRole::Control => control_style(),
            DisplayRole::Error => error_style(),
            DisplayRole::Done => done_style(),
            DisplayRole::Tool(_) => unreachable!(),
        };
        let prefix = if msg.plan_path.is_some() {
            ""
        } else {
            style.prefix
        };
        let surface = match msg.role {
            DisplayRole::User => Surface::User,
            DisplayRole::Assistant => Surface::Assistant,
            DisplayRole::Control => Surface::Control,
            _ => Surface::Plain,
        };
        let base_width = surface.content_width(self.viewport_width).max(1);
        let content_width = base_width
            .saturating_sub(u16::try_from(prefix.width()).unwrap_or_else(|_| u16::MAX))
            .max(1);
        let image_width = base_width;
        let picker_ref = &*self.picker;
        let mut lines = if style.use_markdown {
            text_to_lines(
                &msg.text,
                prefix,
                style.text_style,
                style.prefix_style,
                content_width,
                style.max_line_bytes,
            )
        } else {
            plain_lines(&msg.text, prefix, style.text_style, style.prefix_style)
        };
        if let Some(pp) = &msg.plan_path {
            if msg.text.is_empty() {
                lines.clear();
            } else {
                let rule = hr_line(self.viewport_width, theme::current().plan_rule);
                lines.insert(0, rule.clone());
                lines.push(rule);
            }
            if !msg.text.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                pp.to_owned(),
                theme::current().plan_path,
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "{} to open in editor ($VISUAL / $EDITOR)",
                    key::OPEN_EDITOR.label
                ),
                theme::current().tool_dim,
            )));
        }
        let prefix_width = u16::try_from(prefix.width()).unwrap_or_else(|_| u16::MAX);
        let search_text = format!("{}> {}", role_name(&msg.role), msg.text);
        let mut segment = Segment::with_lines(
            lines,
            search_text,
            Some(msg.text.clone()),
            prefix_width,
            Some(msg_index),
        );
        segment.set_surface(surface);
        let mut out = Vec::new();
        if !msg.text.is_empty() || msg.plan_path.is_some() {
            out.push(segment);
        }
        for (idx, source) in msg.images.iter().enumerate() {
            if idx > 0 || !out.is_empty() {
                out.push(Segment::spacer());
            }
            out.push(Segment::with_image(
                source,
                picker_ref,
                image_width,
                surface,
                Some(msg_index),
            ));
        }
        out
    }

    fn rebuild_line_cache(&mut self) {
        let _start = self.cache.msg_count();
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let Some(DisplayMetadata::Compaction(compaction)) = &msg.metadata {
                self.cache.push_spacer_if_needed();
                self.cache.push(build_compaction_segment(
                    compaction,
                    i,
                    &self.expanded_compactions,
                    &self.tool_output_lines,
                ));
            } else if let DisplayRole::Tool(t) = &msg.role {
                let exp = self
                    .expanded_tools
                    .get(&t.id)
                    .copied()
                    .unwrap_or_else(SectionFlags::default);
                let status = t.status;
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp);
                let id = t.id.clone();
                let search_text = tl.search_text.clone();
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_tool(id.clone());
                seg.search_text = search_text;
                seg.raw_text = Some(msg.text.clone());
                seg.apply_highlight(tl, &self.hl_worker);
                self.cache.push(seg);

                let blocks = msg
                    .tool_output
                    .as_deref()
                    .and_then(n00n_agent::ToolOutput::owned_instructions);
                if let Some(blocks) = blocks {
                    let last_idx = self.cache.len().saturating_sub(1);
                    self.upsert_instruction_segment(&id, &blocks, last_idx);
                }
            } else {
                if matches!(&msg.role, DisplayRole::Thinking) && msg.thinking_collapsed {
                    let text = msg.text.clone();
                    let lines =
                        Self::build_cached_thinking_indicator(&text, msg.annotation.as_deref());
                    let search_text = format!("thinking> {text}");
                    self.cache.push_spacer_if_needed();
                    self.cache.push(Segment::with_lines(
                        lines,
                        search_text,
                        Some(text),
                        0,
                        Some(i),
                    ));
                    continue;
                }
                let style = match &msg.role {
                    DisplayRole::User => user_style(),
                    DisplayRole::Assistant => assistant_style(),
                    DisplayRole::Thinking => thinking_style(),
                    DisplayRole::Control => control_style(),
                    DisplayRole::Error => error_style(),
                    DisplayRole::Done => done_style(),
                    DisplayRole::Tool(_) => unreachable!(),
                };
                let prefix = if msg.plan_path.is_some() {
                    ""
                } else {
                    style.prefix
                };
                let surface = match msg.role {
                    DisplayRole::User => Surface::User,
                    DisplayRole::Assistant => Surface::Assistant,
                    DisplayRole::Control => Surface::Control,
                    _ => Surface::Plain,
                };
                let base_width = surface.content_width(self.viewport_width).max(1);
                let content_width = base_width
                    .saturating_sub(u16::try_from(prefix.width()).unwrap_or_else(|_| u16::MAX))
                    .max(1);
                let image_width = base_width;
                let picker_ref = &*self.picker;
                let mut lines = if style.use_markdown {
                    text_to_lines(
                        &msg.text,
                        prefix,
                        style.text_style,
                        style.prefix_style,
                        content_width,
                        style.max_line_bytes,
                    )
                } else {
                    plain_lines(&msg.text, prefix, style.text_style, style.prefix_style)
                };
                if let Some(pp) = &msg.plan_path {
                    if msg.text.is_empty() {
                        lines.clear();
                    } else {
                        let rule = hr_line(self.viewport_width, theme::current().plan_rule);
                        lines.insert(0, rule.clone());
                        lines.push(rule);
                    }
                    if !msg.text.is_empty() {
                        lines.push(Line::from(""));
                    }
                    lines.push(Line::from(Span::styled(
                        pp.to_owned(),
                        theme::current().plan_path,
                    )));
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} to open in editor ($VISUAL / $EDITOR)",
                            key::OPEN_EDITOR.label
                        ),
                        theme::current().tool_dim,
                    )));
                }

                let prefix_width = u16::try_from(prefix.width()).unwrap_or_else(|_| u16::MAX);
                let search_text = format!("{}> {}", role_name(&msg.role), msg.text);
                self.cache.push_spacer_if_needed();
                if !msg.text.is_empty() || msg.plan_path.is_some() || msg.images.is_empty() {
                    let mut segment = Segment::with_lines(
                        lines,
                        search_text,
                        Some(msg.text.clone()),
                        prefix_width,
                        Some(i),
                    );
                    segment.set_surface(surface);
                    self.cache.push(segment);
                }
                for (idx, source) in msg.images.iter().enumerate() {
                    if idx > 0 || !msg.text.is_empty() {
                        self.cache.push(Segment::spacer());
                    }
                    self.cache.push(Segment::with_image(
                        source,
                        picker_ref,
                        image_width,
                        surface,
                        Some(i),
                    ));
                }
            }
        }
        self.cache.mark_built(self.messages.len());
    }
}

fn build_compaction_segment(
    compaction: &CompactionDisplay,
    msg_index: usize,
    expanded: &HashSet<String>,
    tool_output_lines: &ToolOutputLines,
) -> Segment {
    let mut lines = Vec::new();
    let mut actions = Vec::new();
    append_compaction_lines(
        compaction,
        expanded,
        tool_output_lines,
        0,
        &mut lines,
        &mut actions,
    );
    let search_text = format!(
        "compaction> depth {} {} messages {}",
        compaction.depth,
        compaction.message_count,
        compaction_summary_text(compaction)
    );
    Segment::with_compaction(lines, search_text, msg_index, actions)
}

fn compaction_summary_text(compaction: &CompactionDisplay) -> &str {
    match compaction.summary.as_deref() {
        Some(summary) => summary,
        None => "Summary unavailable",
    }
}

fn append_compaction_lines(
    compaction: &CompactionDisplay,
    expanded: &HashSet<String>,
    tool_output_lines: &ToolOutputLines,
    indent: usize,
    lines: &mut Vec<Line<'static>>,
    actions: &mut Vec<(usize, String)>,
) {
    let is_expanded = expanded.contains(&compaction.id);
    actions.push((lines.len(), compaction.id.clone()));
    let marker = if is_expanded { '▾' } else { '▸' };
    let count_label = if compaction.message_count == 1 {
        "message"
    } else {
        "messages"
    };
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(
            format!(
                "{marker} Compaction · depth {} · {} {count_label}",
                compaction.depth, compaction.message_count
            ),
            theme::current().tool_dim,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(indent + 2)),
        Span::styled("Summary: ", theme::current().assistant_prefix),
        Span::styled(
            compaction_summary_text(compaction).to_owned(),
            theme::current().assistant,
        ),
    ]));
    if !is_expanded {
        return;
    }
    for entry in &compaction.entries {
        if let Some(DisplayMetadata::Compaction(child)) = &entry.metadata {
            append_compaction_lines(
                child,
                expanded,
                tool_output_lines,
                indent + 2,
                lines,
                actions,
            );
        } else {
            append_compaction_entry(entry, tool_output_lines, indent + 2, lines);
        }
    }
}

fn append_compaction_entry(
    message: &DisplayMessage,
    tool_output_lines: &ToolOutputLines,
    indent: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let label = role_name(&message.role);
    let mut text_lines = message.text.lines();
    if let Some(first) = text_lines.next() {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(format!("{label}> "), theme::current().tool_dim),
            Span::raw(first.to_owned()),
        ]));
        for line in text_lines {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent + label.len() + 2)),
                Span::raw(line.to_owned()),
            ]));
        }
    }
    if let Some(output) = message.tool_output.as_deref() {
        let output = output.as_display_text();
        let tool_name = message.role.tool_name().unwrap_or_else(|| "");
        let truncated = truncate_output(&output, tool_output_lines.get(tool_name));
        if !truncated.kept.is_empty() && !message.text.contains(truncated.kept.as_ref()) {
            for line in truncated.kept.lines() {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(indent + label.len() + 2)),
                    Span::raw(line.to_owned()),
                ]));
            }
        }
    }
    if !message.images.is_empty() {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(
                format!("{label}> [{} image(s)]", message.images.len()),
                theme::current().tool_dim,
            ),
        ]));
    }
}

fn thinking_indicator(line_count: usize, duration: Option<&str>) -> Vec<Line<'static>> {
    let theme = theme::current();
    let label = duration.map_or_else(
        || "Thinking".to_owned(),
        |elapsed| format!("Thought for {elapsed}"),
    );
    vec![Line::from(vec![
        Span::styled("  › ", theme.thinking),
        Span::styled(label, theme.thinking),
        Span::styled(format!(" · {line_count} lines"), theme.tool_dim),
        Span::styled("  ›", theme.tool_dim),
    ])]
}

fn format_thinking_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs().max(1);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

fn role_name(role: &DisplayRole) -> &'static str {
    match role {
        DisplayRole::User => "you",
        DisplayRole::Assistant => "n00n",
        DisplayRole::Thinking => "thinking",
        DisplayRole::Control => "control",
        DisplayRole::Error => "error",
        DisplayRole::Done => "done",
        DisplayRole::Tool(_) => "tool",
    }
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + 1
    }
}
