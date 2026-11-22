//! bridge-core: minimalny klient Matrix (sync/send/reakcje) przez reqwest + Bearer.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub struct EventContent {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub msgtype: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub file: Option<serde_json::Value>,
    #[serde(default)]
    pub info: Option<serde_json::Value>,
}

fn enc(s: &str) -> String {
    // percent-encode dla path (room id ma ! i :)
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

impl MatrixClient {
    pub fn new(base: &str, token: &str, room_id: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
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
        let resp = self
            .authed(self.http.get(&url))
            .send()
            .await
            .context("sync: send")?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("sync: HTTP {status}: {}", &body[..body.len().min(300)]);
        }
        let parsed: SyncResponse =
            serde_json::from_str(&body).context("sync: parse json")?;
        Ok(parsed)
    }

    pub async fn send_text(&self, body: &str) -> Result<String> {
        self.send_text_rel(body, None).await
    }

    /// send_text z opcjonalnym m.relates_to (np. wątek m.thread).
    pub async fn send_text_rel(&self, body: &str, rel: Option<serde_json::Value>) -> Result<String> {
        let txn = format!("rs{}", now_nanos());
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.base,
            enc(&self.room_id),
            enc(&txn)
        );
        let mut content = serde_json::json!({"msgtype": "m.text", "body": body});
        if let Some(r) = rel {
            content["m.relates_to"] = r;
        }
        let resp = self
            .authed(self.http.put(&url))
            .json(&content)
            .send()
            .await
            .context("send_text: send")?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("send_text: HTTP {status}: {}", &body[..body.len().min(300)]);
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(v["event_id"].as_str().unwrap_or("").to_string())
    }

    pub async fn react(&self, event_id: &str, emoji: &str) -> Result<()> {
        // reakcję wysyłamy jako zwykły event m.reaction; key MUSI być wewnątrz m.relates_to.
        let txn = format!("rx{}", now_nanos());
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.reaction/{}",
            self.base,
            enc(&self.room_id),
            enc(&txn)
        );
        let resp = self
            .authed(self.http.put(&url))
            .json(&serde_json::json!({
                "m.relates_to": {"rel_type": "m.annotation", "event_id": event_id, "key": emoji}
            }))
            .send()
            .await
            .context("react: send")?;
        if !resp.status().is_success() {
            let b = resp.text().await.unwrap_or_default();
            tracing::warn!(emoji, event_id, "reakcja nieudana: {}", &b[..b.len().min(200)]);
        }
        Ok(())
    }

    /// Pobiera media z mxc://server/media_id. Zwraca (filename, bytes, mime).
    pub async fn download_mxc(&self, mxc: &str) -> Result<(String, Vec<u8>, String)> {
        let rest = mxc.strip_prefix("mxc://").context("zły mxc (brak mxc://)")?;
        let (server, media_id) = rest.split_once('/').context("zły mxc (brak /)")?;
        let url = format!(
            "{}/_matrix/media/v3/download/{}/{}",
            self.base,
            enc(server),
            enc(media_id)
        );
        let resp = self.authed(self.http.get(&url)).send().await.context("download: send")?;
        let status = resp.status();
        let mime = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_string();
        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await.context("download: body")?.to_vec();
        if !status.is_success() {
            anyhow::bail!("download: HTTP {status} ({} bajtów)", bytes.len());
        }
        let filename = cd
            .split("filename=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{media_id}.ogg"));
        Ok((filename, bytes, mime))
    }

    /// Upload media na homeserver. Zwraca mxc://...
    pub async fn upload_media(&self, bytes: &[u8], filename: &str, mime: &str) -> Result<String> {
        let url = format!("{}/_matrix/media/v3/upload?filename={}", self.base, enc(filename));
        let resp = self
            .authed(self.http.post(&url))
            .header("content-type", mime)
            .body(bytes.to_vec())
            .send()
            .await
            .context("upload: send")?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("upload: HTTP {status}: {}", &body[..body.len().min(300)]);
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(v["content_uri"].as_str().context("upload: brak content_uri")?.to_string())
    }

    /// Wysyła voice message (m.audio + org.matrix.msc3245.voice).
    pub async fn send_audio_rel(&self, body: &str, mxc: &str, mime: &str, size: usize, duration_ms: u32, waveform: &[u32], rel: Option<serde_json::Value>) -> Result<String> {
        let txn = format!("au{}", now_nanos());
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.base,
            enc(&self.room_id),
            enc(&txn)
        );
        let mut content = serde_json::json!({
            "msgtype": "m.audio",
            "body": body,
            "url": mxc,
            "info": { "mimetype": mime, "size": size, "duration": duration_ms },
            "org.matrix.msc3245.voice": true,
            "org.matrix.msc1767.audio": [{ "waveform": waveform, "duration": duration_ms }],
        });
        if let Some(r) = rel {
            content["m.relates_to"] = r;
        }
        let resp = self
            .authed(self.http.put(&url))
            .json(&content)
            .send()
            .await
            .context("send_audio: send")?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("send_audio: HTTP {status}: {}", &body[..body.len().min(300)]);
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(v["event_id"].as_str().unwrap_or("").to_string())
    }

    pub async fn send_audio(&self, body: &str, mxc: &str, mime: &str, size: usize, duration_ms: u32, waveform: &[u32]) -> Result<String> {
        self.send_audio_rel(body, mxc, mime, size, duration_ms, waveform, None).await
    }
}
