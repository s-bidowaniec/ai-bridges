use crate::error::Result;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerHealth {
    Ready,
    Busy,
    RateLimited,
    Dead,
}

impl PeerHealth {
    pub fn as_str(&self) -> &'static str {
        match self {
            PeerHealth::Ready => "ready",
            PeerHealth::Busy => "busy",
            PeerHealth::RateLimited => "rate_limited",
            PeerHealth::Dead => "dead",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "busy" => PeerHealth::Busy,
            "rate_limited" => PeerHealth::RateLimited,
            "dead" => PeerHealth::Dead,
            _ => PeerHealth::Ready,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub peer: String,
    pub health: PeerHealth,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLogEntry {
    pub id: i64,
    pub caller: Option<String>,
    pub target: String,
    pub resolved_peer: String,
    pub depth: u32,
    pub status: String,
    pub prompt_preview: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffNote {
    pub id: i64,
    pub author: String,
    pub task_summary: String,
    pub next_steps: String,
    pub files_touched: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Blackboard {
    conn: Arc<Mutex<Connection>>,
}

impl Blackboard {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = db_path.as_ref();
        if let Some(parent) = path_ref.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path_ref)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let board = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        board.init_schema()?;
        Ok(board)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS peer_states (
                peer TEXT PRIMARY KEY,
                health TEXT NOT NULL DEFAULT 'ready',
                cooldown_until TEXT,
                last_error TEXT,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS task_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                caller TEXT,
                target TEXT NOT NULL,
                resolved_peer TEXT NOT NULL,
                depth INTEGER NOT NULL,
                status TEXT NOT NULL,
                prompt_preview TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS handoff_notes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                author TEXT NOT NULL,
                task_summary TEXT NOT NULL,
                next_steps TEXT NOT NULL,
                files_touched_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    pub fn ensure_peers(&self, peer_names: &[String]) -> Result<()> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let now = Utc::now().to_rfc3339();
        for peer in peer_names {
            conn.execute(
                "INSERT OR IGNORE INTO peer_states (peer, health, cooldown_until, last_error, updated_at)
                 VALUES (?1, 'ready', NULL, NULL, ?2)",
                params![peer, now],
            )?;
        }
        Ok(())
    }

    /// Returns current peer record, automatically recovering expired cooldowns back to `Ready`.
    pub fn get_peer(&self, peer: &str) -> Result<PeerRecord> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let now = Utc::now();

        let row: Option<(String, Option<String>, Option<String>, String)> = conn
            .query_row(
                "SELECT health, cooldown_until, last_error, updated_at FROM peer_states WHERE peer = ?1",
                params![peer],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();

        match row {
            Some((health_str, cooldown_str, last_error, updated_str)) => {
                let mut health = PeerHealth::parse(&health_str);
                let mut cooldown_until = cooldown_str
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc));

                if matches!(health, PeerHealth::RateLimited | PeerHealth::Dead) {
                    if let Some(until) = cooldown_until {
                        if now >= until {
                            health = PeerHealth::Ready;
                            cooldown_until = None;
                            conn.execute(
                                "UPDATE peer_states
                                 SET health = 'ready', cooldown_until = NULL, last_error = NULL, updated_at = ?2
                                 WHERE peer = ?1",
                                params![peer, now.to_rfc3339()],
                            )?;
                        }
                    }
                }

                let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or(now);

                Ok(PeerRecord {
                    peer: peer.to_string(),
                    health,
                    cooldown_until,
                    last_error,
                    updated_at,
                })
            }
            None => {
                let now_str = now.to_rfc3339();
                conn.execute(
                    "INSERT INTO peer_states (peer, health, cooldown_until, last_error, updated_at)
                     VALUES (?1, 'ready', NULL, NULL, ?2)",
                    params![peer, now_str],
                )?;
                Ok(PeerRecord {
                    peer: peer.to_string(),
                    health: PeerHealth::Ready,
                    cooldown_until: None,
                    last_error: None,
                    updated_at: now,
                })
            }
        }
    }

    pub fn list_peers(&self, configured_peers: &[String]) -> Result<Vec<PeerRecord>> {
        self.ensure_peers(configured_peers)?;
        let mut records = Vec::with_capacity(configured_peers.len());
        for p in configured_peers {
            records.push(self.get_peer(p)?);
        }
        Ok(records)
    }

    pub fn set_peer_busy(&self, peer: &str) -> Result<()> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        conn.execute(
            "UPDATE peer_states SET health = 'busy', updated_at = ?2 WHERE peer = ?1",
            params![peer, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_peer_ready(&self, peer: &str) -> Result<()> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        conn.execute(
            "UPDATE peer_states
             SET health = 'ready', cooldown_until = NULL, last_error = NULL, updated_at = ?2
             WHERE peer = ?1",
            params![peer, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_peer_cooldown(
        &self,
        peer: &str,
        health: PeerHealth,
        cooldown_secs: u64,
        reason: &str,
    ) -> Result<DateTime<Utc>> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let now = Utc::now();
        let until = now + Duration::seconds(cooldown_secs as i64);
        conn.execute(
            "INSERT INTO peer_states (peer, health, cooldown_until, last_error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer) DO UPDATE SET
                health = excluded.health,
                cooldown_until = excluded.cooldown_until,
                last_error = excluded.last_error,
                updated_at = excluded.updated_at",
            params![
                peer,
                health.as_str(),
                until.to_rfc3339(),
                reason,
                now.to_rfc3339()
            ],
        )?;
        Ok(until)
    }

    pub fn record_task(
        &self,
        caller: Option<&str>,
        target: &str,
        resolved_peer: &str,
        depth: u32,
        status: &str,
        prompt: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let preview: String = prompt.chars().take(160).collect();
        conn.execute(
            "INSERT INTO task_history (caller, target, resolved_peer, depth, status, prompt_preview, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                caller,
                target,
                resolved_peer,
                depth,
                status,
                preview,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn record_handoff(
        &self,
        author: &str,
        task_summary: &str,
        next_steps: &str,
        files_touched: &[String],
    ) -> Result<HandoffNote> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let now = Utc::now();
        let files_json = serde_json::to_string(files_touched)?;
        conn.execute(
            "INSERT INTO handoff_notes (author, task_summary, next_steps, files_touched_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![author, task_summary, next_steps, files_json, now.to_rfc3339()],
        )?;
        let id = conn.last_insert_rowid();
        Ok(HandoffNote {
            id,
            author: author.to_string(),
            task_summary: task_summary.to_string(),
            next_steps: next_steps.to_string(),
            files_touched: files_touched.to_vec(),
            created_at: now,
        })
    }

    pub fn recent_handoffs(&self, limit: usize) -> Result<Vec<HandoffNote>> {
        let conn = self.conn.lock().expect("blackboard mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, author, task_summary, next_steps, files_touched_json, created_at
             FROM handoff_notes
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let files_json: String = r.get(4)?;
            let created_str: String = r.get(5)?;
            let files_touched: Vec<String> = serde_json::from_str(&files_json).unwrap_or_default();
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(HandoffNote {
                id: r.get(0)?,
                author: r.get(1)?,
                task_summary: r.get(2)?,
                next_steps: r.get(3)?,
                files_touched,
                created_at,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
