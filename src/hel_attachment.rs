//! Session-private immutable image blobs. Internal image references are resolved
//! only at the ACP boundary; image bytes never belong in durable relay commands.
use agent_client_protocol::schema::v1::{ContentBlock, ImageContent};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const MAX_IMAGES: usize = 10;
pub const MAX_IMAGE_BYTES: usize = 700 * 1024;
pub const ATTACHMENT_DIR: &str = "attachments";
pub const ARCHIVE_ATTACHMENT_DIR: &str = "mj-attachments";
const URI_PREFIX: &str = "mj-attachment:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRef {
    pub sha256: String,
    pub mime_type: String,
    pub size: usize,
    pub width: u32,
    pub height: u32,
}
impl AttachmentRef {
    pub fn new(bytes: &[u8], mime_type: String, width: u32, height: u32) -> Result<Self> {
        let reference = Self {
            sha256: format!("{:x}", Sha256::digest(bytes)),
            mime_type,
            size: bytes.len(),
            width,
            height,
        };
        reference.verify(bytes)?;
        Ok(reference)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid attachment digest"
        );
        ensure!(
            self.size > 0 && self.size <= MAX_IMAGE_BYTES,
            "image must be at most 700 KiB"
        );
        ensure!(
            matches!(
                self.mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp"
            ),
            "unsupported attachment format"
        );
        ensure!(
            self.width > 0
                && self.height > 0
                && u64::from(self.width) * u64::from(self.height) <= 64 * 1024 * 1024,
            "invalid attachment dimensions"
        );
        Ok(())
    }
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(
            bytes.len() == self.size && format!("{:x}", Sha256::digest(bytes)) == self.sha256,
            "attachment data does not match its digest or size"
        );
        let correct_format = match self.mime_type.as_str() {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        };
        ensure!(
            correct_format,
            "attachment media type does not match its data"
        );
        Ok(())
    }
    pub fn uri(&self) -> String {
        format!(
            "{URI_PREFIX}{}",
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(self).expect("attachment metadata serializes"))
        )
    }
    pub fn from_uri(uri: &str) -> Result<Option<Self>> {
        let Some(encoded) = uri.strip_prefix(URI_PREFIX) else {
            return Ok(None);
        };
        ensure!(encoded.len() <= 1024, "attachment reference is too large");
        let reference: Self = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(encoded)
                .context("decode attachment reference")?,
        )?;
        reference.validate()?;
        Ok(Some(reference))
    }
    pub fn content_block(&self) -> ContentBlock {
        ContentBlock::Image(ImageContent::new("", self.mime_type.clone()).uri(self.uri()))
    }
}

pub fn image_reference(image: &ImageContent) -> Result<Option<AttachmentRef>> {
    let Some(uri) = image.uri.as_deref() else {
        return Ok(None);
    };
    let reference = AttachmentRef::from_uri(uri)?;
    if let Some(reference) = &reference {
        ensure!(
            image.data.is_empty() && image.mime_type == reference.mime_type,
            "attachment reference conflicts with image data"
        );
    }
    Ok(reference)
}
pub fn references(prompt: &[ContentBlock]) -> Result<Vec<AttachmentRef>> {
    let mut images = 0;
    let mut result = Vec::new();
    for block in prompt {
        if let ContentBlock::Image(image) = block {
            images += 1;
            if let Some(reference) = image_reference(image)? {
                result.push(reference);
            }
        }
    }
    // Older workers could persist prompts with more inline images. Keep those
    // historical prompts readable during replay; the limit applies to the
    // content-addressed attachment form introduced by this protocol.
    ensure!(
        result.is_empty() || images <= MAX_IMAGES,
        "a message can contain at most 10 images"
    );
    Ok(result)
}
pub fn has_references(prompt: &[ContentBlock]) -> bool {
    prompt.iter().any(|block| matches!(block, ContentBlock::Image(image) if image.uri.as_deref().is_some_and(|uri| uri.starts_with(URI_PREFIX))))
}

