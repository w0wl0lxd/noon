//! Single-threaded ratatui event loop; the agent runs on smol tasks in a separate thread.
//! `AgentHandles` bundles all flume channels to the agent. `dispatch()` processes
//! `Action`s returned by `App::update()`. Scroll and drag events are coalesced from
//! the queue to avoid jank.

pub mod animation;
pub mod app;
pub(crate) mod cast;
pub mod chat;
mod clipboard;
mod color_compat;
mod components;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand};
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
mod keymap;
pub use theme::BUNDLED_THEMES;
pub mod image;
mod markdown;
pub mod mascot;
mod render_worker;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
pub mod update;

mod agent;
mod event_loop;
mod input;
mod session_lineage;
mod terminal;
mod terminal_image;

use color_eyre::Result;
use n00n_agent::ToolOutput;
use n00n_providers::Message;
use n00n_providers::TokenUsage;
use n00n_storage::id::n00nId;

pub type AppSession = n00n_storage::sessions::Session<Message, TokenUsage, ToolOutput>;

pub(crate) use agent::AgentCommand;
pub use event_loop::EventLoopParams;

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk.
pub enum RunOutcome {
    Exit {
        session_id: Option<n00nId>,
        code: i32,
    },
    Reload {
        tabs: Vec<AppSession>,
        focused: usize,
    },
}

/// Run the n00n UI event loop.
///
/// # Errors
///
/// Returns an error if the terminal or event loop cannot be initialized.
pub fn run(params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let report = {
        let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
        color_compat::init();
        let el = event_loop::EventLoop::new(&mut terminal, params)?;
        el.run(initial_prompt)?
    };
    Ok(match report.exit {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs: report.tabs,
            focused: report.focused,
        },
        _ => RunOutcome::Exit {
            session_id: report
                .tabs
                .get(report.focused)
                .filter(|s| app::session_has_content(s))
                .map(|s| s.id),
            code: report.exit.code(),
        },
    })
}
