//! Inline attachments for the chat composer.
//!
//! Images and files occupy tracked byte ranges in [`PromptPayload::text`].
//! Marker-shaped text outside those ranges stays ordinary text.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::Path;

use agent_client_protocol::v1::schema::{ContentBlock, ImageContent, TextContent};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize};

use crate::clipboard::ClipboardImage;
#[cfg(test)]
use mj_core::attachment::AttachmentRef;
use mj_core::attachment::{
    FileAttachmentRef, MAX_FILE_BYTES, MAX_FILES, MAX_IMAGES, sanitize_filename,
};

/// Prefix used for drafts whose payload is encoded as JSON.
pub use mj_core::draft::CHAT_DRAFT_PREFIX;

/// The text and attachments currently in the composer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptPayload {
    pub text: String,
    pub images: Vec<PromptImage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PromptFile>,
}

/// One image and the marker that displays it in [`PromptPayload::text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptImage {
    pub range: Range<usize>,
    pub number: u64,
    pub image: ClipboardImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFile {
    pub range: Range<usize>,
    pub number: u64,
    pub file: AttachedFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachedFile {
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<FileAttachmentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placeholder: Option<FilePlaceholder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FilePlaceholder {
    Pending,
    Failed,
}

impl AttachedFile {
    pub fn pending(filename: &str) -> Self {
        Self {
            filename: sanitize_filename(filename),
            reference: None,
            placeholder: Some(FilePlaceholder::Pending),
        }
    }

    pub fn failed(filename: &str) -> Self {
        Self {
            placeholder: Some(FilePlaceholder::Failed),
            ..Self::pending(filename)
        }
    }

    pub fn from_reference(reference: FileAttachmentRef) -> Result<Self> {
        reference.validate()?;
        Ok(Self {
            filename: reference.filename.clone(),
            reference: Some(reference),
            placeholder: None,
        })
    }

    pub fn is_pending(&self) -> bool {
        self.placeholder == Some(FilePlaceholder::Pending)
    }

    pub fn is_failed(&self) -> bool {
        self.placeholder == Some(FilePlaceholder::Failed)
    }

    pub fn is_placeholder(&self) -> bool {
        self.is_pending() || self.is_failed()
    }
}

#[derive(Debug, Deserialize)]
struct PromptPayloadWire {
    text: String,
    #[serde(default)]
    images: Option<Vec<PromptImage>>,
    #[serde(default)]
    image: Option<ClipboardImage>,
    #[serde(default)]
    files: Vec<PromptFile>,
}

impl<'de> Deserialize<'de> for PromptPayload {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PromptPayloadWire::deserialize(deserializer)?;
        decode_wire(wire).map_err(serde::de::Error::custom)
    }
}

impl PromptPayload {
    /// Build a text-only payload.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Build a payload with one image marker appended after `text`.
    #[must_use]
    pub fn with_image(text: impl Into<String>, image: ClipboardImage) -> Self {
        let mut text = text.into();
        let start = text.len();
        text.push_str(&marker(1));
        Self {
            text,
            files: Vec::new(),
            images: vec![PromptImage {
                range: start..start + marker(1).len(),
                number: 1,
                image,
            }],
        }
    }

    /// Build ACP content while omitting the inline marker bytes from text
    /// blocks. Only tracked ranges are interpreted as attachments.
    #[must_use]
    pub fn content_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        let mut cursor = 0;
        let mut markers: Vec<_> = self
            .images
            .iter()
            .map(|image| {
                let block = if image.image.is_placeholder() {
                    None
                } else {
                    Some(match &image.image.reference {
                        Some(reference) => reference.content_block(),
                        None => ContentBlock::Image(ImageContent::new(
                            image.image.data_base64.to_string(),
                            image.image.mime_type.clone(),
                        )),
                    })
                };
                (&image.range, block)
            })
            .chain(self.files.iter().map(|file| {
                (
                    &file.range,
                    file.file
                        .reference
                        .as_ref()
                        .map(FileAttachmentRef::content_block),
                )
            }))
            .collect();
        markers.sort_by_key(|(range, _)| range.start);
        for (range, block) in markers {
            if cursor < range.start {
                blocks.push(ContentBlock::Text(TextContent::new(
                    self.text[cursor..range.start].to_owned(),
                )));
            }
            blocks.push(block.unwrap_or_else(|| {
                ContentBlock::Text(TextContent::new(self.text[range.clone()].to_owned()))
            }));
            cursor = range.end;
        }

        if cursor < self.text.len() {
            blocks.push(ContentBlock::Text(TextContent::new(
                self.text[cursor..].to_owned(),
            )));
        }
        blocks
    }

    /// Encode a draft for a session record.  Text-only drafts retain their
    /// historical plain-text representation unless the text could be mistaken
    /// for an encoded envelope.
    #[must_use]
    pub fn encode_draft(&self) -> String {
        if self.images.is_empty()
            && self.files.is_empty()
            && !self.text.starts_with(CHAT_DRAFT_PREFIX)
        {
            return self.text.clone();
        }
        let body = serde_json::to_string(self).expect("prompt draft serialization cannot fail");
        format!("{CHAT_DRAFT_PREFIX}{body}")
    }

    /// Decode a session record draft, accepting the historical plain-text and
    /// single-image forms as well as the current tracked-attachment form.
    pub fn decode_draft(draft: &str) -> Result<Self> {
        let Some(body) = draft.strip_prefix(CHAT_DRAFT_PREFIX) else {
            return Ok(Self::text(draft));
        };
        serde_json::from_str(body).context("decode chat draft envelope")
    }

    /// Trim surrounding whitespace and shift attachment ranges with the text.
    #[must_use]
    pub fn trimmed(self) -> Self {
        let leading = self
            .text
            .char_indices()
            .find(|(_, character)| !character.is_whitespace())
            .map_or(self.text.len(), |(index, _)| index);
        let trailing = self
            .text
            .char_indices()
            .rev()
            .find(|(_, character)| !character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        if leading == 0 && trailing == self.text.len() {
            return self;
        }

        if leading >= trailing {
            return Self::text(String::new());
        }
        let mut payload = self;
        payload.text = payload.text[leading..trailing].to_owned();
        for range in payload
            .images
            .iter_mut()
            .map(|image| &mut image.range)
            .chain(payload.files.iter_mut().map(|file| &mut file.range))
        {
            *range = range.start - leading..range.end - leading;
        }
        payload
    }
}

fn decode_wire(wire: PromptPayloadWire) -> std::result::Result<PromptPayload, String> {
    let mut payload = match (wire.images, wire.image) {
        (Some(_), Some(_)) => Err("chat draft contains both images and image fields".to_owned()),
        (Some(images), None) => validate_images(wire.text, images),
        (None, Some(image)) => {
            let image = validate_image(image)
                .map_err(|error| format!("validate image in chat draft: {error}"))?;
            Ok(PromptPayload::with_image(wire.text, image))
        }
        (None, None) => Ok(PromptPayload::text(wire.text)),
    }?;
    if wire.files.len() > MAX_FILES {
        return Err(format!("a message can contain at most {MAX_FILES} files"));
    }
    let mut end = 0;
    let mut numbers = BTreeSet::new();
    for file in &wire.files {
        let filename = &file.file.filename;
        if file.number == 0
            || !numbers.insert(file.number)
            || file.range.start < end
            || filename != &sanitize_filename(filename)
            || payload.text.get(file.range.clone())
                != Some(file_marker(file.number, filename).as_str())
            || (file.file.reference.is_some() == file.file.is_placeholder())
            || payload.images.iter().any(|image| {
                image.range.start < file.range.end && file.range.start < image.range.end
            })
        {
            return Err("chat draft has invalid file attachment".to_owned());
        }
        if let Some(reference) = &file.file.reference {
            reference.validate().map_err(|error| error.to_string())?;
            if &reference.filename != filename {
                return Err("file reference filename does not match the marker".to_owned());
            }
        }
        end = file.range.end;
    }
    payload.files = wire.files;
    Ok(payload)
}

fn validate_images(
    text: String,
    images: Vec<PromptImage>,
) -> std::result::Result<PromptPayload, String> {
    if images.len() > MAX_IMAGES {
        return Err(format!("a message can contain at most {MAX_IMAGES} images"));
    }
    let mut previous_end = 0;
    let mut numbers = BTreeSet::new();
    let mut validated = Vec::with_capacity(images.len());

    for (index, image) in images.into_iter().enumerate() {
        if image.number == 0 {
            return Err(format!(
                "chat draft image {index} has a non-positive number"
            ));
        }
        if !numbers.insert(image.number) {
            return Err(format!(
                "chat draft contains duplicate image number {}",
                image.number
            ));
        }
        if image.range.start >= image.range.end
            || image.range.start < previous_end
            || image.range.end > text.len()
            || !text.is_char_boundary(image.range.start)
            || !text.is_char_boundary(image.range.end)
        {
            return Err(format!("chat draft image {index} has an invalid range"));
        }
        let expected = marker(image.number);
        if text.get(image.range.clone()) != Some(expected.as_str()) {
            return Err(format!(
                "chat draft image {} marker does not match its range",
                image.number
            ));
        }
        let image_bytes = validate_image(image.image)
            .map_err(|error| format!("validate image {} in chat draft: {error}", image.number))?;
        previous_end = image.range.end;
        validated.push(PromptImage {
            range: image.range,
            number: image.number,
            image: image_bytes,
        });
    }

    Ok(PromptPayload {
        text,
        images: validated,
        files: Vec::new(),
    })
}

fn validate_image(image: ClipboardImage) -> Result<ClipboardImage> {
    if image.is_placeholder() {
        return Ok(image);
    }
    if let Some(reference) = &image.reference {
        reference.validate()?;
        if !image.data_base64.is_empty() {
            bail!("attachment references must not carry inline image bytes");
        }
        if image.mime_type != reference.mime_type {
            bail!("attachment reference MIME type does not match image MIME type");
        }
        return Ok(image);
    }
    ClipboardImage::from_base64(image.data_base64, image.mime_type)
}

/// Prepare client image bytes for upload. Installation belongs to the daemon
/// and must complete before the prompt carries an opaque reference.
/// Filesystem and codec work belongs in the caller's blocking task.
pub(super) fn install_clipboard_image(
    session_id: &str,
    image: ClipboardImage,
) -> Result<ClipboardImage> {
    if image.is_placeholder() {
        bail!("image attachment is still processing or has failed");
    }
    if image.reference.is_some() {
        return validate_image(image);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data_base64.as_bytes())
        .context("decode clipboard image")?;
    install_image_bytes(session_id, &bytes)
}

#[derive(Debug)]
pub(super) enum AttachmentFailure {
    NotARegularFile(String),
    Error(String),
}

impl AttachmentFailure {
    pub(super) fn error(error: impl std::fmt::Display) -> Self {
        Self::Error(error.to_string())
    }

    pub(super) fn message(&self) -> &str {
        match self {
            Self::NotARegularFile(message) | Self::Error(message) => message,
        }
    }
}

fn read_path(path: &Path, limit: usize, limit_name: &str) -> Result<Vec<u8>, AttachmentFailure> {
    let io_error = |operation, error: std::io::Error| {
        let message = format!("{operation} attachment {}: {error}", path.display());
        match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                AttachmentFailure::NotARegularFile(message)
            }
            _ => AttachmentFailure::Error(message),
        }
    };
    let metadata = std::fs::symlink_metadata(path).map_err(|error| io_error("inspect", error))?;
    if !metadata.file_type().is_file() {
        return Err(AttachmentFailure::NotARegularFile(format!(
            "attachment {} is not a regular file",
            path.display()
        )));
    }
    let exceeds_limit = || {
        AttachmentFailure::Error(format!(
            "attachment {} exceeds the {limit_name} limit",
            path.display()
        ))
    };
    if metadata.len() > limit as u64 {
        return Err(exceeds_limit());
    }
    let file = File::open(path).map_err(|error| io_error("open", error))?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read", error))?;
    if bytes.len() > limit {
        return Err(exceeds_limit());
    }
    Ok(bytes)
}

