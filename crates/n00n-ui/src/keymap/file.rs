//! `keymap.toml` — user overrides layered over the compiled-in [`BINDINGS`].
//!
//! One table per dispatch context; each entry maps an action to a stroke
//! or a list of strokes and replaces the action's whole default key list.
//! An empty list unbinds the action; actions left out keep their defaults.
//!
//! ```toml
//! [editing]
//! submit = "enter"
//! newline = ["shift-enter", "ctrl-j"]
//!
//! [general]
//! quit = "ctrl-c"
//! tasks = []            # unbind ctrl-t
//! ```
//!
//! Strokes use dash notation: `ctrl-x`, `alt-x`, `shift-enter`, combined
//! like `ctrl-shift-x`, bare chars, or named keys — `enter`, `esc`, `tab`,
//! `backtab`, `backspace`, `delete`, `insert`, `space`, arrows, `home`,
//! `end`, `pageup`, `pagedown`, `f1`-`f12`.
//!
//! Strokes are claimed in file order: a user stroke displaces a default
//! binding on the same key (warning) but loses to an earlier user claim
//! (warning). `ctrl-z` and the `suspend` action are reserved — suspend is
//! handled before keymap resolution. Only contexts dispatching through
//! `keymap` are writable: `general`, `editing`, `streaming`,
//! `subagent_chat`, `history_search`. Overlay surfaces (pickers, modals,
//! forms) own their keys and are rejected with a warning. Lua
//! `n00n.keymap.set` callbacks still win over this file at dispatch time.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};
use toml_edit::{DocumentMut, Item};

use crate::components::keybindings::KeybindContext;
use crate::keymap::{BINDINGS, EffectiveKeymap, KeyAction, KeyStroke};

/// File name searched in each n00n config directory (next to `init.lua`).
const KEYMAP_FILE_NAME: &str = "keymap.toml";

/// `ctrl-z` is claimed by the raw suspend check in `App::handle_key`
/// before keymap resolution, so a file binding can never take effect.
const SUSPEND_STROKE: KeyStroke = KeyStroke {
    code: KeyCode::Char('z'),
    modifiers: KeyModifiers::CONTROL,
};

/// Writable actions per context, in help-file spelling. Each action lives
/// in exactly one context — the one [`BINDINGS`] binds it in — so the
/// table doubles as the context-membership check for the file format.
const ACTIONS: &[(KeybindContext, &[(&str, KeyAction)])] = &[
    (
        KeybindContext::General,
        &[
            ("quit", KeyAction::QuitOrCancel),
            ("help", KeyAction::HelpToggle),
            ("redraw", KeyAction::Redraw),
            ("suspend", KeyAction::Suspend),
            ("chat_prev", KeyAction::ChatPrev),
            ("chat_next", KeyAction::ChatNext),
            ("scroll_page_up", KeyAction::ScrollPageUp),
            ("scroll_page_down", KeyAction::ScrollPageDown),
            ("scroll_half_up", KeyAction::ScrollHalfUp),
            ("scroll_half_down", KeyAction::ScrollHalfDown),
            ("scroll_top", KeyAction::ScrollTop),
            ("scroll_bottom", KeyAction::ScrollBottom),
            ("search", KeyAction::SearchOpen),
            ("transcript_details", KeyAction::TranscriptDetails),
            ("thinking", KeyAction::ThinkingCycle),
            ("tasks", KeyAction::TasksOpen),
            ("plan", KeyAction::PlanToggle),
            ("edit_input", KeyAction::EditorOpenInput),
            ("edit_plan", KeyAction::EditorOpenPlan),
            ("copy", KeyAction::CopySelection),
            ("paste_image", KeyAction::ImagePaste),
            ("queue_pop", KeyAction::QueuePop),
        ],
    ),
    (
        KeybindContext::Editing,
        &[
            ("submit", KeyAction::Submit),
            ("newline", KeyAction::Newline),
            ("tab", KeyAction::TabOrMode),
            ("escape", KeyAction::Escape),
            ("exit_or_delete", KeyAction::ExitOrDeleteChar),
            ("stash", KeyAction::StashToggle),
            ("history_search", KeyAction::HistorySearch),
            ("input_up", KeyAction::InputUp),
            ("input_down", KeyAction::InputDown),
            ("char_left", KeyAction::CharLeft),
            ("char_right", KeyAction::CharRight),
            ("word_left", KeyAction::WordLeft),
            ("word_right", KeyAction::WordRight),
            ("line_start", KeyAction::LineStart),
            ("line_end", KeyAction::LineEnd),
            ("delete_char_back", KeyAction::DeleteCharBack),
            ("delete_char_forward", KeyAction::DeleteCharForward),
            ("delete_word_back", KeyAction::DeleteWordBack),
            ("delete_word_forward", KeyAction::DeleteWordForward),
            ("kill_line_end", KeyAction::KillLineEnd),
            ("kill_line_start", KeyAction::KillLineStart),
            ("yank", KeyAction::Yank),
            ("yank_pop", KeyAction::YankPop),
            ("undo", KeyAction::Undo),
            ("redo", KeyAction::Redo),
        ],
    ),
    (
        KeybindContext::Streaming,
        &[("cancel", KeyAction::CancelAgent)],
    ),
    (
        KeybindContext::SubagentChat,
        &[
            ("back", KeyAction::SubagentBack),
            ("escape", KeyAction::SubagentEscape),
        ],
    ),
    (
        KeybindContext::HistorySearch,
        &[
            ("accept", KeyAction::HistorySearchAccept),
            ("cancel", KeyAction::HistorySearchCancel),
            ("older", KeyAction::HistorySearchOlder),
            ("newer", KeyAction::HistorySearchNewer),
            ("backspace", KeyAction::HistorySearchBackspace),
        ],
    ),
];

