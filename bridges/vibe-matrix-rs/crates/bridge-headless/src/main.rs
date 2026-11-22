//! bridge-headless v5: Matrix room <-> terminal agent (vibe/codex/claude/agy) w stałej sesji tmux.
//! Bez kluczy API pośrednio — agent żyje w tmuxie, my tylko send-keys + capture-pane.
//! Komendy bota: /new [agent] /agent <n> /attach /stop /status /help; reszta (np. /model) leci do tmuxa.
use anyhow::{Context, Result};
use bridge_common::{clean_ansi, load_seen, load_str_opt, safe_truncate, save_seen, save_str, Config};
use bridge_core::{Event, MatrixClient};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
enum BotCommand {
    New(Option<String>),
    Agent(String),
    Attach,
    Stop,
    Status,
    Help,
}

/// Komendy obsługiwane przez bridge; wszystko inne (też `/model x`) leci do agenta w tmuxie.
fn parse_command(body: &str) -> Option<BotCommand> {
    let body = body.trim();
    if !body.starts_with('/') {
        return None;
    }
    let mut parts = body.split_whitespace();
    let cmd = parts.next()?;
    let arg = parts.next();
    Some(match cmd {
        "/new" => BotCommand::New(arg.map(|s| s.to_string())),
        "/agent" => arg.map(|a| BotCommand::Agent(a.to_string())).unwrap_or(BotCommand::Help),
        "/attach" => BotCommand::Attach,
        "/stop" => BotCommand::Stop,
        "/status" => BotCommand::Status,
        "/help" | "/start" => BotCommand::Help,
        _ => return None, // nie nasza komenda -> do tmuxa
    })
}

const HELP: &str = "🤖 Komendy bridge'a:
/new [agent] — zabij sesję tmux i zacznij nową (agent: vibe|codex|claude|agy)
/agent <vibe|codex|claude|agy> — przełącz agenta (nowa sesja tmux przy następnej wiadomości)
/attach — pokaż ostatnie linie ekranu agenta (capture-pane)
/stop — zabij sesję tmux agenta
/status — stan bota
/help — ta lista
Inne wiadomości (także /model, /think z TUI agenta) lecą do agenta w tmux — kontekst jest pamiętany.";

/// Komenda startowa agenta w tmux.
fn agent_cmd(agent: &str) -> Option<&'static str> {
    match agent.to_ascii_lowercase().as_str() {
        "vibe" => Some("vibe"),
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        "agy" | "antigravity" => Some("/root/.local/bin/agy"),
        _ => None,
    }
}

fn tmux_session(cfg: &Config, agent: &str) -> String {
    format!("{}-{}", cfg.tmux_prefix, agent)
}

