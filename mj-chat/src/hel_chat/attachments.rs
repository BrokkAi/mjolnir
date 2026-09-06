//! Inline image attachments for the chat composer.
//!
//! Images are represented by byte ranges into [`PromptPayload::text`].  The
//! marker text is part of that string, but only the tracked ranges carry image
//! meaning; arbitrary occurrences of `[image N]` remain ordinary text.

use std::collections::BTreeSet;
use std::ops::Range;

use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, TextContent};
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

use crate::hel_clipboard::ClipboardImage;

/// Prefix used for drafts whose payload is encoded as JSON.
pub const CHAT_DRAFT_PREFIX: &str = "mjolnir-chat-draft-v1:";

/// The text and inline images currently in the composer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptPayload {
    pub text: String,
    pub images: Vec<PromptImage>,
}

/// One image and the marker that displays it in [`PromptPayload::text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptImage {
    pub range: Range<usize>,
    pub number: u64,
    pub image: ClipboardImage,
}

#[derive(Debug, Deserialize)]
struct PromptPayloadWire {
    text: String,
    #[serde(default)]
    images: Option<Vec<PromptImage>>,
    #[serde(default)]
    image: Option<ClipboardImage>,
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
            images: vec![PromptImage {
                range: start..start + marker(1).len(),
                number: 1,
                image,
            }],
        }
    }

    /// Build ACP content while omitting the inline marker bytes from text
    /// blocks.  Only tracked ranges are interpreted as images.
    #[must_use]
    pub fn content_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::with_capacity(self.images.len() * 2 + 1);
        let mut cursor = 0;

        for image in &self.images {
            assert!(
                image.range.start >= cursor
                    && image.range.start < image.range.end
                    && image.range.end <= self.text.len()
                    && self.text.is_char_boundary(image.range.start)
                    && self.text.is_char_boundary(image.range.end),
                "invalid tracked image range"
            );
            if cursor < image.range.start {
                blocks.push(ContentBlock::Text(TextContent::new(
                    self.text[cursor..image.range.start].to_owned(),
                )));
            }
            blocks.push(ContentBlock::Image(ImageContent::new(
                image.image.data_base64.to_string(),
                image.image.mime_type.clone(),
            )));
            cursor = image.range.end;
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
        if self.images.is_empty() && !self.text.starts_with(CHAT_DRAFT_PREFIX) {
            return self.text.clone();
        }
        let body = serde_json::to_string(self).expect("prompt draft serialization cannot fail");
        format!("{CHAT_DRAFT_PREFIX}{body}")
    }

    /// Decode a session record draft, accepting the historical plain-text and
    /// single-image forms as well as the current tracked-image form.
    pub fn decode_draft(draft: &str) -> Result<Self> {
        let Some(body) = draft.strip_prefix(CHAT_DRAFT_PREFIX) else {
            return Ok(Self::text(draft));
        };
        serde_json::from_str(body).context("decode chat draft envelope")
    }

    /// Remove leading and trailing whitespace while keeping image ranges in
    /// sync.  Marker bytes themselves are never treated as whitespace.
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
        let text = self.text[leading..trailing].to_owned();
        let images = self
            .images
            .into_iter()
            .map(|mut image| {
                assert!(
                    image.range.start >= leading && image.range.end <= trailing,
                    "image marker outside trimmed text"
                );
                image.range = (image.range.start - leading)..(image.range.end - leading);
                image
            })
            .collect();
        Self { text, images }
    }
}

fn decode_wire(wire: PromptPayloadWire) -> std::result::Result<PromptPayload, String> {
    match (wire.images, wire.image) {
        (Some(_), Some(_)) => Err("chat draft contains both images and image fields".to_owned()),
        (Some(images), None) => validate_images(wire.text, images),
        (None, Some(image)) => {
            let image = ClipboardImage::from_base64(image.data_base64, image.mime_type)
                .map_err(|error| format!("validate image in chat draft: {error}"))?;
            Ok(PromptPayload::with_image(wire.text, image))
        }
        (None, None) => Ok(PromptPayload::text(wire.text)),
    }
}

fn validate_images(
    text: String,
    images: Vec<PromptImage>,
) -> std::result::Result<PromptPayload, String> {
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
        let image_bytes = ClipboardImage::from_base64(
            image.image.data_base64.clone(),
            image.image.mime_type.clone(),
        )
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
    })
}