/// One validated `action = "keys"` entry: replaces the action's default
/// key list inside its context.
#[derive(Debug)]
pub struct UserEntry {
    pub context: KeybindContext,
    pub action: KeyAction,
    /// Already normalized; empty means "unbind the action".
    pub strokes: Vec<KeyStroke>,
}

/// Parsed, validated contents of one `keymap.toml`.
#[derive(Debug, Default)]
pub struct UserKeymap {
    /// Entries in file order — claim order for stroke conflicts.
    pub entries: Vec<UserEntry>,
}

/// Non-fatal `keymap.toml` problem. Startup keeps going with whatever
/// could still be applied; warnings are logged and flashed in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapWarning {
    /// File exists but could not be read.
    Io { path: PathBuf, error: String },
    /// File is not valid TOML; nothing from it applied.
    Parse { path: PathBuf, error: String },
    /// Top-level table name is not a known context.
    UnknownContext { context: String },
    /// Known context, but its keys are owned by the overlay/component.
    ContextNotRemappable { context: &'static str },
    /// A context entry that is not a TOML table.
    ExpectedTable { context: &'static str },
    /// Action name not defined in this context (`defined_in` points to
    /// the context that owns it, when the name is known elsewhere).
    UnknownAction {
        context: &'static str,
        action: String,
        defined_in: Option<&'static str>,
    },
    /// `suspend` is handled before keymap resolution and cannot move.
    ReservedAction {
        context: &'static str,
        action: String,
    },
    /// Value was not a key string or a list of key strings.
    InvalidValue {
        context: &'static str,
        action: String,
    },
    /// One key string failed to parse; that stroke was skipped.
    InvalidKey {
        context: &'static str,
        action: String,
        key: String,
        reason: String,
    },
    /// The parsed stroke is `ctrl-z`; skipped.
    ReservedKey {
        context: &'static str,
        action: String,
        key: String,
    },
    /// Two user actions claim the same stroke; the earlier entry wins.
    Conflict {
        context: &'static str,
        key: String,
        kept: &'static str,
        dropped: &'static str,
    },
    /// A user claim displaced a default binding on the same stroke.
    Shadowed {
        context: &'static str,
        key: String,
        previous: &'static str,
        action: &'static str,
    },
}

impl fmt::Display for KeymapWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => {
                write!(f, "failed to read {}: {error}", path.display())
            }
            Self::Parse { path, error } => {
                write!(f, "failed to parse {}: {error}", path.display())
            }
            Self::UnknownContext { context } => {
                write!(f, "unknown context '[{context}]'")
            }
            Self::ContextNotRemappable { context } => write!(
                f,
                "context '[{context}]' is owned by its overlay and cannot be remapped"
            ),
            Self::ExpectedTable { context } => {
                write!(f, "'{context}' must be a table of action = \"key\" entries")
            }
            Self::UnknownAction {
                context,
                action,
                defined_in,
            } => match defined_in {
                Some(home) => write!(
                    f,
                    "unknown action '{action}' in '[{context}]' (it belongs to '[{home}]')"
                ),
                None => write!(f, "unknown action '{action}' in '[{context}]'"),
            },
            Self::ReservedAction { context, action } => {
                write!(f, "'[{context}] {action}': suspend is not remappable")
            }
            Self::InvalidValue { context, action } => write!(
                f,
                "'[{context}] {action}': expected \"key\", [\"k1\", \"k2\"], or [] to unbind"
            ),
            Self::InvalidKey {
                context,
                action,
                key,
                reason,
            } => write!(f, "'[{context}] {action}': invalid key '{key}': {reason}"),
            Self::ReservedKey {
                context,
                action,
                key,
            } => write!(f, "'[{context}] {action}': '{key}' is reserved for suspend"),
            Self::Conflict {
                context,
                key,
                kept,
                dropped,
            } => write!(
                f,
                "'[{context}]': '{key}' claimed by both '{kept}' and '{dropped}', keeping '{kept}'"
            ),
            Self::Shadowed {
                context,
                key,
                previous,
                action,
            } => write!(
                f,
                "'[{context}]': '{key}' moved from '{previous}' to '{action}'"
            ),
        }
    }
}

