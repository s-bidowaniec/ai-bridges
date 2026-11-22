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
    /// Preset agenta (vibe|agy|claude|codex) lub pusty gdy AGENT_CMD.
    pub agent: String,
    /// Jawna komenda (bez presetu): AGENT_CMD="claude -p {prompt} --dangerously-skip-permissions".
    pub agent_cmd: Vec<String>,
    pub agent_timeout_secs: u64,
    pub sync_timeout_ms: u32,
    pub state_file: String,
    pub agent_env_keys: Vec<String>,
    /// PATH dla procesu agenta.
    pub agent_path: String,
    /// HOME dla procesu agenta (tam są poświadczenia OAuth agentów).
    pub agent_home: String,
}

/// Presety poleceń headless dla supported agentów. `{prompt}` = treść wiadomości.
pub fn agent_preset(name: &str) -> Option<Vec<&'static str>> {
    match name {
        "vibe" => Some(vec!["vibe", "-p", "{prompt}", "--yolo"]),
        "agy" => Some(vec!["agy", "-p", "{prompt}", "--dangerously-skip-permissions"]),
        "claude" => Some(vec!["claude", "-p", "{prompt}", "--dangerously-skip-permissions"]),
        "codex" => Some(vec![
            "codex", "exec", "--dangerously-bypass-approvals-and-sandbox", "{prompt}",
        ]),
        _ => None,
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            std::env::var(k).with_context(|| format!("brak zmiennej env {k}"))
        };
        let allowed = std::env::var("MATRIX_ALLOWED_USERS").unwrap_or_default();
        let agent_env_keys = std::env::var("AGENT_ENV_KEYS")
            .or_else(|_| std::env::var("VIBE_ENV_KEYS"))
            .unwrap_or_default();
        let agent_cmd_raw = std::env::var("AGENT_CMD").unwrap_or_default();
        let agent_cmd: Vec<String> = if agent_cmd_raw.trim().is_empty() {
            Vec::new()
        } else {
            agent_cmd_raw.split_whitespace().map(|s| s.to_string()).collect()
        };
        Ok(Self {
            homeserver: std::env::var("MATRIX_HOMESERVER")
                .unwrap_or_else(|_| "https://matrix.owl-post.eu".to_string()),
            token: get("MATRIX_ACCESS_TOKEN")?,
            room_id: get("MATRIX_ROOM_ID")?,
            bot_user_id: get("MATRIX_BOT_USER_ID")?,
            allowed_users: allowed.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            agent: std::env::var("AGENT").unwrap_or_else(|_| "vibe".to_string()),
            agent_cmd,
            agent_timeout_secs: std::env::var("AGENT_TIMEOUT_SECS")
                .or_else(|_| std::env::var("VIBE_TIMEOUT_SECS"))
                .ok().and_then(|v| v.parse().ok()).unwrap_or(600),
            sync_timeout_ms: std::env::var("SYNC_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30000),
            state_file: std::env::var("STATE_FILE")
                .unwrap_or_else(|_| format!("{}/.local/state/vibe-matrix-rs/seen.txt", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))),
            agent_env_keys: agent_env_keys.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            agent_path: std::env::var("AGENT_PATH")
                .unwrap_or_else(|_| std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())),
            agent_home: std::env::var("AGENT_HOME")
                .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| "/root".into())),
        })
    }

    /// argv agenta z podmienionym {prompt}.
    pub fn agent_argv(&self, prompt: &str) -> Result<Vec<String>> {
        let base: Vec<String> = if !self.agent_cmd.is_empty() {
            self.agent_cmd.clone()
        } else {
            agent_preset(&self.agent)
                .with_context(|| format!("nieznany preset AGENT={} (dozwolone: vibe, agy, claude, codex; albo ustaw AGENT_CMD)", self.agent))?
                .iter().map(|s| s.to_string()).collect()
        };
        Ok(base.iter().map(|a| a.replace("{prompt}", prompt)).collect())
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