/// Read and optimize an image named by `/attach` or a pasted path.
pub(super) fn install_path(
    session_id: &str,
    path: &Path,
) -> Result<ClipboardImage, AttachmentFailure> {
    let bytes = read_path(path, 64 * 1024 * 1024, "64 MiB image input")?;
    install_image_bytes(session_id, &bytes).map_err(|error| {
        AttachmentFailure::Error(format!(
            "{} could not be read as an image: {error:#}",
            path.display()
        ))
    })
}

pub(super) struct FileUpload {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
}

pub(super) fn read_file(path: &Path) -> Result<FileUpload, AttachmentFailure> {
    let limit_name = format!("{} MiB", MAX_FILE_BYTES / (1024 * 1024));
    let bytes = read_path(path, MAX_FILE_BYTES, &limit_name)?;
    let filename = sanitize_filename(&path.file_name().unwrap_or_default().to_string_lossy());
    let extension = Path::new(&filename)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime_type = match extension.as_str() {
        "txt" | "log" | "rs" | "go" | "py" | "c" | "h" | "cpp" | "java" | "sh" | "toml" | "ini" => {
            "text/plain"
        }
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    };
    Ok(FileUpload {
        filename,
        bytes,
        mime_type,
    })
}

fn install_image_bytes(_session_id: &str, bytes: &[u8]) -> Result<ClipboardImage> {
    let optimized = mj_client::image::optimize_image(bytes).context("optimize image attachment")?;
    ClipboardImage::from_base64(
        base64::engine::general_purpose::STANDARD.encode(&optimized.bytes),
        optimized.mime_type,
    )
}

