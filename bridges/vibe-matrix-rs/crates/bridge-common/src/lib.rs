//! bridge-common: config z env, logging z redaction, czyszczenie ANSI.
use anyhow::{Context, Result};
use regex::Regex;
use secrecy::SecretString;
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub homeserver: String,
    pub token: SecretString,
    pub room_id: String,
    pub bot_user_id: String,
    pub matrix_device_id: String,
    pub matrix_store_dir: PathBuf,
    pub matrix_store_passphrase_file: PathBuf,
    pub allow_insecure_loopback: bool,
    access: MatrixAccess,
    pub vibe_cmd: String,
    pub sync_timeout_ms: u32,
    pub inbound_max_per_min: u32,
    pub max_seen_events: usize,
    pub max_pending_media: usize,
    pub state_file: String,
    pub agent_file: String,
    pub active_terminal_file: String,
    pub tmux_prefix: String,
    pub vibe_workdir: String,
    pub uploads_dir: String,
    pub voice_dir: String,
    pub stt_model: String,
}

#[derive(Debug, Clone)]
enum MatrixAccess {
    Only(HashSet<String>),
    WholeRoom,
}

impl MatrixAccess {
    fn parse(allowed: &str, whole_room: &str) -> Result<Self> {
        let users: HashSet<_> = allowed
            .split(',')
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .map(str::to_owned)
            .collect();
        let whole_room = match whole_room.trim() {
            "" | "0" => false,
            "1" => true,
            _ => anyhow::bail!("MATRIX_ALLOW_ALL_ROOM_MEMBERS musi mieć wartość 0 lub 1"),
        };
        match (users.is_empty(), whole_room) {
            (true, true) => Ok(Self::WholeRoom),
            (false, false) => {
                anyhow::ensure!(
                    users.iter().all(|user| {
                        user.strip_prefix('@')
                            .and_then(|id| id.split_once(':'))
                            .is_some_and(|(local, server)| {
                                !local.is_empty()
                                    && !server.is_empty()
                                    && !user.chars().any(char::is_whitespace)
                            })
                    }),
                    "MATRIX_ALLOWED_USERS musi zawierać identyfikatory @użytkownik:serwer"
                );
                Ok(Self::Only(users))
            }
            (false, true) => anyhow::bail!(
                "Ustaw tylko MATRIX_ALLOWED_USERS albo MATRIX_ALLOW_ALL_ROOM_MEMBERS=1"
            ),
            (true, false) => anyhow::bail!(
                "Ustaw MATRIX_ALLOWED_USERS albo jawnie MATRIX_ALLOW_ALL_ROOM_MEMBERS=1"
            ),
        }
    }

