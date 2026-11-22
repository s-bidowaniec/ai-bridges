//! bridge-headless: pętla sync Matrix -> `vibe -p --resume <session>` -> odpowiedź do pokoju.
//! MVP Fazy 0+1 z ROADMAP-RUST.md (bez TUI/tmux, bez mediów, dedup przez plik).
//! Sesja vibe jest kontynuowana między wiadomościami (`--resume`), żeby nie tracić kontekstu.
use anyhow::{Context, Result};
use bridge_common::{clean_ansi, load_seen, load_session, safe_truncate, save_seen, save_session, Config};
use bridge_core::{Event, MatrixClient};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[tokio::main]
async fn main() -> Result<()> {
    bridge_common::init_logging();
    let cfg = Config::from_env().context("config z env")?;
    tracing::info!(room = %cfg.room_id, homeserver = %cfg.homeserver, "vibe-matrix-rs headless start");

    std::fs::create_dir_all(&cfg.vibe_workdir).context("create vibe_workdir")?;

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut session: Option<String> = load_session(&cfg.session_file);
    let mut since: Option<String> = None;

    // pierwszy sync: tylko najnowsze (bez since = od teraz), żeby nie przerabiać historii
    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(&cfg, &client, ev, &mut seen, &mut session).await {
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
    session: &mut Option<String>,
) -> Result<()> {
    if ev.kind != "m.room.message" || ev.content.msgtype != "m.text" {
        return Ok(());
    }
    if ev.sender == cfg.bot_user_id {
        return Ok(()); // własne wiadomości
    }
    if !cfg.allowed_users.is_empty() && !cfg.allowed_users.contains(&ev.sender) {
        tracing::debug!(sender = %ev.sender, "pomijam (poza allowlist)");
        return Ok(());
    }
    if !seen.insert(ev.id.clone()) {
        return Ok(()); // już przetworzone
    }

    let body = ev.content.body.trim();
    if body.is_empty() {
        return Ok(());
    }
    tracing::info!(event = %ev.id, sender = %ev.sender, "nowa wiadomość ({} znaków)", body.len());
    client.react(&ev.id, "👀").await.ok();

    match run_vibe(cfg, body, session.as_deref()).await {
        Ok((sid, text)) => {
            if !sid.is_empty() {
                *session = Some(sid.clone());
                if let Err(e) = save_session(&cfg.session_file, &sid) {
                    tracing::warn!("nie udało się zapisać session: {e:#}");
                }
            }
            let clean = clean_ansi(&text);
            let reply = if clean.is_empty() {
                "(vibe zwrócił pustą odpowiedź)".to_string()
            } else if clean.len() > 3800 {
                format!("{}…\n\n_(obcięte do 3800 znaków)_", safe_truncate(&clean, 3800))
            } else {
                clean
            };
            let sent = client.send_text(&reply).await?;
            tracing::info!(reply_event = %sent, "wysłano odpowiedź");
            client.react(&ev.id, "✅").await.ok();
        }
        Err(e) => {
            let msg = format!("⚠️ błąd vibe: {e:#}");
            client.send_text(&msg).await.ok();
            client.react(&ev.id, "❌").await.ok();
        }
    }
    Ok(())
}

/// Uruchamia `vibe -p <prompt> --yolo --output json [--resume <session>]` z czystym env i timeoutem.
/// Zwraca (session_id, tekst odpowiedzi). Przy kontynuacji sesji vibe pamięta kontekst.
async fn run_vibe(cfg: &Config, prompt: &str, session: Option<&str>) -> Result<(String, String)> {
    let mut cmd = Command::new(&cfg.vibe_cmd);
    cmd.arg("-p").arg(prompt).arg("--yolo").arg("--output").arg("json");
    if let Some(sid) = session {
        cmd.arg("--resume").arg(sid);
    }
    cmd.current_dir(&cfg.vibe_workdir)
        .env_clear()
        .env("PATH", "/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", "/root")
        .env("TERM", "dumb");
    for key in &cfg.vibe_env_keys {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().with_context(|| format!("spawn {}", cfg.vibe_cmd))?;
    let timeout = Duration::from_secs(cfg.vibe_timeout_secs);
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .context("timeout vibe")?
        .context("wait vibe")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("vibe exit {:?}: {}", out.status.code(), &stderr[..stderr.len().min(500)]);
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // JSON journal eventów; fallback: surowe stdout jako tekst
    match parse_vibe_json(&stdout) {
        Some((sid, text)) if !text.is_empty() => Ok((sid, text)),
        _ => Ok((String::new(), stdout)),
    }
}

/// Parsuje `--output json` vibe: bierze sessionId + ostatnią wiadomość assistant (text).
fn parse_vibe_json(s: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let sid = arr
        .iter()
        .find_map(|e| e.get("sessionId")?.as_str())
        .unwrap_or("")
        .to_string();
    let mut text = String::new();
    for e in arr.iter().rev() {
        if e.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(content) = e.get("content").and_then(|c| c.as_array()) {
                for c in content {
                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
            }
            if !text.is_empty() {
                break;
            }
        }
    }
    Some((sid, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_journal() {
        let j = r#"[
            {"sessionId":"s-1","role":"user","content":[{"type":"text","text":"hej"}]},
            {"sessionId":"s-1","role":"assistant","content":[{"type":"text","text":"cześć!"}]}
        ]"#;
        let (sid, text) = parse_vibe_json(j).unwrap();
        assert_eq!(sid, "s-1");
        assert_eq!(text, "cześć!");
    }

    #[test]
    fn parse_plain_fallback() {
        assert!(parse_vibe_json("nie json").is_none());
    }
}
