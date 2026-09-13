//! Reader and workspace relocation support for Muse Code session logs.
//!
//! Muse stores one JSON value per line.  Most lines are ordinary event
//! envelopes; permission transactions are retained as opaque frames whose
//! children contain JSON text.  The frame bytes are part of Muse's integrity
//! surface, so relocation only rewrites the exact workspace-root string in
//! ordinary metadata records.

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

const RECORD_SCHEMA_VERSION: u64 = 1;
const FRAME_SCHEMA_VERSION: u64 = 1;
const MAX_NATIVE_FILE_BYTES: usize = 1024 * 1024 * 1024;
const WORKSPACE_ROOT_PATH: [&str; 3] = ["payload", "record", "workspace_root"];

/// One parsed Muse event.  Retained-frame children are flattened into this
/// stream for consumers that need to inspect lifecycle or conversation data.
#[derive(Debug, Clone)]
pub struct MuseRecord {
    pub value: Value,
    pub line_number: usize,
    pub retained_frame: bool,
}

/// Parse and validate a Muse session log, flattening records held in retained
/// frames.  The input is never modified.
pub fn read_records(data: &[u8]) -> Result<Vec<MuseRecord>> {
    ensure!(
        data.len() <= MAX_NATIVE_FILE_BYTES,
        "Muse session log is too large ({} bytes; maximum is {} bytes)",
        data.len(),
        MAX_NATIVE_FILE_BYTES
    );
    let mut records = Vec::new();
    let mut ids = BTreeSet::new();
    let mut saw_value = false;

    for (line_number, segment) in data.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let mut line = segment;
        if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        saw_value = true;
        let value: Value = serde_json::from_slice(line)
            .with_context(|| format!("parse Muse session record at line {}", line_number + 1))?;
        if value.get("retained_frame").is_some() {
            validate_frame(&value, line_number + 1, &mut ids, &mut records)?;
        } else {
            validate_record(&value, line_number + 1, &mut ids)?;
            records.push(MuseRecord {
                value,
                line_number: line_number + 1,
                retained_frame: false,
            });
        }
    }

    ensure!(saw_value, "Muse session log is empty");
    Ok(records)
}

/// Relocate all ordinary Muse metadata records to `target_cwd`.
///
/// Opaque retained frames, child JSON strings, and every byte outside the
/// matching workspace-root JSON string are copied exactly.  A child session
/// stream may have no metadata; that is valid and returns the original bytes.
pub fn relocate(data: &[u8], target_cwd: &std::path::Path) -> Result<Vec<u8>> {
    ensure!(target_cwd.is_absolute(), "Muse target cwd is not absolute");
    let target = target_cwd
        .to_str()
        .context("Muse target cwd is not valid UTF-8")?;
    let target_json = serde_json::to_vec(target).context("encode Muse target cwd")?;

    // Validate the complete stream first.  This prevents a malformed suffix
    // from being silently copied after a valid metadata prefix.
    let _ = read_records(data)?;

    let mut output = Vec::with_capacity(data.len());
    let mut offset = 0;
    let mut line_number = 0;
    while offset < data.len() {
        let end = data[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(data.len(), |relative| offset + relative + 1);
        let segment = &data[offset..end];
        let mut line = segment;
        let newline = if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
            b"\n".as_slice()
        } else {
            &[]
        };
        let carriage_return = if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
            b"\r".as_slice()
        } else {
            &[]
        };
        line_number += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            output.extend_from_slice(segment);
            offset = end;
            continue;
        }

        let value: Value = serde_json::from_slice(line)
            .with_context(|| format!("parse Muse session record at line {line_number}"))?;
        let is_metadata = value.get("retained_frame").is_none()
            && value.get("payload_type").and_then(Value::as_str)
                == Some("runtime.session.metadata");
        if !is_metadata {
            output.extend_from_slice(segment);
            offset = end;
            continue;
        }

        let mut spans = Vec::new();
        find_workspace_root_spans(line, &mut spans)
            .with_context(|| format!("find Muse workspace metadata at line {line_number}"))?;
        if spans.is_empty() {
            output.extend_from_slice(segment);
            offset = end;
            continue;
        }
        let mut rewritten = Vec::with_capacity(line.len() + target_json.len());
        let mut cursor = 0;
        for span in spans {
            rewritten.extend_from_slice(&line[cursor..span.start]);
            rewritten.extend_from_slice(&target_json);
            cursor = span.end;
        }
        rewritten.extend_from_slice(&line[cursor..]);
        output.extend_from_slice(&rewritten);
        output.extend_from_slice(carriage_return);
        output.extend_from_slice(newline);
        offset = end;
    }
    Ok(output)
}

