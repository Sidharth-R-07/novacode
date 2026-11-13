//! The AI panel is the real `novacode` TUI, unchanged, running in a pseudo-terminal.
//!
//! Its screen is interpreted by a terminal emulator (`vt100`) and drawn into the
//! right-hand pane by the UI. Everything it does (models, logins, sessions, tools,
//! slash commands, settings) is novacode itself; this module only hosts it.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::UnboundedSender;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

pub enum AgentEvent {
    Output(Vec<u8>),
    Exited,
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// The AI panel's program, when not `novacode` from PATH (see `set_agent_program`).
pub(crate) static PROGRAM: OnceLock<OsString> = OnceLock::new();

/// Arguments for the AI panel's first start only (`novacode -c`, a prompt…);
/// a restart after it quit starts a plain chat.
pub(crate) static FIRST_ARGS: Mutex<Vec<OsString>> = Mutex::new(Vec::new());

pub struct Agent {
    parser: vt100::Parser,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    pub exited: bool,
}

impl Agent {
    /// Start `novacode` in `root`. `rows`/`cols` is the pane size to begin with.
    pub fn spawn(root: &Path, rows: u16, cols: u16, tx: UnboundedSender<AgentEvent>) -> Result<Self, String> {
        let program = PROGRAM.get().map_or(OsStr::new("novacode"), OsString::as_os_str);
        let args = std::mem::take(&mut *FIRST_ARGS.lock().unwrap_or_else(|e| e.into_inner()));
        Self::spawn_inner(program, &args, root, rows, cols, tx, 0)
    }

    /// The user's shell (`$SHELL`, else `sh`) in `root`, for the terminal panel.
    /// It keeps some scrollback so the wheel can look back.
    pub fn spawn_shell(root: &Path, rows: u16, cols: u16, tx: UnboundedSender<AgentEvent>) -> Result<Self, String> {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
        let shell = shell.as_deref().unwrap_or("sh");
        Self::spawn_inner(OsStr::new(shell), &[], root, rows, cols, tx, 5000)
    }

    /// `spawn` with another program in the pane (tests host a stand-in).
    #[cfg(test)]
    pub(crate) fn spawn_program(
        program: &str,
        root: &Path,
        rows: u16,
        cols: u16,
        tx: UnboundedSender<AgentEvent>,
    ) -> Result<Self, String> {
        Self::spawn_inner(OsStr::new(program), &[], root, rows, cols, tx, 0)
    }

