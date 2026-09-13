use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::shell::parse_shell_prefix;
use crate::cast;
use crate::highlight;
use crate::keymap::KeyAction;
use crate::text_buffer::{EditResult, TextBuffer};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use n00n_storage::input_history::InputHistory;
use std::mem;

use n00n_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::scrollbar::{ScrollInfo, render_vertical_scrollbar};
use super::{apply_scroll_delta, visual_line_count};
use crate::selection::LineBreaks;

const MAX_INPUT_LINES: u16 = 20;
const NEWLINE_PAD: &str = "  ";
const PREFIX_WIDTH: u16 = 2;
const PLACEHOLDER_SUGGESTIONS: &[&str] = &[
    "research how something works",
    "fix a bug",
    "add a feature",
    "add a database migration",
    "create a helm chart",
    "simplify some function",
    "remove trivial comments",
    "analyze data",
    "profile and improve performance",
    "add tests",
    "add benchmarks",
    "refactor a module",
    "remove dead code",
];

pub enum InputAction {
    OpenFilePicker,
    PaletteSync(String),
    None,
}

pub struct Submission {
    pub text: String,
    pub images: Vec<ImageSource>,
    pub control: bool,
}

impl Submission {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            images: Vec::new(),
            control: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }
}

/// A stashed composer draft: text, attached images, and paste chips move
/// out of the way together and come back together on restore.
struct StashedDraft {
    text: String,
    images: Vec<ImageSource>,
    pastes: Vec<String>,
}

/// Result of a `stash_toggle` press.
pub enum StashOutcome {
    /// Non-empty composer moved into the stash.
    Stashed,
    /// Stash contents restored into an empty composer.
    Restored,
    /// Nothing in the composer and nothing stashed.
    NothingToStash,
    /// Composer non-empty but a stash already exists — refused rather than
    /// silently overwriting it.
    Occupied,
}

/// Inline reverse-history-search state (Ctrl+R). `saved` restores the
/// pre-search buffer on cancel; `matches` are history indices containing
/// `query`, oldest to newest; `pos` is the highlighted match.
pub struct HistorySearch {
    pub query: String,
    matches: Vec<usize>,
    pos: usize,
    saved: String,
}

/// Pastes longer than this collapse into a `[Pasted #N +L lines]` chip in
/// the composer; the full text expands back on submit.
const PASTE_COLLAPSE_LINES: usize = 5;

pub struct InputBox {
    pub(crate) buffer: TextBuffer,
    history: InputHistory,
    history_index: Option<usize>,
    draft: String,
    stash: Option<StashedDraft>,
    history_search: Option<HistorySearch>,
    pastes: Vec<String>,
    scroll_y: u16,
    follow_cursor: bool,
    placeholder_hint: &'static str,
    placeholder_index: usize,
    pending_images: Vec<ImageSource>,
    max_input_lines: u16,
    last_total_vl: u16,
    last_content_height: u16,
}

impl InputBox {
    /// Handles only unbound composer keys (printable chars, `@` trigger).
    /// Bound keys never reach here — `app` resolves them through the keymap
    /// and dispatches `edit_action`/`submit` directly.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        self.follow_cursor = true;

