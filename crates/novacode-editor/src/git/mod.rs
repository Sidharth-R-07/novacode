//! Read-only git views. Every command goes through `novacode_base::git`, the
//! project's one hardened way to run git (no planted-config code execution).

use std::path::{Path, PathBuf};

const MAX_DIFF_LINES: usize = 3000;

pub struct Entry {
    /// Two-letter porcelain status, e.g. " M", "A ", "??".
    pub status: String,
    /// Path relative to the repository root.
    pub path: String,
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = novacode_base::git::command_in(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn toplevel(dir: &Path) -> Result<PathBuf, String> {
    run(dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim()))
}

pub fn status(top: &Path) -> Result<Vec<Entry>, String> {
    run(top, &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"]).map(|s| parse_status(&s))
}

fn parse_status(raw: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut parts = raw.split('\0');
    while let Some(e) = parts.next() {
        if e.len() < 4 {
            continue;
        }
        let status = &e[..2];
        // Renames/copies are followed by an extra token holding the old path.
        if status.contains(['R', 'C']) {
            parts.next();
        }
        out.push(Entry {
            status: status.to_string(),
            path: e[3..].to_string(),
        });
    }
    out
}

pub fn diff(top: &Path, entry: &Entry) -> Vec<String> {
    if entry.status == "??" {
        return vec!["(untracked file: not yet known to git)".into()];
    }
    let text = run(top, &["diff", "--ignore-submodules=all", "HEAD", "--", &entry.path])
        // A fresh repository has no HEAD yet.
        .or_else(|_| run(top, &["diff", "--ignore-submodules=all", "--", &entry.path]))
        .unwrap_or_else(|e| format!("git diff failed: {e}"));
    if text.trim().is_empty() {
        return vec!["(no textual changes)".into()];
    }
    text.lines().take(MAX_DIFF_LINES).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_z_with_rename() {
        let raw = " M src/a.rs\0R  new.rs\0old.rs\0?? x y.txt\0";
        let e = parse_status(raw);
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].status.as_str(), e[0].path.as_str()), (" M", "src/a.rs"));
        assert_eq!(e[1].path, "new.rs");
        assert_eq!(e[2].path, "x y.txt");
    }
}
