//! Context-scoped key resolution for the chat layer.
//!
//! Key events normalize into [`KeyStroke`]s, then [`EffectiveKeymap::resolve`]
//! walks the active context stack (most specific first) and returns the bound
//! [`KeyAction`]. The effective map is [`BINDINGS`] merged with the user's
//! `keymap.toml` overrides (see [`file`]). Unbound keys return `None`; the
//! caller decides whether a raw key may still reach the composer (plain text
//! input only — modified chords never fall through to the buffer).

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::components::keybindings::{KeyLabel, KeybindContext, Platform};
use crate::keymap::file::{KeymapWarning, UserKeymap};

pub mod file;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyStroke {
    /// Normalize an event for lookup: `BackTab` is `Tab`+Shift, and a
    /// shifted `Char` folds to lowercase so `ctrl+shift+c` matches whether
    /// the terminal reports `C` or `c`. Terminals fold Shift into the
    /// codepoint two ways — keeping the SHIFT flag, or substituting the
    /// shifted char and clearing the flag (Kitty `REPORT_ALTERNATE_KEYS`
    /// via crossterm) — so an uppercase char re-derives SHIFT.
    #[must_use]
    pub fn normalize(key: KeyEvent) -> Self {
        let (code, modifiers) = match key.code {
            KeyCode::BackTab => (KeyCode::Tab, key.modifiers | KeyModifiers::SHIFT),
            _ => (key.code, key.modifiers),
        };
        Self::normalize_parts(code, modifiers)
    }

    /// Normalize a raw code+modifiers pair — the same fold `normalize`
    /// applies to events. Use on stored bindings (e.g. Lua `<C-T>`
    /// registers `Char('T')+CONTROL`, shift carried in the codepoint) so
    /// both sides of a comparison share one canonical shape.
    #[must_use]
    pub fn normalize_parts(code: KeyCode, modifiers: KeyModifiers) -> Self {
        match code {
            KeyCode::Char(c) if c.is_uppercase() => {
                let lower = c.to_lowercase().next().unwrap_or_else(|| c);
                Self {
                    code: KeyCode::Char(lower),
                    modifiers: modifiers | KeyModifiers::SHIFT,
                }
            }
            _ => Self { code, modifiers },
        }
    }
}

