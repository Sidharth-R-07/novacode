//! Ratatui rendering: file tree, editor, AI panel and the status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Focus, GitView, Overlay, Panes};
use crate::buffer::visible_slice;

const ACTIVE_LINE_BG: Color = Color::Rgb(40, 44, 52);
const BAR_BG: Color = Color::Rgb(30, 32, 38);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, bar] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());

    app.panes = Panes::default();
    if app.ai_full {
        draw_ai(f, app, main);
        draw_status(f, app, bar);
        let c = centered(main, 80, 80);
        draw_overlay(f, app, c, c);
        return;
    }

    // The terminal panel takes about a fifth of the height (at least 6 rows),
    // across the whole width.
    let main = if app.show_term {
        let h = (main.height / 5).max(6);
        let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(h)]).areas(main);
        draw_term(f, app, bottom);
        top
    } else {
        main
    };

    // 20 / 50 / 30 when everything is visible; the rest share space by weight.
    // Without the editor the chat takes its share: 20 / 80.
    let mut weights = Vec::with_capacity(3);
    if app.show_tree {
        weights.push(Constraint::Fill(2));
    }
    if app.show_editor {
        weights.push(Constraint::Fill(5));
    }
    if app.show_ai {
        weights.push(Constraint::Fill(if app.show_editor { 3 } else { 8 }));
    }
    let cols = Layout::horizontal(weights).split(main);
    let mut cols = cols.iter().copied();

    let mut left = centered(main, 80, 80);
    let mut wide = left;
    if app.show_tree {
        let a = cols.next().unwrap_or_default();
        left = a;
        wide = a;
        if app.overlay.is_none() {
            draw_tree(f, app, a);
        }
    }
    if app.show_editor {
        let a = cols.next().unwrap_or_default();
        if app.show_tree {
            wide = Rect {
                width: wide.width + a.width,
                ..wide
            };
        }
        draw_editor(f, app, a);
    }
    if app.show_ai {
        draw_ai(f, app, cols.next().unwrap_or_default());
    }
    draw_status(f, app, bar);
    draw_overlay(f, app, left, wide);
}

fn pane(title: &str, focused: bool) -> Block<'static> {
    let (color, mods) = if focused {
        (Color::Cyan, Modifier::BOLD)
    } else {
        (Color::DarkGray, Modifier::empty())
    };
    Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(color).add_modifier(mods))
        .title_style(Style::new().fg(color).add_modifier(mods))
}

