//! bridge-headless: odporny, nieblokujący mostek Matrix <-> agent CLI (vibe/agy/claude/codex)
//! z trwałym kontekstem sesji (--resume / --conversation), kolejką FIFO, typing indicator,
//! izolacją grupy procesów, komendami sterującymi (/new, /stop, /status, /help) i graceful shutdown.
use anyhow::{Context, Result};
use bridge_common::{
    parse_agent_output, redact, split_message, truncate_utf8, AgentKind, BridgeState, Config,
};
use bridge_core::{Event, MatrixClient};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, watch, Mutex};

const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 256 * 1024;
const QUEUE_CAPACITY: usize = 32;

const SAFE_PASSTHROUGH_ENV: &[&str] = &[
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

#[derive(Debug, Clone)]
struct Job {
    event_id: String,
    sender: String,
    prompt: String,
}

struct RuntimeStatus {
    started_at: Instant,
    busy: AtomicBool,
    queued: AtomicUsize,
}

#[tokio::main]
async fn main() -> Result<()> {
    bridge_common::init_logging();
    let cfg = Arc::new(Config::from_env().context("config z env")?);
    let kind = cfg.agent_kind()?;
    let secrets = Arc::new(cfg.secrets());

    if cfg.allowed_users.is_empty() {
        tracing::warn!("MATRIX_ALLOWED_USERS jest puste — odpowiadam wszystkim uczestnikom pokoju");
    }

    let state = Arc::new(Mutex::new(BridgeState::load(
        &cfg.state_file,
        cfg.max_seen_events,
    )));
    {
        let st = state.lock().await;
        tracing::info!(
            agent = %kind.as_str(),
            room = %cfg.room_id,
            homeserver = %cfg.homeserver,
            workdir = %cfg.agent_workdir,
            state_file = %st.json_path().display(),
            has_session = st.session_id.is_some(),
            has_next_batch = st.next_batch.is_some(),
            "vibe-matrix-rs headless start"
        );
    }

    let client = Arc::new(MatrixClient::new(&cfg.homeserver, &cfg.token, &cfg.room_id)?);
    let status = Arc::new(RuntimeStatus {
        started_at: Instant::now(),
        busy: AtomicBool::new(false),
        queued: AtomicUsize::new(0),
    });

    // Kanał wyłączenia (SIGTERM / SIGINT)
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Kanał anulowania bieżącego jobu (/stop lub /new) — inkrementowany numer generacji
    let (cancel_tx, cancel_rx) = watch::channel(0u64);
    let (job_tx, job_rx) = mpsc::channel::<Job>(QUEUE_CAPACITY);

    let worker_handle = tokio::spawn(run_worker(
        Arc::clone(&cfg),
        kind,
        Arc::clone(&secrets),
        Arc::clone(&client),
        Arc::clone(&state),
        Arc::clone(&status),
        job_rx,
        cancel_rx,
        shutdown_rx.clone(),
    ));

    let sync_handle = tokio::spawn(run_sync_loop(
        Arc::clone(&cfg),
        kind,
        Arc::clone(&client),
        Arc::clone(&state),
        Arc::clone(&status),
        job_tx,
        cancel_tx,
        shutdown_rx,
    ));

    wait_for_shutdown_signal().await;
    tracing::info!("otrzymano sygnał zakończenia — zamykam mostek czysto...");
    let _ = shutdown_tx.send(true);

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = sync_handle.await;
        let _ = worker_handle.await;
    })
    .await;

    if let Err(e) = state.lock().await.save() {
        tracing::warn!("nie udało się zapisać stanu przy zamknięciu: {e:#}");
    }
    tracing::info!("vibe-matrix-rs zakończony czysto");
    Ok(())
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_sync_loop(
    cfg: Arc<Config>,
    kind: AgentKind,
    client: Arc<MatrixClient>,
    state: Arc<Mutex<BridgeState>>,
    status: Arc<RuntimeStatus>,
    job_tx: mpsc::Sender<Job>,
    cancel_tx: watch::Sender<u64>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut backoff_secs = 2u64;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        let since = state.lock().await.next_batch.clone();
        let is_initial_sync = since.is_none();

        let sync_res = tokio::select! {
            res = client.sync(since.as_deref(), cfg.sync_timeout_ms) => res,
            _ = shutdown_rx.changed() => break,
        };

        match sync_res {
            Ok(resp) => {
                backoff_secs = 2;
                let mut state_dirty = false;

                if !resp.next_batch.is_empty() {
                    let mut st = state.lock().await;
                    if st.next_batch.as_deref() != Some(resp.next_batch.as_str()) {
                        st.next_batch = Some(resp.next_batch.clone());
                        state_dirty = true;
                    }
                }

                if let Some(room) = resp.rooms.join.get(&cfg.room_id) {
                    if is_initial_sync {
                        // Pierwszy sync bez next_batch: zapisujemy istniejące eventy jako seen bez replayu starej historii
                        let mut st = state.lock().await;
                        for ev in &room.timeline.events {
                            if st.mark_seen(ev.id.clone()) {
                                state_dirty = true;
                            }
                        }
                        tracing::info!(
                            skipped = room.timeline.events.len(),
                            "initial sync zakończony — pominięto historię, nasłuchuję nowych wiadomości"
                        );
                    } else {
                        for ev in &room.timeline.events {
                            if handle_incoming_event(
                                &cfg,
                                kind,
                                &client,
                                &state,
                                &status,
                                &job_tx,
                                &cancel_tx,
                                ev,
                            )
                            .await
                            {
                                state_dirty = true;
                            }
                        }
                    }
                }

                if state_dirty {
                    if let Err(e) = state.lock().await.save() {
                        tracing::warn!("nie udało się zapisać stanu: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(backoff_secs, "sync error: {e:#} — retry");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                    _ = shutdown_rx.changed() => break,
                }
                backoff_secs = (backoff_secs * 2).min(30);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming_event(
    cfg: &Config,
    kind: AgentKind,
    client: &MatrixClient,
    state: &Arc<Mutex<BridgeState>>,
    status: &Arc<RuntimeStatus>,
    job_tx: &mpsc::Sender<Job>,
    cancel_tx: &watch::Sender<u64>,
    ev: &Event,
) -> bool {
    if ev.kind != "m.room.message" || ev.content.msgtype != "m.text" || ev.is_edit() {
        return false;
    }
    if ev.sender == cfg.bot_user_id {
        return false;
    }
    if !cfg.allowed_users.is_empty() && !cfg.allowed_users.contains(&ev.sender) {
        tracing::debug!(sender = %ev.sender, "pomijam (poza allowlist)");
        return false;
    }

    let is_new = {
        let mut st = state.lock().await;
        st.mark_seen(ev.id.clone())
    };
    if !is_new {
        return false;
    }

    let body = ev.content.body.trim();
    if body.is_empty() {
        return true;
    }

    // Komendy sterujące mostkiem działają NATYCHMIAST (nawet gdy agent jest zajęty)
    if body.starts_with('/') {
        let cmd_word = body.split_whitespace().next().unwrap_or("");
        match cmd_word {
            "/help" => {
                let help = format!(
                    "🤖 Mostek `{}` — komendy sterujące:\n\
                     • `/status` — stan agenta, aktywna sesja i kolejka\n\
                     • `/new` lub `/reset` — wyczyść kontekst i zacznij nową rozmowę\n\
                     • `/stop` lub `/cancel` — przerwij bieżące wywołanie agenta\n\
                     • `/help` — ta pomoc\n\n\
                     Każda inna wiadomość automatycznie kontynuuje kontekst rozmowy.",
                    kind.as_str()
                );
                let _ = client.send_text(&help).await;
                let _ = client.react(&ev.id, "✅").await;
                return true;
            }
            "/status" => {
                let (sid, turns) = {
                    let st = state.lock().await;
                    (
                        st.session_id
                            .clone()
                            .unwrap_or_else(|| "(brak — nowa sesja przy kolejnej wiadomości)".into()),
                        st.turn_count,
                    )
                };
                let busy = status.busy.load(Ordering::Relaxed);
                let queued = status.queued.load(Ordering::Relaxed);
                let up = status.started_at.elapsed().as_secs();
                let msg = format!(
                    "📊 Status `{}`:\n• stan: {}\n• w kolejce: {}\n• tury w sesji: {}\n• session_id: `{}`\n• workdir: `{}`\n• uptime: {}s",
                    kind.as_str(),
                    if busy { "pracuje ⏳" } else { "gotowy ✅" },
                    queued,
                    turns,
                    sid,
                    cfg.agent_workdir,
                    up
                );
                let _ = client.send_text(&msg).await;
                let _ = client.react(&ev.id, "✅").await;
                return true;
            }
            "/new" | "/reset" => {
                let next_gen = *cancel_tx.borrow() + 1;
                let _ = cancel_tx.send(next_gen);
                {
                    let mut st = state.lock().await;
                    st.clear_session();
                    let _ = st.save();
                }
                let _ = client
                    .send_text("🆕 Sesja wyczyszczona. Następna wiadomość zacznie nowy kontekst.")
                    .await;
                let _ = client.react(&ev.id, "✅").await;
                return true;
            }
            "/stop" | "/cancel" => {
                let next_gen = *cancel_tx.borrow() + 1;
                let _ = cancel_tx.send(next_gen);
                let was_busy = status.busy.load(Ordering::Relaxed);
                let msg = if was_busy {
                    "🛑 Przerywam bieżące zadanie agenta."
                } else {
                    "ℹ️ Agent aktualnie nie wykonuje żadnego zadania."
                };
                let _ = client.send_text(msg).await;
                let _ = client.react(&ev.id, "🛑").await;
                return true;
            }
            _ => {
                // Nieznane /komendy NIE są wysyłane do agenta jako prompt
                let _ = client
                    .send_text(&format!(
                        "❓ Nieznana komenda `{cmd_word}`. Wpisz `/help`, aby zobaczyć dostępne komendy."
                    ))
                    .await;
                let _ = client.react(&ev.id, "❓").await;
                return true;
            }
        }
    }

    tracing::info!(
        event = %ev.id,
        sender = %ev.sender,
        chars = body.chars().count(),
        "nowa wiadomość — dodaję do kolejki"
    );
    let _ = client.react(&ev.id, "👀").await;

    let job = Job {
        event_id: ev.id.clone(),
        sender: ev.sender.clone(),
        prompt: body.to_string(),
    };
    match job_tx.try_send(job) {
        Ok(()) => {
            status.queued.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            let _ = client
                .send_text("⚠️ Kolejka wiadomości jest pełna — poczekaj aż bieżące zadania się skończą.")
                .await;
            let _ = client.react(&ev.id, "❌").await;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    cfg: Arc<Config>,
    kind: AgentKind,
    secrets: Arc<Vec<String>>,
    client: Arc<MatrixClient>,
    state: Arc<Mutex<BridgeState>>,
    status: Arc<RuntimeStatus>,
    mut job_rx: mpsc::Receiver<Job>,
    mut cancel_rx: watch::Receiver<u64>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            maybe_job = job_rx.recv() => match maybe_job {
                Some(j) => j,
                None => break,
            },
            _ = shutdown_rx.changed() => break,
        };

        status.queued.fetch_sub(1, Ordering::Relaxed);
        status.busy.store(true, Ordering::Relaxed);

        let typing_stop = Arc::new(AtomicBool::new(false));
        let typing_task = spawn_typing_heartbeat(
            Arc::clone(&client),
            cfg.bot_user_id.clone(),
            Arc::clone(&typing_stop),
        );

        let initial_cancel_gen = *cancel_rx.borrow();
        let current_session = state.lock().await.session_id.clone();

        let outcome = execute_with_recovery(
            &cfg,
            kind,
            &job.prompt,
            current_session.as_deref(),
            &mut cancel_rx,
            initial_cancel_gen,
            &mut shutdown_rx,
        )
        .await;

        typing_stop.store(true, Ordering::Relaxed);
        typing_task.abort();
        client.set_typing(&cfg.bot_user_id, false, 0).await;
        status.busy.store(false, Ordering::Relaxed);

        match outcome {
            RunOutcome::Success { text, session_id, reset_notice } => {
                {
                    let mut st = state.lock().await;
                    st.record_turn(session_id);
                    if let Err(e) = st.save() {
                        tracing::warn!("nie udało się zapisać session_id: {e:#}");
                    }
                }

                let mut final_text = redact(&text, &secrets);
                if final_text.trim().is_empty() {
                    final_text = format!("({} zwrócił pustą odpowiedź)", kind.as_str());
                }
                if reset_notice {
                    final_text = format!(
                        "_(Poprzednia sesja wygasła — rozpoczęto nową sesję)_\n\n{final_text}"
                    );
                }

                let chunks = split_message(&final_text, cfg.max_msg_chars);
                let mut send_ok = true;
                for chunk in chunks {
                    match client.send_text(&chunk).await {
                        Ok(reply_id) => {
                            tracing::info!(
                                reply_event = %reply_id,
                                sender = %job.sender,
                                "wysłano odpowiedź"
                            );
                        }
                        Err(e) => {
                            tracing::error!(event = %job.event_id, "nie udało się wysłać odpowiedzi: {e:#}");
                            send_ok = false;
                            break;
                        }
                    }
                }
                let _ = client
                    .react(&job.event_id, if send_ok { "✅" } else { "❌" })
                    .await;
            }
            RunOutcome::Cancelled => {
                tracing::info!(event = %job.event_id, "zadanie przerwane przez użytkownika");
                let _ = client.react(&job.event_id, "🛑").await;
            }
            RunOutcome::Shutdown => {
                tracing::info!(event = %job.event_id, "zadanie przerwane przez zamknięcie usługi");
                break;
            }
            RunOutcome::Failed(err_msg) => {
                let safe_err = redact(&err_msg, &secrets);
                let short_err = truncate_utf8(&safe_err, 1500);
                let msg = format!("⚠️ błąd agenta ({}): {short_err}", kind.as_str());
                let _ = client.send_text(&msg).await;
                let _ = client.react(&job.event_id, "❌").await;
            }
        }
    }
}

fn spawn_typing_heartbeat(
    client: Arc<MatrixClient>,
    bot_user_id: String,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !stop.load(Ordering::Relaxed) {
            client.set_typing(&bot_user_id, true, 30_000).await;
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    })
}

enum RunOutcome {
    Success {
        text: String,
        session_id: Option<String>,
        reset_notice: bool,
    },
    Cancelled,
    Shutdown,
    Failed(String),
}

async fn execute_with_recovery(
    cfg: &Config,
    kind: AgentKind,
    prompt: &str,
    session_id: Option<&str>,
    cancel_rx: &mut watch::Receiver<u64>,
    cancel_gen: u64,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> RunOutcome {
    let first = run_agent_once(cfg, kind, prompt, session_id, cancel_rx, cancel_gen, shutdown_rx).await;

    // Jeśli wywołanie z istniejącym session_id zawiodło (np. sesja usunięta/uszkodzona), spróbuj raz na czysto
    if let RunOutcome::Failed(ref err) = first {
        if session_id.is_some() && !is_rate_or_auth_error(err) {
            tracing::warn!(
                agent = %kind.as_str(),
                err = %truncate_utf8(err, 300),
                "resume sesji nie powiódł się — ponawiam w nowej sesji"
            );
            match run_agent_once(cfg, kind, prompt, None, cancel_rx, cancel_gen, shutdown_rx).await {
                RunOutcome::Success { text, session_id, .. } => {
                    return RunOutcome::Success {
                        text,
                        session_id,
                        reset_notice: true,
                    };
                }
                other => return other,
            }
        }
    }
    first
}

fn is_rate_or_auth_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    [
        "weekly limit",
        "usage limit",
        "out of usage credits",
        "rate limit",
        "429",
        "quota",
        "unauthorized",
        "401",
        "authentication",
        "not logged in",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

struct SubprocessOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_agent_once(
    cfg: &Config,
    kind: AgentKind,
    prompt: &str,
    session_id: Option<&str>,
    cancel_rx: &mut watch::Receiver<u64>,
    cancel_gen: u64,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> RunOutcome {
    let codex_tmp: Option<PathBuf> = if kind == AgentKind::Codex {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Some(std::env::temp_dir().join(format!("vibe_matrix_codex_{}_{}.txt", std::process::id(), nanos)))
    } else {
        None
    };

    let argv = match cfg.build_agent_argv(prompt, session_id, codex_tmp.as_deref()) {
        Ok(v) => v,
        Err(e) => return RunOutcome::Failed(format!("{e:#}")),
    };

    let res = spawn_and_drain(cfg, &argv, cancel_rx, cancel_gen, shutdown_rx).await;

    let codex_out_text = codex_tmp.as_ref().and_then(|p| {
        let txt = std::fs::read_to_string(p).ok();
        let _ = std::fs::remove_file(p);
        txt
    });

    match res {
        DrainResult::Completed(out) => {
            if !out.status.success() {
                let tail = |s: &str| {
                    let t = s.trim();
                    let chars: Vec<char> = t.chars().collect();
                    if chars.len() > 600 {
                        chars[chars.len() - 600..].iter().collect::<String>()
                    } else {
                        t.to_string()
                    }
                };
                return RunOutcome::Failed(format!(
                    "exit {:?}: {} {}",
                    out.status.code(),
                    tail(&out.stderr),
                    tail(&out.stdout)
                ));
            }

            let parsed = parse_agent_output(kind, &out.stdout, codex_out_text.as_deref());
            if parsed.is_error {
                return RunOutcome::Failed(parsed.text);
            }
            RunOutcome::Success {
                text: parsed.text,
                session_id: parsed.session_id.or_else(|| session_id.map(String::from)),
                reset_notice: false,
            }
        }
        DrainResult::TimedOut => RunOutcome::Failed(format!(
            "timeout po {}s (proces i jego potomkowie zostali zatrzymani)",
            cfg.agent_timeout_secs
        )),
        DrainResult::Cancelled => RunOutcome::Cancelled,
        DrainResult::Shutdown => RunOutcome::Shutdown,
        DrainResult::SpawnError(e) => RunOutcome::Failed(e),
    }
}

enum DrainResult {
    Completed(SubprocessOutput),
    TimedOut,
    Cancelled,
    Shutdown,
    SpawnError(String),
}

async fn spawn_and_drain(
    cfg: &Config,
    argv: &[String],
    cancel_rx: &mut watch::Receiver<u64>,
    cancel_gen: u64,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> DrainResult {
    let mut cmd = Command::new(&argv[0]);
    for a in &argv[1..] {
        cmd.arg(a);
    }

    // Izolacja środowiska: brak wycieku MATRIX_ACCESS_TOKEN, ale zachowujemy bezpieczne zmienne runtime
    cmd.env_clear()
        .env("PATH", &cfg.agent_path)
        .env("HOME", &cfg.agent_home)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CI", "1");

    for &key in SAFE_PASSTHROUGH_ENV {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    if std::env::var_os("LANG").is_none() {
        cmd.env("LANG", "C.UTF-8");
    }
    for key in &cfg.agent_env_keys {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }

    if std::path::Path::new(&cfg.agent_workdir).is_dir() {
        cmd.current_dir(&cfg.agent_workdir);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Osobna grupa procesów Unix — pozwala czysto zabić całe drzewo potomków przy timeout / /stop
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return DrainResult::SpawnError(format!("spawn {}: {e}", argv[0])),
    };

    let pid = child.id();

    // Równoległe, ograniczone czytanie stdout i stderr zapobiega zakleszczeniu na buforze pipe 64 KB
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        match stdout_pipe {
            Some(pipe) => read_capped(pipe, MAX_STDOUT_BYTES).await,
            None => Vec::new(),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr_pipe {
            Some(pipe) => read_capped(pipe, MAX_STDERR_BYTES).await,
            None => Vec::new(),
        }
    });

    let timeout_fut = tokio::time::sleep(Duration::from_secs(cfg.agent_timeout_secs));
    tokio::pin!(timeout_fut);

    let wait_outcome = tokio::select! {
        status_res = child.wait() => {
            match status_res {
                Ok(status) => Ok(status),
                Err(e) => Err(DrainResult::SpawnError(format!("wait {}: {e}", argv[0]))),
            }
        }
        _ = &mut timeout_fut => {
            kill_process_tree(pid, &mut child).await;
            Err(DrainResult::TimedOut)
        }
        _ = wait_cancel_bump(cancel_rx, cancel_gen) => {
            kill_process_tree(pid, &mut child).await;
            Err(DrainResult::Cancelled)
        }
        _ = shutdown_rx.changed() => {
            kill_process_tree(pid, &mut child).await;
            Err(DrainResult::Shutdown)
        }
    };

    match wait_outcome {
        Ok(status) => {
            let stdout_bytes = tokio::time::timeout(Duration::from_secs(5), stdout_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let stderr_bytes = tokio::time::timeout(Duration::from_secs(5), stderr_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            DrainResult::Completed(SubprocessOutput {
                status,
                stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
                stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
            })
        }
        Err(early) => {
            stdout_task.abort();
            stderr_task.abort();
            early
        }
    }
}

async fn wait_cancel_bump(cancel_rx: &mut watch::Receiver<u64>, initial_gen: u64) {
    loop {
        if *cancel_rx.borrow() != initial_gen {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < cap {
                    let take = n.min(cap - out.len());
                    out.extend_from_slice(&buf[..take]);
                }
                // Zawsze drenujemy do EOF, nawet po przekroczeniu limitu, aby dziecko nie zablokowało się na write()
            }
            Err(_) => break,
        }
    }
    out
}

async fn kill_process_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(p) = pid {
        let pgid = -(p as libc::pid_t);
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        let waited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        if waited.is_err() {
            unsafe {
                libc::kill(pgid, libc::SIGKILL);
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        return;
    }

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}
