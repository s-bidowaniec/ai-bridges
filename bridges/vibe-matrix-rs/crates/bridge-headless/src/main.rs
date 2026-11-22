//! bridge-headless: pętla sync Matrix -> subprocess `vibe -p --yolo` -> odpowiedź do pokoju.
//! MVP Fazy 0+1 z ROADMAP-RUST.md (bez TUI/tmux, bez mediów, dedup przez plik).
use anyhow::{Context, Result};
use bridge_common::{clean_ansi, load_seen, save_seen, Config};
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

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut since: Option<String> = None;

    // pierwszy sync: tylko najnowsze (bez since = od teraz), żeby nie przerabiać historii
    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(&cfg, &client, ev, &mut seen).await {
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

    match run_agent(cfg, body).await {
        Ok(text) => {
            let clean = clean_ansi(&text);
            let reply = if clean.is_empty() {
                "(vibe zwrócił pustą odpowiedź)".to_string()
            } else if clean.len() > 3800 {
                format!("{}…\n\n_(obcięte do 3800 znaków)_", &clean[..3800])
            } else {
                clean
            };
            let sent = client.send_text(&reply).await?;
            tracing::info!(reply_event = %sent, "wysłano odpowiedź");
            client.react(&ev.id, "✅").await.ok();
        }
        Err(e) => {
            let msg = format!("⚠️ błąd agenta: {e:#}");
            client.send_text(&msg).await.ok();
            client.react(&ev.id, "❌").await.ok();
        }
    }
    Ok(())
}

/// Uruchamia agenta (preset AGENT lub AGENT_CMD) z czystym env (tylko whitelist) i timeoutem.
async fn run_agent(cfg: &Config, prompt: &str) -> Result<String> {
    let argv = cfg.agent_argv(prompt)?;
    let mut cmd = Command::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }
    cmd.env_clear()
        .env("PATH", &cfg.agent_path)
        .env("HOME", &cfg.agent_home)
        .env("TERM", "dumb");
    for key in &cfg.agent_env_keys {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().with_context(|| format!("spawn {}", argv[0]))?;
    let timeout = Duration::from_secs(cfg.agent_timeout_secs);
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .context("timeout agenta")?
        .context("wait agenta")?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = |s: &str| s.trim().chars().rev().take(500).collect::<String>().chars().rev().collect::<String>();
        anyhow::bail!(
            "agent exit {:?}: stdout: {} | stderr: {}",
            out.status.code(),
            tail(&stdout),
            tail(&stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