fn validate_record(value: &Value, line_number: usize, ids: &mut BTreeSet<String>) -> Result<()> {
    let object = value.as_object().with_context(|| {
        format!("unsupported Muse record at line {line_number}: expected an object")
    })?;
    match object.get("schema_version").and_then(Value::as_u64) {
        Some(RECORD_SCHEMA_VERSION) => {}
        Some(version) => {
            bail!("unsupported Muse record schema version {version} at line {line_number}")
        }
        None => bail!("Muse record at line {line_number} has no schema_version"),
    }
    ensure_string(object, "id", line_number, "record")?;
    let id = object["id"].as_str().expect("validated record id");
    ensure!(
        ids.insert(id.to_owned()),
        "duplicate Muse record id {id:?} at line {line_number}"
    );
    let stream = object
        .get("stream")
        .and_then(Value::as_object)
        .with_context(|| format!("Muse record at line {line_number} has no stream object"))?;
    ensure!(
        stream.get("kind").and_then(Value::as_str) == Some("session"),
        "unsupported Muse stream kind at line {line_number}"
    );
    ensure_string(stream, "id", line_number, "stream")?;
    ensure!(
        object.get("sequence").and_then(Value::as_u64).is_some(),
        "Muse record at line {line_number} has no sequence"
    );
    ensure!(
        object.get("recorded_at").and_then(Value::as_i64).is_some(),
        "Muse record at line {line_number} has no integer recorded_at"
    );
    ensure_string(object, "record_type", line_number, "record")?;
    ensure_string(object, "durability", line_number, "record")?;
    ensure_string(object, "payload_type", line_number, "record")?;
    let payload_schema_version = object
        .get("payload_schema_version")
        .and_then(Value::as_u64)
        .with_context(|| {
            format!("Muse record at line {line_number} has no payload_schema_version")
        })?;
    ensure!(
        object.contains_key("payload"),
        "Muse record at line {line_number} has no payload"
    );
    let payload_type = object.get("payload_type").and_then(Value::as_str);
    let supported = match payload_type {
        // Muse has added fields to the generic runtime-session payload while
        // retaining the v1 run-event shape used by the import projection.
        Some("runtime.session") => matches!(payload_schema_version, 1 | 2),
        Some(
            "runtime.session.metadata"
            | "runtime.user_intent.accepted"
            | "runtime.user_intent.materialized"
            | "session.name.changed",
        ) => payload_schema_version == RECORD_SCHEMA_VERSION,
        // Unknown payloads remain opaque.  Their bytes are retained in the
        // native artifact and must not make an otherwise readable session
        // unusable merely because their schema evolves independently.
        _ => true,
    };
    ensure!(
        supported,
        "unsupported Muse payload schema version {payload_schema_version} for {payload_type:?} at line {line_number}"
    );
    // Child-agent metadata only declares its model/provider and inherits the
    // parent workspace. The top-level importer requires an explicit root.
    if payload_type == Some("runtime.session.metadata")
        && let Some(workspace) = value.pointer("/payload/record/workspace_root")
    {
        let workspace_root = workspace
            .as_str()
            .with_context(|| {
                format!(
                    "Muse metadata record at line {line_number} has no string payload.record.workspace_root"
                )
            })?;
        ensure!(
            !workspace_root.trim().is_empty(),
            "Muse metadata record at line {line_number} has an empty workspace_root"
        );
    }
    Ok(())
}

fn validate_frame(
    value: &Value,
    line_number: usize,
    ids: &mut BTreeSet<String>,
    records: &mut Vec<MuseRecord>,
) -> Result<()> {
    let object = value.as_object().with_context(|| {
        format!("unsupported Muse retained frame at line {line_number}: expected an object")
    })?;
    let frame_name = object
        .get("retained_frame")
        .and_then(Value::as_str)
        .with_context(|| format!("Muse retained frame at line {line_number} has no name"))?;
    ensure!(
        !frame_name.is_empty(),
        "Muse retained frame at line {line_number} has an empty name"
    );
    match object.get("frame_schema_version").and_then(Value::as_u64) {
        Some(FRAME_SCHEMA_VERSION) => {}
        Some(version) => {
            bail!("unsupported Muse retained-frame schema version {version} at line {line_number}")
        }
        None => bail!("Muse retained frame at line {line_number} has no schema version"),
    }
    let children = object
        .get("children")
        .and_then(Value::as_array)
        .with_context(|| format!("Muse retained frame at line {line_number} has no children"))?;
    for (child_index, child) in children.iter().enumerate() {
        let child_object = child.as_object().with_context(|| {
            format!(
                "Muse retained frame at line {line_number} child {child_index} is not an object"
            )
        })?;
        ensure!(
            child_object
                .get("child_index")
                .and_then(Value::as_u64)
                .is_some(),
            "Muse retained frame at line {line_number} child {child_index} has no child_index"
        );
        let record_json = child_object
            .get("record_json")
            .and_then(Value::as_str)
            .with_context(|| {
                format!(
                    "Muse retained frame at line {line_number} child {child_index} has no record_json"
                )
            })?;
        let record: Value = serde_json::from_str(record_json).with_context(|| {
            format!("parse Muse retained frame at line {line_number} child {child_index} record")
        })?;
        validate_record(&record, line_number, ids).with_context(|| {
            format!("validate Muse retained frame at line {line_number} child {child_index} record")
        })?;
        records.push(MuseRecord {
            value: record,
            line_number,
            retained_frame: true,
        });
    }
    Ok(())
}

