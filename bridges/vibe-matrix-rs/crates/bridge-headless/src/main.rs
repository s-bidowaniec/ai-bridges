//! bridge-headless v5: Matrix room <-> terminal agent (vibe/codex/claude/agy) w stałej sesji tmux.
//! Bez kluczy API pośrednio — agent żyje w tmuxie, my tylko send-keys + capture-pane.
//! Komendy bota: /new [agent] /agent <n> /attach /stop /status /help; reszta (np. /model) leci do tmuxa.
use anyhow::{Context, Result};
use bridge_common::{
    clean_tui, load_seen, load_str_opt, remove_state_file, safe_truncate, save_seen, save_str,
    Config, TerminalCmd,
};
use bridge_core::{Event, MatrixClient};
use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, PartialEq)]
enum BotCommand {
    New(Option<String>),
    Agent(String),
    Attach,
    Window,
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
        "/agent" => arg
            .map(|a| BotCommand::Agent(a.to_string()))
            .unwrap_or(BotCommand::Help),
        "/attach" => BotCommand::Attach,
        "/window" => BotCommand::Window,
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
/window — otwórz ponownie okno terminala z sesją agenta (mostek sam otwiera terminal)
/stop — zabij sesję tmux agenta
/voice — status trybu głosowego
/voice on|off — odpowiadaj głosem (TTS) też na tekstowe
/voice new — nowa konwersacja głosowa (reset sesji agenta)
/status — stan bota
/help — ta lista
Inne wiadomości (także /model, /think z TUI agenta) lecą do agenta w tmux — kontekst jest pamiętany.";

/// Komenda startowa agenta. `agy` jest konfigurowalne przez env AGY_CMD
/// (default: `agy` z PATH) — bez hardcodowanej ścieżki /root/.
fn agent_cmd(agent: &str) -> Option<String> {
    match agent.to_ascii_lowercase().as_str() {
        "vibe" => Some("vibe".to_string()),
        "codex" => Some("codex".to_string()),
        "claude" => Some("claude".to_string()),
        "agy" | "antigravity" => {
            Some(std::env::var("AGY_CMD").unwrap_or_else(|_| "agy".to_string()))
        }
        _ => None,
    }
}

fn tmux_session(cfg: &Config, agent: &str) -> String {
    // TMUX_SESSION wymusza konkretną sesję (np. podpięcie już otwartego okna przez reptyr)
    if let Ok(s) = std::env::var("TMUX_SESSION") {
        if !s.is_empty() {
            return s;
        }
    }
    format!("{}-{}", cfg.tmux_prefix, agent)
}

/// Target sesji tmux z wymuszonym exact match (`=` przed nazwą) — bez tego tmux robi też
/// dopasowanie prefiksowe i mógłby złapać inną sesję (np. `…-vibe2` przy `…-vibe`).
fn tmux_target(session: &str) -> String {
    format!("={session}")
}

/// Uruchamia `tmux ...` synchronicznie, zwraca stdout; błąd gdy exit != 0.
/// Zawsze z TMUX_TMPDIR z configu — socket musi być współdzielony z shellem
/// użytkownika (przy PrivateTmp=true domyślny /tmp serwisu byłby prywatny).
fn tmux(cfg: &Config, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("tmux")
        .args(args)
        .env("TMUX_TMPDIR", &cfg.tmux_tmpdir)
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
fn tmux_has_session(cfg: &Config, session: &str) -> bool {
    let t = tmux_target(session);
    std::process::Command::new("tmux")
        .args(["has-session", "-t", t.as_str()])
        .env("TMUX_TMPDIR", &cfg.tmux_tmpdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Czy pane sesji jest martwy (agent wyszedł/padł, ale sesja żyje dzięki remain-on-exit).
fn tmux_pane_dead(cfg: &Config, session: &str) -> bool {
    let t = tmux_target(session);
    std::process::Command::new("tmux")
        .args(["display-message", "-p", "-t", t.as_str(), "#{pane_dead}"])
        .env("TMUX_TMPDIR", &cfg.tmux_tmpdir)
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

/// Zapewnia żyjącą sesję tmux z agentem. Sesja NIGDY nie jest cicho kasowana i tworzona od nowa:
/// jeśli istnieje, ale pane jest martwy (agent się wysypał/wyszedł), agent jest respawnowany
/// w tej samej sesji — `tmux attach -t <sesja>` dalej pokazuje to samo okno.
async fn ensure_session(cfg: &Config, agent: &str) -> Result<String> {
    let session = tmux_session(cfg, agent);
    let t = tmux_target(&session);
    let cmd = agent_cmd(agent)
        .with_context(|| format!("nieznany agent `{agent}` (vibe|codex|claude|agy)"))?;
    if !tmux_has_session(cfg, &session) {
        tmux(
            cfg,
            &[
                "new-session",
                "-d",
                "-s",
                &session,
                "-c",
                &cfg.vibe_workdir,
                "-x",
                "200",
                "-y",
                "50",
                cmd.as_str(),
            ],
        )?;
        // sesja ma przetrwać wyjście agenta — wtedy respawn poniżej
        let _ = tmux(
            cfg,
            &[
                "set-window-option",
                "-t",
                t.as_str(),
                "remain-on-exit",
                "on",
            ],
        );
        // poczekaj aż agent wystartuje
        tokio::time::sleep(Duration::from_secs(3)).await;
        // mostek sam otwiera okno terminala podpięte do tej sesji (użytkownik tylko czyta/pisze)
        open_terminal_window(cfg, &session);
    } else if tmux_pane_dead(cfg, &session) {
        tracing::warn!(session = %session, agent = %agent, "pane martwy — respawn agenta w istniejącej sesji tmux");
        tmux(
            cfg,
            &[
                "respawn-pane",
                "-k",
                "-t",
                t.as_str(),
                "-c",
                &cfg.vibe_workdir,
                cmd.as_str(),
            ],
        )?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(session)
}

/// Ścieżka do binarki w PATH (z bitem wykonywalnym).
fn which(bin: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let p = format!("{dir}/{bin}");
        if let Ok(md) = std::fs::metadata(&p) {
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                return Some(p);
            }
        }
    }
    None
}

/// Komenda otwierająca okno terminala: TERMINAL_CMD (np. "konsole -e") albo auto-detect emulatora.
fn terminal_cmd_parts(cfg: &Config) -> Option<Vec<String>> {
    match &cfg.terminal_cmd {
        TerminalCmd::Disabled => None,
        TerminalCmd::Cmd(v) => Some(v.clone()),
        TerminalCmd::Auto => {
            // bez sesji graficznej (headless VPS) nie ma sensu otwierać okna
            if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
                return None;
            }
            for (bin, prefix) in [
                ("konsole", &["-e"][..]),
                ("gnome-terminal", &["--"][..]),
                ("kgx", &["-e"][..]),
                ("xterm", &["-e"][..]),
                ("alacritty", &["-e"][..]),
                ("wezterm", &["start", "--"][..]),
                ("foot", &[][..]),
                ("kitty", &[][..]),
            ] {
                if let Some(path) = which(bin) {
                    let mut v = vec![path];
                    v.extend(prefix.iter().map(|s| s.to_string()));
                    return Some(v);
                }
            }
            None
        }
    }
}

/// Otwiera widoczne okno terminala podpięte do sesji tmux (`tmux attach -t <sesja>`).
/// Sesja działa dalej headless mimo zamknięcia okna — Matrix widzi to samo co okno.
fn open_terminal_window(cfg: &Config, session: &str) {
    let Some(mut parts) = terminal_cmd_parts(cfg) else {
        tracing::info!(session = %session, "brak emulatora terminala (TERMINAL_CMD) — sesja tmux headless");
        return;
    };
    parts.extend(
        ["tmux", "attach", "-t", session]
            .iter()
            .map(|s| s.to_string()),
    );
    match std::process::Command::new(&parts[0])
        .args(&parts[1..])
        .env("TMUX_TMPDIR", &cfg.tmux_tmpdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            tracing::info!(session = %session, term = %parts[0], "otwarto okno terminala (tmux attach)")
        }
        Err(e) => {
            tracing::warn!(session = %session, "nie udało się otworzyć okna terminala: {e:#}")
        }
    }
}

fn kill_session(cfg: &Config, agent: &str) {
    let t = tmux_target(&tmux_session(cfg, agent));
    let _ = tmux(cfg, &["kill-session", "-t", t.as_str()]);
}

/// Wysyła tekst do sesji tmux (literal + Enter).
fn tmux_send(cfg: &Config, session: &str, text: &str) -> Result<()> {
    let t = tmux_target(session);
    tmux(cfg, &["send-keys", "-t", t.as_str(), "-l", text])?;
    tmux(cfg, &["send-keys", "-t", t.as_str(), "Enter"])?;
    Ok(())
}

/// Capture ostatnich linii ekranu (cała historia do -300).
fn tmux_capture(cfg: &Config, session: &str) -> Result<String> {
    let t = tmux_target(session);
    tmux(cfg, &["capture-pane", "-p", "-t", t.as_str(), "-S", "-300"])
}

/// Czeka na odpowiedź agenta: poll capture co 2s; koniec = 3 kolejne próbki bez zmian; max timeout.
async fn wait_reply(cfg: &Config, session: &str, prev: &str, timeout_secs: u64) -> String {
    let mut last = prev.to_string();
    let mut stable = 0;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let cur = match tmux_capture(cfg, session) {
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
    std::fs::create_dir_all(&cfg.tmux_tmpdir).context("create tmux_tmpdir")?;

    let client = MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?;
    // watcher: pliki z voice/out/ -> m.audio na Matrix (tryb "konektor": terminal odpowiada głosem)
    {
        let cfg2 = cfg.clone();
        let client2 = client.clone();
        tokio::spawn(async move { voice_out_watcher(cfg2, client2).await });
    }
    let mut seen: HashSet<String> = load_seen(&cfg.state_file);
    let mut agent: String = load_str_opt(&cfg.agent_file).unwrap_or_else(|| "vibe".to_string());
    if agent_cmd(&agent).is_none() {
        tracing::warn!(agent = %agent, "agent z AGENT_FILE nieznany — reset do `vibe`");
        agent = "vibe".to_string();
    }
    let mut voice_on: bool = load_str_opt(&cfg.voice_file)
        .map(|v| v == "on")
        .unwrap_or(false);
    let mut sender_hits: HashMap<String, VecDeque<Instant>> = HashMap::new();
    let mut since: Option<String> = None;
    let started = Instant::now();

    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(resp) => {
                since = Some(resp.next_batch.clone());
                let mut seen_dirty = false;
                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    for ev in &room.timeline.events {
                        if let Err(e) = handle_event(
                            &cfg,
                            &client,
                            ev,
                            &mut seen,
                            &mut seen_dirty,
                            &mut agent,
                            &mut voice_on,
                            &mut sender_hits,
                            started,
                        )
                        .await
                        {
                            tracing::error!(event = %ev.id, "błąd obsługi eventu: {e:#}");
                        }
                    }
                }
                if seen_dirty {
                    if let Err(e) = save_seen(&cfg.state_file, &seen) {
                        tracing::warn!("nie udało się zapisać stanu: {e:#}");
                    }
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
    seen_dirty: &mut bool,
    agent: &mut String,
    voice_on: &mut bool,
    sender_hits: &mut HashMap<String, VecDeque<Instant>>,
    started: Instant,
) -> Result<()> {
    if ev.kind != "m.room.message" {
        return Ok(());
    }
    if ev.content.msgtype != "m.text" && ev.content.msgtype != "m.audio" {
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
    *seen_dirty = true;
    if seen.len() > cfg.max_seen_events {
        tracing::warn!(
            len = seen.len(),
            max = cfg.max_seen_events,
            "przekroczono MAX_SEEN_EVENTS — czyszczę zbiór seen"
        );
        seen.clear();
        seen.insert(ev.id.clone());
    }

    // inbound rate limit per nadawca (sliding window 60s) — chroni też m.audio i zapobiega
    // zalaniu pętli sync przez allowlistowanego nadawcę
    let now = Instant::now();
    let hits = sender_hits.entry(ev.sender.clone()).or_default();
    hits.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if hits.len() >= cfg.inbound_max_per_min as usize {
        tracing::warn!(
            sender = %ev.sender,
            "inbound rate limit ({}/min) — pomijam",
            cfg.inbound_max_per_min
        );
        client
            .send_text("⏳ Zwolnij — limit wiadomości na minutę został przekroczony.")
            .await
            .ok();
        client.react(&ev.id, "❌").await.ok();
        return Ok(());
    }
    hits.push_back(now);

    if ev.content.msgtype == "m.audio" {
        return handle_voice(cfg, client, ev, agent, voice_on).await;
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
                        remove_state_file(&session_file_for(cfg, agent));
                        format!("✅ Nowa sesja agenta `{agent}` (tmux)")
                    }
                } else {
                    kill_session(cfg, agent);
                    remove_state_file(&session_file_for(cfg, agent));
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
            BotCommand::Attach => match ensure_session(cfg, agent).await {
                Ok(s) => match tmux_capture(cfg, &s) {
                    Ok(c) => {
                        let lines: Vec<&str> = c.lines().collect();
                        let start = lines.len().saturating_sub(60);
                        format!(
                            "📎 Ostatnie linie `{s}`:\n```\n{}\n```",
                            lines[start..].join("\n")
                        )
                    }
                    Err(e) => format!("⚠️ capture-pane: {e:#}"),
                },
                Err(e) => format!("⚠️ tmux: {e:#}"),
            },
            BotCommand::Window => match ensure_session(cfg, agent).await {
                Ok(s) => {
                    open_terminal_window(cfg, &s);
                    format!("🪟 Otwieram okno terminala dla sesji `{s}` (jeśli jest emulator; inaczej headless)")
                }
                Err(e) => format!("⚠️ tmux: {e:#}"),
            },
            BotCommand::Stop => {
                kill_session(cfg, agent);
                "🛑 Sesja tmux zabita.".to_string()
            }
            BotCommand::Status => {
                let s = tmux_session(cfg, agent);
                let alive = tmux_has_session(cfg, &s);
                format!(
                    "📊 Status:\n• pokój: {}\n• agent: {agent} (tmux `{s}`, alive={alive})\n• voice: {}\n• homeserver: {}\n• seen: {} eventów\n• uptime: {:?}",
                    cfg.room_id, if *voice_on { "ON" } else { "OFF" }, cfg.homeserver, seen.len(), started.elapsed()
                )
            }
            BotCommand::Voice(arg) => match arg.as_deref() {
                None => format!(
                    "🎙️ Tryb voice: {}",
                    if *voice_on {
                        "ON (odpowiedzi głosem)"
                    } else {
                        "OFF"
                    }
                ),
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
        let sent = client
            .send_text_rel(&reply, Some(thread_rel(&ev.id)))
            .await?;
        tracing::info!(reply_event = %sent, "odpowiedź na komendę");
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }

    // zwykła wiadomość (lub /komenda agenta typu /model) -> agent
    match run_agent(cfg, agent, body).await {
        Ok(text) => {
            let clean = clean_tui(&text);
            let reply = if clean.is_empty() {
                "(agent zwrócił pustą odpowiedź — /attach żeby zobaczyć ekran)".to_string()
            } else if clean.len() > 3800 {
                format!(
                    "{}…\n\n_(obcięte do 3800 znaków — /attach po pełny ekran)_",
                    safe_truncate(&clean, 3800)
                )
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
    /// headless `AGENT -p --resume <sid>` — tylko jako opt-in (AGENT_MODE=subprocess)
    Subprocess,
    /// DOMYŚLNY: agent (vibe/codex/claude/agy) żyje jako pełne TUI w sesji tmux
    /// <TMUX_PREFIX>-<agent> — send-keys + capture-pane; można się podpiąć `tmux attach`
    /// i pisać ręcznie w tym samym oknie co Matrix. To główny tryb mostka.
    Tmux,
}

fn agent_mode(agent: &str) -> AgentMode {
    let env_mode = std::env::var("AGENT_MODE").ok();
    agent_mode_with(agent, env_mode.as_deref())
}

fn agent_mode_with(agent: &str, env_mode: Option<&str>) -> AgentMode {
    // AGENT_MODE=tmux|subprocess wymusza tryb dla wszystkich agentów;
    // default (bez zmiennej): WSZYSCY agenci w tmux — pełne okno terminala z natywnym TUI
    if let Some(m) = env_mode {
        match m.to_ascii_lowercase().as_str() {
            "tmux" => return AgentMode::Tmux,
            "subprocess" | "oneshot" => return AgentMode::Subprocess,
            _ => {}
        }
    }
    let _ = agent;
    AgentMode::Tmux
}

/// Dispatch per agent.
async fn run_agent(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    match agent_mode(agent) {
        AgentMode::Subprocess => run_agent_subprocess(cfg, agent, prompt).await,
        AgentMode::Tmux => run_agent_tmux(cfg, agent, prompt).await,
    }
}

/// Plik z session id agenta (subprocess mode), per agent.
fn session_file_for(cfg: &Config, agent: &str) -> String {
    format!("{}.{}", cfg.session_file, agent)
}

/// Subprocess tryb: `AGENT -p <prompt> --yolo --output json [--resume <sid>]`; sesja per agent w pliku.
/// Gdy zapisany sid jest martwy (np. `vibe --resume` exit 1 "Session not found"), czyścimy go
/// i próbujemy raz jeszcze bez `--resume` — mostek sam wraca do życia zamiast błędować w kółko.
async fn run_agent_subprocess(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    let sess_file = session_file_for(cfg, agent);
    let sid = load_str_opt(&sess_file);
    let mut out = run_agent_once(cfg, agent, prompt, sid.as_deref()).await?;
    if !out.status.success() && sid.is_some() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            agent = %agent,
            sid = %sid.as_deref().unwrap_or(""),
            "run z --resume nieudany (exit {:?}) — czyszczę sid i retry bez --resume: {}",
            out.status.code(),
            &stderr[..stderr.len().min(200)]
        );
        remove_state_file(&sess_file);
        out = run_agent_once(cfg, agent, prompt, None).await?;
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{agent} exit {:?}: {}",
            out.status.code(),
            &stderr[..stderr.len().min(500)]
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let (mut new_sid, text) = parse_agent_json(&stdout);
    if new_sid.is_empty() {
        new_sid = discover_session_id(cfg, agent);
    }
    if !new_sid.is_empty() {
        let _ = save_str(&sess_file, &new_sid);
    }
    Ok(if text.is_empty() { stdout } else { text })
}

/// Jedno wywołanie agenta headless: `AGENT -p <prompt> --yolo --output json [--resume <sid>]`.
async fn run_agent_once(
    cfg: &Config,
    agent: &str,
    prompt: &str,
    sid: Option<&str>,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(agent_cmd(agent).context("nieznany agent")?);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--yolo")
        .arg("--output")
        .arg("json");
    if let Some(s) = sid {
        cmd.arg("--resume").arg(s);
    }
    let agent_path = std::env::var("AGENT_PATH")
        .or_else(|_| std::env::var("PATH"))
        .unwrap_or_else(|_| {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
        });
    cmd.current_dir(&cfg.vibe_workdir)
        .env_clear()
        .env("PATH", agent_path)
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
    let out = tokio::time::timeout(
        Duration::from_secs(cfg.vibe_timeout_secs),
        child.wait_with_output(),
    )
    .await
    .context("timeout agenta")?
    .context("wait agenta")?;
    Ok(out)
}

/// vibe 2.x nie wystawia `sessionId` w `--output json` — bierzemy najnowszą sesję
/// z katalogu `$AGENT_HOME/.vibe/logs/session/*/meta.json` pasującą do VIBE_WORKDIR
/// (taki sam store, którego używa `vibe -c`). Best-effort: jak layout się zmieni, zwracamy "".
fn discover_session_id(cfg: &Config, agent: &str) -> String {
    if agent.to_ascii_lowercase() != "vibe" {
        return String::new();
    }
    let dir = format!("{}/.vibe/logs/session", cfg.agent_home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    let workdir = std::fs::canonicalize(&cfg.vibe_workdir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cfg.vibe_workdir.clone());
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in rd.flatten() {
        let Ok(meta) = std::fs::read_to_string(e.path().join("meta.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&meta) else {
            continue;
        };
        let wd = v
            .get("environment")
            .and_then(|e| e.get("working_directory"))
            .and_then(|w| w.as_str())
            .unwrap_or("");
        let wd = std::fs::canonicalize(wd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| wd.to_string());
        if wd != workdir {
            continue;
        }
        let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) else {
            continue;
        };
        let mtime = std::fs::metadata(e.path().join("messages.jsonl"))
            .and_then(|m| m.modified())
            .ok()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, sid.to_string()));
        }
    }
    best.map(|(_, s)| s).unwrap_or_default()
}

/// Wysyła prompt do sesji tmux agenta i czeka na odpowiedź (capture-pane). Zwraca nowy tekst ekranu.
async fn run_agent_tmux(cfg: &Config, agent: &str, prompt: &str) -> Result<String> {
    let session = ensure_session(cfg, agent).await?;
    let prev = tmux_capture(cfg, &session).unwrap_or_default();
    tmux_send(cfg, &session, prompt)?;
    let reply = wait_reply(cfg, &session, &prev, cfg.vibe_timeout_secs).await;
    if !tmux_has_session(cfg, &session) {
        anyhow::bail!("sesja tmux `{session}` zniknęła w trakcie odpowiedzi");
    }
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
            if !(name.ends_with(".opus")
                || name.ends_with(".ogg")
                || name.ends_with(".wav")
                || name.ends_with(".mp3"))
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(e.path()) else {
                continue;
            };
            let mime = if name.ends_with(".wav") {
                "audio/wav"
            } else {
                "audio/ogg"
            };
            match client.upload_media(&bytes, &name, mime).await {
                Ok(mxc) => {
                    let dur = (bytes.len() / 4000) as u32 * 10; // estymata
                    let wf = pseudo_waveform(bytes.len());
                    match client
                        .send_audio(
                            &format!("🎙️ {} (odpowiedź głosowa)", name),
                            &mxc,
                            mime,
                            bytes.len(),
                            dur.max(500),
                            &wf,
                        )
                        .await
                    {
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

async fn send_reply(
    client: &MatrixClient,
    cfg: &Config,
    text: &str,
    voice_on: bool,
    thread_root: Option<&str>,
) -> Result<String> {
    let rel = thread_root.map(thread_rel);
    if voice_on && std::env::var("MISTRAL_API_KEY").is_ok() {
        match tts(cfg, text).await {
            Ok((bytes, dur)) => {
                let mxc = client
                    .upload_media(&bytes, "reply.opus", "audio/ogg")
                    .await?;
                let wf = pseudo_waveform(text.len());
                let body = format!("🎙️ {}", safe_truncate(text, 200));
                // UWAGA: ten Synapse odrzuca m.audio z m.relates_to ("Cannot start threads from an event with a relation")
                // więc voice odpowiedź idzie jako zwykła wiadomość (bez wątku); tekst może być w wątku.
                return client
                    .send_audio_rel(&body, &mxc, "audio/ogg", bytes.len(), dur, &wf, None)
                    .await;
            }
            Err(e) => tracing::warn!("TTS nieudane, wysyłam tekst: {e:#}"),
        }
    }
    client.send_text_rel(text, rel).await
}

/// m.audio -> download -> STT -> agent -> odpowiedź (głosem jeśli voice_on).
async fn handle_voice(
    cfg: &Config,
    client: &MatrixClient,
    ev: &Event,
    agent: &mut String,
    voice_on: &mut bool,
) -> Result<()> {
    if ev.content.file.is_some() {
        client
            .send_text("⚠️ zaszyfrowane media (E2EE) nie są obsługiwane w MVP")
            .await
            .ok();
        return Ok(());
    }
    let mxc = ev.content.url.clone().context("m.audio bez url")?;
    client.react(&ev.id, "👀").await.ok();
    let (fname, bytes, mime) = match client.download_mxc(&mxc).await {
        Ok(t) => t,
        Err(e) => {
            client
                .send_text(&format!("⚠️ nie pobrałem audio: {e:#}"))
                .await
                .ok();
            return Ok(());
        }
    };
    std::fs::create_dir_all(&cfg.uploads_dir).ok();
    let safe_name: String = fname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
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
        if answered {
            break;
        }
    }
    if answered {
        client.react(&ev.id, "✅").await.ok();
        return Ok(());
    }
    tracing::warn!(
        "brak odpowiedzi z voice/out/ po {}s — fallback API (STT->agent->TTS)",
        cfg.voice_fallback_secs
    );

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
            let clean = clean_tui(&reply);
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
            client
                .send_text(&format!("⚠️ błąd agenta: {e:#}"))
                .await
                .ok();
            client.react(&ev.id, "❌").await.ok();
        }
    }
    Ok(())
}

/// STT przez Mistral /v1/audio/transcriptions.
async fn stt(cfg: &Config, audio: &[u8], filename: &str, mime: &str) -> Result<String> {
    let key =
        std::env::var("MISTRAL_API_KEY").context("brak MISTRAL_API_KEY — voice niedostępne")?;
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
    let b64 = v["audio_data"]
        .as_str()
        .context("TTS: brak audio_data w odpowiedzi")?;
    let bytes = STANDARD.decode(b64).context("TTS: base64 decode")?;
    let duration_ms = ((text.split_whitespace().count() as f64 / 150.0) * 60_000.0) as u32;
    Ok((bytes, duration_ms.max(500)))
}

/// Prosta pseudo-fala (sinusoida) do voice message.
fn pseudo_waveform(_text_len: usize) -> Vec<u32> {
    (0..40)
        .map(|i| (300.0 + (i as f32 * 0.7).sin() * 200.0) as u32)
        .collect()
}

/// Parsuje stdout agenta w `--output json`. Obsługiwane formaty:
/// - tablica wiadomości (vibe 2.x: `[{role, content}, ...]`, content = string albo lista bloków)
/// - pojedynczy obiekt / NDJSON po linii (vibe 3.x: wpisy historii z `sessionId` i zagnieżdżonym `message`)
/// Zwraca (session_id, tekst ostatniej odpowiedzi assistant).
fn parse_agent_json(s: &str) -> (String, String) {
    let mut sid = String::new();
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        collect_agent_json(v, &mut sid, &mut messages);
    } else {
        for line in s.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                collect_agent_json(v, &mut sid, &mut messages);
            }
        }
    }
    let mut text = String::new();
    for m in messages.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            text = content_text(m.get("content"));
            if !text.is_empty() {
                break;
            }
        }
    }
    (sid, text)
}

fn collect_agent_json(
    v: serde_json::Value,
    sid: &mut String,
    messages: &mut Vec<serde_json::Value>,
) {
    match v {
        serde_json::Value::Array(arr) => {
            for e in arr {
                collect_agent_json(e, sid, messages);
            }
        }
        serde_json::Value::Object(map) => {
            for key in ["sessionId", "session_id"] {
                if sid.is_empty() {
                    if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                        sid.push_str(s);
                    }
                }
            }
            // vibe 3.x: wpis historii {type: "message", sessionId, message: {role, content}}
            if let Some(msg) = map.get("message") {
                if msg.get("role").is_some() {
                    messages.push(msg.clone());
                    return;
                }
            }
            if map.get("role").is_some() {
                messages.push(serde_json::Value::Object(map));
            }
        }
        _ => {}
    }
}

