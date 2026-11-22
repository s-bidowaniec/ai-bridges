//! bridge-headless v5: Matrix room <-> terminal agent (vibe/codex/claude/agy) w stałej sesji tmux.
//! Bez kluczy API pośrednio — agent żyje w tmuxie, my tylko send-keys + capture-pane.
//! Komendy bota: /new [agent] /agent <n> /attach /stop /status /help; reszta (np. /model) leci do tmuxa.
use anyhow::{Context, Result};
use bridge_common::{clean_ansi, load_seen, load_str_opt, safe_truncate, save_seen, save_str, Config};
use bridge_core::{Event, MatrixClient};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, PartialEq)]
enum BotCommand {
    New(Option<String>),
    Agent(String),
    Attach,
    Stop,
    Voice(Option<String>),
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
        "/voice" => BotCommand::Voice(arg.map(|s| s.to_string())),
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
/voice — status trybu głosowego
/voice on|off — odpowiadaj głosem (TTS) też na tekstowe
/voice new — nowa konwersacja głosowa (reset sesji agenta)
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
    std::fs::create_dir_all(&cfg.agent_home).context("create agent_home")?;

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    // watcher: pliki z voice/out/ -> m.audio na Matrix (tryb "konektor": terminal odpowiada głosem)
    {
        let cfg2 = cfg.clone();
        let client2 = client.clone();
        tokio::spawn(async move { voice_out_watcher(cfg2, client2).await });
    }
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut agent: String = load_str_opt(&cfg.agent_file).unwrap_or_else(|| "vibe".to_string());
    let mut voice_on: bool = load_str_opt(&cfg.voice_file).map(|v| v == "on").unwrap_or(false);
    let mut since: Option<String> = None;
    let started = Instant::now();

    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(&cfg, &client, ev, &mut seen, &mut agent, &mut voice_on, started).await {
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
    voice_on: &mut bool,
    started: Instant,
) -> Result<()> {
    if ev.kind != "m.room.message" {
        return Ok(());
    }
    if ev.content.msgtype == "m.audio" {
        return handle_voice(cfg, client, ev, seen, agent, voice_on).await;
    }
    if ev.content.msgtype != "m.text" {
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
                    "📊 Status:\n• pokój: {}\n• agent: {agent} (tmux `{s}`, alive={alive})\n• voice: {}\n• homeserver: {}\n• seen: {} eventów\n• uptime: {:?}",
                    cfg.room_id, if *voice_on { "ON" } else { "OFF" }, cfg.homeserver, seen.len(), started.elapsed()
                )
            }
            BotCommand::Voice(arg) => match arg.as_deref() {
                None => format!("🎙️ Tryb voice: {}", if *voice_on { "ON (odpowiedzi głosem)" } else { "OFF" }),
                Some("on") => {
                    *voice_on = true;
                    let _ = save_str(&cfg.voice_file, "on");
                    "✅ Tryb voice ON — będę odpowiadał głosem (TTS).".to_string()
                }
                Some("off") => {
                    *voice_on = false;
                    let _ = save_str(&cfg.voice_file, "off");
                    "✅ Tryb voice OFF — odpowiedzi tekstowe.".to_string()
                }
                Some("new") => {
                    kill_session(cfg, agent);
                    "🎙️🔃 Nowa konwersacja głosowa — poprzedni kontekst zapomniany.".to_string()
                }
                Some(other) => format!("❓ /voice on|off|new (podano: {other})"),
            },
            BotCommand::Help => HELP.to_string(),
        };
        let sent = client.send_text_rel(&reply, Some(thread_rel(&ev.id))).await?;
        tracing::info!(reply_event = %sent, "odpowiedź na komendę");
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }

    // zwykła wiadomość (lub /komenda agenta typu /model) -> agent
    match run_agent(cfg, agent, body).await {
        Ok(text) => {
            let clean = clean_ansi(&text);
            let reply = if clean.is_empty() {
                "(agent zwrócił pustą odpowiedź — /attach żeby zobaczyć ekran)".to_string()
            } else if clean.len() > 3800 {
                format!("{}…\n\n_(obcięte do 3800 znaków — /attach po pełny ekran)_", safe_truncate(&clean, 3800))
            } else {
                clean
            };
            let sent = send_reply(client, cfg, &reply, *voice_on, Some(&ev.id)).await?;
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

#[derive(Debug, PartialEq, Clone, Copy)]
enum AgentMode {
    /// vibe/codex/claude: headless `AGENT -p --resume <sid>` — szybko, bez menu TUI, kontekst w pliku sesji
    Subprocess,
    /// agy (Antigravity): żyje w tmux TUI (ma tryb voice) — send-keys + capture-pane
    Tmux,
}

fn agent_mode(agent: &str) -> AgentMode {
    match agent.to_ascii_lowercase().as_str() {
        "agy" | "antigravity" => AgentMode::Tmux,
        _ => AgentMode::Subprocess,
    }
}

/// Dispatch per agent.
async fn run_agent(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    match agent_mode(agent) {
        AgentMode::Subprocess => run_agent_subprocess(cfg, agent, prompt).await,
        AgentMode::Tmux => run_agent_tmux(cfg, agent, prompt).await,
    }
}

/// Subprocess tryb: `vibe -p <prompt> --yolo --output json [--resume <sid>]`; sesja per agent w pliku.
async fn run_agent_subprocess(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    let sess_file = format!("{}.{}", cfg.session_file, agent);
    let sid = load_str_opt(&sess_file);
    let mut cmd = Command::new(agent_cmd(agent).context("nieznany agent")?);
    cmd.arg("-p").arg(prompt).arg("--yolo").arg("--output").arg("json");
    if let Some(s) = &sid {
        cmd.arg("--resume").arg(s);
    }
    cmd.current_dir(&cfg.vibe_workdir)
        .env_clear()
        .env("PATH", "/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", &cfg.agent_home)
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
    let child = cmd.spawn().with_context(|| format!("spawn {agent}"))?;
    let out = tokio::time::timeout(Duration::from_secs(cfg.vibe_timeout_secs), child.wait_with_output())
        .await
        .context("timeout agenta")?
        .context("wait agenta")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{agent} exit {:?}: {}", out.status.code(), &stderr[..stderr.len().min(500)]);
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    match parse_vibe_json(&stdout) {
        Some((sid, text)) if !text.is_empty() => {
            if !sid.is_empty() {
                let _ = save_str(&sess_file, &sid);
            }
            Ok(text)
        }
        _ => Ok(stdout),
    }
}

/// Wysyła prompt do sesji tmux agenta i czeka na odpowiedź (capture-pane). Zwraca nowy tekst ekranu.
async fn run_agent_tmux(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    let session = ensure_session(cfg, agent)?;
    let prev = tmux_capture(&session).unwrap_or_default();
    tmux_send(&session, prompt)?;
    let reply = wait_reply(&session, &prev, cfg.vibe_timeout_secs).await;
    Ok(reply)
}

/// Watcher katalogu voice/out/: każdy nowy plik audio -> upload -> m.audio na Matrix.
/// (To jest "konektor": terminal agenta w trybie voice odpowiada plikiem tutaj.)
async fn voice_out_watcher(cfg: Config, client: MatrixClient) {
    use std::collections::HashSet as HS2;
    let out_dir = format!("{}/out", cfg.voice_dir);
    let _ = std::fs::create_dir_all(&out_dir);
    let mut sent: HS2<String> = HS2::new();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let rd = match std::fs::read_dir(&out_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if sent.contains(&name) {
                continue;
            }
            if !(name.ends_with(".opus") || name.ends_with(".ogg") || name.ends_with(".wav") || name.ends_with(".mp3")) {
                continue;
            }
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let mime = if name.ends_with(".wav") { "audio/wav" } else { "audio/ogg" };
            match client.upload_media(&bytes, &name, mime).await {
                Ok(mxc) => {
                    let dur = (bytes.len() / 4000) as u32 * 10; // estymata
                    let wf = pseudo_waveform(bytes.len());
                    match client.send_audio(&format!("🎙️ {} (odpowiedź głosowa)", name), &mxc, mime, bytes.len(), dur.max(500), &wf).await {
                        Ok(eid) => {
                            tracing::info!(event = %eid, file = %name, "voice out -> Matrix");
                            sent.insert(name.clone());
                            let _ = std::fs::remove_file(e.path());
                        }
                        Err(err) => tracing::warn!("send_audio fail: {err:#}"),
                    }
                }
                Err(err) => tracing::warn!("upload fail: {err:#}"),
            }
        }
    }
}

/// Odpowiada na Matrix: głosem (TTS→m.audio) gdy voice_on, inaczej tekstem.
fn thread_rel(root: &str) -> serde_json::Value {
    serde_json::json!({"rel_type": "m.thread", "event_id": root})
}

async fn send_reply(client: &MatrixClient, cfg: &Config, text: &str, voice_on: bool, thread_root: Option<&str>) -> Result<String> {
    let rel = thread_root.map(thread_rel);
    if voice_on && std::env::var("MISTRAL_API_KEY").is_ok() {
        match tts(cfg, text).await {
            Ok((bytes, dur)) => {
                let mxc = client.upload_media(&bytes, "reply.opus", "audio/ogg").await?;
                let wf = pseudo_waveform(text.len());
                let body = format!("🎙️ {}", safe_truncate(text, 200));
                // UWAGA: ten Synapse odrzuca m.audio z m.relates_to ("Cannot start threads from an event with a relation")
                // więc voice odpowiedź idzie jako zwykła wiadomość (bez wątku); tekst może być w wątku.
                return client.send_audio_rel(&body, &mxc, "audio/ogg", bytes.len(), dur, &wf, None).await;
            }
            Err(e) => tracing::warn!("TTS nieudane, wysyłam tekst: {e:#}"),
        }
    }
    client.send_text_rel(text, rel).await
}

/// m.audio -> download -> STT -> agent (tmux) -> odpowiedź (głosem jeśli voice_on).
async fn handle_voice(
    cfg: &Config,
    client: &MatrixClient,
    ev: &Event,
    seen: &mut HashSet<String>,
    agent: &mut String,
    voice_on: &mut bool,
) -> Result<()> {
    if ev.content.file.is_some() {
        client.send_text("⚠️ zaszyfrowane media (E2EE) nie są obsługiwane w MVP").await.ok();
        return Ok(());
    }
    let mxc = ev.content.url.clone().context("m.audio bez url")?;
    if !seen.insert(ev.id.clone()) {
        return Ok(());
    }
    client.react(&ev.id, "👀").await.ok();
    let (fname, bytes, mime) = match client.download_mxc(&mxc).await {
        Ok(t) => t,
        Err(e) => {
            client.send_text(&format!("⚠️ nie pobrałem audio: {e:#}")).await.ok();
            return Ok(());
        }
    };
    std::fs::create_dir_all(&cfg.uploads_dir).ok();
    let safe_name: String = fname.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
    let path = format!("{}/{}", cfg.uploads_dir, safe_name);
    let _ = std::fs::write(&path, &bytes);

    // v8 PASSTHROUGH: wrzuć audio do voice/in/, terminal (agent w trybie voice) odpowiada do voice/out/
    let stem = safe_name.trim_end_matches(|c| c != '.').to_string();
    let stem = stem.rsplit('.').next().unwrap_or(&safe_name).to_string();
    let in_dir = format!("{}/in", cfg.voice_dir);
    let out_dir = format!("{}/out", cfg.voice_dir);
    std::fs::create_dir_all(&in_dir).ok();
    std::fs::create_dir_all(&out_dir).ok();
    let in_file = format!("{}/{}", in_dir, safe_name);
    let _ = std::fs::write(&in_file, &bytes);
    tracing::info!(file = %in_file, "voice in -> czekam na out/ ({}s)", cfg.voice_fallback_secs);
    let mut answered = false;
    for _ in 0..cfg.voice_fallback_secs {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Ok(rd) = std::fs::read_dir(&out_dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&stem) {
                    answered = true; // watcher wyśle to jako m.audio
                    break;
                }
            }
        }
        if answered { break; }
    }
    if answered {
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }
    tracing::warn!("brak odpowiedzi z voice/out/ po {}s — fallback API (STT->agent->TTS)", cfg.voice_fallback_secs);

    let text = match stt(cfg, &bytes, &safe_name, &mime).await {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            client.send_text("🎙️ (nie rozpoznałem mowy)").await.ok();
            return Ok(());
        }
        Err(e) => {
            client.send_text(&format!("⚠️ STT błąd: {e:#}")).await.ok();
            client.react(&ev.id, "❌").await.ok();
            return Ok(());
        }
    };
    tracing::info!(event = %ev.id, len = text.len(), "STT: {text:.80}");
    let prompt = format!("[voice message od {}]: {}", ev.sender, text);
    match run_agent(cfg, agent, &prompt).await {
        Ok(reply) => {
            let clean = clean_ansi(&reply);
            let reply = if clean.is_empty() {
                "(agent nic nie odpowiedział — /attach)".to_string()
            } else if clean.len() > 3800 {
                format!("{}…", safe_truncate(&clean, 3800))
            } else {
                clean
            };
            let sent = send_reply(client, cfg, &reply, *voice_on, Some(&ev.id)).await?;
            tracing::info!(reply_event = %sent, "wysłano odpowiedź (voice)");
            client.react(&ev.id, "✅").await.ok();
        }
        Err(e) => {
            client.send_text(&format!("⚠️ błąd agenta: {e:#}")).await.ok();
            client.react(&ev.id, "❌").await.ok();
        }
    }
    Ok(())
}

