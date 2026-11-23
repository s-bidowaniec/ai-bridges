//! Matrix E2EE over the SDK, with verified-device and room membership checks.
use anyhow::{bail, ensure, Context, Result};
use bridge_common::{private_directory, read_private_secret, Config};
use matrix_sdk::{
    attachment::{AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseImageInfo},
    authentication::matrix::MatrixSession,
    config::{SyncSettings, SyncToken},
    deserialized_responses::{TimelineEvent, VerificationState},
    reqwest::{redirect::Policy, Url},
    ruma::{
        api::client::{
            filter::FilterDefinition, membership::joined_members, sync::sync_events::v3::Filter,
        },
        events::{room::message::RoomMessageEventContent, room::EncryptedFile},
        OwnedRoomId, RoomId, UserId,
    },
    Client, Room, RoomState, SessionMeta, SessionTokens,
};
use matrix_sdk_crypto::{
    AttachmentDecryptor, CollectStrategy, DecryptionSettings, TrustRequirement,
};
use secrecy::ExposeSecret;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Cursor, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct MatrixClient {
    client: Client,
    http: matrix_sdk::reqwest::Client,
    homeserver: Url,
    room_id: OwnedRoomId,
    audience: Option<Arc<HashSet<String>>>,
    _store_lock: Arc<File>,
}

pub struct SyncBatch {
    pub next_batch: String,
    pub events: Vec<Event>,
}

#[derive(Deserialize, Clone)]
pub struct Event {
    #[serde(rename = "event_id")]
    pub id: String,
    pub sender: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: EventContent,
}

#[derive(Deserialize, Clone, Default)]
pub struct EventContent {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub msgtype: String,
    #[serde(default)]
    file: Option<EncryptedFile>,
    #[serde(default)]
    info: Option<serde_json::Value>,
    #[serde(rename = "m.relates_to", default)]
    pub relates_to: Option<serde_json::Value>,
}

fn authenticated_event(event: &TimelineEvent) -> Option<Event> {
    let info = event.encryption_info()?;
    if info.verification_state != VerificationState::Verified || info.forwarder.is_some() {
        return None;
    }
    let event: Event = serde_json::from_str(event.raw().json().get()).ok()?;
    (event.sender == info.sender.as_str()).then_some(event)
}

fn homeserver_url(value: &str, allow_loopback: bool) -> Result<Url> {
    let url = Url::parse(value).context("niepoprawny MATRIX_HOMESERVER")?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (allow_loopback && url.scheme() == "http" && loopback),
        "MATRIX_HOMESERVER wymaga HTTPS; HTTP jest dostępne tylko dla testów przez loopback"
    );
    ensure!(
        url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "MATRIX_HOMESERVER musi być adresem bez danych logowania, parametrów i fragmentu"
    );
    Ok(url)
}

