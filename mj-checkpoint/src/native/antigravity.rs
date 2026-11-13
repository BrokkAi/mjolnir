//! Workspace relocation for Antigravity (`agy`) conversation databases.
//!
//! agy keeps one SQLite database per conversation. The row `main` of its
//! `trajectory_metadata_blob` table is a protobuf message, written when the
//! conversation is created and never rewritten by a later load, that names
//! every workspace root the conversation was given: the launch directory and
//! each additional directory, in alphabetical order with nothing marking the
//! launch directory. Each root appears twice, as the URI inside one repeated
//! message (field 1, whose field 1 is the URI) and as one repeated string of
//! its own (field 7). agy runs commands in the alphabetically first of these
//! roots and the roots of the request that opens the conversation, so a
//! recorded root that is not where the conversation now runs, whether it is
//! gone or is another copy of the tree, points agy's commands at the wrong
//! directory. Relocation records the new launch directory as the only root
//! and copies every other byte of the record as it was. Every request that
//! opens the conversation names its additional directories again, so agy's
//! roots are then the new launch directory and the request's.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};

/// The record's repeated field of root messages, and the field of such a
/// message that holds the URI.
const ROOT_MESSAGE_FIELD: u64 = 1;
const ROOT_MESSAGE_URI_FIELD: u64 = 1;
/// The record's repeated field of root URIs.
const ROOT_FIELD: u64 = 7;

const LENGTH_DELIMITED: u8 = 2;

/// The conversation database with its launch root moved to `target_cwd`.
pub fn relocate(database: &[u8], target_cwd: &Path) -> Result<Vec<u8>> {
    let uri = workspace_uri(target_cwd)?;
    let scratch = tempfile::tempdir().context("create Antigravity relocation directory")?;
    let path = scratch.path().join("conversation.db");
    std::fs::write(&path, database).context("write Antigravity conversation database")?;
    {
        let connection = rusqlite::Connection::open(&path)
            .context("open the checkpointed Antigravity conversation database")?;
        let blob: Vec<u8> = connection
            .query_row(
                "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
                [],
                |row| row.get(0),
            )
            .context("read the Antigravity conversation's workspace record")?;
        let blob = rewrite_workspace_roots(&blob, &uri)?;
        let changed = connection
            .execute(
                "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main'",
                [blob],
            )
            .context("write the Antigravity conversation's workspace record")?;
        ensure!(
            changed == 1,
            "the Antigravity conversation has no single workspace record"
        );
        // Move the update out of the log, which is not carried with the file.
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    }
    std::fs::read(&path).context("read the relocated Antigravity conversation database")
}

/// The `file://` URI agy writes for a workspace directory. agy escapes as Go's
/// `url.URL` does for a path: every byte but ASCII letters and digits,
/// `-_.~` and `$&+,/:;=@` becomes `%XX`. Observed with agy 1.5.8: a session
/// started in a directory named `a'b(c)[d]!e*f~g$h&i+j,k;l=m@n:o{p}q^r|s<t>u"v`w?x y日本`
/// (plus the `%` and `#` of an earlier probe) had `trajectory_metadata_blob`
/// record `%27 %28 %29 %5B %5D %21 %2A %7B %7D %5E %7C %3C %3E %22 %60 %3F
/// %20 %E6%97%A5%E6%9C%AC %25 %23` for those characters and left `~ $ & + , ;
/// = @ :` as they were. The `url` crate leaves `'()[]!*^|` unescaped, so the
/// URI it builds differs from agy's for such a directory, and agy would see
/// the recorded root and the request's own directory as two roots.
fn workspace_uri(directory: &Path) -> Result<String> {
    ensure!(
        directory.is_absolute(),
        "Antigravity workspace {} is not an absolute path",
        directory.display()
    );
    let text = directory
        .to_str()
        .with_context(|| format!("Antigravity workspace {} is not UTF-8", directory.display()))?;
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        match byte {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'$'
            | b'&'
            | b'+'
            | b','
            | b'/'
            | b':'
            | b';'
            | b'='
            | b'@' => uri.push(char::from(byte)),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(uri)
}

/// One field of a protobuf message: where it sits, and its value.
struct Field<'a> {
    number: u64,
    wire_type: u8,
    /// The field exactly as it was encoded, tag included.
    encoded: &'a [u8],
    /// The tag as it was encoded.
    tag: &'a [u8],
    /// A length-delimited field's bytes; empty for any other wire type.
    payload: &'a [u8],
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*at).context("truncated protobuf varint")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("protobuf varint is too long")
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

