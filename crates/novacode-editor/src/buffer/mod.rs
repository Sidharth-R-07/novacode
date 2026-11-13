//! Rope-backed text buffer with a (line, column) cursor.
//!
//! Columns are counted in `char`s. Display columns (tabs, wide glyphs) are
//! derived on demand by the renderer so edits never touch layout state.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ropey::Rope;
use unicode_width::UnicodeWidthChar;

/// Files above this size are refused: the editor is for quick edits, not logs.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const TAB_WIDTH: usize = 4;

const LINE_BREAKS: [char; 7] = ['\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}'];

pub struct Buffer {
    rope: Rope,
    pub path: Option<PathBuf>,
    pub modified: bool,
    /// File modification time at last load/save, to notice outside changes.
    mtime: Option<SystemTime>,
    /// Cursor line (0-based).
    pub line: usize,
    /// Cursor column in chars (0-based).
    pub col: usize,
    /// Column the cursor "wants" when moving vertically across short lines.
    want_col: usize,
    /// First visible line / first visible display column. Owned by the renderer.
    pub scroll: usize,
    pub hscroll: usize,
    /// Height of the viewport at last render, used for PageUp/PageDown.
    pub view_height: usize,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            modified: false,
            mtime: None,
            line: 0,
            col: 0,
            want_col: 0,
            scroll: 0,
            hscroll: 0,
            view_height: 20,
        }
    }
}

impl Buffer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(io::Error::other(format!(
                "file too large ({} MB, limit {} MB)",
                meta.len() >> 20,
                MAX_FILE_BYTES >> 20
            )));
        }
        let bytes = std::fs::read(path)?;
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return Err(io::Error::other("binary file"));
        }
        let text = String::from_utf8_lossy(&bytes);
        Ok(Self {
            rope: Rope::from_str(&text),
            path: Some(path.to_path_buf()),
            mtime: meta.modified().ok(),
            ..Self::default()
        })
    }

    pub fn save(&mut self) -> io::Result<PathBuf> {
        let path = self.path.clone().ok_or_else(|| io::Error::other("no file open"))?;
        self.rope.write_to(BufWriter::new(File::create(&path)?))?;
        self.modified = false;
        self.mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Ok(path)
    }

    /// Reload from disk if the file changed outside the editor and there are no
    /// unsaved edits. The cursor stays where it was, clamped. Returns true if reloaded.
    pub fn reload_if_changed(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let on_disk = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if self.modified || on_disk.is_none() || on_disk == self.mtime {
            return false;
        }
        let Ok(fresh) = Self::open(&path) else {
            return false;
        };
        self.rope = fresh.rope;
        self.mtime = fresh.mtime;
        self.line = self.line.min(self.line_count() - 1);
        self.col = self.col.min(self.line_len(self.line));
        self.want_col = self.col;
        true
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// Length of `line` in chars, excluding its line break.
    pub fn line_len(&self, line: usize) -> usize {
        let l = self.rope.line(line);
        let mut n = l.len_chars();
        while n > 0 && LINE_BREAKS.contains(&l.char(n - 1)) {
            n -= 1;
        }
        n
    }

    /// The chars of `line` without its line break.
    pub fn line_chars(&self, line: usize) -> impl Iterator<Item = char> + '_ {
        self.rope.line(line).chars().take_while(|c| !LINE_BREAKS.contains(c))
    }

    /// Cursor column in terminal cells.
    pub fn display_col(&self) -> usize {
        self.line_chars(self.line).take(self.col).map(char_width).sum()
    }

    fn cursor_idx(&self) -> usize {
        self.rope.line_to_char(self.line) + self.col
    }

    fn touch(&mut self) {
        self.modified = true;
        self.want_col = self.col;
    }

    // ---- edits ----

    pub fn insert_char(&mut self, c: char) {
        self.rope.insert_char(self.cursor_idx(), c);
        self.col += 1;
        self.touch();
    }

    /// Insert pasted text verbatim (no auto-indent), normalising line endings.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let idx = self.cursor_idx();
        self.rope.insert(idx, &text);
        match text.rsplit_once('\n') {
            Some((head, tail)) => {
                self.line += head.matches('\n').count() + 1;
                self.col = tail.chars().count();
            }
            None => self.col += text.chars().count(),
        }
        self.touch();
    }

    pub fn newline(&mut self) {
        // Carry the current line's indentation over.
        let indent: String = self
            .line_chars(self.line)
            .take(self.col)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let idx = self.cursor_idx();
        self.rope.insert_char(idx, '\n');
        self.rope.insert(idx + 1, &indent);
        self.line += 1;
        self.col = indent.chars().count();
        self.touch();
    }

    pub fn backspace(&mut self) {
        let idx = self.cursor_idx();
        if idx == 0 {
            return;
        }
        if self.col > 0 {
            self.rope.remove(idx - 1..idx);
            self.col -= 1;
        } else {
            // Join with the previous line, dropping its whole line break.
            let prev_len = self.line_len(self.line - 1);
            let start = self.rope.line_to_char(self.line - 1) + prev_len;
            self.rope.remove(start..idx);
            self.line -= 1;
            self.col = prev_len;
        }
        self.touch();
    }

    pub fn delete(&mut self) {
        let idx = self.cursor_idx();
        if self.col < self.line_len(self.line) {
            self.rope.remove(idx..idx + 1);
        } else if self.line + 1 < self.line_count() {
            let next = self.rope.line_to_char(self.line + 1);
            self.rope.remove(idx..next);
        } else {
            return;
        }
        self.touch();
    }

    // ---- movement ----

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.line > 0 {
            self.line -= 1;
            self.col = self.line_len(self.line);
        }
        self.want_col = self.col;
    }

    pub fn right(&mut self) {
        if self.col < self.line_len(self.line) {
            self.col += 1;
        } else if self.line + 1 < self.line_count() {
            self.line += 1;
            self.col = 0;
        }
        self.want_col = self.col;
    }

    pub fn up(&mut self) {
        self.move_lines(-1);
    }

    pub fn down(&mut self) {
        self.move_lines(1);
    }

    pub fn page_up(&mut self) {
        self.move_lines(-(self.view_height.max(1) as isize));
    }

    pub fn page_down(&mut self) {
        self.move_lines(self.view_height.max(1) as isize);
    }

    fn move_lines(&mut self, delta: isize) {
        let last = self.line_count() - 1;
        self.line = self.line.saturating_add_signed(delta).min(last);
        self.col = self.want_col.min(self.line_len(self.line));
    }

    pub fn home(&mut self) {
        // Smart home: first non-blank, then column 0.
        let first = self.line_chars(self.line).take_while(|c| c.is_whitespace()).count();
        self.col = if self.col == first { 0 } else { first };
        self.want_col = self.col;
    }

    pub fn end(&mut self) {
        self.col = self.line_len(self.line);
        self.want_col = self.col;
    }

    /// Put the cursor on `line` at display column `dcol` (a mouse click), both
    /// clamped. A click inside a tab or a wide glyph lands before it.
    pub fn set_cursor_display(&mut self, line: usize, dcol: usize) {
        self.line = line.min(self.line_count() - 1);
        let mut x = 0;
        self.col = self
            .line_chars(self.line)
            .take_while(|&c| {
                x += char_width(c);
                x <= dcol
            })
            .count();
        self.want_col = self.col;
    }

    /// Scroll the view by `delta` lines (the mouse wheel), keeping the page full and
    /// taking the cursor along so the renderer does not scroll back to it.
    pub fn scroll_by(&mut self, delta: isize) {
        let h = self.view_height.max(1);
        let last = self.line_count() - 1;
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.line_count().saturating_sub(h));
        let line = self.line.clamp(self.scroll, (self.scroll + h - 1).min(last));
        if line != self.line {
            self.line = line;
            self.col = self.want_col.min(self.line_len(line));
        }
    }
}

