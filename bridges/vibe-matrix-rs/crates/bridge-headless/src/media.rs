use anyhow::{Context, Result};
use bridge_common::{private_directory, Config};
use bridge_core::{Event, MatrixClient};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

pub const MAX_MEDIA_BYTES: usize = 20 * 1024 * 1024;

fn image_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}

pub fn image_outbox(cfg: &Config) -> PathBuf {
    Path::new(&cfg.uploads_dir).join("images-out")
}

pub async fn receive_image(cfg: &Config, client: &MatrixClient, event: &Event) -> Result<PathBuf> {
    let (_, bytes, _) = client.download_mxc(&event.content, MAX_MEDIA_BYTES).await?;
    let (extension, _) = image_type(&bytes).context("obsługiwane obrazy: PNG, JPEG, GIF i WebP")?;
    let directory = Path::new(&cfg.uploads_dir).join("images-in");
    private_directory(&directory)?;
    let directory = fs::canonicalize(directory).await?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let file = directory.join(format!("{time:x}.{extension}"));
    fs::write(&file, bytes)
        .await
        .context("zapis obrazu Matrix")?;
    Ok(file)
}

async fn send_image(
    client: &MatrixClient,
    path: &Path,
    metadata: &std::fs::Metadata,
    transaction: &str,
) -> Result<String> {
    anyhow::ensure!(
        metadata.len() <= MAX_MEDIA_BYTES as u64,
        "obraz przekracza limit 20 MiB"
    );
    let bytes = fs::read(path).await.context("odczyt obrazu z outboxa")?;
    anyhow::ensure!(
        bytes.len() <= MAX_MEDIA_BYTES,
        "obraz przekracza limit 20 MiB"
    );
    let (_, mime) = image_type(&bytes).context("nieobsługiwany format obrazu")?;
    let filename = path
        .file_name()
        .context("brak nazwy obrazu")?
        .to_string_lossy()
        .into_owned();
    client.send_image(&filename, mime, bytes, transaction).await
}

pub async fn watch_images(client: MatrixClient, outbox: PathBuf) {
    let mut pending = HashMap::new();
    let mut sent = std::collections::HashSet::new();
    let mut ticks = tokio::time::interval(Duration::from_secs(2));
    loop {
        ticks.tick().await;
        let mut directory = match fs::read_dir(&outbox).await {
            Ok(directory) => directory,
            Err(error) => {
                tracing::warn!(%error, "nie udało się otworzyć outboxa obrazów");
                continue;
            }
        };
        while let Ok(Some(entry)) = directory.next_entry().await {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || !name.rsplit_once('.').is_some_and(|(_, extension)| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "webp"
                    )
                })
            {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path).await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            if !sent.contains(&path) {
                let transaction = pending.entry(path.clone()).or_insert_with(|| {
                    let time = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();
                    format!("image-{time:x}")
                });
                match send_image(&client, &path, &metadata, transaction).await {
                    Ok(event) => tracing::info!(%event, "wysłano obraz z outboxa"),
                    Err(error) => {
                        tracing::warn!(%error, "wysłanie obrazu Matrix nie powiodło się");
                        continue;
                    }
                }
                pending.remove(&path);
                sent.insert(path.clone());
            }
            match fs::remove_file(&path).await {
                Ok(()) => {
                    sent.remove(&path);
                }
                Err(error) => tracing::warn!(%error, "nie można usunąć wysłanej kopii obrazu"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_content_controls_type() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n"), Some(("png", "image/png")));
        assert_eq!(image_type(b"RIFFxxxxWEBP"), Some(("webp", "image/webp")));
        assert_eq!(image_type(b"GIF89a"), Some(("gif", "image/gif")));
        assert_eq!(image_type(b"\xff\xd8\xff"), Some(("jpg", "image/jpeg")));
        assert_eq!(image_type(b"<svg />"), None);
    }
}
