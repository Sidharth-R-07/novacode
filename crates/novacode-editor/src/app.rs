//! Application state, focus management and event routing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::agent::{Agent, AgentEvent, EventTx};
use crate::buffer::Buffer;
use crate::git;
use crate::tree::Tree;

const STATUS_TTL: Duration = Duration::from_secs(5);
const WHEEL_LINES: isize = 3;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Editor,
    Ai,
    /// The terminal panel along the bottom.
    Term,
}

/// Where the panes were drawn last frame, so a click can find its pane.
/// Written by the renderer.
#[derive(Clone, Copy, Default)]
pub struct Panes {
    pub tree: Option<Rect>,
    pub tree_inner: Rect,
    pub editor: Rect,
    pub editor_inner: Rect,
    /// Screen column where the editor's text starts, right of the line numbers.
    pub editor_text_x: u16,
    pub ai: Option<Rect>,
    pub ai_inner: Rect,
    pub term: Option<Rect>,
    pub term_inner: Rect,
}

pub struct GitView {
    pub top: PathBuf,
    pub entries: Vec<git::Entry>,
    pub selected: usize,
    /// `Some` while viewing the diff of the selected file.
    pub diff: Option<Vec<String>>,
    pub scroll: usize,
    pub error: Option<String>,
}

pub enum Overlay {
    Git(GitView),
}

pub struct App {
    pub focus: Focus,
    pub show_tree: bool,
    pub show_editor: bool,
    pub show_ai: bool,
    /// The AI pane fills the whole window (F11 / Alt+Z).
    pub ai_full: bool,
    pub tree: Tree,
    pub buffer: Buffer,
    /// The hosted `novacode` chats, one per tab (`/new` in a chat or Alt+N opens
    /// one). Empty when it could not start; see `agent_error`.
    pub tabs: Vec<Agent>,
    /// The tab on screen in the AI pane.
    pub active: usize,
    next_id: u64,
    /// Where each tab's label was drawn on the AI pane's top edge (columns
    /// from, to), so a click can switch tabs. Written by the renderer.
    pub tab_hits: Vec<(u16, u16)>,
    pub agent_error: Option<String>,
    pub status: String,
    pub should_quit: bool,
    /// Set whenever visible state changes; the main loop redraws only then.
    pub dirty: bool,
    pub overlay: Option<Overlay>,
    pub panes: Panes,
    /// The terminal panel (Ctrl+Esc / Ctrl+` / Ctrl+T) is on screen.
    pub show_term: bool,
    /// The shell in the terminal panel; started the first time the panel opens.
    pub term: Option<Agent>,
    pub term_error: Option<String>,
    term_tx: EventTx,
    /// Last terminal panel size, so the shell starts at the right size.
    pub term_size: (u16, u16),
    /// Where focus goes back to when the terminal panel is left or closed.
    term_return: Focus,

    /// The pane a mouse button went down in: its drag and release go there too.
    mouse_owner: Option<Focus>,
    /// Last click in the tree (row index, when), to spot a double click.
    last_click: Option<(usize, Instant)>,
    status_at: Instant,
    quit_armed: bool,
    agent_tx: EventTx,
    /// Last pane size, so a restart starts at the right size.
    pub agent_size: (u16, u16),
}

impl App {
    /// `term` is the terminal size, used to guess the AI pane before the first draw.
    /// `term_tx` carries the terminal panel's shell output.
    pub fn new(root: PathBuf, agent_tx: EventTx, term_tx: EventTx, term: (u16, u16)) -> Self {
        let mut app = Self::without_agent(root, agent_tx, term_tx, term);
        app.start_agent();
        app
    }

    fn without_agent(root: PathBuf, agent_tx: EventTx, term_tx: EventTx, term: (u16, u16)) -> Self {
        let size = (
            (term.1.saturating_sub(3)).max(5),
            (term.0 * 3 / 10).saturating_sub(2).max(20),
        );
        Self {
            overlay: None,
            panes: Panes::default(),
            show_term: false,
            term: None,
            term_error: None,
            term_tx,
            term_size: ((term.1 / 3).saturating_sub(2).max(3), term.0.saturating_sub(2).max(20)),
            term_return: Focus::Tree,
            mouse_owner: None,
            last_click: None,
            focus: Focus::Tree,
            show_tree: true,
            show_editor: true,
            show_ai: true,
            ai_full: false,
            tree: Tree::new(root),
            buffer: Buffer::default(),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            tab_hits: Vec::new(),
            agent_error: None,
            status: String::new(),
            should_quit: false,
            dirty: true,
            status_at: Instant::now(),
            quit_armed: false,
            agent_tx,
            agent_size: size,
        }
    }

    fn spawn_chat(&mut self) -> Result<Agent, String> {
        let (rows, cols) = self.agent_size;
        let id = self.next_id;
        self.next_id += 1;
        Agent::spawn(&self.tree.root, rows, cols, self.agent_tx.clone(), id)
    }

    /// Start (or restart) the chat in the tab on screen.
    fn start_agent(&mut self) {
        self.dirty = true;
        match self.spawn_chat() {
            Ok(a) => {
                self.agent_error = None;
                // Replacing the old one drops (kills) it.
                match self.tabs.get_mut(self.active) {
                    Some(slot) => *slot = a,
                    None => {
                        self.tabs.push(a);
                        self.active = self.tabs.len() - 1;
                    }
                }
            }
            Err(e) => self.agent_error = Some(e),
        }
    }

