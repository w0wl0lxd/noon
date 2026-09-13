use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::highlight::TAB_SPACES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditResult {
    Ignored,
    Moved,
    Changed,
}

const UNDO_LIMIT: usize = 200;
const KILL_RING_LIMIT: usize = 30;

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    raw_x: usize,
    cursor_y: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Remove,
    Other,
}

/// Char-cell span of the most recent yank, for `yank_pop` cycling.
#[derive(Clone, Copy)]
struct YankSpan {
    y: usize,
    x: usize,
    end_y: usize,
    end_x: usize,
    ring_pos: usize,
}

pub struct TextBuffer {
    lines: Vec<String>,
    raw_x: usize,
    cursor_y: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<EditKind>,
    kill_ring: Vec<String>,
    yank: Option<YankSpan>,
}

impl TextBuffer {
    pub fn new(input: &str) -> Self {
        let lines: Vec<String> = input.split('\n').map(str::to_string).collect();
        Self {
            lines,
            raw_x: 0,
            cursor_y: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            kill_ring: Vec::new(),
            yank: None,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            raw_x: self.raw_x,
            cursor_y: self.cursor_y,
        }
    }

    fn restore(&mut self, snap: Snapshot) {
        self.lines = snap.lines;
        self.raw_x = snap.raw_x;
        self.cursor_y = snap.cursor_y;
    }

    /// Push the pre-edit state onto the undo stack. Consecutive edits of the
    /// same `Insert`/`Remove` kind coalesce into one undo step; `Other` ops
    /// (kills, line splits, yanks) always form their own step.
    fn record(&mut self, kind: EditKind) {
        if kind != EditKind::Other && self.last_edit == Some(kind) {
            return;
        }
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.last_edit = Some(kind);
        self.yank = None;
    }

    /// Cursor motion breaks an undo group and cancels a pending yank-pop.
    fn note_move(&mut self) {
        self.last_edit = None;
        self.yank = None;
    }

