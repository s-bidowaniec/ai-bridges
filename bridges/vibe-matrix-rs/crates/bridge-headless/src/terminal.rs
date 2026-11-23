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
    error.starts_with("can't find session:")
        || error.starts_with("no server running on ")
        || (error.starts_with("error connecting to ")
            && error.contains("(No such file or directory)"))
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
    Ok(())
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

#[derive(Clone)]
pub struct Follow {
    pub pane: String,
    pub baseline: String,
}

fn screen_delta(previous: &str, current: &str) -> String {
    if let Some(suffix) = current.strip_prefix(previous) {
        return clean_tui(suffix);
    }
    let old: Vec<_> = previous.lines().collect();
    let new: Vec<_> = current.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let overlap = (1..=old.len().min(new.len()))
        .rev()
        .find(|&count| old[old.len() - count..] == new[..count])
        .unwrap_or(0);
    let changed = &new[prefix.max(overlap)..];
    clean_tui(&changed[changed.len().saturating_sub(80)..].join("\n"))
}

pub async fn relay(client: MatrixClient, mut follow: watch::Receiver<Option<Follow>>) {
    let mut updates = tokio::time::interval(Duration::from_secs(2));
    updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut target = None;
    loop {
        tokio::select! {
            biased;
            change = follow.changed() => {
                if change.is_err() {
                    break;
                }
                target = follow.borrow_and_update().clone();
            }
            _ = updates.tick() => {
                let Some(active) = &mut target else { continue };
                let captured = match capture(&active.pane).await {
                    Ok(screen) => screen,
                    Err(error) => {
                        tracing::warn!(%error, "odłączam widok Matrix po utracie panelu");
                        target = None;
                        let _ = client.send_text("Panel tmux przestał być dostępny. Użyj /attach, aby podłączyć sesję.").await;
                        continue;
                    }
                };
                if follow.has_changed().unwrap_or(true) || captured == active.baseline {
                    continue;
                }
                let delta = screen_delta(&active.baseline, &captured);
                if delta.is_empty() {
                    active.baseline = captured;
                } else {
                    let message = if delta.len() > 3800 {
                        format!("{}…\n\n[Pozostały tekst w terminalu]", safe_truncate(&delta, 3800))
                    } else {
                        delta
                    };
                    match client.send_text(&message).await {
                        Ok(_) => active.baseline = captured,
                        Err(error) => tracing::warn!(%error, "nie udało się opublikować zmian terminala"),
                    }
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
}
