//! DeepSeek Harness (DSH) v0 native session storage.
//!
//! DSH writes one JSONL session per directory.  The compressed form is a
//! concatenation of independent Zstandard frames: the first frame contains
//! only the header and subsequent frames contain event batches.  This module
//! keeps the physical bytes available to checkpoint restoration while also
//! exposing the lossless row decoder used by native import.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

const SESSION_FORMAT_VERSION: u64 = 0;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const MAX_SESSION_DECOMPRESSED_BYTES: usize = 512 * 1024 * 1024;
const MAX_HEADER_DECOMPRESSED_BYTES: usize = 1024 * 1024;

/// The decoded header and logical events in one DSH session log.
///
/// Packed rows are expanded to their original `assistant/chunk` events.  The
/// header is kept as a JSON value so callers can preserve fields introduced by
/// the harness without making the core reader depend on its entire schema.
#[derive(Debug, Clone)]
pub struct SessionLog {
    pub header: Value,
    pub events: Vec<Value>,
}

/// Return DSH's readable project directory key for an on-disk working path.
///
/// DSH's JavaScript implementation operates on UTF-16 code units.  Iterating
/// `encode_utf16` here is therefore intentional: an astral character becomes
/// two `~XXXX` escapes, exactly as it does in DSH, rather than one Unicode
/// scalar value.
pub fn project_key(cwd: &Path) -> Result<String> {
    let raw = cwd
        .to_str()
        .context("DeepSeek session cwd is not valid UTF-8")?;
    ensure!(
        !raw.is_empty(),
        "cannot encode an empty DeepSeek project path"
    );

    let mut readable = String::new();
    let mut separator_run = false;
    for code in raw.encode_utf16() {
        if code == b'/' as u16 || code == b'\\' as u16 || code == b':' as u16 {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else if is_safe_code_unit(code) {
            readable.push(code as u8 as char);
            separator_run = false;
        } else {
            use std::fmt::Write as _;
            write!(&mut readable, "~{code:04X}").expect("writing to String cannot fail");
            separator_run = false;
        }
    }

    let readable = readable.trim_start_matches('-');
    let readable = if readable.is_empty() {
        "root"
    } else {
        readable
    };
    let readable = readable.get(..readable.len().min(251)).unwrap_or(readable);
    Ok(format!("--{readable}--"))
}

/// Encode one DSH session id as the safe single path segment used by DSH.
pub fn encode_segment(raw: &str) -> Result<String> {
    ensure!(
        !raw.is_empty(),
        "cannot encode an empty DeepSeek session id"
    );
    if raw == "." {
        return Ok("~002E".into());
    }
    if raw == ".." {
        return Ok("~002E~002E".into());
    }
    let mut encoded = String::new();
    for code in raw.encode_utf16() {
        if is_safe_code_unit(code) && code != b'~' as u16 {
            encoded.push(code as u8 as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "~{code:04X}").expect("writing to String cannot fail");
        }
    }
    Ok(encoded)
}

/// Decode one DSH UTF-16 path segment.  Invalid escapes return `None`.
pub fn decode_segment(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let mut units = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            let digits = raw.get(index + 1..index + 5)?;
            if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            units.push(u16::from_str_radix(digits, 16).ok()?);
            index += 5;
        } else {
            let character = raw[index..].chars().next()?;
            if !is_safe_path_character(character) || character == '~' {
                return None;
            }
            units.push(character as u16);
            index += character.len_utf8();
        }
    }
    String::from_utf16(&units).ok()
}

/// Whether a path names one of DSH's canonical session-log generations.
///
/// Version zero uses the suffix-only names.  Later canonical names are still
/// recognized so callers can report an unsupported generation instead of
/// silently selecting an older v0 file in the same directory.
pub fn is_session_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if matches!(name, "session.jsonl" | "session.jsonl.zstd") {
        return true;
    }
    let Some(rest) = name.strip_prefix("session.v") else {
        return false;
    };
    let rest = rest
        .strip_suffix(".jsonl.zstd")
        .or_else(|| rest.strip_suffix(".jsonl"));
    let Some(version) = rest else {
        return false;
    };
    !version.is_empty()
        && !version.starts_with('0')
        && version.bytes().all(|byte| byte.is_ascii_digit())
}

