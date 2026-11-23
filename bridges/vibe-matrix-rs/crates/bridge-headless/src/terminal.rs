use anyhow::{bail, ensure, Context, Result};
use bridge_common::{clean_tui, safe_truncate, Config};
use bridge_core::MatrixClient;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;

pub fn executable(config: &Config, agent: &str) -> Option<String> {
    Some(match agent {
        "vibe" => config.vibe_cmd.clone(),
        "codex" => std::env::var("CODEX_CMD").unwrap_or_else(|_| "codex".to_owned()),
        "claude" => std::env::var("CLAUDE_CMD").unwrap_or_else(|_| "claude".to_owned()),
        "agy" => std::env::var("AGY_CMD").unwrap_or_else(|_| "agy".to_owned()),
        _ => return None,
    })
}

pub fn agent_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "antigravity" => "agy".to_owned(),
        other => other.to_owned(),
    }
}

pub fn session_name(config: &Config, agent: &str) -> String {
    std::env::var("TMUX_SESSION")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("{}-{agent}", config.tmux_prefix))
}

fn command() -> Command {
    let mut command = Command::new("tmux");
    command.arg("-u").env_clear().kill_on_drop(true);
    for key in [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "TMUX_TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("LC_MESSAGES", "C");
    command
}

async fn run(args: &[&str]) -> Result<Output> {
    command()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("uruchomienie tmux")
}

fn check(action: &str, output: Output) -> Result<String> {
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("tmux {action}: {}", safe_truncate(error.trim(), 500));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn missing_session(output: &Output) -> bool {
    let error = String::from_utf8_lossy(&output.stderr);
    let trimmed = error.trim();
    trimmed == "no current target"
        || trimmed.starts_with("can't find session:")
        || trimmed.starts_with("no server running on ")
        || (trimmed.starts_with("error connecting to ")
            && trimmed.contains("(No such file or directory)"))
}

pub async fn session_exists(name: &str) -> Result<bool> {
    let output = run(&["has-session", "-t", &format!("={name}")]).await?;
    if missing_session(&output) {
        return Ok(false);
    }
    check("has-session", output)?;
    Ok(true)
}

pub fn named_session(config: &Config, name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "nazwa terminala musi zawierać 1–64 liter ASCII, cyfr, myślników lub podkreśleń"
    );
    Ok(format!("{}-{name}", config.tmux_prefix))
}

pub async fn create(config: &Config, name: &str, agent: &str) -> Result<()> {
    let executable = executable(config, agent).context("nieznany agent")?;
    let session_id = check(
        "new-session",
        run(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id}",
            "-s",
            name,
            "-c",
            &config.vibe_workdir,
            "--",
            "env",
            "--",
            &executable,
        ])
        .await?,
    )?;
    check(
        "set-option",
        run(&[
            "set-option",
            "-t",
            session_id.trim(),
            "destroy-unattached",
            "off",
        ])
        .await?,
    )?;
    if let Err(error) = open_window(name).await {
        tracing::warn!(session = %name, %error, "nie udało się automatycznie otworzyć okna terminala");
    }
    Ok(())
}

fn find_in_path(bin: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return candidate.to_str().map(str::to_owned);
            }
        }
    }
    None
}

fn terminal_command_from(
    configured: Option<&str>,
    has_display: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    if let Some(raw) = configured {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "off" | "none" | "0" | "false"
            )
        {
            return None;
        }
        if !trimmed.eq_ignore_ascii_case("auto") {
            let parts: Vec<String> = trimmed.split_whitespace().map(str::to_owned).collect();
            return (!parts.is_empty()).then_some(parts);
        }
    }
    if !has_display {
        return None;
    }
    for (bin, prefix) in [
        ("konsole", &["-e"][..]),
        ("gnome-terminal", &["--"][..]),
        ("kgx", &["-e"][..]),
        ("ptyxis", &["--"][..]),
        ("alacritty", &["-e"][..]),
        ("kitty", &[][..]),
        ("wezterm", &["start", "--"][..]),
        ("foot", &[][..]),
        ("xterm", &["-e"][..]),
    ] {
        if let Some(path) = lookup(bin) {
            let mut parts = vec![path];
            parts.extend(prefix.iter().map(|arg| (*arg).to_owned()));
            return Some(parts);
        }
    }
    None
}

