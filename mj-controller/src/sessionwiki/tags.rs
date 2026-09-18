//! Mjolnir's own metadata on an indexed session, stored as SessionWiki tags.
//!
//! SessionWiki's index is a cache of every tool's sessions plus a few *durable*
//! tables the crate promises never to drop on a schema bump: `tags`, `notes`,
//! `summaries` and `archive` (see `index::open` in the `brokk-sessionwiki`
//! crate). Mjolnir needs a Mjolnir session's target, profile and harness to
//! survive in the index, because the archive job destroys the Mjolnir record
//! and the rows that most need the metadata are exactly the ones with no record
//! left to join against. So the metadata is written into the durable `tags`
//! table, one tag each: `mj-target:<target template id>`,
//! `mj-profile:<harness profile id>` and `mj-harness:<harness kind id>`.
//!
//! Mjolnir writes and reads the table with its own SQL rather than through the
//! crate's `add_tag` and `remove_tag`, for two reasons. The crate normalizes a
//! tag by lowercasing it, and Mjolnir ids may be mixed case, so a round trip
//! through `add_tag` would corrupt an id. And the crate offers no way to find a
//! stale `mj-target:` tag after a Move without reading every tag back.
//!
//! Keeping the SQL here, in one module with a round-trip test against a real
//! index, means an upstream change to the table fails a test rather than a
//! user. Ids contain no commas (`mj_core::config::validate_id` allows only
//! `[A-Za-z0-9._-]`), so the crate's comma-joined `tags` column still parses.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

/// The tag prefix of each field, and how a tag maps back onto the struct.
const TARGET: &str = "mj-target:";
const PROFILE: &str = "mj-profile:";
const HARNESS: &str = "mj-harness:";

/// What Mjolnir knows about one of its own indexed sessions beyond what
/// SessionWiki stores for every tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MjTags {
    /// The target template the session ran on.
    pub target: Option<String>,
    /// The harness profile the session last ran under.
    pub profile: Option<String>,
    /// The harness kind, as `mj_core::config::HarnessKind::id` spells it.
    pub harness: Option<String>,
}

impl MjTags {
    /// Whether there is anything worth storing. A session with no record left
    /// contributes nothing and must not clear what an earlier sync wrote.
    pub fn is_empty(&self) -> bool {
        self.target.is_none() && self.profile.is_none() && self.harness.is_none()
    }

    /// The tags this metadata is stored as.
    fn tags(&self) -> impl Iterator<Item = String> + '_ {
        [
            self.target.as_deref().map(|value| format!("{TARGET}{value}")),
            self.profile
                .as_deref()
                .map(|value| format!("{PROFILE}{value}")),
            self.harness
                .as_deref()
                .map(|value| format!("{HARNESS}{value}")),
        ]
        .into_iter()
        .flatten()
    }

    /// Record one stored tag, ignoring any `mj-` tag this version does not know.
    fn absorb(&mut self, tag: &str) {
        if let Some(value) = tag.strip_prefix(TARGET) {
            self.target = Some(value.to_owned());
        } else if let Some(value) = tag.strip_prefix(PROFILE) {
            self.profile = Some(value.to_owned());
        } else if let Some(value) = tag.strip_prefix(HARNESS) {
            self.harness = Some(value.to_owned());
        }
    }
}

/// Replace one session's Mjolnir tags with the current ones.
///
/// The delete is what keeps a Move or a profile switch from leaving a stale
/// target behind: the record changed, and the next sync writes the new value
/// over the old one. Runs inside the caller's transaction, so a sync pays for
/// one commit rather than one per session.
pub fn write(connection: &rusqlite::Connection, session_id: &str, tags: &MjTags) -> Result<()> {
    connection
        .execute(
            "DELETE FROM tags
             WHERE session_id = ?1
               AND (tag LIKE 'mj-target:%' OR tag LIKE 'mj-profile:%' OR tag LIKE 'mj-harness:%')",
            rusqlite::params![session_id],
        )
        .with_context(|| format!("clear the indexed metadata of session {session_id}"))?;
    let mut insert = connection
        .prepare_cached("INSERT OR IGNORE INTO tags(session_id, tag) VALUES (?1, ?2)")
        .context("prepare the indexed metadata insert")?;
    for tag in tags.tags() {
        insert
            .execute(rusqlite::params![session_id, tag])
            .with_context(|| format!("store {tag:?} on session {session_id}"))?;
    }
    Ok(())
}

