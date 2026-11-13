//! File explorer: a flat list of the currently visible nodes, expanded lazily.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

const IGNORED: [&str; 3] = [".git", "target", "node_modules"];

pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct Tree {
    pub root: PathBuf,
    /// Visible nodes in display order.
    pub nodes: Vec<Node>,
    pub selected: usize,
    /// First visible row. Owned by the renderer.
    pub offset: usize,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let nodes = read_children(&root, 0);
        Self {
            root,
            nodes,
            selected: 0,
            offset: 0,
        }
    }

    /// Re-read the directories that are open, keeping what is expanded and what is
    /// selected. Returns true if the listing changed.
    pub fn refresh(&mut self) -> bool {
        let open: Vec<PathBuf> = self
            .nodes
            .iter()
            .filter(|n| n.is_dir && n.expanded)
            .map(|n| n.path.clone())
            .collect();
        let mut nodes = Vec::with_capacity(self.nodes.len());
        build(&self.root, 0, &open, &mut nodes);
        let same = nodes.len() == self.nodes.len() && nodes.iter().zip(&self.nodes).all(|(a, b)| a.path == b.path);
        if same {
            return false;
        }
        let keep = self.selected_node().map(|n| n.path.clone());
        self.nodes = nodes;
        self.selected = keep
            .and_then(|p| self.nodes.iter().position(|n| n.path == p))
            .unwrap_or_else(|| self.selected.min(self.nodes.len().saturating_sub(1)));
        true
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.nodes.get(self.selected)
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.nodes.len() {
            self.selected += 1;
        }
    }

    /// Expand the selected directory. Returns true if anything changed.
    pub fn expand(&mut self) -> bool {
        let Some(node) = self.nodes.get_mut(self.selected) else {
            return false;
        };
        if !node.is_dir || node.expanded {
            return false;
        }
        node.expanded = true;
        let children = read_children(&node.path, node.depth + 1);
        let at = self.selected + 1;
        self.nodes.splice(at..at, children);
        true
    }

    /// Collapse the selected directory, or jump to the parent of a file /
    /// collapsed directory.
    pub fn collapse_or_parent(&mut self) {
        let Some(node) = self.nodes.get(self.selected) else {
            return;
        };
        if node.is_dir && node.expanded {
            let depth = node.depth;
            let start = self.selected + 1;
            let end = self.nodes[start..]
                .iter()
                .position(|n| n.depth <= depth)
                .map_or(self.nodes.len(), |p| start + p);
            self.nodes.drain(start..end);
            self.nodes[self.selected].expanded = false;
        } else if node.depth > 0 {
            let depth = node.depth;
            if let Some(p) = self.nodes[..self.selected].iter().rposition(|n| n.depth < depth) {
                self.selected = p;
            }
        }
    }

    /// Scroll by `delta` rows (the mouse wheel) in a view `height` rows tall, taking
    /// the selection along so the renderer does not scroll back to it.
    pub fn scroll_by(&mut self, delta: isize, height: usize) {
        let h = height.max(1);
        self.offset = self
            .offset
            .saturating_add_signed(delta)
            .min(self.nodes.len().saturating_sub(h));
        let last = (self.offset + h - 1).min(self.nodes.len().saturating_sub(1));
        self.selected = self.selected.clamp(self.offset, last.max(self.offset));
    }

    /// Expand/collapse a directory (Enter). No-op for files.
    pub fn toggle(&mut self) {
        if !self.expand() && self.nodes.get(self.selected).is_some_and(|n| n.is_dir && n.expanded) {
            self.collapse_or_parent();
        }
    }
}

fn build(dir: &Path, depth: usize, open: &[PathBuf], out: &mut Vec<Node>) {
    for mut n in read_children(dir, depth) {
        let expand = n.is_dir && open.contains(&n.path);
        n.expanded = expand;
        let path = n.path.clone();
        out.push(n);
        if expand {
            build(&path, depth + 1, open, out);
        }
    }
}

/// Immediate children of `dir`, directories first, then case-insensitive name.
fn read_children(dir: &Path, depth: usize) -> Vec<Node> {
    WalkDir::new(dir)
        .min_depth(1)
        .max_depth(1)
        .sort_by(|a, b| {
            b.file_type().is_dir().cmp(&a.file_type().is_dir()).then_with(|| {
                a.file_name()
                    .to_ascii_lowercase()
                    .cmp(&b.file_name().to_ascii_lowercase())
            })
        })
        .into_iter()
        .filter_entry(|e| !IGNORED.iter().any(|i| e.file_name() == *i))
        .filter_map(Result::ok)
        .map(|e| Node {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: e.file_type().is_dir(),
            path: e.into_path(),
            depth,
            expanded: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_and_collapse() {
        let d = std::env::temp_dir().join(format!("nova-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/a.txt"), "x").unwrap();
        std::fs::write(d.join("b.txt"), "x").unwrap();

        let mut t = Tree::new(d.clone());
        assert_eq!(t.nodes.len(), 2);
        assert!(t.nodes[0].is_dir, "dirs sort first");
        t.expand();
        assert_eq!(t.nodes.len(), 3);
        t.down();
        t.collapse_or_parent();
        assert_eq!(t.selected, 0, "file collapses to its parent");
        t.collapse_or_parent();
        assert_eq!(t.nodes.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
