//! Read-only follower for Antigravity's native task state.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

// Conversation DB WAL checkpoint/close has been observed taking about 570 ms.
const ANTIGRAVITY_SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(2_000);

pub fn is_sqlite_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(err, _)) if matches!(err.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityRunStatus {
    Unspecified,
    Idle,
    Running,
    Canceling,
    Busy,
}

impl AntigravityRunStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Canceling | Self::Busy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravitySummarySnapshot {
    pub status: AntigravityRunStatus,
    pub not_fully_idle: bool,
    pub killed: bool,
    pub step_count: i64,
    pub background_task_count: u64,
    pub has_running_subagents: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityBackgroundTask {
    pub step_index: i64,
    pub id: String,
    pub command: String,
    pub started_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravitySnapshot {
    pub summary: Option<AntigravitySummarySnapshot>,
    pub conversation_db_present: bool,
    pub summary_commit_changed: bool,
    pub summary_row_digest: Option<[u8; 32]>,
    pub running_tasks: Vec<AntigravityBackgroundTask>,
}

#[derive(Debug, Clone, Default)]
pub struct AntigravityRefreshCursor {
    task_data_version: Option<i64>,
    running_tasks: Vec<AntigravityBackgroundTask>,
    data_version: Option<i64>,
    summary: Option<(AntigravitySummarySnapshot, [u8; 32])>,
}

#[derive(Debug)]
pub struct AntigravityStateFollower {
    home: PathBuf,
    conversation_id: String,
    tasks: Option<Connection>,
    tasks_identity: Option<(u64, u64)>,
    summary: Option<Connection>,
    summary_identity: Option<(u64, u64)>,
    cursor: AntigravityRefreshCursor,
    #[cfg(test)]
    busy_timeout: Duration,
}

struct SummaryRow {
    status: String,
    not_fully_idle: Value,
    killed: Value,
    step_count: i64,
    last_modified_time: Value,
    raw_summary: Option<Vec<u8>>,
}

impl SummaryRow {
    fn digest(&self) -> [u8; 32] {
        fn write_bytes(hash: &mut Sha256, bytes: &[u8]) {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }

        fn write_value(hash: &mut Sha256, value: &Value) {
            match value {
                Value::Null => hash.update([0]),
                Value::Integer(n) => {
                    hash.update([1]);
                    hash.update(n.to_le_bytes());
                }
                Value::Real(n) => {
                    hash.update([2]);
                    hash.update(n.to_le_bytes());
                }
                Value::Text(s) => {
                    hash.update([3]);
                    write_bytes(hash, s.as_bytes());
                }
                Value::Blob(bytes) => {
                    hash.update([4]);
                    write_bytes(hash, bytes);
                }
            }
        }

        let mut hash = Sha256::new();
        write_bytes(&mut hash, self.status.as_bytes());
        write_value(&mut hash, &self.not_fully_idle);
        write_value(&mut hash, &self.killed);
        hash.update(self.step_count.to_le_bytes());
        write_value(&mut hash, &self.last_modified_time);
        hash.update([u8::from(self.raw_summary.is_some())]);
        if let Some(bytes) = &self.raw_summary {
            write_bytes(&mut hash, bytes);
        }
        hash.finalize().into()
    }

    fn snapshot(&self) -> Result<AntigravitySummarySnapshot> {
        let mut summary = AntigravitySummarySnapshot {
            status: status_from_text(&self.status),
            not_fully_idle: value_bool(&self.not_fully_idle)?,
            killed: value_bool(&self.killed)?,
            step_count: self.step_count,
            background_task_count: 0,
            has_running_subagents: false,
        };
        if let Some(raw) = &self.raw_summary {
            decode_summary(raw, &mut summary)?;
        }
        Ok(summary)
    }
}

impl AntigravityStateFollower {
    pub fn new(home: PathBuf, conversation_id: String) -> Self {
        Self {
            home,
            conversation_id,
            tasks: None,
            tasks_identity: None,
            summary: None,
            summary_identity: None,
            cursor: AntigravityRefreshCursor::default(),
            #[cfg(test)]
            busy_timeout: ANTIGRAVITY_SQLITE_BUSY_TIMEOUT,
        }
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
        if let Some(tasks) = &self.tasks {
            tasks
                .busy_timeout(timeout)
                .expect("configure test task busy timeout");
        }
        if let Some(summary) = &self.summary {
            summary
                .busy_timeout(timeout)
                .expect("configure test summary busy timeout");
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn refresh(&mut self) -> Result<AntigravitySnapshot> {
        let (snapshot, cursor) = self.refresh_pending()?;
        self.commit_refresh(cursor);
        Ok(snapshot)
    }

    /// Read the current database state without advancing the committed summary
    /// or task cursor. Call [`Self::commit_refresh`] only after the relay accepts the
    /// snapshot so a rejected concurrent scan cannot drop a real commit.
    pub fn refresh_pending(&mut self) -> Result<(AntigravitySnapshot, AntigravityRefreshCursor)> {
        validate_conversation_id(&self.conversation_id)?;
        #[cfg(test)]
        let timeout = self.busy_timeout;
        #[cfg(not(test))]
        let timeout = ANTIGRAVITY_SQLITE_BUSY_TIMEOUT;
        let dir = self.home.join("antigravity-cli");
        // Do not consume a summary commit if the steps read fails and the
        // relay cannot reconcile the resulting snapshot.
        let task_path = dir
            .join("conversations")
            .join(format!("{}.db", self.conversation_id));
        let conversation_db_present = task_path
            .try_exists()
            .context("locate Antigravity steps database")?;
        if conversation_db_present {
            let meta = fs::metadata(&task_path).context("stat Antigravity steps database")?;
            #[cfg(unix)]
            let identity = (meta.dev(), meta.ino());
            #[cfg(not(unix))]
            let identity = (0, meta.len());
            if self.tasks_identity != Some(identity) || self.tasks.is_none() {
                self.tasks = Some(open_readonly_with_timeout(&task_path, timeout)?);
                self.tasks_identity = Some(identity);
                self.cursor.task_data_version = None;
            }
        } else {
            self.tasks = None;
            self.tasks_identity = None;
            self.cursor.task_data_version = None;
        }
        let (task_data_version, running_tasks) = if let Some(conn) = &self.tasks {
            let version: i64 = conn
                .query_row("PRAGMA data_version", [], |r| r.get(0))
                .context("read Antigravity steps data_version")?;
            let tasks = if self.cursor.task_data_version == Some(version) {
                self.cursor.running_tasks.clone()
            } else {
                read_tasks_conn(conn)?
            };
            (Some(version), tasks)
        } else {
            (None, Vec::new())
        };
        let summary_path = summary_db_path(&self.home);
        if summary_path
            .try_exists()
            .context("locate Antigravity summary database")?
        {
            let meta = fs::metadata(&summary_path).context("stat Antigravity summary database")?;
            #[cfg(unix)]
            let identity = (meta.dev(), meta.ino());
            #[cfg(not(unix))]
            let identity = (0, meta.len());
            if self.summary_identity != Some(identity) || self.summary.is_none() {
                self.summary = Some(open_readonly_with_timeout(&summary_path, timeout)?);
                self.summary_identity = Some(identity);
                self.cursor.data_version = None;
            }
        } else {
            self.summary = None;
            self.summary_identity = None;
        }
        let mut cursor = if let Some(conn) = &self.summary {
            let version: i64 = conn
                .query_row("PRAGMA data_version", [], |r| r.get(0))
                .context("read Antigravity summary data_version")?;
            if self.cursor.data_version == Some(version) {
                self.cursor.clone()
            } else {
                let summary = read_summary_row(conn, &self.conversation_id)?
                    .map(|row| Ok::<_, anyhow::Error>((row.snapshot()?, row.digest())))
                    .transpose()?;
                AntigravityRefreshCursor {
                    task_data_version: None,
                    running_tasks: Vec::new(),
                    data_version: Some(version),
                    summary,
                }
            }
        } else {
            AntigravityRefreshCursor::default()
        };
        cursor.task_data_version = task_data_version;
        cursor.running_tasks = running_tasks.clone();
        // data_version covers the entire database. A child conversation
        // can commit without changing any field in the parent's row.
        let summary_row_digest = cursor.summary.as_ref().map(|(_, digest)| *digest);
        let summary_commit_changed = summary_row_digest.is_some()
            && summary_row_digest != self.cursor.summary.as_ref().map(|(_, digest)| *digest);
        Ok((
            AntigravitySnapshot {
                summary: cursor.summary.as_ref().map(|(summary, _)| summary.clone()),
                conversation_db_present,
                summary_commit_changed,
                summary_row_digest,
                running_tasks,
            },
            cursor,
        ))
    }

    pub fn commit_refresh(&mut self, cursor: AntigravityRefreshCursor) {
        self.cursor = cursor;
    }
}

fn validate_conversation_id(conversation_id: &str) -> Result<()> {
    ensure!(
        !conversation_id.is_empty()
            && !conversation_id.contains('/')
            && !conversation_id.contains('\\')
            && !conversation_id.contains(".."),
        "invalid Antigravity conversation ID"
    );
    Ok(())
}

fn summary_db_path(home: &Path) -> PathBuf {
    home.join("antigravity-cli")
        .join("conversation_summaries.db")
}

/// Sample the committed row while Antigravity is waiting for its Stop hook.
#[cfg(unix)]
pub fn read_summary_row_digest(home: &Path, conversation_id: &str) -> Result<Option<[u8; 32]>> {
    validate_conversation_id(conversation_id)?;
    let path = summary_db_path(home);
    if !path
        .try_exists()
        .context("locate Antigravity summary database")?
    {
        return Ok(None);
    }
    let conn = open_readonly(&path)?;
    Ok(read_summary_row(&conn, conversation_id)?.map(|row| row.digest()))
}

fn read_summary_row(conn: &Connection, conversation_id: &str) -> Result<Option<SummaryRow>> {
    conn.query_row(
        "SELECT status, not_fully_idle, killed, step_count, last_modified_time, raw_summary
         FROM conversation_summaries WHERE conversation_id = ?1",
        [conversation_id],
        |r| {
            Ok(SummaryRow {
                status: r.get(0)?,
                not_fully_idle: r.get(1)?,
                killed: r.get(2)?,
                step_count: r.get(3)?,
                last_modified_time: r.get(4)?,
                raw_summary: r.get(5)?,
            })
        },
    )
    .optional()
    .context("query Antigravity conversation_summaries")
}

#[cfg(any(unix, test))]
fn open_readonly(path: &Path) -> Result<Connection> {
    open_readonly_with_timeout(path, ANTIGRAVITY_SQLITE_BUSY_TIMEOUT)
}

fn open_readonly_with_timeout(path: &Path, timeout: Duration) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open Antigravity database {}", path.display()))?;
    conn.busy_timeout(timeout)?;
    Ok(conn)
}
fn status_from_text(s: &str) -> AntigravityRunStatus {
    match s {
        "CASCADE_RUN_STATUS_IDLE" => AntigravityRunStatus::Idle,
        "CASCADE_RUN_STATUS_RUNNING" => AntigravityRunStatus::Running,
        "CASCADE_RUN_STATUS_CANCELING" => AntigravityRunStatus::Canceling,
        "CASCADE_RUN_STATUS_BUSY" => AntigravityRunStatus::Busy,
        _ => AntigravityRunStatus::Unspecified,
    }
}
fn value_bool(v: &Value) -> Result<bool> {
    use rusqlite::types::Value::*;
    match v {
        Integer(n) => Ok(*n != 0),
        Real(n) => Ok(*n != 0.0),
        Text(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" | "" => Ok(false),
            _ => bail!("invalid Antigravity boolean {s}"),
        },
        Null => Ok(false),
        Blob(_) => bail!("invalid blob Antigravity boolean"),
    }
}

fn read_varint(b: &[u8], p: &mut usize) -> Result<u64> {
    let mut n = 0u64;
    for shift in (0..70).step_by(7) {
        ensure!(*p < b.len(), "truncated protobuf varint");
        let byte = b[*p];
        *p += 1;
        ensure!(shift < 63 || byte <= 1, "protobuf varint overflow");
        n |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(n);
        }
    }
    bail!("invalid protobuf varint")
}
fn fields(b: &[u8], mut f: impl FnMut(u32, u8, u64, Option<&[u8]>) -> Result<()>) -> Result<()> {
    let mut p = 0;
    while p < b.len() {
        let key = read_varint(b, &mut p)?;
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        ensure!(field != 0, "protobuf field zero");
        let (v, bytes) = match wire {
            0 => (read_varint(b, &mut p)?, None),
            1 => {
                ensure!(b.len() - p >= 8, "truncated fixed64");
                let x = u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                (x, None)
            }
            2 => {
                let n =
                    usize::try_from(read_varint(b, &mut p)?).context("protobuf length overflow")?;
                ensure!(n <= b.len() - p, "truncated protobuf field");
                let x = &b[p..p + n];
                p += n;
                (0, Some(x))
            }
            5 => {
                ensure!(b.len() - p >= 4, "truncated fixed32");
                let x = u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as u64;
                p += 4;
                (x, None)
            }
            _ => bail!("unsupported protobuf wire type {wire}"),
        };
        f(field, wire, v, bytes)?;
    }
    Ok(())
}
fn decode_summary(bytes: &[u8], s: &mut AntigravitySummarySnapshot) -> Result<()> {
    fields(bytes, |n, w, v, _| {
        match n {
            5 => {
                ensure!(w == 0, "invalid summary status wire type");
                s.status = match v {
                    1 => AntigravityRunStatus::Idle,
                    2 => AntigravityRunStatus::Running,
                    3 => AntigravityRunStatus::Canceling,
                    4 => AntigravityRunStatus::Busy,
                    _ => AntigravityRunStatus::Unspecified,
                };
            }
            21 => {
                ensure!(w == 0, "invalid summary idle wire type");
                s.not_fully_idle = v != 0;
            }
            29 => {
                ensure!(w == 0, "invalid summary task count wire type");
                s.background_task_count = v;
            }
            30 => {
                ensure!(w == 0, "invalid summary subagent wire type");
                s.has_running_subagents = v != 0;
            }
            _ => (),
        }
        Ok(())
    })
}
#[cfg(test)]
fn read_tasks(path: &Path) -> Result<Vec<AntigravityBackgroundTask>> {
    read_tasks_conn(&open_readonly(path)?)
}
fn read_tasks_conn(conn: &Connection) -> Result<Vec<AntigravityBackgroundTask>> {
    let mut stmt=conn.prepare("SELECT idx, task_details FROM steps WHERE status = 2 AND task_details IS NOT NULL ORDER BY idx ASC").context("prepare Antigravity steps query")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .context("query Antigravity steps")?;
    let mut tasks = Vec::new();
    for row in rows {
        let (idx, b) = row?;
        if let Some(t) = decode_task(idx, &b)? {
            tasks.push(t)
        }
    }
    Ok(tasks)
}
fn decode_task(idx: i64, b: &[u8]) -> Result<Option<AntigravityBackgroundTask>> {
    let (mut id, mut title, mut description, mut daemon, mut timestamp) =
        (String::new(), String::new(), String::new(), false, None);
    fields(b, |n, w, v, bytes| {
        match n {
            1 | 4 | 7 => {
                ensure!(w == 2, "invalid TaskDetails string wire type");
                let s = std::str::from_utf8(bytes.unwrap())
                    .context("invalid TaskDetails UTF-8")?
                    .to_owned();
                match n {
                    1 => id = s,
                    4 => description = s,
                    7 => title = s,
                    _ => (),
                }
            }
            2 => ensure!(w == 2, "invalid TaskDetails log URI wire type"),
            6 => {
                ensure!(w == 0, "invalid daemon wire type");
                daemon = v != 0;
            }
            8 => {
                ensure!(w == 2, "invalid timestamp wire type");
                timestamp = Some(decode_timestamp(bytes.unwrap())?);
            }
            _ => (),
        }
        Ok(())
    })?;
    if daemon {
        return Ok(None);
    }
    let command = [&title, &description, &id]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("step-{idx}"));
    Ok(Some(AntigravityBackgroundTask {
        step_index: idx,
        id: if id.is_empty() {
            format!("step-{idx}")
        } else {
            id
        },
        command,
        started_at_ms: timestamp,
    }))
}

fn decode_timestamp(b: &[u8]) -> Result<i64> {
    let (mut sec, mut nanos) = (0i64, 0i64);
    fields(b, |n, w, v, _| {
        match n {
            1 => {
                ensure!(w == 0, "invalid timestamp seconds wire type");
                sec = v as i64
            }
            2 => {
                ensure!(w == 0, "invalid timestamp nanos wire type");
                nanos = v as i64
            }
            _ => (),
        }
        Ok(())
    })?;
    sec.checked_mul(1000)
        .and_then(|x| x.checked_add(nanos / 1_000_000))
        .context("timestamp overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn varint(mut n: u64, out: &mut Vec<u8>) {
        while n >= 128 {
            out.push((n as u8) | 128);
            n >>= 7;
        }
        out.push(n as u8);
    }
    fn uint(field: u64, n: u64, out: &mut Vec<u8>) {
        varint(field << 3, out);
        varint(n, out);
    }
    fn bytes(field: u64, b: &[u8], out: &mut Vec<u8>) {
        varint((field << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }
    fn summary(status: u64, idle: u64) -> Vec<u8> {
        let mut b = Vec::new();
        uint(5, status, &mut b);
        uint(21, idle, &mut b);
        uint(29, 7, &mut b);
        uint(30, 1, &mut b);
        b
    }
    fn dirs(root: &Path) -> PathBuf {
        let d = root.join("antigravity-cli");
        fs::create_dir_all(d.join("conversations")).unwrap();
        d
    }

    #[test]
    fn antigravity_task_data_version_tracks_wal_commits_and_rejected_cursors() {
        let temp = tempfile::tempdir().unwrap();
        let path = dirs(temp.path()).join("conversations/c1.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE steps (idx INTEGER,status INTEGER,task_details BLOB);").unwrap();
        let mut follower = AntigravityStateFollower::new(temp.path().into(), "c1".into());
        follower.refresh().unwrap();
        let initial = follower.cursor.task_data_version;
        assert!(follower.refresh().unwrap().running_tasks.is_empty());
        assert_eq!(initial, follower.cursor.task_data_version);
        writer
            .execute(
                "INSERT INTO steps VALUES (1,2,?1)",
                [b"\x0a\x04task".as_slice()],
            )
            .unwrap();
        let (snapshot, rejected) = follower.refresh_pending().unwrap();
        assert_eq!(snapshot.running_tasks.len(), 1);
        assert_ne!(initial, rejected.task_data_version);
        assert_eq!(initial, follower.cursor.task_data_version);
        // A rejected read must not acknowledge its data_version.
        let (snapshot, accepted) = follower.refresh_pending().unwrap();
        assert_eq!(snapshot.running_tasks.len(), 1);
        assert_eq!(rejected.task_data_version, accepted.task_data_version);
        follower.commit_refresh(accepted);
        drop(writer);
        fs::rename(&path, path.with_extension("old")).unwrap();
        let replacement = Connection::open(&path).unwrap();
        replacement
            .execute_batch("CREATE TABLE steps (idx INTEGER,status INTEGER,task_details BLOB);")
            .unwrap();
        assert!(follower.refresh().unwrap().running_tasks.is_empty());
        drop(replacement);
        fs::remove_file(&path).unwrap();
        assert!(!follower.refresh().unwrap().conversation_db_present);
        assert!(follower.cursor.task_data_version.is_none());
    }

    #[test]
    fn antigravity_short_exclusive_lock_clears_inside_busy_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let path = dirs(temp.path()).join("conversations/c1.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE steps (idx INTEGER,status INTEGER,task_details BLOB); BEGIN EXCLUSIVE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            writer.execute_batch("ROLLBACK").unwrap();
        });
        let mut follower = AntigravityStateFollower::new(temp.path().into(), "c1".into());
        assert!(follower.refresh_pending().is_ok());
        release.join().unwrap();
    }

    #[test]
    fn antigravity_wal_data_version_only_advances_summary_for_the_parent_row() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        let path = d.join("conversation_summaries.db");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY, status TEXT, not_fully_idle, killed,
                step_count INTEGER, last_modified_time, raw_summary BLOB
            );",
            )
            .unwrap();
        let mut follower = AntigravityStateFollower::new(tmp.path().to_owned(), "c1".into());
        writer
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1,?2,?3,0,9,1,?4)",
                params!["c1", "CASCADE_RUN_STATUS_RUNNING", 1, summary(2, 1)],
            )
            .unwrap();
        let a = follower.refresh().unwrap();
        assert!(a.summary_commit_changed);
        let s = a.summary.unwrap();
        assert_eq!(s.status, AntigravityRunStatus::Running);
        assert!(s.not_fully_idle);
        assert_eq!(s.background_task_count, 7);
        assert!(s.has_running_subagents);
        assert!(!follower.refresh().unwrap().summary_commit_changed);
        let version = follower.cursor.data_version.unwrap();
        writer
            .execute(
                "INSERT INTO conversation_summaries
                 SELECT 'subagent', status, not_fully_idle, killed, step_count, last_modified_time, raw_summary
                 FROM conversation_summaries WHERE conversation_id='c1'",
                [],
            )
            .unwrap();
        let child_commit = follower.refresh().unwrap();
        assert_ne!(follower.cursor.data_version.unwrap(), version);
        assert!(!child_commit.summary_commit_changed);

        writer
            .execute(
                "UPDATE conversation_summaries SET last_modified_time=2 WHERE conversation_id='c1'",
                [],
            )
            .unwrap();
        let timestamp_commit = follower.refresh().unwrap();
        assert!(timestamp_commit.summary_commit_changed);
        assert_eq!(timestamp_commit.summary, child_commit.summary);
        writer.execute("UPDATE conversation_summaries SET status=?2,not_fully_idle=0,raw_summary=?3 WHERE conversation_id=?1",params!["c1","CASCADE_RUN_STATUS_IDLE",summary(1,0)]).unwrap();
        let b = follower.refresh().unwrap();
        assert!(b.summary_commit_changed);
        let s = b.summary.unwrap();
        assert_eq!(s.status, AntigravityRunStatus::Idle);
        assert!(!s.not_fully_idle);

        writer
            .execute(
                "DELETE FROM conversation_summaries WHERE conversation_id='c1'",
                [],
            )
            .unwrap();
        let deleted = follower.refresh().unwrap();
        assert!(!deleted.summary_commit_changed);
        assert!(deleted.summary.is_none());
        writer.execute(
            "INSERT INTO conversation_summaries VALUES ('c1','CASCADE_RUN_STATUS_IDLE',0,0,9,2,?1)",
            [summary(1, 0)],
        ).unwrap();
        assert!(follower.refresh().unwrap().summary_commit_changed);
    }

    #[test]
    fn trajectory_tasks_decode_filter_and_order() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dirs(tmp.path());
        let path = d.join("conversations/c1.db");
        let c = Connection::open(&path).unwrap();
        c.execute(
            "CREATE TABLE steps (idx INTEGER,status INTEGER,task_details BLOB)",
            [],
        )
        .unwrap();
        let mut stamp = Vec::new();
        uint(1, 123, &mut stamp);
        uint(2, 456_000_000, &mut stamp);
        let mut finite = Vec::new();
        bytes(1, b"task", &mut finite);
        bytes(4, b"description", &mut finite);
        bytes(7, b"title", &mut finite);
        bytes(8, &stamp, &mut finite);
        let mut daemon = Vec::new();
        uint(6, 1, &mut daemon);
        bytes(1, b"daemon", &mut daemon);
        let mut fallback = Vec::new();
        bytes(1, b"fallback-id", &mut fallback);
        c.execute("INSERT INTO steps VALUES (1,2,?1)", [finite])
            .unwrap();
        c.execute("INSERT INTO steps VALUES (2,2,?1)", [daemon])
            .unwrap();
        c.execute("INSERT INTO steps VALUES (3,3,?1)", [fallback.clone()])
            .unwrap();
        c.execute("INSERT INTO steps VALUES (4,2,?1)", [fallback])
            .unwrap();
        let tasks = read_tasks(&path).unwrap();
        assert_eq!(
            tasks,
            vec![
                AntigravityBackgroundTask {
                    step_index: 1,
                    id: "task".into(),
                    command: "title".into(),
                    started_at_ms: Some(123456)
                },
                AntigravityBackgroundTask {
                    step_index: 4,
                    id: "fallback-id".into(),
                    command: "fallback-id".into(),
                    started_at_ms: None
                }
            ]
        );
    }

    #[test]
    fn missing_and_broken_databases_and_invalid_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = AntigravityStateFollower::new(tmp.path().into(), "c1".into());
        let snap = f.refresh().unwrap();
        assert!(snap.summary.is_none());
        assert!(snap.running_tasks.is_empty());
        let d = dirs(tmp.path());
        Connection::open(d.join("conversation_summaries.db"))
            .unwrap()
            .execute(
                "CREATE TABLE conversation_summaries (conversation_id TEXT)",
                [],
            )
            .unwrap();
        assert!(f.refresh().is_err());
        for id in ["../x", "", "a/b"] {
            let mut bad = AntigravityStateFollower::new(tmp.path().into(), id.into());
            assert!(bad.refresh().is_err());
        }
    }
}