fn marker(number: u64) -> String {
    format!("[image {number}]")
}

/// Replace a byte range in a composer, treating tracked markers as atomic.
/// The returned payload is the removed range with image ranges made relative
/// to that payload.
pub(super) fn replace_range(
    text: &mut String,
    images: &mut Vec<PromptImage>,
    range: Range<usize>,
    inserted: &PromptPayload,
) -> (usize, PromptPayload) {
    let mut start = floor_char_boundary(text, range.start.min(text.len()));
    let mut end = floor_char_boundary(text, range.end.min(text.len()));
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    if start == end {
        if let Some(image) = images
            .iter()
            .find(|image| image.range.start < start && start < image.range.end)
        {
            start = image.range.end;
            end = image.range.end;
        }
    } else {
        let mut expanded_start = start;
        let mut expanded_end = end;
        for image in images.iter() {
            if image.range.end > expanded_start && image.range.start < expanded_end {
                expanded_start = expanded_start.min(image.range.start);
                expanded_end = expanded_end.max(image.range.end);
            }
        }
        start = expanded_start;
        end = expanded_end;
    }

    let removed_images = images
        .iter()
        .filter(|image| image.range.start >= start && image.range.end <= end)
        .map(|image| PromptImage {
            range: (image.range.start - start)..(image.range.end - start),
            number: image.number,
            image: image.image.clone(),
        })
        .collect();
    let removed = PromptPayload {
        text: text[start..end].to_owned(),
        images: removed_images,
    };

    let mut before: Vec<_> = images
        .iter()
        .filter(|image| image.range.end <= start)
        .cloned()
        .collect();
    let mut after: Vec<_> = images
        .iter()
        .filter(|image| image.range.start >= end)
        .map(|image| PromptImage {
            range: (image.range.start - end)..(image.range.end - end),
            number: image.number,
            image: image.image.clone(),
        })
        .collect();
    let mut inserted_images = inserted.images.clone();

    // Existing IDs are stable whenever possible.  Reserve all retained IDs
    // before assigning yanked IDs so an inserted duplicate always gets the
    // fresh number and its marker can be rewritten.
    let max_existing = before
        .iter()
        .chain(after.iter())
        .map(|image| image.number)
        .filter(|number| *number > 0)
        .max()
        .unwrap_or(0);
    let mut next_number = if max_existing == u64::MAX {
        1
    } else {
        max_existing + 1
    };
    let mut used = BTreeSet::new();
    assign_existing_numbers(&mut before, &mut used, &mut next_number);
    assign_existing_numbers(&mut after, &mut used, &mut next_number);
    for image in &mut inserted_images {
        if image.number == 0 || used.contains(&image.number) {
            image.number = allocate_number(&used, &mut next_number);
        }
        used.insert(image.number);
    }

    let (before_text, mut before_images) = render_part(&text[..start], &before);
    let (inserted_text, mut inserted_rendered) = render_part(&inserted.text, &inserted_images);
    let (after_text, mut after_images) = render_part(&text[end..], &after);

    let before_len = before_text.len();
    let inserted_len = inserted_text.len();
    shift_images(&mut inserted_rendered, before_len);
    shift_images(&mut after_images, before_len + inserted_len);
    before_images.append(&mut inserted_rendered);
    before_images.append(&mut after_images);

    let mut output_text = before_text;
    output_text.push_str(&inserted_text);
    output_text.push_str(&after_text);
    *text = output_text;
    *images = before_images;

    (before_len + inserted_len, removed)
}

