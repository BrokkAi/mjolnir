//! Session-private immutable attachment blobs resolved at the ACP boundary.
use crate::hex::lower_hex;
use agent_client_protocol::v1::schema::{ContentBlock, ImageContent, ResourceLink, TextContent};
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
pub const MAX_FILES: usize = 10;
// A 5 MiB payload expands to about 6.67 MiB in base64, leaving room for the
// JSON envelope within crate::relay::MAX_FRAME_BYTES (8 MiB).
pub const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;
pub const ATTACHMENT_DIR: &str = "attachments";
pub const ARCHIVE_ATTACHMENT_DIR: &str = "mj-attachments";
const URI_PREFIX: &str = "mj-attachment:";
const FILE_URI_PREFIX: &str = "mj-file-attachment:";

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
            sha256: lower_hex(Sha256::digest(bytes)),
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
            bytes.len() == self.size && lower_hex(Sha256::digest(bytes)) == self.sha256,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAttachmentRef {
    pub sha256: String,
    pub filename: String,
    pub mime_type: String,
    pub size: usize,
}

pub fn sanitize_filename(name: &str) -> String {
    const MAX_FILENAME_BYTES: usize = 128;
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let name = if matches!(name.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()) {
        &name[2..]
    } else {
        name
    };
    let name: String = name
        .chars()
        .filter(|ch| !ch.is_ascii_control())
        .map(|ch| if ch == ':' { '_' } else { ch })
        .collect();
    let name = name.trim();
    if matches!(name, "" | "." | "..") {
        return "attachment".into();
    }
    if name.len() <= MAX_FILENAME_BYTES {
        return name.into();
    }

    let (stem, extension) = match name.rfind('.') {
        Some(index) if index > 0 && name.len() - index < MAX_FILENAME_BYTES => name.split_at(index),
        _ => (name, ""),
    };
    let end = stem.floor_char_boundary(MAX_FILENAME_BYTES - extension.len());
    let truncated = format!("{}{extension}", stem[..end].trim_end());
    if matches!(truncated.as_str(), "" | "." | "..") {
        return "attachment".into();
    }
    truncated
}

impl FileAttachmentRef {
    pub fn new(bytes: &[u8], filename: &str, mime_type: String) -> Result<Self> {
        ensure!(
            !mime_type.chars().any(char::is_control),
            "invalid file attachment MIME type"
        );
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "file attachment exceeds 5 MiB"
        );
        let reference = Self {
            sha256: lower_hex(Sha256::digest(bytes)),
            filename: sanitize_filename(filename),
            mime_type: mime_type.trim().into(),
            size: bytes.len(),
        };
        reference.verify(bytes)?;
        Ok(reference)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(valid_digest(&self.sha256), "invalid attachment digest");
        ensure!(self.size <= MAX_FILE_BYTES, "file attachment exceeds 5 MiB");
        ensure!(
            self.filename == sanitize_filename(&self.filename),
            "invalid attachment filename"
        );
        ensure!(
            !self.mime_type.is_empty()
                && self.mime_type.len() <= 128
                && self.mime_type.trim() == self.mime_type
                && !self.mime_type.chars().any(char::is_control),
            "invalid file attachment MIME type"
        );
        Ok(())
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(
            bytes.len() == self.size && lower_hex(Sha256::digest(bytes)) == self.sha256,
            "attachment data does not match its digest or size"
        );
        Ok(())
    }

    pub fn uri(&self) -> String {
        format!(
            "{FILE_URI_PREFIX}{}",
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(self).expect("attachment metadata serializes"))
        )
    }

    pub fn from_uri(uri: &str) -> Result<Option<Self>> {
        let Some(encoded) = uri.strip_prefix(FILE_URI_PREFIX) else {
            return Ok(None);
        };
        ensure!(uri.len() <= 2048, "file attachment reference is too large");
        let reference: Self = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(encoded)
                .context("decode file attachment reference")?,
        )?;
        reference.validate()?;
        Ok(Some(reference))
    }

    pub fn content_block(&self) -> ContentBlock {
        ContentBlock::ResourceLink(
            ResourceLink::new(self.filename.clone(), self.uri())
                .mime_type(self.mime_type.clone())
                .size(self.size as i64),
        )
    }
}