    fn spawn_inner(
        program: &OsStr,
        args: &[OsString],
        root: &Path,
        rows: u16,
        cols: u16,
        tx: UnboundedSender<AgentEvent>,
        scrollback: usize,
    ) -> Result<Self, String> {
        let (rows, cols) = (rows.max(2), cols.max(10));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("cannot open a pseudo-terminal: {e}"))?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(root);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("NOVA_EDIT", "1");
        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            let program = program.to_string_lossy();
            format!("cannot start `{program}`: {e} (is it installed and on PATH?)")
        })?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;
        let writer: Writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?,
        ));

        let reply = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        answer_queries(&mut carry, &buf[..n], &reply, (rows, cols));
                        if tx.send(AgentEvent::Output(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(AgentEvent::Exited);
        });

        Ok(Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            writer,
            master: pair.master,
            child,
            size: (rows, cols),
            exited: false,
        })
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(10));
        if (rows, cols) == self.size || self.exited {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.set_size(rows, cols);
    }

    fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes).and_then(|()| w.flush());
        }
    }

    /// Scroll the view back (`delta` < 0) or forward through the scrollback.
    pub fn scroll_by(&mut self, delta: isize) {
        let now = self.parser.screen().scrollback();
        self.parser.set_scrollback(now.saturating_add_signed(-delta));
    }

    /// The program wants mouse reports (so the wheel goes to it, not the scrollback).
    pub fn wants_mouse(&self) -> bool {
        self.screen().mouse_protocol_mode() != MouseProtocolMode::None
    }

    pub fn send_key(&mut self, k: KeyEvent) {
        // Typing jumps back to the live screen.
        self.parser.set_scrollback(0);
        let bytes = encode_key(k, self.screen().application_cursor());
        if !bytes.is_empty() {
            self.write(&bytes);
        }
    }

    pub fn send_paste(&self, text: &str) {
        if self.screen().bracketed_paste() {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.replace('\n', "\r").as_bytes());
        }
    }

    /// Report a mouse event at `col`/`row` (0-based, inside the pane), if the
    /// hosted program asked for that kind of report.
    pub fn send_mouse(&self, kind: MouseEventKind, mods: KeyModifiers, col: u16, row: u16) {
        let s = self.screen();
        let bytes = encode_mouse(
            kind,
            mods,
            (col, row),
            s.mouse_protocol_mode(),
            s.mouse_protocol_encoding(),
        );
        if !bytes.is_empty() {
            self.write(&bytes);
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// novacode (through crossterm) asks the terminal a few things at start-up and
/// waits for an answer. A bare emulator never replies, so answer here, in the
/// reader thread, before the bytes reach the screen.
fn answer_queries(carry: &mut Vec<u8>, chunk: &[u8], w: &Writer, size: (u16, u16)) {
    const DA1: &[&[u8]] = &[b"\x1b[c", b"\x1b[0c"];
    const CPR: &[u8] = b"\x1b[6n";
    const BG: &[&[u8]] = &[b"\x1b]11;?\x07", b"\x1b]11;?\x1b\\"];

    let old = carry.len();
    let mut data = std::mem::take(carry);
    data.extend_from_slice(chunk);
    let mut replies: Vec<Vec<u8>> = Vec::new();
    let mut scan = |pat: &[u8], reply: Vec<u8>| {
        let mut from = 0;
        while let Some(p) = find(&data[from..], pat) {
            let end = from + p + pat.len();
            // Matches wholly inside the carried bytes were answered last time.
            if end > old {
                replies.push(reply.clone());
            }
            from = end;
        }
    };
    for p in DA1 {
        // VT220-class device attributes; deliberately no kitty-keyboard answer.
        scan(p, b"\x1b[?62;c".to_vec());
    }
    scan(CPR, format!("\x1b[{};1R", size.0).into_bytes());
    for p in BG {
        scan(p, b"\x1b]11;rgb:0000/0000/0000\x07".to_vec());
    }
    if !replies.is_empty()
        && let Ok(mut w) = w.lock()
    {
        for r in replies {
            let _ = w.write_all(&r);
        }
        let _ = w.flush();
    }
    let keep = data.len().saturating_sub(8);
    *carry = data[keep..].to_vec();
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Encode a key press the way an xterm sends it.
pub fn encode_key(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let esc = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };
    let csi = |final_byte: char, plain_o: bool| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[1;{m}{final_byte}").into_bytes()
        } else if plain_o && app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    'A'..='Z' => c as u8 - b'A' + 1,
                    ' ' | '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '/' | '7' => 0x1f,
                    _ => return esc(c.to_string().into_bytes()),
                };
                esc(vec![b])
            } else {
                esc(c.to_string().into_bytes())
            }
        }
        KeyCode::Enter => esc(vec![b'\r']),
        KeyCode::Tab => esc(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => esc(vec![0x7f]),
        KeyCode::Esc => esc(vec![0x1b]),
        KeyCode::Up => csi('A', true),
        KeyCode::Down => csi('B', true),
        KeyCode::Right => csi('C', true),
        KeyCode::Left => csi('D', true),
        KeyCode::Home => csi('H', true),
        KeyCode::End => csi('F', true),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let f = (b'P' + n - 1) as char;
            if m > 1 {
                format!("\x1b[1;{m}{f}").into_bytes()
            } else {
                format!("\x1bO{f}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]),
        _ => Vec::new(),
    }
}

/// Encode a mouse event the way xterm reports it to a program that turned mouse
/// reports on. `at` is the 0-based (column, row) inside the pane. Empty when
/// `mode` does not cover this kind of event, or the encoding cannot hold `at`.
pub fn encode_mouse(
    kind: MouseEventKind,
    mods: KeyModifiers,
    at: (u16, u16),
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Vec<u8> {
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    // (button code, is a release, is a motion report)
    let (mut code, release, motion): (u32, bool, bool) = match kind {
        MouseEventKind::Down(b) => (button(b), false, false),
        MouseEventKind::Up(b) => (button(b), true, false),
        MouseEventKind::Drag(b) => (button(b) + 32, false, true),
        MouseEventKind::Moved => (3 + 32, false, true),
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
    };
    let wanted = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => !release && !motion,
        MouseProtocolMode::PressRelease => !motion,
        MouseProtocolMode::ButtonMotion => kind != MouseEventKind::Moved,
        MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return Vec::new();
    }
    // X10 (press-only) reports carry no modifiers.
    if mode != MouseProtocolMode::Press {
        for (m, bit) in [
            (KeyModifiers::SHIFT, 4),
            (KeyModifiers::ALT, 8),
            (KeyModifiers::CONTROL, 16),
        ] {
            if mods.contains(m) {
                code += bit;
            }
        }
    }
    let (x, y) = (u32::from(at.0) + 1, u32::from(at.1) + 1);
    if encoding == MouseProtocolEncoding::Sgr {
        let end = if release { 'm' } else { 'M' };
        return format!("\x1b[<{code};{x};{y}{end}").into_bytes();
    }
    // The older encodings cannot say which button went up.
    if release {
        code = (code & !3) | 3;
    }
    let mut out = b"\x1b[M".to_vec();
    for v in [code, x, y].map(|v| v + 32) {
        if encoding == MouseProtocolEncoding::Utf8 {
            let Some(c) = char::from_u32(v) else {
                return Vec::new();
            };
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            let Ok(b) = u8::try_from(v) else {
                return Vec::new();
            };
            out.push(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, mods), false)
    }

    #[test]
    fn encodes_common_keys() {
        assert_eq!(key(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(key(KeyCode::Char('r'), KeyModifiers::CONTROL), [0x12]);
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
    }

    #[test]
    fn encodes_mouse_reports_for_the_mode_asked_for() {
        use MouseEventKind as K;
        let none = KeyModifiers::NONE;
        let left = MouseButton::Left;
        let sgr = |kind, mods, mode| encode_mouse(kind, mods, (4, 2), mode, MouseProtocolEncoding::Sgr);
        let motion = MouseProtocolMode::ButtonMotion;
        assert_eq!(sgr(K::Down(left), none, motion), b"\x1b[<0;5;3M");
        assert_eq!(sgr(K::Up(left), none, motion), b"\x1b[<0;5;3m");
        assert_eq!(sgr(K::Drag(left), none, motion), b"\x1b[<32;5;3M");
        assert_eq!(sgr(K::ScrollDown, none, motion), b"\x1b[<65;5;3M");
        assert_eq!(
            sgr(K::Down(MouseButton::Right), KeyModifiers::CONTROL, motion),
            b"\x1b[<18;5;3M"
        );
        // Only what the program asked for.
        assert!(sgr(K::Down(left), none, MouseProtocolMode::None).is_empty());
        assert!(sgr(K::Drag(left), none, MouseProtocolMode::PressRelease).is_empty());
        assert!(sgr(K::Moved, none, motion).is_empty());
        // Legacy encoding: offset by 32, and a release is button 3.
        let x10 = |kind| encode_mouse(kind, none, (0, 0), motion, MouseProtocolEncoding::Default);
        assert_eq!(x10(K::Down(left)), b"\x1b[M\x20\x21\x21");
        assert_eq!(x10(K::Up(left)), b"\x1b[M\x23\x21\x21");
        let far = encode_mouse(K::Down(left), none, (300, 0), motion, MouseProtocolEncoding::Default);
        assert!(far.is_empty(), "column 301 does not fit in a byte");
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn answers_startup_queries_once_even_when_split() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let w: Writer = Arc::new(Mutex::new(Box::new(Sink(got.clone()))));
        let mut carry = Vec::new();
        answer_queries(&mut carry, b"hello\x1b[", &w, (20, 40));
        answer_queries(&mut carry, b"c more", &w, (20, 40));
        answer_queries(&mut carry, b"x\x1b[6n", &w, (20, 40));
        // The DA1 query is answered once (not again from the carried tail), plus the cursor report.
        assert_eq!(got.lock().unwrap().as_slice(), b"\x1b[?62;c\x1b[20;1R");
    }
}