/// Tekst z pola `content`: string albo lista bloków `{type: "text"|"output_text", text}`.
fn content_text(c: Option<&serde_json::Value>) -> String {
    match c {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut t = String::new();
            for b in arr {
                if matches!(
                    b.get("type").and_then(|x| x.as_str()),
                    Some("text") | Some("output_text")
                ) {
                    if let Some(x) = b.get("text").and_then(|x| x.as_str()) {
                        t.push_str(x);
                    }
                }
            }
            t
        }
        _ => String::new(),
    }
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
        assert_eq!(
            parse_command("/agent agy"),
            Some(BotCommand::Agent("agy".to_string()))
        );
        assert_eq!(parse_command("/attach"), Some(BotCommand::Attach));
        assert_eq!(parse_command("/window"), Some(BotCommand::Window));
        assert_eq!(parse_command("/stop"), Some(BotCommand::Stop));
        assert_eq!(parse_command("/status"), Some(BotCommand::Status));
        assert_eq!(
            parse_command("/voice on"),
            Some(BotCommand::Voice(Some("on".to_string())))
        );
        assert_eq!(parse_command("/voice"), Some(BotCommand::Voice(None)));
        assert_eq!(
            parse_command("/voice new"),
            Some(BotCommand::Voice(Some("new".to_string())))
        );
        assert_eq!(parse_command("/help"), Some(BotCommand::Help));
        // komendy agenta (NIE bridge'a) -> None = lecą do tmuxa
        assert_eq!(parse_command("/model mistral-large-latest"), None);
        assert_eq!(parse_command("/think high"), None);
        assert_eq!(parse_command("zwykły tekst"), None);
    }

    #[test]
    fn agent_cmds() {
        assert_eq!(agent_cmd("vibe").as_deref(), Some("vibe"));
        assert_eq!(agent_cmd("codex").as_deref(), Some("codex"));
        assert_eq!(agent_cmd("claude").as_deref(), Some("claude"));
        // AGY_CMD nieustawione w testach -> domyślne "agy" z PATH
        assert_eq!(agent_cmd("AGY").as_deref(), Some("agy"));
        assert_eq!(agent_cmd("nieznany"), None);
    }

    #[test]
    fn parse_agent_json_vibe2_array_string_content() {
        // vibe 2.x --output json: tablica LLMMessage, content jako string, bez sessionId
        let s = r#"[
            {"role": "user", "content": "cześć"},
            {"role": "assistant", "content": "Cześć! W czym pomóc?"}
        ]"#;
        let (sid, text) = parse_agent_json(s);
        assert_eq!(sid, "");
        assert_eq!(text, "Cześć! W czym pomóc?");
    }

    #[test]
    fn parse_agent_json_vibe2_block_content_last_assistant() {
        let s = r#"[
            {"role": "assistant", "content": [{"type": "text", "text": "pierwsza odpowiedź"}]},
            {"role": "user", "content": "a teraz?"},
            {"role": "assistant", "content": [{"type": "text", "text": "druga "}, {"type": "text", "text": "odpowiedź"}]}
        ]"#;
        let (_, text) = parse_agent_json(s);
        assert_eq!(text, "druga odpowiedź");
    }

    #[test]
    fn parse_agent_json_vibe3_entries_with_session_id() {
        // vibe 3.x: wpisy historii (camelCase) z sessionId i zagnieżdżonym message
        let s = r#"[
            {"type": "message", "sessionId": "abc123-xyz", "message": {"role": "user", "content": "hej"}},
            {"type": "message", "sessionId": "abc123-xyz", "message": {"role": "assistant", "content": [{"type": "text", "text": "OK"}]}}
        ]"#;
        let (sid, text) = parse_agent_json(s);
        assert_eq!(sid, "abc123-xyz");
        assert_eq!(text, "OK");
    }

    #[test]
    fn parse_agent_json_ndjson_streaming() {
        let s = "{\"type\":\"message\",\"session_id\":\"sid-1\",\"message\":{\"role\":\"assistant\",\"content\":\"stream odpowiedź\"}}\n{\"type\":\"notice\"}";
        let (sid, text) = parse_agent_json(s);
        assert_eq!(sid, "sid-1");
        assert_eq!(text, "stream odpowiedź");
    }

    #[test]
    fn parse_agent_json_garbage() {
        let (sid, text) = parse_agent_json("nie-json");
        assert_eq!(sid, "");
        assert_eq!(text, "");
    }

    #[test]
    fn agent_mode_defaults_to_tmux_for_all_agents() {
        // główny tryb mostka: pełne okno TUI w tmux dla każdego natywnego agenta
        for a in ["vibe", "codex", "claude", "agy", "antigravity"] {
            assert_eq!(agent_mode_with(a, None), AgentMode::Tmux, "agent {a}");
            assert_eq!(agent_mode_with(a, Some("")), AgentMode::Tmux, "agent {a}");
            assert_eq!(agent_mode_with(a, Some("tmux")), AgentMode::Tmux);
        }
        // subprocess tylko jako opt-in
        assert_eq!(
            agent_mode_with("vibe", Some("subprocess")),
            AgentMode::Subprocess
        );
        assert_eq!(
            agent_mode_with("agy", Some("oneshot")),
            AgentMode::Subprocess
        );
        // nieznana wartość -> też tmux (default)
        assert_eq!(agent_mode_with("vibe", Some("cos-innego")), AgentMode::Tmux);
    }
}