/// Opens a desktop terminal window attached to the given tmux session without
/// leaking Matrix secrets into the GUI process. Returns `Ok(false)` on headless
/// hosts or when `TERMINAL_CMD` is disabled.
pub async fn open_window(name: &str) -> Result<bool> {
    let configured = std::env::var("TERMINAL_CMD").ok();
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let Some(mut parts) = terminal_command_from(configured.as_deref(), has_display, find_in_path)
    else {
        return Ok(false);
    };
    let tmux_bin = find_in_path("tmux").unwrap_or_else(|| "tmux".to_owned());
    parts.extend([
        tmux_bin,
        "-u".to_owned(),
        "attach-session".to_owned(),
        "-t".to_owned(),
        format!("={name}"),
    ]);

    if let (Some(systemd_run), Some(_)) = (
        find_in_path("systemd-run"),
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
    ) {
        let mut cmd = Command::new(systemd_run);
        cmd.env_clear();
        for key in ["HOME", "USER", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.args(["--user", "--collect", "--quiet"]);
        if let Ok(tmpdir) = std::env::var("TMUX_TMPDIR") {
            if !tmpdir.is_empty() {
                cmd.arg(format!("--property=Environment=TMUX_TMPDIR={tmpdir}"));
            }
        }
        cmd.arg("--").args(&parts);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Ok(output) = cmd.output().await {
            if output.status.success() {
                tracing::info!(session = %name, term = %parts[0], "otwarto okno terminala przez systemd-run");
                return Ok(true);
            }
            tracing::debug!(
                session = %name,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "systemd-run nie powiódł się, próbuję bezpośrednio"
            );
        }
    }

    let mut cmd = Command::new(&parts[0]);
    cmd.args(&parts[1..]).env_clear();
    for key in [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "TMUX_TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("uruchomienie emulatora terminala `{}`", parts[0]))?;
    tracing::info!(session = %name, term = %parts[0], "otwarto okno terminala (tmux attach)");
    Ok(true)
}

pub async fn list(config: &Config, agent: &str) -> Result<Vec<String>> {
    let output = run(&["list-sessions", "-F", "#{session_name}"]).await?;
    if missing_session(&output) {
        return Ok(Vec::new());
    }
    let prefix = format!("{}-", config.tmux_prefix);
    let default = session_name(config, agent);
    Ok(check("list-sessions", output)?
        .lines()
        .filter(|name| name.starts_with(&prefix) || *name == default)
        .map(str::to_owned)
        .collect())
}

pub async fn pane(name: &str) -> Result<String> {
    let panes = check(
        "list-panes",
        run(&[
            "list-panes",
            "-t",
            &format!("={name}:"),
            "-F",
            "#{pane_id} #{pane_active} #{pane_dead}",
        ])
        .await?,
    )?;
    panes
        .lines()
        .find_map(|line| {
            let (pane, state) = line.split_once(' ')?;
            (state == "1 0" && pane.starts_with('%')).then(|| pane.to_owned())
        })
        .context("w sesji tmux nie ma aktywnego, działającego panelu")
}

pub async fn capture(pane: &str) -> Result<String> {
    check(
        "capture-pane",
        run(&["capture-pane", "-p", "-t", pane, "-S", "-300"]).await?,
    )
}

static NEXT_BUFFER: AtomicU64 = AtomicU64::new(0);

pub async fn send_text(pane: &str, text: &str) -> Result<()> {
    ensure!(
        text.chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t')),
        "wiadomość zawiera znaki sterujące; skróty wysyłaj przez /key"
    );
    let sequence = NEXT_BUFFER.fetch_add(1, Ordering::Relaxed);
    let buffer = format!("matrix-{}-{sequence}", std::process::id());
    let mut child = command()
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("tmux load-buffer")?;
    let mut stdin = child
        .stdin
        .take()
        .context("brak wejścia tmux load-buffer")?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);
    check("load-buffer", child.wait_with_output().await?)?;
    check(
        "paste-buffer",
        run(&[
            "paste-buffer",
            "-d",
            "-p",
            "-b",
            &buffer,
            "-t",
            pane,
            ";",
            "send-keys",
            "-t",
            pane,
            "Enter",
        ])
        .await?,
    )?;
    Ok(())
}

pub async fn send_key(pane: &str, key: &str) -> Result<()> {
    ensure!(
        matches!(
            key,
            "Enter" | "Escape" | "Up" | "Down" | "Left" | "Right" | "Tab" | "C-c"
        ),
        "dozwolone skróty: Enter, Escape, Up, Down, Left, Right, Tab, C-c"
    );
    check("send-keys", run(&["send-keys", "-t", pane, key]).await?)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedMenu {
    pub count: usize,
    pub selected: usize,
}

fn is_cursor_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("❯ ")
        || trimmed.starts_with("› ")
        || trimmed.starts_with("▸ ")
        || trimmed.starts_with("▶ ")
        || trimmed.starts_with("● ")
        || trimmed.starts_with("◉ ")
}

fn is_unselected_option(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("○ ") || trimmed.starts_with("◯ ") || trimmed.starts_with("[ ] ") {
        return true;
    }
    // Numbered menu line like "2. Always allow" or "2) Always allow"
    let bytes = trimmed.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_digit()
        && matches!(bytes[1], b'.' | b')')
        && bytes[2] == b' '
}

/// Detects an active TUI selection menu near the bottom of the cleaned screen.
pub fn detect_menu(cleaned: &str) -> Option<DetectedMenu> {
    let lines: Vec<&str> = cleaned.lines().collect();
    let tail_start = lines.len().saturating_sub(25);
    let tail = &lines[tail_start..];
    let cursor_idx = tail.iter().rposition(|line| is_cursor_marker(line))?;

    let mut start = cursor_idx;
    while start > 0 && (is_cursor_marker(tail[start - 1]) || is_unselected_option(tail[start - 1]))
    {
        start -= 1;
    }
    let mut end = cursor_idx;
    while end + 1 < tail.len()
        && (is_cursor_marker(tail[end + 1]) || is_unselected_option(tail[end + 1]))
    {
        end += 1;
    }
    let count = end - start + 1;
    if count < 2 {
        return None;
    }
    Some(DetectedMenu {
        count,
        selected: cursor_idx - start + 1,
    })
}

fn navigation_keys(current: usize, target: usize) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if target > current {
        keys.extend(std::iter::repeat_n("Down", target - current));
    } else if target < current {
        keys.extend(std::iter::repeat_n("Up", current - target));
    }
    keys.push("Enter");
    keys
}

