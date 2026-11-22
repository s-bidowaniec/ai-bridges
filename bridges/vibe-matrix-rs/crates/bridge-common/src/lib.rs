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