/// Return the canonical generation text for a session-log filename.
///
/// This is deliberately textual so an arbitrarily large future generation can
/// still be identified and rejected with a useful error.
pub fn session_generation(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if matches!(name, "session.jsonl" | "session.jsonl.zstd") {
        return Some("0".into());
    }
    let rest = name.strip_prefix("session.v")?;
    let version = rest
        .strip_suffix(".jsonl.zstd")
        .or_else(|| rest.strip_suffix(".jsonl"))?;
    (!version.is_empty()
        && !version.starts_with('0')
        && version.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| version.to_owned())
}

/// Decode a DSH v0 native log from stable file bytes.
pub fn read(path: &Path, data: &[u8]) -> Result<SessionLog> {
    ensure!(
        is_session_log(path),
        "unsupported DeepSeek session artifact path {}",
        path.display()
    );
    if let Some(generation) = session_generation(path)
        && generation != "0"
    {
        bail!(
            "DeepSeek session log {} uses unsupported format generation v{}; upgrade the harness",
            path.display(),
            generation
        );
    }
    let plaintext = if is_zstd_log(path) {
        let frames = scan_zstd_frames(data)
            .with_context(|| format!("scan DeepSeek session log {}", path.display()))?;
        let first = frames
            .first()
            .copied()
            .context("empty or header-less DeepSeek Zstandard session log")?;
        validate_zstd_header_frame(path, &data[first.0..first.1])?;
        decode_zstd(data, MAX_SESSION_DECOMPRESSED_BYTES)
            .with_context(|| format!("decompress DeepSeek session log {}", path.display()))?
    } else {
        data.to_vec()
    };
    parse_plaintext(path, &plaintext)
}

/// Alias that makes the physical nature of [`read`] explicit at call sites.
pub fn read_log(path: &Path, data: &[u8]) -> Result<SessionLog> {
    read(path, data)
}

/// Relocate a v0 session's persisted workspace while retaining native bytes.
///
/// Plain logs replace only the header line.  Compressed logs replace only the
/// first (header) frame; every event frame is copied byte-for-byte.  This is
/// important for DSH's opaque event payloads and for its checksum/framing
/// semantics.
pub fn relocate(path: &Path, data: &[u8], target_cwd: &Path) -> Result<Vec<u8>> {
    ensure!(
        is_session_log(path),
        "unsupported DeepSeek session artifact path {}",
        path.display()
    );
    if let Some(generation) = session_generation(path)
        && generation != "0"
    {
        bail!(
            "DeepSeek session log {} uses unsupported format generation v{}; upgrade the harness",
            path.display(),
            generation
        );
    }
    ensure!(
        target_cwd.is_absolute(),
        "DeepSeek target cwd is not absolute: {}",
        target_cwd.display()
    );
    let target = target_cwd
        .to_str()
        .context("DeepSeek target cwd is not valid UTF-8")?;

    if is_zstd_log(path) {
        let frames = scan_zstd_frames(data)
            .with_context(|| format!("scan DeepSeek session log {}", path.display()))?;
        let first = frames
            .first()
            .copied()
            .context("empty or header-less DeepSeek Zstandard session log")?;
        let header = validate_zstd_header_frame(path, &data[first.0..first.1])?;
        let current = header.get("cwd").and_then(Value::as_str);
        if current == Some(target) {
            return Ok(data.to_vec());
        }
        let relocated = relocate_header(path, header, target)?;
        let encoded = encode_header_frame(&relocated)
            .with_context(|| format!("compress DeepSeek header frame {}", path.display()))?;
        let mut output = encoded;
        output.extend_from_slice(&data[first.1..]);
        Ok(output)
    } else {
        let (header, header_end) = parse_header_line(path, data)?;
        let current = header.get("cwd").and_then(Value::as_str);
        if current == Some(target) {
            return Ok(data.to_vec());
        }
        let relocated = relocate_header(path, header, target)?;
        let encoded =
            serde_json::to_vec(&relocated).context("serialize DeepSeek session header")?;
        let mut output = encoded;
        output.push(b'\n');
        output.extend_from_slice(&data[header_end..]);
        Ok(output)
    }
}

fn relocate_header(path: &Path, mut header: Value, target: &str) -> Result<Value> {
    let object = header
        .as_object_mut()
        .with_context(|| format!("DeepSeek session {} has an invalid header", path.display()))?;
    object.insert("cwd".into(), Value::String(target.to_owned()));
    validate_header(path, &header)?;
    Ok(header)
}