/// STT przez Mistral /v1/audio/transcriptions.
async fn stt(cfg: &Config, audio: &[u8], filename: &str, mime: &str) -> Result<String> {
    let key = std::env::var("MISTRAL_API_KEY").context("brak MISTRAL_API_KEY — voice niedostępne")?;
    let http = reqwest::Client::new();
    let part = reqwest::multipart::Part::bytes(audio.to_vec())
        .file_name(filename.to_string())
        .mime_str(mime)
        .context("mime")?;
    let form = reqwest::multipart::Form::new()
        .text("model", cfg.stt_model.clone())
        .part("file", part); // bez language = auto-detect (pl nie jest na liście modeli transcribe)
    let resp = http
        .post("https://api.mistral.ai/v1/audio/transcriptions")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .context("STT send")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.context("STT json")?;
    if !status.is_success() {
        anyhow::bail!("STT HTTP {status}: {v}");
    }
    Ok(v["text"].as_str().unwrap_or("").to_string())
}

/// TTS przez Mistral /v1/audio/speech → (opus bytes, duration_ms).
async fn tts(cfg: &Config, text: &str) -> Result<(Vec<u8>, u32)> {
    let key = std::env::var("MISTRAL_API_KEY").context("brak MISTRAL_API_KEY")?;
    let http = reqwest::Client::new();
    let body = serde_json::json!({
        "model": cfg.tts_model,
        "input": text,
        "voice": cfg.tts_voice,
        "response_format": "opus",
    });
    let resp = http
        .post("https://api.mistral.ai/v1/audio/speech")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("TTS send")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.context("TTS json")?;
    if !status.is_success() {
        anyhow::bail!("TTS HTTP {status}: {v}");
    }
    // Mistral zwraca JSON {"audio_data": "<base64 OGG/Opus>"}
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let b64 = v["audio_data"].as_str().context("TTS: brak audio_data w odpowiedzi")?;
    let bytes = STANDARD.decode(b64).context("TTS: base64 decode")?;
    let duration_ms = ((text.split_whitespace().count() as f64 / 150.0) * 60_000.0) as u32;
    Ok((bytes, duration_ms.max(500)))
}

/// Prosta pseudo-fala (sinusoida) do voice message.
fn pseudo_waveform(_text_len: usize) -> Vec<u32> {
    (0..40).map(|i| (300.0 + (i as f32 * 0.7).sin() * 200.0) as u32).collect()
}

fn parse_vibe_json(s: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let sid = arr.iter().find_map(|e| e.get("sessionId")?.as_str()).unwrap_or("").to_string();
    let mut text = String::new();
    for e in arr.iter().rev() {
        if e.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(content) = e.get("content").and_then(|c| c.as_array()) {
                for c in content {
                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = c.get("text").and_then(|t| t.as_str()) { text.push_str(t); }
                    }
                }
            }
            if !text.is_empty() { break; }
        }
    }
    Some((sid, text))
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
        assert_eq!(parse_command("/voice on"), Some(BotCommand::Voice(Some("on".to_string()))));
        assert_eq!(parse_command("/voice"), Some(BotCommand::Voice(None)));
        assert_eq!(parse_command("/voice new"), Some(BotCommand::Voice(Some("new".to_string()))));
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