impl MatrixClient {
    pub async fn new(config: &Config) -> Result<Self> {
        let homeserver = homeserver_url(&config.homeserver, config.allow_insecure_loopback)?;
        let room_id = RoomId::parse(&config.room_id).context("niepoprawny MATRIX_ROOM_ID")?;
        let user_id =
            UserId::parse(&config.bot_user_id).context("niepoprawny MATRIX_BOT_USER_ID")?;
        ensure!(
            !config.matrix_device_id.trim().is_empty(),
            "MATRIX_BOT_DEVICE_ID jest pusty"
        );
        private_directory(&config.matrix_store_dir).context("katalog stanu E2EE")?;
        let store_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .mode(0o600)
            .open(config.matrix_store_dir.join("bridge.lock"))?;
        store_lock
            .try_lock()
            .context("magazyn E2EE jest używany przez inną instancję")?;
        let passphrase = read_private_secret(&config.matrix_store_passphrase_file)
            .context("odczyt hasła magazynu E2EE")?;
        let http = matrix_sdk::reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(90))
            .build()?;
        let client = Client::builder()
            .homeserver_url(homeserver.as_str())
            .http_client(http.clone())
            .sqlite_store(&config.matrix_store_dir, Some(passphrase.expose_secret()))
            .with_enable_share_history_on_invite(false)
            .with_room_key_recipient_strategy(CollectStrategy::OnlyTrustedDevices)
            .with_decryption_settings(DecryptionSettings {
                sender_device_trust_requirement: TrustRequirement::CrossSigned,
            })
            .build()
            .await
            .context("inicjalizacja klienta E2EE")?;
        client
            .restore_session(MatrixSession {
                meta: SessionMeta {
                    user_id: user_id.clone(),
                    device_id: config.matrix_device_id.as_str().into(),
                },
                tokens: SessionTokens {
                    access_token: config.token.expose_secret().to_owned(),
                    refresh_token: None,
                },
            })
            .await?;
        let identity = client
            .whoami()
            .await
            .context("sprawdzenie konta i urządzenia Matrix")?;
        ensure!(
            identity.user_id == user_id
                && identity
                    .device_id
                    .as_deref()
                    .is_some_and(|device| device.as_str() == config.matrix_device_id),
            "token Matrix nie należy do wskazanego konta i urządzenia"
        );
        Ok(Self {
            client,
            http,
            homeserver,
            room_id,
            audience: config.room_audience().map(Arc::new),
            _store_lock: Arc::new(store_lock),
        })
    }

    pub async fn recover_device(&self, key_file: &Path) -> Result<()> {
        let key = read_private_secret(key_file).context("odczyt klucza odzyskiwania Matrix")?;
        self.client
            .sync_once(SyncSettings::new().timeout(Duration::ZERO))
            .await?;
        self.client
            .encryption()
            .recovery()
            .recover(key.expose_secret())
            .await?;
        self.check_device().await
    }

    pub async fn verify_user(&self, mxid: &str, expected_key: &str) -> Result<()> {
        ensure!(
            self.audience
                .as_ref()
                .is_none_or(|audience| audience.contains(mxid))
                && self
                    .client
                    .user_id()
                    .is_some_and(|own| own.as_str() != mxid),
            "weryfikowany użytkownik musi być na MATRIX_ALLOWED_USERS"
        );
        self.check_device().await?;
        let user = UserId::parse(mxid)?;
        let identity = self
            .client
            .encryption()
            .request_user_identity(&user)
            .await?
            .context("użytkownik Matrix nie ma tożsamości cross-signing")?;
        let key = identity
            .master_key()
            .get_first_key()
            .context("brak klucza głównego Matrix")?;
        ensure!(
            key.to_base64() == expected_key.trim_end_matches('='),
            "odcisk klucza Matrix różni się od sprawdzonego poza mostkiem"
        );
        identity.verify().await?;
        ensure!(
            self.client
                .encryption()
                .request_user_identity(&user)
                .await?
                .is_some_and(|identity| identity.is_verified()),
            "tożsamość Matrix nie została zweryfikowana"
        );
        Ok(())
    }

    pub async fn check_device(&self) -> Result<()> {
        ensure!(
            self.client
                .encryption()
                .get_own_device()
                .await?
                .is_some_and(|device| device.is_verified_with_cross_signing()),
            "urządzenie mostka wymaga weryfikacji cross-signing; użyj recover-device"
        );
        Ok(())
    }

    fn encrypted_room(&self) -> Result<Room> {
        let room = self
            .client
            .get_room(&self.room_id)
            .context("konto mostka musi dołączyć do pokoju Matrix")?;
        ensure!(
            room.state() == RoomState::Joined,
            "konto mostka nie jest uczestnikiem pokoju"
        );
        ensure!(
            room.encryption_state().is_encrypted(),
            "pokój Matrix musi mieć włączone E2EE"
        );
        Ok(room)
    }

    async fn ready_to_send(&self) -> Result<Room> {
        self.check_device().await?;
        let room = self.encrypted_room()?;
        if let Some(audience) = &self.audience {
            let response = self
                .client
                .send(joined_members::v3::Request::new(self.room_id.clone()))
                .await
                .context("sprawdzenie uczestników pokoju Matrix")?;
            ensure!(
                !response.joined.is_empty()
                    && response
                        .joined
                        .keys()
                        .all(|user| audience.contains(user.as_str())),
                "wstrzymano publikację: w pokoju są uczestnicy spoza MATRIX_ALLOWED_USERS"
            );
        }
        Ok(room)
    }

    pub async fn sync(&self, since: Option<&str>, timeout_ms: u32) -> Result<SyncBatch> {
        let mut filter = FilterDefinition::default();
        filter.room.rooms = Some(vec![self.room_id.clone()]);
        let settings = SyncSettings::new()
            .filter(Filter::FilterDefinition(filter))
            .timeout(Duration::from_millis(u64::from(timeout_ms)))
            .token(since.map_or(SyncToken::NoToken, |value| {
                SyncToken::Specific(value.to_owned())
            }));
        let response = self
            .client
            .sync_once(settings)
            .await
            .context("synchronizacja Matrix E2EE")?;
        self.encrypted_room()?;
        if since.is_none() {
            self.check_device().await?;
        }
        let events = response
            .rooms
            .joined
            .get(&self.room_id)
            .into_iter()
            .flat_map(|room| &room.timeline.events)
            .filter_map(authenticated_event)
            .collect();
        Ok(SyncBatch {
            next_batch: response.next_batch,
            events,
        })
    }

    pub async fn send_text(&self, body: &str) -> Result<String> {
        let room = self.ready_to_send().await?;
        let event = room.send(RoomMessageEventContent::text_plain(body)).await?;
        Ok(event.response.event_id.to_string())
    }

    /// Download encrypted bytes from the authenticated homeserver, never from a URL
    /// supplied by the sender. The SDK decryptor verifies the ciphertext digest.
    pub async fn download_mxc(
        &self,
        content: &EventContent,
        limit: usize,
    ) -> Result<(String, Vec<u8>, String)> {
        let file = content
            .file
            .as_ref()
            .context("odrzucono niezaszyfrowany załącznik Matrix")?;
        let (server, media_id) = file.url.parts()?;
        let mut url = self.homeserver.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("nie można zbudować adresu mediów Matrix"))?
            .pop_if_empty()
            .extend([
                "_matrix",
                "client",
                "v1",
                "media",
                "download",
                server.as_str(),
                media_id,
            ]);
        let token = self.client.access_token().context("brak sesji Matrix")?;
        let mut response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response.status().is_success(),
            "nieoczekiwany status pobrania mediów"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= limit as u64),
            "plik Matrix przekracza {limit} bajtów"
        );
        let mut ciphertext = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= limit - ciphertext.len(),
                "plik Matrix przekracza {limit} bajtów"
            );
            ciphertext.extend_from_slice(&chunk);
        }
        let mut ciphertext = Cursor::new(ciphertext);
        let mut reader = AttachmentDecryptor::new(&mut ciphertext, file.clone().into())?;
        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .context("weryfikacja i odszyfrowanie załącznika")?;
        let mime = content
            .info
            .as_ref()
            .and_then(|info| info["mimetype"].as_str())
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok((content.body.clone(), plaintext, mime))
    }

    pub async fn send_image(
        &self,
        filename: &str,
        mime: &str,
        bytes: Vec<u8>,
        transaction: &str,
    ) -> Result<String> {
        let config =
            AttachmentConfig::new()
                .txn_id(transaction.into())
                .info(AttachmentInfo::Image(BaseImageInfo {
                    size: bytes.len().try_into().ok(),
                    ..Default::default()
                }));
        self.send_attachment(filename, mime, bytes, config).await
    }

    pub async fn send_audio(
        &self,
        filename: &str,
        mime: &str,
        bytes: Vec<u8>,
        duration: Duration,
    ) -> Result<String> {
        let info = BaseAudioInfo {
            duration: Some(duration),
            size: bytes.len().try_into().ok(),
            waveform: None,
        };
        let config = AttachmentConfig::new().info(if mime == "audio/ogg" {
            AttachmentInfo::Voice(info)
        } else {
            AttachmentInfo::Audio(info)
        });
        self.send_attachment(filename, mime, bytes, config).await
    }

    async fn send_attachment(
        &self,
        filename: &str,
        mime: &str,
        bytes: Vec<u8>,
        config: AttachmentConfig,
    ) -> Result<String> {
        let room = self.ready_to_send().await?;
        let Ok(mime) = mime.parse() else {
            bail!("nieprawidłowy typ załącznika")
        };
        let event = room.send_attachment(filename, &mime, bytes, config).await?;
        Ok(event.event_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{deserialized_responses::TimelineEvent, ruma::serde::Raw};

    #[test]
    fn plaintext_commands_are_never_authenticated() {
        let json = serde_json::json!({
            "type": "m.room.message", "sender": "@operator:example.org", "event_id": "$1",
            "origin_server_ts": 100, "content": {"msgtype": "m.text", "body": "/new work"}
        });
        let event = TimelineEvent::from_plaintext(Raw::from_json_string(json.to_string()).unwrap());
        assert!(authenticated_event(&event).is_none());
    }

    #[test]
    fn token_is_only_used_with_https_or_explicit_test_loopback() {
        assert!(homeserver_url("https://matrix.example.org", false).is_ok());
        assert!(homeserver_url("http://matrix.example.org", true).is_err());
        assert!(homeserver_url("https://user:password@matrix.example.org", false).is_err());
        assert!(homeserver_url("http://127.0.0.1:8008", false).is_err());
        assert!(homeserver_url("http://127.0.0.1:8008", true).is_ok());
    }
}