    fn push_kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.kill_ring.push(text);
        if self.kill_ring.len() > KILL_RING_LIMIT {
            self.kill_ring.remove(0);
        }
    }

    /// Replace the whole buffer as one undoable edit (history recall, stash
    /// restore, external editor round-trip).
    pub fn set_text(&mut self, text: &str) {
        self.record(EditKind::Other);
        self.lines = text.split('\n').map(str::to_string).collect();
        self.raw_x = 0;
        self.cursor_y = 0;
    }

    /// Replace the whole buffer without recording an undo step — for
    /// history-search preview and its cancel-restore, where the search
    /// itself is the recovery mechanism. Still breaks the edit group and
    /// cancels a pending yank-pop like a cursor move would.
    pub fn set_text_silent(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        self.raw_x = 0;
        self.cursor_y = 0;
        self.note_move();
    }

    pub fn undo(&mut self) -> bool {
        if let Some(snap) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(snap);
            self.last_edit = None;
            self.yank = None;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(snap) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(snap);
            self.last_edit = None;
            self.yank = None;
            true
        } else {
            false
        }
    }

    /// Insert the newest kill-ring entry at the cursor.
    pub fn yank(&mut self) -> bool {
        let Some(text) = self.kill_ring.last().cloned() else {
            return false;
        };
        self.record(EditKind::Other);
        let (sy, sx) = (self.cursor_y, self.x());
        self.insert_text_inner(&text);
        // The cursor sits at the insertion end — reading it back keeps the
        // span correct through `\t` expansion and any future sanitizing.
        let (end_y, end_x) = (self.cursor_y, self.x());
        self.yank = Some(YankSpan {
            y: sy,
            x: sx,
            end_y,
            end_x,
            ring_pos: self.kill_ring.len() - 1,
        });
        true
    }

    /// Replace the last yank with the next-older kill-ring entry. Only valid
    /// immediately after `yank`/`yank_pop` with no other edit in between.
    pub fn yank_pop(&mut self) -> bool {
        let Some(span) = self.yank else {
            return false;
        };
        if self.kill_ring.len() < 2 {
            return false;
        }
        self.record(EditKind::Other);
        self.remove_span(span.y, span.x, span.end_y, span.end_x);
        let ring_pos = (span.ring_pos + self.kill_ring.len() - 1) % self.kill_ring.len();
        let text = self.kill_ring[ring_pos].clone();
        self.cursor_y = span.y;
        self.raw_x = span.x;
        self.insert_text_inner(&text);
        let (end_y, end_x) = (self.cursor_y, self.x());
        self.yank = Some(YankSpan {
            end_y,
            end_x,
            ring_pos,
            ..span
        });
        true
    }

    fn remove_span(&mut self, sy: usize, sx: usize, ey: usize, ex: usize) {
        let start_byte = Self::char_to_byte(&self.lines[sy], sx);
        let end_byte = Self::char_to_byte(&self.lines[ey], ex);
        let mut merged = self.lines[sy][..start_byte].to_string();
        merged.push_str(&self.lines[ey][end_byte..]);
        self.lines[sy] = merged;
        self.lines.drain(sy + 1..=ey);
        self.cursor_y = sy;
        self.raw_x = sx;
    }

    pub fn value(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn x(&self) -> usize {
        self.raw_x.min(self.current_line_len())
    }

    pub fn y(&self) -> usize {
        self.cursor_y
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_y]
    }

    fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map_or(s.len(), |(byte_idx, _)| byte_idx)
    }

    fn byte_x(&self) -> usize {
        Self::char_to_byte(self.current_line(), self.x())
    }

    pub fn push_char(&mut self, c: char) {
        self.record(EditKind::Insert);
        let bx = self.byte_x();
        self.lines[self.cursor_y].insert(bx, c);
        self.raw_x = self.x() + 1;
    }

    pub fn insert_text(&mut self, text: &str) {
        self.record(EditKind::Insert);
        self.insert_text_inner(text);
    }

    fn insert_text_inner(&mut self, text: &str) {
        let sanitized = text.replace('\t', TAB_SPACES);
        for (i, chunk) in sanitized.split('\n').enumerate() {
            if i > 0 {
                self.add_line_inner();
            }
            if !chunk.is_empty() {
                let bx = self.byte_x();
                self.lines[self.cursor_y].insert_str(bx, chunk);
                self.raw_x = self.x() + chunk.chars().count();
            }
        }
    }

    pub fn add_line(&mut self) {
        self.record(EditKind::Other);
        self.add_line_inner();
    }

    fn add_line_inner(&mut self) {
        let bx = self.byte_x();
        let (left, right) = self.lines[self.cursor_y].split_at(bx);
        let (left, right) = (left.to_string(), right.to_string());
        self.lines[self.cursor_y] = left;
        self.lines.insert(self.cursor_y + 1, right);
        self.raw_x = 0;
        self.cursor_y += 1;
    }

    pub fn remove_char(&mut self) {
        if self.x() == 0 && self.cursor_y == 0 {
            return;
        }
        self.record(EditKind::Remove);
        let x = self.x();
        if x == 0 {
            self.merge_with_previous_line();
        } else {
            let bx = Self::char_to_byte(self.current_line(), x - 1);
            self.lines[self.cursor_y].remove(bx);
            self.raw_x = x - 1;
        }
    }

    pub fn delete_char(&mut self) {
        if self.x() == self.current_line_len() && self.cursor_y + 1 == self.lines.len() {
            return;
        }
        self.record(EditKind::Remove);
        let x = self.x();
        if x == self.current_line_len() {
            self.merge_with_next_line();
        } else {
            let bx = self.byte_x();
            self.lines[self.cursor_y].remove(bx);
        }
    }

    fn wrap_to_prev_line(&mut self) -> bool {
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
            self.raw_x = self.current_line_len();
            true
        } else {
            false
        }
    }

    fn wrap_to_next_line(&mut self) -> bool {
        if self.cursor_y < self.lines.len().saturating_sub(1) {
            self.cursor_y += 1;
            self.raw_x = 0;
            true
        } else {
            false
        }
    }

    fn find_prev_word_boundary(&self, char_x: usize) -> usize {
        let chars: Vec<char> = self.current_line().chars().collect();
        let mut i = char_x;
        while i > 0 && chars[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        i
    }

    fn find_next_word_boundary(&self, char_x: usize) -> usize {
        let chars: Vec<char> = self.current_line().chars().collect();
        let len = chars.len();
        let mut i = char_x;
        while i < len && chars[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }

    /// Delete the word after the cursor, saving it to the kill ring.
    pub fn delete_word_after_cursor(&mut self) {
        let x = self.x();
        let line_len = self.current_line_len();
        if x == line_len {
            if self.cursor_y + 1 < self.lines.len() {
                self.record(EditKind::Other);
                self.merge_with_next_line();
                self.push_kill("\n".to_string());
            }
            return;
        }
        self.record(EditKind::Other);
        let new_x = self.find_next_word_boundary(x);
        let byte_start = Self::char_to_byte(self.current_line(), x);
        let byte_end = Self::char_to_byte(self.current_line(), new_x);
        let killed = self.lines[self.cursor_y][byte_start..byte_end].to_string();
        self.lines[self.cursor_y].replace_range(byte_start..byte_end, "");
        self.push_kill(killed);
    }

    /// Delete to the end of the line, saving the killed text to the ring.
    pub fn kill_to_end_of_line(&mut self) {
        let bx = self.byte_x();
        if bx == self.lines[self.cursor_y].len() {
            return;
        }
        self.record(EditKind::Other);
        let killed = self.lines[self.cursor_y][bx..].to_string();
        self.lines[self.cursor_y].truncate(bx);
        self.push_kill(killed);
    }

    /// Delete the word before the cursor, saving it to the kill ring.
    pub fn remove_word_before_cursor(&mut self) {
        let x = self.x();
        if x == 0 {
            if self.cursor_y == 0 {
                return;
            }
            self.record(EditKind::Other);
            self.merge_with_previous_line();
            self.push_kill("\n".to_string());
            return;
        }
        self.record(EditKind::Other);
        let new_x = self.find_prev_word_boundary(x);
        let line = self.current_line();
        let byte_start = Self::char_to_byte(line, new_x);
        let byte_end = Self::char_to_byte(line, x);
        let killed = line[byte_start..byte_end].to_string();
        self.lines[self.cursor_y].replace_range(byte_start..byte_end, "");
        self.raw_x = new_x;
        self.push_kill(killed);
    }

    pub fn move_word_left(&mut self) {
        self.note_move();
        let x = self.x();
        if x == 0 {
            self.wrap_to_prev_line();
            return;
        }
        self.raw_x = self.find_prev_word_boundary(x);
    }

    pub fn move_word_right(&mut self) {
        self.note_move();
        let x = self.x();
        if x == self.current_line_len() {
            self.wrap_to_next_line();
            return;
        }
        self.raw_x = self.find_next_word_boundary(x);
    }

    pub fn move_left(&mut self) {
        self.note_move();
        let x = self.x();
        if x > 0 {
            self.raw_x = x - 1;
        } else {
            self.wrap_to_prev_line();
        }
    }

    pub fn move_right(&mut self) {
        self.note_move();
        let x = self.x();
        if x < self.current_line_len() {
            self.raw_x = x + 1;
        } else {
            self.wrap_to_next_line();
        }
    }

    pub fn move_up(&mut self) {
        self.note_move();
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
        }
    }

    pub fn move_down(&mut self) {
        self.note_move();
        if self.cursor_y < self.lines.len().saturating_sub(1) {
            self.cursor_y += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.note_move();
        self.raw_x = 0;
    }

    pub fn move_end(&mut self) {
        self.note_move();
        self.raw_x = self.current_line_len();
    }

    /// Reset the buffer and its edit history (submit/discard starts a new
    /// editing session; the kill ring survives like in readline).
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.raw_x = 0;
        self.cursor_y = 0;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        self.yank = None;
    }

    pub fn move_to_end(&mut self) {
        self.note_move();
        self.cursor_y = self.lines.len().saturating_sub(1);
        self.raw_x = self.current_line_len();
    }

    fn merge_with_next_line(&mut self) {
        if self.cursor_y + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_y + 1);
            self.lines[self.cursor_y].push_str(&next);
        }
    }

    fn merge_with_previous_line(&mut self) {
        if self.cursor_y == 0 {
            return;
        }
        self.cursor_y -= 1;
        self.raw_x = self.current_line_len();
        self.merge_with_next_line();
    }

    /// Delete to the start of the line, saving the killed text to the ring.
    pub fn kill_to_start_of_line(&mut self) {
        let byte_x = Self::char_to_byte(&self.lines[self.cursor_y], self.x());
        if byte_x == 0 {
            return;
        }
        self.record(EditKind::Other);
        let killed = self.lines[self.cursor_y][..byte_x].to_string();
        self.lines[self.cursor_y].drain(..byte_x);
        self.raw_x = 0;
        self.push_kill(killed);
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> EditResult {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT);
        let alt = m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL);
        let sup = m.contains(KeyModifiers::SUPER);

        if ctrl {
            return match key.code {
                KeyCode::Left => {
                    self.move_word_left();
                    EditResult::Moved
                }
                KeyCode::Right => {
                    self.move_word_right();
                    EditResult::Moved
                }
                KeyCode::Backspace | KeyCode::Char('w') => {
                    self.remove_word_before_cursor();
                    EditResult::Changed
                }
                KeyCode::Delete => {
                    self.delete_word_after_cursor();
                    EditResult::Changed
                }
                KeyCode::Char('k') => {
                    self.kill_to_end_of_line();
                    EditResult::Changed
                }
                KeyCode::Char('u') => {
                    self.kill_to_start_of_line();
                    EditResult::Changed
                }
                KeyCode::Char('y') => {
                    return if self.yank() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    };
                }
                KeyCode::Char('a') => {
                    self.move_home();
                    EditResult::Moved
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    EditResult::Moved
                }
                KeyCode::Char('_' | '-') => {
                    return if self.undo() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    };
                }
                KeyCode::Char(c)
                    if c.eq_ignore_ascii_case(&'z') && m.contains(KeyModifiers::SHIFT) =>
                {
                    return if self.redo() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    };
                }
                _ => EditResult::Ignored,
            };
        }

        if alt {
            return match key.code {
                KeyCode::Left | KeyCode::Char('b') => {
                    self.move_word_left();
                    EditResult::Moved
                }
                KeyCode::Right | KeyCode::Char('f') => {
                    self.move_word_right();
                    EditResult::Moved
                }
                KeyCode::Backspace => {
                    self.remove_word_before_cursor();
                    EditResult::Changed
                }
                KeyCode::Delete | KeyCode::Char('d') => {
                    self.delete_word_after_cursor();
                    EditResult::Changed
                }
                KeyCode::Char('y') => {
                    return if self.yank_pop() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    };
                }
                KeyCode::Char(c)
                    if c.eq_ignore_ascii_case(&'z') && m.contains(KeyModifiers::SHIFT) =>
                {
                    return if self.redo() {
                        EditResult::Changed
                    } else {
                        EditResult::Ignored
                    };
                }
                _ => EditResult::Ignored,
            };
        }

        if sup {
            return match key.code {
                KeyCode::Left => {
                    self.move_home();
                    EditResult::Moved
                }
                KeyCode::Right => {
                    self.move_end();
                    EditResult::Moved
                }
                KeyCode::Backspace => {
                    self.kill_to_start_of_line();
                    EditResult::Changed
                }
                _ => EditResult::Ignored,
            };
        }

        match key.code {
            KeyCode::Char(c) => {
                self.push_char(c);
                EditResult::Changed
            }
            KeyCode::Backspace => {
                self.remove_char();
                EditResult::Changed
            }
            KeyCode::Delete => {
                self.delete_char();
                EditResult::Changed
            }
            KeyCode::Left => {
                self.move_left();
                EditResult::Moved
            }
            KeyCode::Right => {
                self.move_right();
                EditResult::Moved
            }
            KeyCode::Home => {
                self.move_home();
                EditResult::Moved
            }
            KeyCode::End => {
                self.move_end();
                EditResult::Moved
            }
            KeyCode::Up => {
                self.move_up();
                EditResult::Moved
            }
            KeyCode::Down => {
                self.move_down();
                EditResult::Moved
            }
            _ => EditResult::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EditResult, TextBuffer};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use test_case::test_case;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn insert_at_middle() {
        let mut buf = TextBuffer::new("");
        buf.push_char('a');
        buf.push_char('c');
        buf.raw_x = 1;
        buf.push_char('b');
        assert_eq!(buf.value(), "abc");
    }

    #[test]
    fn split_then_merge_is_identity() {
        let mut buf = TextBuffer::new("abcd");
        buf.raw_x = 2;
        buf.add_line();
        assert_eq!(buf.lines(), &["ab", "cd"]);

        buf.remove_char();
        assert_eq!(buf.value(), "abcd");
    }

    #[test]
    fn delete_char_merges_lines() {
        let mut buf = TextBuffer::new("ab\ncd");
        buf.raw_x = 2;
        buf.delete_char();
        assert_eq!(buf.value(), "abcd");
    }

    #[test]
    fn cursor_wraps_across_lines() {
        let mut buf = TextBuffer::new("ab\ncd");
        buf.raw_x = 2;
        buf.move_right();
        assert_eq!((buf.y(), buf.x()), (1, 0));

        buf.move_left();
        assert_eq!((buf.y(), buf.x()), (0, 2));
    }

    #[test]
    fn insert_text_multiline() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("line1\nline2\nline3");
        assert_eq!(buf.lines(), &["line1", "line2", "line3"]);
        assert_eq!(buf.y(), 2);
        assert_eq!(buf.x(), 5);
    }

    #[test]
    fn insert_text_at_cursor_middle() {
        let mut buf = TextBuffer::new("abcd");
        buf.raw_x = 2;
        buf.insert_text("X\nY");
        assert_eq!(buf.lines(), &["abX", "Ycd"]);
    }

    #[test]
    fn insert_text_replaces_tabs_with_spaces() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("\tindented\n\t\tdouble");
        assert_eq!(buf.lines(), &["  indented", "    double"]);
    }

    #[test]
    fn remove_word() {
        let mut buf = TextBuffer::new("hello world");
        buf.raw_x = 11;
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "hello ");

        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "");

        let mut buf = TextBuffer::new("ab\ncd");
        buf.cursor_y = 1;
        buf.raw_x = 0;
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "abcd");

        let mut buf = TextBuffer::new("hello ●●●");
        buf.move_to_end();
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "hello ");
    }

    #[test]
    fn move_word_left() {
        let mut buf = TextBuffer::new("hello world");
        buf.move_to_end();
        buf.move_word_left();
        assert_eq!(buf.x(), 6);
        buf.move_word_left();
        assert_eq!(buf.x(), 0);

        let mut buf = TextBuffer::new("  hello");
        buf.move_to_end();
        buf.move_word_left();
        assert_eq!(buf.x(), 2);

        let mut buf = TextBuffer::new("ab\ncd");
        buf.cursor_y = 1;
        buf.raw_x = 0;
        buf.move_word_left();
        assert_eq!((buf.y(), buf.x()), (0, 2));
    }

    #[test]
    fn move_word_right() {
        let mut buf = TextBuffer::new("hello world");
        buf.move_word_right();
        assert_eq!(buf.x(), 5);
        buf.move_word_right();
        assert_eq!(buf.x(), 11);

        let mut buf = TextBuffer::new("hello  ");
        buf.move_word_right();
        assert_eq!(buf.x(), 5);

        let mut buf = TextBuffer::new("ab\ncd");
        buf.raw_x = 2;
        buf.move_word_right();
        assert_eq!((buf.y(), buf.x()), (1, 0));
    }

    #[test]
    fn multibyte_operations() {
        let mut buf = TextBuffer::new("");
        buf.push_char('a');
        buf.push_char('●');
        buf.push_char('b');
        assert_eq!(buf.value(), "a●b");

        buf.remove_char();
        assert_eq!(buf.value(), "a●");
        buf.remove_char();
        assert_eq!(buf.value(), "a");

        let mut buf = TextBuffer::new("a●b");
        buf.move_to_end();
        buf.move_left();
        assert_eq!(buf.x(), 2);
        buf.move_left();
        assert_eq!(buf.x(), 1);
        buf.move_right();
        assert_eq!(buf.x(), 2);

        let mut buf = TextBuffer::new("a●b");
        buf.raw_x = 1;
        buf.delete_char();
        assert_eq!(buf.value(), "ab");

        let mut buf = TextBuffer::new("a●b");
        buf.raw_x = 2;
        buf.add_line();
        assert_eq!(buf.lines(), &["a●", "b"]);

        let mut buf = TextBuffer::new("a●b");
        buf.raw_x = 2;
        buf.insert_text("X");
        assert_eq!(buf.value(), "a●Xb");
    }

    #[test]
    fn sticky_x_with_multibyte() {
        let mut buf = TextBuffer::new("a●cd\nhi\na●cd");
        buf.raw_x = 4;
        buf.move_down();
        assert_eq!(buf.x(), 2);
        buf.move_down();
        assert_eq!(buf.x(), 4);
    }

    #[test]
    fn delete_word_after_cursor() {
        let mut buf = TextBuffer::new("hello world");
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), " world");

        let mut buf = TextBuffer::new("hello world");
        buf.raw_x = 6;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "hello ");

        let mut buf = TextBuffer::new("ab\ncd");
        buf.raw_x = 2;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "abcd");

        let mut buf = TextBuffer::new("end");
        buf.raw_x = 3;
        buf.delete_word_after_cursor();
        assert_eq!(buf.value(), "end");
    }

    #[test]
    fn kill_to_end_of_line() {
        let mut buf = TextBuffer::new("hello world");
        buf.raw_x = 5;
        buf.kill_to_end_of_line();
        assert_eq!(buf.value(), "hello");

        let mut buf = TextBuffer::new("ab\ncd");
        buf.kill_to_end_of_line();
        assert_eq!(buf.lines(), &["", "cd"]);

        let mut buf = TextBuffer::new("●text");
        buf.raw_x = 1;
        buf.kill_to_end_of_line();
        assert_eq!(buf.value(), "●");
    }

    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::ALT)
    }

    fn super_key(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::SUPER)
    }

    #[test_case(plain(KeyCode::Char('a')),      EditResult::Changed ; "plain_changed")]
    #[test_case(plain(KeyCode::Left),            EditResult::Moved   ; "plain_moved")]
    #[test_case(plain(KeyCode::F(1)),            EditResult::Ignored ; "plain_ignored")]
    #[test_case(ctrl(KeyCode::Char('e')),        EditResult::Moved   ; "ctrl_e_moved")]
    #[test_case(ctrl(KeyCode::Char('k')),        EditResult::Changed ; "ctrl_changed")]
    #[test_case(ctrl(KeyCode::Char('a')),        EditResult::Moved   ; "ctrl_moved")]
    #[test_case(ctrl(KeyCode::Char('z')),        EditResult::Ignored ; "ctrl_ignored")]
    #[test_case(alt(KeyCode::Backspace),         EditResult::Changed ; "alt_changed")]
    #[test_case(alt(KeyCode::Left),              EditResult::Moved   ; "alt_moved")]
    #[test_case(alt(KeyCode::Char('z')),         EditResult::Ignored ; "alt_ignored")]
    #[test_case(super_key(KeyCode::Backspace),   EditResult::Changed ; "super_changed")]
    #[test_case(super_key(KeyCode::Left),        EditResult::Moved   ; "super_moved")]
    #[test_case(super_key(KeyCode::Char('z')),   EditResult::Ignored ; "super_ignored")]
    #[test_case(plain(KeyCode::Up),              EditResult::Moved   ; "plain_up")]
    #[test_case(plain(KeyCode::Down),            EditResult::Moved   ; "plain_down")]
    fn handle_key_returns_correct_result(key: KeyEvent, expected: EditResult) {
        let mut buf = TextBuffer::new("hello world");
        buf.raw_x = 5;
        assert_eq!(buf.handle_key(key), expected);
    }

    #[test]
    fn kill_to_start_of_line() {
        let mut buf = TextBuffer::new("●hello world");
        buf.raw_x = 1;
        buf.kill_to_start_of_line();
        assert_eq!(buf.value(), "hello world");
        assert_eq!(buf.x(), 0);
    }

    #[test]
    fn undo_coalesces_typed_chars() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("hello");
        assert!(buf.undo());
        assert_eq!(buf.value(), "");
        assert!(!buf.undo());
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("ab");
        buf.move_home();
        buf.delete_char();

        assert!(buf.undo());
        assert_eq!(buf.value(), "ab");
        assert!(buf.undo());
        assert_eq!(buf.value(), "");

        assert!(buf.redo());
        assert_eq!(buf.value(), "ab");
        assert!(buf.redo());
        assert_eq!(buf.value(), "b");
        assert!(!buf.redo());
    }

    #[test]
    fn cursor_move_breaks_undo_group() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("ab");
        buf.move_left();
        buf.insert_text("c");
        assert_eq!(buf.value(), "acb");

        assert!(buf.undo());
        assert_eq!(buf.value(), "ab");
        assert!(buf.undo());
        assert_eq!(buf.value(), "");
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut buf = TextBuffer::new("");
        buf.insert_text("a");
        assert!(buf.undo());
        buf.insert_text("b");
        assert!(!buf.redo());
    }

    #[test]
    fn kill_and_yank_round_trip() {
        let mut buf = TextBuffer::new("hello world");
        buf.move_home();
        buf.kill_to_end_of_line();
        assert_eq!(buf.value(), "");

        assert!(buf.yank());
        assert_eq!(buf.value(), "hello world");
    }

    #[test]
    fn yank_pop_cycles_kill_ring() {
        let mut buf = TextBuffer::new("one two three");
        buf.move_end();
        buf.remove_word_before_cursor();
        buf.remove_word_before_cursor();
        assert_eq!(buf.value(), "one ");

        buf.move_end();
        assert!(buf.yank());
        assert_eq!(buf.value(), "one two ");
        assert!(buf.yank_pop());
        assert_eq!(buf.value(), "one three");
        assert!(buf.yank_pop());
        assert_eq!(buf.value(), "one two ");
    }

    #[test]
    fn yank_pop_only_immediately_after_yank() {
        let mut buf = TextBuffer::new("one two");
        buf.move_end();
        buf.remove_word_before_cursor();
        buf.move_home();
        assert!(buf.yank());
        buf.push_char('x');
        assert!(!buf.yank_pop());
    }

    #[test]
    fn kill_ring_survives_clear() {
        let mut buf = TextBuffer::new("keep me");
        buf.move_end();
        buf.kill_to_start_of_line();
        buf.clear();
        assert!(buf.yank());
        assert_eq!(buf.value(), "keep me");
    }

    #[test]
    fn word_kill_joins_ring_entries() {
        let mut buf = TextBuffer::new("alpha beta");
        buf.move_end();
        buf.remove_word_before_cursor();
        assert!(buf.yank());
        assert_eq!(buf.value(), "alpha beta");
    }

    #[test]
    fn noop_delete_leaves_no_dead_undo_step() {
        let mut buf = TextBuffer::new("");
        buf.remove_char(); // backspace on empty buffer — nothing happens
        buf.delete_char();
        buf.push_char('x');
        assert!(buf.undo());
        assert_eq!(buf.value(), "");
        assert!(!buf.undo(), "no-op deletes must not record undo snapshots");
    }

    #[test]
    fn yank_pop_spans_tab_expanded_kill() {
        // `set_text` can land raw tabs in the buffer (history, editor,
        // steering restore); a kill then stores them raw while yank
        // inserts the expanded form — the tracked span must follow the
        // inserted text, not the raw kill text, or the next pop removes
        // too little and leaves residue.
        let mut buf = TextBuffer::new("");
        buf.set_text("a\tb");
        buf.move_home();
        buf.kill_to_end_of_line();
        buf.set_text("x");
        buf.move_end();
        buf.kill_to_start_of_line();

        assert!(buf.yank());
        assert_eq!(buf.value(), "x");
        assert!(buf.yank_pop());
        assert_eq!(buf.value(), "a  b");
        assert!(buf.yank_pop());
        assert_eq!(buf.value(), "x");
    }
}