fn assign_existing_numbers(
    images: &mut [PromptImage],
    used: &mut BTreeSet<u64>,
    next_number: &mut u64,
) {
    for image in images {
        if image.number == 0 || used.contains(&image.number) {
            image.number = allocate_number(used, next_number);
        }
        used.insert(image.number);
    }
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

fn render_part(text: &str, images: &[PromptImage]) -> (String, Vec<PromptImage>) {
    let mut output = String::with_capacity(text.len());
    let mut rendered = Vec::with_capacity(images.len());
    let mut cursor = 0;

    for image in images {
        assert!(
            image.range.start >= cursor
                && image.range.start < image.range.end
                && image.range.end <= text.len()
                && text.is_char_boundary(image.range.start)
                && text.is_char_boundary(image.range.end),
            "invalid tracked image range"
        );
        output.push_str(&text[cursor..image.range.start]);
        let start = output.len();
        output.push_str(&marker(image.number));
        rendered.push(PromptImage {
            range: start..output.len(),
            number: image.number,
            image: image.image.clone(),
        });
        cursor = image.range.end;
    }
    output.push_str(&text[cursor..]);
    (output, rendered)
}

fn shift_images(images: &mut [PromptImage], amount: usize) {
    for image in images {
        image.range.start += amount;
        image.range.end += amount;
    }
}

fn floor_char_boundary(text: &str, mut position: usize) -> usize {
    while position > 0 && !text.is_char_boundary(position) {
        position -= 1;
    }
    position
}

/// Snap a cursor that lies strictly inside a marker to the marker boundary.
pub(super) fn snap_cursor(images: &[PromptImage], position: usize, direction: isize) -> usize {
    for image in images {
        if image.range.start < position && position < image.range.end {
            return if direction < 0 {
                image.range.start
            } else {
                image.range.end
            };
        }
    }
    position
}

/// Insert one tracked marker at `position`, returning the cursor after it.
pub(super) fn insert_image(
    text: &mut String,
    images: &mut Vec<PromptImage>,
    position: usize,
    number: u64,
    image: ClipboardImage,
) -> usize {
    let position = floor_char_boundary(text, snap_cursor(images, position.min(text.len()), 1));
    let marker_text = marker(number);
    let marker_len = marker_text.len();
    text.insert_str(position, &marker_text);
    for existing in images.iter_mut() {
        if existing.range.start >= position {
            existing.range.start += marker_len;
            existing.range.end += marker_len;
        }
    }
    images.push(PromptImage {
        range: position..position + marker_len,
        number,
        image,
    });
    images.sort_by_key(|existing| existing.range.start);
    position + marker_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ContentBlock;
    use base64::Engine;

    fn image(tag: &str) -> ClipboardImage {
        ClipboardImage {
            data_base64: tag.to_owned().into(),
            mime_type: "image/png".to_owned(),
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
            images: vec![PromptImage {
                range: range..range + marker(number).len(),
                number,
                image,
            }],
        }
    }

    #[test]
    fn unicode_offsets_are_bytes() {
        let mut text = "λ".to_owned();
        let mut images = Vec::new();
        let text_end = text.len();
        let cursor = insert_image(&mut text, &mut images, text_end, 1, image("a"));
        assert_eq!(text, "λ[image 1]");
        assert_eq!(images[0].range, 2..11);
        assert_eq!(cursor, 11);
        let mut inserted_text = "é".to_owned();
        let mut inserted_images = Vec::new();
        let (_, removed) = replace_range(
            &mut inserted_text,
            &mut inserted_images,
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
        replace_range(&mut text, &mut images, 0..1, &yanked);
        assert_eq!(text, "[image 2][image 1]b");
        assert_eq!(
            images.iter().map(|item| item.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(images[0].range, 0..9);
        assert_eq!(images[1].range, 9..18);
    }

    #[test]
    fn content_blocks_keep_interleaved_text_and_images() {
        let first = marker(1);
        let second = marker(2);
        let text = format!("a{first}b{second}c");
        let payload = PromptPayload {
            text,
            images: vec![
                PromptImage {
                    range: 1..1 + first.len(),
                    number: 1,
                    image: image("one"),
                },
                PromptImage {
                    range: 1 + first.len() + 1..1 + first.len() + 1 + second.len(),
                    number: 2,
                    image: image("two"),
                },
            ],
        };
        let blocks = payload.content_blocks();
        assert!(matches!(&blocks[0], ContentBlock::Text(content) if content.text == "a"));
        assert!(matches!(&blocks[1], ContentBlock::Image(content) if content.data == "one"));
        assert!(matches!(&blocks[2], ContentBlock::Text(content) if content.text == "b"));
        assert!(matches!(&blocks[3], ContentBlock::Image(content) if content.data == "two"));
        assert!(matches!(&blocks[4], ContentBlock::Text(content) if content.text == "c"));
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
}
