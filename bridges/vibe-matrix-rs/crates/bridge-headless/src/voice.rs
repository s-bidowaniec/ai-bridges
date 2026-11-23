use anyhow::{Context, Result};
use bridge_common::private_directory;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);
const AUDIO_FORMATS: [(&str, &str); 4] = [
    ("opus", "audio/ogg"),
    ("ogg", "audio/ogg"),
    ("wav", "audio/wav"),
    ("mp3", "audio/mpeg"),
];

pub fn input_extension(filename: &str, mime: &str) -> &'static str {
    let extension = Path::new(filename).extension().and_then(|ext| ext.to_str());
    AUDIO_FORMATS
        .iter()
        .find(|(ext, _)| extension.is_some_and(|value| value.eq_ignore_ascii_case(ext)))
        .or_else(|| {
            AUDIO_FORMATS
                .iter()
                .find(|(_, candidate)| *candidate == mime)
        })
        .map_or("bin", |(ext, _)| ext)
}

pub struct VoiceExchange {
    output_dir: PathBuf,
    input_path: PathBuf,
    id: String,
}

pub struct VoiceReply {
    path: PathBuf,
    name: String,
    mime: &'static str,
    bytes: Vec<u8>,
}

impl VoiceExchange {
    pub async fn publish(
        root: &Path,
        uploads: &Path,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let sequence = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let id = format!("{now:x}-{:x}-{sequence:x}", std::process::id());
        let input_dir = root.join("in");
        let output_dir = root.join("out");
        private_directory(&input_dir)?;
        private_directory(&output_dir)?;
        private_directory(uploads)?;

        let name = format!("{id}.{}", input_extension(filename, mime));
        fs::write(uploads.join(&name), bytes)
            .await
            .context("zapis pobranego audio")?;
        let staging = input_dir.join(format!(".{id}.tmp"));
        fs::write(&staging, bytes)
            .await
            .context("zapis wejścia audio")?;
        let input_path = fs::canonicalize(input_dir).await?.join(name);
        fs::rename(&staging, &input_path)
            .await
            .context("publikacja wejścia audio")?;
        Ok(Self {
            output_dir,
            input_path,
            id,
        })
    }

    pub fn input_path(&self) -> &Path {
        &self.input_path
    }

    async fn find_reply(&self) -> Result<Option<VoiceReply>> {
        for (extension, mime) in AUDIO_FORMATS {
            let name = format!("{}.{extension}", self.id);
            let path = self.output_dir.join(&name);
            match fs::symlink_metadata(&path).await {
                Ok(metadata) if metadata.is_file() => {
                    anyhow::ensure!(
                        metadata.len() <= crate::media::MAX_MEDIA_BYTES as u64,
                        "plik odpowiedzi audio jest za duży"
                    );
                    let bytes = fs::read(&path).await.context("odczyt odpowiedzi audio")?;
                    anyhow::ensure!(!bytes.is_empty(), "plik odpowiedzi audio jest pusty");
                    return Ok(Some(VoiceReply {
                        path,
                        name,
                        mime,
                        bytes,
                    }));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("sprawdzanie odpowiedzi audio"),
            }
        }
        Ok(None)
    }

    pub async fn wait(&self) -> Result<VoiceReply> {
        loop {
            if let Some(reply) = self.find_reply().await? {
                return Ok(reply);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

impl VoiceReply {
    pub async fn deliver<F, SendReply>(self, send: F) -> Result<String>
    where
        F: FnOnce(String, &'static str, Vec<u8>) -> SendReply,
        SendReply: Future<Output = Result<String>>,
    {
        let sent = send(self.name, self.mime, self.bytes).await?;
        if let Err(error) = fs::remove_file(&self.path).await {
            tracing::warn!(%error, "nie udało się usunąć wysłanego pliku audio");
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_exact_ids_and_leaves_other_and_failed_replies_untouched() {
        let root = std::env::temp_dir().join(format!("matrix-voice-test-{}", std::process::id()));
        let uploads = root.join("uploads");
        let first = VoiceExchange::publish(&root, &uploads, "note.ogg", "audio/ogg", b"first")
            .await
            .unwrap();
        let second = VoiceExchange::publish(&root, &uploads, "note.ogg", "audio/ogg", b"second")
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        let in_dir = root.join("in");
        assert_eq!(
            fs::read(in_dir.join(format!("{}.ogg", first.id)))
                .await
                .unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(in_dir.join(format!("{}.ogg", second.id)))
                .await
                .unwrap(),
            b"second"
        );

        let unrelated = root.join("out/unrelated.opus");
        let first_output = root.join(format!("out/{}.mp3", first.id));
        let second_output = root.join(format!("out/{}.ogg", second.id));
        fs::write(&unrelated, b"unrelated").await.unwrap();
        fs::write(&second_output, b"second answer").await.unwrap();
        assert!(first.find_reply().await.unwrap().is_none());
        // A reply for first cannot be sent by the second exchange.
        fs::write(&first_output, b"late").await.unwrap();
        let reply = second.wait().await.unwrap();
        let event = reply
            .deliver(|name, mime, bytes| async move {
                assert!(name.ends_with(".ogg"));
                assert_eq!(mime, "audio/ogg");
                assert_eq!(bytes, b"second answer");
                Ok("$sent".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(event, "$sent");
        assert!(!second_output.exists());
        assert!(first_output.exists() && unrelated.exists());

        let failed = VoiceExchange::publish(&root, &uploads, "note.ogg", "audio/ogg", b"third")
            .await
            .unwrap();
        let failed_output = root.join(format!("out/{}.mp3", failed.id));
        fs::write(&failed_output, b"diagnostic").await.unwrap();
        let reply = failed.wait().await.unwrap();
        let result = reply
            .deliver(|_, mime, _| async move {
                assert_eq!(mime, "audio/mpeg");
                anyhow::bail!("send failed")
            })
            .await;
        assert!(result.is_err());
        assert!(failed_output.exists());
        fs::remove_dir_all(root).await.unwrap();
    }
}