// ------------------------------------------------------------------- tree

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Tree;
    let block = pane("Files", focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.panes.tree = Some(area);
    app.panes.tree_inner = inner;

    let h = inner.height as usize;
    let t = &mut app.tree;
    if t.selected < t.offset {
        t.offset = t.selected;
    } else if h > 0 && t.selected >= t.offset + h {
        t.offset = t.selected + 1 - h;
    }

    let lines: Vec<Line> = t
        .nodes
        .iter()
        .enumerate()
        .skip(t.offset)
        .take(h)
        .map(|(i, n)| {
            let icon = if n.is_dir { "📁" } else { "📄" };
            let style = if i == t.selected {
                if focused {
                    Style::new().bg(Color::Cyan).fg(Color::Black)
                } else {
                    Style::new().bg(Color::DarkGray).fg(Color::White)
                }
            } else if n.is_dir {
                Style::new().fg(Color::Blue)
            } else {
                Style::new()
            };
            Line::styled(format!("{}{icon} {}", "  ".repeat(n.depth), n.name), style)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ----------------------------------------------------------------- editor

fn draw_editor(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Editor;
    let title = match &app.buffer.path {
        Some(p) => format!("{}{}", app.rel(p), if app.buffer.modified { " ●" } else { "" }),
        None => "Editor".to_string(),
    };
    let block = pane(&title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.panes.editor = area;
    app.panes.editor_inner = inner;

    let b = &mut app.buffer;
    if b.path.is_none() && b.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from("nova-edit").style(Style::new().add_modifier(Modifier::BOLD)),
            Line::from(""),
            Line::from("Pick a file in the tree (Enter or double-click), then edit."),
            Line::from("Switch panes with Tab, Alt+←/→, Ctrl+Space or a click. Ctrl+S saves."),
            Line::from(""),
            Line::from("In the AI pane Tab and Esc go to novacode;"),
            Line::from("leave it with Alt+←, Ctrl+Space or a click on another pane."),
            Line::from(""),
            Line::from("F11 or Alt+Z: AI chat full screen. Alt+E (Ctrl+E here): hide the editor."),
        ])
        .style(Style::new().fg(Color::DarkGray))
        .wrap(ratatui::widgets::Wrap { trim: false });
        f.render_widget(hint, inner);
        return;
    }

    let gutter = b.line_count().to_string().len() + 1;
    app.panes.editor_text_x = inner.x.saturating_add(u16::try_from(gutter).unwrap_or(u16::MAX));
    let text_w = (inner.width as usize).saturating_sub(gutter + 1);
    let h = inner.height as usize;
    b.view_height = h;

    // Keep the cursor in view.
    if b.line < b.scroll {
        b.scroll = b.line;
    } else if h > 0 && b.line >= b.scroll + h {
        b.scroll = b.line + 1 - h;
    }
    let dcol = b.display_col();
    if dcol < b.hscroll {
        b.hscroll = dcol;
    } else if text_w > 0 && dcol >= b.hscroll + text_w {
        b.hscroll = dcol + 1 - text_w;
    }

    let lines: Vec<Line> = (b.scroll..b.line_count())
        .take(h)
        .map(|i| {
            let current = i == b.line;
            let num_style = if current {
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let text = visible_slice(b.line_chars(i), b.hscroll, text_w);
            let line = Line::from(vec![
                Span::styled(format!("{:>w$} ", i + 1, w = gutter - 1), num_style),
                Span::raw(text),
            ]);
            if current && focused {
                line.style(Style::new().bg(ACTIVE_LINE_BG))
            } else {
                line
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    if focused {
        let x = inner.x + (gutter + dcol - b.hscroll) as u16;
        let y = inner.y + (b.line - b.scroll) as u16;
        if x < inner.right() && y < inner.bottom() {
            f.set_cursor_position((x, y));
        }
    }
}

// ------------------------------------------------------- AI pane (novacode)

fn draw_ai(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Ai;
    let live = app.agent_live();
    let title = if app.ai_full {
        "AI · novacode · full screen"
    } else {
        "AI · novacode"
    };
    let mut block = pane(title, focused);
    if focused && live {
        // Every key but these goes to novacode, so say how to get out, on the
        // bottom edge: the top row is the one a desktop bar is likely to cover.
        let hint = if app.ai_full {
            " F11/Alt+Z exit full screen · Alt+← leave "
        } else {
            " leave: Alt+← · Ctrl+Space · click · F11 full "
        };
        block = block.title_bottom(Line::from(hint).right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.agent_size = (inner.height, inner.width);
    app.panes.ai = Some(area);
    app.panes.ai_inner = inner;

    let Some(agent) = app.agent.as_mut() else {
        let msg = app.agent_error.clone().unwrap_or_default();
        let lines = vec![
            Line::styled(msg, Style::new().fg(Color::Yellow)),
            Line::from(""),
            Line::styled("Press Enter to retry.", Style::new().fg(Color::DarkGray)),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        return;
    };
    agent.resize(inner.height, inner.width);
    render_screen(f.buffer_mut(), inner, agent.screen());

    if agent.exited {
        let note = " novacode exited: press Enter to restart ";
        let row = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        f.render_widget(
            Paragraph::new(note).style(Style::new().bg(Color::Yellow).fg(Color::Black)),
            row,
        );
    } else if focused && !agent.screen().hide_cursor() {
        let (r, c) = agent.screen().cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position((inner.x + c, inner.y + r));
        }
    }
}

// ------------------------------------------------------- terminal panel

fn draw_term(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Term;
    let scrolled = app.term.as_ref().map_or(0, |t| t.screen().scrollback());
    let title = if scrolled > 0 {
        format!("Terminal · scrolled back {scrolled} lines (type to return)")
    } else {
        "Terminal".to_string()
    };
    let mut block = pane(&title, focused);
    if focused && app.term_live() {
        block = block.title_bottom(Line::from(" hide: Ctrl+Esc · leave: Alt+↑ · Ctrl+Space · click ").right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.term_size = (inner.height, inner.width);
    app.panes.term = Some(area);
    app.panes.term_inner = inner;

    let Some(term) = app.term.as_mut() else {
        let msg = app.term_error.clone().unwrap_or_default();
        let lines = vec![
            Line::styled(msg, Style::new().fg(Color::Yellow)),
            Line::styled("Press Enter to retry.", Style::new().fg(Color::DarkGray)),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
        return;
    };
    term.resize(inner.height, inner.width);
    render_screen(f.buffer_mut(), inner, term.screen());
    let s = term.screen();
    if focused && s.scrollback() == 0 && !s.hide_cursor() {
        let (r, c) = s.cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position((inner.x + c, inner.y + r));
        }
    }
}

fn convert(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Copy the emulated terminal screen into the frame, cell by cell.
fn render_screen(buf: &mut ratatui::buffer::Buffer, area: Rect, screen: &vt100::Screen) {
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut mods = Modifier::empty();
            if cell.bold() {
                mods |= Modifier::BOLD;
            }
            if cell.italic() {
                mods |= Modifier::ITALIC;
            }
            if cell.underline() {
                mods |= Modifier::UNDERLINED;
            }
            if cell.inverse() {
                mods |= Modifier::REVERSED;
            }
            let text = cell.contents();
            if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
                dst.set_symbol(if text.is_empty() { " " } else { &text });
                dst.set_style(
                    Style::new()
                        .fg(convert(cell.fgcolor()))
                        .bg(convert(cell.bgcolor()))
                        .add_modifier(mods),
                );
            }
        }
    }
}

// ---------------------------------------------------------------- overlays

fn centered(area: Rect, w_pct: u16, h_pct: u16) -> Rect {
    let [_, v, _] = Layout::vertical([
        Constraint::Percentage((100 - h_pct) / 2),
        Constraint::Percentage(h_pct),
        Constraint::Percentage((100 - h_pct) / 2),
    ])
    .areas(area);
    let [_, h, _] = Layout::horizontal([
        Constraint::Percentage((100 - w_pct) / 2),
        Constraint::Percentage(w_pct),
        Constraint::Percentage((100 - w_pct) / 2),
    ])
    .areas(v);
    h
}

fn selected_style() -> Style {
    Style::new().bg(Color::Cyan).fg(Color::Black)
}

/// Window of `h` rows that keeps `selected` visible.
fn window(selected: usize, h: usize) -> usize {
    if h > 0 && selected >= h { selected + 1 - h } else { 0 }
}

fn draw_overlay(f: &mut Frame, app: &mut App, left: Rect, wide: Rect) {
    match &mut app.overlay {
        None => {}
        Some(Overlay::Git(g)) => {
            let area = if g.diff.is_some() { wide } else { left };
            f.render_widget(Clear, area);
            draw_git(f, g, area);
        }
    }
}

fn draw_git(f: &mut Frame, g: &GitView, area: Rect) {
    if let Some(diff) = &g.diff {
        let name = g.entries.get(g.selected).map_or("", |e| e.path.as_str());
        let block = pane(&format!("diff: {name}  (↑↓ scroll · o open · Esc back)"), true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let lines: Vec<Line> = diff
            .iter()
            .skip(g.scroll)
            .take(inner.height as usize)
            .map(|l| {
                let color = match l.as_bytes().first() {
                    Some(b'+') if !l.starts_with("+++") => Color::Green,
                    Some(b'-') if !l.starts_with("---") => Color::Red,
                    Some(b'@') => Color::Cyan,
                    _ => Color::Gray,
                };
                Line::styled(l.replace('\t', "    "), Style::new().fg(color))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let title = format!(
        "Git changes ({})  Enter diff · o open · r refresh · Esc close",
        g.entries.len()
    );
    let block = pane(&title, true);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(err) = &g.error {
        f.render_widget(
            Paragraph::new(format!("git: {err}")).style(Style::new().fg(Color::Yellow)),
            inner,
        );
        return;
    }
    if g.entries.is_empty() {
        f.render_widget(
            Paragraph::new("Working tree clean.").style(Style::new().fg(Color::Green)),
            inner,
        );
        return;
    }
    let h = inner.height as usize;
    let lines: Vec<Line> = g
        .entries
        .iter()
        .enumerate()
        .skip(window(g.selected, h))
        .take(h)
        .map(|(i, e)| {
            let color = match e.status.trim_start().chars().next() {
                Some('?') | Some('A') => Color::Green,
                Some('D') => Color::Red,
                _ => Color::Yellow,
            };
            let line = Line::from(vec![
                Span::styled(format!(" {} ", e.status), Style::new().fg(color)),
                Span::raw(e.path.clone()),
            ]);
            if i == g.selected {
                line.style(selected_style())
            } else {
                line
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// -------------------------------------------------------------- status bar

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let base = Style::new().bg(BAR_BG).fg(Color::Gray);
    let mut spans = vec![Span::styled(" [FOCUS: ", base)];
    let mut names = vec![("TREE", Focus::Tree), ("EDITOR", Focus::Editor), ("AI", Focus::Ai)];
    if app.show_term {
        names.push(("TERM", Focus::Term));
    }
    for (i, (name, focus)) in names.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", base));
        }
        let style = if app.focus == focus {
            Style::new()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            base.fg(Color::DarkGray)
        };
        spans.push(Span::styled(name, style));
    }
    spans.push(Span::styled("] ", base));

    let file = app
        .buffer
        .path
        .as_deref()
        .map_or_else(|| "[no file]".to_string(), |p| app.rel(p));
    let dirty = if app.buffer.modified { " ●" } else { "" };
    spans.push(Span::styled(
        format!(
            " {file}{dirty}  Ln {}, Col {}  ",
            app.buffer.line + 1,
            app.buffer.col + 1
        ),
        base.fg(Color::White),
    ));

    let tail = if !app.status.is_empty() {
        app.status.clone()
    } else if app.focus == Focus::Ai && app.agent_live() {
        "keys go to novacode · leave Alt+←/Ctrl+Space/click · F11 full · Alt+E editor · Ctrl+Esc terminal".to_string()
    } else if app.focus == Focus::Term && app.term_live() {
        "keys go to the shell · Ctrl+Esc hide · leave Alt+↑/Ctrl+Space/click · wheel scrolls back".to_string()
    } else {
        "Tab/Alt+←→/click focus · ^B tree · ^E editor · ^J AI · F11 full AI · ^S save · ^Esc/^T terminal · ^G git"
            .to_string()
    };
    let color = if app.status.is_empty() {
        Color::DarkGray
    } else {
        Color::Yellow
    };
    spans.push(Span::styled(tail, base.fg(color)));

    f.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}
