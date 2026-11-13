//! `novacode auth status` and `novacode doctor` (L2.25, L9.10).

use futures_util::future::join_all;
use novacode_ai::connect::Providers;
use novacode_base::settings::Settings;
use novacode_tools::sandbox::{Mode, Sandbox};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `5h 12m`, `3m`, `2d 4h`.
fn span(ms: u64) -> String {
    let m = ms / 60_000;
    match (m / 1440, (m / 60) % 24, m % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Every login and key novacode can use, read locally (no network, nothing refreshed).
pub fn auth_status(p: &Providers) -> ExitCode {
    let now = now_ms();
    let logins = p.auth.logins();
    let mut rows: Vec<(String, String, String)> = logins
        .iter()
        .map(|(prov, account, c)| {
            let mut state = if c.expires_ms > now {
                format!("token valid for {}", span(c.expires_ms - now))
            } else if c.refresh.is_empty() {
                "expired, log in again".to_string()
            } else {
                "token expired, refreshes on next use".to_string()
            };
            if let Some(pr) = &c.project {
                state.push_str(&format!(", project {pr}"));
            }
            (prov.clone(), format!("login `{account}`"), state)
        })
        .collect();
    for pr in p.profiles.values().filter(|pr| !pr.oauth()) {
        if let Some(src) = p.key_source(pr) {
            let kind = if pr.key_optional && pr.api_key.is_none() {
                "local"
            } else {
                "API key"
            };
            rows.push((pr.id.clone(), kind.into(), src));
        }
    }
    // Local servers that need no key are listed but don't count as a login or key.
    let none = rows.iter().all(|r| r.1 == "local");
    let w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    for (prov, kind, state) in rows {
        println!("{prov:w$}  {kind:w2$}  {state}");
    }
    if none {
        eprintln!(
            "No logins or keys. Run `novacode login <provider>` (claude, codex, antigravity, copilot, or any API-key provider) or set its API key env var."
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// One row of `novacode accounts`.
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct AccountRow {
    pub provider: String,
    pub account: String,
    /// Picked with `/account`: new sessions start on it.
    pub selected: bool,
    /// Epoch ms when a saved rate limit or used-up quota ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exhausted_until_ms: Option<u64>,
}

/// Subscription accounts per provider, in the order a new session tries them.
pub fn account_rows(auth: &novacode_ai::auth::AuthStore, only: Option<&str>) -> Vec<AccountRow> {
    let mut providers: Vec<String> = auth.logins().into_iter().map(|(p, ..)| p).collect();
    providers.dedup(); // logins() comes sorted by provider
    providers
        .into_iter()
        .filter(|p| only.is_none_or(|o| o == p))
        .flat_map(|p| {
            let picked = auth.selected_account(&p);
            auth.accounts(&p).into_iter().map(move |a| AccountRow {
                selected: picked.as_ref() == Some(&a),
                exhausted_until_ms: auth.exhausted_until(&p, &a),
                provider: p.clone(),
                account: a,
            })
        })
        .collect()
}

/// `novacode accounts [provider] [--json]`: read locally, nothing refreshed.
pub fn accounts(auth: &novacode_ai::auth::AuthStore, only: Option<&str>, json: bool) -> ExitCode {
    let rows = account_rows(auth, only);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    if rows.is_empty() {
        let p = only.unwrap_or("<provider>");
        eprintln!(
            "No subscription accounts. Add one with `novacode login {p} --account <name>` or `/login {p} --account <name>`."
        );
        return ExitCode::FAILURE;
    }
    let now = now_ms();
    let w = rows.iter().map(|r| r.account.len()).max().unwrap_or(0);
    let mut last = "";
    for r in &rows {
        if r.provider != last {
            println!("{}", r.provider);
            last = &r.provider;
        }
        let mut notes = Vec::new();
        if r.selected {
            notes.push("selected".to_string());
        }
        if let Some(t) = r.exhausted_until_ms {
            notes.push(format!("used up, free again in {}", span(t.saturating_sub(now))));
        }
        println!("  {:w$}  {}", r.account, notes.join(", "));
    }
    ExitCode::SUCCESS
}

/// Check every connected provider (a model-list call: free, but it proves the
/// credential and the endpoint), plus settings and key-binding problems.
pub async fn doctor(
    p: &Arc<Providers>,
    settings: &Settings,
    sandbox: &Sandbox,
    security: novacode_core::policy::Security,
    key_problems: Vec<String>,
    shadowed: Vec<String>,
) -> ExitCode {
    let mut failed = false;
    println!("providers");
    let connected: Vec<_> = p.profiles.values().filter(|pr| p.connected(pr)).collect();
    let checks = join_all(connected.iter().map(|pr| async move {
        let t = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(30), async {
            match p.adapter(&pr.id, None).await {
                Ok((a, _)) => a.list_models().await,
                Err(e) => Err(e.to_string()),
            }
        })
        .await
        .unwrap_or_else(|_| Err("no answer in 30s".into()));
        (pr, r, t.elapsed())
    }))
    .await;
    let w = connected.iter().map(|pr| pr.id.len()).max().unwrap_or(0);
    for (pr, r, took) in checks {
        let id = &pr.id;
        match r {
            Ok(ids) => println!("  ✓ {id:w$}  {} models ({:.1}s)", ids.len(), took.as_secs_f64()),
            // A local server that isn't running isn't a fault.
            Err(_) if pr.key_optional => println!("  - {id:w$}  local server not running"),
            Err(e) => {
                failed = true;
                println!("  ✗ {id:w$}  {}", e.lines().next().unwrap_or_default());
            }
        }
    }
    if connected.is_empty() {
        failed = true;
        println!("  ✗ nothing connected: run `novacode login <provider>` or set an API key");
    }
    let mut diags = settings.diagnostics();
    for d in settings.check() {
        if !diags.contains(&d) {
            diags.push(d);
        }
    }
    println!("settings");
    if diags.is_empty() {
        println!("  ✓ no problems");
    }
    for d in diags {
        failed = true;
        let file = d.file.map(|f| format!("{}: ", f.display())).unwrap_or_default();
        println!("  ✗ {file}{}", d.message);
    }
    println!("keys");
    if key_problems.is_empty() {
        println!("  ✓ no clashes");
    }
    for k in key_problems {
        failed = true;
        println!("  ✗ {k}");
    }
    // Same name in two folders: the earlier one loads (L7.12). Not a failure.
    println!("skills, prompts, agents");
    if shadowed.is_empty() {
        println!("  ✓ nothing shadowed or disabled");
    }
    for s in shadowed {
        println!("  · {s}");
    }
    println!("security");
    println!("  {}", security_line(security));
    println!("sandbox");
    let mode = sandbox.mode();
    match sandbox_line(mode, mode != Mode::Off && sandbox.available(), sandbox.symlink()) {
        (true, line) if sandbox.light() => println!("  ✓ {line}, credentials only (the lenient level)"),
        (true, line) => println!("  ✓ {line}"),
        (false, line) => {
            failed |= mode == Mode::On;
            println!("  {} {line}", if mode == Mode::On { "✗" } else { "-" });
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// The doctor's line under `security` (L6.9). Not a failure either way.
fn security_line(s: novacode_core::policy::Security) -> &'static str {
    match s {
        novacode_core::policy::Security::Lenient => {
            "- lenient (default): no command judge, and in auto mode only catastrophic commands, writes into credentials and ~/.novacode, and your rules are stopped. Commands can read your secrets and send them out. Set \"permissions\": {\"security\": \"hardened\"} for the judge and the full sandbox."
        }
        novacode_core::policy::Security::Hardened => {
            "✓ hardened: command judge, sandbox (where available), asks for shared and config changes"
        }
    }
}

/// The doctor's sandbox line (L6.5), under the `sandbox` heading: (fine, text).
/// `symlink`: a protected path that is a symlink, which makes it unavailable.
fn sandbox_line(mode: Mode, works: bool, symlink: Option<&Path>) -> (bool, String) {
    let why = match symlink {
        Some(p) => format!("{} is a symlink", p.display()),
        None if cfg!(windows) => "no sandbox on Windows".into(),
        None => "bwrap not found".into(),
    };
    match (mode, works) {
        (Mode::Off, _) => (false, "off (`sandbox.mode`, or the lenient level)".into()),
        (_, true) => (true, "on (bubblewrap)".into()),
        (Mode::Auto, false) => (false, format!("unavailable ({why}), commands run unsandboxed")),
        (Mode::On, false) => (
            false,
            format!("unavailable ({why}), commands are refused (`sandbox.mode` is on)"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use novacode_tools::sandbox::Mode;
    use std::process::ExitCode;

    #[test]
    fn auth_status_fails_with_only_keyless_local_servers() {
        let load = || {
            novacode_ai::connect::Providers::load(
                std::path::Path::new("/nonexistent"),
                &novacode_base::settings::Settings::default(),
            )
        };
        let mut p = load();
        p.profiles.retain(|_, pr| pr.key_optional && pr.api_key.is_none());
        assert!(!p.profiles.is_empty(), "bundled local servers");
        assert_eq!(super::auth_status(&p), ExitCode::FAILURE);
        let mut p = load();
        p.cli_key = Some(("openrouter".into(), "sk-test".into()));
        assert_eq!(super::auth_status(&p), ExitCode::SUCCESS);
    }

    /// `novacode accounts`: rotation order per provider, the pick and used-up accounts marked.
    #[tokio::test]
    async fn account_rows_follow_rotation_order() {
        use novacode_ai::auth::{AuthStore, OAuthCredential};
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthStore::new(dir.path().join("auth.json"));
        let cred = || OAuthCredential {
            access: "a".into(),
            refresh: "r".into(),
            expires_ms: u64::MAX / 2,
            project: None,
        };
        for (p, a) in [
            ("claude", "default"),
            ("claude", "work"),
            ("claude", "zed"),
            ("codex", "default"),
        ] {
            auth.put(p, a, cred()).await.unwrap();
        }
        auth.select("claude", "zed");
        auth.mark_exhausted("claude", "work", super::now_ms() + 60_000);
        let rows = super::account_rows(&auth, None);
        let names: Vec<_> = rows.iter().map(|r| format!("{}/{}", r.provider, r.account)).collect();
        assert_eq!(names, ["claude/zed", "claude/default", "claude/work", "codex/default"]);
        assert!(rows[0].selected && !rows[1].selected);
        assert!(rows[2].exhausted_until_ms.is_some() && rows[0].exhausted_until_ms.is_none());
        assert_eq!(super::account_rows(&auth, Some("codex")).len(), 1);
        assert!(super::account_rows(&auth, Some("copilot")).is_empty());
    }

    #[test]
    fn sandbox_lines() {
        use std::path::Path;
        let line = |m, w, s: Option<&str>| super::sandbox_line(m, w, s.map(Path::new));
        assert_eq!(line(Mode::Auto, true, None), (true, "on (bubblewrap)".into()));
        assert_eq!(
            line(Mode::Off, true, None),
            (false, "off (`sandbox.mode`, or the lenient level)".into())
        );
        assert_eq!(
            line(Mode::Auto, false, None).1,
            if cfg!(windows) {
                "unavailable (no sandbox on Windows), commands run unsandboxed"
            } else {
                "unavailable (bwrap not found), commands run unsandboxed"
            }
        );
        assert!(line(Mode::On, false, None).1.contains("refused"));
        assert_eq!(
            line(Mode::Auto, false, Some("/p/.git/hooks")).1,
            "unavailable (/p/.git/hooks is a symlink), commands run unsandboxed"
        );
        assert!(
            line(Mode::On, false, Some("/p/.mcp.json"))
                .1
                .starts_with("unavailable (/p/.mcp.json is a symlink), commands are refused")
        );
    }

    #[test]
    fn spans() {
        assert_eq!(super::span(3 * 60_000), "3m");
        assert_eq!(super::span((5 * 60 + 12) * 60_000), "5h 12m");
        assert_eq!(super::span((52 * 60) * 60_000), "2d 4h");
    }
}