/// Selects a 1-based option in an active TUI menu by moving from the currently
/// highlighted item (or item 1 if no cursor is detected) and pressing Enter.
pub async fn pick_option(pane: &str, target: usize) -> Result<String> {
    ensure!(
        (1..=9).contains(&target),
        "wybór opcji obsługuje numery 1–9"
    );
    let captured = capture(pane).await?;
    let cleaned = clean_tui(&captured);
    let current = match detect_menu(&cleaned) {
        Some(menu) => {
            ensure!(
                target <= menu.count,
                "menu ma tylko {} opcji (wybrano {})",
                menu.count,
                target
            );
            menu.selected
        }
        None => 1,
    };
    for key in navigation_keys(current, target) {
        send_key(pane, key).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    Ok(format!("Wybrano opcję {target}."))
}

fn menu_hint(cleaned: &str) -> Option<String> {
    if let Some(menu) = detect_menu(cleaned) {
        let choices = (1..=menu.count.min(9))
            .map(|n| format!("/{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        return Some(format!("Wybór: {choices} · /ok (Enter) · /esc (Anuluj)"));
    }
    let last = cleaned.lines().rev().take(4).collect::<Vec<_>>().join(" ");
    if last.contains("[y/N]")
        || last.contains("[Y/n]")
        || last.contains("(y/n)")
        || last.contains("(Y/n)")
    {
        return Some("Odpowiedz: y / n · /ok (Enter) · /esc (Anuluj)".to_owned());
    }
    None
}

#[derive(Clone)]
pub struct Follow {
    pub pane: String,
    pub baseline: String,
}

fn screen_delta(previous_clean: &str, current_clean: &str) -> String {
    if previous_clean == current_clean {
        return String::new();
    }
    if let Some(suffix) = current_clean.strip_prefix(previous_clean) {
        return suffix.trim().to_owned();
    }
    let old: Vec<_> = previous_clean.lines().collect();
    let new: Vec<_> = current_clean.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let overlap = (1..=old.len().min(new.len()))
        .rev()
        .find(|&count| old[old.len() - count..] == new[..count])
        .unwrap_or(0);
    let changed = &new[prefix.max(overlap)..];
    changed[changed.len().saturating_sub(80)..]
        .join("\n")
        .trim()
        .to_owned()
}

pub async fn relay(client: MatrixClient, mut follow: watch::Receiver<Option<Follow>>) {
    let mut updates = tokio::time::interval(Duration::from_millis(1500));
    updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut target: Option<Follow> = None;
    let mut pending_clean: Option<String> = None;
    let mut dirty_since: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            biased;
            change = follow.changed() => {
                if change.is_err() {
                    break;
                }
                target = follow.borrow_and_update().clone().map(|mut f| {
                    f.baseline = clean_tui(&f.baseline);
                    f
                });
                pending_clean = None;
                dirty_since = None;
            }
            _ = updates.tick() => {
                let Some(active) = &mut target else { continue };
                let captured = match capture(&active.pane).await {
                    Ok(screen) => screen,
                    Err(error) => {
                        tracing::warn!(%error, "odłączam widok Matrix po utracie panelu");
                        target = None;
                        pending_clean = None;
                        dirty_since = None;
                        let _ = client.send_text("Panel tmux przestał być dostępny. Użyj /attach, aby podłączyć sesję.").await;
                        continue;
                    }
                };
                if follow.has_changed().unwrap_or(true) {
                    continue;
                }
                let cleaned = clean_tui(&captured);
                if cleaned == active.baseline {
                    pending_clean = None;
                    dirty_since = None;
                    continue;
                }
                // Wait one tick for streaming output to settle (max 4.5s) so Matrix gets
                // complete blocks instead of fragmented lines.
                let now = std::time::Instant::now();
                let first_dirty = *dirty_since.get_or_insert(now);
                let settled = pending_clean.as_deref() == Some(cleaned.as_str());
                if !settled && now.duration_since(first_dirty) < Duration::from_millis(4500) {
                    pending_clean = Some(cleaned);
                    continue;
                }
                let delta = screen_delta(&active.baseline, &cleaned);
                if delta.is_empty() {
                    active.baseline = cleaned;
                    pending_clean = None;
                    dirty_since = None;
                    continue;
                }
                let mut body = if delta.len() > 3700 {
                    format!("{}…\n\n[Pozostały tekst w terminalu]", safe_truncate(&delta, 3700))
                } else {
                    delta
                };
                if let Some(hint) = menu_hint(&cleaned) {
                    body.push_str("\n\n");
                    body.push_str(&hint);
                }
                match client.send_text(&body).await {
                    Ok(_) => {
                        active.baseline = cleaned;
                        pending_clean = None;
                        dirty_since = None;
                    }
                    Err(error) => tracing::warn!(%error, "nie udało się opublikować zmian terminala"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_excludes_existing_lines_and_handles_scrolled_history() {
        assert_eq!(screen_delta("one\ntwo\n", "one\ntwo\nthree\n"), "three");
        assert_eq!(screen_delta("one\ntwo\n", "two\nthree\n"), "three");
        assert_eq!(screen_delta("one\ntwo", "one\nupdated"), "updated");
        assert_eq!(screen_delta("one\ntwo", "two"), "");
        assert_eq!(agent_name("Antigravity"), "agy");
    }

    #[test]
    fn missing_session_recognizes_empty_tmux_daemon() {
        use std::os::unix::process::ExitStatusExt;
        let out = |err: &str| Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: err.as_bytes().to_vec(),
        };
        assert!(missing_session(&out("no current target\n")));
        assert!(missing_session(&out("can't find session: vibe-matrix-rs-a\n")));
        assert!(missing_session(&out("no server running on /tmp/tmux-1000/default\n")));
        assert!(!missing_session(&out("permission denied\n")));
    }

    #[test]
    fn terminal_command_respects_disabled_custom_and_auto_modes() {
        let lookup = |bin: &str| (bin == "konsole").then(|| "/usr/bin/konsole".to_owned());
        assert_eq!(terminal_command_from(Some("off"), true, lookup), None);
        assert_eq!(terminal_command_from(Some(""), true, lookup), None);
        assert_eq!(terminal_command_from(None, false, lookup), None);
        assert_eq!(
            terminal_command_from(None, true, lookup),
            Some(vec!["/usr/bin/konsole".to_owned(), "-e".to_owned()])
        );
        assert_eq!(
            terminal_command_from(Some("kitty --single-instance"), false, lookup),
            Some(vec!["kitty".to_owned(), "--single-instance".to_owned()])
        );
    }

    #[test]
    fn menu_detection_and_navigation_work_for_numbered_and_radio_lists() {
        let numbered = "Pozwolić?\n  1. Raz\n❯ 2. Zawsze\n  3. Nie\n";
        assert_eq!(
            detect_menu(numbered),
            Some(DetectedMenu {
                count: 3,
                selected: 2
            })
        );
        assert_eq!(navigation_keys(2, 3), vec!["Down", "Enter"]);
        assert_eq!(navigation_keys(2, 1), vec!["Up", "Enter"]);
        assert_eq!(navigation_keys(2, 2), vec!["Enter"]);
        assert_eq!(
            menu_hint(numbered).as_deref(),
            Some("Wybór: /1 /2 /3 · /ok (Enter) · /esc (Anuluj)")
        );

        let radio = "Tryb:\n● Szybki\n○ Dokładny\n";
        assert_eq!(
            detect_menu(radio),
            Some(DetectedMenu {
                count: 2,
                selected: 1
            })
        );
    }
}