fn ensure_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    line_number: usize,
    kind: &str,
) -> Result<()> {
    ensure!(
        object.get(key).and_then(Value::as_str).is_some(),
        "Muse {kind} at line {line_number} has no string {key}"
    );
    Ok(())
}

/// Find all string spans at `/payload/record/workspace_root`, without
/// serializing the surrounding JSON. The scanner is deliberately small and
/// only returns spans after serde_json has already validated the line.
fn find_workspace_root_spans(line: &[u8], spans: &mut Vec<Range<usize>>) -> Result<()> {
    let mut scanner = JsonScanner {
        bytes: line,
        position: 0,
    };
    let mut path = Vec::new();
    scanner.scan_value(&mut path, spans)?;
    scanner.skip_whitespace();
    ensure!(
        scanner.position == line.len(),
        "Muse metadata line has trailing JSON data"
    );
    Ok(())
}

struct JsonScanner<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl JsonScanner<'_> {
    fn skip_whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn scan_value(&mut self, path: &mut Vec<String>, spans: &mut Vec<Range<usize>>) -> Result<()> {
        self.skip_whitespace();
        match self.bytes.get(self.position).copied() {
            Some(b'{') => self.scan_object(path, spans),
            Some(b'[') => self.scan_array(path, spans),
            Some(b'"') => {
                let span = self.scan_string()?;
                if path.as_slice() == WORKSPACE_ROOT_PATH {
                    spans.push(span);
                }
                Ok(())
            }
            Some(_) => self.scan_scalar(),
            None => bail!("unexpected end of Muse metadata JSON"),
        }
    }

    fn scan_object(&mut self, path: &mut Vec<String>, spans: &mut Vec<Range<usize>>) -> Result<()> {
        self.position += 1;
        self.skip_whitespace();
        if self.bytes.get(self.position) == Some(&b'}') {
            self.position += 1;
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            let key_span = self.scan_string()?;
            let key: String =
                serde_json::from_slice(&self.bytes[key_span]).context("decode Muse JSON key")?;
            self.skip_whitespace();
            ensure!(
                self.bytes.get(self.position) == Some(&b':'),
                "Muse metadata object is missing a colon"
            );
            self.position += 1;
            path.push(key);
            self.scan_value(path, spans)?;
            path.pop();
            self.skip_whitespace();
            match self.bytes.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(());
                }
                _ => bail!("Muse metadata object is missing a comma or closing brace"),
            }
        }
    }

    fn scan_array(&mut self, path: &mut Vec<String>, spans: &mut Vec<Range<usize>>) -> Result<()> {
        self.position += 1;
        self.skip_whitespace();
        if self.bytes.get(self.position) == Some(&b']') {
            self.position += 1;
            return Ok(());
        }
        loop {
            self.scan_value(path, spans)?;
            self.skip_whitespace();
            match self.bytes.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    return Ok(());
                }
                _ => bail!("Muse metadata array is missing a comma or closing bracket"),
            }
        }
    }

    fn scan_scalar(&mut self) -> Result<()> {
        let start = self.position;
        while let Some(byte) = self.bytes.get(self.position) {
            if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                break;
            }
            self.position += 1;
        }
        ensure!(
            self.position > start,
            "Muse metadata contains an empty JSON value"
        );
        Ok(())
    }

    fn scan_string(&mut self) -> Result<Range<usize>> {
        let start = self.position;
        ensure!(
            self.bytes.get(self.position) == Some(&b'"'),
            "Muse metadata expected a JSON string"
        );
        self.position += 1;
        while let Some(byte) = self.bytes.get(self.position).copied() {
            match byte {
                b'\\' => {
                    self.position += 1;
                    ensure!(
                        self.bytes.get(self.position).is_some(),
                        "Muse metadata string ends after an escape"
                    );
                    self.position += 1;
                }
                b'"' => {
                    self.position += 1;
                    let span = start..self.position;
                    let _: String = serde_json::from_slice(&self.bytes[span.clone()])
                        .context("decode Muse metadata string")?;
                    return Ok(span);
                }
                _ => self.position += 1,
            }
        }
        bail!("Muse metadata string is unterminated")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sequence: u64, payload_type: &str, payload: Value) -> String {
        serde_json::json!({
            "schema_version": 1,
            "id": format!("record-{sequence}"),
            "stream": {"kind": "session", "id": "session-1"},
            "sequence": sequence,
            "recorded_at": 1_788_871_599_426_557_i64 + sequence as i64,
            "record_type": "event",
            "durability": "durable",
            "payload_type": payload_type,
            "payload_schema_version": 1,
            "payload": payload,
        })
        .to_string()
    }

    #[test]
    fn relocation_preserves_child_metadata_with_inherited_workspace() {
        let source = record(
            1,
            "runtime.session.metadata",
            serde_json::json!({"kind":"metadata","record":{"provider_id":"muse","model_id":"test"}}),
        );
        assert_eq!(
            relocate(source.as_bytes(), std::path::Path::new("/new")).unwrap(),
            source.as_bytes()
        );
    }

    #[test]
    fn relocation_preserves_opaque_frame_and_rewrites_repeated_metadata() {
        let metadata = record(
            3,
            "runtime.session.metadata",
            serde_json::json!({"kind":"metadata","record":{"workspace_root":"/old","other":"keep"}}),
        );
        let second_metadata = record(
            4,
            "runtime.session.metadata",
            serde_json::json!({"kind":"metadata","record":{"workspace_root":"/old-again"}}),
        );
        let child = record(
            1,
            "runtime.session.permission_format_declared",
            serde_json::json!({"format":"profile_v1"}),
        );
        let frame = serde_json::json!({
            "retained_frame":"session_permission_transaction",
            "frame_schema_version":1,
            "outer_log_ordinal":1,
            "transaction_id":"tx-1",
            "children":[{"child_index":0,"record_json":child}],
            "content_sha256":"sha256:opaque",
        })
        .to_string();
        let source = format!("{frame}\n{metadata}\r\n{second_metadata}\n").into_bytes();
        let relocated = relocate(&source, std::path::Path::new("/new/workspace")).unwrap();
        let text = String::from_utf8(relocated.clone()).unwrap();
        assert!(text.contains(r#""workspace_root":"/new/workspace""#));
        let original_frame = format!("{frame}\n");
        assert!(text.starts_with(&original_frame));
        assert!(text.ends_with("\n"));
        assert_eq!(read_records(&relocated).unwrap().len(), 3);
    }

    #[test]
    fn relocation_handles_large_stream_and_rejects_unknown_envelope() {
        let mut source = String::new();
        for sequence in 1..=2_000 {
            source.push_str(&record(
                sequence,
                "runtime.session",
                serde_json::json!({"kind":"padding","text":"x".repeat(64)}),
            ));
            source.push('\n');
        }
        assert!(source.len() > 64 * 1024);
        let relocated = relocate(source.as_bytes(), std::path::Path::new("/new")).unwrap();
        assert_eq!(relocated, source.as_bytes());
        let error = relocate(
            b"{\"not\":\"a Muse record\"}\n",
            std::path::Path::new("/new"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("schema_version"));
    }

    #[test]
    fn interpreted_payloads_reject_unsupported_schema_versions() {
        let mut metadata: Value = serde_json::from_str(&record(
            1,
            "runtime.session.metadata",
            serde_json::json!({
                "kind": "metadata",
                "record": {"workspace_root": "/old"}
            }),
        ))
        .unwrap();
        metadata["payload_schema_version"] = Value::from(2);
        let error = read_records((serde_json::to_string(&metadata).unwrap() + "\n").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported Muse payload schema version 2"));

        let mut runtime: Value = serde_json::from_str(&record(
            1,
            "runtime.session",
            serde_json::json!({"kind": "run", "event": {"kind": "started"}}),
        ))
        .unwrap();
        runtime["payload_schema_version"] = Value::from(3);
        let error = read_records((serde_json::to_string(&runtime).unwrap() + "\n").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported Muse payload schema version 3"));

        runtime["payload_schema_version"] = Value::from(2);
        assert!(read_records((serde_json::to_string(&runtime).unwrap() + "\n").as_bytes()).is_ok());
    }
}