/// Load `keymap.toml` from the first config dir containing it — same
/// first-found precedence as `init.lua`. A missing file is not a warning:
/// defaults apply. Read/parse problems and bad entries all degrade to
/// warnings; the returned map always resolves, if only to `BINDINGS`.
#[must_use]
pub fn load(dirs: &[PathBuf]) -> (EffectiveKeymap, Vec<KeymapWarning>) {
    for dir in dirs {
        let path = dir.join(KEYMAP_FILE_NAME);
        if path.is_file() {
            return load_from(&path);
        }
    }
    (EffectiveKeymap::default(), Vec::new())
}

fn load_from(path: &Path) -> (EffectiveKeymap, Vec<KeymapWarning>) {
    match fs::read_to_string(path) {
        Ok(source) => {
            let (user, mut warnings) = parse(&source, path);
            let (effective, merge_warnings) = EffectiveKeymap::build(&user);
            warnings.extend(merge_warnings);
            (effective, warnings)
        }
        Err(error) => (
            EffectiveKeymap::default(),
            vec![KeymapWarning::Io {
                path: path.to_path_buf(),
                error: error.to_string(),
            }],
        ),
    }
}

/// Parse `keymap.toml` contents into validated entries plus warnings.
/// `toml_edit` keeps the document's original key order — required for the
/// first-in-file-wins conflict rule. Never fails: malformed input degrades
/// to warnings and whatever entries still applied.
#[must_use]
pub fn parse(source: &str, path: &Path) -> (UserKeymap, Vec<KeymapWarning>) {
    let mut warnings = Vec::new();
    let doc: DocumentMut = match source.parse() {
        Ok(doc) => doc,
        Err(error) => {
            warnings.push(KeymapWarning::Parse {
                path: path.to_path_buf(),
                error: error.to_string(),
            });
            return (UserKeymap::default(), warnings);
        }
    };
    let mut user = UserKeymap::default();
    for (context_name, item) in doc.as_table() {
        let Some(context) = KeybindContext::from_name(context_name) else {
            warnings.push(KeymapWarning::UnknownContext {
                context: context_name.to_owned(),
            });
            continue;
        };
        if !BINDINGS.iter().any(|(ctx, _)| *ctx == context) {
            warnings.push(KeymapWarning::ContextNotRemappable {
                context: context.name(),
            });
            continue;
        }
        let Some(table) = item.as_table_like() else {
            warnings.push(KeymapWarning::ExpectedTable {
                context: context.name(),
            });
            continue;
        };
        for (action_name, value) in table.iter() {
            if let Some(entry) = parse_entry(context, action_name, value, &mut warnings) {
                user.entries.push(entry);
            }
        }
    }
    (user, warnings)
}