fn char_width(c: char) -> usize {
    if c == '\t' { TAB_WIDTH } else { c.width().unwrap_or(0) }
}

/// Render `line` for a viewport that starts at display column `hscroll` and is
/// `width` cells wide. Tabs are expanded.
pub fn visible_slice(chars: impl Iterator<Item = char>, hscroll: usize, width: usize) -> String {
    let mut out = String::new();
    let mut x = 0;
    for c in chars {
        let w = char_width(c);
        if x < hscroll {
            x += w;
            continue;
        }
        if x - hscroll + w > width {
            break;
        }
        if c == '\t' {
            out.push_str("    ");
        } else {
            out.push(c);
        }
        x += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        Buffer {
            rope: Rope::from_str(s),
            ..Buffer::default()
        }
    }

    #[test]
    fn edit_and_join_lines() {
        let mut b = buf("ab\ncd");
        b.down();
        b.backspace();
        assert_eq!(b.rope.to_string(), "abcd");
        assert_eq!((b.line, b.col), (0, 2));
        b.newline();
        assert_eq!(b.rope.to_string(), "ab\ncd");
        b.delete();
        assert_eq!(b.rope.to_string(), "ab\nd");
    }

    #[test]
    fn crlf_lines_and_vertical_memory() {
        let mut b = buf("long line\r\nx\r\nlong line");
        assert_eq!(b.line_len(0), 9);
        b.end();
        b.down();
        assert_eq!(b.col, 1);
        b.down();
        assert_eq!(b.col, 9);
    }

    #[test]
    fn autoindent() {
        let mut b = buf("  x");
        b.end();
        b.newline();
        assert_eq!((b.line, b.col), (1, 2));
    }

    #[test]
    fn click_places_cursor_by_display_column() {
        let mut b = buf("a\tb\nxy");
        b.set_cursor_display(0, 3); // inside the tab
        assert_eq!((b.line, b.col), (0, 1));
        b.set_cursor_display(0, 5);
        assert_eq!(b.col, 2);
        b.set_cursor_display(9, 40); // below the text, past the end
        assert_eq!((b.line, b.col), (1, 2));
    }

    #[test]
    fn wheel_scroll_takes_the_cursor_along() {
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let mut b = buf(&text);
        b.view_height = 10;
        b.scroll_by(3);
        assert_eq!((b.scroll, b.line), (3, 3));
        b.scroll_by(100);
        assert_eq!(b.scroll, 41, "stops with the last page full");
        b.scroll_by(-100);
        assert_eq!((b.scroll, b.line), (0, 9));
    }

    #[test]
    fn slicing_expands_tabs_and_scrolls() {
        assert_eq!(visible_slice("a\tb".chars(), 0, 10), "a    b");
        assert_eq!(visible_slice("abcdef".chars(), 2, 3), "cde");
    }
}