/// Upload prepared bytes on the attachment task's cancellable connection.
pub(super) async fn upload_prepared_image(
    session_id: &str,
    image: ClipboardImage,
) -> Result<ClipboardImage> {
    if image.reference.is_some() {
        return Ok(image);
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(image.data_base64.as_bytes())?;
    let reference = mj_client::daemon::connect_existing()
        .await?
        .http()
        .await?
        .upload_attachment(session_id, bytes, &image.mime_type)
        .await?;
    ClipboardImage::from_reference(reference)
}

fn marker(number: u64) -> String {
    format!("[image {number}]")
}

pub(super) use mj_core::draft::file_marker;

fn marker_ranges<'a>(images: &'a [PromptImage], files: &'a [PromptFile]) -> Vec<&'a Range<usize>> {
    let mut ranges: Vec<_> = images
        .iter()
        .map(|image| &image.range)
        .chain(files.iter().map(|file| &file.range))
        .collect();
    ranges.sort_by_key(|range| range.start);
    ranges
}

pub(super) fn text_without_attachments(
    text: &str,
    images: &[PromptImage],
    files: &[PromptFile],
) -> String {
    untracked_ranges(text, images, files)
        .into_iter()
        .map(|range| &text[range])
        .collect()
}

/// Ordinary text in the input's original coordinates.
pub(super) fn untracked_ranges(
    text: &str,
    images: &[PromptImage],
    files: &[PromptFile],
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    for range in marker_ranges(images, files) {
        if cursor < range.start {
            ranges.push(cursor..range.start);
        }
        cursor = range.end;
    }
    if cursor < text.len() {
        ranges.push(cursor..text.len());
    }
    ranges
}

/// Replace a byte range atomically and return the removed tracked content.
pub(super) fn replace_range(
    text: &mut String,
    images: &mut Vec<PromptImage>,
    files: &mut Vec<PromptFile>,
    range: Range<usize>,
    inserted: &PromptPayload,
) -> (usize, PromptPayload) {
    let mut start = floor_char_boundary(text, range.start.min(text.len()));
    let mut end = floor_char_boundary(text, range.end.min(text.len()));
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    if start == end {
        start = snap_cursor(images, files, start, 1);
        end = start;
    } else {
        for range in marker_ranges(images, files) {
            if range.end > start && range.start < end {
                start = start.min(range.start);
                end = end.max(range.end);
            }
        }
    }

    let mut used_images = images
        .iter()
        .filter(|image| image.range.end <= start || image.range.start >= end)
        .map(|image| image.number)
        .collect::<BTreeSet<_>>();
    let mut used_files = files
        .iter()
        .filter(|file| file.range.end <= start || file.range.start >= end)
        .map(|file| file.number)
        .collect::<BTreeSet<_>>();
    let mut next_image = used_images.last().copied().unwrap_or(0).wrapping_add(1);
    let mut next_file = used_files.last().copied().unwrap_or(0).wrapping_add(1);

    let mut replacement = PromptPayload::text("");
    let mut cursor = 0;
    for range in marker_ranges(&inserted.images, &inserted.files) {
        replacement
            .text
            .push_str(&inserted.text[cursor..range.start]);
        let offset = replacement.text.len();
        if let Some(image) = inserted.images.iter().find(|image| &image.range == range) {
            let mut image = image.clone();
            if image.number == 0 || used_images.contains(&image.number) {
                image.number = allocate_number(&used_images, &mut next_image);
            }
            used_images.insert(image.number);
            replacement.text.push_str(&marker(image.number));
            image.range = (start + offset)..(start + replacement.text.len());
            replacement.images.push(image);
        } else if let Some(file) = inserted.files.iter().find(|file| &file.range == range) {
            let mut file = file.clone();
            if file.number == 0 || used_files.contains(&file.number) {
                file.number = allocate_number(&used_files, &mut next_file);
            }
            used_files.insert(file.number);
            replacement
                .text
                .push_str(&file_marker(file.number, &file.file.filename));
            file.range = (start + offset)..(start + replacement.text.len());
            replacement.files.push(file);
        }
        cursor = range.end;
    }
    replacement.text.push_str(&inserted.text[cursor..]);
    let new_end = start + replacement.text.len();
    let mut removed = PromptPayload::text(&text[start..end]);
    removed.images = splice_markers(
        images,
        replacement.images,
        start..end,
        new_end,
        |image| &mut image.range,
        |image| &image.range,
    );
    removed.files = splice_markers(
        files,
        replacement.files,
        start..end,
        new_end,
        |file| &mut file.range,
        |file| &file.range,
    );
    text.replace_range(start..end, &replacement.text);
    (new_end, removed)
}

fn splice_markers<T>(
    markers: &mut Vec<T>,
    inserted: Vec<T>,
    replaced: Range<usize>,
    new_end: usize,
    range_of: impl Fn(&mut T) -> &mut Range<usize>,
    range_of_shared: impl Fn(&T) -> &Range<usize>,
) -> Vec<T> {
    let mut removed = Vec::new();
    for mut marker in std::mem::take(markers) {
        let range = range_of(&mut marker);
        if range.end <= replaced.start {
            markers.push(marker);
        } else if range.start >= replaced.end {
            *range = (range.start - replaced.end + new_end)..(range.end - replaced.end + new_end);
            markers.push(marker);
        } else {
            *range = (range.start - replaced.start)..(range.end - replaced.start);
            removed.push(marker);
        }
    }
    markers.extend(inserted);
    markers.sort_by_key(|marker| {
        // Sorting only needs a shared range reference.
        range_of_shared(marker).start
    });
    removed
}

fn allocate_number(used: &BTreeSet<u64>, next_number: &mut u64) -> u64 {
    let mut candidate = (*next_number).max(1);
    loop {
        if !used.contains(&candidate) {
            *next_number = candidate.wrapping_add(1).max(1);
            return candidate;
        }
        candidate = candidate.wrapping_add(1).max(1);
    }
}

fn floor_char_boundary(text: &str, mut position: usize) -> usize {
    while position > 0 && !text.is_char_boundary(position) {
        position -= 1;
    }
    position
}

/// Snap a cursor that lies strictly inside a marker to the marker boundary.
pub(super) fn snap_cursor(
    images: &[PromptImage],
    files: &[PromptFile],
    position: usize,
    direction: isize,
) -> usize {
    images
        .iter()
        .map(|image| &image.range)
        .chain(files.iter().map(|file| &file.range))
        .find(|range| range.start < position && position < range.end)
        .map_or(position, |range| {
            if direction < 0 {
                range.start
            } else {
                range.end
            }
        })
}

pub(super) fn insert_image(
    text: &mut String,
    images: &mut Vec<PromptImage>,
    files: &mut Vec<PromptFile>,
    position: usize,
    number: u64,
    image: ClipboardImage,
) -> usize {
    let mut inserted = PromptPayload::with_image("", image);
    inserted.images[0].number = number;
    replace_range(text, images, files, position..position, &inserted).0
}

pub(super) fn insert_file(
    text: &mut String,
    images: &mut Vec<PromptImage>,
    files: &mut Vec<PromptFile>,
    position: usize,
    number: u64,
    file: AttachedFile,
) -> usize {
    replace_range(
        text,
        images,
        files,
        position..position,
        &file_payload(number, file),
    )
    .0
}

pub(super) fn file_payload(number: u64, file: AttachedFile) -> PromptPayload {
    let mut payload = PromptPayload::text(file_marker(number, &file.filename));
    payload.files.push(PromptFile {
        range: 0..payload.text.len(),
        number,
        file,
    });
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::ContentBlock;
    use base64::Engine;

    fn image(tag: &str) -> ClipboardImage {
        ClipboardImage {
            data_base64: tag.to_owned().into(),
            mime_type: "image/png".to_owned(),
            reference: None,
        }
    }

    fn valid_image() -> ClipboardImage {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        ClipboardImage::from_png_base64(base64::engine::general_purpose::STANDARD.encode(bytes))
            .unwrap()
    }

    fn tracked(text: &str, number: u64, image: ClipboardImage) -> PromptPayload {
        let range = text.find(&marker(number)).unwrap();
        PromptPayload {
            text: text.to_owned(),
            files: Vec::new(),
            images: vec![PromptImage {
                range: range..range + marker(number).len(),
                number,
                image,
            }],
        }
    }

    #[test]
    fn mixed_draft_round_trips_and_edits_atomic_markers() {
        let reference =
            FileAttachmentRef::new(b"file bytes", "résumé.txt", "text/plain".into()).unwrap();
        let file = AttachedFile::from_reference(reference.clone()).unwrap();
        let mut payload = PromptPayload::with_image("   λ ", valid_image());
        assert!(!payload.encode_draft().contains("\"files\""));
        let cursor = insert_file(
            &mut payload.text,
            &mut payload.images,
            &mut payload.files,
            0,
            1,
            file,
        );
        assert_eq!(cursor, "[file 1: résumé.txt]".len());
        payload.text.push_str("   ");
        let mut payload = PromptPayload::decode_draft(&payload.encode_draft())
            .unwrap()
            .trimmed();
        assert!(matches!(payload.content_blocks().as_slice(),
            [ContentBlock::ResourceLink(_), ContentBlock::Text(text), ContentBlock::Image(_)] if text.text == "   λ "));
        let (_, removed) = replace_range(
            &mut payload.text,
            &mut payload.images,
            &mut payload.files,
            1..2,
            &PromptPayload::text(""),
        );
        assert_eq!(removed.content_blocks(), vec![reference.content_block()]);
        assert_eq!(payload.text, "   λ [image 1]");
        let old_end = payload.text.len();
        replace_range(
            &mut payload.text,
            &mut payload.images,
            &mut payload.files,
            old_end..old_end,
            &removed,
        );
        replace_range(
            &mut payload.text,
            &mut payload.images,
            &mut payload.files,
            0..0,
            &removed,
        );
        assert_eq!(
            payload.text,
            "[file 2: résumé.txt]   λ [image 1][file 1: résumé.txt]"
        );
        assert_eq!(
            PromptPayload::decode_draft(&payload.encode_draft()).unwrap(),
            payload
        );
    }

    #[test]
    fn unicode_offsets_are_bytes() {
        let mut text = "λ".to_owned();
        let mut images = Vec::new();
        let text_end = text.len();
        let cursor = insert_image(
            &mut text,
            &mut images,
            &mut Vec::new(),
            text_end,
            1,
            image("a"),
        );
        assert_eq!(text, "λ[image 1]");
        assert_eq!(images[0].range, 2..11);
        assert_eq!(cursor, 11);
        let mut inserted_text = "é".to_owned();
        let mut inserted_images = Vec::new();
        let (_, removed) = replace_range(
            &mut inserted_text,
            &mut inserted_images,
            &mut Vec::new(),
            0..2,
            &PromptPayload::text("x"),
        );
        assert_eq!(removed.text, "é");
        assert_eq!(inserted_text, "x");
    }

    #[test]
    fn deleting_any_marker_byte_removes_the_whole_marker() {
        let payload = tracked("a[image 1]b", 1, image("a"));
        let marker_start = payload.images[0].range.start;
        let marker_end = payload.images[0].range.end;
        let mut text = payload.text.clone();
        let mut images = payload.images.clone();
        let (_, removed) = replace_range(
            &mut text,
            &mut images,
            &mut Vec::new(),
            (marker_end - 1)..marker_end,
            &PromptPayload::text(""),
        );
        assert_eq!(text, "ab");
        assert!(images.is_empty());
        assert_eq!(removed.text, "[image 1]");
        assert_eq!(removed.images[0].range, 0..marker_end - marker_start);

        let mut text = payload.text;
        let mut images = payload.images;
        replace_range(
            &mut text,
            &mut images,
            &mut Vec::new(),
            marker_start..marker_start + 1,
            &PromptPayload::text(""),
        );
        assert_eq!(text, "ab");
        assert!(images.is_empty());
    }

    #[test]
    fn literal_marker_text_is_not_an_image() {
        let payload = PromptPayload::text("[image 1]");
        let blocks = payload.content_blocks();
        assert!(
            matches!(&blocks[..], [ContentBlock::Text(content)] if content.text == "[image 1]")
        );
    }

    #[test]
    fn replacement_preserves_bytes_and_reassigns_yanked_numbers() {
        let current = tracked("a[image 1]b", 1, image("old"));
        let yanked = tracked("[image 1]", 1, image("new"));
        let mut text = current.text;
        let mut images = current.images;
        replace_range(&mut text, &mut images, &mut Vec::new(), 0..1, &yanked);
        assert_eq!(text, "[image 2][image 1]b");
        assert_eq!(
            images.iter().map(|item| item.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(images[0].range, 0..9);
        assert_eq!(images[1].range, 9..18);
    }

    #[test]
    fn trimming_unicode_whitespace_preserves_image_ranges() {
        let mut payload = PromptPayload::with_image("  λ ", image("one"));
        payload.text.push_str("\u{2003}\n");
        let trimmed = payload.trimmed();
        assert_eq!(trimmed.text, "λ [image 1]");
        assert_eq!(trimmed.images[0].range, 3..12);
        assert!(matches!(trimmed.content_blocks().as_slice(),
            [ContentBlock::Text(text), ContentBlock::Image(_)] if text.text == "λ "));
    }

    #[test]
    fn malformed_saved_marker_ranges_fail_instead_of_losing_images() {
        let payload = PromptPayload::with_image("λ ", valid_image());
        for range in [1..12, 3..13, 3..4] {
            let mut invalid = payload.clone();
            invalid.images[0].range = range;
            assert!(PromptPayload::decode_draft(&invalid.encode_draft()).is_err());
        }
    }

    #[test]
    fn old_and_new_draft_envelopes_decode() {
        let image = valid_image();
        let old = serde_json::json!({ "text": "look", "image": image });
        let old = format!("{CHAT_DRAFT_PREFIX}{old}");
        let old_payload = PromptPayload::decode_draft(&old).unwrap();
        assert_eq!(old_payload.text, "look[image 1]");
        assert_eq!(old_payload.images[0].range, 4..13);

        let current = PromptPayload::with_image("look", valid_image());
        let encoded = current.encode_draft();
        assert_eq!(PromptPayload::decode_draft(&encoded).unwrap(), current);
        let literal = PromptPayload::text(format!("{CHAT_DRAFT_PREFIX}literal"));
        assert_eq!(
            PromptPayload::decode_draft(&literal.encode_draft()).unwrap(),
            literal
        );
    }

    #[test]
    fn reference_images_use_reserved_uri_and_drafts_carry_no_bytes() {
        let inline = valid_image();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(inline.data_base64.as_bytes())
            .unwrap();
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 1, 1).unwrap();
        let image = ClipboardImage::from_reference(reference.clone()).unwrap();
        let payload = PromptPayload::with_image("look", image);
        let blocks = payload.content_blocks();
        let [ContentBlock::Text(text), ContentBlock::Image(content)] = blocks.as_slice() else {
            panic!("expected mixed text and reference image blocks");
        };
        assert_eq!(text.text, "look");
        assert!(content.data.is_empty());
        assert_eq!(content.uri.as_deref(), Some(reference.uri().as_str()));
        let encoded = payload.encode_draft();
        assert!(!encoded.contains(&base64::engine::general_purpose::STANDARD.encode(bytes)));
        assert_eq!(PromptPayload::decode_draft(&encoded).unwrap(), payload);
    }

    #[test]
    fn drafts_reject_more_than_ten_images() {
        let image = valid_image();
        let mut payload = PromptPayload::text("");
        for number in 1..=11 {
            let end = payload.text.len();
            payload.text.push_str(&marker(number));
            payload.images.push(PromptImage {
                range: end..payload.text.len(),
                number,
                image: image.clone(),
            });
        }
        assert!(PromptPayload::decode_draft(&payload.encode_draft()).is_err());
    }
}
