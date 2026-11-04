//! Shared persisted composer-envelope operations. Clipboard processing stays in
//! the composer; storage merges envelopes without interpreting native images.
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

pub const CHAT_DRAFT_PREFIX: &str = "mjolnir-chat-draft-v1:";

struct Envelope {
    fields: Map<String, Value>,
    text: String,
    images: Vec<Value>,
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
            let (start, end, number) = image_range(image)?;
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
        Ok(Self {
            fields,
            text,
            images,
        })
    }

    fn encode(mut self) -> Result<String> {
        if self.images.is_empty()
            && self.fields.is_empty()
            && !self.text.starts_with(CHAT_DRAFT_PREFIX)
        {
            return Ok(self.text);
        }
        self.fields.insert("text".into(), Value::String(self.text));
        self.fields
            .insert("images".into(), Value::Array(self.images));
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

fn image_range(image: &Value) -> Result<(usize, usize, u64)> {
    let start = image
        .pointer("/range/start")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .context("invalid image range start")?;
    let end = image
        .pointer("/range/end")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .context("invalid image range end")?;
    let number = image
        .get("number")
        .and_then(Value::as_u64)
        .context("invalid image number")?;
    Ok((start, end, number))
}

/// Append recovered input while preserving tracked images and envelope metadata.
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
    let mut next = left
        .images
        .iter()
        .filter_map(|image| image.get("number").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let mut cursor = 0;
    for mut image in right.images {
        let (start, end, _) = image_range(&image)?;
        left.text.push_str(&right.text[cursor..start]);
        next = next
            .checked_add(1)
            .context("composer image number overflow")?;
        let offset = left.text.len();
        left.text.push_str(&format!("[image {next}]"));
        image["number"] = json!(next);
        image["range"]["start"] = json!(offset);
        image["range"]["end"] = json!(left.text.len());
        left.images.push(image);
        cursor = end;
    }
    left.text.push_str(&right.text[cursor..]);
    left.encode()
}

/// Durable startup drafts must carry installed references, never inline bytes.
pub fn validate_reference_draft(draft: &str) -> Result<()> {
    for image in Envelope::decode(draft)?.images {
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
    Ok(())
}
