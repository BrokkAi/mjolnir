//! A separate-process SQLite WAL read pin.
//!
//! SQLite holds POSIX `fcntl` byte-range locks, and POSIX drops every lock a
//! process holds on a file when that process closes *any* descriptor for it.
//! A process that holds a WAL read transaction and also reads the database file
//! with ordinary file I/O therefore loses its read mark without noticing: the
//! daemon's last connection can then checkpoint and delete the WAL, and the
//! pinned snapshot silently vanishes.
//!
//! The structure that makes this impossible: [`run_pin_helper`] runs in its own
//! OS process (`mj migrate-pin`) and touches the database only through
//! `rusqlite`. The sender process reads raw bytes and never holds a SQLite
//! connection that must survive those reads. The kernel isolates the two.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_core::subprocess::SupervisedPipeChild;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// First token of the line [`run_pin_helper`] prints when the pin is held.
/// The sender looks for it anywhere in a line and ignores other output.
const READY_PREFIX: &str = "MJ-PIN-READY ";
const RELEASE: &str = "RELEASE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinReady {
    pub user_version: i64,
    pub journal_mode: String,
}

/// Hold a WAL read transaction on `db_path` until `RELEASE` or end of input.
///
/// This function must run in a process that does nothing else with `db_path`:
/// it never opens the file directly, only through SQLite.
pub fn run_pin_helper(db_path: &Path, stdin: impl BufRead, mut stdout: impl Write) -> Result<()> {
    // Best effort: start from an empty WAL so the pin generation is short.
    // A busy database (a live daemon) is the normal case and is not an error.
    match checkpoint_truncate_briefly(db_path) {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "pin helper skipped the initial WAL checkpoint"),
    }

    let pin = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", db_path.display()))?;
    pin.busy_timeout(std::time::Duration::from_secs(5))?;
    // The read mark is taken by the first read inside the transaction.
    pin.execute_batch("BEGIN DEFERRED")?;
    pin.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(other),
        })
        .context("take the WAL read mark")?;
    let journal_mode: String = pin.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    ensure!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "{} is in journal mode {journal_mode:?}, not wal; it cannot be pinned",
        db_path.display()
    );
    let user_version: i64 = pin.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let ready = PinReady {
        user_version,
        journal_mode,
    };
    writeln!(stdout, "{READY_PREFIX}{}", serde_json::to_string(&ready)?)?;
    stdout.flush()?;

    for line in stdin.lines() {
        if line?.trim() == RELEASE {
            break;
        }
    }
    // Rolling back ends the read transaction cleanly; end of input does too.
    pin.execute_batch("ROLLBACK")?;
    drop(pin);
    Ok(())
}

fn checkpoint_truncate_briefly(db_path: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_millis(200))?;
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(())
}

/// What the sender sees of `<db>-wal`. Inspected by the sender process, never
/// by [`run_pin_helper`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalIdentity {
    pub exists: bool,
    pub inode: Option<u64>,
    pub header: Option<[u8; 32]>,
    pub size: u64,
}

const WAL_MAGIC_BIG_ENDIAN_CHECKSUM: u32 = 0x377f_0682;
const WAL_MAGIC_LITTLE_ENDIAN_CHECKSUM: u32 = 0x377f_0683;

pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

impl WalIdentity {
    pub fn inspect(db_path: &Path) -> Result<Self> {
        use std::io::Read;
        let path = wal_path(db_path);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    exists: false,
                    inode: None,
                    header: None,
                    size: 0,
                });
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        let size = metadata.len();
        let header = if size >= 32 {
            let mut header = [0_u8; 32];
            // The WAL file, not the database: reading it never disturbs the pin.
            std::fs::File::open(&path)
                .and_then(|mut file| file.read_exact(&mut header))
                .with_context(|| format!("read WAL header {}", path.display()))?;
            Some(header)
        } else {
            None
        };
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            Some(metadata.ino())
        };
        #[cfg(not(unix))]
        let inode = None;
        Ok(Self {
            exists: true,
            inode,
            header,
            size,
        })
    }

    fn has_valid_header(&self) -> bool {
        let Some(header) = &self.header else {
            return false;
        };
        let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let salts_set = header[16..24].iter().any(|byte| *byte != 0);
        (magic == WAL_MAGIC_BIG_ENDIAN_CHECKSUM || magic == WAL_MAGIC_LITTLE_ENDIAN_CHECKSUM)
            && salts_set
    }

    /// Whether `after` is the same WAL generation as `self`, or its first one.
    ///
    /// With a pin held a WAL cannot be reset, so an existing WAL keeps its
    /// inode and 32-byte header (magic, version, page size, checkpoint
    /// sequence, salts) and only grows. A WAL that was absent or empty at pin
    /// time may gain its first header. Anything else means the pin lost its
    /// hold and a copied base plus WAL would not reproduce the source.
    pub fn is_valid_continuation(&self, after: &WalIdentity) -> bool {
        if !self.exists || self.size < 32 {
            return !after.exists || after.size == 0 || after.has_valid_header();
        }
        after.exists
            && after.inode == self.inode
            && after.header == self.header
            && after.size >= self.size
    }
}