    fn permits(&self, bot: &str, sender: &str) -> bool {
        sender != bot
            && match self {
                Self::Only(users) => users.contains(sender),
                Self::WholeRoom => true,
            }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            std::env::var(k).with_context(|| format!("brak zmiennej env {k}"))
        };
        let optional = |name| match std::env::var(name) {
            Ok(value) => Ok(value),
            Err(std::env::VarError::NotPresent) => Ok(String::new()),
            Err(error) => Err(error).with_context(|| format!("nie można odczytać {name}")),
        };
        let allowed = optional("MATRIX_ALLOWED_USERS")?;
        let whole_room = optional("MATRIX_ALLOW_ALL_ROOM_MEMBERS")?;
        let access = MatrixAccess::parse(&allowed, &whole_room)?;
        let token_file = optional("MATRIX_ACCESS_TOKEN_FILE")?;
        let token = if token_file.is_empty() {
            SecretString::new(get("MATRIX_ACCESS_TOKEN")?.into())
        } else {
            anyhow::ensure!(
                optional("MATRIX_ACCESS_TOKEN")?.is_empty(),
                "ustaw tylko jedno źródło tokena Matrix"
            );
            read_private_secret(Path::new(&token_file)).context("odczyt tokena Matrix")?
        };
        Ok(Self {
            homeserver: get("MATRIX_HOMESERVER")?,
            token,
            room_id: get("MATRIX_ROOM_ID")?,
            bot_user_id: get("MATRIX_BOT_USER_ID")?,
            matrix_device_id: get("MATRIX_BOT_DEVICE_ID")?,
            matrix_store_dir: PathBuf::from(get("MATRIX_STORE_DIR")?),
            matrix_store_passphrase_file: PathBuf::from(get("MATRIX_STORE_PASSPHRASE_FILE")?),
            allow_insecure_loopback: optional("ALLOW_INSECURE_HTTP")? == "1",
            access,
            vibe_cmd: std::env::var("VIBE_CMD").unwrap_or_else(|_| "vibe".to_string()),
            sync_timeout_ms: std::env::var("SYNC_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30000),
            inbound_max_per_min: std::env::var("INBOUND_MAX_PER_MIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6),
            max_seen_events: std::env::var("MAX_SEEN_EVENTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            max_pending_media: std::env::var("MAX_PENDING_MEDIA")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v| (1..=64).contains(v))
                .unwrap_or(8),
            state_file: std::env::var("STATE_FILE")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/seen.txt".to_string()),
            agent_file: std::env::var("AGENT_FILE")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/agent.txt".to_string()),
            active_terminal_file: std::env::var("ACTIVE_TERMINAL_FILE")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/active-terminal.txt".to_owned()),
            tmux_prefix: std::env::var("TMUX_PREFIX")
                .unwrap_or_else(|_| "vibe-matrix-rs".to_string()),
            uploads_dir: std::env::var("UPLOADS_DIR")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/uploads".to_string()),
            voice_dir: std::env::var("VOICE_DIR")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/voice".to_string()),
            stt_model: std::env::var("STT_MODEL")
                .unwrap_or_else(|_| "voxtral-mini-latest".to_string()),
            vibe_workdir: std::env::var("VIBE_WORKDIR")
                .unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/vibe-work".to_string()),
        })
    }

    pub fn accepts_sender(&self, sender: &str) -> bool {
        self.access.permits(&self.bot_user_id, sender)
    }

    pub fn room_audience(&self) -> Option<HashSet<String>> {
        match &self.access {
            MatrixAccess::Only(users) => {
                let mut audience = users.clone();
                audience.insert(self.bot_user_id.clone());
                Some(audience)
            }
            MatrixAccess::WholeRoom => None,
        }
    }
}

/// Use an existing private directory or create it without a public mode window.
pub fn private_directory(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "wymagany prywatny katalog bez symlinka (chmod 700): {}",
        path.display()
    );
    Ok(())
}

pub fn read_private_secret(path: &Path) -> Result<SecretString> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "wymagany prywatny plik bez symlinka (chmod 600)"
    );
    anyhow::ensure!(metadata.len() <= 4096, "plik sekretu jest za duży");
    let content = SecretString::from(std::fs::read_to_string(path)?);
    use secrecy::ExposeSecret;
    let value = content.expose_secret().trim();
    anyhow::ensure!(!value.is_empty(), "pusty plik sekretu");
    Ok(SecretString::new(value.into()))
}

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

/// A bounded FIFO of event IDs keeps recent commands deduplicated across restarts.
pub struct SeenEvents {
    ids: HashSet<String>,
    order: VecDeque<String>,
    limit: usize,
}

