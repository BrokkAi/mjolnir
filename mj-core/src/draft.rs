//! Shared persisted composer-envelope operations. Clipboard processing stays in
//! the composer; storage merges envelopes without interpreting native images.
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

pub const CHAT_DRAFT_PREFIX: &str = "mjolnir-chat-draft-v1:";

struct Envelope {
    fields: Map<String, Value>,
    text: String,
    images: Vec<Value>,
    files: Vec<Value>,
}

impl Envelope {
    fn decode(draft: &str) -> Result<Self> {
        let mut fields = match draft.strip_prefix(CHAT_DRAFT_PREFIX) {
            Some(body) => serde_json::from_str::<Map<String, Value>>(body)
                .context("decode composer draft envelope")?,
            None => Map::from_iter([("text".into(), Value::String(draft.into()))]),
        };
        let mut text = fields
            .remove("text")
            .and_then(|value| value.as_str().map(str::to_owned))
            .context("composer draft is missing text")?;
        let images = fields.remove("images").filter(|value| !value.is_null());
        let legacy = fields.remove("image").filter(|value| !value.is_null());
        ensure!(
            images.is_none() || legacy.is_none(),
            "composer draft has conflicting image fields"
        );
        let images = match (images, legacy) {
            (Some(Value::Array(images)), None) => images,
            (None, Some(image)) => {
                let start = text.len();
                text.push_str("[image 1]");
                vec![
                    json!({"range": {"start": start, "end": text.len()}, "number": 1, "image": image}),
                ]
            }
            (None, None) => Vec::new(),
            _ => anyhow::bail!("composer draft has invalid images"),
        };
        ensure!(
            images.len() <= crate::attachment::MAX_IMAGES,
            "a message can contain at most 10 images"
        );
        let mut previous_end = 0;
        let mut numbers = std::collections::BTreeSet::new();
        for image in &images {
            let (start, end, number) = marker_range(image)?;
            ensure!(
                start >= previous_end
                    && start < end
                    && text.get(start..end) == Some(format!("[image {number}]").as_str())
                    && number != 0
                    && numbers.insert(number),
                "composer draft has invalid image ranges or numbers"
            );
            ensure!(
                image.get("image").is_some_and(Value::is_object),
                "composer draft has invalid image metadata"
            );
            previous_end = end;
        }
        let files = match fields.remove("files") {
            Some(Value::Array(files)) => files,
            None => Vec::new(),
            _ => anyhow::bail!("composer draft has invalid files"),
        };
        ensure!(
            files.len() <= crate::attachment::MAX_FILES,
            "composer draft contains too many files"
        );
        previous_end = 0;
        numbers.clear();
        for file in &files {
            let (start, end, number) = marker_range(file)?;
            let filename = file_filename(file)?;
            ensure!(
                start >= previous_end
                    && number != 0
                    && numbers.insert(number)
                    && text.get(start..end) == Some(file_marker(number, filename).as_str()),
                "composer draft has invalid file ranges or numbers"
            );
            for image in &images {
                let (image_start, image_end, _) = marker_range(image)?;
                ensure!(
                    image_end <= start || image_start >= end,
                    "composer draft has overlapping attachments"
                );
            }
            previous_end = end;
        }
        Ok(Self {
            fields,
            text,
            images,
            files,
        })
    }

    fn encode(mut self) -> Result<String> {
        if self.images.is_empty()
            && self.files.is_empty()
            && self.fields.is_empty()
            && !self.text.starts_with(CHAT_DRAFT_PREFIX)
        {
            return Ok(self.text);
        }
        self.fields.insert("text".into(), Value::String(self.text));
        self.fields
            .insert("images".into(), Value::Array(self.images));
        if !self.files.is_empty() {
            self.fields.insert("files".into(), Value::Array(self.files));
        }
        Ok(format!(
            "{CHAT_DRAFT_PREFIX}{}",
            serde_json::to_string(&self.fields)?
        ))
    }
}

/// Retire exactly one saved startup receipt when its payload becomes a draft.
/// Native submissions and other startup identities keep their saved ownership.
pub fn remove_startup_receipt(draft: &str, command_id: &str) -> Result<String> {
    let mut envelope = Envelope::decode(draft)?;
    let Some(pending) = envelope.fields.get_mut("pending") else {
        return Ok(draft.to_owned());
    };
    let pending = pending
        .as_array_mut()
        .context("invalid pending composer submissions")?;
    let previous_len = pending.len();
    pending.retain(|submission| {
        submission.get("id").and_then(Value::as_str) != Some(command_id)
            || !submission
                .get("startup_origin")
                .is_some_and(Value::is_object)
    });
    if pending.len() == previous_len {
        return Ok(draft.to_owned());
    }
    if pending.is_empty() {
        envelope.fields.remove("pending");
    }
    envelope.encode()
}