    /// A new chat in a new tab, shown and focused; the others keep running.
    pub fn new_tab(&mut self) {
        self.dirty = true;
        match self.spawn_chat() {
            Ok(a) => {
                self.tabs.push(a);
                self.active = self.tabs.len() - 1;
                self.show_ai = true;
                self.focus = Focus::Ai;
                let n = self.tabs.len();
                self.set_status(format!("chat {n} opened · Alt+1…9 or Alt+, / Alt+. switch tabs"));
            }
            Err(e) => self.set_status(format!("cannot open a chat tab: {e}")),
        }
    }

    /// Show tab `i` (0-based) in the AI pane.
    pub fn switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
            self.show_ai = true;
            self.focus = Focus::Ai;
            self.dirty = true;
        }
    }

    /// The next (or previous) tab, wrapping around.
    fn step_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n > 1 {
            let i = if forward {
                (self.active + 1) % n
            } else {
                (self.active + n - 1) % n
            };
            self.switch_tab(i);
        }
    }

    /// The chat on screen in the AI pane.
    pub fn agent(&self) -> Option<&Agent> {
        self.tabs.get(self.active)
    }

    pub fn agent_mut(&mut self) -> Option<&mut Agent> {
        self.tabs.get_mut(self.active)
    }

    /// The hosted chat is running and should receive keystrokes.
    pub fn agent_live(&self) -> bool {
        self.agent().is_some_and(|a| !a.exited)
    }

    /// Output and requests from the chat with tag `id`, whichever tab it is in.
    pub fn on_agent(&mut self, id: u64, ev: AgentEvent) {
        let Some(i) = self.tabs.iter().position(|a| a.id == id) else {
            return; // a chat already closed
        };
        self.dirty = true;
        match ev {
            AgentEvent::Output(bytes) => self.tabs[i].feed(&bytes),
            AgentEvent::NewTab => self.new_tab(),
            AgentEvent::Exited if self.tabs.len() > 1 => {
                // `/quit` in one of several tabs closes that tab only.
                self.tabs.remove(i);
                if self.active > i || self.active >= self.tabs.len() {
                    self.active = self.active.saturating_sub(1);
                }
                let left = self.tabs.len();
                self.set_status(format!("chat tab closed; {left} left"));
            }
            AgentEvent::Exited => {
                self.tabs[i].exited = true;
                // `/quit` in the AI panel closes the window, as in novacode alone,
                // unless the editor has unsaved changes (then Enter restarts the chat).
                if self.buffer.modified {
                    self.set_status("novacode quit; unsaved changes: Ctrl+S to save, Ctrl+Q to quit");
                } else {
                    self.should_quit = true;
                }
            }
        }
    }

    fn start_term(&mut self) {
        self.term = None;
        let (rows, cols) = self.term_size;
        match Agent::spawn_shell(&self.tree.root, rows, cols, self.term_tx.clone()) {
            Ok(t) => {
                self.term = Some(t);
                self.term_error = None;
            }
            Err(e) => self.term_error = Some(e),
        }
        self.dirty = true;
    }

    /// The terminal panel's shell is running and should receive keystrokes.
    pub fn term_live(&self) -> bool {
        self.term.as_ref().is_some_and(|t| !t.exited)
    }

    /// Output from the terminal panel's shell. When the shell exits (`exit`,
    /// Ctrl+D) the panel closes, as in an IDE.
    pub fn on_term(&mut self, ev: AgentEvent) {
        self.dirty = true;
        match ev {
            AgentEvent::NewTab => {}
            AgentEvent::Output(bytes) => {
                if let Some(t) = self.term.as_mut() {
                    t.feed(&bytes);
                }
            }
            AgentEvent::Exited => {
                self.term = None;
                if self.show_term {
                    self.show_term = false;
                    if self.focus == Focus::Term {
                        self.focus = self.term_return;
                    }
                    self.fix_layout(Focus::Editor);
                }
            }
        }
    }

    /// Ctrl+Esc / Ctrl+`: open the terminal panel and focus it; from the panel,
    /// close it again (the shell keeps running until it exits).
    fn toggle_term(&mut self) {
        if self.show_term && self.focus == Focus::Term {
            self.show_term = false;
            self.focus = self.term_return;
            self.fix_layout(Focus::Editor);
            return;
        }
        self.ai_full = false;
        self.show_term = true;
        if self.focus != Focus::Term {
            self.term_return = self.focus;
        }
        self.focus = Focus::Term;
        if self.term.is_none() {
            self.start_term();
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_at = Instant::now();
        self.dirty = true;
    }

    /// Called on a slow timer: fades transient messages and picks up changes made
    /// outside the editor (the AI agent, a shell, git).
    pub fn expire_status(&mut self) {
        if !self.status.is_empty() && self.status_at.elapsed() > STATUS_TTL {
            self.status.clear();
            self.dirty = true;
        }
        self.refresh_live();
    }

    pub fn refresh_live(&mut self) {
        if self.tree.refresh() {
            self.dirty = true;
        }
        if self.buffer.reload_if_changed() {
            self.dirty = true;
            let name = self.buffer.path.as_deref().map(|p| self.rel(p)).unwrap_or_default();
            self.set_status(format!("{name} changed on disk: reloaded"));
        }
        // The git list follows the working tree while it is open (not while reading a diff).
        let fresh = match &self.overlay {
            Some(Overlay::Git(g)) if g.diff.is_none() && g.error.is_none() => git::status(&g.top).ok(),
            _ => None,
        };
        if let (Some(entries), Some(Overlay::Git(g))) = (fresh, self.overlay.as_mut()) {
            let same = entries.len() == g.entries.len()
                && entries
                    .iter()
                    .zip(&g.entries)
                    .all(|(a, b)| a.path == b.path && a.status == b.status);
            if !same {
                let keep = g.entries.get(g.selected).map(|e| e.path.clone());
                g.selected = keep
                    .and_then(|p| entries.iter().position(|e| e.path == p))
                    .unwrap_or_else(|| g.selected.min(entries.len().saturating_sub(1)));
                g.entries = entries;
                self.dirty = true;
            }
        }
    }

    pub fn open_file(&mut self, path: &Path) {
        match Buffer::open(path) {
            Ok(b) => self.buffer = b,
            Err(e) => self.set_status(format!("cannot open {}: {e}", path.display())),
        }
        self.dirty = true;
    }

    // ------------------------------------------------------------- events

    pub fn on_event(&mut self, ev: Event) {
        // Bare pointer motion changes nothing; don't redraw for it.
        if let Event::Mouse(m) = &ev
            && m.kind == MouseEventKind::Moved
        {
            return;
        }
        self.dirty = true;
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Paste(text) => self.on_paste(&text),
            Event::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        if self.overlay.is_some() {
            return;
        }
        match self.focus {
            Focus::Ai => {
                if let (true, Some(a)) = (self.agent_live(), self.agent()) {
                    a.send_paste(text);
                }
            }
            Focus::Editor => self.buffer.paste(text),
            Focus::Term => {
                if let (true, Some(t)) = (self.term_live(), self.term.as_ref()) {
                    t.send_paste(text);
                }
            }
            Focus::Tree => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);

        if !(k.code == KeyCode::Esc && self.focus == Focus::Tree) {
            self.quit_armed = false;
        }

        if self.overlay.is_some() {
            if ctrl && matches!(k.code, KeyCode::Char('q' | 'c')) {
                self.should_quit = true;
            } else {
                self.key_overlay(k.code, ctrl);
            }
            return;
        }

        // These work in every pane, so the AI pane, which receives every other key
        // (Tab and Esc included), can be left; so can a click on another pane.
        // Ctrl+Space (or Ctrl+\, which terminals report as Ctrl+4) moves focus on.
        if ctrl && matches!(k.code, KeyCode::Char(' ' | '\\' | '4')) {
            if self.ai_full {
                self.ai_full = false;
            }
            self.cycle_focus(true);
            return;
        }
        // Ctrl+Esc (or Ctrl+`, as in VS Code): the terminal panel at the bottom.
        // Terminals can only tell these apart from Esc / Ctrl+Space with the
        // keyboard enhancement main.rs turns on; Ctrl+T works everywhere else.
        if ctrl && matches!(k.code, KeyCode::Esc | KeyCode::Char('`')) {
            self.toggle_term();
            return;
        }
        // Alt+↓ goes down to the terminal panel, Alt+↑ back up.
        if k.modifiers == KeyModifiers::ALT && matches!(k.code, KeyCode::Up | KeyCode::Down) {
            if k.code == KeyCode::Down && self.show_term && self.focus != Focus::Term {
                self.ai_full = false;
                self.term_return = self.focus;
                self.focus = Focus::Term;
            } else if k.code == KeyCode::Up && self.focus == Focus::Term {
                self.focus = self.term_return;
                self.fix_layout(Focus::Editor);
            } else if self.focus == Focus::Ai
                && self.agent_live()
                && let Some(a) = self.agent_mut()
            {
                a.send_key(k);
            }
            return;
        }
        // F11 or Alt+Z: the AI pane full screen and back. Alt+E: hide/show the
        // editor, Alt+T the file tree (Alt+B is word-left in the chat).
        if k.code == KeyCode::F(11) || (k.modifiers == KeyModifiers::ALT && k.code == KeyCode::Char('z')) {
            self.toggle_ai_full();
            return;
        }
        if k.modifiers == KeyModifiers::ALT && k.code == KeyCode::Char('e') {
            self.toggle_editor();
            return;
        }
        if k.modifiers == KeyModifiers::ALT && k.code == KeyCode::Char('t') {
            self.toggle_tree();
            return;
        }
        // Chat tabs: Alt+N a new one, Alt+1…9 go to one, Alt+, / Alt+. the previous / next.
        if k.modifiers == KeyModifiers::ALT || k.modifiers == KeyModifiers::ALT | KeyModifiers::SHIFT {
            match k.code {
                KeyCode::Char('n' | 'N') => return self.new_tab(),
                KeyCode::Char(c @ '1'..='9') => {
                    let i = usize::from(c as u8 - b'1');
                    if i < self.tabs.len() {
                        self.switch_tab(i);
                    } else {
                        self.set_status(format!("no chat tab {}", i + 1));
                    }
                    return;
                }
                KeyCode::Char(',' | '<') => return self.step_tab(false),
                KeyCode::Char('.' | '>') => return self.step_tab(true),
                _ => {}
            }
        }
        // Alt+←/→ go to the pane on that side. novacode also moves by word with
        // Ctrl+←/→, so the AI pane loses nothing.
        if k.modifiers == KeyModifiers::ALT && matches!(k.code, KeyCode::Left | KeyCode::Right) {
            if self.ai_full {
                // Leaving the full-screen chat brings the other panes back.
                self.ai_full = false;
            }
            self.focus_side(k.code == KeyCode::Right);
            return;
        }

        // Ctrl+G opens the git changes from every pane, the AI chat included.
        if ctrl && k.code == KeyCode::Char('g') {
            self.ai_full = false;
            self.show_tree = true;
            self.open_git();
            return;
        }

        // The AI pane is the real novacode: it gets every other key untouched.
        if self.focus == Focus::Ai && self.agent_live() {
            if let Some(a) = self.agent_mut() {
                a.send_key(k);
            }
            return;
        }
        // So is the shell in the terminal panel.
        if self.focus == Focus::Term && self.term_live() {
            if let Some(t) = self.term.as_mut() {
                t.send_key(k);
            }
            return;
        }

        match k.code {
            KeyCode::Char('q' | 'c') if ctrl => self.should_quit = true,
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Char('b') if ctrl => self.toggle_tree(),
            KeyCode::Char('j') if ctrl => self.toggle_ai(),
            KeyCode::Char('e') if ctrl => self.toggle_editor(),
            KeyCode::Char('t') if ctrl => self.toggle_term(),
            KeyCode::Char('g') if ctrl => self.open_git(),
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            KeyCode::Esc => self.escape(),
            _ => match self.focus {
                Focus::Tree => self.key_tree(k.code),
                Focus::Editor => self.key_editor(k.code, ctrl, alt),
                Focus::Ai => {
                    // Only reached when novacode is not running: Enter restarts it.
                    if k.code == KeyCode::Enter {
                        self.start_agent();
                    }
                }
                Focus::Term => {
                    // Only reached when the shell could not start: Enter retries.
                    if k.code == KeyCode::Enter {
                        self.start_term();
                    }
                }
            },
        }
    }

    fn escape(&mut self) {
        match self.focus {
            Focus::Tree => {
                if self.buffer.modified && !self.quit_armed {
                    self.quit_armed = true;
                    self.set_status("unsaved changes: Esc again to quit, Ctrl+S to save");
                } else {
                    self.should_quit = true;
                }
            }
            Focus::Editor | Focus::Ai | Focus::Term => {
                self.ai_full = false;
                self.show_tree = true;
                self.focus = Focus::Tree;
            }
        }
    }

    /// The panes on screen: the columns left to right, then the terminal panel.
    fn visible_panes(&self) -> Vec<Focus> {
        let mut v = self.columns();
        if self.show_term && !self.ai_full {
            v.push(Focus::Term);
        }
        v
    }

    /// The column panes on screen, left to right.
    fn columns(&self) -> Vec<Focus> {
        if self.ai_full {
            return vec![Focus::Ai];
        }
        [
            (self.show_tree, Focus::Tree),
            (self.show_editor, Focus::Editor),
            (self.show_ai, Focus::Ai),
        ]
        .into_iter()
        .filter_map(|(on, f)| on.then_some(f))
        .collect()
    }

    fn cycle_focus(&mut self, forward: bool) {
        let order = self.visible_panes();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward { (at + 1) % n } else { (at + n - 1) % n }];
    }

    /// Focus the pane right (or left) of the current one; stays at the edge.
    fn focus_side(&mut self, right: bool) {
        let order = self.columns();
        let from = if self.focus == Focus::Term {
            self.term_return
        } else {
            self.focus
        };
        let at = order.iter().position(|f| *f == from).unwrap_or(0);
        let to = if right {
            (at + 1).min(order.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        self.focus = order[to];
    }

    /// Keep at least one pane on screen and the focus on a visible pane.
    fn fix_layout(&mut self, prefer: Focus) {
        if !(self.show_tree || self.show_editor || self.show_ai) {
            self.show_editor = true;
        }
        let order = self.visible_panes();
        if self.focus == Focus::Term && !order.contains(&Focus::Term) {
            self.focus = self.term_return;
        }
        if !order.contains(&self.focus) {
            self.focus = if order.contains(&prefer) { prefer } else { order[0] };
        }
    }

    fn toggle_tree(&mut self) {
        self.ai_full = false;
        self.show_tree = !self.show_tree;
        if self.show_tree {
            self.focus = Focus::Tree;
        }
        self.fix_layout(Focus::Editor);
    }

    fn toggle_editor(&mut self) {
        self.ai_full = false;
        self.show_editor = !self.show_editor;
        if self.show_editor {
            self.focus = Focus::Editor;
        }
        self.fix_layout(Focus::Ai);
    }

    fn toggle_ai(&mut self) {
        self.ai_full = false;
        self.show_ai = !self.show_ai;
        if self.show_ai {
            self.focus = Focus::Ai;
        }
        self.fix_layout(Focus::Editor);
    }

    fn toggle_ai_full(&mut self) {
        self.ai_full = !self.ai_full;
        if self.ai_full {
            self.show_ai = true;
            self.focus = Focus::Ai;
        } else {
            self.fix_layout(Focus::Ai);
        }
    }

    fn save(&mut self) {
        match self.buffer.save() {
            Ok(p) => self.set_status(format!("saved {}", self.rel(&p))),
            Err(e) => self.set_status(format!("save failed: {e}")),
        }
    }

    // --------------------------------------------------------------- tree

    fn key_tree(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.tree.up();
                self.preview_selected();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.tree.down();
                self.preview_selected();
            }
            KeyCode::Left | KeyCode::Char('h') => self.tree.collapse_or_parent(),
            KeyCode::Right | KeyCode::Char('l') => {
                if !self.tree.expand() {
                    self.open_selected(true);
                }
            }
            KeyCode::Enter => {
                if self.tree.selected_node().is_some_and(|n| n.is_dir) {
                    self.tree.toggle();
                } else {
                    self.open_selected(true);
                }
            }
            _ => {}
        }
    }

    /// Auto-load the file under the cursor, but never clobber unsaved edits.
    fn preview_selected(&mut self) {
        if self.buffer.modified {
            return;
        }
        self.open_selected(false);
    }

    fn open_selected(&mut self, focus_editor: bool) {
        let Some(node) = self.tree.selected_node() else {
            return;
        };
        if node.is_dir {
            return;
        }
        let path = node.path.clone();
        self.open_path(&path, focus_editor);
    }

    fn open_path(&mut self, path: &Path, focus_editor: bool) {
        if self.buffer.path.as_deref() != Some(path) {
            if self.buffer.modified {
                self.set_status("unsaved changes: save with Ctrl+S before opening another file");
                return;
            }
            self.open_file(path);
        }
        if focus_editor && self.buffer.path.is_some() {
            // Opening a file brings the editor back if it was hidden.
            self.ai_full = false;
            self.show_editor = true;
            self.focus = Focus::Editor;
        }
    }

    // ------------------------------------------------------------- editor

    fn key_editor(&mut self, code: KeyCode, ctrl: bool, alt: bool) {
        let b = &mut self.buffer;
        match code {
            KeyCode::Char(c) if !ctrl && !alt => b.insert_char(c),
            KeyCode::Enter => b.newline(),
            KeyCode::Backspace => b.backspace(),
            KeyCode::Delete => b.delete(),
            KeyCode::Left => b.left(),
            KeyCode::Right => b.right(),
            KeyCode::Up => b.up(),
            KeyCode::Down => b.down(),
            KeyCode::Home => b.home(),
            KeyCode::End => b.end(),
            KeyCode::PageUp => b.page_up(),
            KeyCode::PageDown => b.page_down(),
            _ => {}
        }
    }

    // -------------------------------------------------------------- mouse

    fn pane_at(&self, col: u16, row: u16) -> Option<Focus> {
        let p = Position::new(col, row);
        let hit = |r: Option<Rect>| r.is_some_and(|r| r.contains(p));
        if hit(self.panes.tree) {
            Some(Focus::Tree)
        } else if hit(Some(self.panes.editor)) {
            Some(Focus::Editor)
        } else if hit(self.panes.ai) {
            Some(Focus::Ai)
        } else if hit(self.panes.term) {
            Some(Focus::Term)
        } else {
            None
        }
    }

    /// A click focuses the pane under it; the wheel scrolls the pane under it.
    fn on_mouse(&mut self, m: MouseEvent) {
        if self.overlay.is_some() {
            // The git list scrolls with the wheel; clicks do nothing there.
            let code = match m.kind {
                MouseEventKind::ScrollUp => KeyCode::Up,
                MouseEventKind::ScrollDown => KeyCode::Down,
                _ => return,
            };
            self.key_overlay(code, false);
            return;
        }
        let hit = self.pane_at(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(button) => {
                self.mouse_owner = hit;
                let Some(pane) = hit else {
                    return;
                };
                if pane == Focus::Term && self.focus != Focus::Term {
                    self.term_return = self.focus;
                }
                self.focus = pane;
                if pane != Focus::Tree {
                    self.last_click = None;
                }
                match pane {
                    Focus::Ai if self.click_tab(m.column, m.row) => {}
                    Focus::Ai => self.mouse_to_agent(m),
                    Focus::Term => self.mouse_to_term(m),
                    Focus::Tree if button == MouseButton::Left => self.click_tree(m.row),
                    Focus::Editor if button == MouseButton::Left => {
                        self.click_editor(m.column, m.row);
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                match self.mouse_owner {
                    Some(Focus::Ai) => self.mouse_to_agent(m),
                    Some(Focus::Term) => self.mouse_to_term(m),
                    _ => {}
                }
                if matches!(m.kind, MouseEventKind::Up(_)) {
                    self.mouse_owner = None;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if m.kind == MouseEventKind::ScrollUp {
                    -WHEEL_LINES
                } else {
                    WHEEL_LINES
                };
                match hit {
                    Some(Focus::Tree) => {
                        self.tree.scroll_by(delta, usize::from(self.panes.tree_inner.height));
                    }
                    Some(Focus::Editor) => self.buffer.scroll_by(delta),
                    Some(Focus::Ai) => self.mouse_to_agent(m),
                    Some(Focus::Term) => {
                        // Programs that track the mouse (less, vim) get the wheel;
                        // at a prompt it scrolls back through the output.
                        let wants = self.term.as_ref().is_some_and(Agent::wants_mouse);
                        if wants {
                            self.mouse_to_term(m);
                        } else if let Some(t) = self.term.as_mut() {
                            t.scroll_by(delta);
                        }
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// Single click: select (and preview a file, or fold a directory).
    /// Double click on a file: open it in the editor.
    fn click_tree(&mut self, row: u16) {
        let inner = self.panes.tree_inner;
        if row < inner.y || row >= inner.bottom() {
            return;
        }
        let i = self.tree.offset + usize::from(row - inner.y);
        let Some(is_dir) = self.tree.nodes.get(i).map(|n| n.is_dir) else {
            return;
        };
        let double = self
            .last_click
            .take()
            .is_some_and(|(j, at)| j == i && at.elapsed() < DOUBLE_CLICK);
        if !double {
            self.last_click = Some((i, Instant::now()));
        }
        self.tree.selected = i;
        if is_dir {
            // The second click of a double click must not fold what the first opened.
            if !double {
                self.tree.toggle();
            }
        } else if double {
            self.open_selected(true);
        } else {
            self.preview_selected();
        }
    }

    fn click_editor(&mut self, col: u16, row: u16) {
        let inner = self.panes.editor_inner;
        let b = &mut self.buffer;
        if (b.path.is_none() && b.is_empty()) || row < inner.y || row >= inner.bottom() {
            return;
        }
        let line = b.scroll + usize::from(row - inner.y);
        let dcol = b.hscroll + usize::from(col.saturating_sub(self.panes.editor_text_x));
        b.set_cursor_display(line, dcol);
    }

    /// Pass a mouse event to the hosted novacode in its own coordinates. Presses
    /// and the wheel count inside the pane only; a drag that leaves the pane is
    /// pinned to its edge, as a terminal does.
    fn mouse_to_agent(&self, m: MouseEvent) {
        if self.agent_live() {
            mouse_to(self.agent(), self.panes.ai_inner, m);
        }
    }

    /// A click on a tab label on the AI pane's top edge shows that tab.
    fn click_tab(&mut self, col: u16, row: u16) -> bool {
        if self.panes.ai.is_none_or(|r| r.y != row) {
            return false;
        }
        match self.tab_hits.iter().position(|&(from, to)| (from..to).contains(&col)) {
            Some(i) => {
                self.switch_tab(i);
                true
            }
            None => false,
        }
    }

    /// The same for the shell in the terminal panel.
    fn mouse_to_term(&self, m: MouseEvent) {
        if self.term_live() {
            mouse_to(self.term.as_ref(), self.panes.term_inner, m);
        }
    }

    /// Path relative to the tree root when possible.
    pub fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.tree.root).unwrap_or(p).display().to_string()
    }
}

/// Send a mouse event to a hosted program drawn in `r`, in its own coordinates.
fn mouse_to(target: Option<&Agent>, r: Rect, m: MouseEvent) {
    let inside = r.contains(Position::new(m.column, m.row));
    let drag = matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_));
    if r.is_empty() || !(inside || drag) {
        return;
    }
    let col = m.column.clamp(r.x, r.right() - 1) - r.x;
    let row = m.row.clamp(r.y, r.bottom() - 1) - r.y;
    if let Some(a) = target {
        a.send_mouse(m.kind, m.modifiers, col, row);
    }
}

// ------------------------------------------------------------------ overlays

impl App {
    fn key_overlay(&mut self, code: KeyCode, ctrl: bool) {
        let Some(ov) = self.overlay.take() else {
            return;
        };
        self.overlay = match ov {
            Overlay::Git(g) => self.key_git(g, code, ctrl),
        };
    }

    // ---- git

    fn open_git(&mut self) {
        self.overlay = Some(Overlay::Git(load_git(&self.tree.root)));
    }

    fn key_git(&mut self, mut g: GitView, code: KeyCode, ctrl: bool) -> Option<Overlay> {
        if ctrl && code == KeyCode::Char('g') {
            return None;
        }
        let up = matches!(code, KeyCode::Up | KeyCode::Char('k'));
        let down = matches!(code, KeyCode::Down | KeyCode::Char('j'));
        if let Some(diff) = &g.diff {
            let last = diff.len().saturating_sub(1);
            match code {
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('q') => {
                    g.diff = None;
                }
                KeyCode::PageUp => g.scroll = g.scroll.saturating_sub(15),
                KeyCode::PageDown => g.scroll = (g.scroll + 15).min(last),
                KeyCode::Char('o') => return self.open_git_file(&g),
                _ if up => g.scroll = g.scroll.saturating_sub(1),
                _ if down => g.scroll = (g.scroll + 1).min(last),
                _ => {}
            }
            return Some(Overlay::Git(g));
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => return None,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('d') => {
                if let Some(e) = g.entries.get(g.selected) {
                    g.diff = Some(git::diff(&g.top, e));
                    g.scroll = 0;
                }
            }
            KeyCode::Char('o') => return self.open_git_file(&g),
            KeyCode::Char('r') => g = load_git(&self.tree.root),
            _ if up => g.selected = g.selected.saturating_sub(1),
            _ if down => g.selected = (g.selected + 1).min(g.entries.len().saturating_sub(1)),
            _ => {}
        }
        Some(Overlay::Git(g))
    }

    /// Open the selected changed file in the editor and close the overlay.
    fn open_git_file(&mut self, g: &GitView) -> Option<Overlay> {
        let entry = g.entries.get(g.selected)?;
        let path = g.top.join(&entry.path);
        if path.is_file() {
            self.open_path(&path, true);
            None
        } else {
            self.set_status(format!("{} is not an openable file", entry.path));
            Some(Overlay::Git(GitView {
                top: g.top.clone(),
                entries: git::status(&g.top).unwrap_or_default(),
                selected: g.selected,
                diff: None,
                scroll: 0,
                error: None,
            }))
        }
    }
}

fn load_git(root: &Path) -> GitView {
    let result = git::toplevel(root).and_then(|top| git::status(&top).map(|e| (top, e)));
    let (top, entries, error) = match result {
        Ok((top, entries)) => (top, entries, None),
        Err(e) => (root.to_path_buf(), Vec::new(), Some(e)),
    };
    GitView {
        top,
        entries,
        selected: 0,
        diff: None,
        scroll: 0,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A scratch project; removed on drop. Holds the agent channel open.
    struct Scratch {
        dir: PathBuf,
        _rx: tokio::sync::mpsc::UnboundedReceiver<(u64, AgentEvent)>,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// An app on a scratch project, drawn at 120x30, with `cat` hosted in the AI
    /// pane in place of novacode (so the pane is live and swallows keys).
    fn app(name: &str) -> (App, Scratch) {
        let dir = std::env::temp_dir().join(format!("nova-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "x").unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let long: String = (0..100).map(|i| format!("{i}\n")).collect();
        std::fs::write(dir.join("long.txt"), long).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::without_agent(dir.clone(), tx.clone(), tx.clone(), (120, 30));
        app.tabs.push(Agent::spawn_program("cat", &dir, 27, 34, tx, 0).unwrap());
        draw(&mut app);
        (app, Scratch { dir, _rx: rx })
    }

    fn draw(app: &mut App) {
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
    }

    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.on_event(Event::Key(KeyEvent::new(code, mods)));
    }

    fn mouse(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) {
        let modifiers = KeyModifiers::NONE;
        app.on_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }));
    }

    fn click(app: &mut App, at: (u16, u16)) {
        mouse(app, MouseEventKind::Down(MouseButton::Left), at);
        mouse(app, MouseEventKind::Up(MouseButton::Left), at);
    }

    fn middle(r: Rect) -> (u16, u16) {
        (r.x + r.width / 2, r.y + r.height / 2)
    }

    /// `app` with two more `cat` chats as tabs (ids 1 and 2), the first on screen.
    fn tabbed(name: &str) -> (App, Scratch) {
        let (mut app, s) = app(name);
        let tx = app.agent_tx.clone();
        for id in 1..=2 {
            app.tabs
                .push(Agent::spawn_program("cat", &s.dir, 27, 34, tx.clone(), id).unwrap());
        }
        draw(&mut app);
        (app, s)
    }

    #[test]
    fn chat_tabs_switch_with_keys_and_clicks() {
        let (mut app, _s) = tabbed("tabs");
        assert_eq!((app.tabs.len(), app.active), (3, 0));
        assert_eq!(app.tab_hits.len(), 3, "three tab labels drawn");
        key(&mut app, KeyCode::Char('3'), KeyModifiers::ALT);
        assert_eq!((app.active, app.focus), (2, Focus::Ai));
        key(&mut app, KeyCode::Char('.'), KeyModifiers::ALT);
        assert_eq!(app.active, 0, "next wraps around");
        key(&mut app, KeyCode::Char(','), KeyModifiers::ALT);
        assert_eq!(app.active, 2, "previous wraps around");
        key(&mut app, KeyCode::Char('9'), KeyModifiers::ALT);
        assert_eq!(app.active, 2, "no tab 9");
        draw(&mut app);
        let (from, _) = app.tab_hits[1];
        let row = app.panes.ai.unwrap().y;
        click(&mut app, (from, row));
        assert_eq!(app.active, 1, "a click on a label shows that tab");
    }

    #[test]
    fn quitting_one_of_several_chats_closes_its_tab_only() {
        let (mut app, _s) = tabbed("tabquit");
        app.switch_tab(2);
        app.on_agent(1, AgentEvent::Output(b"hi".to_vec()));
        app.on_agent(1, AgentEvent::Exited);
        assert!(!app.should_quit);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(
            (app.active, app.tabs[app.active].id),
            (1, 2),
            "the same chat stays on screen"
        );
        app.on_agent(2, AgentEvent::Exited);
        assert_eq!((app.tabs.len(), app.active), (1, 0));
        app.on_agent(0, AgentEvent::Exited);
        assert!(app.should_quit, "the last chat quitting closes the window");
    }

    #[test]
    fn the_ai_pane_can_be_left_with_keys() {
        let (mut app, _s) = app("keys");
        assert!(app.agent_live());
        app.focus = Focus::Ai;
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Ai, "Tab, Shift+Tab and Esc belong to novacode");
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Editor);
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Tree, "stops at the left edge");
        key(&mut app, KeyCode::Right, KeyModifiers::ALT);
        key(&mut app, KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Ai);
        key(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(app.focus, Focus::Tree, "Ctrl+Space wraps around");
    }

    #[test]
    fn a_click_focuses_the_pane_under_it() {
        let (mut app, _s) = app("focus");
        let (tree, editor, ai) = (app.panes.tree.unwrap(), app.panes.editor, app.panes.ai.unwrap());
        for (rect, want) in [
            (ai, Focus::Ai),
            (editor, Focus::Editor),
            (ai, Focus::Ai),
            (tree, Focus::Tree),
        ] {
            click(&mut app, middle(rect));
            assert_eq!(app.focus, want);
        }
    }

    #[test]
    fn tree_clicks_select_unfold_and_open() {
        let (mut app, _s) = app("tree");
        let r = app.panes.tree_inner;
        // Rows: sub/, a.txt, long.txt
        click(&mut app, (r.x + 2, r.y + 1));
        assert_eq!((app.focus, app.tree.selected), (Focus::Tree, 1));
        assert!(
            app.buffer.path.as_ref().is_some_and(|p| p.ends_with("a.txt")),
            "previewed"
        );
        click(&mut app, (r.x + 2, r.y + 1));
        assert_eq!(app.focus, Focus::Editor, "a double click opens the file");
        click(&mut app, (r.x + 2, r.y));
        assert_eq!(
            (app.focus, app.tree.nodes.len()),
            (Focus::Tree, 4),
            "a click unfolds sub/"
        );
    }

    #[test]
    fn an_editor_click_moves_the_cursor() {
        let (mut app, s) = app("editor");
        app.open_file(&s.dir.join("a.txt"));
        draw(&mut app);
        let at = (app.panes.editor_text_x + 2, app.panes.editor_inner.y + 1);
        click(&mut app, at);
        assert_eq!((app.focus, app.buffer.line, app.buffer.col), (Focus::Editor, 1, 2));
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_pointer() {
        let (mut app, s) = app("wheel");
        app.open_file(&s.dir.join("long.txt"));
        draw(&mut app);
        let at = middle(app.panes.editor_inner);
        mouse(&mut app, MouseEventKind::ScrollDown, at);
        assert_eq!(app.buffer.scroll, 3);
        assert_eq!(app.focus, Focus::Tree, "scrolling does not move focus");
    }

    #[test]
    fn the_ai_pane_goes_full_screen_and_back() {
        let (mut app, _s) = app("full");
        app.focus = Focus::Ai;
        key(&mut app, KeyCode::F(11), KeyModifiers::NONE);
        draw(&mut app);
        assert!(app.ai_full && app.panes.tree.is_none());
        assert_eq!(app.panes.ai.unwrap().width, 120, "fills the window");
        key(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
        draw(&mut app);
        assert!(!app.ai_full && app.panes.tree.is_some());
        key(&mut app, KeyCode::F(11), KeyModifiers::NONE);
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert!(!app.ai_full, "Alt+← leaves full screen");
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn ctrl_esc_opens_the_terminal_panel_at_the_bottom() {
        let (mut app, s) = app("term");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.term = Some(Agent::spawn_program("cat", &s.dir, 8, 100, tx, 0).unwrap());
        app.focus = Focus::Ai;
        key(&mut app, KeyCode::Esc, KeyModifiers::CONTROL);
        draw(&mut app);
        assert!(app.show_term && app.focus == Focus::Term);
        let t = app.panes.term.unwrap();
        assert_eq!((t.x, t.width), (0, 120), "spans the window");
        assert!(t.y > app.panes.editor.bottom() - 1, "below the editor");
        assert_eq!(t.bottom(), 29, "above the status bar");
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Term, "plain Esc goes to the shell");
        key(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Ai, "Alt+↑ goes back up");
        click(&mut app, middle(t));
        assert_eq!(app.focus, Focus::Term);
        key(&mut app, KeyCode::Esc, KeyModifiers::CONTROL);
        draw(&mut app);
        assert!(!app.show_term && app.panes.term.is_none() && app.focus == Focus::Ai);
        key(&mut app, KeyCode::Char('`'), KeyModifiers::CONTROL);
        assert!(app.show_term && app.focus == Focus::Term, "Ctrl+` too");
    }

    #[test]
    fn the_editor_can_be_hidden() {
        let (mut app, s) = app("noeditor");
        app.focus = Focus::Ai;
        key(&mut app, KeyCode::Char('e'), KeyModifiers::ALT);
        draw(&mut app);
        assert!(!app.show_editor && app.panes.editor.is_empty());
        assert_eq!(app.focus, Focus::Ai);
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Tree, "the hidden editor is skipped");
        app.open_path(&s.dir.join("a.txt"), true);
        assert!(app.show_editor && app.focus == Focus::Editor, "opening a file shows it");
    }

    #[test]
    fn the_tree_can_be_hidden_from_the_ai_pane() {
        let (mut app, _s) = app("notree");
        app.focus = Focus::Ai;
        key(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        assert!(!app.show_tree);
        assert_eq!(app.focus, Focus::Ai, "hiding the tree keeps the focus");
        key(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        assert!(app.show_tree && app.focus == Focus::Tree, "Alt+T shows it again");
    }
}