fn parse_plaintext(path: &Path, plaintext: &[u8]) -> Result<SessionLog> {
    let (header, header_end) = parse_header_line(path, plaintext)?;
    let mut events = Vec::new();
    let mut expected_seq = 0_u64;
    let body = &plaintext[header_end..];
    for (line_index, line) in body.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        ensure!(
            !line.is_empty(),
            "corrupt DeepSeek session log {}: empty event line {}",
            path.display(),
            line_index + 2
        );
        let value: Value = serde_json::from_slice(line).with_context(|| {
            format!(
                "parse DeepSeek session {} event line {}",
                path.display(),
                line_index + 2
            )
        })?;
        let decoded = decode_storage_record(path, value).with_context(|| {
            format!(
                "decode DeepSeek session {} event line {}",
                path.display(),
                line_index + 2
            )
        })?;
        for event in decoded {
            let seq = event
                .get("seq")
                .and_then(Value::as_u64)
                .context("DeepSeek event seq is not a non-negative integer")?;
            ensure!(
                seq == expected_seq,
                "corrupt DeepSeek session log {}: expected event seq {}, got {}",
                path.display(),
                expected_seq,
                seq
            );
            ensure!(
                event.get("type").and_then(Value::as_str).is_some(),
                "DeepSeek event type is missing"
            );
            ensure!(
                event.get("time").and_then(safe_integer).is_some(),
                "DeepSeek event time is not an integer"
            );
            ensure!(
                event.get("data").is_some(),
                "DeepSeek event data is missing"
            );
            events.push(event);
            expected_seq = expected_seq
                .checked_add(1)
                .context("DeepSeek session event sequence overflow")?;
        }
    }
    Ok(SessionLog { header, events })
}

fn parse_header_line(path: &Path, data: &[u8]) -> Result<(Value, usize)> {
    let newline = data
        .iter()
        .position(|byte| *byte == b'\n')
        .context("empty or header-less DeepSeek session log")?;
    let header: Value = serde_json::from_slice(&data[..newline])
        .with_context(|| format!("parse DeepSeek session {} header line", path.display()))?;
    validate_header(path, &header)?;
    Ok((header, newline + 1))
}

fn validate_header(path: &Path, header: &Value) -> Result<()> {
    let object = header.as_object().with_context(|| {
        format!(
            "corrupt DeepSeek session log {}: header is not an object",
            path.display()
        )
    })?;
    ensure!(
        object.get("type").and_then(Value::as_str) == Some("session"),
        "corrupt DeepSeek session log {}: first line is not a session header",
        path.display()
    );
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .context("corrupt DeepSeek session header: version is not an integer")?;
    ensure!(
        version == SESSION_FORMAT_VERSION,
        "DeepSeek session log {} uses unsupported format version {}; upgrade the harness",
        path.display(),
        version
    );
    ensure!(
        object
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "corrupt DeepSeek session log {}: header id is missing",
        path.display()
    );
    ensure!(
        object
            .get("createdAt")
            .and_then(safe_nonnegative_integer)
            .is_some(),
        "corrupt DeepSeek session log {}: header createdAt is invalid",
        path.display()
    );
    ensure!(
        object
            .get("delegationDepth")
            .and_then(safe_nonnegative_integer)
            .is_some(),
        "corrupt DeepSeek session log {}: delegationDepth is invalid",
        path.display()
    );
    ensure!(
        !object.contains_key("sandboxMode") && !object.contains_key("approvalPolicy"),
        "unsupported DeepSeek session header policy fields in {}",
        path.display()
    );
    if let Some(cwd) = object.get("cwd") {
        let cwd = cwd
            .as_str()
            .context("corrupt DeepSeek session header: cwd is not a string")?;
        ensure!(
            !cwd.is_empty(),
            "corrupt DeepSeek session header: cwd is empty"
        );
        ensure!(
            Path::new(cwd).is_absolute(),
            "corrupt DeepSeek session header: cwd is not absolute: {}",
            cwd
        );
    }
    if let Some(seed_length) = object.get("seedLength") {
        ensure!(
            safe_nonnegative_integer(seed_length).is_some(),
            "corrupt DeepSeek session header: seedLength is invalid"
        );
    }
    if let Some(origin) = object.get("origin") {
        ensure!(
            origin.as_str() == Some("subagent"),
            "corrupt DeepSeek session header: origin is invalid"
        );
    }
    if let Some(parent) = object.get("parentSession") {
        ensure!(
            parent.as_str().is_some_and(|id| !id.is_empty()),
            "corrupt DeepSeek session header: parentSession is invalid"
        );
    }
    Ok(())
}

