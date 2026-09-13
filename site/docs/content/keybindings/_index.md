+++
title = "Keybindings"
weight = 7
[extra]
group = "Reference"
+++

# Keybindings

On macOS, some bindings use Option or Fn keys instead (run `/help` for exact keybindings).

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Interrupt / clear / quit |
| `Ctrl+H` | Show keybindings |
| `Ctrl+L` | Redraw screen |
| `Ctrl+N` / `Ctrl+P` | Next / previous task chat |
| `PageUp` / `PageDown` | Scroll page up / down |
| `Alt+U` / `Alt+D` | Scroll half page up / down |
| `Alt+G` / `Alt+Shift+G` | Scroll to top / bottom |
| `Ctrl+F` | Search messages |
| `Ctrl+O` | Toggle transcript details |
| `Ctrl+T` | Open tasks |
| `Alt+P` | Toggle plan panel |
| `Alt+Shift+P` | Open plan in editor |
| `Ctrl+G` | Edit input in external editor |
| `Alt+T` / `Ctrl+Shift+T` | Cycle thinking level |
| `Ctrl+Shift+C` | Copy selection |
| `Ctrl+V` | Paste image from clipboard |
| `Ctrl+Q` | Pop queue |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `\+Enter` / `Ctrl+J` | Newline |
| `Tab` | Toggle mode / queue message |
| `Esc Esc` | Clear draft / rewind |
| `↑` / `↓` | History / cursor |
| `Home` / `End` | Start / end of line |
| `Del` | Delete char forward |
| `Ctrl+A` | Jump to start of line |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+W / Ctrl+Bksp` | Delete word backward |
| `Ctrl+Del` / `Alt+D` | Delete word forward |
| `Ctrl+←` / `Ctrl+→` | Move word left / right |
| `Alt+←` / `Alt+→` | Move word left / right |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+U` | Delete to start of line |
| `Ctrl+Y` | Paste deleted text |
| `Alt+Y` | Cycle paste history |
| `Ctrl+_` / `Ctrl+-` | Undo last edit |
| `Ctrl+Shift+Z` | Redo edit |
| `Ctrl+S` | Stash / restore draft |
| `Ctrl+R` | Search input history |
| `Ctrl+D` | Delete char / exit |
| `/command` | Open command palette |
| `@` | Mention a file (Esc leaves a literal @) |

## While Streaming

| Key | Action |
|-----|--------|
| `Esc` | Interrupt agent |

## Form

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate options |
| `Enter` | Select option |
| `Esc` | Close |

## Pickers

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate |
| `Enter` | Select |
| `Esc` | Close |
| `Type` | Filter |
| `PageUp` / `PageDown` | Scroll page up / down |
| `Ctrl+U` / `Ctrl+D` | Scroll page up / down |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Subagent Chat | `←` | Back to main chat |
| Subagent Chat | `Esc` | Back / cancel subagent |
| History Search | `Enter` / `Tab` | Accept match |
| History Search | `Esc` | Cancel search |
| History Search | `↑` / `Ctrl+R` | Older match |
| History Search | `↓` | Newer match |
| Queue | `Enter` | Remove item |
| Commands | `Tab` | Complete command |
| Model Picker | `!/@/#/$` | Set tier (strong/medium/weak/compaction) |
| Model Picker | `Alt+T` | Cycle thinking level |
| Session Picker | `Ctrl+N` | New session |
| Session Picker | `Ctrl+R` | Rename session |
| Session Picker | `Ctrl+D` | Delete session (press twice) |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Editing** is the base for: Subagent Chat, History Search
- **Pickers** is the base for: Task Picker, Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker

## Overriding Keybindings

Plugins and `init.lua` can rebind keys at runtime with `n00n.keymap.set` and `n00n.keymap.del`. The tables above are the built-in defaults. An override on the same key wins, unless a modal or overlay is open (help, plan form, permission prompt).

Precedence, high to low:

1. **Suspend** (`Ctrl+Z`, Unix). Always wins, non-remappable.
2. **Modal and overlay keys.** An open modal or picker consumes its keys first, so they cannot be shadowed while open.
3. **Lua overrides** from `n00n.keymap.set`. Last set wins; binding the same key twice warns.
4. **Built-in defaults.** An override on the same key shadows them; `n00n.keymap.del` lifts the override so the default returns. Suspend is the only binding outside this layer, so every key is remappable except `Ctrl+Z`.

Only single-key bindings can be overridden. Multi-key combinations and non-key rows (like `Type` to filter) cannot.

The `/help` modal and the splash show default labels, not live overrides, but pressing the key still runs the override.

### Recovering from a bad keymap

If an override leaves n00n stuck (a rebound `Ctrl+C`, a modal that won't close, a plugin that throws on load), boot without plugins:

```bash
n00n --no-plugins
```

This skips the Lua host and runs the full default keymap from Rust, so quit, Esc, scroll, and suspend always work.

The defaults live in Rust, not Lua, so `--no-plugins` never drops them.
