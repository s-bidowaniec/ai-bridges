//! bridge-headless: pętla sync Matrix -> `vibe -p --resume <session>` -> odpowiedź do pokoju.
//! MVP Fazy 0+1 z ROADMAP-RUST.md + komendy: /new /model /think /status /help.
//! Sesja vibe kontynuowana między wiadomościami (`--resume`), żeby nie tracić kontekstu.
use anyhow::{Context, Result};
use bridge_common::{
    clean_ansi, load_seen, load_str_opt, provider_env_key, remove_state_file, safe_truncate,
    save_seen, save_str, Config,
};
use bridge_core::{Event, MatrixClient};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, PartialEq)]
enum BotCommand {
    New,
    Model(Option<String>),
    Think(Option<String>),
    Provider(Option<String>),
    Status,
    Help,
    Unknown(String),
}

/// Parsuje komendy `/...`; wiadomości niebędące komendą -> None (leci do agenta).
fn parse_command(body: &str) -> Option<BotCommand> {
    let body = body.trim();
    if !body.starts_with('/') {
        return None;
    }
    let mut parts = body.split_whitespace();
    let cmd = parts.next()?;
    let arg = parts.next();
    Some(match cmd {
        "/new" => BotCommand::New,
        "/model" => BotCommand::Model(arg.map(|s| s.to_string())),
        "/think" => BotCommand::Think(arg.map(|s| s.to_string())),
        "/provider" => BotCommand::Provider(arg.map(|s| s.to_string())),
        "/status" => BotCommand::Status,
        "/help" | "/start" => BotCommand::Help,
        other => BotCommand::Unknown(other.to_string()),
    })
}

const HELP: &str = "🤖 Komendy bota:
/new — nowa sesja vibe (reset kontekstu)
/model — pokaż aktualny model
/model <nazwa> — ustaw model (np. /model mistral-large-latest)
/think — pokaż poziom myślenia
/think <off|low|medium|high|xhigh|max> — ustaw thinking (działa od nowej sesji)
/provider — pokaż aktualnego providera
/provider <mistral|openai|anthropic|google> — przełącz providera (i klucz API)
/status — stan bota (pokój, sesja, model, seen)
/help — ta lista
Każda inna wiadomość leci do agenta vibe (sesja jest pamiętana między wiadomościami).";