fn decode_storage_record(path: &Path, value: Value) -> Result<Vec<Value>> {
    let Some(object) = value.as_object() else {
        bail!("DeepSeek storage record is not an object");
    };
    let Some(tag) = object.get("type").and_then(Value::as_str) else {
        bail!("DeepSeek storage record has no type");
    };
    if !matches!(tag, "text-chunks" | "reasoning-chunks" | "tool-call-chunks") {
        return Ok(vec![Value::Object(object.clone())]);
    }
    decode_packed_row(path, object, tag)
}

fn decode_packed_row(path: &Path, object: &Map<String, Value>, tag: &str) -> Result<Vec<Value>> {
    ensure!(
        object.len() == 4
            && object.contains_key("type")
            && object.contains_key("seq0")
            && object.contains_key("time0")
            && object.contains_key("data"),
        "malformed {} storage row in {}: invalid envelope",
        tag,
        path.display()
    );
    let seq0 = object
        .get("seq0")
        .and_then(Value::as_u64)
        .context("packed DeepSeek row seq0 is invalid")?;
    let mut time = object
        .get("time0")
        .and_then(safe_integer)
        .context("packed DeepSeek row time0 is invalid")?;
    let data = object
        .get("data")
        .and_then(Value::as_object)
        .context("packed DeepSeek row data is invalid")?;
    let expected_data_keys: &[&str] = match tag {
        "tool-call-chunks" => {
            if data.contains_key("name") {
                &["turn", "step", "index", "dt", "args", "id", "name"]
            } else {
                &["turn", "step", "index", "dt", "args", "id"]
            }
        }
        _ => &["turn", "step", "index", "dt", "texts"],
    };
    ensure!(
        data.len() == expected_data_keys.len()
            && expected_data_keys.iter().all(|key| data.contains_key(*key)),
        "malformed {} storage row in {}: invalid data fields",
        tag,
        path.display()
    );
    let turn = data
        .get("turn")
        .and_then(safe_integer)
        .context("packed DeepSeek row turn is invalid")?;
    let step = data
        .get("step")
        .and_then(safe_integer)
        .context("packed DeepSeek row step is invalid")?;
    let index = data
        .get("index")
        .and_then(safe_integer)
        .context("packed DeepSeek row index is invalid")?;
    let dt = data
        .get("dt")
        .and_then(Value::as_array)
        .context("packed DeepSeek row dt is invalid")?;
    let payload_key = if tag == "tool-call-chunks" {
        "args"
    } else {
        "texts"
    };
    let payload = data
        .get(payload_key)
        .and_then(Value::as_array)
        .context("packed DeepSeek row payload is invalid")?;
    ensure!(
        !payload.is_empty() && payload.iter().all(|value| value.as_str().is_some()),
        "malformed {} storage row: payload must be a non-empty string array",
        tag
    );
    ensure!(
        dt.len() + 1 == payload.len() && dt.iter().all(|value| safe_integer(value).is_some()),
        "malformed {} storage row: dt length does not match payload",
        tag
    );
    let (tool_id, tool_name) = if tag == "tool-call-chunks" {
        let id = data
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("packed DeepSeek tool-call row id is invalid")?;
        let name = data.get("name").and_then(Value::as_str).map(str::to_owned);
        (Some(id.to_owned()), name)
    } else {
        (None, None)
    };

    let mut events = Vec::with_capacity(payload.len());
    for (offset, value) in payload.iter().enumerate() {
        if offset > 0 {
            time = time
                .checked_add(
                    dt[offset - 1]
                        .as_i64()
                        .or_else(|| dt[offset - 1].as_u64().and_then(|n| i64::try_from(n).ok()))
                        .unwrap(),
                )
                .context("packed DeepSeek row timestamp overflow")?;
        }
        let seq = seq0
            .checked_add(offset as u64)
            .context("packed DeepSeek row sequence overflow")?;
        let text = value.as_str().expect("payload was validated as strings");
        let chunk = if tag == "text-chunks" {
            json!({"type": "text-delta", "index": index, "text": text})
        } else if tag == "reasoning-chunks" {
            json!({"type": "reasoning-delta", "index": index, "text": text})
        } else {
            let mut chunk = Map::new();
            chunk.insert("type".into(), Value::String("tool-call-delta".into()));
            chunk.insert("index".into(), json!(index));
            chunk.insert("id".into(), Value::String(tool_id.clone().unwrap()));
            if let Some(name) = &tool_name {
                chunk.insert("name".into(), Value::String(name.clone()));
            }
            chunk.insert("argumentsDelta".into(), Value::String(text.into()));
            Value::Object(chunk)
        };
        events.push(json!({
            "type": "assistant/chunk",
            "seq": seq,
            "time": time,
            "data": {"turn": turn, "step": step, "chunk": chunk}
        }));
    }
    Ok(events)
}