/// Split a message into its fields. Groups are refused: a field this editor
/// cannot measure cannot be copied byte for byte.
fn fields(message: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut parsed = Vec::new();
    let mut at = 0;
    while at < message.len() {
        let start = at;
        let tag = read_varint(message, &mut at)?;
        let tag_end = at;
        let wire_type = (tag & 7) as u8;
        let mut payload: &[u8] = &[];
        match wire_type {
            0 => {
                read_varint(message, &mut at)?;
            }
            1 => at = at.checked_add(8).context("protobuf field overflows")?,
            LENGTH_DELIMITED => {
                let length = usize::try_from(read_varint(message, &mut at)?)
                    .context("protobuf field is too long")?;
                let end = at.checked_add(length).context("protobuf field overflows")?;
                payload = message
                    .get(at..end)
                    .context("protobuf field runs past its message")?;
                at = end;
            }
            5 => at = at.checked_add(4).context("protobuf field overflows")?,
            other => bail!("protobuf wire type {other} is not supported"),
        }
        ensure!(at <= message.len(), "protobuf field runs past its message");
        parsed.push(Field {
            number: tag >> 3,
            wire_type,
            encoded: &message[start..at],
            tag: &message[start..tag_end],
            payload,
        });
    }
    Ok(parsed)
}

fn require_string(field: &Field<'_>, what: &str) -> Result<()> {
    ensure!(
        field.wire_type == LENGTH_DELIMITED,
        "the Antigravity workspace record's {what} is not a string"
    );
    Ok(())
}

fn length_delimited(tag: &[u8], payload: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(tag);
    write_varint(payload.len() as u64, output);
    output.extend_from_slice(payload);
}

/// The workspace record with `new_uri` as its only root.
///
/// Nothing in the record marks the launch root, and the checkpoint does not
/// record where it was, so no root is kept: a root that exists where the
/// conversation is restored may be the old tree, and agy would run commands
/// in it. The first root message and the first root URI keep their places
/// and every other field of the record is copied as it was. A record that is
/// not of the shape agy writes (every root a file URI, the two lists naming
/// the same roots) is not one this relocation understands, and the restore
/// fails instead of leaving the conversation in a directory nobody chose.
fn rewrite_workspace_roots(blob: &[u8], new_uri: &str) -> Result<Vec<u8>> {
    let top = fields(blob).context("parse the Antigravity workspace record")?;
    let messages = top
        .iter()
        .enumerate()
        .filter(|(_, field)| field.number == ROOT_MESSAGE_FIELD)
        .collect::<Vec<_>>();
    let strings = top
        .iter()
        .enumerate()
        .filter(|(_, field)| field.number == ROOT_FIELD)
        .collect::<Vec<_>>();
    ensure!(
        !messages.is_empty() && !strings.is_empty(),
        "the Antigravity workspace record holds {} root messages and {} root URIs",
        messages.len(),
        strings.len()
    );

    let file_uri = |bytes: &[u8], what: &str| -> Result<String> {
        let uri = std::str::from_utf8(bytes)
            .map_err(|_| anyhow!("the Antigravity workspace record's {what} is not UTF-8"))?;
        ensure!(
            uri.starts_with("file://"),
            "the Antigravity workspace record's {what} {uri:?} is not a file URI"
        );
        Ok(uri.to_owned())
    };
    let mut nested_roots = std::collections::BTreeSet::new();
    let mut message_parts = Vec::new();
    for (index, message) in &messages {
        require_string(message, "root message")?;
        let nested = fields(message.payload).context("parse an Antigravity root message")?;
        let mut uris = nested
            .iter()
            .enumerate()
            .filter(|(_, field)| field.number == ROOT_MESSAGE_URI_FIELD);
        let (nested_index, uri) = uris
            .next()
            .ok_or_else(|| anyhow!("an Antigravity root message has no URI"))?;
        ensure!(
            uris.next().is_none(),
            "an Antigravity root message has more than one URI"
        );
        require_string(uri, "root message URI")?;
        nested_roots.insert(file_uri(uri.payload, "root message URI")?);
        message_parts.push((*index, nested, nested_index));
    }
    let mut roots = std::collections::BTreeSet::new();
    for (_, string) in &strings {
        require_string(string, "root URI")?;
        roots.insert(file_uri(string.payload, "root URI")?);
    }
    ensure!(
        nested_roots == roots,
        "the Antigravity workspace record names two different sets of workspaces ({nested_roots:?} and {roots:?})"
    );

    let (first_message, first_nested, first_nested_index) = &message_parts[0];
    let mut root_message = Vec::new();
    for (index, field) in first_nested.iter().enumerate() {
        if index == *first_nested_index {
            length_delimited(field.tag, new_uri.as_bytes(), &mut root_message);
        } else {
            root_message.extend_from_slice(field.encoded);
        }
    }
    let first_string = strings[0].0;
    let mut output = Vec::with_capacity(blob.len() + new_uri.len());
    for (index, field) in top.iter().enumerate() {
        if index == *first_message {
            length_delimited(field.tag, &root_message, &mut output);
        } else if index == first_string {
            length_delimited(field.tag, new_uri.as_bytes(), &mut output);
        } else if field.number != ROOT_MESSAGE_FIELD && field.number != ROOT_FIELD {
            output.extend_from_slice(field.encoded);
        }
    }
    Ok(output)
}