#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}
impl AttachmentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn controller(session_id: &str) -> Result<Self> {
        crate::hel_config::validate_id("session", session_id)?;
        Ok(Self::new(
            crate::hel_config::sessions_dir()
                .join(session_id)
                .join(ATTACHMENT_DIR),
        ))
    }
    pub fn worker(root: &Path) -> Self {
        Self::new(root.join(ATTACHMENT_DIR))
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn lock(&self) -> Result<fs::File> {
        let parent = self
            .root
            .parent()
            .context("attachment store has no parent")?;
        fs::create_dir_all(parent)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.with_extension("lock"))?;
        file.lock().context("lock attachment store")?;
        Ok(file)
    }
    fn ensure_open(&self) -> Result<()> {
        ensure!(
            !self.root.with_extension("deleted").try_exists()?,
            "this session was deleted; the attachment was not saved"
        );
        Ok(())
    }
    /// A tombstone prevents a late clipboard/upload task recreating a deleted
    /// session's store. The lock also coordinates native and daemon processes.
    pub fn remove_session_data(&self) -> Result<()> {
        let _lock = self.lock()?;
        fs::File::create(self.root.with_extension("deleted"))?.sync_all()?;
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        fs::File::open(
            self.root
                .parent()
                .context("attachment store has no parent")?,
        )?
        .sync_all()?;
        Ok(())
    }
    pub fn install(&self, reference: &AttachmentRef, bytes: &[u8]) -> Result<()> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        reference.verify(bytes)?;
        fs::create_dir_all(&self.root).context("create image attachment store")?;
        let path = self.root.join(&reference.sha256);
        if self.existing_attachment_valid(reference)?.unwrap_or(false) {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        // The store lock serializes installers, and `persist` replaces a
        // corrupt regular blob with one verified above in a single rename.
        file.persist(&path).map_err(|error| error.error)?;
        #[cfg(unix)]
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn read(&self, reference: &AttachmentRef) -> Result<Vec<u8>> {
        reference.validate()?;
        let path = self.root.join(&reference.sha256);
        ensure!(
            fs::symlink_metadata(&path)
                .with_context(|| format!("missing image attachment {}", reference.sha256))?
                .file_type()
                .is_file(),
            "attachment is not a regular file"
        );
        ensure!(
            fs::metadata(&path)?.len() == reference.size as u64,
            "attachment has incorrect size"
        );
        let bytes = fs::read(&path)?;
        reference.verify(&bytes)?;
        Ok(bytes)
    }
    pub fn contains(&self, reference: &AttachmentRef) -> Result<bool> {
        reference.validate()?;
        Ok(self.existing_attachment_valid(reference)?.unwrap_or(false))
    }

    /// Return whether the existing path is a valid copy of `reference`.
    /// Missing files and content corruption are ordinary cache misses. Filesystem
    /// failures and non-regular paths remain errors so callers do not hide a
    /// broken store or unsafe path.
    fn existing_attachment_valid(&self, reference: &AttachmentRef) -> Result<Option<bool>> {
        let path = self.root.join(&reference.sha256);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect image attachment {}", reference.sha256));
            }
        };
        ensure!(
            metadata.file_type().is_file(),
            "attachment is not a regular file"
        );
        if metadata.len() != reference.size as u64 {
            return Ok(Some(false));
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("read image attachment {}", reference.sha256))?;
        Ok(Some(reference.verify(&bytes).is_ok()))
    }
    /// Archive blobs independently of any prompt; references already live in
    /// canonical history. Unfinished temporary files are never exported.
    pub fn archive_artifacts(&self) -> Result<Vec<crate::hel_archive::NativeArtifact>> {
        if !self.root.try_exists()? {
            return Ok(Vec::new());
        }
        let mut artifacts = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(digest) = name.to_str() else {
                continue;
            };
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                continue;
            }
            ensure!(
                entry.file_type()?.is_file(),
                "attachment is not a regular file"
            );
            ensure!(
                entry.metadata()?.len() <= MAX_IMAGE_BYTES as u64,
                "attachment exceeds image budget"
            );
            let data = fs::read(entry.path())?;
            ensure!(
                format!("{:x}", Sha256::digest(&data)) == digest,
                "corrupt attachment in checkpoint"
            );
            artifacts.push(crate::hel_archive::NativeArtifact {
                relative_path: Path::new(ARCHIVE_ATTACHMENT_DIR).join(digest),
                data,
                mode: 0o600,
            });
        }
        Ok(artifacts)
    }
    pub fn restore_artifact(&self, relative_path: &Path, bytes: &[u8]) -> Result<bool> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        let Ok(relative) = relative_path.strip_prefix(ARCHIVE_ATTACHMENT_DIR) else {
            return Ok(false);
        };
        ensure!(
            relative.components().count() == 1,
            "invalid archived attachment path"
        );
        let digest = relative
            .to_str()
            .context("invalid archived attachment digest")?;
        ensure!(
            bytes.len() <= MAX_IMAGE_BYTES
                && !bytes.is_empty()
                && format!("{:x}", Sha256::digest(bytes)) == digest,
            "archived attachment failed integrity check"
        );
        fs::create_dir_all(&self.root)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(digest))
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        fs::File::open(&self.root)?.sync_all()?;
        Ok(true)
    }
    pub fn resolve(&self, prompt: &mut [ContentBlock]) -> Result<()> {
        references(prompt)?;
        for block in prompt {
            if let ContentBlock::Image(image) = block
                && let Some(reference) = image_reference(image)?
            {
                image.data =
                    base64::engine::general_purpose::STANDARD.encode(self.read(&reference)?);
                image.uri = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn photo() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(MAX_IMAGE_BYTES, 7);
        bytes
    }
    #[test]
    fn ten_large_images_resolve_without_large_control_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(dir.path().join("images"));
        let bytes = photo();
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        store.install(&reference, &bytes).unwrap();
        store.install(&reference, &bytes).unwrap();
        let mut prompt = vec![reference.content_block(); 10];
        assert!(serde_json::to_vec(&prompt).unwrap().len() < 8192);
        store.resolve(&mut prompt).unwrap();
        for block in &prompt {
            let ContentBlock::Image(image) = block else {
                panic!()
            };
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .unwrap(),
                bytes
            );
        }
        prompt.push(reference.content_block());
        assert!(references(&prompt).is_err());
    }
    #[test]
    fn photo_queue_survives_restart_and_archive_transfer() {
        use crate::hel_worker::test_support::{SESSION, relay_request, submit_relay};
        use crate::hel_worker::{
            DurableRelay, RELAY_STATE_FILE, RelayCommand, RelayRequest, RelayResponseBody,
        };
        let source = tempfile::tempdir().unwrap();
        let store = AttachmentStore::worker(source.path());
        let mut prompt = Vec::new();
        for index in 0..10 {
            let mut bytes = photo();
            bytes[100] = index;
            let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
            store.install(&reference, &bytes).unwrap();
            prompt.push(reference.content_block());
        }
        let command = RelayCommand::Prompt {
            prompt: prompt.clone(),
        };
        let mut relay = DurableRelay::open(source.path(), SESSION, "1.0.0").unwrap();
        let ordinal = submit_relay(&mut relay, "photos-command-1", command.clone());
        assert_eq!(
            submit_relay(&mut relay, "photos-command-1", command.clone()),
            ordinal
        );
        assert!(
            fs::metadata(source.path().join(RELAY_STATE_FILE))
                .unwrap()
                .len()
                < 64 * 1024
        );
        drop(relay);
        let mut relay = DurableRelay::open(source.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(
            submit_relay(&mut relay, "photos-command-1", command.clone()),
            ordinal
        );
        let target = tempfile::tempdir().unwrap();
        let restored = AttachmentStore::worker(target.path());
        for artifact in store.archive_artifacts().unwrap() {
            assert!(
                restored
                    .restore_artifact(&artifact.relative_path, &artifact.data)
                    .unwrap()
            );
        }
        restored.resolve(&mut prompt).unwrap();
        for (index, block) in prompt.iter().enumerate() {
            let ContentBlock::Image(image) = block else {
                panic!()
            };
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .unwrap()[100],
                index as u8
            );
        }
        let missing = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(missing.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(relay_request(
            "missing-photos",
            RelayRequest::Submit {
                command_id: "photos-command-2".into(),
                command,
            },
        ));
        assert!(matches!(response.body, RelayResponseBody::Error { .. }));
        assert_eq!(relay.latest_ordinal(), 0);
    }

    #[test]
    fn deleted_session_rejects_late_upload_completion() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::worker(dir.path());
        let bytes = photo();
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        store.install(&reference, &bytes).unwrap();
        store.remove_session_data().unwrap();
        assert!(store.install(&reference, &bytes).is_err());
        assert!(!store.root().exists());
    }

    #[test]
    fn references_and_stored_bytes_are_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(dir.path().join("images"));
        let bytes = photo();
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        assert_eq!(
            AttachmentRef::from_uri(&reference.uri()).unwrap(),
            Some(reference.clone())
        );
        assert!(!store.contains(&reference).unwrap());
        store.install(&reference, &bytes).unwrap();
        let mut corrupt = bytes.clone();
        corrupt[100] ^= 1;
        fs::write(store.root().join(&reference.sha256), corrupt).unwrap();
        assert!(store.read(&reference).is_err());
        assert!(!store.contains(&reference).unwrap());
        store.install(&reference, &bytes).unwrap();
        assert_eq!(store.read(&reference).unwrap(), bytes);
        let mut bad = reference;
        bad.sha256 = "../escape".into();
        assert!(store.contains(&bad).is_err());
    }

    #[test]
    fn legacy_inline_image_lists_are_accepted_but_referenced_lists_are_bounded() {
        let inline = (0..11)
            .map(|_| ContentBlock::Image(ImageContent::new("legacy", "image/png")))
            .collect::<Vec<_>>();
        assert!(references(&inline).is_ok());

        let bytes = photo();
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        assert!(references(&vec![reference.content_block(); 11]).is_err());
    }
}
