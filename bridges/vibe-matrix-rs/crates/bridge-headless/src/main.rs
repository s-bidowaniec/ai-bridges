//! Zdalny dostęp przez Matrix do natywnych terminali projektu.
mod media;
mod terminal;
mod voice;

use anyhow::{bail, Context, Result};
use bridge_common::{load_str_opt, private_directory, safe_truncate, save_str, Config, SeenEvents};
use bridge_core::{Event, MatrixClient};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Semaphore};

const HELP: &str = "Terminal projektu
/new <nazwa> [vibe|codex|claude|agy] — uruchom nowy terminal
/sessions — lista terminali
/open <nazwa> — wróć do działającego terminala
/attach — pokaż ekran i włącz przekazywanie zmian
/window — otwórz okno terminala na monitorze dla bieżącej sesji
/detach — odłącz widok Matrix, zostaw terminal przy pracy
/key <Enter|Escape|Up|Down|Left|Right|Tab|C-c> — wyślij klawisz
/agent <vibe|codex|claude|agy> — wybierz domyślnego agenta
/voice — pokaż tryb obsługi plików głosowych
/status — stan połączenia
Pozostały tekst i załączniki trafiają do wybranego terminala.";

#[derive(Debug, PartialEq)]
enum BridgeCommand<'a> {
    New(&'a str, Option<&'a str>),
    Open(&'a str),
    Key(&'a str),
    Agent(&'a str),
    Sessions,
    Attach,
    Window,
    Detach,
    Status,
    Voice,
    Help,
}

fn parse_command(body: &str) -> Result<Option<BridgeCommand<'_>>> {
    let mut parts = body.split_whitespace();
    let result = match parts.next() {
        Some("/new") => BridgeCommand::New(
            parts.next().context("użycie: /new <nazwa> [agent]")?,
            parts.next(),
        ),
        Some("/open") => BridgeCommand::Open(parts.next().context("użycie: /open <nazwa>")?),
        Some("/key") => BridgeCommand::Key(parts.next().context("użycie: /key <klawisz>")?),
        Some("/agent") => BridgeCommand::Agent(parts.next().context("użycie: /agent <agent>")?),
        Some("/sessions") => BridgeCommand::Sessions,
        Some("/attach") => BridgeCommand::Attach,
        Some("/window") => BridgeCommand::Window,
        Some("/detach" | "/stop") => BridgeCommand::Detach,
        Some("/status") => BridgeCommand::Status,
        Some("/voice") => BridgeCommand::Voice,
        Some("/help" | "/start") => BridgeCommand::Help,
        _ => return Ok(None),
    };
    anyhow::ensure!(parts.next().is_none(), "nadmiarowe argumenty; zobacz /help");
    Ok(Some(result))
}

#[derive(Clone, Copy)]
enum VoiceMode {
    Transcribe,
    Files,
}

impl VoiceMode {
    fn from_env() -> Result<Self> {
        match std::env::var("VOICE_INPUT_MODE").as_deref() {
            Ok("files") => Ok(Self::Files),
            Ok("transcribe") => Ok(Self::Transcribe),
            Err(std::env::VarError::NotPresent) => Ok(Self::Files),
            _ => bail!("VOICE_INPUT_MODE musi mieć wartość transcribe lub files"),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Transcribe => "Nagrania Matrix są transkrybowane do wybranego terminala.",
            Self::Files => {
                "Nagrania trafiają do voice/in, a odpowiedzi o tym samym ID wracają z voice/out."
            }
        }
    }
}

struct Workspace {
    agent: String,
    selected: Option<String>,
    follow: watch::Sender<Option<terminal::Follow>>,
    voice: VoiceMode,
    image_outbox: PathBuf,
    pending_media: Arc<Semaphore>,
}

impl Workspace {
    fn select(&mut self, cfg: &Config, name: String) -> Result<()> {
        save_str(&cfg.active_terminal_file, &name)?;
        self.follow.send_replace(None);
        self.selected = Some(name);
        Ok(())
    }

    async fn current_session(&self, cfg: &Config) -> Result<String> {
        if let Some(name) = &self.selected {
            anyhow::ensure!(
                terminal::session_exists(name).await?,
                "terminal `{name}` już nie działa; użyj /sessions, /open lub /new"
            );
            return Ok(name.clone());
        }
        let name = terminal::session_name(cfg, &self.agent);
        if !terminal::session_exists(&name).await? {
            terminal::create(cfg, &name, &self.agent).await?;
        }
        Ok(name)
    }

    async fn attach(&self, cfg: &Config, explicit: bool) -> Result<(String, String)> {
        if !explicit {
            if let Some(active) = self.follow.borrow().as_ref() {
                return Ok((active.pane.clone(), String::new()));
            }
        }
        let name = self.current_session(cfg).await?;
        let pane = terminal::pane(&name).await?;
        let captured = terminal::capture(&pane).await?;
        self.follow.send_replace(Some(terminal::Follow {
            pane: pane.clone(),
            baseline: captured.clone(),
        }));
        Ok((pane, captured))
    }

    async fn relay_text(&self, cfg: &Config, text: &str) -> Result<()> {
        let (pane, _) = self.attach(cfg, false).await?;
        terminal::send_text(&pane, text).await
    }

    async fn command(&mut self, cfg: &Config, command: BridgeCommand<'_>) -> Result<String> {
        Ok(match command {
            BridgeCommand::New(label, agent) => {
                let agent = terminal::agent_name(agent.unwrap_or(&self.agent));
                let name = terminal::named_session(cfg, label)?;
                anyhow::ensure!(
                    !terminal::session_exists(&name).await?,
                    "terminal już działa; użyj /open {label}"
                );
                terminal::create(cfg, &name, &agent).await?;
                self.select(cfg, name.clone())?;
                self.attach(cfg, true).await?;
                format!("Uruchomiono `{name}` z agentem `{agent}`. Na komputerze: `tmux attach -t {name}`.")
            }
            BridgeCommand::Open(label) => {
                let name = if label == terminal::session_name(cfg, &self.agent) {
                    label.to_owned()
                } else {
                    terminal::named_session(cfg, label)?
                };
                anyhow::ensure!(
                    terminal::session_exists(&name).await?,
                    "brak terminala `{name}`"
                );
                terminal::pane(&name).await?;
                self.select(cfg, name.clone())?;
                let (_, captured) = self.attach(cfg, true).await?;
                format!("Otworzono `{name}`.\n{}", visible_screen(&captured))
            }
            BridgeCommand::Sessions => {
                let prefix = format!("{}-", cfg.tmux_prefix);
                let names = terminal::list(cfg, &self.agent).await?;
                if names.is_empty() {
                    "Brak działających terminali. Użyj /new <nazwa> [agent].".to_owned()
                } else {
                    names
                        .iter()
                        .map(|name| format!("/open {}", name.strip_prefix(&prefix).unwrap_or(name)))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            BridgeCommand::Attach => {
                let name = self.current_session(cfg).await?;
                let (_, captured) = self.attach(cfg, true).await?;
                format!(
                    "Widok `{name}` jest podłączony.\n{}",
                    visible_screen(&captured)
                )
            }
            BridgeCommand::Window => {
                let name = self.current_session(cfg).await?;
                if terminal::open_window(&name).await? {
                    format!("Otwarto okno terminala dla `{name}`.")
                } else {
                    format!("Brak sesji graficznej lub TERMINAL_CMD wyłączony — `{name}` działa w tle.")
                }
            }
            BridgeCommand::Detach => {
                self.follow.send_replace(None);
                "Widok Matrix odłączony. Terminal pozostaje otwarty.".to_owned()
            }
            BridgeCommand::Key(key) => {
                let (pane, _) = self.attach(cfg, false).await?;
                terminal::send_key(&pane, key).await?;
                return Ok(String::new());
            }
            BridgeCommand::Agent(agent) => {
                let agent = terminal::agent_name(agent);
                anyhow::ensure!(
                    terminal::executable(cfg, &agent).is_some(),
                    "nieznany agent `{agent}`"
                );
                let name = terminal::named_session(cfg, &agent)?;
                if !terminal::session_exists(&name).await? {
                    terminal::create(cfg, &name, &agent).await?;
                }
                terminal::pane(&name).await?;
                save_str(&cfg.agent_file, &agent)?;
                self.select(cfg, name.clone())?;
                self.agent = agent.clone();
                self.attach(cfg, true).await?;
                format!("Wybrano `{agent}` w terminalu `{name}`.")
            }
            BridgeCommand::Status => {
                let name = self
                    .selected
                    .clone()
                    .unwrap_or_else(|| terminal::session_name(cfg, &self.agent));
                format!(
                    "Terminal: `{name}`\nWidok Matrix: {}\nObrazy do wysłania: `{}`",
                    if self.follow.borrow().is_some() {
                        "podłączony"
                    } else {
                        "odłączony"
                    },
                    self.image_outbox.display()
                )
            }
            BridgeCommand::Voice => self.voice.description().to_owned(),
            BridgeCommand::Help => HELP.to_owned(),
        })
    }
}

fn visible_screen(captured: &str) -> String {
    let lines: Vec<_> = captured.trim_end().lines().collect();
    let tail = lines[lines.len().saturating_sub(40)..].join("\n");
    safe_truncate(&tail, 3400).to_owned()
}

#[tokio::main]
async fn main() -> Result<()> {
    bridge_common::init_logging();
    let cfg = Config::from_env().context("konfiguracja Matrix")?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let client = MatrixClient::new(&cfg).await?;
    match args.as_slice() {
        [command, path] if command == "recover-device" => {
            client.recover_device(Path::new(path)).await?;
            println!("Urządzenie mostka jest zweryfikowane.");
            return Ok(());
        }
        [command, user, fingerprint] if command == "verify-user" => {
            client.verify_user(user, fingerprint).await?;
            println!("Zweryfikowano tożsamość {user}.");
            return Ok(());
        }
        [] => {}
        _ => bail!("użycie: bridge-headless [recover-device <plik> | verify-user <MXID> <klucz-publiczny>]"),
    }
    let voice = VoiceMode::from_env()?;
    private_directory(Path::new(&cfg.uploads_dir))
        .context("katalog prywatnych załączników Matrix")?;
    if matches!(voice, VoiceMode::Files) {
        private_directory(Path::new(&cfg.voice_dir)).context("prywatny katalog voice")?;
    }
    tokio::fs::create_dir_all(&cfg.vibe_workdir)
        .await
        .context("katalog projektu")?;
    let outbox = media::image_outbox(&cfg);
    private_directory(&outbox).context("outbox obrazów")?;
    let outbox = tokio::fs::canonicalize(outbox).await?;
    let (follow, receiver) = watch::channel(None);
    tokio::spawn(terminal::relay(client.clone(), receiver));
    tokio::spawn(media::watch_images(client.clone(), outbox.clone()));

    let mut agent =
        terminal::agent_name(load_str_opt(&cfg.agent_file).as_deref().unwrap_or("vibe"));
    if terminal::executable(&cfg, &agent).is_none() {
        agent = "vibe".to_owned();
    }
    let mut workspace = Workspace {
        agent,
        selected: load_str_opt(&cfg.active_terminal_file),
        follow,
        voice,
        image_outbox: outbox,
        pending_media: Arc::new(Semaphore::new(cfg.max_pending_media)),
    };
    let mut seen = SeenEvents::load(&cfg.state_file, cfg.max_seen_events)?;
    let mut sender_hits = HashMap::new();
    let mut since = None;
    tracing::info!(room = %cfg.room_id, "start bramki terminala Matrix");

    loop {
        match client.sync(since.as_deref(), cfg.sync_timeout_ms).await {
            Ok(response) => {
                let initial_sync = since.is_none();
                since = Some(response.next_batch);
                // A fresh bridge follows new messages only, so old tasks are not typed again.
                if initial_sync {
                    continue;
                }
                let mut dirty = false;
                for event in &response.events {
                    if !accept_event(&cfg, event) || !seen.insert(&event.id) {
                        continue;
                    }
                    dirty = true;
                    let detaching = event.content.msgtype == "m.text"
                        && matches!(event.content.body.trim(), "/detach" | "/stop");
                    if !detaching && !within_rate_limit(&cfg, &mut sender_hits, &event.sender) {
                        continue;
                    }
                    if let Err(error) = handle_event(&cfg, &client, &mut workspace, event).await {
                        tracing::warn!(event = %event.id, %error, "nie udało się obsłużyć wiadomości");
                        let _ = client.send_text(&format!("⚠️ {error:#}")).await;
                    }
                }
                if dirty {
                    if let Err(error) = seen.save(&cfg.state_file) {
                        tracing::warn!(%error, "nie udało się zapisać obsłużonych zdarzeń");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "błąd synchronizacji Matrix");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn accept_event(cfg: &Config, event: &Event) -> bool {
    event.kind == "m.room.message"
        && cfg.accepts_sender(&event.sender)
        && matches!(
            event.content.msgtype.as_str(),
            "m.text" | "m.audio" | "m.image"
        )
        && event
            .content
            .relates_to
            .as_ref()
            .is_none_or(|rel| rel["rel_type"] != "m.replace")
}

fn within_rate_limit(
    cfg: &Config,
    hits: &mut HashMap<String, VecDeque<Instant>>,
    sender: &str,
) -> bool {
    let now = Instant::now();
    let recent = hits.entry(sender.to_owned()).or_default();
    recent.retain(|time| now.duration_since(*time) < Duration::from_secs(60));
    if recent.len() >= cfg.inbound_max_per_min as usize {
        return false;
    }
    recent.push_back(now);
    true
}

async fn handle_event(
    cfg: &Config,
    client: &MatrixClient,
    workspace: &mut Workspace,
    event: &Event,
) -> Result<()> {
    match event.content.msgtype.as_str() {
        "m.text" => {
            let body = event.content.body.trim();
            if let Some(command) = parse_command(body)? {
                let message = workspace.command(cfg, command).await?;
                if !message.is_empty() {
                    client.send_text(&message).await?;
                }
            } else if !body.is_empty() {
                workspace.relay_text(cfg, body).await?;
            }
        }
        "m.image" | "m.audio" => {
            let permit = workspace
                .pending_media
                .clone()
                .try_acquire_owned()
                .context("limit równoczesnych załączników Matrix został osiągnięty")?;
            let (pane, _) = workspace.attach(cfg, false).await?;
            let cfg = cfg.clone();
            let client = client.clone();
            let event = event.clone();
            let outbox = workspace.image_outbox.clone();
            let voice_mode = workspace.voice;
            tokio::spawn(async move {
                let _permit = permit;
                let result = if event.content.msgtype == "m.image" {
                    receive_image(&cfg, &client, &event, &pane, &outbox).await
                } else {
                    receive_audio(&cfg, &client, voice_mode, &event, &pane).await
                };
                if let Err(error) = result {
                    tracing::warn!(event = %event.id, %error, "nie udało się przekazać załącznika");
                    let _ = client.send_text(&format!("⚠️ {error:#}")).await;
                }
            });
        }
        _ => {}
    }
    Ok(())
}

async fn receive_image(
    cfg: &Config,
    client: &MatrixClient,
    event: &Event,
    pane: &str,
    outbox: &Path,
) -> Result<()> {
    let file = media::receive_image(cfg, client, event).await?;
    terminal::send_text(pane, &format!(
        "[Obraz z Matrix] Plik lokalny: {}\nOpis: {}\nAby wysłać obraz z powrotem, umieść gotową kopię w katalogu {}.",
        file.display(), event.content.body, outbox.display()
    ))
    .await
}

async fn receive_audio(
    cfg: &Config,
    client: &MatrixClient,
    voice_mode: VoiceMode,
    event: &Event,
    pane: &str,
) -> Result<()> {
    let (name, bytes, mime) = client
        .download_mxc(&event.content, media::MAX_MEDIA_BYTES)
        .await?;
    match voice_mode {
        VoiceMode::Transcribe => {
            let name = format!("voice.{}", voice::input_extension(&name, &mime));
            let transcript = transcribe(cfg, bytes, name, &mime).await?;
            anyhow::ensure!(
                !transcript.trim().is_empty(),
                "nie udało się rozpoznać mowy"
            );
            terminal::send_text(pane, &format!("[Nagranie Matrix] {transcript}")).await?;
        }
        VoiceMode::Files => {
            let exchange = voice::VoiceExchange::publish(
                Path::new(&cfg.voice_dir),
                Path::new(&cfg.uploads_dir),
                &name,
                &mime,
                &bytes,
            )
            .await?;
            let path = exchange.input_path();
            terminal::send_text(pane, &format!(
                "[Nagranie Matrix] Plik lokalny: {}\nJeśli obsługujesz audio, zapisz odpowiedź z tą samą nazwą bazową w katalogu {}.",
                path.display(),
                Path::new(&cfg.voice_dir).join("out").display()
            )).await?;
            let reply = exchange.wait().await?;
            reply
                .deliver(|filename, mime, bytes| async move {
                    let duration =
                        Duration::from_millis(((bytes.len() as u64 / 4000) * 10).max(500));
                    client.send_audio(&filename, mime, bytes, duration).await
                })
                .await?;
        }
    }
    Ok(())
}

async fn transcribe(cfg: &Config, audio: Vec<u8>, filename: String, mime: &str) -> Result<String> {
    let key = std::env::var("MISTRAL_API_KEY").context("tryb transcribe wymaga MISTRAL_API_KEY")?;
    let part = reqwest::multipart::Part::bytes(audio)
        .file_name(filename)
        .mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model", cfg.stt_model.clone())
        .part("file", part);
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .build()?
        .post("https://api.mistral.ai/v1/audio/transcriptions")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let value: serde_json::Value = response.json().await?;
    Ok(value["text"]
        .as_str()
        .context("brak tekstu transkrypcji")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_controls_are_distinct_from_native_messages() {
        assert_eq!(
            parse_command("/new mapy codex").unwrap(),
            Some(BridgeCommand::New("mapy", Some("codex")))
        );
        assert_eq!(
            parse_command("/open mapy").unwrap(),
            Some(BridgeCommand::Open("mapy"))
        );
        assert_eq!(
            parse_command("/detach").unwrap(),
            Some(BridgeCommand::Detach)
        );
        assert_eq!(
            parse_command("/window").unwrap(),
            Some(BridgeCommand::Window)
        );
        assert!(parse_command("/window extra").is_err());
        assert!(parse_command("/new").is_err());
        assert!(parse_command("/open mapy extra").is_err());
        assert_eq!(parse_command("/model fast").unwrap(), None);
    }
}