/// Validate one `action = value` entry. Returns `Some` for a well-formed
/// entry — including `strokes` empty (explicit unbind) — `None` when the
/// entry was rejected with a warning.
fn parse_entry(
    context: KeybindContext,
    action_name: &str,
    value: &Item,
    warnings: &mut Vec<KeymapWarning>,
) -> Option<UserEntry> {
    let Some(action) = action_from_name(context, action_name) else {
        warnings.push(KeymapWarning::UnknownAction {
            context: context.name(),
            action: action_name.to_owned(),
            defined_in: action_home(action_name),
        });
        return None;
    };
    if action == KeyAction::Suspend {
        warnings.push(KeymapWarning::ReservedAction {
            context: context.name(),
            action: action_name.to_owned(),
        });
        return None;
    }
    let raw_keys: Vec<&str> = if let Some(single) = value.as_str() {
        vec![single]
    } else if let Some(items) = value.as_array() {
        let mut keys = Vec::with_capacity(items.len());
        let mut all_strings = true;
        for item in items {
            if let Some(s) = item.as_str() {
                keys.push(s);
            } else {
                all_strings = false;
            }
        }
        if !all_strings {
            warnings.push(KeymapWarning::InvalidValue {
                context: context.name(),
                action: action_name.to_owned(),
            });
            return None;
        }
        keys
    } else {
        warnings.push(KeymapWarning::InvalidValue {
            context: context.name(),
            action: action_name.to_owned(),
        });
        return None;
    };
    let mut strokes = Vec::with_capacity(raw_keys.len());
    for raw in raw_keys {
        match parse_stroke(raw) {
            Ok(stroke) if stroke == SUSPEND_STROKE => {
                warnings.push(KeymapWarning::ReservedKey {
                    context: context.name(),
                    action: action_name.to_owned(),
                    key: raw.to_owned(),
                });
            }
            Ok(stroke) => strokes.push(stroke),
            Err(reason) => warnings.push(KeymapWarning::InvalidKey {
                context: context.name(),
                action: action_name.to_owned(),
                key: raw.to_owned(),
                reason,
            }),
        }
    }
    Some(UserEntry {
        context,
        action,
        strokes,
    })
}

/// Parse dash notation: `ctrl-x`, `alt-x`, `shift-enter`, `ctrl-alt-x`,
/// `super-left`; bare chars; or named keys. Case-insensitive modifier and
/// key names; a shifted `Char` folds to lowercase + `SHIFT` exactly like
/// `KeyStroke::normalize` does for events. Vim-style `<C-x>` is accepted
/// too — same syntax as Lua `n00n.keymap.set`, so binds copy across.
fn parse_stroke(raw: &str) -> Result<KeyStroke, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty key".into());
    }
    if s.starts_with('<') && s.ends_with('>') && s.len() > 2 {
        return parse_stroke_inner(&s[1..s.len() - 1], true);
    }
    parse_stroke_inner(s, false)
}

fn parse_stroke_inner(s: &str, angle: bool) -> Result<KeyStroke, String> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = s;
    while let Some((head, tail)) = rest.split_once('-') {
        let next = match head.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "c" if angle => KeyModifiers::CONTROL,
            "alt" | "option" => KeyModifiers::ALT,
            "a" | "m" if angle => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "s" if angle => KeyModifiers::SHIFT,
            "super" | "cmd" => KeyModifiers::SUPER,
            "hyper" => KeyModifiers::HYPER,
            _ => break,
        };
        modifiers |= next;
        rest = tail;
    }
    if rest.eq_ignore_ascii_case("backtab") {
        return Ok(KeyStroke::normalize_parts(
            KeyCode::Tab,
            modifiers | KeyModifiers::SHIFT,
        ));
    }
    let code = parse_key_code(rest)?;
    Ok(KeyStroke::normalize_parts(code, modifiers))
}

