//! Run against a throwaway loopback Synapse with registration enabled.
use anyhow::{ensure, Context, Result};
use bridge_common::Config;
use bridge_core::MatrixClient;
use matrix_sdk::{
    attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo},
    config::SyncSettings,
    deserialized_responses::{TimelineEventKind, VerificationState},
    media::{MediaFormat, MediaRequestParameters},
    ruma::{
        api::client::{
            room::create_room,
            uiaa::{AuthData, Password},
        },
        events::room::{message::RoomMessageEventContent, EncryptedFile, MediaSource},
    },
    Client,
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-E2EE-image";
const AUDIO_BYTES: &[u8] = b"OggS-synthetic-E2EE-audio";

async fn create_account(url: &str, name: &str, password: &str) -> Result<Client> {
    let response = matrix_sdk::reqwest::Client::new()
        .post(format!("{url}/_matrix/client/v3/register"))
        .header("content-type", "application/json")
        .body(
            json!({
                "username": name, "password": password, "auth": {"type":"m.login.dummy"},
                "inhibit_login": true
            })
            .to_string(),
        )
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "register: {}",
        response.text().await?
    );
    let client = login(url, name, password).await?;
    let encryption = client.encryption();
    if let Err(error) = encryption.bootstrap_cross_signing(None).await {
        let Some(uiaa) = error.as_uiaa_response() else {
            return Err(error.into());
        };
        let mut auth = Password::new(
            client.user_id().unwrap().to_owned().into(),
            password.to_owned(),
        );
        auth.session = uiaa.session.clone();
        encryption
            .bootstrap_cross_signing(Some(AuthData::Password(auth)))
            .await?;
    }
    fresh_sync(&client).await?;
    Ok(client)
}

async fn login(url: &str, name: &str, password: &str) -> Result<Client> {
    let client = Client::builder().homeserver_url(url).build().await?;
    client
        .matrix_auth()
        .login_username(name, password)
        .send()
        .await?;
    Ok(client)
}