/// Canonical dash notation — the `keymap.toml` spelling. Round-trips with
/// `file::parse_stroke`: `Char('x')+CONTROL|SHIFT` renders `ctrl-shift-x`.
impl fmt::Display for KeyStroke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mods = self.modifiers;
        if mods.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl-")?;
        }
        if mods.contains(KeyModifiers::ALT) {
            f.write_str("alt-")?;
        }
        if mods.contains(KeyModifiers::SHIFT) {
            f.write_str("shift-")?;
        }
        if mods.contains(KeyModifiers::SUPER) {
            f.write_str("super-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::BackTab => f.write_str("backtab"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("delete"),
            KeyCode::Insert => f.write_str("insert"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pageup"),
            KeyCode::PageDown => f.write_str("pagedown"),
            KeyCode::F(n) => write!(f, "f{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

macro_rules! stroke {
    (char $c:expr, $mods:expr) => {
        KeyStroke {
            code: KeyCode::Char($c),
            modifiers: $mods,
        }
    };
    ($code:expr, $mods:expr) => {
        KeyStroke {
            code: $code,
            modifiers: $mods,
        }
    };
}

macro_rules! ctrl {
    ($c:expr) => {
        stroke!(char $c, KeyModifiers::CONTROL)
    };
}

macro_rules! ctrl_shift {
    ($c:expr) => {
        stroke!(
            char $c,
            KeyModifiers::from_bits_truncate(
                KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits()
            )
        )
    };
}

macro_rules! alt {
    ($c:expr) => {
        stroke!(char $c, KeyModifiers::ALT)
    };
}

macro_rules! alt_shift {
    ($c:expr) => {
        stroke!(
            char $c,
            KeyModifiers::from_bits_truncate(KeyModifiers::ALT.bits() | KeyModifiers::SHIFT.bits())
        )
    };
}

macro_rules! plain {
    ($code:ident) => {
        stroke!(KeyCode::$code, KeyModifiers::NONE)
    };
}

macro_rules! modified {
    ($code:ident, $mods:expr) => {
        stroke!(KeyCode::$code, $mods)
    };
}

/// Every dispatchable chat-layer action. `perform` in `app` maps these onto
/// concrete behavior; context decides which binding produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum KeyAction {
    // App-wide
    QuitOrCancel,
    HelpToggle,
    Redraw,
    Suspend,
    // Chat navigation
    ChatPrev,
    ChatNext,
    ScrollHalfUp,
    ScrollHalfDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    // Surfaces
    SearchOpen,
    TranscriptDetails,
    ThinkingCycle,
    TasksOpen,
    PlanToggle,
    EditorOpenInput,
    EditorOpenPlan,
    CopySelection,
    ImagePaste,
    QueuePop,
    // Composer lifecycle
    Submit,
    Newline,
    TabOrMode,
    Escape,
    ExitOrDeleteChar,
    StashToggle,
    HistorySearch,
    // Cursor + history
    InputUp,
    InputDown,
    CharLeft,
    CharRight,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    // Deletion + kill ring
    DeleteCharBack,
    DeleteCharForward,
    DeleteWordBack,
    DeleteWordForward,
    KillLineEnd,
    KillLineStart,
    Yank,
    YankPop,
    Undo,
    Redo,
    // Streaming overrides
    CancelAgent,
    // Subagent chat
    SubagentBack,
    SubagentEscape,
    // Inline history search
    HistorySearchAccept,
    HistorySearchCancel,
    HistorySearchOlder,
    HistorySearchNewer,
    HistorySearchBackspace,
}

pub struct KeyBinding {
    pub stroke: KeyStroke,
    pub action: KeyAction,
    /// `None` hides the binding from the help listing (aliases).
    pub label: Option<KeyLabel>,
    pub description: &'static str,
    pub platform: Platform,
}

macro_rules! bind {
    ($stroke:expr, $action:expr, $label:expr, $desc:expr) => {
        KeyBinding {
            stroke: $stroke,
            action: $action,
            label: $label,
            description: $desc,
            platform: Platform::All,
        }
    };
    ($stroke:expr, $action:expr, $label:expr, $desc:expr, unix) => {
        KeyBinding {
            stroke: $stroke,
            action: $action,
            label: $label,
            description: $desc,
            platform: Platform::UnixOnly,
        }
    };
}

/// Default chat-layer keymap. Resolution walks each context's slice in
/// order, so ordering inside a context matters only for duplicates.
/// Modifier-free editing keys (arrows, Home/End, Backspace) are bound so the
/// composer never has to guess which keys are "text".
pub static BINDINGS: &[(KeybindContext, &[KeyBinding])] = &[
    (
        KeybindContext::Streaming,
        &[bind!(
            plain!(Esc),
            KeyAction::CancelAgent,
            Some(KeyLabel::Single("Esc")),
            "Interrupt agent"
        )],
    ),
    (
        KeybindContext::SubagentChat,
        &[
            bind!(
                plain!(Left),
                KeyAction::SubagentBack,
                Some(KeyLabel::Single("←")),
                "Back to main chat"
            ),
            bind!(
                plain!(Esc),
                KeyAction::SubagentEscape,
                Some(KeyLabel::Single("Esc")),
                "Back / cancel subagent"
            ),
        ],
    ),
    (
        KeybindContext::HistorySearch,
        &[
            bind!(
                plain!(Enter),
                KeyAction::HistorySearchAccept,
                Some(KeyLabel::Alt("Enter", "Tab")),
                "Accept match"
            ),
            bind!(
                plain!(Tab),
                KeyAction::HistorySearchAccept,
                None,
                "Accept match"
            ),
            bind!(
                plain!(Esc),
                KeyAction::HistorySearchCancel,
                Some(KeyLabel::Single("Esc")),
                "Cancel search"
            ),
            bind!(
                plain!(Up),
                KeyAction::HistorySearchOlder,
                Some(KeyLabel::Alt("↑", "Ctrl+R")),
                "Older match"
            ),
            bind!(
                ctrl!('r'),
                KeyAction::HistorySearchOlder,
                None,
                "Older match"
            ),
            bind!(
                plain!(Down),
                KeyAction::HistorySearchNewer,
                Some(KeyLabel::Single("↓")),
                "Newer match"
            ),
            bind!(
                plain!(Backspace),
                KeyAction::HistorySearchBackspace,
                None,
                "Edit query"
            ),
        ],
    ),
    (
        KeybindContext::Editing,
        &[
            bind!(
                plain!(Enter),
                KeyAction::Submit,
                Some(KeyLabel::Single("Enter")),
                "Submit prompt"
            ),
            bind!(
                modified!(Enter, KeyModifiers::SHIFT),
                KeyAction::Newline,
                Some(KeyLabel::MacMulti(
                    &["Shift+Enter", "\\+Enter", "Ctrl+J"],
                    &["⇧↵", "⌃J", "⌥↵"]
                )),
                "Newline"
            ),
            bind!(
                modified!(Enter, KeyModifiers::CONTROL),
                KeyAction::Newline,
                None,
                "Newline"
            ),
            bind!(
                modified!(Enter, KeyModifiers::ALT),
                KeyAction::Newline,
                None,
                "Newline"
            ),
            bind!(
                modified!(
                    Enter,
                    KeyModifiers::from_bits_truncate(
                        KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits()
                    )
                ),
                KeyAction::Newline,
                None,
                "Newline"
            ),
            bind!(ctrl!('j'), KeyAction::Newline, None, "Newline"),
            bind!(
                plain!(Tab),
                KeyAction::TabOrMode,
                Some(KeyLabel::Single("Tab")),
                "Toggle mode / queue message"
            ),
            bind!(
                plain!(Esc),
                KeyAction::Escape,
                Some(KeyLabel::Single("Esc Esc")),
                "Clear draft / rewind"
            ),
            bind!(
                plain!(Up),
                KeyAction::InputUp,
                Some(KeyLabel::Alt("↑", "↓")),
                "History / cursor"
            ),
            bind!(plain!(Down), KeyAction::InputDown, None, "History / cursor"),
            bind!(plain!(Left), KeyAction::CharLeft, None, "Cursor left"),
            bind!(plain!(Right), KeyAction::CharRight, None, "Cursor right"),
            bind!(
                plain!(Home),
                KeyAction::LineStart,
                Some(KeyLabel::Alt("Home", "End")),
                "Start / end of line"
            ),
            bind!(plain!(End), KeyAction::LineEnd, None, "End of line"),
            bind!(
                plain!(Backspace),
                KeyAction::DeleteCharBack,
                None,
                "Delete char back"
            ),
            bind!(
                plain!(Delete),
                KeyAction::DeleteCharForward,
                Some(KeyLabel::Single("Del")),
                "Delete char forward"
            ),
            bind!(
                ctrl!('a'),
                KeyAction::LineStart,
                Some(KeyLabel::Single("Ctrl+A")),
                "Jump to start of line"
            ),
            bind!(
                ctrl!('e'),
                KeyAction::LineEnd,
                Some(KeyLabel::Single("Ctrl+E")),
                "Jump to end of line"
            ),
            bind!(
                ctrl!('w'),
                KeyAction::DeleteWordBack,
                Some(KeyLabel::MacAlt("Ctrl+W / Ctrl+Bksp", "⌥⌫")),
                "Delete word backward"
            ),
            bind!(
                modified!(Backspace, KeyModifiers::CONTROL),
                KeyAction::DeleteWordBack,
                None,
                ""
            ),
            bind!(
                modified!(Backspace, KeyModifiers::ALT),
                KeyAction::DeleteWordBack,
                None,
                ""
            ),
            bind!(
                modified!(Delete, KeyModifiers::CONTROL),
                KeyAction::DeleteWordForward,
                Some(KeyLabel::Alt("Ctrl+Del", "Alt+D")),
                "Delete word forward"
            ),
            bind!(
                modified!(Delete, KeyModifiers::ALT),
                KeyAction::DeleteWordForward,
                None,
                ""
            ),
            bind!(
                alt!('d'),
                KeyAction::DeleteWordForward,
                None,
                "Delete word forward"
            ),
            bind!(
                modified!(Left, KeyModifiers::CONTROL),
                KeyAction::WordLeft,
                Some(KeyLabel::Alt("Ctrl+←", "Ctrl+→")),
                "Move word left / right"
            ),
            bind!(
                modified!(Right, KeyModifiers::CONTROL),
                KeyAction::WordRight,
                None,
                ""
            ),
            bind!(
                modified!(Left, KeyModifiers::ALT),
                KeyAction::WordLeft,
                Some(KeyLabel::Alt("Alt+←", "Alt+→")),
                "Move word left / right"
            ),
            bind!(
                modified!(Right, KeyModifiers::ALT),
                KeyAction::WordRight,
                None,
                ""
            ),
            bind!(alt!('b'), KeyAction::WordLeft, None, "Move word left"),
            bind!(alt!('f'), KeyAction::WordRight, None, "Move word right"),
            bind!(
                ctrl!('k'),
                KeyAction::KillLineEnd,
                Some(KeyLabel::Single("Ctrl+K")),
                "Delete to end of line"
            ),
            bind!(
                ctrl!('u'),
                KeyAction::KillLineStart,
                Some(KeyLabel::Single("Ctrl+U")),
                "Delete to start of line"
            ),
            bind!(
                ctrl!('y'),
                KeyAction::Yank,
                Some(KeyLabel::Single("Ctrl+Y")),
                "Paste deleted text"
            ),
            bind!(
                alt!('y'),
                KeyAction::YankPop,
                Some(KeyLabel::Single("Alt+Y")),
                "Cycle paste history"
            ),
            bind!(
                ctrl!('_'),
                KeyAction::Undo,
                Some(KeyLabel::Alt("Ctrl+_", "Ctrl+-")),
                "Undo last edit"
            ),
            bind!(ctrl!('-'), KeyAction::Undo, None, "Undo last edit"),
            bind!(
                ctrl_shift!('z'),
                KeyAction::Redo,
                Some(KeyLabel::Single("Ctrl+Shift+Z")),
                "Redo edit"
            ),
            bind!(alt_shift!('z'), KeyAction::Redo, None, "Redo edit"),
            bind!(
                ctrl!('s'),
                KeyAction::StashToggle,
                Some(KeyLabel::Single("Ctrl+S")),
                "Stash / restore draft"
            ),
            bind!(
                ctrl!('r'),
                KeyAction::HistorySearch,
                Some(KeyLabel::Single("Ctrl+R")),
                "Search input history"
            ),
            bind!(
                ctrl!('d'),
                KeyAction::ExitOrDeleteChar,
                Some(KeyLabel::Single("Ctrl+D")),
                "Delete char / exit"
            ),
            bind!(
                modified!(Left, KeyModifiers::SUPER),
                KeyAction::LineStart,
                None,
                ""
            ),
            bind!(
                modified!(Right, KeyModifiers::SUPER),
                KeyAction::LineEnd,
                None,
                ""
            ),
            bind!(
                modified!(Backspace, KeyModifiers::SUPER),
                KeyAction::KillLineStart,
                None,
                ""
            ),
        ],
    ),
    (
        KeybindContext::General,
        &[
            bind!(
                ctrl!('c'),
                KeyAction::QuitOrCancel,
                Some(KeyLabel::Single("Ctrl+C")),
                "Interrupt / clear / quit"
            ),
            bind!(
                ctrl!('h'),
                KeyAction::HelpToggle,
                Some(KeyLabel::Single("Ctrl+H")),
                "Show keybindings"
            ),
            bind!(
                stroke!(KeyCode::F(1), KeyModifiers::NONE),
                KeyAction::HelpToggle,
                None,
                "Show keybindings"
            ),
            bind!(
                ctrl!('l'),
                KeyAction::Redraw,
                Some(KeyLabel::Single("Ctrl+L")),
                "Redraw screen"
            ),
            bind!(
                ctrl!('z'),
                KeyAction::Suspend,
                Some(KeyLabel::Single("Ctrl+Z")),
                "Suspend process",
                unix
            ),
            bind!(
                ctrl!('p'),
                KeyAction::ChatPrev,
                Some(KeyLabel::Alt("Ctrl+N", "Ctrl+P")),
                "Next / previous task chat"
            ),
            bind!(ctrl!('n'), KeyAction::ChatNext, None, "Next task chat"),
            bind!(
                plain!(PageUp),
                KeyAction::ScrollPageUp,
                Some(KeyLabel::Alt("PageUp", "PageDown")),
                "Scroll page up / down"
            ),
            bind!(
                plain!(PageDown),
                KeyAction::ScrollPageDown,
                None,
                "Scroll page down"
            ),
            bind!(
                alt!('u'),
                KeyAction::ScrollHalfUp,
                Some(KeyLabel::Alt("Alt+U", "Alt+D")),
                "Scroll half page up / down"
            ),
            bind!(
                alt!('d'),
                KeyAction::ScrollHalfDown,
                None,
                "Scroll half page down"
            ),
            bind!(
                alt!('g'),
                KeyAction::ScrollTop,
                Some(KeyLabel::Alt("Alt+G", "Alt+Shift+G")),
                "Scroll to top / bottom"
            ),
            bind!(
                alt_shift!('g'),
                KeyAction::ScrollBottom,
                None,
                "Scroll to bottom"
            ),
            bind!(
                ctrl!('f'),
                KeyAction::SearchOpen,
                Some(KeyLabel::Single("Ctrl+F")),
                "Search messages"
            ),
            bind!(
                ctrl!('o'),
                KeyAction::TranscriptDetails,
                Some(KeyLabel::Single("Ctrl+O")),
                "Toggle transcript details"
            ),
            bind!(
                alt!('i'),
                KeyAction::TranscriptDetails,
                None,
                "Toggle transcript details"
            ),
            bind!(
                ctrl!('t'),
                KeyAction::TasksOpen,
                Some(KeyLabel::Single("Ctrl+T")),
                "Open tasks"
            ),
            bind!(
                alt!('p'),
                KeyAction::PlanToggle,
                Some(KeyLabel::Single("Alt+P")),
                "Toggle plan panel"
            ),
            bind!(
                alt_shift!('p'),
                KeyAction::EditorOpenPlan,
                Some(KeyLabel::Single("Alt+Shift+P")),
                "Open plan in editor"
            ),
            bind!(
                ctrl!('g'),
                KeyAction::EditorOpenInput,
                Some(KeyLabel::Single("Ctrl+G")),
                "Edit input in external editor"
            ),
            bind!(
                alt!('o'),
                KeyAction::EditorOpenInput,
                None,
                "Edit input in editor"
            ),
            bind!(
                ctrl_shift!('t'),
                KeyAction::ThinkingCycle,
                Some(KeyLabel::MacMulti(
                    &["Alt+T", "Ctrl+Shift+T"],
                    &["⌥T", "⌃⇧T"]
                )),
                "Cycle thinking level"
            ),
            bind!(
                alt!('t'),
                KeyAction::ThinkingCycle,
                None,
                "Cycle thinking level"
            ),
            bind!(
                ctrl_shift!('c'),
                KeyAction::CopySelection,
                Some(KeyLabel::Single("Ctrl+Shift+C")),
                "Copy selection"
            ),
            bind!(
                ctrl!('v'),
                KeyAction::ImagePaste,
                Some(KeyLabel::Single("Ctrl+V")),
                "Paste image from clipboard"
            ),
            bind!(
                ctrl!('q'),
                KeyAction::QueuePop,
                Some(KeyLabel::Single("Ctrl+Q")),
                "Pop queue"
            ),
        ],
    ),
];

/// One stroke→action claim in the effective map. `from_user` marks claims
/// written by `keymap.toml`; during the merge a user claim may displace a
/// default claim on the same stroke but never an earlier user claim.
#[derive(Debug, Clone, Copy)]
struct EffectiveBinding {
    stroke: KeyStroke,
    action: KeyAction,
    platform: Platform,
    from_user: bool,
}

/// `BINDINGS` merged with the user's `keymap.toml` overrides, built once
/// per UI generation and held by `App`. [`Self::resolve`] walks the
/// context stack against this map; [`BINDINGS`] stays the compiled-in
/// default layer and the help/docgen source.
#[derive(Debug)]
pub struct EffectiveKeymap {
    contexts: Vec<(KeybindContext, Vec<EffectiveBinding>)>,
}

impl EffectiveKeymap {
    /// Merge `BINDINGS` with `user` overrides. Each entry replaces its
    /// action's whole key list in that context. Strokes are then claimed
    /// in file order: a user stroke displaces a default claim (warning)
    /// but loses to an earlier user claim (warning).
    #[must_use]
    pub fn build(user: &UserKeymap) -> (Self, Vec<KeymapWarning>) {
        let mut contexts: Vec<(KeybindContext, Vec<EffectiveBinding>)> = BINDINGS
            .iter()
            .map(|(ctx, bindings)| {
                (
                    *ctx,
                    bindings
                        .iter()
                        .map(|b| EffectiveBinding {
                            stroke: KeyStroke::normalize_parts(b.stroke.code, b.stroke.modifiers),
                            action: b.action,
                            platform: b.platform,
                            from_user: false,
                        })
                        .collect(),
                )
            })
            .collect();
        let mut warnings = Vec::new();
        for entry in &user.entries {
            let Some((_, bindings)) = contexts.iter_mut().find(|(ctx, _)| *ctx == entry.context)
            else {
                continue;
            };
            bindings.retain(|b| b.action != entry.action);
            for &stroke in &entry.strokes {
                match bindings.iter_mut().find(|b| b.stroke == stroke) {
                    Some(existing) if existing.action == entry.action => {}
                    Some(existing) if existing.from_user => {
                        warnings.push(KeymapWarning::Conflict {
                            context: entry.context.name(),
                            key: stroke.to_string(),
                            kept: file::action_name(existing.action),
                            dropped: file::action_name(entry.action),
                        });
                    }
                    Some(existing) => {
                        warnings.push(KeymapWarning::Shadowed {
                            context: entry.context.name(),
                            key: stroke.to_string(),
                            previous: file::action_name(existing.action),
                            action: file::action_name(entry.action),
                        });
                        existing.action = entry.action;
                        existing.platform = Platform::All;
                        existing.from_user = true;
                    }
                    None => bindings.push(EffectiveBinding {
                        stroke,
                        action: entry.action,
                        platform: Platform::All,
                        from_user: true,
                    }),
                }
            }
        }
        (Self { contexts }, warnings)
    }

    /// Resolve a key event against the active context stack. Returns the
    /// first matching binding's action, most specific context first.
    #[must_use]
    pub fn resolve(&self, stack: &[KeybindContext], key: KeyEvent) -> Option<KeyAction> {
        let stroke = KeyStroke::normalize(key);
        for ctx in stack {
            let Some((_, bindings)) = self.contexts.iter().find(|(c, _)| c == ctx) else {
                continue;
            };
            if let Some(binding) = bindings
                .iter()
                .find(|binding| binding.stroke == stroke && binding.platform.is_visible())
            {
                return Some(binding.action);
            }
        }
        self.enter_fallback(stack, key)
    }

    /// The binding table can't enumerate every Enter modifier combo, and
    /// the pre-keymap composer treated them all: any of Shift/Ctrl/Alt
    /// meant newline, Enter with anything else (e.g. `Super`) submitted.
    /// The blanket is part of the *default* layer: once `keymap.toml`
    /// rewrites `submit` or `newline`, the action's claims are all
    /// `from_user` and the fallback dies — a rebind really hands those
    /// keys back to the composer instead of silently keeping them.
    fn enter_fallback(&self, stack: &[KeybindContext], key: KeyEvent) -> Option<KeyAction> {
        const NEWLINE_MODS: KeyModifiers = KeyModifiers::SHIFT
            .union(KeyModifiers::CONTROL)
            .union(KeyModifiers::ALT);
        if key.code != KeyCode::Enter
            || !stack.contains(&KeybindContext::Editing)
            || stack.contains(&KeybindContext::HistorySearch)
        {
            return None;
        }
        let multiline = key.modifiers.intersects(NEWLINE_MODS);
        let action = if multiline {
            KeyAction::Newline
        } else {
            KeyAction::Submit
        };
        let default_enter_survives = self
            .contexts
            .iter()
            .find(|(ctx, _)| *ctx == KeybindContext::Editing)
            .is_some_and(|(_, bindings)| {
                bindings.iter().any(|b| {
                    b.action == action
                        && !b.from_user
                        && b.stroke.code == KeyCode::Enter
                        && b.platform.is_visible()
                        && if multiline {
                            b.stroke.modifiers.intersects(NEWLINE_MODS)
                        } else {
                            b.stroke.modifiers == KeyModifiers::NONE
                        }
                })
            });
        default_enter_survives.then_some(action)
    }
}

impl Default for EffectiveKeymap {
    fn default() -> Self {
        Self::build(&UserKeymap::default()).0
    }
}

/// Resolve against the compiled-in defaults — `BINDINGS` with no user
/// overrides applied.
#[cfg(test)]
fn resolve(stack: &[KeybindContext], key: KeyEvent) -> Option<KeyAction> {
    static DEFAULT: std::sync::LazyLock<EffectiveKeymap> =
        std::sync::LazyLock::new(EffectiveKeymap::default);
    DEFAULT.resolve(stack, key)
}

/// Keys that may still reach the composer when unbound: printable input and
/// paste-style events only. Modified chords that match no binding are dead
/// on purpose — they must not leak into the buffer as editing commands.
/// `Char`+Ctrl+Alt is `AltGr` on several layouts: it composes printable text
/// (`\`, `@`, `$`, `€`), so it always counts as input, never a chord.
#[must_use]
pub fn reaches_composer(key: &KeyEvent) -> bool {
    let mods = key.modifiers;
    if matches!(key.code, KeyCode::Char(_))
        && mods.contains(KeyModifiers::CONTROL)
        && mods.contains(KeyModifiers::ALT)
    {
        return true;
    }
    !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn editing_binds_resolve_in_editing_context() {
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Some(KeyAction::KillLineStart)
        );
        assert_eq!(
            resolve(&stack, key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(KeyAction::Submit)
        );
    }

    #[test]
    fn general_binds_resolve_through_editing() {
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(KeyAction::QuitOrCancel)
        );
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::TasksOpen)
        );
    }

    #[test]
    fn streaming_esc_overrides_editing_esc() {
        let stack = [
            KeybindContext::Streaming,
            KeybindContext::Editing,
            KeybindContext::General,
        ];
        assert_eq!(
            resolve(&stack, key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(KeyAction::CancelAgent)
        );
    }

    #[test]
    fn unbound_ctrl_resolves_nothing() {
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('m'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn unbound_modified_keys_never_reach_composer() {
        assert!(!reaches_composer(&key(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        )));
        assert!(!reaches_composer(&key(
            KeyCode::Char('x'),
            KeyModifiers::ALT
        )));
        assert!(reaches_composer(&key(
            KeyCode::Char('x'),
            KeyModifiers::NONE
        )));
        assert!(reaches_composer(&key(
            KeyCode::Backspace,
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn shift_char_normalizes_for_matching() {
        let stack = [KeybindContext::Editing, KeybindContext::General];
        let event = key(
            KeyCode::Char('C'),
            KeyModifiers::from_bits_truncate(
                KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits(),
            ),
        );
        assert_eq!(resolve(&stack, event), Some(KeyAction::CopySelection));
    }

    #[test]
    fn shifted_codepoint_without_shift_flag_still_matches() {
        // Kitty `REPORT_ALTERNATE_KEYS` via crossterm substitutes the
        // shifted codepoint AND clears the SHIFT flag: Ctrl+Shift+C arrives
        // as `Char('C')+CONTROL`. Normalization must re-derive SHIFT or
        // every ctrl-shift / alt-shift binding is dead on those terminals.
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            Some(KeyAction::CopySelection),
            "ctrl+shift+c with folded shift must not hit ctrl+c quit"
        );
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('G'), KeyModifiers::ALT)),
            Some(KeyAction::ScrollBottom)
        );
        assert_eq!(
            resolve(&stack, key(KeyCode::Char('P'), KeyModifiers::ALT)),
            Some(KeyAction::EditorOpenPlan)
        );
    }

    #[test]
    fn enter_modifier_fallback_covers_unenumerated_combos() {
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            resolve(
                &stack,
                key(
                    KeyCode::Enter,
                    KeyModifiers::from_bits_truncate(
                        KeyModifiers::ALT.bits() | KeyModifiers::SHIFT.bits()
                    )
                )
            ),
            Some(KeyAction::Newline)
        );
        assert_eq!(
            resolve(&stack, key(KeyCode::Enter, KeyModifiers::SUPER)),
            Some(KeyAction::Submit)
        );
    }

    #[test]
    fn enter_fallback_stays_out_of_history_search() {
        let stack = [
            KeybindContext::HistorySearch,
            KeybindContext::Editing,
            KeybindContext::General,
        ];
        // Table-bound combos still resolve (Shift+Enter accepts the match
        // then newlines, via edit_action's accept-first rule)…
        assert_eq!(
            resolve(&stack, key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(KeyAction::Newline)
        );
        // …but the fallback never fires — Super+Enter mid-search can't
        // submit the shown match by accident.
        assert_eq!(
            resolve(&stack, key(KeyCode::Enter, KeyModifiers::SUPER)),
            None
        );
    }
}