impl SeenEvents {
    pub fn load(path: &str, limit: usize) -> Result<Self> {
        let mut events = Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
            limit: limit.max(1),
        };
        match std::fs::File::open(path) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    let line = line?;
                    let id = line.trim();
                    if !id.is_empty() {
                        events.insert(id);
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("odczyt historii zdarzeń Matrix"),
        }
        Ok(events)
    }

    pub fn insert(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > self.limit {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        true
    }

    pub fn save(&self, path: &str) -> Result<()> {
        let path = Path::new(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary = path.with_extension(format!("tmp-{}-{now:x}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| {
            for id in &self.order {
                writeln!(file, "{id}")?;
            }
            file.sync_all()?;
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.context("zapis historii zdarzeń Matrix")
    }
}

/// Session id vibe (do `--resume`), trzymany w osobnym pliku.
pub fn load_session(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_session(path: &str, session_id: &str) -> Result<()> {
    save_str(path, session_id)
}

/// Generyczny load opcjonalnego stringa z pliku (model/think/session).
pub fn load_str_opt(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Generyczny zapis stringa do pliku stanu (tworzy katalogi).
pub fn save_str(path: &str, val: &str) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{val}\n")).context("zapis pliku stanu")
}

/// Usuwa plik stanu (np. session.txt przy /new).
pub fn remove_state_file(path: &str) {
    let _ = std::fs::remove_file(path);
}

/// Bezpieczne obcięcie do `max` bajtów na granicy znaku UTF-8 (nie panikuje przy emoji/PL znakach).
pub fn safe_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_cache_evicts_only_the_oldest_id_and_survives_reload() {
        let root = std::env::temp_dir().join(format!("matrix-seen-test-{}", std::process::id()));
        private_directory(&root).unwrap();
        let path = root.join("seen.txt");
        let path = path.to_str().unwrap();
        let mut seen = SeenEvents::load(path, 2).unwrap();
        for id in ["old", "previous", "new"] {
            assert!(seen.insert(id));
        }
        assert!(!seen.insert("previous"));
        seen.save(path).unwrap();
        let mut loaded = SeenEvents::load(path, 2).unwrap();
        assert!(!loaded.insert("new"));
        assert!(loaded.insert("old"));
        assert!(loaded.insert("previous"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secrets_and_crypto_directories_require_private_permissions() {
        let root = std::env::temp_dir().join(format!("matrix-private-test-{}", std::process::id()));
        private_directory(&root).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, "synthetic test value\n").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_secret(&secret).is_err());
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private_secret(&secret).is_ok());
        let link = root.join("link");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(read_private_secret(&link).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matrix_access_is_explicit_and_always_excludes_the_bot() {
        let bot = "@bot:example.org";
        let allowed = "@alice:example.org";
        let other = "@bob:example.org";
        for config in [
            ("", ""),
            (" , ", "0"),
            ("alice", "0"),
            (allowed, "1"),
            ("", "yes"),
        ] {
            assert!(MatrixAccess::parse(config.0, config.1).is_err());
        }
        let only = MatrixAccess::parse(&format!(" {allowed}, {bot} "), "0").unwrap();
        assert!(only.permits(bot, allowed));
        assert!(!only.permits(bot, other));
        assert!(!only.permits(bot, bot));
        let room = MatrixAccess::parse("", "1").unwrap();
        assert!(room.permits(bot, other));
        assert!(!room.permits(bot, bot));
    }

    #[test]
    fn truncate_utf8_safe() {
        let s = "zażółć gęślą jaźń 🦀🦀🦀 kotek";
        for max in 0..s.len() {
            let t = safe_truncate(s, max);
            assert!(t.len() <= max);
            assert!(s.starts_with(t));
        }
    }

    #[test]
    fn ansi_clean() {
        assert_eq!(clean_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(clean_ansi("a\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn session_roundtrip() {
        let dir = std::env::temp_dir().join("vibe-matrix-rs-test");
        let p = dir.join("session.txt");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_session(p.to_str().unwrap()), None);
        save_session(p.to_str().unwrap(), "abc-123").unwrap();
        assert_eq!(
            load_session(p.to_str().unwrap()),
            Some("abc-123".to_string())
        );
    }
}

/// Usuwa kody ANSI/OSC i znaki kontrolne z odpowiedzi TUI vibe.
pub fn clean_ansi(input: &str) -> String {
    let re_csi = Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]").unwrap();
    let re_osc = Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap();
    let re_ctrl = Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap();
    let s = re_osc.replace_all(input, "");
    let s = re_csi.replace_all(&s, "");
    let s = re_ctrl.replace_all(&s, "");
    collapse_blanks(&s)
}

fn collapse_blanks(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Czyści zrzut ekranu TUI agenta (capture-pane) do czytelnego tekstu na Matrix:
/// - ANSI/control (clean_ansi)
/// - linie czysto z box-drawing / separatorów (─── │ ┌─┐)
/// - status bary: zawierające bloki/procenty/„usage"/„ctx"/model (np. `usage ██▋ 44% · PAYG · ctx ▁ 2%`)
/// - spinnery (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ● ○ ◐ etc.) i „Thought"/„⏵" markery
/// - końcowy prompt (`>`, `›`, `❯` jako osobna linia)
pub fn clean_tui(input: &str) -> String {
    let s = clean_ansi(input);
    // box-drawing U+2500–257F + block elements U+2580–259F + whitespace
    let re_boxline = Regex::new(r"^[\s\u{2500}-\u{259F}]+$").unwrap();
    // linia = same box-drawing/whitespace (separator ────, ramki)
    let re_status = Regex::new(
        r"(usage\s|ctx\s|PAYG|\d+\s?%|Mistral|OpenAI|Codex \(|Antigravity CLI|Gemini \d|Claude Code|vibe \d|codex-cli|agy \d)",
    )
    .unwrap();
    let re_spinner = Regex::new(r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏●○◐◓◑◒◕]").unwrap();
    let re_prompt = Regex::new(r"^[>›❯#\$]\s*$").unwrap();
    let re_marker = Regex::new(r"^(⏵|▼|▶)\s*(Thought|Rzecz|Myśl)\s*$").unwrap();

    let mut out_lines: Vec<String> = Vec::new();
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            out_lines.push(String::new());
            continue;
        }
        if re_boxline.is_match(t) {
            continue; // separator/ramka
        }
        if re_prompt.is_match(t) {
            continue; // sam prompt
        }
        if re_marker.is_match(t) {
            continue; // zwinięty Thought
        }
        // status bar / separator z opisem: linia z box-drawing + słowa statusu (model, usage, %, tryb)
        let boxchars = t
            .chars()
            .filter(|c| ('\u{2500}'..='\u{259F}').contains(c))
            .count();
        if boxchars >= 4
            && (re_status.is_match(t)
                || t.contains('%')
                || t.contains("usage")
                || t.contains("ctx"))
        {
            continue;
        }
        // pasek użycia typu `usage ██▋ 44% · PAYG · ctx ▁ 2%` (bloki + %)
        let blocks = t.chars().filter(|c| "█▇▆▅▄▃▂▁".contains(*c)).count();
        if blocks >= 2 && t.contains('%') {
            continue;
        }
        if re_spinner.is_match(t) && t.chars().count() < 40 {
            continue; // sama spinnerowa linia
        }
        out_lines.push(t.to_string());
    }
    collapse_blanks(&out_lines.join("\n"))
}

#[cfg(test)]
mod tui_tests {
    use super::*;

    #[test]
    fn clean_tui_strips_chrome() {
        let pane = "\n────────────────────────────\n⏵ Thought\n  OK-VIBE-4\n────────────────────────────────────────────\n>\n─\n~/.local/state/vibe-matrix-rs/vibe-work       usage ██▋    44% · PAYG · ctx ▁ 2%\n────────────────── Mistral Large 4[high] · accept edits · auto-approve ──\n";
        let out = clean_tui(pane);
        assert!(out.contains("OK-VIBE-4"), "out={out:?}");
        assert!(!out.contains("44%"), "out={out:?}");
        assert!(!out.contains('─'), "out={out:?}");
        assert!(!out.contains('⏵'), "out={out:?}");
        assert!(!out.contains("Mistral"), "out={out:?}");
        assert!(!out.trim_end().ends_with('>'), "out={out:?}");
    }
}
