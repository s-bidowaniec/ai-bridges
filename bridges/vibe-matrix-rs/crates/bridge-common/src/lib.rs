//! bridge-common: config z env, logging z redaction, czyszczenie ANSI.
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    pub homeserver: String,
    pub token: String,
    pub room_id: String,
    pub bot_user_id: String,
    pub allowed_users: Vec<String>,
    pub vibe_cmd: String,
    pub vibe_timeout_secs: u64,
    pub sync_timeout_ms: u32,
    pub state_file: String,
    pub session_file: String,
    pub agent_file: String,
    pub tmux_prefix: String,
    pub vibe_workdir: String,
    pub vibe_env_keys: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            std::env::var(k).with_context(|| format!("brak zmiennej env {k}"))
        };
        let allowed = std::env::var("MATRIX_ALLOWED_USERS").unwrap_or_default();
        let vibe_env_keys = std::env::var("VIBE_ENV_KEYS")
            .unwrap_or_else(|_| "MISTRAL_API_KEY".to_string());
        Ok(Self {
            homeserver: std::env::var("MATRIX_HOMESERVER")
                .unwrap_or_else(|_| "https://matrix.owl-post.eu".to_string()),
            token: get("MATRIX_ACCESS_TOKEN")?,
            room_id: get("MATRIX_ROOM_ID")?,
            bot_user_id: get("MATRIX_BOT_USER_ID")?,
            allowed_users: allowed.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            vibe_cmd: std::env::var("VIBE_CMD").unwrap_or_else(|_| "vibe".to_string()),
            vibe_timeout_secs: std::env::var("VIBE_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600),
            sync_timeout_ms: std::env::var("SYNC_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30000),
            state_file: std::env::var("STATE_FILE").unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/seen.txt".to_string()),
            session_file: std::env::var("SESSION_FILE").unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/session.txt".to_string()),
            agent_file: std::env::var("AGENT_FILE").unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/agent.txt".to_string()),
            tmux_prefix: std::env::var("TMUX_PREFIX").unwrap_or_else(|_| "vibe-matrix-rs".to_string()),
            vibe_workdir: std::env::var("VIBE_WORKDIR").unwrap_or_else(|_| "/var/lib/vibe-matrix-rs/vibe-work".to_string()),
            vibe_env_keys: vibe_env_keys.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        })
    }
}

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

/// Wczytuje zbiór już przetworzonych event_id.
pub fn load_seen(path: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let l = line.trim();
            if !l.is_empty() { set.insert(l.to_string()); }
        }
    }
    set
}

pub fn save_seen(path: &str, seen: &HashSet<String>) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut v: Vec<&String> = seen.iter().collect();
    v.sort();
    std::fs::write(path, v.iter().map(|s| format!("{s}\n")).collect::<String>())
        .context("zapis state_file")
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
        assert_eq!(load_session(p.to_str().unwrap()), Some("abc-123".to_string()));
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
    // zwiń nadmiarowe puste linie
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 { continue; }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}
