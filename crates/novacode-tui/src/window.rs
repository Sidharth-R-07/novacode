//! `/new-window`: open another terminal window running novacode's panel layout
//! (`novacode edit <dir>`) in the same folder. Also chat tabs: inside the
//! panel layout's AI panel, `/new` asks the layout for a new chat tab.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// The escape sequence (a private OSC) by which a chat hosted in the panel
/// layout asks it for a new chat tab. The layout reads it off the pty.
pub const NEW_TAB_OSC: &[u8] = b"\x1b]7771;novacode;new-tab\x07";

static TABBED: AtomicBool = AtomicBool::new(false);

/// Say that this chat runs in the panel layout's AI panel, which hosts chats
/// as tabs: `/new` then opens a tab instead of replacing the session.
pub fn set_tabbed(on: bool) {
    TABBED.store(on, Ordering::Relaxed);
}

pub fn tabbed() -> bool {
    TABBED.load(Ordering::Relaxed)
}

/// Ask the hosting panel layout for a new chat tab.
pub fn request_tab() -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(NEW_TAB_OSC)?;
    out.flush()
}

/// The program and arguments that open a new terminal window running `cmd`
/// in `cwd`. `var` reads the environment, `on_path` says whether a program is
/// installed. Inside tmux it is a tmux window; else the terminal novacode runs
/// in, then `$TERMINAL`, then the first known terminal installed. `None` when
/// no terminal is found.
pub fn launch_args(
    cmd: &[String],
    cwd: &str,
    var: impl Fn(&str) -> Option<String>,
    on_path: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let with = |head: &[&str], sep: &[&str]| -> Vec<String> {
        head.iter()
            .chain(sep)
            .map(|s| (*s).to_string())
            .chain(cmd.iter().cloned())
            .collect()
    };
    if var("TMUX").is_some() {
        return Some(with(&["tmux", "new-window", "-c", cwd], &[]));
    }
    let known = |name: &str| -> Option<Vec<String>> {
        Some(match name {
            "kitty" => with(&["kitty", "--directory", cwd], &[]),
            "foot" => with(&["foot", "--working-directory", cwd], &[]),
            "alacritty" => with(&["alacritty", "--working-directory", cwd], &["-e"]),
            "wezterm" => with(&["wezterm", "start", "--cwd", cwd], &["--"]),
            "ghostty" => with(&["ghostty", &format!("--working-directory={cwd}")], &["-e"]),
            "konsole" => with(&["konsole", "--workdir", cwd], &["-e"]),
            "gnome-terminal" => with(&["gnome-terminal", &format!("--working-directory={cwd}")], &["--"]),
            "xterm" => with(&["xterm"], &["-e"]),
            _ => return None,
        })
    };
    // The terminal this window is in, by the variables it sets.
    let current = [
        ("KITTY_WINDOW_ID", "kitty"),
        ("WEZTERM_PANE", "wezterm"),
        ("ALACRITTY_WINDOW_ID", "alacritty"),
        ("GHOSTTY_RESOURCES_DIR", "ghostty"),
        ("KONSOLE_VERSION", "konsole"),
        ("GNOME_TERMINAL_SCREEN", "gnome-terminal"),
    ];
    for (v, name) in current {
        if var(v).is_some() && on_path(name) {
            return known(name);
        }
    }
    if var("TERM").is_some_and(|t| t == "foot" || t.starts_with("foot-")) && on_path("foot") {
        return known("foot");
    }
    if let Some(t) = var("TERMINAL").filter(|t| !t.trim().is_empty()) {
        let name = t.trim().to_string();
        let base = Path::new(&name)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&name)
            .to_string();
        return Some(match known(&base) {
            Some(mut a) => {
                a[0] = name;
                a
            }
            None => with(&[&name], &["-e"]),
        });
    }
    [
        "kitty",
        "foot",
        "alacritty",
        "wezterm",
        "ghostty",
        "konsole",
        "gnome-terminal",
        "xterm",
    ]
    .into_iter()
    .find(|n| on_path(n))
    .and_then(known)
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(name).is_file())
}

/// Open a new window with the panel layout on `cwd`. On failure, the error says
/// what to run by hand.
pub fn open(cwd: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_or("novacode".into(), |p| p.display().to_string());
    let cmd = vec![exe.clone(), "edit".to_string(), cwd.to_string()];
    let by_hand = format!("run in a new terminal: {exe} edit {cwd}");
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(args) = launch_args(&cmd, cwd, var, on_path) else {
        return Err(format!("no terminal found (set $TERMINAL); {by_hand}"));
    };
    let mut c = Command::new(&args[0]);
    c.args(&args[1..])
        .current_dir(cwd)
        // The new window is a top-level novacode, not one hosted in a panel.
        .env_remove("NOVA_EDIT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: Ctrl+C here or this window closing leaves it alone.
        c.process_group(0);
    }
    match c.spawn() {
        Ok(mut child) => {
            // Reap it whenever it exits (a terminal's launcher often exits at once).
            std::thread::spawn(move || child.wait());
            Ok(format!("opened a new window ({})", args[0]))
        }
        Err(e) => Err(format!("cannot start `{}`: {e}; {by_hand}", args[0])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> Vec<String> {
        vec!["/bin/nc".into(), "edit".into(), "/p".into()]
    }

    fn run(vars: &[(&str, &str)], installed: &[&str]) -> Option<String> {
        let var = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        launch_args(&cmd(), "/p", var, |n| installed.contains(&n)).map(|a| a.join(" "))
    }

    #[test]
    fn tmux_first() {
        assert_eq!(
            run(&[("TMUX", "x"), ("KITTY_WINDOW_ID", "1")], &["kitty"]).unwrap(),
            "tmux new-window -c /p /bin/nc edit /p"
        );
    }

    #[test]
    fn current_terminal() {
        assert_eq!(
            run(&[("KITTY_WINDOW_ID", "1")], &["kitty", "xterm"]).unwrap(),
            "kitty --directory /p /bin/nc edit /p"
        );
        assert_eq!(
            run(&[("ALACRITTY_WINDOW_ID", "1")], &["alacritty"]).unwrap(),
            "alacritty --working-directory /p -e /bin/nc edit /p"
        );
    }

    #[test]
    fn terminal_var_then_installed() {
        assert_eq!(
            run(&[("TERMINAL", "/usr/bin/foot")], &[]).unwrap(),
            "/usr/bin/foot --working-directory /p /bin/nc edit /p"
        );
        assert_eq!(run(&[("TERMINAL", "st")], &[]).unwrap(), "st -e /bin/nc edit /p");
        assert_eq!(
            run(&[], &["xterm", "alacritty"]).unwrap(),
            "alacritty --working-directory /p -e /bin/nc edit /p"
        );
        assert_eq!(run(&[], &[]), None);
    }
}
