//! bridge-core: odporny klient Matrix (sync/send/reakcje/typing) przez reqwest + Bearer
//! z retry, obsługą 429 retry_after_ms, wykrywaniem edycji m.replace i bezpiecznym UTF-8.
use anyhow::{Context, Result};
use bridge_common::truncate_utf8;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct MatrixClient {
    http: reqwest::Client,
    base: String,
    token: String,
    room_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SyncResponse {
    #[serde(default)]
    pub next_batch: String,
    #[serde(default)]
    pub rooms: Rooms,
}

#[derive(Debug, Deserialize, Default)]
pub struct Rooms {
    #[serde(default)]
    pub join: std::collections::HashMap<String, JoinedRoom>,
}

#[derive(Debug, Deserialize, Default)]
pub struct JoinedRoom {
    #[serde(default)]
    pub timeline: Timeline,
}

#[derive(Debug, Deserialize, Default)]
pub struct Timeline {
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub limited: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Event {
    #[serde(rename = "event_id")]
    pub id: String,
    pub sender: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: EventContent,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RelatesTo {
    #[serde(default)]
    pub rel_type: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct EventContent {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub msgtype: String,
    #[serde(rename = "m.relates_to", default)]
    pub relates_to: Option<RelatesTo>,
}

impl Event {
    pub fn is_edit(&self) -> bool {
        self.content
            .relates_to
            .as_ref()
            .map(|r| r.rel_type == "m.replace")
            .unwrap_or(false)
    }
}

pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn retry_after_from_body(body: &str) -> Option<Duration> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let ms = v.get("retry_after_ms")?.as_u64()?;
    Some(Duration::from_millis(ms.min(60_000)))
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

impl MatrixClient {
    pub fn new(base: &str, token: &str, room_id: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            room_id: room_id.to_string(),
        })
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }

    pub async fn sync(&self, since: Option<&str>, timeout_ms: u32) -> Result<SyncResponse> {
        let mut url = format!("{}/_matrix/client/v3/sync?timeout={}", self.base, timeout_ms);
        if let Some(s) = since {
            url.push_str(&format!("&since={}", enc(s)));
        }
        let req_timeout = Duration::from_millis(timeout_ms as u64 + 35_000);
        let resp = self
            .authed(self.http.get(&url))
            .timeout(req_timeout)
            .send()
            .await
            .context("sync: send")?;
        let status = resp.status();
        let body = resp.text().await.context("sync: read body")?;
        if !status.is_success() {
            anyhow::bail!("sync: HTTP {status}: {}", truncate_utf8(&body, 300));
        }
        let parsed: SyncResponse = serde_json::from_str(&body).context("sync: parse json")?;
        Ok(parsed)
    }

    /// Wysyła wiadomość tekstową z idempotentnym `txn_id` i retry przy 429/5xx/błędach sieci.
    pub async fn send_text(&self, body: &str) -> Result<String> {
        let txn = format!("rs{}", now_nanos());
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.base,
            enc(&self.room_id),
            enc(&txn)
        );
        let payload = serde_json::json!({"msgtype": "m.text", "body": body});
        let delays = [1u64, 2, 4, 8, 15];

        for (attempt, &delay_secs) in delays.iter().enumerate() {
            let res = self
                .authed(self.http.put(&url))
                .timeout(Duration::from_secs(30))
                .json(&payload)
                .send()
                .await;

            match res {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        let v: serde_json::Value =
                            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                        return Ok(v["event_id"].as_str().unwrap_or("").to_string());
                    }
                    if is_retryable_status(status) && attempt + 1 < delays.len() {
                        let wait = retry_after_from_body(&text)
                            .unwrap_or_else(|| Duration::from_secs(delay_secs));
                        tracing::warn!(
                            attempt = attempt + 1,
                            %status,
                            wait_ms = wait.as_millis() as u64,
                            "send_text retryable HTTP error: {}",
                            truncate_utf8(&text, 200)
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    anyhow::bail!("send_text: HTTP {status}: {}", truncate_utf8(&text, 300));
                }
                Err(err) => {
                    if attempt + 1 < delays.len() {
                        tracing::warn!(
                            attempt = attempt + 1,
                            delay_secs,
                            "send_text network error: {err} — retry"
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        continue;
                    }
                    return Err(err).context("send_text: send");
                }
            }
        }
        anyhow::bail!("send_text: wyczerpano próby retry")
    }

    pub async fn react(&self, event_id: &str, emoji: &str) -> Result<()> {
        // Reakcje przez send/m.reaction z key wewnątrz m.relates_to (zgodne z tym Synapse).
        let txn = format!("rx{}", now_nanos());
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.reaction/{}",
            self.base,
            enc(&self.room_id),
            enc(&txn)
        );
        let payload = serde_json::json!({
            "m.relates_to": {"rel_type": "m.annotation", "event_id": event_id, "key": emoji}
        });

        for attempt in 0..3 {
            match self
                .authed(self.http.put(&url))
                .timeout(Duration::from_secs(15))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if is_retryable_status(status) && attempt < 2 {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
                    tracing::warn!(
                        emoji,
                        event_id,
                        %status,
                        "reakcja nieudana: {}",
                        truncate_utf8(&text, 200)
                    );
                    return Ok(());
                }
                Err(err) => {
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
                    tracing::warn!(emoji, event_id, "reakcja network error: {err}");
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Wysyła wskaźnik pisania (`m.typing`) w pokoju.
    pub async fn set_typing(&self, bot_user_id: &str, typing: bool, timeout_ms: u64) {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/typing/{}",
            self.base,
            enc(&self.room_id),
            enc(bot_user_id)
        );
        let payload = if typing {
            serde_json::json!({"typing": true, "timeout": timeout_ms})
        } else {
            serde_json::json!({"typing": false})
        };
        if let Err(err) = self
            .authed(self.http.put(&url))
            .timeout(Duration::from_secs(10))
            .json(&payload)
            .send()
            .await
        {
            tracing::debug!("typing indicator error: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enc_room_id() {
        assert_eq!(enc("!abc:example.org"), "%21abc%3Aexample.org");
    }

    #[test]
    fn test_event_is_edit() {
        let raw = r#"{
            "event_id": "$1",
            "sender": "@u:ex",
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "* fixed",
                "m.relates_to": {"rel_type": "m.replace", "event_id": "$0"}
            }
        }"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        assert!(ev.is_edit());
    }
}