/// A bounded, single-line preview of a persisted composer draft.
pub fn preview(draft: &str) -> String {
    let text = match draft.strip_prefix(CHAT_DRAFT_PREFIX) {
        Some(_) => match Envelope::decode(draft) {
            Ok(envelope) => std::borrow::Cow::Owned(envelope.text),
            Err(_) => return "Unreadable draft".into(),
        },
        None => std::borrow::Cow::Borrowed(draft),
    };
    let mut chars = text
        .split(|ch: char| ch.is_whitespace() || ch.is_control())
        .filter(|word| !word.is_empty())
        .enumerate()
        .flat_map(|(index, word)| (index > 0).then_some(' ').into_iter().chain(word.chars()));
    let mut preview: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn marker_range(image: &Value) -> Result<(usize, usize, u64)> {
    let start = image
        .pointer("/range/start")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .context("invalid attachment range start")?;
    let end = image
        .pointer("/range/end")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .context("invalid attachment range end")?;
    let number = image
        .get("number")
        .and_then(Value::as_u64)
        .context("invalid attachment number")?;
    Ok((start, end, number))
}

/// Format a tracked file marker for a sanitized filename.
pub fn file_marker(number: u64, filename: &str) -> String {
    format!("[file {number}: {filename}]")
}

fn file_filename(file: &Value) -> Result<&str> {
    let filename = file
        .pointer("/file/filename")
        .and_then(Value::as_str)
        .context("invalid composer file filename")?;
    ensure!(
        filename == crate::attachment::sanitize_filename(filename),
        "invalid composer file filename"
    );
    Ok(filename)
}

/// Append recovered input while preserving tracked attachments and envelope metadata.
/// Conflicting unknown metadata fails visibly rather than silently discarding it.
pub fn merge_drafts(existing: &str, recovered: &str) -> Result<String> {
    let mut left = Envelope::decode(existing)?;
    let right = Envelope::decode(recovered)?;
    if recovered.is_empty() {
        return Ok(existing.to_owned());
    }
    if existing.is_empty() {
        return Ok(recovered.to_owned());
    }
    ensure!(
        left.images.len() + right.images.len() <= crate::attachment::MAX_IMAGES,
        "recovered draft would contain more than 10 images"
    );
    ensure!(
        left.files.len() + right.files.len() <= crate::attachment::MAX_FILES,
        "recovered draft would contain more than {} files",
        crate::attachment::MAX_FILES
    );
    for (key, value) in right.fields {
        if let Some(previous) = left.fields.get(&key) {
            ensure!(
                previous == &value,
                "cannot merge conflicting composer metadata {key}"
            );
        } else {
            left.fields.insert(key, value);
        }
    }
    left.text.push_str("\n\n");
    let max_number = |markers: &[Value]| {
        markers
            .iter()
            .filter_map(|marker| marker.get("number").and_then(Value::as_u64))
            .max()
            .unwrap_or(0)
    };
    let mut next_image = max_number(&left.images);
    let mut next_file = max_number(&left.files);
    let mut markers = right
        .images
        .into_iter()
        .map(|value| (false, value))
        .chain(right.files.into_iter().map(|value| (true, value)))
        .map(|(is_file, value)| Ok((marker_range(&value)?, is_file, value)))
        .collect::<Result<Vec<_>>>()?;
    markers.sort_by_key(|((start, _, _), _, _)| *start);
    let mut cursor = 0;
    for ((start, end, _), is_file, mut marker) in markers {
        left.text.push_str(&right.text[cursor..start]);
        let next = if is_file {
            &mut next_file
        } else {
            &mut next_image
        };
        *next = next
            .checked_add(1)
            .context("composer attachment number overflow")?;
        let offset = left.text.len();
        if is_file {
            left.text
                .push_str(&file_marker(*next, file_filename(&marker)?));
        } else {
            left.text.push_str(&format!("[image {next}]"));
        }
        marker["number"] = json!(next);
        marker["range"] = json!({"start": offset, "end": left.text.len()});
        if is_file {
            left.files.push(marker);
        } else {
            left.images.push(marker);
        }
        cursor = end;
    }
    left.text.push_str(&right.text[cursor..]);
    left.encode()
}

/// Durable startup drafts must carry installed references, never inline bytes.
pub fn validate_reference_draft(draft: &str) -> Result<()> {
    let envelope = Envelope::decode(draft)?;
    for image in envelope.images {
        let image = &image["image"];
        ensure!(
            image
                .get("data_base64")
                .is_none_or(|value| value.as_str() == Some("")),
            "queued drafts cannot contain inline image bytes"
        );
        let reference: crate::attachment::AttachmentRef = serde_json::from_value(
            image
                .get("reference")
                .cloned()
                .context("queued draft image is not installed")?,
        )?;
        reference.validate()?;
    }
    for file in envelope.files {
        let metadata = file["file"]
            .as_object()
            .context("invalid queued draft file")?;
        ensure!(
            metadata
                .keys()
                .all(|key| matches!(key.as_str(), "filename" | "reference")),
            "queued drafts cannot contain file placeholders or inline bytes"
        );
        let reference: crate::attachment::FileAttachmentRef = serde_json::from_value(
            metadata
                .get("reference")
                .cloned()
                .context("queued draft file is not installed")?,
        )?;
        reference.validate()?;
        ensure!(
            reference.filename == file_filename(&file)?,
            "queued draft file reference filename does not match"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment::{FileAttachmentRef, MAX_FILES};

    fn add_file(text: &mut String, number: u64, filename: &str) -> Value {
        let reference = FileAttachmentRef::new(b"contents", filename, "text/plain".into()).unwrap();
        let start = text.len();
        text.push_str(&file_marker(number, filename));
        json!({"range": {"start": start, "end": text.len()}, "number": number,
            "file": {"filename": filename, "reference": reference}})
    }

    fn draft(text: &str, images: &[Value], files: &[Value]) -> String {
        format!(
            "{CHAT_DRAFT_PREFIX}{}",
            json!({"text": text, "images": images, "files": files})
        )
    }

    #[test]
    fn draft_merge_preserves_interleaved_markers_with_independent_numbers() {
        let mut left = "left ".to_owned();
        let left_file = add_file(&mut left, 8, "left.txt");
        let left_draft = draft(&left, &[], &[left_file]);
        let mut right = "right λ ".to_owned();
        let right_file = add_file(&mut right, 8, "right.txt");
        let start = right.len();
        right.push_str("[image 9]");
        let image =
            json!({"range": {"start": start, "end": right.len()}, "number": 9, "image": {}});
        let tail_file = add_file(&mut right, 1, "tail.txt");
        let right_draft = draft(&right, &[image], &[right_file, tail_file]);
        let plain = Envelope::decode(&merge_drafts("plain", &right_draft).unwrap()).unwrap();
        assert_eq!(
            plain.text,
            "plain\n\nright λ [file 1: right.txt][image 1][file 2: tail.txt]"
        );

        let merged = Envelope::decode(&merge_drafts(&left_draft, &right_draft).unwrap()).unwrap();
        assert_eq!(
            merged.text,
            "left [file 8: left.txt]\n\nright λ [file 9: right.txt][image 1][file 10: tail.txt]"
        );
        assert_eq!(merged.files.len(), 3);
        assert_eq!(merged.images.len(), 1);
    }

    #[test]
    fn draft_rejects_file_overflow_and_an_image_inside_a_filename_marker() {
        let mut text = String::new();
        let files: Vec<_> = (1..=MAX_FILES)
            .map(|i| add_file(&mut text, i as u64, "note.txt"))
            .collect();
        let mut other = String::new();
        let extra = add_file(&mut other, 1, "extra.txt");
        assert!(merge_drafts(&draft(&text, &[], &files), &draft(&other, &[], &[extra])).is_err());

        let mut text = String::new();
        let file = add_file(&mut text, 1, "[image 1].txt");
        let start = text.find("[image 1]").unwrap();
        let image = json!({"range": {"start": start, "end": start + "[image 1]".len()}, "number": 1, "image": {}});
        assert!(Envelope::decode(&draft(&text, &[image], &[file])).is_err());
    }

    #[test]
    fn reference_draft_rejects_file_placeholders_bytes_and_invalid_names() {
        let mut text = String::new();
        let file = add_file(&mut text, 1, "report.txt");
        validate_reference_draft(&draft(&text, &[], std::slice::from_ref(&file))).unwrap();
        for (field, value) in [
            ("placeholder", json!("pending")),
            ("data_base64", json!("YQ==")),
            ("filename", json!("../report.txt")),
        ] {
            let mut invalid = file.clone();
            invalid["file"][field] = value;
            assert!(validate_reference_draft(&draft(&text, &[], &[invalid])).is_err());
        }
        let mut mismatch = file;
        mismatch["file"]["reference"]["filename"] = json!("another.txt");
        assert!(validate_reference_draft(&draft(&text, &[], &[mismatch])).is_err());
    }
}