pub fn file_reference(link: &ResourceLink) -> Result<Option<FileAttachmentRef>> {
    let reference = FileAttachmentRef::from_uri(&link.uri)?;
    if let Some(reference) = &reference {
        ensure!(
            link.name == reference.filename
                && link
                    .mime_type
                    .as_ref()
                    .is_none_or(|mime_type| mime_type == &reference.mime_type)
                && link.size.is_none_or(|size| size == reference.size as i64),
            "attachment reference conflicts with resource link"
        );
    }
    Ok(reference)
}

pub fn file_references(prompt: &[ContentBlock]) -> Result<Vec<FileAttachmentRef>> {
    let mut result = Vec::new();
    for block in prompt {
        if let ContentBlock::ResourceLink(link) = block
            && let Some(reference) = file_reference(link)?
        {
            ensure!(
                result.len() < MAX_FILES,
                "a message can contain at most 10 files"
            );
            result.push(reference);
        }
    }
    Ok(result)
}

pub fn has_file_references(prompt: &[ContentBlock]) -> bool {
    prompt.iter().any(|block| matches!(block, ContentBlock::ResourceLink(link) if link.uri.starts_with(FILE_URI_PREFIX)))
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
    has_file_references(prompt) || prompt.iter().any(|block| matches!(block, ContentBlock::Image(image) if image.uri.as_deref().is_some_and(|uri| uri.starts_with(URI_PREFIX))))
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
        crate::config::validate_id("session", session_id)?;
        Ok(Self::new(
            crate::config::sessions_dir()
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
    fn lock(&self) -> Result<crate::file_lock::FileLock> {
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
        Ok(crate::file_lock::FileLock::new(file))
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
        write_attachment(&path, bytes)
    }
    pub fn read(&self, reference: &AttachmentRef) -> Result<Vec<u8>> {
        reference.validate()?;
        let path = self.root.join(&reference.sha256);
        let bytes = read_attachment(&path, reference.size)?
            .with_context(|| format!("missing image attachment {}", reference.sha256))?;
        reference.verify(&bytes)?;
        Ok(bytes)
    }
    pub fn contains(&self, reference: &AttachmentRef) -> Result<bool> {
        reference.validate()?;
        Ok(self.existing_attachment_valid(reference)?.unwrap_or(false))
    }

    fn existing_attachment_valid(&self, reference: &AttachmentRef) -> Result<Option<bool>> {
        existing_attachment_valid(
            &self.root.join(&reference.sha256),
            reference.size,
            |bytes| reference.verify(bytes),
        )
    }

    pub fn install_file(&self, reference: &FileAttachmentRef, bytes: &[u8]) -> Result<()> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        reference.verify(bytes)?;
        ensure_attachment_directory(&self.root)?;
        let path = self.root.join(&reference.sha256);
        if existing_attachment_valid(&path, reference.size, |bytes| reference.verify(bytes))?
            != Some(true)
        {
            write_attachment(&path, bytes)?;
        }
        Ok(())
    }

    pub fn read_file(&self, reference: &FileAttachmentRef) -> Result<Vec<u8>> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        self.read_file_blob(reference)
    }

    fn read_file_blob(&self, reference: &FileAttachmentRef) -> Result<Vec<u8>> {
        reference.validate()?;
        let path = self.root.join(&reference.sha256);
        let bytes = if attachment_directory_exists(&self.root)? {
            read_attachment(&path, reference.size)?
        } else {
            None
        }
        .with_context(|| format!("missing file attachment {}", reference.sha256))?;
        reference.verify(&bytes)?;
        Ok(bytes)
    }

    pub fn contains_file(&self, reference: &FileAttachmentRef) -> Result<bool> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        reference.validate()?;
        if !attachment_directory_exists(&self.root)? {
            return Ok(false);
        }
        Ok(existing_attachment_valid(
            &self.root.join(&reference.sha256),
            reference.size,
            |bytes| reference.verify(bytes),
        )? == Some(true))
    }

    pub fn materialize_file(&self, reference: &FileAttachmentRef) -> Result<PathBuf> {
        let _lock = self.lock()?;
        self.ensure_open()?;
        let bytes = self.read_file_blob(reference)?;
        let files = self.root.join("files");
        ensure_attachment_directory(&files)?;
        let digest_dir = files.join(&reference.sha256[..16]);
        ensure_attachment_directory(&digest_dir)?;
        let path = digest_dir.join(&reference.filename);
        if existing_attachment_valid(&path, reference.size, |bytes| reference.verify(bytes))?
            != Some(true)
        {
            write_attachment(&path, &bytes)?;
        }
        Ok(path)
    }

    /// Archive blobs independently of any prompt; references already live in
    /// canonical history. Unfinished temporary files are never exported.
    pub fn archive_artifacts(&self) -> Result<Vec<crate::archive::NativeArtifact>> {
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
            if !valid_digest(digest) {
                continue;
            }
            ensure!(
                entry.file_type()?.is_file(),
                "attachment is not a regular file"
            );
            ensure!(
                entry.metadata()?.len() <= MAX_FILE_BYTES as u64,
                "attachment exceeds file budget"
            );
            let data = fs::read(entry.path())?;
            ensure!(
                lower_hex(Sha256::digest(&data)) == digest,
                "corrupt attachment in checkpoint"
            );
            artifacts.push(crate::archive::NativeArtifact {
                relative_path: Path::new(ARCHIVE_ATTACHMENT_DIR).join(digest),
                data: data.into(),
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
            bytes.len() <= MAX_FILE_BYTES && lower_hex(Sha256::digest(bytes)) == digest,
            "archived attachment failed integrity check"
        );
        fs::create_dir_all(&self.root)?;
        write_attachment(&self.root.join(digest), bytes)?;
        Ok(true)
    }
    pub fn resolve(&self, prompt: &mut [ContentBlock]) -> Result<()> {
        references(prompt)?;
        file_references(prompt)?;
        for block in prompt {
            if let ContentBlock::Image(image) = block
                && let Some(reference) = image_reference(image)?
            {
                image.data =
                    base64::engine::general_purpose::STANDARD.encode(self.read(&reference)?);
                image.uri = None;
            } else if let ContentBlock::ResourceLink(link) = block
                && let Some(reference) = file_reference(link)?
            {
                let path = self.materialize_file(&reference)?;
                *block = ContentBlock::Text(TextContent::new(format!(
                    "Attached file: {} ({}, {}) at {}",
                    reference.filename,
                    reference.mime_type,
                    format_byte_size(reference.size),
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

fn ensure_attachment_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            #[cfg(unix)]
            fs::File::open(
                path.parent()
                    .context("attachment directory has no parent")?,
            )?
            .sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                attachment_directory_exists(path)?,
                "attachment directory disappeared: {}",
                path.display()
            );
        }
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    Ok(())
}

fn attachment_directory_exists(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(
        metadata.is_dir(),
        "attachment directory is not a regular directory: {}",
        path.display()
    );
    Ok(true)
}

fn attachment_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(
        metadata.is_file(),
        "attachment is not a regular file: {}",
        path.display()
    );
    Ok(Some(metadata))
}

fn read_attachment(path: &Path, size: usize) -> Result<Option<Vec<u8>>> {
    let Some(metadata) = attachment_metadata(path)? else {
        return Ok(None);
    };
    ensure!(
        metadata.len() == size as u64,
        "attachment has incorrect size"
    );
    Ok(Some(
        fs::read(path).with_context(|| format!("read {}", path.display()))?,
    ))
}

/// A missing or corrupt blob is a cache miss. Filesystem errors and unsafe
/// paths must be reported to the caller.
fn existing_attachment_valid(
    path: &Path,
    size: usize,
    verify: impl FnOnce(&[u8]) -> Result<()>,
) -> Result<Option<bool>> {
    let Some(metadata) = attachment_metadata(path)? else {
        return Ok(None);
    };
    if metadata.len() != size as u64 {
        return Ok(Some(false));
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(verify(&bytes).is_ok()))
}

fn write_attachment(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("attachment path has no parent")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn format_byte_size(size: usize) -> String {
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KiB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", size as f64 / (1024.0 * 1024.0))
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
    #[test]
    fn file_attachment_names_and_metadata_are_safe() {
        for (raw, expected) in [
            ("../../etc/passwd", "passwd"),
            (r"C:\foo\bar.txt", "bar.txt"),
            ("C:relative.txt", "relative.txt"),
            (" foo\0\n\u{7f}.txt ", "foo.txt"),
            (".", "attachment"),
            ("..", "attachment"),
            ("", "attachment"),
        ] {
            assert_eq!(sanitize_filename(raw), expected);
        }
        let long = sanitize_filename(&format!("{}.pdf", "界".repeat(200)));
        assert_eq!(long, format!("{}.pdf", "界".repeat(41)));
        let reference = FileAttachmentRef::new(b"abc", &long, " text/plain ".into()).unwrap();
        assert_eq!(reference.mime_type, "text/plain");
        assert_eq!(
            FileAttachmentRef::from_uri(&reference.uri()).unwrap(),
            Some(reference.clone())
        );
        assert!(file_references(&vec![reference.content_block(); MAX_FILES + 1]).is_err());
        assert!(
            FileAttachmentRef::from_uri(&format!("{FILE_URI_PREFIX}{}", "x".repeat(2048))).is_err()
        );
        assert!(reference.verify(b"xyz").is_err());
        assert!(reference.verify(b"abcd").is_err());
        assert!(FileAttachmentRef::new(b"", "empty", "text/plain\r\n".into()).is_err());
        let mut bad = reference.clone();
        bad.sha256.make_ascii_uppercase();
        assert!(bad.validate().is_err());
        bad = reference;
        bad.size = MAX_FILE_BYTES + 1;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn file_attachment_checkpoint_preserves_blobs_and_resolves_worker_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::worker(dir.path());
        let restored_dir = tempfile::tempdir().unwrap();
        let restored = AttachmentStore::worker(restored_dir.path());
        let bytes = vec![0xa5; MAX_IMAGE_BYTES + 1];
        let reference =
            FileAttachmentRef::new(&bytes, "input.bin", "application/octet-stream".into()).unwrap();
        let empty = FileAttachmentRef::new(&[], "empty.txt", "text/plain".into()).unwrap();
        assert!(!store.contains_file(&reference).unwrap());
        store.install_file(&reference, &bytes).unwrap();
        store.install_file(&empty, &[]).unwrap();
        let path = store.materialize_file(&reference).unwrap();
        fs::write(&path, vec![0; bytes.len()]).unwrap();
        assert_eq!(store.materialize_file(&reference).unwrap(), path);
        assert_eq!(fs::read(path).unwrap(), bytes);

        let artifacts = store.archive_artifacts().unwrap();
        assert_eq!(artifacts.len(), 2);
        for artifact in artifacts {
            assert!(
                restored
                    .restore_artifact(&artifact.relative_path, &artifact.data.bytes().unwrap())
                    .unwrap()
            );
        }
        assert_eq!(restored.read_file(&empty).unwrap(), Vec::<u8>::new());
        let mut prompt = [reference.content_block()];
        assert!(has_references(&prompt));
        restored.resolve(&mut prompt).unwrap();
        let path = restored
            .root()
            .join("files")
            .join(&reference.sha256[..16])
            .join("input.bin");
        assert!(
            matches!(&prompt[0], ContentBlock::Text(text) if text.text.ends_with(&format!(" at {}", path.display())))
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);

        restored.remove_session_data().unwrap();
        assert!(restored.contains_file(&reference).is_err());
        assert!(restored.read_file(&reference).is_err());
        assert!(restored.materialize_file(&reference).is_err());
        assert!(restored.install_file(&reference, &bytes).is_err());
        assert!(!restored.root().exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_attachment_materialization_rejects_symlinks_at_each_level() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = AttachmentStore::worker(dir.path());
        let reference = FileAttachmentRef::new(b"abc", "input.txt", "text/plain".into()).unwrap();
        store.install_file(&reference, b"abc").unwrap();
        let mut path = store.root().join("files");
        for component in [&reference.sha256[..16], reference.filename.as_str()] {
            symlink(outside.path(), &path).unwrap();
            assert!(store.materialize_file(&reference).is_err());
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            path.push(component);
        }
        let external_file = outside.path().join("external");
        fs::write(&external_file, b"abc").unwrap();
        symlink(&external_file, &path).unwrap();
        assert!(store.materialize_file(&reference).is_err());
        assert_eq!(fs::read(&external_file).unwrap(), b"abc");
    }
}