/// The key part of a stroke: a bare char or a named key.
fn parse_key_code(name: &str) -> Result<KeyCode, String> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(c));
    }
    let lower = name.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix('f')
        && let Ok(n) = digits.parse::<u8>()
    {
        if (1..=12).contains(&n) {
            return Ok(KeyCode::F(n));
        }
        return Err(format!("function key '{name}' out of range (f1-f12)"));
    }
    let code = match lower.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        _ => return Err(format!("unknown key '{name}'")),
    };
    Ok(code)
}

fn action_from_name(context: KeybindContext, name: &str) -> Option<KeyAction> {
    ACTIONS
        .iter()
        .find(|(ctx, _)| *ctx == context)
        .and_then(|(_, actions)| actions.iter().find(|(n, _)| *n == name))
        .map(|(_, action)| *action)
}

/// The context an action name belongs to, if it is a known name at all.
fn action_home(name: &str) -> Option<&'static str> {
    ACTIONS
        .iter()
        .find_map(|(ctx, actions)| actions.iter().any(|(n, _)| *n == name).then(|| ctx.name()))
}

/// File-format name of an action (reverse of [`ACTIONS`]); `"unknown"` is
/// unreachable for actions reachable through `keymap` dispatch.
pub(crate) fn action_name(action: KeyAction) -> &'static str {
    ACTIONS
        .iter()
        .flat_map(|(_, actions)| *actions)
        .find(|(_, a)| *a == action)
        .map_or("unknown", |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn parse_ok(source: &str) -> (UserKeymap, Vec<KeymapWarning>) {
        parse(source, Path::new("test.toml"))
    }

    #[test]
    fn modifier_combos_parse() {
        for (raw, code, mods) in [
            ("ctrl-x", KeyCode::Char('x'), KeyModifiers::CONTROL),
            ("control-x", KeyCode::Char('x'), KeyModifiers::CONTROL),
            ("alt-x", KeyCode::Char('x'), KeyModifiers::ALT),
            ("option-x", KeyCode::Char('x'), KeyModifiers::ALT),
            ("shift-x", KeyCode::Char('x'), KeyModifiers::SHIFT),
            (
                "ctrl-alt-x",
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (
                "alt-ctrl-x",
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (
                "ctrl-shift-x",
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (
                "shift-ctrl-x",
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (
                "ctrl-alt-shift-x",
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            ("super-x", KeyCode::Char('x'), KeyModifiers::SUPER),
            ("cmd-x", KeyCode::Char('x'), KeyModifiers::SUPER),
            ("shift-enter", KeyCode::Enter, KeyModifiers::SHIFT),
            ("ctrl-backspace", KeyCode::Backspace, KeyModifiers::CONTROL),
            ("alt-left", KeyCode::Left, KeyModifiers::ALT),
            ("ctrl-pageup", KeyCode::PageUp, KeyModifiers::CONTROL),
            ("shift-f5", KeyCode::F(5), KeyModifiers::SHIFT),
            // Vim/Lua angle notation — same spellings `n00n.keymap.set` takes.
            ("<C-t>", KeyCode::Char('t'), KeyModifiers::CONTROL),
            ("<Ctrl-x>", KeyCode::Char('x'), KeyModifiers::CONTROL),
            ("<A-x>", KeyCode::Char('x'), KeyModifiers::ALT),
            ("<M-x>", KeyCode::Char('x'), KeyModifiers::ALT),
            ("<S-Tab>", KeyCode::Tab, KeyModifiers::SHIFT),
            (
                "<C-S-a>",
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            assert_eq!(
                parse_stroke(raw),
                Ok(KeyStroke {
                    code,
                    modifiers: mods
                }),
                "{raw}"
            );
        }
    }

    #[test]
    fn named_keys_and_bare_chars_parse() {
        for (raw, code) in [
            ("a", KeyCode::Char('a')),
            ("-", KeyCode::Char('-')),
            ("?", KeyCode::Char('?')),
            ("enter", KeyCode::Enter),
            ("return", KeyCode::Enter),
            ("esc", KeyCode::Esc),
            ("escape", KeyCode::Esc),
            ("tab", KeyCode::Tab),
            ("backspace", KeyCode::Backspace),
            ("delete", KeyCode::Delete),
            ("del", KeyCode::Delete),
            ("insert", KeyCode::Insert),
            ("space", KeyCode::Char(' ')),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("pgup", KeyCode::PageUp),
            ("pagedown", KeyCode::PageDown),
            ("f1", KeyCode::F(1)),
            ("f12", KeyCode::F(12)),
            ("F5", KeyCode::F(5)),
        ] {
            assert_eq!(
                parse_stroke(raw),
                Ok(KeyStroke {
                    code,
                    modifiers: KeyModifiers::NONE
                }),
                "{raw}"
            );
        }
    }

    #[test]
    fn backtab_and_shift_tab_both_parse_to_shifted_tab() {
        for raw in ["backtab", "shift-tab", "ctrl-backtab"] {
            let stroke = parse_stroke(raw).expect("parses");
            assert_eq!(stroke.code, KeyCode::Tab, "{raw}");
            assert!(stroke.modifiers.contains(KeyModifiers::SHIFT), "{raw}");
        }
        assert_eq!(
            parse_stroke("ctrl-backtab"),
            Ok(KeyStroke {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT
            })
        );
    }

    #[test]
    fn case_and_shift_fold_like_event_normalization() {
        // `ctrl-T` folds exactly like a terminal's ctrl+shift+t event.
        assert_eq!(
            parse_stroke("ctrl-T"),
            Ok(KeyStroke {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT
            })
        );
        assert_eq!(
            parse_stroke("Z"),
            Ok(KeyStroke {
                code: KeyCode::Char('z'),
                modifiers: KeyModifiers::SHIFT
            })
        );
        assert_eq!(parse_stroke("CTRL-x"), parse_stroke("ctrl-x"));
        assert_eq!(parse_stroke("ENTER"), parse_stroke("enter"));
    }

    #[test]
    fn invalid_keys_error() {
        for raw in ["", "   ", "ctrl-", "abc", "x-y", "f0", "f13", "ctrl", "nul"] {
            assert!(parse_stroke(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn ctrl_z_parses_to_the_reserved_stroke() {
        assert_eq!(parse_stroke("ctrl-z"), Ok(SUSPEND_STROKE));
    }

    #[test]
    fn keystroke_display_round_trips() {
        for raw in [
            "ctrl-x",
            "ctrl-shift-x",
            "ctrl-alt-x",
            "shift-enter",
            "alt-left",
            "enter",
            "esc",
            "tab",
            "backspace",
            "space",
            "f5",
            "pageup",
            "a",
            "super-x",
        ] {
            let stroke = parse_stroke(raw).expect("parses");
            assert_eq!(parse_stroke(&stroke.to_string()), Ok(stroke), "{raw}");
        }
    }

    #[test]
    fn entries_parse_in_file_order() {
        let (user, warnings) = parse_ok(
            r#"
            [editing]
            submit = "enter"
            newline = ["shift-enter", "ctrl-j"]

            [general]
            quit = "ctrl-c"
            tasks = []
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(user.entries.len(), 4);
        assert_eq!(user.entries[0].action, KeyAction::Submit);
        assert_eq!(user.entries[1].action, KeyAction::Newline);
        assert_eq!(user.entries[1].strokes.len(), 2);
        assert_eq!(user.entries[2].action, KeyAction::QuitOrCancel);
        assert!(user.entries[3].strokes.is_empty());
    }

    #[test]
    fn unknown_context_warns() {
        let (user, warnings) = parse_ok("[bogus]\nquit = \"x\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::UnknownContext {
                context: "bogus".into()
            }]
        );
        assert!(user.entries.is_empty());
    }

    #[test]
    fn overlay_context_warns_not_remappable() {
        for ctx in ["picker", "command_palette", "form_input", "search"] {
            let (_, warnings) = parse_ok(&format!("[{ctx}]\nup = \"k\""));
            assert_eq!(
                warnings,
                vec![KeymapWarning::ContextNotRemappable { context: ctx }],
                "{ctx}"
            );
        }
    }

    #[test]
    fn unknown_action_warns_with_home_hint() {
        let (user, warnings) = parse_ok("[editing]\nquit = \"x\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::UnknownAction {
                context: "editing",
                action: "quit".into(),
                defined_in: Some("general"),
            }]
        );
        assert!(user.entries.is_empty());

        let (_, warnings) = parse_ok("[general]\nnonsense = \"x\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::UnknownAction {
                context: "general",
                action: "nonsense".into(),
                defined_in: None,
            }]
        );
    }

    #[test]
    fn suspend_action_is_reserved() {
        let (user, warnings) = parse_ok("[general]\nsuspend = \"ctrl-x\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::ReservedAction {
                context: "general",
                action: "suspend".into()
            }]
        );
        assert!(user.entries.is_empty());
    }

    #[test]
    fn ctrl_z_stroke_is_reserved() {
        let (user, warnings) = parse_ok("[general]\nquit = \"ctrl-z\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::ReservedKey {
                context: "general",
                action: "quit".into(),
                key: "ctrl-z".into()
            }]
        );
        // The action still applied — just with no strokes left.
        assert_eq!(user.entries.len(), 1);
        assert!(user.entries[0].strokes.is_empty());
    }

    #[test]
    fn non_table_context_warns() {
        let (_, warnings) = parse_ok("general = \"x\"");
        assert_eq!(
            warnings,
            vec![KeymapWarning::ExpectedTable { context: "general" }]
        );
    }

    #[test]
    fn invalid_value_shapes_warn() {
        for src in [
            "[general]\nquit = 5",
            "[general]\nquit = [\"ctrl-x\", 5]",
            "[general]\nquit = { x = 1 }",
        ] {
            let (user, warnings) = parse_ok(src);
            assert_eq!(
                warnings,
                vec![KeymapWarning::InvalidValue {
                    context: "general",
                    action: "quit".into()
                }],
                "{src}"
            );
            assert!(user.entries.is_empty(), "{src}");
        }
    }

    #[test]
    fn invalid_key_in_list_skips_just_that_key() {
        let (user, warnings) = parse_ok("[general]\nquit = [\"f0\", \"ctrl-q\"]");
        assert_eq!(warnings.len(), 1);
        assert!(matches!(warnings[0], KeymapWarning::InvalidKey { .. }));
        assert_eq!(user.entries[0].strokes.len(), 1);
    }

    #[test]
    fn bad_toml_warns_and_yields_defaults() {
        let (user, warnings) = parse_ok("this is [not toml");
        assert!(matches!(warnings[0], KeymapWarning::Parse { .. }));
        assert!(user.entries.is_empty());
    }

    #[test]
    fn every_action_is_named_once_in_its_bindings_context() {
        use strum::IntoEnumIterator;
        for action in KeyAction::iter() {
            let homes: Vec<KeybindContext> = ACTIONS
                .iter()
                .filter(|(_, actions)| actions.iter().any(|(_, a)| *a == action))
                .map(|(ctx, _)| *ctx)
                .collect();
            assert_eq!(homes.len(), 1, "{action:?} needs exactly one ACTIONS entry");
            let bound: Vec<KeybindContext> = BINDINGS
                .iter()
                .filter(|(_, bs)| bs.iter().any(|b| b.action == action))
                .map(|(ctx, _)| *ctx)
                .collect();
            assert_eq!(
                bound, homes,
                "{action:?} ACTIONS context must match BINDINGS"
            );
        }
        for (ctx, actions) in ACTIONS {
            let mut names: Vec<&str> = actions.iter().map(|(n, _)| *n).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                actions.len(),
                "duplicate action names in {ctx:?}"
            );
        }
    }

    #[test]
    fn user_override_resolves_and_drops_defaults() {
        // ctrl-enter is a default `newline` key — the claim steals it.
        let (user, warnings) = parse_ok("[editing]\nsubmit = \"ctrl-enter\"");
        assert!(warnings.is_empty(), "{warnings:?}");
        let (map, warnings) = EffectiveKeymap::build(&user);
        assert_eq!(
            warnings,
            vec![KeymapWarning::Shadowed {
                context: "editing",
                key: "ctrl-enter".into(),
                previous: "newline",
                action: "submit",
            }]
        );
        let stack = [KeybindContext::Editing, KeybindContext::General];
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(KeyAction::Submit)
        );
        // Default `enter` binding is gone and the Enter fallback no longer
        // applies — the key falls through to the composer.
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        // Unrelated actions keep their defaults.
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::TasksOpen)
        );
    }

    #[test]
    fn empty_list_unbinds() {
        let (user, _) = parse_ok("[general]\ntasks = []");
        let (map, _) = EffectiveKeymap::build(&user);
        let stack = [KeybindContext::General];
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn multi_key_list_replaces_defaults() {
        let (user, _) = parse_ok("[editing]\nnewline = [\"alt-enter\", \"ctrl-j\"]");
        let (map, _) = EffectiveKeymap::build(&user);
        let stack = [KeybindContext::Editing, KeybindContext::General];
        for mods in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
            let code = if mods == KeyModifiers::CONTROL {
                KeyCode::Char('j')
            } else {
                KeyCode::Enter
            };
            assert_eq!(
                map.resolve(&stack, key(code, mods)),
                Some(KeyAction::Newline)
            );
        }
        // Shift+Enter lost its newline binding; the Enter fallback does not
        // resurrect it because `newline` keeps no Shift-Enter stroke.
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Enter, KeyModifiers::SHIFT)),
            None
        );
    }

    #[test]
    fn first_user_claim_wins_conflicts() {
        let (user, warnings) = parse_ok("[general]\nquit = \"ctrl-q\"\ntasks = \"ctrl-q\"");
        assert!(warnings.is_empty(), "{warnings:?}");
        let (map, warnings) = EffectiveKeymap::build(&user);
        // ctrl-q's default (queue_pop) was displaced by quit; then tasks
        // lost to quit's earlier user claim.
        assert_eq!(
            warnings,
            vec![
                KeymapWarning::Shadowed {
                    context: "general",
                    key: "ctrl-q".into(),
                    previous: "queue_pop",
                    action: "quit",
                },
                KeymapWarning::Conflict {
                    context: "general",
                    key: "ctrl-q".into(),
                    kept: "quit",
                    dropped: "tasks",
                },
            ]
        );
        let stack = [KeybindContext::General];
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(KeyAction::QuitOrCancel)
        );
        // tasks' own default was removed too (its list was replaced).
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn reserved_keys_stay_out_of_the_map() {
        let (user, warnings) = parse_ok("[general]\nquit = \"ctrl-z\"\nsuspend = \"ctrl-x\"");
        assert_eq!(warnings.len(), 2);
        let (map, warnings) = EffectiveKeymap::build(&user);
        assert!(warnings.is_empty(), "{warnings:?}");
        let stack = [KeybindContext::General];
        // The default ctrl-z → Suspend claim stays (handle_key intercepts
        // it before resolution anyway); the file cannot move it to ctrl-x.
        // It is unix-gated in BINDINGS, so it resolves to nothing elsewhere.
        let expected_suspend = cfg!(unix).then_some(KeyAction::Suspend);
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            expected_suspend
        );
        assert_eq!(
            map.resolve(&stack, key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn defaults_survive_unspecified() {
        let (map, warnings) = EffectiveKeymap::build(&UserKeymap::default());
        assert!(warnings.is_empty());
        assert_eq!(
            map.resolve(
                &[KeybindContext::General],
                key(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(KeyAction::QuitOrCancel)
        );
    }
}
