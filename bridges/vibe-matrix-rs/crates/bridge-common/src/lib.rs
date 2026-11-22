//! bridge-common: konfiguracja, atomowy stan sesji/dedup, presety agentów z resume,
//! parsery JSON wyjścia agentów, redakcja sekretów, bezpieczne dzielenie UTF-8 i czyszczenie ANSI.
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_MAX_SEEN: usize = 5_000;
pub const DEFAULT_MAX_MSG_CHARS: usize = 3_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Vibe,
    Agy,
    Claude,
    Codex,
    Custom,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Vibe => "vibe",
            AgentKind::Agy => "agy",
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub homeserver: String,
    pub allow_insecure_http: bool,
    pub token: String,
    pub room_id: String,
    pub bot_user_id: String,
    pub allowed_users: Vec<String>,
    /// Preset agenta (vibe|agy|claude|codex) lub pusty gdy AGENT_CMD.
    pub agent: String,
    /// Jawna komenda (bez presetu): AGENT_CMD="moj-agent -p {prompt}".
    pub agent_cmd: Vec<String>,
    pub agent_timeout_secs: u64,
    pub sync_timeout_ms: u32,
    pub state_file: String,
    pub max_seen_events: usize,
    pub max_msg_chars: usize,
    pub agent_env_keys: Vec<String>,
    /// PATH dla procesu agenta.
    pub agent_path: String,
    /// HOME dla procesu agenta (tam są poświadczenia OAuth agentów).
    pub agent_home: String,
    /// Katalog roboczy procesu agenta (domyślnie AGENT_HOME).
    pub agent_workdir: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            std::env::var(k).with_context(|| format!("brak zmiennej env {k}"))
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER")
            .unwrap_or_else(|_| "https://matrix.owl-post.eu".to_string());
        let allow_insecure_http = std::env::var("MATRIX_ALLOW_INSECURE_HTTP")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if !allow_insecure_http && !homeserver.starts_with("https://") {
            bail!(
                "MATRIX_HOMESERVER musi zaczynać się od https:// (albo ustaw MATRIX_ALLOW_INSECURE_HTTP=1)"
            );
        }

        let room_id = get("MATRIX_ROOM_ID")?;
        if !room_id.starts_with('!') || room_id.len() < 4 {
            bail!("MATRIX_ROOM_ID musi zaczynać się od '!': {room_id}");
        }
        let bot_user_id = get("MATRIX_BOT_USER_ID")?;
        if !bot_user_id.starts_with('@') || !bot_user_id.contains(':') {
            bail!(
                "MATRIX_BOT_USER_ID ma nieprawidłowy format (oczekiwano @user:server): {bot_user_id}"
            );
        }

        let allowed = std::env::var("MATRIX_ALLOWED_USERS").unwrap_or_default();
        let agent_env_keys = std::env::var("AGENT_ENV_KEYS")
            .or_else(|_| std::env::var("VIBE_ENV_KEYS"))
            .unwrap_or_default();
        let agent_cmd_raw = std::env::var("AGENT_CMD").unwrap_or_default();
        let agent_cmd: Vec<String> = if agent_cmd_raw.trim().is_empty() {
            Vec::new()
        } else {
            agent_cmd_raw
                .split_whitespace()
                .map(|s| s.to_string())
                .collect()
        };
        let agent_home = std::env::var("AGENT_HOME")
            .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        let agent_workdir = std::env::var("AGENT_WORKDIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| agent_home.clone());

        let cfg = Self {
            homeserver,
            allow_insecure_http,
            token: get("MATRIX_ACCESS_TOKEN")?,
            room_id,
            bot_user_id,
            allowed_users: allowed
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            agent: std::env::var("AGENT").unwrap_or_else(|_| "vibe".to_string()),
            agent_cmd,
            agent_timeout_secs: std::env::var("AGENT_TIMEOUT_SECS")
                .or_else(|_| std::env::var("VIBE_TIMEOUT_SECS"))
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
            sync_timeout_ms: std::env::var("SYNC_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30_000),
            state_file: std::env::var("STATE_FILE").unwrap_or_else(|_| {
                format!(
                    "{}/.local/state/vibe-matrix-rs/seen.txt",
                    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
                )
            }),
            max_seen_events: std::env::var("MAX_SEEN_EVENTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n >= 100)
                .unwrap_or(DEFAULT_MAX_SEEN),
            max_msg_chars: std::env::var("AGENT_MAX_OUTPUT_CHARS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n >= 500)
                .unwrap_or(DEFAULT_MAX_MSG_CHARS),
            agent_env_keys: agent_env_keys
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            agent_path: std::env::var("AGENT_PATH").unwrap_or_else(|_| {
                std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
            }),
            agent_home,
            agent_workdir,
        };
        // Walidacja presetu już na starcie (fail fast)
        let _ = cfg.agent_kind()?;
        Ok(cfg)
    }

    pub fn agent_kind(&self) -> Result<AgentKind> {
        if !self.agent_cmd.is_empty() {
            return Ok(AgentKind::Custom);
        }
        match self.agent.as_str() {
            "vibe" => Ok(AgentKind::Vibe),
            "agy" => Ok(AgentKind::Agy),
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            other => bail!(
                "nieznany preset AGENT={other} (dozwolone: vibe, agy, claude, codex; albo ustaw AGENT_CMD)"
            ),
        }
    }

    /// Buduje argv agenta z natywnym wznowieniem sesji (jeśli `session_id` jest dostępne).
    pub fn build_agent_argv(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        codex_out_file: Option<&Path>,
    ) -> Result<Vec<String>> {
        match self.agent_kind()? {
            AgentKind::Vibe => {
                let mut argv = vec![
                    "vibe".to_string(),
                    "--workdir".to_string(),
                    self.agent_workdir.clone(),
                    "--yolo".to_string(),
                    "--output".to_string(),
                    "json".to_string(),
                ];
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    argv.push("--resume".to_string());
                    argv.push(sid.to_string());
                }
                argv.push(format!("--prompt={prompt}"));
                Ok(argv)
            }
            AgentKind::Agy => {
                let mut argv = vec![
                    "agy".to_string(),
                    "--dangerously-skip-permissions".to_string(),
                    "--output-format".to_string(),
                    "json".to_string(),
                    "--print-timeout".to_string(),
                    format!("{}s", self.agent_timeout_secs),
                ];
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    argv.push("--conversation".to_string());
                    argv.push(sid.to_string());
                }
                argv.push(format!("--print={prompt}"));
                Ok(argv)
            }
            AgentKind::Claude => {
                let mut argv = vec![
                    "claude".to_string(),
                    "-p".to_string(),
                    "--dangerously-skip-permissions".to_string(),
                    "--output-format".to_string(),
                    "json".to_string(),
                ];
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    argv.push("--resume".to_string());
                    argv.push(sid.to_string());
                }
                argv.push("--".to_string());
                argv.push(prompt.to_string());
                Ok(argv)
            }
            AgentKind::Codex => {
                let mut argv = vec!["codex".to_string(), "exec".to_string()];
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    argv.push("resume".to_string());
                    argv.push("--skip-git-repo-check".to_string());
                    argv.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                    argv.push("--json".to_string());
                    if let Some(out) = codex_out_file {
                        argv.push("-o".to_string());
                        argv.push(out.to_string_lossy().to_string());
                    }
                    argv.push("--".to_string());
                    argv.push(sid.to_string());
                    argv.push(prompt.to_string());
                } else {
                    argv.push("--skip-git-repo-check".to_string());
                    argv.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                    argv.push("--json".to_string());
                    if let Some(out) = codex_out_file {
                        argv.push("-o".to_string());
                        argv.push(out.to_string_lossy().to_string());
                    }
                    argv.push("--".to_string());
                    argv.push(prompt.to_string());
                }
                Ok(argv)
            }
            AgentKind::Custom => {
                let sid = session_id.unwrap_or("");
                Ok(self
                    .agent_cmd
                    .iter()
                    .map(|a| a.replace("{prompt}", prompt).replace("{session_id}", sid))
                    .collect())
            }
        }
    }

    /// Zwraca listę sekretów do redakcji w logach i odpowiedziach Matrix.
    pub fn secrets(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.token.len() >= 8 {
            out.push(self.token.clone());
        }
        for key in &self.agent_env_keys {
            let upper = key.to_ascii_uppercase();
            if upper.contains("TOKEN")
                || upper.contains("KEY")
                || upper.contains("SECRET")
                || upper.contains("PASS")
            {
                if let Ok(val) = std::env::var(key) {
                    if val.len() >= 8 {
                        out.push(val);
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAgentOutput {
    pub text: String,
    pub session_id: Option<String>,
    pub is_error: bool,
}

/// Parsuje ustrukturyzowane wyjście JSON poszczególnych CLI (z fallbackiem na czysty tekst).
pub fn parse_agent_output(
    kind: AgentKind,
    stdout: &str,
    codex_out_text: Option<&str>,
) -> ParsedAgentOutput {
    match kind {
        AgentKind::Vibe => parse_vibe_json(stdout),
        AgentKind::Agy => parse_agy_json(stdout),
        AgentKind::Claude => parse_claude_json(stdout),
        AgentKind::Codex => parse_codex_jsonl(stdout, codex_out_text),
        AgentKind::Custom => ParsedAgentOutput {
            text: clean_ansi(stdout),
            session_id: None,
            is_error: false,
        },
    }
}

fn parse_vibe_json(stdout: &str) -> ParsedAgentOutput {
    // Vibe może wypisać linię ostrzeżenia przed tablicą JSON (np. keyring notice).
    let trimmed = stdout.trim();
    let json_slice = trimmed.find('[').map(|i| &trimmed[i..]).unwrap_or(trimmed);

    if let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(json_slice) {
        let session_id = entries
            .iter()
            .rev()
            .find_map(|e| {
                e.get("sessionId")
                    .or_else(|| e.get("session_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        // Znajdź ostatnie turnId należące do wiadomości asystenta (w --resume vibe zwraca całą historię).
        let last_assistant_turn = entries.iter().rev().find_map(|e| {
            let is_assistant = e.get("type").and_then(|v| v.as_str()) == Some("message")
                && e.get("role").and_then(|v| v.as_str()) == Some("assistant");
            if is_assistant {
                e.get("turnId").and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                None
            }
        });

        let mut parts = Vec::new();
        for e in &entries {
            let is_assistant = e.get("type").and_then(|v| v.as_str()) == Some("message")
                && e.get("role").and_then(|v| v.as_str()) == Some("assistant");
            if !is_assistant {
                continue;
            }
            if let Some(ref target_turn) = last_assistant_turn {
                if e.get("turnId").and_then(|v| v.as_str()) != Some(target_turn.as_str()) {
                    continue;
                }
            }
            if let Some(content) = e.get("content") {
                if let Some(arr) = content.as_array() {
                    for block in arr {
                        if let Some(txt) = block.get("text").and_then(|v| v.as_str()) {
                            if !txt.trim().is_empty() {
                                parts.push(txt.to_string());
                            }
                        }
                    }
                } else if let Some(txt) = content.as_str() {
                    if !txt.trim().is_empty() {
                        parts.push(txt.to_string());
                    }
                }
            }
        }

        // Jeśli nie było turnId, bierz ostatnią wypowiedź asystenta
        let text = if last_assistant_turn.is_some() {
            parts.join("\n\n")
        } else {
            parts.pop().unwrap_or_default()
        };

        return ParsedAgentOutput {
            text: clean_ansi(&text),
            session_id,
            is_error: false,
        };
    }

    ParsedAgentOutput {
        text: clean_ansi(stdout),
        session_id: None,
        is_error: false,
    }
}

fn parse_agy_json(stdout: &str) -> ParsedAgentOutput {
    let trimmed = stdout.trim();
    // Szukamy ostatniej linii JSON albo całego obiektu zaczynającego się od '{'
    let parsed = serde_json::from_str::<Value>(trimmed)
        .ok()
        .or_else(|| {
            trimmed
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        })
        .or_else(|| {
            trimmed
                .find('{')
                .and_then(|i| serde_json::from_str::<Value>(&trimmed[i..]).ok())
        });

    if let Some(Value::Object(map)) = parsed {
        let session_id = map
            .get("conversation_id")
            .or_else(|| map.get("conversationId"))
            .or_else(|| map.get("session_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let status = map
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("SUCCESS");
        let is_error = status.eq_ignore_ascii_case("ERROR") || status.eq_ignore_ascii_case("FAILED");
        let response = map
            .get("response")
            .or_else(|| map.get("result"))
            .or_else(|| map.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return ParsedAgentOutput {
            text: clean_ansi(response),
            session_id,
            is_error,
        };
    }

    ParsedAgentOutput {
        text: clean_ansi(stdout),
        session_id: None,
        is_error: false,
    }
}

fn parse_claude_json(stdout: &str) -> ParsedAgentOutput {
    let trimmed = stdout.trim();
    let parsed = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        trimmed
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
    });

    if let Some(Value::Object(map)) = parsed {
        let session_id = map
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let is_error = map
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let result = map
            .get("result")
            .or_else(|| map.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return ParsedAgentOutput {
            text: clean_ansi(result),
            session_id,
            is_error,
        };
    }

    ParsedAgentOutput {
        text: clean_ansi(stdout),
        session_id: None,
        is_error: false,
    }
}

fn parse_codex_jsonl(stdout: &str, codex_out_text: Option<&str>) -> ParsedAgentOutput {
    let mut thread_id: Option<String> = None;
    let mut last_agent_msg: Option<String> = None;

    for line in stdout.lines() {
        let t = line.trim();
        if !t.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            if thread_id.is_none() {
                if let Some(tid) = v.get("thread_id").and_then(|x| x.as_str()) {
                    thread_id = Some(tid.to_string());
                }
            }
            if v.get("type").and_then(|x| x.as_str()) == Some("item.completed") {
                if let Some(item) = v.get("item") {
                    if item.get("type").and_then(|x| x.as_str()) == Some("agent_message") {
                        if let Some(txt) = item.get("text").and_then(|x| x.as_str()) {
                            last_agent_msg = Some(txt.to_string());
                        }
                    }
                }
            }
        }
    }

    let chosen = codex_out_text
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or(last_agent_msg)
        .unwrap_or_else(|| stdout.to_string());

    ParsedAgentOutput {
        text: clean_ansi(&chosen),
        session_id: thread_id,
        is_error: false,
    }
}

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedState {
    next_batch: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    seen: Vec<String>,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    last_activity_unix: u64,
}

/// Trwały, atomowo zapisywany stan instancji mostka (dedup + next_batch + session_id).
#[derive(Debug, Clone)]
pub struct BridgeState {
    json_path: PathBuf,
    max_seen: usize,
    pub next_batch: Option<String>,
    pub session_id: Option<String>,
    seen_order: Vec<String>,
    seen_set: HashSet<String>,
    pub turn_count: u64,
    pub last_activity_unix: u64,
}

pub fn derive_state_json_path(state_file: &str) -> PathBuf {
    if let Some(prefix) = state_file.strip_suffix(".seen.txt") {
        PathBuf::from(format!("{prefix}.state.json"))
    } else if state_file.ends_with(".json") {
        PathBuf::from(state_file)
    } else {
        PathBuf::from(format!("{state_file}.state.json"))
    }
}

impl BridgeState {
    pub fn load(state_file: &str, max_seen: usize) -> Self {
        let json_path = derive_state_json_path(state_file);
        let mut persisted: PersistedState = std::fs::read_to_string(&json_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();

        // Jednorazowa migracja ze starszego pliku *.seen.txt
        if persisted.seen.is_empty() && Path::new(state_file).exists() && state_file != json_path.to_string_lossy() {
            if let Ok(content) = std::fs::read_to_string(state_file) {
                for line in content.lines() {
                    let l = line.trim();
                    if !l.is_empty() {
                        persisted.seen.push(l.to_string());
                    }
                }
            }
        }

        let mut state = Self {
            json_path,
            max_seen: max_seen.max(100),
            next_batch: persisted.next_batch,
            session_id: persisted.session_id,
            seen_order: Vec::new(),
            seen_set: HashSet::new(),
            turn_count: persisted.turn_count,
            last_activity_unix: persisted.last_activity_unix,
        };

        for id in persisted.seen {
            state.mark_seen(id);
        }
        state
    }

    pub fn json_path(&self) -> &Path {
        &self.json_path
    }

    /// Zwraca `true` jeśli event jest NOWY i został właśnie zapisany w `seen`.
    pub fn mark_seen(&mut self, event_id: String) -> bool {
        if !self.seen_set.insert(event_id.clone()) {
            return false;
        }
        self.seen_order.push(event_id);
        if self.seen_order.len() > self.max_seen {
            let overflow = self.seen_order.len() - self.max_seen;
            for old in self.seen_order.drain(..overflow) {
                self.seen_set.remove(&old);
            }
        }
        true
    }

    pub fn set_session(&mut self, sid: Option<String>) {
        self.session_id = sid.filter(|s| !s.trim().is_empty());
        self.last_activity_unix = now_unix();
    }

    pub fn record_turn(&mut self, sid: Option<String>) {
        if let Some(s) = sid.filter(|s| !s.trim().is_empty()) {
            self.session_id = Some(s);
        }
        self.turn_count += 1;
        self.last_activity_unix = now_unix();
    }

    pub fn clear_session(&mut self) {
        self.session_id = None;
        self.turn_count = 0;
        self.last_activity_unix = now_unix();
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.json_path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let persisted = PersistedState {
            next_batch: self.next_batch.clone(),
            session_id: self.session_id.clone(),
            seen: self.seen_order.clone(),
            turn_count: self.turn_count,
            last_activity_unix: self.last_activity_unix,
        };
        let payload = serde_json::to_vec_pretty(&persisted).context("serializacja BridgeState")?;
        let tmp_path = self.json_path.with_extension(format!("json.tmp.{}", std::process::id()));
        {
            let mut file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("create {}", tmp_path.display()))?;
            file.write_all(&payload)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &self.json_path)
            .with_context(|| format!("rename {} -> {}", tmp_path.display(), self.json_path.display()))?;
        Ok(())
    }
}

/// Bezpieczne obcięcie stringa na granicy znaku UTF-8 (nigdy nie panikuje).
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Dzieli długi tekst na części <= `max_chars` znaków (preferując akapity / linie / spacje).
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(200);
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }

    // Zostaw miejsce na prefiks "(12/12)\n"
    let chunk_budget = limit.saturating_sub(16);
    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= chunk_budget {
            chunks.push(remaining.trim_end().to_string());
            break;
        }
        let byte_limit = remaining
            .char_indices()
            .nth(chunk_budget)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
        let window = &remaining[..byte_limit];

        let split_at = window
            .rfind("\n\n")
            .filter(|&i| i >= byte_limit / 3)
            .or_else(|| window.rfind('\n').filter(|&i| i >= byte_limit / 3))
            .or_else(|| window.rfind(' ').filter(|&i| i >= byte_limit / 2))
            .unwrap_or(byte_limit);

        let (head, tail) = remaining.split_at(split_at);
        chunks.push(head.trim_end().to_string());
        remaining = tail.trim_start_matches('\n');
    }

    let total = chunks.len();
    if total <= 1 {
        return chunks;
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| format!("({}/{})\n{c}", i + 1, total))
        .collect()
}

/// Maskuje wrażliwe tokeny/klucze w tekście przed logowaniem lub wysłaniem na Matrix.
pub fn redact(input: &str, secrets: &[String]) -> String {
    let mut out = input.to_string();
    for secret in secrets {
        if secret.len() >= 8 && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// Usuwa kody ANSI/OSC i znaki kontrolne z wyjścia CLI.
pub fn clean_ansi(input: &str) -> String {
    static RE_CSI: OnceLock<Regex> = OnceLock::new();
    static RE_OSC: OnceLock<Regex> = OnceLock::new();
    static RE_CTRL: OnceLock<Regex> = OnceLock::new();

    let re_csi = RE_CSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]").expect("static regex"));
    let re_osc =
        RE_OSC.get_or_init(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").expect("static regex"));
    let re_ctrl =
        RE_CTRL.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").expect("static regex"));

    let s = re_osc.replace_all(input, "");
    let s = re_csi.replace_all(&s, "");
    let s = re_ctrl.replace_all(&s, "");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_utf8_never_panics_on_polish_or_emoji() {
        let s = "Zażółć gęślą jaźń 🚀🔥";
        for max in 0..=s.len() + 2 {
            let cut = truncate_utf8(s, max);
            assert!(cut.len() <= max);
            assert!(s.starts_with(cut));
        }
    }

    #[test]
    fn test_split_message_preserves_multibyte_content() {
        let para = "Zażółć gęślą jaźń — linia testowa.\n";
        let big = para.repeat(30);
        let chunks = split_message(&big, 250);
        assert!(chunks.len() > 2);
        assert!(chunks[0].starts_with("(1/"));
        for c in &chunks {
            assert!(c.chars().count() <= 250);
        }
    }

    #[test]
    fn test_redact_masks_secrets() {
        let secrets = vec!["syt_super_secret_token_12345".to_string()];
        let out = redact("token=syt_super_secret_token_12345 ok", &secrets);
        assert_eq!(out, "token=<redacted> ok");
    }

    #[test]
    fn test_parse_vibe_json_extracts_last_turn_and_session() {
        let raw = r#"Reading API keys from the system keyring
[
  {"type":"message","role":"assistant","sessionId":"sess-1","turnId":"turn-1","content":[{"type":"text","text":"OLD"}]},
  {"type":"message","role":"user","sessionId":"sess-1","turnId":"turn-2","content":[{"type":"text","text":"next"}]},
  {"type":"message","role":"assistant","sessionId":"sess-1","turnId":"turn-2","content":[{"type":"text","text":"BANANA42"}]}
]"#;
        let parsed = parse_agent_output(AgentKind::Vibe, raw, None);
        assert_eq!(parsed.session_id.as_deref(), Some("sess-1"));
        assert_eq!(parsed.text, "BANANA42");
    }

    #[test]
    fn test_parse_agy_json() {
        let raw = r#"{"conversation_id":"conv-42","status":"SUCCESS","response":"MANGO99\n"}"#;
        let parsed = parse_agent_output(AgentKind::Agy, raw, None);
        assert_eq!(parsed.session_id.as_deref(), Some("conv-42"));
        assert_eq!(parsed.text, "MANGO99");
        assert!(!parsed.is_error);
    }

    #[test]
    fn test_parse_claude_json() {
        let raw = r#"{"type":"result","session_id":"cl-7","result":"Cześć!","is_error":false}"#;
        let parsed = parse_agent_output(AgentKind::Claude, raw, None);
        assert_eq!(parsed.session_id.as_deref(), Some("cl-7"));
        assert_eq!(parsed.text, "Cześć!");
        assert!(!parsed.is_error);
    }

    #[test]
    fn test_parse_codex_jsonl() {
        let raw = "{\"type\":\"thread.started\",\"thread_id\":\"th-99\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"from jsonl\"}}\n";
        let parsed = parse_agent_output(AgentKind::Codex, raw, Some("from file"));
        assert_eq!(parsed.session_id.as_deref(), Some("th-99"));
        assert_eq!(parsed.text, "from file");
    }

    #[test]
    fn test_bridge_state_roundtrip_and_cap() {
        let dir = std::env::temp_dir().join(format!("vibe_matrix_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("vibe.seen.txt");
        std::fs::write(&legacy, "$ev1\n$ev2\n").unwrap();

        let mut st = BridgeState::load(&legacy.to_string_lossy(), 100);
        assert!(!st.mark_seen("$ev1".to_string()));
        assert!(st.mark_seen("$ev3".to_string()));
        st.next_batch = Some("s123".to_string());
        st.record_turn(Some("sess-abc".to_string()));
        st.save().unwrap();

        let loaded = BridgeState::load(&legacy.to_string_lossy(), 100);
        assert_eq!(loaded.next_batch.as_deref(), Some("s123"));
        assert_eq!(loaded.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(loaded.turn_count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