/// The Mjolnir metadata of each of `session_ids` that has any, in one query.
pub fn read(
    connection: &rusqlite::Connection,
    session_ids: &[&str],
) -> Result<BTreeMap<String, MjTags>> {
    let mut found: BTreeMap<String, MjTags> = BTreeMap::new();
    if session_ids.is_empty() {
        return Ok(found);
    }
    // SQLite's parameter limit is in the hundreds by default and a search page
    // is capped well below it, but chunking costs nothing and removes the
    // question.
    for chunk in session_ids.chunks(200) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = connection
            .prepare(&format!(
                "SELECT session_id, tag FROM tags
                 WHERE tag LIKE 'mj-%' AND session_id IN ({placeholders})"
            ))
            .context("prepare the indexed metadata query")?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("read the indexed session metadata")?;
        for row in rows {
            let (session_id, tag) = row.context("read one indexed metadata tag")?;
            found.entry(session_id).or_default().absorb(&tag);
        }
    }
    Ok(found)
}

/// Opening a SessionWiki index of this test process's own.
///
/// The location comes from the process-wide `SESSIONWIKI_DATA` variable, which
/// is also what tells Mjolnir it may touch an index at all
/// (`super::index_is_writable`). Every test that points it somewhere has to
/// take the same turn, so the guard lives here and the tests of this module and
/// of its parent share it.
#[cfg(test)]
pub(super) mod testing {
    /// Held for as long as a test relies on `SESSIONWIKI_DATA`.
    pub(crate) static INDEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        INDEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A real SessionWiki index in a temporary directory, opened through the
    /// crate's own `open` so the schema under test is the shipped one. Call it
    /// only while holding [`lock`].
    pub(crate) fn isolated_index() -> (tempfile::TempDir, rusqlite::Connection) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        // SAFETY: the caller holds `lock`, so no other test in this process is
        // reading or writing this variable at the same time.
        unsafe {
            std::env::set_var("SESSIONWIKI_DATA", directory.path());
        }
        let connection = sessionwiki::index::open().expect("open a fresh index");
        (directory, connection)
    }

    /// One row in the index's cache, which is what a query answers from.
    pub(crate) fn index_row(connection: &rusqlite::Connection, session_id: &str, tool: &str) {
        connection
            .execute(
                "INSERT INTO files(path, tool, session_id, project, title, mtime, size, msg_count)
                 VALUES (?1, ?2, ?3, '/src/project', 'an indexed session', 0, 0, 2)",
                rusqlite::params![format!("/checkpoints/{session_id}"), tool, session_id],
            )
            .expect("insert a cached row");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{index_row, isolated_index as index, lock};
    use super::*;

    fn tags(target: &str, profile: &str, harness: &str) -> MjTags {
        MjTags {
            target: Some(target.to_owned()),
            profile: Some(profile.to_owned()),
            harness: Some(harness.to_owned()),
        }
    }

    #[test]
    fn tags_round_trip_through_the_index() {
        let _held = lock();
        let (_directory, connection) = index();
        let written = tags("Prod-Box", "codex-Main", "codex");
        write(&connection, "session-1", &written).expect("write tags");
        write(&connection, "session-2", &MjTags::default()).expect("write no tags");

        let read_back = read(&connection, &["session-1", "session-2", "absent"]).expect("read");
        assert_eq!(read_back.get("session-1"), Some(&written));
        assert_eq!(read_back.get("session-2"), None, "no tag, no entry");
        assert_eq!(read_back.get("absent"), None);
    }

    #[test]
    fn a_changed_target_replaces_the_stale_tag() {
        let _held = lock();
        let (_directory, connection) = index();
        write(&connection, "session-1", &tags("old", "codex", "codex")).expect("write tags");
        write(&connection, "session-1", &tags("new", "claude", "claude")).expect("rewrite tags");

        let read_back = read(&connection, &["session-1"]).expect("read");
        assert_eq!(read_back.get("session-1"), Some(&tags("new", "claude", "claude")));
        let stored: i64 = connection
            .query_row(
                "SELECT count(*) FROM tags WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .expect("count tags");
        assert_eq!(stored, 3, "the stale tags are gone, not merely shadowed");
    }

    /// The point of the raw SQL is that SessionWiki itself sees these tags. If
    /// the crate's own row query stops reporting them, this fails instead of a
    /// user's Archived tab going quiet.
    #[test]
    fn the_crate_reports_the_same_tags_on_its_own_row() {
        let _held = lock();
        let (_directory, connection) = index();
        // A row in the cache for the crate's query to find. `files` is the
        // cache table every row query reads from.
        index_row(&connection, "session-1", "mjolnir");
        let written = tags("Prod-Box", "codex-Main", "codex");
        write(&connection, "session-1", &written).expect("write tags");

        let rows = sessionwiki::index::resolve(&connection, "session-1").expect("resolve");
        let row = rows
            .iter()
            .find(|row| row.session_id == "session-1")
            .expect("the row is indexed");
        let reported: Vec<&str> = row
            .tags
            .as_deref()
            .expect("the crate reports the tags")
            .split(',')
            .collect();
        for tag in written.tags() {
            assert!(
                reported.contains(&tag.as_str()),
                "the crate reports {tag:?}, got {reported:?}"
            );
        }
    }
}