async fn fresh_sync(client: &Client) -> Result<()> {
    client
        .sync_once(SyncSettings::new().timeout(Duration::ZERO))
        .await?;
    Ok(())
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable loopback Synapse with registration enabled"]
async fn verified_encrypted_text_and_media_round_trip() -> Result<()> {
    let url = std::env::var("MATRIX_TEST_URL")?;
    let parsed = matrix_sdk::reqwest::Url::parse(&url)?;
    ensure!(
        parsed.host_str() == Some("127.0.0.1"),
        "use only a local test server"
    );
    let suffix = format!(
        "{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let root = std::env::temp_dir().join(format!("bridge-e2ee-test-{suffix}"));
    bridge_common::private_directory(&root)?;
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        run_round_trip(&url, &suffix, &root),
    )
    .await;
    let _ = fs::remove_dir_all(&root);
    result.context("test E2EE exceeded its safety deadline")?
}

async fn run_round_trip(url: &str, suffix: &str, root: &Path) -> Result<()> {
    let bot_name = format!("bot_{suffix}");
    let user_name = format!("operator_{suffix}");
    let password = format!("synthetic-{suffix}");
    let bot_owner = create_account(url, &bot_name, &password)
        .await
        .context("provisioning bot")?;
    let operator = create_account(url, &user_name, &password)
        .await
        .context("provisioning operator")?;
    let recovery = bot_owner.encryption().recovery().enable().await?;
    let bot_session = login(url, &bot_name, &password).await?;
    let bot_id = bot_session.user_id().unwrap();
    let user_id = operator.user_id().unwrap();

    let mut request = create_room::v3::Request::new();
    request.invite.push(user_id.to_owned());
    request.is_direct = true;
    let bot_room = bot_owner.create_room(request).await?;
    bot_room.enable_encryption().await?;
    let operator_room = operator.join_room_by_id(bot_room.room_id()).await?;
    fresh_sync(&operator).await?;

    let recovery_file = root.join("recovery");
    private_file(&recovery_file, recovery.as_bytes())?;
    let passphrase_file = root.join("passphrase");
    private_file(
        &passphrase_file,
        format!("store-passphrase-{suffix}").as_bytes(),
    )?;

    // This test is a standalone integration-test binary and runs a single case.
    std::env::remove_var("MATRIX_ACCESS_TOKEN_FILE");
    std::env::remove_var("MATRIX_ALLOW_ALL_ROOM_MEMBERS");
    std::env::set_var("ALLOW_INSECURE_HTTP", "1");
    std::env::set_var("MATRIX_HOMESERVER", url);
    std::env::set_var("MATRIX_ROOM_ID", bot_room.room_id().as_str());
    std::env::set_var("MATRIX_BOT_USER_ID", bot_id.as_str());
    std::env::set_var(
        "MATRIX_BOT_DEVICE_ID",
        bot_session.device_id().unwrap().as_str(),
    );
    std::env::set_var("MATRIX_ACCESS_TOKEN", bot_session.access_token().unwrap());
    std::env::set_var("MATRIX_ALLOWED_USERS", user_id.as_str());
    std::env::set_var("MATRIX_STORE_DIR", root.join("store"));
    std::env::set_var("MATRIX_STORE_PASSPHRASE_FILE", passphrase_file);

    let config = Config::from_env()?;
    let bridge = MatrixClient::new(&config).await?;
    assert!(MatrixClient::new(&config).await.is_err());
    bridge
        .recover_device(&recovery_file)
        .await
        .context("recovering bridge device")?;

    let user_identity = bot_owner
        .encryption()
        .request_user_identity(user_id)
        .await?
        .unwrap();
    let public_key = user_identity
        .master_key()
        .get_first_key()
        .unwrap()
        .to_base64();
    assert!(bridge.verify_user(user_id.as_str(), "wrong").await.is_err());
    bridge.verify_user(user_id.as_str(), &public_key).await?;
    operator
        .encryption()
        .request_user_identity(bot_id)
        .await?
        .unwrap()
        .verify()
        .await?;
    ensure!(
        operator
            .encryption()
            .request_user_identity(bot_id)
            .await?
            .is_some_and(|id| id.is_verified()),
        "operator did not trust the bridge account"
    );

    let initial = bridge.sync(None, 0).await?;
    let outbound_id = bridge.send_text("encrypted terminal output").await?;
    let inbound_id = operator_room
        .send(RoomMessageEventContent::text_plain(
            "encrypted terminal input",
        ))
        .await?
        .response
        .event_id;
    let image_config =
        AttachmentConfig::new().info(AttachmentInfo::Image(BaseImageInfo::default()));
    let inbound_image = operator_room
        .send_attachment(
            "map.png",
            &"image/png".parse()?,
            PNG_BYTES.to_vec(),
            image_config,
        )
        .await?
        .event_id;
    let outbound_image = bridge
        .send_image("out.png", "image/png", PNG_BYTES.to_vec(), "test-image")
        .await?;
    let outbound_audio = bridge
        .send_audio(
            "out.ogg",
            "audio/ogg",
            AUDIO_BYTES.to_vec(),
            Duration::from_secs(1),
        )
        .await?;

    let operator_sync = operator
        .sync_once(SyncSettings::new().timeout(Duration::ZERO))
        .await?;
    let events = &operator_sync
        .rooms
        .joined
        .get(bot_room.room_id())
        .unwrap()
        .timeline
        .events;
    for (expected, media) in [
        (&outbound_id, None),
        (&outbound_image, Some(PNG_BYTES)),
        (&outbound_audio, Some(AUDIO_BYTES)),
    ] {
        let event = events
            .iter()
            .find(|event| event.event_id().is_some_and(|id| id.as_str() == expected))
            .context("outbound event missing from sync")?;
        match &event.kind {
            TimelineEventKind::Decrypted(_) => ensure!(
                event
                    .encryption_info()
                    .is_some_and(|info| info.verification_state == VerificationState::Verified),
                "outbound event not verified: {:?}",
                event.encryption_info().map(|info| &info.verification_state)
            ),
            TimelineEventKind::UnableToDecrypt { utd_info, .. } => {
                anyhow::bail!("outbound event could not be decrypted: {utd_info:?}")
            }
            _ => anyhow::bail!("outbound event was not encrypted"),
        }
        if let Some(media) = media {
            let message: serde_json::Value = serde_json::from_str(event.raw().json().get())?;
            let file: EncryptedFile = serde_json::from_value(message["content"]["file"].clone())?;
            let request = MediaRequestParameters {
                source: MediaSource::Encrypted(Box::new(file)),
                format: MediaFormat::File,
            };
            assert_eq!(
                operator.media().get_media_content(&request, false).await?,
                media
            );
        }
    }
    let batch = bridge.sync(Some(&initial.next_batch), 0).await?;
    assert!(batch
        .events
        .iter()
        .any(|event| event.id == inbound_id.as_str()
            && event.content.body == "encrypted terminal input"));
    let event = batch
        .events
        .iter()
        .find(|event| event.id == inbound_image.as_str())
        .context("missing verified encrypted image")?;
    let (_, downloaded, mime) = bridge.download_mxc(&event.content, 1024).await?;
    assert_eq!(downloaded, PNG_BYTES);
    assert_eq!(mime, "image/png");
    assert!(bridge.download_mxc(&event.content, 3).await.is_err());

    let other_room = bot_owner
        .create_room(create_room::v3::Request::new())
        .await?;
    let mut plain_config = config.clone();
    plain_config.room_id = other_room.room_id().to_string();
    drop(bridge);
    let reopened = MatrixClient::new(&plain_config).await?;
    reopened.check_device().await?;
    assert!(reopened.sync(None, 0).await.is_err());
    assert!(reopened.send_text("should never be public").await.is_err());
    Ok(())
}
