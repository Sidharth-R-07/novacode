//! The editor layout (files, editor, AI, terminal, git panels): a small, fast,
//! Zed-inspired terminal editor with novacode in its AI panel. It runs as
//! `novacode edit` and as the standalone `nova-edit` binary.

mod agent;
mod app;
mod buffer;
mod git;
mod tree;
mod ui;

use std::ffi::OsString;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use app::App;

/// Mouse reports for clicks, drags and the wheel, SGR-encoded. Not crossterm's
/// EnableMouseCapture: that adds a report for every pointer move, which would
/// wake the editor for nothing. (Shift+drag still selects text in most terminals.)
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// Puts the terminal in raw/alternate-screen mode and restores it on drop,
/// including during unwinding.
struct TerminalGuard {
    term: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, EnableBracketedPaste, Print(MOUSE_ON)) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        // Where the terminal speaks the kitty keyboard protocol (kitty, foot,
        // WezTerm, Alacritty, Ghostty...), ask it to tell Ctrl+Esc from Esc and
        // Ctrl+` from Ctrl+Space, so they can open the terminal panel.
        if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
            && execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            KEYS_ENHANCED.store(true, Ordering::SeqCst);
        }
        Ok(Self {
            term: Terminal::new(CrosstermBackend::new(out))?,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// The keyboard enhancement was pushed and must be popped on the way out.
static KEYS_ENHANCED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if KEYS_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        Print(MOUSE_OFF),
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
}

/// The program run in the AI panel (default `novacode` from PATH). `novacode edit`
/// passes its own executable, so the panel runs the same build. Call before [`run`].
pub fn set_agent_program(program: impl Into<OsString>) {
    let _ = agent::PROGRAM.set(program.into());
}

/// Arguments for the AI panel's program when it first starts (plain `novacode`
/// forwards its own: `-c`, `-m <model>`, a prompt…). Call before [`run`].
pub fn set_agent_args(args: Vec<OsString>) {
    *agent::FIRST_ARGS.lock().unwrap_or_else(|e| e.into_inner()) = args;
}

/// Open the editor on `target` (a directory, a file, or the current directory)
/// and run it until the user quits. A missing `target` is a `NotFound` error,
/// returned before the terminal is touched.
pub async fn run(target: Option<PathBuf>) -> io::Result<()> {
    // Resolve the target before touching the terminal so usage errors print normally.
    let (root, file) = match target {
        Some(p) if p.is_dir() => (p, None),
        Some(p) if p.is_file() => (std::env::current_dir()?, Some(p.canonicalize().unwrap_or(p))),
        Some(p) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: no such file or directory", p.display()),
            ));
        }
        None => (std::env::current_dir()?, None),
    };
    let root = root.canonicalize().unwrap_or(root);

    // A crash must not leave the shell in raw mode on the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    let (term_tx, mut term_rx) = mpsc::unbounded_channel();
    let size = crossterm::terminal::size().unwrap_or((120, 30));
    let mut app = App::new(root, agent_tx, term_tx, size);
    if let Some(f) = file {
        app.open_file(&f);
        app.focus = app::Focus::Editor;
    }

    let mut guard = TerminalGuard::new()?;
    let mut events = EventStream::new();

    // Render ticks are capped at ~60 FPS and only armed while something changed,
    // so an idle editor sleeps until input arrives.
    let mut frame_tick = interval(Duration::from_millis(16));
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut slow_tick = interval(Duration::from_secs(1));
    slow_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while !app.should_quit {
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_event(ev),
                Some(Err(e)) => return Err(e),
                None => break,
            },
            Some((id, ev)) = agent_rx.recv() => app.on_agent(id, ev),
            Some((_, ev)) = term_rx.recv() => app.on_term(ev),
            _ = frame_tick.tick(), if app.dirty => {
                app.dirty = false;
                guard.term.draw(|f| ui::draw(f, &mut app))?;
            }
            _ = slow_tick.tick() => app.expire_status(),
        }
    }
    Ok(())
}