/// A running `mj migrate-pin` child holding one database's read mark.
pub struct PinHandle {
    child: SupervisedPipeChild,
    stdin: Option<tokio::process::ChildStdin>,
    /// Held so the helper's stdout pipe stays open until release or drop.
    _stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    ready: PinReady,
}

impl PinHandle {
    /// Start `command` (`mj migrate-pin --db <path>`) and wait until it reports
    /// that the read transaction is open.
    pub async fn spawn(command: &mut tokio::process::Command) -> Result<Self> {
        let mut child = SupervisedPipeChild::spawn(command)?;
        let stdin = child.take_stdin().context("pin helper has no stdin")?;
        let stdout = child.take_stdout().context("pin helper has no stdout")?;
        let mut stdout = tokio::io::BufReader::new(stdout);
        let ready = loop {
            let mut line = String::new();
            match stdout.read_line(&mut line).await? {
                0 => {
                    let status = child.wait().await.ok();
                    bail!(
                        "pin helper exited before holding the database ({status:?}): {}",
                        String::from_utf8_lossy(&child.stderr()).trim()
                    );
                }
                _ => {
                    // The marker may follow other text on its line, such as a test harness's.
                    if let Some((_, json)) = line.split_once(READY_PREFIX) {
                        break serde_json::from_str::<PinReady>(json)
                            .context("parse pin helper readiness")?;
                    }
                }
            }
        };
        Ok(Self {
            child,
            stdin: Some(stdin),
            _stdout: stdout,
            ready,
        })
    }

    pub fn user_version(&self) -> i64 {
        self.ready.user_version
    }

    /// Fail if the helper exited; its read mark is gone with it.
    pub fn ensure_alive(&self) -> Result<()> {
        if let Some(outcome) = self.child.try_exited() {
            bail!(
                "database pin was lost: helper exited ({outcome:?}): {}",
                String::from_utf8_lossy(&self.child.stderr()).trim()
            );
        }
        Ok(())
    }

    /// Ask the helper to end its transaction and wait for a clean exit.
    pub async fn release(mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            stdin.write_all(format!("{RELEASE}\n").as_bytes()).await?;
            stdin.flush().await?;
        }
        let status = self.child.wait().await?;
        ensure!(
            status.success(),
            "pin helper failed ({status}): {}",
            String::from_utf8_lossy(&self.child.stderr()).trim()
        );
        Ok(())
    }
}

/// Separate-process roles for tests: the test binary re-runs itself as a pin
/// helper or a database writer. The kernel isolates POSIX locks per process,
/// which is exactly what these tests need.
#[cfg(all(test, unix))]
pub(crate) mod test_roles {
    use super::*;

    const ROLE: &str = "MJ_MIGRATE_TEST_ROLE";
    const DB: &str = "MJ_MIGRATE_TEST_DB";
    const DIR: &str = "MJ_MIGRATE_TEST_DIR";

    pub(crate) fn command(role: &str, db: &Path, dir: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "migrate::pin::test_roles::child_role_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, role)
            .env(DB, db)
            .env(DIR, dir);
        command
    }

    pub(crate) fn pin_command(db: &Path) -> tokio::process::Command {
        tokio::process::Command::from(command("pin", db, db.parent().unwrap()))
    }