fn safe_integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
}

fn safe_nonnegative_integer(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn is_safe_path_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

fn is_safe_code_unit(code: u16) -> bool {
    let Some(byte) = u8::try_from(code).ok() else {
        return false;
    };
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn is_zstd_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zstd"))
}

fn decode_zstd(data: &[u8], max_output_bytes: usize) -> Result<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(data))
        .context("create DeepSeek Zstandard decoder")?;
    let mut output = Vec::new();
    decoder
        .take((max_output_bytes as u64).saturating_add(1))
        .read_to_end(&mut output)
        .context("decode DeepSeek Zstandard frames")?;
    ensure!(
        output.len() <= max_output_bytes,
        "DeepSeek Zstandard session log expands beyond the {} byte limit",
        max_output_bytes
    );
    Ok(output)
}

fn validate_zstd_header_frame(path: &Path, frame: &[u8]) -> Result<Value> {
    let plaintext = decode_zstd(frame, MAX_HEADER_DECOMPRESSED_BYTES)
        .with_context(|| format!("decompress DeepSeek header frame {}", path.display()))?;
    let (header, header_end) = parse_header_line(path, &plaintext)?;
    ensure!(
        header_end == plaintext.len(),
        "corrupt DeepSeek Zstandard session log {}: first frame contains event data",
        path.display()
    );
    Ok(header)
}

fn encode_header_frame(header: &Value) -> Result<Vec<u8>> {
    let mut plaintext = serde_json::to_vec(header).context("serialize DeepSeek session header")?;
    plaintext.push(b'\n');
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3)
        .context("create DeepSeek Zstandard encoder")?;
    encoder
        .include_checksum(true)
        .context("enable DeepSeek Zstandard checksum")?;
    encoder
        .write_all(&plaintext)
        .context("write DeepSeek session header")?;
    encoder.finish().context("finish DeepSeek session header")
}