#[tokio::main]
async fn main() -> Result<()> {
    bridge_common::init_logging();
    let cfg = Config::from_env().context("config z env")?;
    tracing::info!(room = %cfg.room_id, homeserver = %cfg.homeserver, "vibe-matrix-rs headless start");

    std::fs::create_dir_all(&cfg.vibe_workdir).context("create vibe_workdir")?;

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut session: Option<String> = load_str_opt(&cfg.session_file);
    let mut model: Option<String> = load_str_opt(&cfg.model_file);
    let mut think: Option<String> = load_str_opt(&cfg.think_file);
    let mut provider: Option<String> = load_str_opt(&cfg.provider_file);
    let mut since: Option<String> = None;
    let started = Instant::now();

    // pierwszy sync: tylko najnowsze (bez since = od teraz), żeby nie przerabiać historii
    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(
                            &cfg, &client, ev, &mut seen, &mut session, &mut model, &mut think, &mut provider,
                            started,
                        )
                        .await
                        {
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

#[allow(clippy::too_many_arguments)]
async fn handle_event(
    cfg: &Config,
    client: &MatrixClient,
    ev: &Event,
    seen: &mut HashSet<String>,
    session: &mut Option<String>,
    model: &mut Option<String>,
    think: &mut Option<String>,
    provider: &mut Option<String>,
    started: Instant,
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

    // komendy `/...` — nie lecą do agenta
    if let Some(cmd) = parse_command(body) {
        let reply = match cmd {
            BotCommand::New => {
                remove_state_file(&cfg.session_file);
                *session = None;
                "✅ Nowa sesja vibe — poprzedni kontekst zapomniany.".to_string()
            }
            BotCommand::Model(arg) => match arg {
                None => format!("📌 Aktualny model: {}", model.as_deref().unwrap_or("(domyślny z configu vibe)")),
                Some(m) => {
                    *model = Some(m.clone());
                    let _ = save_str(&cfg.model_file, &m);
                    format!("✅ Model ustawiony na `{m}` (od następnej wiadomości)")
                }
            },
            BotCommand::Think(arg) => match arg {
                None => format!("🧠 Aktualny think: {}", think.as_deref().unwrap_or("(domyślny)")),
                Some(t) => {
                    *think = Some(t.clone());
                    let _ = save_str(&cfg.think_file, &t);
                    format!("✅ Thinking ustawiony na `{t}` (vibe применяет przy nowej sesji; zrób /new żeby wymusić)")
                }
            },
            BotCommand::Provider(arg) => match arg {
                None => format!("🔌 Aktualny provider: {}", provider.as_deref().unwrap_or("(domyślny — wszystkie klucze z VIBE_ENV_KEYS)")),
                Some(p) => {
                    let pl = p.to_ascii_lowercase();
                    match provider_env_key(&pl) {
                        None => format!("❓ Nieznany provider `{p}`. Dostępni: mistral, openai, anthropic, google"),
                        Some((env_key, default_model)) => {
                            let has_key = std::env::var(env_key).is_ok();
                            *provider = Some(pl.clone());
                            let _ = save_str(&cfg.provider_file, &pl);
                            if has_key {
                                format!("✅ Provider `{pl}` (klucz {env_key} OK). Domyślny model: `{default_model}` — ustaw /model jeśli chcesz inny.")
                            } else {
                                format!("⚠️ Provider `{pl}` ustawiony, ale brak zmiennej {env_key} w env bota! Dodaj do /etc/vibe-matrix-rs.env i zrestartuj. Domyślny model: `{default_model}`")
                            }
                        }
                    }
                }
            },
            BotCommand::Status => {
                let sess = session.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("(brak)");
                format!(
                    "📊 Status:\n• pokój: {}\n• homeserver: {}\n• sesja vibe: {}…\n• provider: {}\n• model: {}\n• think: {}\n• przetworzone eventy (seen): {}\n• uptime: {:?}",
                    cfg.room_id,
                    cfg.homeserver,
                    sess,
                    provider.as_deref().unwrap_or("(domyślny)"),
                    model.as_deref().unwrap_or("(domyślny)"),
                    think.as_deref().unwrap_or("(domyślny)"),
                    seen.len(),
                    started.elapsed()
                )
            }
            BotCommand::Help => HELP.to_string(),
            BotCommand::Unknown(c) => format!("❓ Nieznana komenda `{c}` — wpisz /help"),
        };
        let sent = client.send_text(&reply).await?;
        tracing::info!(reply_event = %sent, "wysłano odpowiedź na komendę");
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }

    match run_vibe(cfg, body, session.as_deref(), model.as_deref(), think.as_deref(), provider.as_deref()).await {
        Ok((sid, text)) => {
            if !sid.is_empty() {
                *session = Some(sid.clone());
                if let Err(e) = save_str(&cfg.session_file, &sid) {
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

/// Uruchamia `vibe -p <prompt> --yolo --output json [--resume <s>] [--model <m>] [--thinking <t>]`,
/// czysty env, timeout. Zwraca (session_id, tekst odpowiedzi).
async fn run_vibe(
    cfg: &Config,
    prompt: &str,
    session: Option<&str>,
    model: Option<&str>,
    think: Option<&str>,
    provider: Option<&str>,
) -> Result<(String, String)> {
    let mut cmd = Command::new(&cfg.vibe_cmd);
    cmd.arg("-p").arg(prompt).arg("--yolo").arg("--output").arg("json");
    if let Some(sid) = session {
        cmd.arg("--resume").arg(sid);
    }
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if let Some(t) = think {
        cmd.arg("--thinking").arg(t);
    }
    cmd.current_dir(&cfg.vibe_workdir)
        .env_clear()
        .env("PATH", "/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", "/root")
        .env("TERM", "dumb");
    if let Some(p) = provider {
        // tryb providera: tylko jego klucz API (whitelist env_clear już zrobiona)
        if let Some((env_key, _)) = provider_env_key(p) {
            match std::env::var(env_key) {
                Ok(v) => {
                    cmd.env(env_key, v);
                }
                Err(_) => {
                    anyhow::bail!("provider `{p}` wymaga zmiennej {env_key} w env bota (EnvironmentFile)");
                }
            }
        }
    } else {
        // backward-compat: wszystkie klucze z whitelisty VIBE_ENV_KEYS
        for key in &cfg.vibe_env_keys {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
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
    fn parse_commands() {
        assert_eq!(parse_command("/new"), Some(BotCommand::New));
        assert_eq!(parse_command("/model"), Some(BotCommand::Model(None)));
        assert_eq!(
            parse_command("/model mistral-large-latest"),
            Some(BotCommand::Model(Some("mistral-large-latest".to_string())))
        );
        assert_eq!(parse_command("/think high"), Some(BotCommand::Think(Some("high".to_string()))));
        assert_eq!(parse_command("/provider"), Some(BotCommand::Provider(None)));
        assert_eq!(parse_command("/provider openai"), Some(BotCommand::Provider(Some("openai".to_string()))));
        assert_eq!(parse_command("/status"), Some(BotCommand::Status));
        assert_eq!(parse_command("/help"), Some(BotCommand::Help));
        assert_eq!(parse_command("/foobar x"), Some(BotCommand::Unknown("/foobar".to_string())));
        assert_eq!(parse_command("zwykła wiadomość"), None);
        assert_eq!(parse_command("coś /new w środku"), None);
    }

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