/// Uruchamia `tmux ...` synchronicznie, zwraca stdout; błąd gdy exit != 0.
fn tmux(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("spawn tmux")?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux {:?} exit {:?}: {}",
            args,
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Czy sesja tmux istnieje.
fn tmux_has_session(session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Zapewnia żyjącą sesję tmux z agentem (startuje jeśli nie ma).
fn ensure_session(cfg: &Config, agent: &str) -> Result<String> {
    let session = tmux_session(cfg, agent);
    if !tmux_has_session(&session) {
        let cmd = agent_cmd(agent).with_context(|| format!("nieznany agent `{agent}` (vibe|codex|claude|agy)"))?;
        tmux(&["new-session", "-d", "-s", &session, "-c", &cfg.vibe_workdir, cmd])?;
        // poczekaj aż agent wystartuje
        std::thread::sleep(Duration::from_secs(3));
    }
    Ok(session)
}

fn kill_session(cfg: &Config, agent: &str) {
    let _ = tmux(&["kill-session", "-t", &tmux_session(cfg, agent)]);
}

/// Wysyła tekst do sesji tmux (literal + Enter).
fn tmux_send(session: &str, text: &str) -> Result<()> {
    tmux(&["send-keys", "-t", session, "-l", text])?;
    tmux(&["send-keys", "-t", session, "Enter"])?;
    Ok(())
}

/// Capture ostatnich linii ekranu (cała historia do -300).
fn tmux_capture(session: &str) -> Result<String> {
    tmux(&["capture-pane", "-p", "-t", session, "-S", "-300"])
}

/// Czeka na odpowiedź agenta: poll capture co 2s; koniec = 3 kolejne próbki bez zmian; max timeout.
async fn wait_reply(session: &str, prev: &str, timeout_secs: u64) -> String {
    let mut last = prev.to_string();
    let mut stable = 0;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let cur = match tmux_capture(session) {
            Ok(c) => c,
            Err(_) => break, // sesja padła
        };
        if cur == last {
            stable += 1;
            if stable >= 3 || Instant::now() >= deadline {
                break;
            }
        } else {
            stable = 0;
            last = cur;
        }
    }
    // nowa część = suffix po poprzednim capture; jak się nie zgadza (scroll), bierz ostatnie 80 linii
    if last.starts_with(prev) {
        last[prev.len()..].trim().to_string()
    } else {
        let lines: Vec<&str> = last.lines().collect();
        let start = lines.len().saturating_sub(80);
        lines[start..].join("\n")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bridge_common::init_logging();
    let cfg = Config::from_env().context("config z env")?;
    tracing::info!(room = %cfg.room_id, homeserver = %cfg.homeserver, "vibe-matrix-rs headless v5 (tmux) start");

    std::fs::create_dir_all(&cfg.vibe_workdir).context("create vibe_workdir")?;

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut agent: String = load_str_opt(&cfg.agent_file).unwrap_or_else(|| "vibe".to_string());
    let mut since: Option<String> = None;
    let started = Instant::now();

    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(&cfg, &client, ev, &mut seen, &mut agent, started).await {
                            tracing::error!(event = %ev.id, "błąd obsługi eventu: {e:#}");
                        }
                    }
                }
                if let Err(e) = save_seen(&cfg.state_file, &seen) {
                    tracing::warn!("nie udało się zapisać stanu: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!("sync error: {e:#} — retry za 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle_event(
    cfg: &Config,
    client: &MatrixClient,
    ev: &Event,
    seen: &mut HashSet<String>,
    agent: &mut String,
    started: Instant,
) -> Result<()> {
    if ev.kind != "m.room.message" || ev.content.msgtype != "m.text" {
        return Ok(());
    }
    if ev.sender == cfg.bot_user_id {
        return Ok(());
    }
    if !cfg.allowed_users.is_empty() && !cfg.allowed_users.contains(&ev.sender) {
        tracing::debug!(sender = %ev.sender, "pomijam (poza allowlist)");
        return Ok(());
    }
    if !seen.insert(ev.id.clone()) {
        return Ok(());
    }

    let body = ev.content.body.trim();
    if body.is_empty() {
        return Ok(());
    }
    tracing::info!(event = %ev.id, sender = %ev.sender, "nowa wiadomość ({} znaków)", body.len());
    client.react(&ev.id, "👀").await.ok();

    // komendy bridge'a
    if let Some(cmd) = parse_command(body) {
        let reply = match cmd {
            BotCommand::New(new_agent) => {
                if let Some(a) = &new_agent {
                    if agent_cmd(a).is_none() {
                        format!("❓ Nieznany agent `{a}` (vibe|codex|claude|agy)")
                    } else {
                        *agent = a.to_ascii_lowercase();
                        let _ = save_str(&cfg.agent_file, agent);
                        kill_session(cfg, agent);
                        format!("✅ Nowa sesja agenta `{agent}` (tmux)")
                    }
                } else {
                    kill_session(cfg, agent);
                    format!("✅ Sesja `{agent}` zabita — następna wiadomość zacznie nową")
                }
            }
            BotCommand::Agent(a) => {
                if agent_cmd(&a).is_none() {
                    format!("❓ Nieznany agent `{a}` (vibe|codex|claude|agy)")
                } else {
                    *agent = a.to_ascii_lowercase();
                    let _ = save_str(&cfg.agent_file, agent);
                    format!("✅ Agent ustawiony na `{agent}` (sesja tmux wystartuje przy następnej wiadomości)")
                }
            }
            BotCommand::Attach => {
                match ensure_session(cfg, agent) {
                    Ok(s) => match tmux_capture(&s) {
                        Ok(c) => {
                            let lines: Vec<&str> = c.lines().collect();
                            let start = lines.len().saturating_sub(60);
                            format!("📎 Ostatnie linie `{s}`:\n```\n{}\n```", lines[start..].join("\n"))
                        }
                        Err(e) => format!("⚠️ capture-pane: {e:#}"),
                    },
                    Err(e) => format!("⚠️ tmux: {e:#}"),
                }
            }
            BotCommand::Stop => {
                kill_session(cfg, agent);
                "🛑 Sesja tmux zabita.".to_string()
            }
            BotCommand::Status => {
                let s = tmux_session(cfg, agent);
                let alive = tmux_has_session(&s);
                format!(
                    "📊 Status:\n• pokój: {}\n• agent: {agent} (tmux `{s}`, alive={alive})\n• homeserver: {}\n• seen: {} eventów\n• uptime: {:?}",
                    cfg.room_id, cfg.homeserver, seen.len(), started.elapsed()
                )
            }
            BotCommand::Help => HELP.to_string(),
        };
        let sent = client.send_text(&reply).await?;
        tracing::info!(reply_event = %sent, "odpowiedź na komendę");
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }

    // zwykła wiadomość (lub /komenda agenta typu /model) -> tmux
    match run_agent_tmux(cfg, agent, body).await {
        Ok(text) => {
            let clean = clean_ansi(&text);
            let reply = if clean.is_empty() {
                "(agent zwrócił pustą odpowiedź — /attach żeby zobaczyć ekran)".to_string()
            } else if clean.len() > 3800 {
                format!("{}…\n\n_(obcięte do 3800 znaków — /attach po pełny ekran)_", safe_truncate(&clean, 3800))
            } else {
                clean
            };
            let sent = client.send_text(&reply).await?;
            tracing::info!(reply_event = %sent, "wysłano odpowiedź agenta");
            client.react(&ev.id, "✅").await.ok();
        }
        Err(e) => {
            let msg = format!("⚠️ błąd agenta (tmux): {e:#}");
            client.send_text(&msg).await.ok();
            client.react(&ev.id, "❌").await.ok();
        }
    }
    Ok(())
}

/// Wysyła prompt do sesji tmux agenta i czeka na odpowiedź (capture-pane). Zwraca nowy tekst ekranu.
async fn run_agent_tmux(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    let session = ensure_session(cfg, agent)?;
    let prev = tmux_capture(&session).unwrap_or_default();
    tmux_send(&session, prompt)?;
    let reply = wait_reply(&session, &prev, cfg.vibe_timeout_secs).await;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bridge_commands() {
        assert_eq!(parse_command("/new"), Some(BotCommand::New(None)));
        assert_eq!(
            parse_command("/new codex"),
            Some(BotCommand::New(Some("codex".to_string())))
        );
        assert_eq!(parse_command("/agent agy"), Some(BotCommand::Agent("agy".to_string())));
        assert_eq!(parse_command("/attach"), Some(BotCommand::Attach));
        assert_eq!(parse_command("/stop"), Some(BotCommand::Stop));
        assert_eq!(parse_command("/status"), Some(BotCommand::Status));
        assert_eq!(parse_command("/help"), Some(BotCommand::Help));
        // komendy agenta (NIE bridge'a) -> None = lecą do tmuxa
        assert_eq!(parse_command("/model mistral-large-latest"), None);
        assert_eq!(parse_command("/think high"), None);
        assert_eq!(parse_command("zwykły tekst"), None);
    }

    #[test]
    fn agent_cmds() {
        assert_eq!(agent_cmd("vibe"), Some("vibe"));
        assert_eq!(agent_cmd("AGY"), Some("/root/.local/bin/agy"));
        assert_eq!(agent_cmd("nieznany"), None);
    }
}