fn scan_zstd_frames(data: &[u8]) -> Result<Vec<(usize, usize)>> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let start = offset;
        ensure!(
            data.len() - offset >= ZSTD_MAGIC.len(),
            "corrupt DeepSeek Zstandard session log: truncated frame header"
        );
        ensure!(
            data[offset..offset + ZSTD_MAGIC.len()] == ZSTD_MAGIC,
            "corrupt DeepSeek Zstandard session log: invalid frame magic at byte {}",
            offset
        );
        let frame_size = zstd::zstd_safe::find_frame_compressed_size(&data[offset..])
            .map_err(|code| anyhow::anyhow!("Zstandard frame parser error code {code}"))?;
        ensure!(
            frame_size > 0 && frame_size <= data.len() - offset,
            "corrupt DeepSeek Zstandard session log: invalid frame size at byte {}",
            offset
        );
        offset += frame_size;
        ensure!(
            offset > start,
            "corrupt DeepSeek Zstandard session log: empty frame"
        );
        frames.push((start, offset));
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(id: &str, cwd: &str) -> Value {
        json!({
            "type": "session",
            "version": 0,
            "id": id,
            "createdAt": 1,
            "cwd": cwd,
            "delegationDepth": 0,
        })
    }

    fn event(event_type: &str, seq: u64, time: i64, data: Value) -> Value {
        json!({
            "type": event_type,
            "seq": seq,
            "time": time,
            "data": data,
        })
    }

    fn jsonl(header: &Value, records: &[Value]) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(header).unwrap();
        bytes.push(b'\n');
        for record in records {
            bytes.extend_from_slice(&serde_json::to_vec(record).unwrap());
            bytes.push(b'\n');
        }
        bytes
    }

    fn zstd_frame(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn project_and_session_paths_match_dsh_utf16_encoding() {
        assert_eq!(
            encode_segment("a/é/😀").unwrap(),
            "a~002F~00E9~002F~D83D~DE00"
        );
        assert_eq!(
            project_key(Path::new("/work/é/😀")).unwrap(),
            "--work-~00E9-~D83D~DE00--"
        );
        assert_eq!(
            decode_segment("a~002F~00E9~002F~D83D~DE00").as_deref(),
            Some("a/é/😀")
        );
    }

    #[test]
    fn reads_large_packed_zstd_rows_without_duplicate_chunks() {
        let header = header("session-1", "/work");
        let text = "x".repeat(70 * 1024);
        let records = vec![
            event("turn/start", 0, 1, json!({"turn": 1})),
            json!({
                "type": "text-chunks",
                "seq0": 1,
                "time0": 2,
                "data": {
                    "turn": 1,
                    "step": 1,
                    "index": 0,
                    "dt": [1, 1],
                    "texts": [&text, &text, &text],
                },
            }),
            event(
                "turn/end",
                4,
                5,
                json!({"turn": 1, "reason": {"kind": "completed"}}),
            ),
        ];
        let header_bytes = jsonl(&header, &[]);
        let body = jsonl(&json!({"ignored": true}), &records);
        let mut compressed = zstd_frame(&header_bytes);
        compressed.extend_from_slice(&zstd_frame(
            &body[body.iter().position(|byte| *byte == b'\n').unwrap() + 1..],
        ));

        let log = read(Path::new("session.jsonl.zstd"), &compressed).unwrap();
        assert_eq!(log.events.len(), 5);
        assert_eq!(
            log.events
                .iter()
                .filter(
                    |record| record.get("type").and_then(Value::as_str) == Some("assistant/chunk")
                )
                .count(),
            3
        );
    }

    #[test]
    fn relocation_preserves_compressed_event_frames_and_plain_event_bytes() {
        let header = header("session-1", "/old");
        let records = [event(
            "turn/end",
            0,
            1,
            json!({"turn": 1, "reason": {"kind": "completed"}}),
        )];
        let plain = jsonl(&header, &records);
        let relocated_plain =
            relocate(Path::new("session.jsonl"), &plain, Path::new("/new")).unwrap();
        let first_newline = relocated_plain
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap();
        assert_eq!(
            &relocated_plain[first_newline + 1..],
            &plain[serde_json::to_vec(&header).unwrap().len() + 1..]
        );

        let header_bytes = jsonl(&header, &[]);
        let body = jsonl(&json!({"ignored": true}), &records);
        let mut compressed = zstd_frame(&header_bytes);
        compressed.extend_from_slice(&zstd_frame(
            &body[body.iter().position(|byte| *byte == b'\n').unwrap() + 1..],
        ));
        let frames = scan_zstd_frames(&compressed).unwrap();
        let relocated = relocate(
            Path::new("session.jsonl.zstd"),
            &compressed,
            Path::new("/new"),
        )
        .unwrap();
        assert_eq!(&relocated[frames[0].1..], &compressed[frames[0].1..]);
        let parsed = read(Path::new("session.jsonl.zstd"), &relocated).unwrap();
        assert_eq!(
            parsed.header.get("cwd").and_then(Value::as_str),
            Some("/new")
        );
        assert_eq!(parsed.events, records);
    }

    #[test]
    fn rejects_blank_rows_malformed_packed_rows_and_future_generations() {
        assert!(is_session_log(Path::new("session.v2.jsonl.zstd")));
        assert!(!is_session_log(Path::new("session.v0.jsonl.zstd")));
        let header = header("session-1", "/work");
        let blank = [serde_json::to_vec(&header).unwrap(), b"\n\n".to_vec()].concat();
        assert!(read(Path::new("session.jsonl"), &blank).is_err());
        let malformed = jsonl(
            &header,
            &[json!({
                "type": "text-chunks",
                "seq0": 0,
                "time0": 1,
                "data": {"turn": 1, "step": 1, "index": 0, "dt": [], "texts": ["a"], "extra": true},
            })],
        );
        assert!(read(Path::new("session.jsonl"), &malformed).is_err());
        assert!(relocate(Path::new("session.v2.jsonl"), &[], Path::new("/new")).is_err());
    }
}