    pub(crate) fn row(id: i64, len: usize) -> Vec<u8> {
        let mut state = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"1").unwrap();
    }

    fn deadline() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    fn wait_for(dir: &Path, name: &str) {
        let limit = deadline();
        while !dir.join(name).exists() {
            assert!(
                std::time::Instant::now() < limit,
                "timed out waiting for {name}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Not a test: the entry point of the child processes. Without a role it
    /// returns at once.
    #[test]
    fn child_role_entry() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        let db = PathBuf::from(std::env::var(DB).unwrap());
        let dir = PathBuf::from(std::env::var(DIR).unwrap());
        match role.as_str() {
            "pin" => {
                run_pin_helper(&db, std::io::stdin().lock(), std::io::stdout().lock()).unwrap();
            }
            "writer" => {
                let connection = Connection::open(&db).unwrap();
                connection
                    .busy_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                connection
                    .execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA synchronous=NORMAL;")
                    .unwrap();
                // An idle reader keeps the pin helper's TRUNCATE from resetting the WAL.
                let reader = Connection::open(&db).unwrap();
                reader
                    .execute_batch("BEGIN; SELECT count(*) FROM scratch;")
                    .unwrap();
                let mut next = 0_i64;
                let insert = |next: &mut i64, count: i64| {
                    let tx = connection.unchecked_transaction().unwrap();
                    for _ in 0..count {
                        tx.execute(
                            "INSERT INTO scratch(id, v) VALUES (?1, ?2)",
                            rusqlite::params![*next, row(*next, 4096)],
                        )
                        .unwrap();
                        *next += 1;
                    }
                    tx.commit().unwrap();
                };
                while std::fs::metadata(wal_path(&db)).map_or(0, |m| m.len()) < 2_400_000 {
                    insert(&mut next, 50);
                }
                touch(&dir, "ready");
                wait_for(&dir, "go");
                reader.execute_batch("COMMIT").unwrap();
                let limit = deadline();
                while !dir.join("stop").exists() {
                    assert!(
                        std::time::Instant::now() < limit,
                        "writer was never told to stop"
                    );
                    insert(&mut next, 25);
                    let backfilled: i64 = connection
                        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(2))
                        .unwrap();
                    if backfilled > 0 {
                        touch(&dir, "checkpointed");
                    }
                }
                std::fs::write(dir.join("result"), next.to_string()).unwrap();
                drop(reader);
                drop(connection);
            }
            other => panic!("unknown test role {other}"),
        }
        std::process::exit(0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_roles::{command, pin_command, row};
    use super::*;
    use mj_store::database::{StoreFenceRecord, read_store_fence, set_store_fence_file};
    use std::io::{Read, Seek, SeekFrom, Write};

    /// Kills and reaps the child if the test panics, so a failed assertion
    /// never leaves it running with the test's pipes open.
    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wait_for(dir: &Path, name: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !dir.join(name).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {name}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn pinned_wal_and_a_torn_base_copied_across_processes_recover_exact_state() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let copy = root.path().join("copy");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&copy).unwrap();
        let db = source.join("mj.sqlite3");
        mj_store::database::save_session_to(
            &db,
            &mj_store::database::test_session("one", "bundle"),
        )
        .unwrap();
        let setup = Connection::open(&db).unwrap();
        let mode: String = setup
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        setup
            .execute_batch("CREATE TABLE scratch(id INTEGER PRIMARY KEY, v BLOB NOT NULL)")
            .unwrap();
        drop(setup);

        // A live writer process builds a WAL of more than 2 MiB.
        let mut writer = KillOnDrop(
            command("writer", &db, &source)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for(&source, "ready");
        let pin = PinHandle::spawn(&mut pin_command(&db)).await.unwrap();
        assert_eq!(pin.user_version(), mj_store::database::STORE_SCHEMA_VERSION);
        let initial = WalIdentity::inspect(&db).unwrap();
        assert!(
            initial.exists && initial.size > 2 * 1024 * 1024,
            "{initial:?}"
        );

        // Raw-copy the first half of the database, let the writer backfill
        // pages and keep writing, then copy the rest: a torn base.
        let mut input = std::fs::File::open(&db).unwrap();
        let before = std::fs::read(&db).unwrap();
        let half = before.len() / 2;
        let mut output = std::fs::File::create(copy.join("mj.sqlite3")).unwrap();
        let mut bytes = vec![0_u8; half];
        input.read_exact(&mut bytes).unwrap();
        output.write_all(&bytes).unwrap();
        std::fs::write(source.join("go"), b"1").unwrap();
        wait_for(&source, "checkpointed");
        let after = std::fs::read(&db).unwrap();
        assert_ne!(
            before, after,
            "the writer must have changed the database file under the copy"
        );
        let mut rest = Vec::new();
        input.seek(SeekFrom::Start(half as u64)).unwrap();
        input.read_to_end(&mut rest).unwrap();
        output.write_all(&rest).unwrap();
        output.sync_all().unwrap();
        drop(input);

        std::fs::write(source.join("stop"), b"1").unwrap();
        assert!(writer.0.wait().unwrap().success());
        let total: i64 = std::fs::read_to_string(source.join("result"))
            .unwrap()
            .parse()
            .unwrap();

        // The fence, written by this process while the pin helper holds its mark.
        let fence = StoreFenceRecord {
            destination: "b".into(),
            fenced_at: "now".into(),
            fenced_host: "a".into(),
            fenced_data_dir: source.clone(),
            note: None,
        };
        set_store_fence_file(&db, &fence).unwrap();
        pin.ensure_alive().unwrap();
        let current = WalIdentity::inspect(&db).unwrap();
        assert!(
            initial.is_valid_continuation(&current),
            "{initial:?} {current:?}"
        );
        assert!(current.size >= initial.size);

        // Only the complete WAL goes with the torn base, never the -shm.
        std::fs::copy(wal_path(&db), wal_path(&copy.join("mj.sqlite3"))).unwrap();
        pin.release().await.unwrap();
        assert!(!copy.join("mj.sqlite3-shm").exists());

        let recovered = Connection::open(copy.join("mj.sqlite3")).unwrap();
        let (count, bytes): (i64, i64) = recovered
            .query_row("SELECT count(*), sum(length(v)) FROM scratch", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((count, bytes), (total, total * 4096));
        let sample: Vec<u8> = recovered
            .query_row("SELECT v FROM scratch WHERE id = ?1", [total / 2], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sample, row(total / 2, 4096));
        assert_eq!(read_store_fence(&recovered).unwrap(), Some(fence));
        let check: String = recovered
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok");
    }

    #[tokio::test]
    async fn a_killed_pin_helper_is_reported_as_lost() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("db.sqlite3");
        let setup = Connection::open(&db).unwrap();
        setup
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(a); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(setup);
        let mut pin = PinHandle::spawn(&mut pin_command(&db)).await.unwrap();
        pin.ensure_alive().unwrap();
        pin.child.terminate().await.unwrap();
        let error = pin.ensure_alive().unwrap_err().to_string();
        assert!(error.contains("pin was lost"), "{error}");
        assert!(pin.release().await.is_err());
    }

    #[test]
    fn a_reset_wal_is_not_a_continuation_of_the_pinned_one() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("db.sqlite3");
        let absent = WalIdentity::inspect(&db).unwrap();
        assert!(!absent.exists);
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(a BLOB);
                 INSERT INTO t VALUES (zeroblob(5000));",
            )
            .unwrap();
        let pinned = WalIdentity::inspect(&db).unwrap();
        assert!(pinned.size >= 32);
        // A WAL absent or empty at pin time may gain its first valid header.
        assert!(absent.is_valid_continuation(&pinned));
        connection
            .execute_batch("INSERT INTO t VALUES (zeroblob(5000));")
            .unwrap();
        let grown = WalIdentity::inspect(&db).unwrap();
        assert!(pinned.is_valid_continuation(&grown));
        // Reset and rewritten: same inode, new salts.
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let truncated = WalIdentity::inspect(&db).unwrap();
        assert!(!pinned.is_valid_continuation(&truncated));
        connection
            .execute_batch("INSERT INTO t VALUES (zeroblob(5000));")
            .unwrap();
        let rewritten = WalIdentity::inspect(&db).unwrap();
        assert_eq!(rewritten.inode, pinned.inode);
        assert_ne!(rewritten.header, pinned.header);
        assert!(!pinned.is_valid_continuation(&rewritten));
        assert!(!pinned.is_valid_continuation(&absent));
    }
}