        if self.history_search.is_some() {
            if let KeyCode::Char(c) = key.code {
                // `Char`+Ctrl+Alt is `AltGr` — printable input, not a chord.
                let altgr = key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::ALT);
                if altgr
                    || !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    self.history_search_push(c);
                }
            }
            return InputAction::None;
        }

        if let KeyCode::Char('@') = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.char_before_cursor_is_whitespace_or_start()
        {
            return InputAction::OpenFilePicker;
        }

        match self.buffer.handle_key(key) {
            EditResult::Changed => InputAction::PaletteSync(self.buffer.value()),
            EditResult::Moved | EditResult::Ignored => InputAction::None,
        }
    }

    /// Execute a resolved editing-context action. `Changed` maps to
    /// `PaletteSync` so the command palette tracks the new text.
    pub fn edit_action(&mut self, action: KeyAction) -> InputAction {
        self.follow_cursor = true;
        // An editing key during history search accepts the shown match
        // first, then applies — the readline model.
        if self.history_search.is_some() {
            self.history_search_accept();
        }
        let changed = match action {
            KeyAction::InputUp => {
                if self.is_at_first_line() {
                    self.history_up();
                } else {
                    self.buffer.move_up();
                }
                false
            }
            KeyAction::InputDown => {
                if self.is_at_last_line() {
                    self.history_down();
                } else {
                    self.buffer.move_down();
                }
                false
            }
            KeyAction::CharLeft => {
                self.buffer.move_left();
                false
            }
            KeyAction::CharRight => {
                self.buffer.move_right();
                false
            }
            KeyAction::WordLeft => {
                self.buffer.move_word_left();
                false
            }
            KeyAction::WordRight => {
                self.buffer.move_word_right();
                false
            }
            KeyAction::LineStart => {
                self.buffer.move_home();
                false
            }
            KeyAction::LineEnd => {
                self.buffer.move_end();
                false
            }
            KeyAction::SelectCharLeft => {
                self.buffer.select_left();
                false
            }
            KeyAction::SelectCharRight => {
                self.buffer.select_right();
                false
            }
            KeyAction::SelectWordLeft => {
                self.buffer.select_word_left();
                false
            }
            KeyAction::SelectWordRight => {
                self.buffer.select_word_right();
                false
            }
            KeyAction::SelectLineStart => {
                self.buffer.select_home();
                false
            }
            KeyAction::SelectLineEnd => {
                self.buffer.select_end();
                false
            }
            // Shift+Up/Down select within the buffer only — they must not
            // touch history navigation the way plain arrows do.
            KeyAction::SelectUp => {
                self.buffer.select_up();
                false
            }
            KeyAction::SelectDown => {
                self.buffer.select_down();
                false
            }
            KeyAction::SelectAll => {
                self.buffer.select_all();
                false
            }
            KeyAction::DeleteCharBack => {
                self.buffer.remove_char();
                true
            }
            KeyAction::DeleteCharForward => {
                self.buffer.delete_char();
                true
            }
            KeyAction::DeleteWordBack => {
                self.buffer.remove_word_before_cursor();
                true
            }
            KeyAction::DeleteWordForward => {
                self.buffer.delete_word_after_cursor();
                true
            }
            KeyAction::KillLineEnd => {
                self.buffer.kill_to_end_of_line();
                true
            }
            KeyAction::KillLineStart => {
                self.buffer.kill_to_start_of_line();
                true
            }
            KeyAction::Yank => self.buffer.yank(),
            KeyAction::YankPop => self.buffer.yank_pop(),
            KeyAction::Undo => self.buffer.undo(),
            KeyAction::Redo => self.buffer.redo(),
            KeyAction::Newline => {
                self.buffer.add_line();
                true
            }
            _ => false,
        };
        if changed {
            InputAction::PaletteSync(self.buffer.value())
        } else {
            InputAction::None
        }
    }

    /// Stash a non-empty draft for later, or restore the stash when the
    /// composer is empty. An occupied stash is never overwritten — the
    /// caller flashes a hint instead.
    pub fn stash_toggle(&mut self) -> StashOutcome {
        self.history_search_cancel();
        if !self.buffer.value().trim().is_empty() || !self.pending_images.is_empty() {
            if self.stash.is_some() {
                return StashOutcome::Occupied;
            }
            self.stash = Some(StashedDraft {
                text: self.buffer.value(),
                images: mem::take(&mut self.pending_images),
                pastes: mem::take(&mut self.pastes),
            });
            self.discard();
            return StashOutcome::Stashed;
        }
        if let Some(stashed) = self.stash.take() {
            self.buffer.set_text(&stashed.text);
            self.pending_images = stashed.images;
            self.pastes = stashed.pastes;
            self.buffer.move_to_end();
            return StashOutcome::Restored;
        }
        StashOutcome::NothingToStash
    }

    pub fn history_search_active(&self) -> bool {
        self.history_search.is_some()
    }

    pub fn history_search_query(&self) -> Option<&str> {
        self.history_search.as_ref().map(|s| s.query.as_str())
    }

    pub fn start_history_search(&mut self) {
        if self.history_search.is_some() {
            return;
        }
        let saved = self.buffer.value();
        self.history_search = Some(HistorySearch {
            query: String::new(),
            matches: Vec::new(),
            pos: 0,
            saved,
        });
        self.search_apply();
    }

    pub fn history_search_push(&mut self, c: char) {
        if let Some(search) = &mut self.history_search {
            search.query.push(c);
        }
        self.search_apply();
    }

    pub fn history_search_backspace(&mut self) {
        if let Some(search) = &mut self.history_search {
            search.query.pop();
        }
        self.search_apply();
    }

    pub fn history_search_older(&mut self) {
        self.search_step(true);
    }

    pub fn history_search_newer(&mut self) {
        self.search_step(false);
    }

    /// Accept: keep the matched entry in the composer.
    pub fn history_search_accept(&mut self) {
        self.history_search = None;
        self.history_index = None;
        self.draft.clear();
    }

    /// Cancel: restore the buffer as it was before the search. The restore
    /// skips undo recording — the search itself is the recovery mechanism.
    pub fn history_search_cancel(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.buffer.set_text_silent(&search.saved);
            self.buffer.move_to_end();
        }
    }

    fn search_apply(&mut self) {
        let Some(search) = &self.history_search else {
            return;
        };
        let query = search.query.clone();
        let matches: Vec<usize> = (0..self.history.len())
            .filter(|&i| {
                self.history
                    .get(i)
                    .is_some_and(|entry| entry.contains(query.as_str()))
            })
            .collect();
        let Some(search) = &mut self.history_search else {
            return;
        };
        search.pos = matches.len().saturating_sub(1);
        search.matches = matches;
        self.search_show_current();
    }

    /// Step through `matches`: `older=true` moves toward older history
    /// entries (lower index), `false` toward newer ones.
    fn search_step(&mut self, older: bool) {
        let Some(search) = &mut self.history_search else {
            return;
        };
        let next = if older {
            search.pos.checked_sub(1)
        } else {
            search.pos.checked_add(1)
        };
        let Some(next) = next else {
            return;
        };
        if next >= search.matches.len() {
            return;
        }
        search.pos = next;
        self.search_show_current();
    }

    fn search_show_current(&mut self) {
        let Some(search) = &self.history_search else {
            return;
        };
        let Some(&idx) = search.matches.get(search.pos) else {
            return;
        };
        if let Some(entry) = self.history.get(idx) {
            let text = entry.to_string();
            self.buffer.set_text_silent(&text);
            self.buffer.move_to_end();
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        // Pasting mid-search restores the pre-search draft first, so the
        // paste lands on real content instead of a shown match.
        self.history_search_cancel();
        let line_count = text.split('\n').count();
        if line_count >= PASTE_COLLAPSE_LINES {
            self.pastes.push(text.to_string());
            let chip = self.paste_chip(self.pastes.len() - 1);
            self.buffer.insert_text(&chip);
        } else {
            self.buffer.insert_text(text);
        }
        InputAction::PaletteSync(self.buffer.value())
    }

    /// The chip embeds a zero-width space so text typed to *look* like a
    /// chip can never collide with a real marker on expand.
    fn paste_chip(&self, idx: usize) -> String {
        let line_count = self.pastes[idx].split('\n').count();
        format!("[Pasted\u{200B}#{} +{line_count} lines]", idx + 1)
    }

    /// Swap paste chips back for their stored content, in place. Every
    /// occurrence expands — a duplicated chip pastes its content twice.
    fn expand_pastes(&self, text: &str) -> String {
        let mut expanded = text.to_string();
        for (i, content) in self.pastes.iter().enumerate() {
            let chip = self.paste_chip(i);
            let mut from = 0;
            while let Some(pos) = expanded[from..].find(&chip).map(|p| from + p) {
                expanded.replace_range(pos..pos + chip.len(), content);
                from = pos + content.len();
            }
        }
        expanded
    }

    /// Inserting a file path mid-word looks broken ("read/tmp/x" instead of
    /// "read /tmp/x"). This adds spaces around the paste only when needed.
    pub fn handle_paste_with_spaces(&mut self, text: &str) -> InputAction {
        let line = &self.buffer.lines()[self.buffer.y()];
        let bx = TextBuffer::char_to_byte(line, self.buffer.x());

        let char_before = line[..bx].chars().next_back();
        let char_after = line[bx..].chars().next();

        let is_word_boundary =
            |c: char| -> bool { c.is_alphanumeric() || c == '_' || ")]}>".contains(c) };

        let needs_leading = char_before.is_some_and(&is_word_boundary) && !text.starts_with(' ');
        let needs_trailing = char_after.is_some_and(&is_word_boundary) && !text.ends_with(' ');

        if !needs_leading && !needs_trailing {
            return self.handle_paste(text);
        }

        let mut spaced = String::with_capacity(
            text.len() + usize::from(needs_leading) + usize::from(needs_trailing),
        );

        if needs_leading {
            spaced.push(' ');
        }
        spaced.push_str(text);
        if needs_trailing {
            spaced.push(' ');
        }

        self.handle_paste(&spaced)
    }

    pub fn new(history: InputHistory) -> Self {
        Self {
            buffer: TextBuffer::new(""),
            history,
            history_index: None,
            draft: String::new(),
            stash: None,
            history_search: None,
            pastes: Vec::new(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: PLACEHOLDER_SUGGESTIONS[0],
            placeholder_index: 0,
            pending_images: Vec::new(),
            max_input_lines: MAX_INPUT_LINES,
            last_total_vl: 1,
            last_content_height: 1,
        }
    }

    pub fn set_max_input_lines(&mut self, max: u32) {
        self.max_input_lines = cast::u32_to_u16(max.clamp(1, u32::from(u16::MAX) - 2));
    }

    /// Buffer text for clipboard copies. The `❯ `/indent prefixes are
    /// render chrome, not content — a full-input copy must not include them.
    pub fn copy_text(&self) -> String {
        self.buffer.value()
    }

    /// Text inside the keyboard selection, if one is active.
    pub fn selected_text(&self) -> Option<String> {
        self.buffer.selected_text()
    }

    pub fn line_breaks(&self, content_width: u16) -> LineBreaks {
        let ew = effective_width(content_width.saturating_sub(2) as usize);
        LineBreaks::from_heights(
            self.buffer
                .lines()
                .iter()
                .map(|line| cast::usize_to_u16(visual_line_count(line.width(), ew))),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let ew = effective_width(width.saturating_sub(2) as usize);
        let mut visual_lines = total_visual_lines(&self.buffer, ew, true);
        if !self.pending_images.is_empty() {
            visual_lines += 1;
        }
        let capped = visual_lines.min(self.max_input_lines as usize);
        cast::usize_to_u16(capped + 2)
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.y() == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.y() == self.buffer.line_count().saturating_sub(1)
    }

    fn char_before_cursor(&self) -> Option<char> {
        let x = self.buffer.x();
        if x == 0 {
            return None;
        }
        let line = &self.buffer.lines()[self.buffer.y()];
        let byte_idx = TextBuffer::char_to_byte(line, x - 1);
        line[byte_idx..].chars().next()
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        self.char_before_cursor() == Some('\\')
    }

    fn char_before_cursor_is_whitespace_or_start(&self) -> bool {
        match self.char_before_cursor() {
            None => true,
            Some(c) => c.is_whitespace(),
        }
    }

    pub fn continue_line(&mut self) {
        self.buffer.remove_char();
        self.buffer.add_line();
    }

    pub fn submit(&mut self) -> Option<Submission> {
        let text = self.expand_pastes(&self.buffer.value()).trim().to_string();
        let images = mem::take(&mut self.pending_images);
        if text.is_empty() && images.is_empty() {
            return None;
        }
        self.history.push(text.as_str());
        self.discard();
        Some(Submission {
            text,
            images,
            control: false,
        })
    }

    pub fn discard(&mut self) {
        self.pending_images.clear();
        self.pastes.clear();
        self.history_index = None;
        self.history_search = None;
        self.draft.clear();
        self.buffer.clear();
        self.scroll_y = 0;
        self.placeholder_index = (self.placeholder_index + 1) % PLACEHOLDER_SUGGESTIONS.len();
        self.placeholder_hint = PLACEHOLDER_SUGGESTIONS[self.placeholder_index];
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value().trim().is_empty() && self.pending_images.is_empty()
    }

    pub fn attach_image(&mut self, source: ImageSource) {
        self.pending_images.push(source);
    }

    pub fn set_input(&mut self, s: &str) {
        self.history_search = None;
        self.buffer.set_text(s);
    }

    pub fn set_submission(&mut self, sub: Submission) {
        self.history_search = None;
        self.buffer.set_text(&sub.text);
        self.pending_images = sub.images;
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_index = match self.history_index {
            None => {
                self.draft = self.buffer.value();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(new_index);
        if let Some(entry) = self.history.get(new_index) {
            let text = entry.to_string();
            self.set_input(&text);
        }
        self.buffer.move_to_end();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_index = Some(i + 1);
            if let Some(entry) = self.history.get(i + 1) {
                let text = entry.to_string();
                self.set_input(&text);
            }
        } else {
            self.history_index = None;
            let draft = mem::take(&mut self.draft);
            self.set_input(&draft);
        }
    }

    fn visual_cursor_y(&self, ew: usize) -> u16 {
        let lines_above: u16 = self
            .buffer
            .lines()
            .iter()
            .take(self.buffer.y())
            .map(|line| cast::usize_to_u16(visual_line_count(line.width(), ew)))
            .sum();

        let wrap_row = {
            let line = &self.buffer.lines()[self.buffer.y()];
            let cursor_col: usize = line
                .chars()
                .take(self.buffer.x())
                .map(|c| c.width().unwrap_or_else(|| 1))
                .sum();
            cast::usize_to_u16(cursor_col.checked_div(ew).unwrap_or_else(|| 0))
        };

        lines_above + wrap_row
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        streaming: bool,
        border_style: Style,
        focused: bool,
        top_right_hint: Option<Line<'_>>,
    ) {
        let content_height = area.height.saturating_sub(2);
        let ew = effective_width(area.width.saturating_sub(2) as usize);

        if self.follow_cursor {
            let visual_cursor_y = self.visual_cursor_y(ew);
            if visual_cursor_y < self.scroll_y {
                self.scroll_y = visual_cursor_y;
            } else if visual_cursor_y >= self.scroll_y + content_height {
                self.scroll_y = visual_cursor_y - content_height + 1;
            }
        }

        let mut total_vl = u16::try_from(total_visual_lines(&self.buffer, ew, focused))
            .unwrap_or_else(|_| u16::MAX);
        if !self.pending_images.is_empty() {
            total_vl += 1;
        }
        let max_scroll = total_vl.saturating_sub(content_height);
        self.scroll_y = self.scroll_y.min(max_scroll);
        self.last_total_vl = total_vl;
        self.last_content_height = content_height.max(1);

        let is_empty = self.buffer.value().is_empty();
        let mut styled_lines: Vec<Line> = if is_empty && self.pending_images.is_empty() {
            let placeholder_base = theme::current().input_placeholder;
            if streaming {
                vec![Line::from(vec![
                    super::chevron_span(),
                    if focused {
                        Span::styled("Q", placeholder_base.reversed())
                    } else {
                        Span::styled("Q", placeholder_base)
                    },
                    Span::styled("ueue another prompt...", placeholder_base),
                ])]
            } else {
                vec![Line::from(vec![
                    super::chevron_span(),
                    if focused {
                        Span::styled("A", placeholder_base.reversed())
                    } else {
                        Span::styled("A", placeholder_base)
                    },
                    Span::styled("sk n00n to ", placeholder_base),
                    Span::styled(
                        self.placeholder_hint,
                        placeholder_base.add_modifier(ratatui::style::Modifier::ITALIC),
                    ),
                    Span::styled("...", placeholder_base),
                ])]
            }
        } else {
            let cursor_y = self.buffer.y();
            let cursor_x = self.buffer.x();
            let selection = focused.then(|| self.buffer.selection()).flatten();
            self.buffer
                .lines()
                .iter()
                .enumerate()
                .flat_map(|(i, line)| {
                    let is_cursor_line = i == cursor_y && focused;
                    let line_sel = selection.and_then(|((sy, sx), (ey, ex))| {
                        if i < sy || i > ey {
                            return None;
                        }
                        let start = if i == sy { sx } else { 0 };
                        let end = if i == ey { ex } else { line.chars().count() };
                        (start < end).then_some((start, end))
                    });
                    let shell_spans = if i == 0 {
                        shell_highlight_spans(line)
                    } else {
                        None
                    };
                    wrap_line(
                        line,
                        ew,
                        is_cursor_line,
                        cursor_x,
                        i == 0,
                        shell_spans.as_deref(),
                        line_sel,
                    )
                })
                .collect()
        };

        if !self.pending_images.is_empty() {
            let n = self.pending_images.len();
            let label = match n {
                1 => "1 image".to_string(),
                _ => format!("{n} images"),
            };
            styled_lines.push(Line::from(Span::styled(
                label,
                theme::current().input_placeholder,
            )));
        }

        let text = Text::from(styled_lines);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        if let Some(hint) = top_right_hint {
            block = block.title_top(hint.right_aligned());
        }
        if let Some(query) = self.history_search_query() {
            block = block.title_bottom(
                Line::from(Span::styled(
                    format!("(ctrl+r) `{query}`"),
                    theme::current().input_placeholder,
                ))
                .left_aligned(),
            );
        }
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(theme::current().foreground))
            .scroll((self.scroll_y, 0))
            .block(block);
        frame.render_widget(paragraph, area);

        if max_scroll > 0 {
            let inner = area.inner(ratatui::layout::Margin::new(0, 1));
            render_vertical_scrollbar(frame, inner, total_vl, self.scroll_y, None);
        }
    }

    pub fn scroll_y(&self) -> u16 {
        self.scroll_y
    }

    pub fn scroll_info(&self, area: Rect) -> Option<ScrollInfo> {
        if self.last_total_vl > area.height {
            let max_scroll = self.last_total_vl.saturating_sub(area.height);
            Some(ScrollInfo {
                content_len: self.last_total_vl,
                position: self.scroll_y.min(max_scroll),
            })
        } else {
            None
        }
    }

    pub fn set_scroll_y(&mut self, y: u16) {
        self.scroll_y = y;
        self.follow_cursor = false;
    }

    pub fn history(&self) -> &InputHistory {
        &self.history
    }

    pub fn scroll(&mut self, delta: i32) {
        let max_scroll = self
            .last_total_vl
            .saturating_sub(self.last_content_height.max(1));
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(max_scroll);
        self.follow_cursor = false;
    }
}

fn effective_width(content_width: usize) -> usize {
    content_width.saturating_sub(PREFIX_WIDTH as usize)
}

#[allow(clippy::too_many_arguments)]
fn wrap_line(
    line: &str,
    ew: usize,
    is_cursor_line: bool,
    cursor_x: usize,
    is_first_line: bool,
    shell_spans: Option<&[Span<'static>]>,
    sel: Option<(usize, usize)>,
) -> Vec<Line<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let widths: Vec<usize> = chars
        .iter()
        .map(|c| c.width().unwrap_or_else(|| 1))
        .collect();
    let row_width = ew.max(1);

    let mut row_ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0;
    let mut row_col = 0;
    for (i, &w) in widths.iter().enumerate() {
        if row_col + w > row_width && row_col > 0 {
            row_ranges.push((row_start, i));
            row_start = i;
            row_col = 0;
        }
        row_col += w;
    }
    if row_start < chars.len() || row_ranges.is_empty() {
        row_ranges.push((row_start, chars.len()));
    }
    if is_cursor_line && row_col + 1 > row_width {
        row_ranges.push((chars.len(), chars.len()));
    }

    row_ranges
        .into_iter()
        .enumerate()
        .map(|(row, (start, end))| {
            let prefix_span = if row == 0 && is_first_line {
                super::chevron_span()
            } else if row == 0 {
                Span::raw(NEWLINE_PAD)
            } else {
                Span::raw("")
            };
            let mut spans = vec![prefix_span];

            let chunk_spans = if let Some(styled) = &shell_spans {
                slice_styled_spans(styled, start, end)
            } else {
                let chunk_text: String = chars[start..end].iter().collect();
                vec![Span::raw(chunk_text)]
            };
            // Intersect this row's char window with the line's selected range.
            let row_sel = sel.and_then(|(s, e)| {
                let lo = s.clamp(start, end);
                let hi = e.clamp(start, end);
                (lo < hi).then_some((lo - start, hi - start))
            });
            let chunk_spans = apply_selection(chunk_spans, row_sel);

            if is_cursor_line && cursor_x >= start && cursor_x <= end {
                let local_cursor = cursor_x.saturating_sub(start);
                spans.extend(overlay_cursor(chunk_spans, local_cursor));
            } else {
                spans.extend(chunk_spans);
            }

            Line::from(spans)
        })
        .collect()
}

fn shell_highlight_spans(line: &str) -> Option<Vec<Span<'static>>> {
    if !highlight::is_ready() {
        return None;
    }
    let parsed = parse_shell_prefix(line)?;
    let prefix = &line[..parsed.prefix_len];
    let command = &line[parsed.prefix_len..];
    let shell_style = theme::current().shell_prefix;
    let mut spans = vec![Span::styled(prefix.to_owned(), shell_style)];
    let mut hl = n00n_highlight::Highlighter::for_token("bash");
    for span in highlight::highlight_line(&mut hl, command) {
        spans.push(span);
    }
    Some(spans)
}

fn slice_styled_spans(
    spans: &[Span<'static>],
    char_start: usize,
    char_end: usize,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    for span in spans {
        let span_len = span.content.chars().count();
        let span_end = pos + span_len;
        if span_end <= char_start || pos >= char_end {
            pos = span_end;
            continue;
        }
        let lo = char_start.saturating_sub(pos);
        let hi = (char_end - pos).min(span_len);
        let slice: String = span.content.chars().skip(lo).take(hi - lo).collect();
        if !slice.is_empty() {
            result.push(Span::styled(slice, span.style));
        }
        pos = span_end;
    }
    result
}

fn overlay_cursor(spans: Vec<Span<'static>>, cursor_char_pos: usize) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    let mut cursor_placed = false;
    for span in spans {
        let span_len = span.content.chars().count();
        if !cursor_placed && cursor_char_pos >= pos && cursor_char_pos < pos + span_len {
            let local = cursor_char_pos - pos;
            let byte_pos = TextBuffer::char_to_byte(&span.content, local);
            let (before, after) = span.content.split_at(byte_pos);
            if !before.is_empty() {
                result.push(Span::styled(before.to_string(), span.style));
            }
            let mut cs = after.chars();
            let Some(cursor_char) = cs.next() else {
                break;
            };
            result.push(Span::styled(cursor_char.to_string(), span.style.reversed()));
            let rest: String = cs.collect();
            if !rest.is_empty() {
                result.push(Span::styled(rest.clone(), span.style));
            }
            cursor_placed = true;
        } else {
            result.push(span);
        }
        pos += span_len;
    }
    if !cursor_placed {
        result.push(Span::styled(" ", Style::new().reversed()));
    }
    result
}

/// Add REVERSED to the chars in `[start, end)` (offsets into the text covered
/// by `spans`) — the same technique the cursor overlay uses, over a range.
fn apply_selection(spans: Vec<Span<'static>>, sel: Option<(usize, usize)>) -> Vec<Span<'static>> {
    let Some((start, end)) = sel else {
        return spans;
    };
    let mut result = Vec::with_capacity(spans.len() + 2);
    let mut pos = 0usize;
    for span in spans {
        let span_len = span.content.chars().count();
        let span_end = pos + span_len;
        let lo = start.clamp(pos, span_end).saturating_sub(pos);
        let hi = end.clamp(pos, span_end).saturating_sub(pos);
        if lo >= hi {
            result.push(span);
        } else {
            let mut chars = span.content.chars();
            let before: String = chars.by_ref().take(lo).collect();
            let mid: String = chars.by_ref().take(hi - lo).collect();
            let after: String = chars.collect();
            if !before.is_empty() {
                result.push(Span::styled(before, span.style));
            }
            result.push(Span::styled(mid, span.style.reversed()));
            if !after.is_empty() {
                result.push(Span::styled(after, span.style));
            }
        }
        pos = span_end;
    }
    result
}

fn total_visual_lines(buffer: &TextBuffer, ew: usize, cursor_visible: bool) -> usize {
    let cursor_y = buffer.y();
    buffer
        .lines()
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut text_len = line.width();
            if cursor_visible && i == cursor_y {
                text_len += 1;
            }
            visual_line_count(text_len, ew)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use test_case::test_case;

    fn type_text(input: &mut InputBox, text: &str) {
        for c in text.chars() {
            input.buffer.push_char(c);
        }
    }

    fn submit_text(input: &mut InputBox, text: &str) {
        type_text(input, text);
        input.submit();
    }

    #[test]
    fn submit() {
        let mut input = InputBox::new(InputHistory::default());
        assert!(input.submit().is_none());

        type_text(&mut input, " ");
        assert!(input.submit().is_none());

        type_text(&mut input, " x ");
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "x");
        assert!(sub.images.is_empty());
        assert_eq!(input.buffer.value(), "");

        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.submit().unwrap().text, "line1\nline2");
    }

    #[test]
    fn backslash_continuation() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello\\");
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["hello", ""]);

        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "asd\\asd");
        for _ in 0..3 {
            input.buffer.move_left();
        }
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["asd", "asd"]);
    }

    const TEST_WIDTH: u16 = 80;

    #[test]
    fn height_capped_at_max() {
        let mut input = InputBox::new(InputHistory::default());
        let base = input.height(TEST_WIDTH);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        assert!(input.height(TEST_WIDTH) > base);
        assert!(input.height(TEST_WIDTH) <= MAX_INPUT_LINES + 2);
    }

    #[test]
    fn height_respects_configured_max() {
        let mut input = InputBox::new(InputHistory::default());
        input.set_max_input_lines(3);
        for _ in 0..10 {
            input.buffer.add_line();
        }
        assert_eq!(input.height(TEST_WIDTH), 3 + 2);
    }

    #[test]
    fn first_last_line() {
        let mut input = InputBox::new(InputHistory::default());
        assert!(input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.add_line();
        assert!(!input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.move_up();
        assert!(input.is_at_first_line());
        assert!(!input.is_at_last_line());
    }

    #[test]
    fn history() {
        let mut input = InputBox::new(InputHistory::default());

        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "");

        submit_text(&mut input, "a");
        submit_text(&mut input, "b");
        type_text(&mut input, "draft");

        input.history_up();
        assert_eq!(input.buffer.value(), "b");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");

        input.history_down();
        assert_eq!(input.buffer.value(), "b");
        input.history_down();
        assert_eq!(input.buffer.value(), "draft");

        input.buffer.clear();
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert!(input.is_at_last_line());
        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_first_line());

        input.submit();
        input.history_up();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");

        input.set_input("alpha\nbeta");
        input.submit();
        input.set_input("gamma\ndelta");
        input.submit();

        input.history_up();
        input.history_up();
        assert_eq!(input.buffer.value(), "alpha\nbeta");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "gamma\ndelta");
        assert!(input.is_at_first_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn cursor_adds_extra_wrap_row_at_boundary() {
        let width: u16 = 12;
        let ew = effective_width(width.saturating_sub(2) as usize);

        let mut at_boundary = InputBox::new(InputHistory::default());
        type_text(&mut at_boundary, &"x".repeat(ew));

        let mut before_boundary = InputBox::new(InputHistory::default());
        type_text(&mut before_boundary, &"x".repeat(ew - 1));

        assert_eq!(
            at_boundary.height(width),
            before_boundary.height(width) + 1,
            "cursor at boundary should cause one extra visual line"
        );
    }

    fn render_input_with(
        input: &mut InputBox,
        width: u16,
        height: u16,
        streaming: bool,
        border_style: Style,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                input.view(frame, area, streaming, border_style, true, None);
            })
            .unwrap();
        terminal
    }

    fn render_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_input_with(
            input,
            width,
            height,
            false,
            Style::new().fg(theme::current().mode_build),
        )
    }

    fn has_scrollbar_thumb(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> bool {
        let buf = terminal.backend().buffer();
        (0..buf.area.height).any(|y| {
            buf.cell((buf.area.width - 1, y))
                .is_some_and(|c| c.symbol() == SCROLLBAR_THUMB)
        })
    }

    #[test_case(20, true  ; "visible_when_content_overflows")]
    #[test_case(0,  false ; "hidden_when_content_fits")]
    fn scrollbar_visibility(extra_lines: usize, expect_visible: bool) {
        let mut input = InputBox::new(InputHistory::default());
        for _ in 0..extra_lines {
            input.buffer.add_line();
        }
        let terminal = render_input(&mut input, 40, MAX_INPUT_LINES + 2);
        assert_eq!(has_scrollbar_thumb(&terminal), expect_visible);
    }

    #[test]
    fn scroll_clamped_on_content_shrink() {
        let mut input = InputBox::new(InputHistory::default());
        for _ in 0..20 {
            input.buffer.add_line();
        }
        let area_height = 5_u16;
        let _ = render_input(&mut input, 40, area_height);
        let scroll_before = input.scroll_y;
        assert!(scroll_before > 0);

        input.buffer = TextBuffer::new("short");
        let _ = render_input(&mut input, 40, area_height);
        assert_eq!(input.scroll_y, 0);
    }

    #[test]
    fn multibyte_input_renders_without_panic() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "● grep> hello");
        input.buffer.move_home();
        input.buffer.move_right();
        input.buffer.move_right();
        let _ = render_input(&mut input, 40, 5);
    }

    #[test_case("●\\", true  ; "after_multibyte")]
    #[test_case("●", false   ; "inside_multibyte_would_be_false")]
    fn char_before_cursor_backslash(input: &str, expected: bool) {
        let mut input_box = InputBox::new(InputHistory::default());
        type_text(&mut input_box, input);
        assert_eq!(input_box.char_before_cursor_is_backslash(), expected);
    }

    fn rendered_row(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        row: u16,
    ) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|col| buf.cell((col, row)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn composer_renders_complete_rounded_border() {
        let mut input = InputBox::new(InputHistory::default());
        let terminal = render_input(&mut input, 24, 3);

        assert!(rendered_row(&terminal, 0).starts_with('╭'));
        assert!(rendered_row(&terminal, 0).ends_with('╮'));
        assert!(rendered_row(&terminal, 2).starts_with('╰'));
        assert!(rendered_row(&terminal, 2).ends_with('╯'));
    }

    #[test]
    fn prefix_on_single_line() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 20, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with("│❯"), "row: {row:?}");
        assert!(row.contains("hello"));
    }

    #[test]
    fn prefix_on_multiline() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "aaa");
        input.buffer.add_line();
        type_text(&mut input, "bbb");
        let terminal = render_input(&mut input, 20, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with("│❯"), "row0: {row0:?}");
        assert!(row1.starts_with("│  "), "row1: {row1:?}");
    }

    #[test]
    fn wrapped_line_gets_no_padding() {
        let mut input = InputBox::new(InputHistory::default());
        let ew = effective_width(12);
        type_text(&mut input, &"x".repeat(ew + 3));
        let terminal = render_input(&mut input, 14, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with("│❯"), "row0: {row0:?}");
        assert!(
            row1.starts_with("│x"),
            "wrapped row should start at the card inset: {row1:?}"
        );
    }

    #[test]
    fn copy_text_excludes_prompt_prefix() {
        let input = InputBox::new(InputHistory::default());
        assert_eq!(input.copy_text(), "");

        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.copy_text(), "line1\nline2");
    }

    #[test]
    fn placeholder_has_prefix() {
        let mut input = InputBox::new(InputHistory::default());
        let terminal = render_input(&mut input, 40, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with("│❯"), "placeholder row: {row:?}");
    }

    #[test]
    fn placeholder_rotates_on_discard() {
        let mut input = InputBox::new(InputHistory::default());
        let first = input.placeholder_hint;
        for i in 1..=PLACEHOLDER_SUGGESTIONS.len() {
            input.discard();
            if i < PLACEHOLDER_SUGGESTIONS.len() {
                assert_ne!(input.placeholder_hint, first);
            }
        }
        assert_eq!(input.placeholder_hint, first);
    }

    fn test_image() -> ImageSource {
        use n00n_providers::ImageMediaType;
        use std::sync::Arc;
        ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="))
    }

    #[test]
    fn submit_with_images() {
        let mut input = InputBox::new(InputHistory::default());

        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert!(sub.text.is_empty());
        assert_eq!(sub.images.len(), 1);
        assert!(input.submit().is_none(), "images cleared after submit");

        type_text(&mut input, "describe this");
        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "describe this");
        assert_eq!(sub.images.len(), 1);
    }

    const IMAGE_LABEL: &str = "1 image";

    #[test]
    fn image_label_rendered() {
        let mut input = InputBox::new(InputHistory::default());
        input.attach_image(test_image());
        let h = input.height(40);
        let terminal = render_input(&mut input, 40, h);
        let found = (0..h).any(|row| rendered_row(&terminal, row).contains(IMAGE_LABEL));
        assert!(found, "image label not found in rendered output");
    }

    #[test]
    fn height_accounts_for_pending_images() {
        let mut input = InputBox::new(InputHistory::default());
        let base_height = input.height(TEST_WIDTH);
        input.attach_image(test_image());
        assert_eq!(input.height(TEST_WIDTH), base_height + 1);
    }

    #[test_case("read", "src/main.rs", " src/main.rs" ; "leading_after_ascii")]
    #[test_case("打开", "src/main.rs", " src/main.rs" ; "leading_after_unicode")]
    #[test_case("", "src/main.rs", "src/main.rs" ; "no_leading_at_start")]
    #[test_case("read ", "src/main.rs", "src/main.rs" ; "no_leading_after_space")]
    #[test_case("--file=", "src/main.rs", "src/main.rs" ; "no_leading_after_equals")]
    #[test_case("/", "src/main.rs", "src/main.rs" ; "no_leading_after_slash")]
    #[test_case("\"", "src/main.rs", "src/main.rs" ; "no_leading_after_quote")]
    #[test_case("'", "src/main.rs", "src/main.rs" ; "no_leading_after_squote")]
    #[test_case("foo_", "src/main.rs", " src/main.rs" ; "leading_after_underscore")]
    #[test_case("$(cmd)", "src/main.rs", " src/main.rs" ; "leading_after_closing_paren")]
    #[test_case("arr[0]", "src/main.rs", " src/main.rs" ; "leading_after_closing_bracket")]
    fn paste_with_spaces_leading(before: &str, paste: &str, expected_suffix: &str) {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, before);
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), format!("{before}{expected_suffix}"));
    }

    #[test_case("file", 0, "/tmp/foo", "/tmp/foo file" ; "trailing_before_ascii")]
    #[test_case("を読む", 0, "/tmp/foo", "/tmp/foo を読む" ; "trailing_before_unicode")]
    #[test_case("foobar", 3, "src/main.rs", "foo src/main.rs bar" ; "both_sides_mid_word")]
    #[test_case("in  between", 3, "file.rs", "in file.rs between" ; "neither_side_between_spaces")]
    #[test_case("read ''", 6, "src/main.rs", "read 'src/main.rs'" ; "neither_side_between_quotes")]
    fn paste_with_spaces_at_cursor(before: &str, cursor_at: usize, paste: &str, expected: &str) {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, before);
        let back = before.chars().count() - cursor_at;
        for _ in 0..back {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), expected);
    }

    #[test]
    fn paste_with_spaces_empty_line() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "file.rs");
    }

    #[test]
    fn paste_with_spaces_text_has_leading_space() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "read");
        input.handle_paste_with_spaces(" file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    #[test]
    fn paste_with_spaces_text_has_trailing_space() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "file");
        for _ in 0..4 {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces("src/main.rs ");
        assert_eq!(input.buffer.value(), "src/main.rs file");
    }

    #[test]
    fn paste_with_spaces_multiline_buffer_cursor_on_second_line() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste("first\nread");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "first\nread file.rs");
    }

    #[test]
    fn paste_with_spaces_cursor_at_end_no_trailing() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "read");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    fn key_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn at_mention_opens_picker_at_start() {
        let mut input = InputBox::new(InputHistory::default());
        let action = input.handle_key(key_char('@'));
        assert!(matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn at_mention_opens_picker_after_whitespace() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "read ");
        let action = input.handle_key(key_char('@'));
        assert!(matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "read ");
    }

    #[test]
    fn at_mention_opens_picker_at_new_line_start() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "read");
        input.buffer.add_line();
        let action = input.handle_key(key_char('@'));
        assert!(matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "read\n");
    }

    #[test]
    fn at_mention_is_literal_mid_word() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "em");
        let action = input.handle_key(key_char('@'));
        assert!(!matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "em@");
    }

    #[test]
    fn at_mention_is_literal_after_punctuation() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "see(");
        let action = input.handle_key(key_char('@'));
        assert!(!matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "see(@");
    }

    #[test]
    fn at_mention_is_literal_with_ctrl_modifier() {
        let mut input = InputBox::new(InputHistory::default());
        let action = input.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL));
        assert!(!matches!(action, InputAction::OpenFilePicker));
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn stash_hides_and_restores_draft() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "draft text");
        input.attach_image(test_image());

        assert!(matches!(input.stash_toggle(), StashOutcome::Stashed));
        assert!(input.is_empty());
        assert!(input.buffer.value().is_empty());

        assert!(matches!(input.stash_toggle(), StashOutcome::Restored));
        assert_eq!(input.buffer.value(), "draft text");
        let sub = input.submit().unwrap();
        assert_eq!(sub.images.len(), 1);
    }

    #[test]
    fn stash_toggle_empty_is_noop() {
        let mut input = InputBox::new(InputHistory::default());
        assert!(matches!(input.stash_toggle(), StashOutcome::NothingToStash));
        assert!(input.buffer.value().is_empty());
    }

    #[test]
    fn stash_occupied_is_refused_not_overwritten() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "first");
        assert!(matches!(input.stash_toggle(), StashOutcome::Stashed));

        type_text(&mut input, "second");
        assert!(matches!(input.stash_toggle(), StashOutcome::Occupied));
        assert_eq!(input.buffer.value(), "second");

        input.discard();
        assert!(matches!(input.stash_toggle(), StashOutcome::Restored));
        assert_eq!(input.buffer.value(), "first");
    }

    #[test]
    fn history_search_finds_and_accepts_match() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "first command");
        submit_text(&mut input, "second command");
        submit_text(&mut input, "third thing");

        input.start_history_search();
        assert!(input.history_search_active());
        for c in "com".chars() {
            input.history_search_push(c);
        }
        assert_eq!(input.buffer.value(), "second command");

        input.history_search_older();
        assert_eq!(input.buffer.value(), "first command");
        input.history_search_newer();
        assert_eq!(input.buffer.value(), "second command");

        input.history_search_accept();
        assert!(!input.history_search_active());
        assert_eq!(input.buffer.value(), "second command");
    }

    #[test]
    fn history_search_cancel_restores_draft() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "old entry");
        type_text(&mut input, "wip");

        input.start_history_search();
        input.history_search_push('o');
        assert_eq!(input.buffer.value(), "old entry");

        input.history_search_cancel();
        assert!(!input.history_search_active());
        assert_eq!(input.buffer.value(), "wip");
    }

    #[test]
    fn history_search_backspace_edits_query() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "alpha");
        submit_text(&mut input, "alpine");

        input.start_history_search();
        for c in "alpz".chars() {
            input.history_search_push(c);
        }
        assert_eq!(input.buffer.value(), "alpine", "no match keeps last shown");
        input.history_search_backspace();
        input.history_search_backspace();
        assert_eq!(input.history_search_query(), Some("al"));
        assert_eq!(input.buffer.value(), "alpine");
    }

    #[test]
    fn long_paste_collapses_to_chip_and_expands_on_submit() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste("l1\nl2\nl3\nl4\nl5\nl6");
        assert_eq!(input.buffer.value(), "[Pasted\u{200B}#1 +6 lines]");

        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "l1\nl2\nl3\nl4\nl5\nl6");
    }

    #[test]
    fn short_paste_inserts_verbatim() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste("a\nb");
        assert_eq!(input.buffer.value(), "a\nb");
    }

    #[test]
    fn chip_expands_inline_with_surrounding_text() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "run this ");
        input.handle_paste("a\nb\nc\nd\ne\nf");
        type_text(&mut input, " now");

        assert_eq!(
            input.buffer.value(),
            "run this [Pasted\u{200B}#1 +6 lines] now"
        );
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "run this a\nb\nc\nd\ne\nf now");
    }

    #[test]
    fn paste_chip_survives_stash_round_trip() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste("x\ny\nz\nw\nv\nu");

        assert!(matches!(input.stash_toggle(), StashOutcome::Stashed));
        assert!(matches!(input.stash_toggle(), StashOutcome::Restored));
        assert_eq!(input.buffer.value(), "[Pasted\u{200B}#1 +6 lines]");

        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "x\ny\nz\nw\nv\nu");
    }

    #[test]
    fn typed_chip_lookalike_does_not_collide() {
        let mut input = InputBox::new(InputHistory::default());
        input.handle_paste("real\npaste\ncontent\nhere\nyes\nok");
        // A literal that LOOKS like the chip but lacks the ZWSP marker must
        // pass through untouched while the real chip still expands.
        type_text(&mut input, " [Pasted #1 +6 lines]");
        let sub = input.submit().unwrap();
        assert_eq!(
            sub.text,
            "real\npaste\ncontent\nhere\nyes\nok [Pasted #1 +6 lines]"
        );
    }

    #[test]
    fn paste_during_search_restores_draft_then_inserts() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "old cmd");
        type_text(&mut input, "wip");

        input.start_history_search();
        assert_eq!(input.buffer.value(), "old cmd");
        input.handle_paste("a\nb\nc\nd\ne\nf");

        assert!(!input.history_search_active());
        assert_eq!(input.buffer.value(), "wip[Pasted\u{200B}#1 +6 lines]");
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "wipa\nb\nc\nd\ne\nf");
    }

    #[test]
    fn discard_clears_history_search() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "old cmd");
        type_text(&mut input, "wip");
        input.start_history_search();

        input.discard();
        assert!(!input.history_search_active());
        // Nothing is resurrected: the discard is final.
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn edit_action_during_search_accepts_match_first() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "old cmd");
        input.start_history_search();
        assert_eq!(input.buffer.value(), "old cmd");

        // A bound edit key acts on the accepted match, not the query.
        input.edit_action(crate::keymap::KeyAction::KillLineStart);
        assert!(!input.history_search_active());
        assert_eq!(input.buffer.value(), "");
        let sub = input.submit();
        assert!(sub.is_none());
    }

    #[test]
    fn undo_after_search_cancel_ignores_search_edits() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "old cmd");
        type_text(&mut input, "wip");
        input.start_history_search();
        input.history_search_push('o');
        input.history_search_cancel();
        assert_eq!(input.buffer.value(), "wip");

        input.buffer.undo();
        assert_eq!(input.buffer.value(), "", "undo rewinds typing, not search");
    }

    #[test]
    fn altgr_chars_reach_search_query() {
        let mut input = InputBox::new(InputHistory::default());
        submit_text(&mut input, "find \\ me");
        input.start_history_search();
        input.handle_key(KeyEvent::new(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(input.history_search_query(), Some("\\"));
    }

    #[test]
    fn edit_action_select_then_type_replaces() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello");
        input.edit_action(KeyAction::SelectCharLeft);
        input.edit_action(KeyAction::SelectCharLeft);
        assert_eq!(input.selected_text().as_deref(), Some("lo"));

        let action = input.edit_action(KeyAction::DeleteCharBack);
        assert!(matches!(action, InputAction::PaletteSync(_)));
        assert_eq!(input.buffer.value(), "hel");
        assert!(input.selected_text().is_none());
    }

    #[test]
    fn edit_action_select_all() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "ab");
        input.buffer.add_line();
        type_text(&mut input, "cd");
        input.edit_action(KeyAction::SelectAll);
        assert_eq!(input.selected_text().as_deref(), Some("ab\ncd"));
    }

    #[test]
    fn plain_move_action_drops_selection() {
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello");
        input.edit_action(KeyAction::SelectCharLeft);
        assert!(input.selected_text().is_some());
        input.edit_action(KeyAction::CharLeft);
        assert!(input.selected_text().is_none());
    }

    #[test]
    fn selection_renders_reversed() {
        use ratatui::style::Modifier;
        let mut input = InputBox::new(InputHistory::default());
        type_text(&mut input, "hello");
        input.edit_action(KeyAction::SelectCharLeft);
        input.edit_action(KeyAction::SelectCharLeft);
        let terminal = render_input(&mut input, 20, 5);
        let buf = terminal.backend().buffer();
        let reversed: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|pos| {
                buf.cell(*pos)
                    .is_some_and(|c| c.style().add_modifier.contains(Modifier::REVERSED))
            })
            .filter_map(|pos| Some(buf.cell(pos)?.symbol().to_string()))
            .collect();
        assert!(
            reversed.contains('l') && reversed.contains('o'),
            "selected chars should render reversed: {reversed:?}"
        );
    }
}
