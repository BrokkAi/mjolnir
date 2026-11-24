//! The one owner of a session's message index and character offsets.
//!
//! `read_session` pages and every search hit that cites `(session, i, offset)`
//! must agree character for character, so both take their messages, redaction
//! and slicing from here.

use anyhow::{Context, Result};
use sessionwiki::redact::redact;

/// One message as `read_session` shows it: `i` is its 0-based position in the
/// session's messages ordered by row id, and `text` is already redacted.
pub(super) struct IndexedMessage {
    pub id: i64,
    pub i: usize,
    pub role: &'static str,
    pub text: String,
}

/// A slice of one message's redacted text, in Unicode scalar values.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Excerpt {
    pub text: String,
    /// Where `text` starts in the redacted message.
    pub offset: usize,
    /// Where the next excerpt starts, or `None` when `text` reaches the end.
    pub next_offset: Option<usize>,
    /// Length of the whole redacted message.
    pub text_chars: usize,
}

/// Load a session's messages with the roles and redaction `read_session`
/// serves. Roles other than user and assistant read as `tool`, as in
/// `sessionwiki::index::session_from_index`.
pub(super) fn load(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<IndexedMessage>> {
    let mut statement = connection
        .prepare("SELECT id, role, text FROM messages WHERE session_id = ?1 ORDER BY id ASC")
        .context("prepare the session message query")?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .context("query session messages")?;
    let mut messages = Vec::new();
    for (i, row) in rows.enumerate() {
        let (id, role, raw) = row.context("read a session message")?;
        messages.push(message(id, i, &role, &raw));
    }
    Ok(messages)
}

/// The single place that maps a stored role and raw text to what
/// `read_session` serves.
fn message(id: i64, i: usize, role: &str, raw: &str) -> IndexedMessage {
    IndexedMessage {
        id,
        i,
        role: match role {
            "user" => "user",
            "assistant" => "assistant",
            _ => "tool",
        },
        text: redact(raw).into_owned(),
    }
}

/// Row ids of a session's messages in `i` order, without reading any text.
/// `ids[i]` is the id of message `i`, and the ids ascend, so a binary search
/// turns an id back into `i`.
pub(super) fn load_ids(connection: &rusqlite::Connection, session_id: &str) -> Result<Vec<i64>> {
    let mut statement = connection
        .prepare("SELECT id FROM messages WHERE session_id = ?1 ORDER BY id ASC")
        .context("prepare the session message id query")?;
    statement
        .query_map([session_id], |row| row.get::<_, i64>(0))
        .context("query session message ids")?
        .collect::<rusqlite::Result<_>>()
        .context("read a session message id")
}

/// Only the requested `(row id, i)` messages, keyed by `i`, with the same roles
/// and redaction as [`load`]. An id that no longer exists is absent.
pub(super) fn load_by_ids(
    connection: &rusqlite::Connection,
    ids_with_indices: &[(i64, usize)],
) -> Result<std::collections::HashMap<usize, IndexedMessage>> {
    let mut statement = connection
        .prepare("SELECT role, text FROM messages WHERE id = ?1")
        .context("prepare the message body query")?;
    let mut messages = std::collections::HashMap::with_capacity(ids_with_indices.len());
    for &(id, i) in ids_with_indices {
        let row = statement.query_row([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        match row {
            Ok((role, raw)) => {
                messages.insert(i, message(id, i, &role, &raw));
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error).context("read a session message"),
        }
    }
    Ok(messages)
}

/// At most `take` characters of `text` from character `skip`. A `skip` past
/// the end yields an empty excerpt that still reports the requested offset.
pub(super) fn excerpt(text: &str, skip: usize, take: usize) -> Excerpt {
    let text_chars = text.chars().count();
    let part: String = text.chars().skip(skip).take(take).collect();
    let end = skip + part.chars().count();
    Excerpt {
        text: part,
        offset: skip,
        next_offset: (end < text_chars).then_some(end),
        text_chars,
    }
}
