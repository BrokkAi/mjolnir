//! Integration tests for the transfer engine over small in-process pipes. The
//! database pins are real helper processes (the test binary re-run), because
//! the pin only works across processes.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use anyhow::{Result, anyhow};
use mj_core::config::{Config, DataLayout, MachineId, SshConnection};
use mj_core::state::{TargetLocator, TargetRuntimeSettings};
use mj_store::database::{
    MigrationRewriteSpec, StoreFenceRecord, clear_store_fence_file, read_store_fence_file,
    save_session_to, set_store_fence_file,
};
use rusqlite::Connection;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::*;
use crate::migrate::pin::test_roles::{pin_command, row};
use crate::migrate::preflight::HomeCopy;
use crate::migrate::protocol::*;
use crate::migrate::rewrite::StagedConfigRewriteSpec;

struct Counting<W> {
    inner: W,
    bytes: Arc<AtomicUsize>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Counting<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(count)) = &poll {
            self.bytes.fetch_add(*count, Ordering::SeqCst);
        }
        poll
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct World {
    _root: tempfile::TempDir,
    a_data: PathBuf,
    a_config: PathBuf,
    a_home: PathBuf,
    a_wiki: PathBuf,
    b_data: PathBuf,
    b_config: PathBuf,
    b_home: PathBuf,
    b_wiki: PathBuf,
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn big(seed: i64, len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    let mut id = seed;
    while bytes.len() < len {
        bytes.extend(row(id, 4096));
        id += 1;
    }
    bytes.truncate(len);
    bytes
}

fn wal_database(path: &Path, rows: i64) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE IF NOT EXISTS scratch(id INTEGER PRIMARY KEY, v BLOB NOT NULL);",
        )
        .unwrap();
    insert_rows(&connection, 0, rows);
    connection
}

fn insert_rows(connection: &Connection, from: i64, count: i64) {
    let tx = connection.unchecked_transaction().unwrap();
    for id in from..from + count {
        tx.execute(
            "INSERT INTO scratch(id, v) VALUES (?1, ?2)",
            rusqlite::params![id, row(id, 4096)],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn count_rows(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM scratch", [], |row| row.get(0))
        .unwrap()
}

fn session() -> mj_core::state::SessionRecord {
    let mut session = mj_store::database::test_session("one", "bundle");
    session.checkpoint = None;
    session.target_template_id = "localhost".into();
    session.target_runtime = Some(Arc::new(TargetRuntimeSettings::from(
        &mj_core::config::TargetTemplate::LocalBare,
    )));
    session
}

impl World {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let world = World {
            a_data: r.join("a/data"),
            a_config: r.join("a/home/.config/mjolnir"),
            a_home: r.join("a/home"),
            a_wiki: r.join("a/wiki/index.db"),
            b_data: r.join("b/data"),
            b_config: r.join("b/home/.config/mjolnir"),
            b_home: r.join("b/home"),
            b_wiki: r.join("b/wiki/index.db"),
            _root: root,
        };
        let db = DataLayout::at(&world.a_data).database();
        let mut one = session();
        one.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: DataLayout::at(&world.a_data).worker_root("one"),
        }));
        one.project_directory = Some(world.a_home.join("project"));
        std::fs::create_dir_all(world.a_home.join("project")).unwrap();
        save_session_to(&db, &one).unwrap();
        let store = Connection::open(&db).unwrap();
        store
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE scratch(id INTEGER PRIMARY KEY, v BLOB NOT NULL);")
            .unwrap();
        drop(store);
        // Three thousand 4 KiB rows: a database of about twelve 1 MiB blocks.
        let scratch = Connection::open(&db).unwrap();
        insert_rows(&scratch, 0, 3000);
        drop(scratch);

        write(&world.a_data.join("instance-identity"), b"identity-a\n");
        write(&world.a_data.join("api-token"), b"token");
        write(
            &world.a_data.join("sessions/one/attachments/big.bin"),
            &big(1, 3 * 1024 * 1024 + 123),
        );
        write(&world.a_data.join("sessions/one/attachments.deleted"), b"");
        write(&world.a_data.join("sessions/one/attachments.lock"), b"lock");
        std::fs::create_dir_all(world.a_data.join("sessions/empty-dir")).unwrap();
        write(
            &world.a_data.join("tool-details/a/b.bin"),
            &big(9, 1_500_000),
        );
        write(&world.a_data.join("tool-details/a/keep.txt"), b"keep");
        write(&world.a_data.join("projects/x/memory/notes.md"), b"notes");
        write(&world.a_data.join("unknown-user-file"), b"mine");
        write(&world.a_data.join("workers/one/state"), b"local worker");
        write(&world.a_data.join("daemon.log"), b"log");

        Config::default()
            .save_to(&world.a_config.join("config.toml"))
            .unwrap();
        write(&world.a_config.join("ec2/x/key"), b"ec2 key");
        write(&world.a_home.join("secrets/actual"), b"credential");
        std::fs::create_dir_all(world.a_config.join("keys")).unwrap();
        symlink(
            world.a_home.join("secrets/actual"),
            world.a_config.join("keys/linked"),
        )
        .unwrap();

        write(
            &world.a_home.join(".codex-personal/auth.json"),
            b"{\"a\":1}",
        );
        symlink(
            "/nonexistent/absolute",
            world.a_home.join(".codex-personal/dangling"),
        )
        .unwrap();
        write(
            &world.a_home.join(".secrets/token-target"),
            b"private token",
        );
        symlink(
            world.a_home.join(".secrets/token-target"),
            world.a_home.join(".secrets/token"),
        )
        .unwrap();
        // OpenCode's live database: its newest rows exist only in the WAL.
        let opencode = wal_database(&world.a_home.join(".codex-personal/opencode.db"), 300);
        std::mem::forget(opencode);

        let wiki = wal_database(&world.a_wiki, 600);
        drop(wiki);
        world
    }

    fn plan(&self) -> SenderPlan {
        SenderPlan {
            data_dir: self.a_data.clone(),
            config_dir: self.a_config.clone(),
            sessionwiki_db: self.a_wiki.clone(),
            home_copies: vec![
                HomeCopy {
                    source: self.a_home.join(".codex-personal"),
                    destination: self.b_home.join(".codex-personal"),
                    private: false,
                },
                HomeCopy {
                    source: self.a_home.join(".secrets/token"),
                    destination: self.b_home.join(".secrets/token"),
                    private: true,
                },
            ],
            dereference_config_paths: [PathBuf::from("keys/linked")].into(),
            include_logs: false,
            migrate_sessionwiki: true,
            warm_base_pass: true,
            pin_command: Box::new(pin_command),
        }
    }

    fn receiver(&self, hook: Option<PublicationHook>) -> ReceiverConfig {
        ReceiverConfig {
            data_dir: self.b_data.clone(),
            config_dir: self.b_config.clone(),
            home_dir: self.b_home.clone(),
            sessionwiki_db: self.b_wiki.clone(),
            start_daemon: None,
            hook,
        }
    }

    fn fence(&self, at: &str) -> StoreFenceRecord {
        StoreFenceRecord {
            destination: "b".into(),
            fenced_at: at.into(),
            fenced_host: "a".into(),
            fenced_data_dir: self.a_data.clone(),
            note: None,
        }
    }
}

struct Hooks {
    a_db: PathBuf,
    fence: StoreFenceRecord,
    world: (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf),
    log: Vec<&'static str>,
}

impl Hooks {
    fn new(world: &World, fence: StoreFenceRecord) -> Self {
        Self {
            a_db: DataLayout::at(&world.a_data).database(),
            fence,
            world: (
                world.a_data.clone(),
                world.a_config.clone(),
                world.a_home.clone(),
                world.b_data.clone(),
                world.b_config.clone(),
                world.b_home.clone(),
            ),
            log: Vec::new(),
        }
    }
}

impl CutoverHooks for Hooks {
    async fn stop_and_lock(&mut self) -> Result<()> {
        self.log.push("stop");
        Ok(())
    }
    async fn recheck(&mut self, pinned_user_version: i64) -> Result<()> {
        assert_eq!(
            pinned_user_version,
            mj_store::database::STORE_SCHEMA_VERSION
        );
        self.log.push("recheck");
        Ok(())
    }
    async fn fence(&mut self) -> Result<StoreFenceRecord> {
        self.log.push("fence");
        set_store_fence_file(&self.a_db, &self.fence)?;
        Ok(self.fence.clone())
    }
    async fn clear_fence(&mut self) -> Result<()> {
        self.log.push("clear");
        clear_store_fence_file(&self.a_db, &self.fence)
    }
    async fn build_commit(
        &mut self,
        fence: &StoreFenceRecord,
        _destination: &HelloAck,
        home_targets: &[HomeTarget],
    ) -> Result<CommitPublication> {
        self.log.push("build");
        let (a_data, a_config, a_home, b_data, b_config, b_home) = self.world.clone();
        let source_ssh = SshConnection {
            host: "source".into(),
            user: None,
            identity_file: None,
            extra_args: vec![],
        };
        Ok(CommitPublication {
            rewrite_spec: MigrationRewriteSpec {
                expected_fence: fence.clone(),
                old_data_root: a_data,
                new_data_root: b_data,
                old_config_root: a_config.clone(),
                new_config_root: b_config.clone(),
                old_home: a_home.clone(),
                new_home: b_home.clone(),
                remote_template_ids: Default::default(),
                source_ssh: source_ssh.clone(),
                source_machine_id: MachineId::new("source").unwrap(),
                source_templates: Default::default(),
                pinned_memory_keys: BTreeMap::new(),
                staged_sessions_dir: PathBuf::new(),
            },
            config_rewrite_spec: StagedConfigRewriteSpec {
                old_config_root: a_config,
                new_config_root: b_config,
                old_home: a_home,
                new_home: b_home,
                source_machine_id: MachineId::new("source").unwrap(),
                source_ssh,
                historical_local_templates: BTreeMap::from([(
                    "localhost".into(),
                    ["local-bare".to_owned()].into(),
                )]),
            },
            home_targets: home_targets.to_vec(),
            migrate_sessionwiki: true,
            overwrite_sessionwiki: false,
            start_daemon: false,
        })
    }
    async fn rollback(&mut self) -> Result<()> {
        self.log.push("rollback");
        Ok(())
    }
    async fn activated(&mut self, _summary: &PublicationSummary) -> Result<()> {
        self.log.push("activated");
        Ok(())
    }
}

fn pipe() -> (
    impl AsyncRead + Unpin,
    Counting<impl AsyncWrite + Unpin>,
    Arc<AtomicUsize>,
    impl AsyncRead + Unpin,
    impl AsyncWrite + Unpin,
) {
    // 64 KiB: the buffer size that deadlocks a sender that writes everything first.
    let (sender_end, receiver_end) = tokio::io::duplex(64 * 1024);
    let (sender_read, sender_write) = tokio::io::split(sender_end);
    let (receiver_read, receiver_write) = tokio::io::split(receiver_end);
    let bytes = Arc::new(AtomicUsize::new(0));
    (
        sender_read,
        Counting {
            inner: sender_write,
            bytes: bytes.clone(),
        },
        bytes,
        receiver_read,
        receiver_write,
    )
}

fn spawn_receiver(
    config: ReceiverConfig,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) -> tokio::task::JoinHandle<Result<()>> {
    mj_core::runtime::spawn(run_receiver(config, reader, writer))
}

#[tokio::test]
async fn migration_resumes_over_small_pipes_and_publishes_one_activation() {
    let world = World::new();
    let a_db = DataLayout::at(&world.a_data).database();
    // Phase 1 only, then the sender goes away: an interrupted pre-sync.
    let (sender_read, sender_write, first_bytes, receiver_read, receiver_write) = pipe();
    let receiver = spawn_receiver(world.receiver(None), receiver_read, receiver_write);
    let mut sender = Sender::new(world.plan(), sender_read, sender_write);
    let phase1 = sender.phase1().await.unwrap();
    drop(phase1);
    drop(sender);
    receiver.await.unwrap().unwrap();
    let first = first_bytes.load(Ordering::SeqCst);
    assert!(first > 15 * 1024 * 1024, "first run sent {first} bytes");
    let stage = DataLayout::at(&world.b_data).migrate_stage();
    assert_eq!(
        std::fs::read(stage.join("raw/data/mj.sqlite3")).unwrap(),
        std::fs::read(&a_db).unwrap(),
        "an idle source copies to an identical base"
    );

    // The source moves on: a little database change, a changed file, a removed
    // file and a new one.
    let scratch = Connection::open(&a_db).unwrap();
    insert_rows(&scratch, 3000, 20);
    drop(scratch);
    write(
        &world.a_data.join("tool-details/a/keep.txt"),
        b"keep, changed",
    );
    std::fs::remove_file(world.a_data.join("tool-details/a/b.bin")).unwrap();
    write(&world.a_data.join("tool-details/new.txt"), b"new");

    let (sender_read, sender_write, second_bytes, receiver_read, receiver_write) = pipe();
    let receiver = spawn_receiver(world.receiver(None), receiver_read, receiver_write);
    let mut hooks = Hooks::new(&world, world.fence("t1"));
    let sender = Sender::new(world.plan(), sender_read, sender_write);
    let mut sender = sender;
    let phase1 = sender.phase1().await.unwrap();
    let resumed = second_bytes.load(Ordering::SeqCst);
    assert!(
        resumed * 2 < first,
        "resuming sent {resumed} bytes against {first} for the first run"
    );
    let outcome = sender.phase2(phase1, &mut hooks).await.unwrap();
    drop(sender);
    receiver.await.unwrap().unwrap();
    assert!(!outcome.daemon_started);
    assert_eq!(
        hooks.log,
        ["stop", "recheck", "fence", "build", "activated"]
    );

    // B: the activated store, rewritten, unfenced and writable.
    let b_db = DataLayout::at(&world.b_data).database();
    assert_eq!(read_store_fence_file(&b_db).unwrap(), None);
    let mut one = session();
    one.id = "two".into();
    save_session_to(&b_db, &one).unwrap();
    let b = Connection::open(&b_db).unwrap();
    let kind: String = b
        .query_row(
            "SELECT kind FROM session_targets WHERE session_id='one'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "ssh-host-bare");
    assert_eq!(count_rows(&b_db), 3020, "the WAL-only rows arrived");
    drop(b);
    assert_eq!(
        read_store_fence_file(&a_db).unwrap(),
        Some(world.fence("t1")),
        "the source stays fenced"
    );
    assert!(!stage.exists(), "staging is removed after activation");
    let data = |name: &str| world.b_data.join(name);
    assert_eq!(
        std::fs::read(data("instance-identity")).unwrap(),
        b"identity-a\n"
    );
    assert_eq!(
        std::fs::read(data("sessions/one/attachments/big.bin")).unwrap(),
        big(1, 3 * 1024 * 1024 + 123)
    );
    assert!(data("sessions/empty-dir").is_dir());
    assert!(data("sessions/one/attachments.deleted").exists());
    assert!(!data("sessions/one/attachments.lock").exists());
    assert!(!data("workers").exists() && !data("daemon.log").exists());
    assert_eq!(std::fs::read(data("unknown-user-file")).unwrap(), b"mine");
    assert_eq!(
        std::fs::read(data("tool-details/a/keep.txt")).unwrap(),
        b"keep, changed"
    );
    assert!(
        !data("tool-details/a/b.bin").exists(),
        "a file removed on A is removed"
    );
    assert_eq!(std::fs::read(data("tool-details/new.txt")).unwrap(), b"new");

    // Config: rewritten, with the credential link sent as a private regular file.
    Config::load_from(&world.b_config.join("config.toml")).unwrap();
    assert_eq!(
        std::fs::read(world.b_config.join("ec2/x/key")).unwrap(),
        b"ec2 key"
    );
    let linked = world.b_config.join("keys/linked");
    assert!(std::fs::symlink_metadata(&linked).unwrap().is_file());
    assert_eq!(std::fs::metadata(&linked).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&linked).unwrap(), b"credential");

    // Home: links stay links, a private link becomes a regular 0600 file, and
    // the live SQLite database arrives as a whole snapshot without WAL files.
    let profile = world.b_home.join(".codex-personal");
    assert_eq!(
        std::fs::read(profile.join("auth.json")).unwrap(),
        b"{\"a\":1}"
    );
    assert_eq!(
        std::fs::read_link(profile.join("dangling")).unwrap(),
        Path::new("/nonexistent/absolute")
    );
    let token = world.b_home.join(".secrets/token");
    assert!(std::fs::symlink_metadata(&token).unwrap().is_file());
    assert_eq!(
        std::fs::metadata(&token).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read(&token).unwrap(), b"private token");
    assert_eq!(count_rows(&profile.join("opencode.db")), 300);
    assert!(!profile.join("opencode.db-wal").exists() && !profile.join("opencode.db-shm").exists());

    // SessionWiki: one file, WAL folded in.
    assert_eq!(count_rows(&world.b_wiki), 600);
    assert!(!world.b_wiki.with_file_name("index.db-wal").exists());
    assert!(!world.b_wiki.with_file_name("index.db-shm").exists());
}

async fn exchange(
    config: ReceiverConfig,
    requests: Vec<(Request, Vec<u8>)>,
) -> (Vec<Response>, Result<()>) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let receiver = spawn_receiver(config, server_read, server_write);
    let (mut reader, mut writer) = tokio::io::split(client);
    let reader_task = mj_core::runtime::spawn(async move {
        let mut responses = Vec::new();
        while let Ok(Some((response, _))) = read_frame::<_, Response>(&mut reader).await {
            responses.push(response);
        }
        responses
    });
    for (request, payload) in requests {
        if write_frame(&mut writer, &request, &payload).await.is_err() {
            break;
        }
    }
    writer.flush().await.unwrap();
    let outcome = receiver.await.unwrap();
    drop(writer);
    (reader_task.await.unwrap(), outcome)
}

fn hello() -> (Request, Vec<u8>) {
    (
        Request::Hello(Hello {
            mj_version: "test".into(),
            schema_version: mj_store::database::STORE_SCHEMA_VERSION,
            protocol_version: PROTOCOL_VERSION,
            os_family: "unix".into(),
        }),
        Vec::new(),
    )
}

fn put(scope: TransferScope, rel: &str, data: &[u8]) -> Vec<(Request, Vec<u8>)> {
    let mut requests = vec![(
        Request::PutFileStart {
            scope,
            rel_path: rel.into(),
            kind: FileKind::Regular {
                size: data.len() as u64,
                mode: 0o640,
                mtime_ns: 1_700_000_000_000_000_000,
            },
        },
        Vec::new(),
    )];
    for chunk in data.chunks(BLOCK_BYTES) {
        requests.push((
            Request::PutFileChunk {
                raw_len: chunk.len() as u32,
            },
            compress(chunk).unwrap(),
        ));
    }
    requests.push((
        Request::PutFileEnd {
            sha256: sha256_hex(data),
        },
        Vec::new(),
    ));
    requests
}

fn is_error(responses: &[Response]) -> bool {
    matches!(responses.last(), Some(Response::Error { .. }))
}

#[tokio::test]
async fn receiver_rejects_corruption_escapes_and_follows_no_staged_symlink() {
    let world = World::new();
    let stage = DataLayout::at(&world.b_data).migrate_stage();
    let outside = world._root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    // More than 1 MiB through 64 KiB pipes, intact: staged byte for byte, mode and mtime kept.
    let data = big(5, 3 * 1024 * 1024 + 1);
    let mut requests = vec![hello()];
    requests.extend(put(TransferScope::Data, "sessions/s/big.bin", &data));
    requests.push((Request::Done, Vec::new()));
    let (responses, outcome) = exchange(world.receiver(None), requests).await;
    outcome.unwrap();
    assert!(
        matches!(responses.last(), Some(Response::DoneAck)),
        "{responses:?}"
    );
    let staged = stage.join("raw/data/sessions/s/big.bin");
    assert_eq!(std::fs::read(&staged).unwrap(), data);
    let metadata = std::fs::metadata(&staged).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o640);
    assert_eq!(metadata.mtime(), 1_700_000_000);

    // A corrupted file, a corrupted block, and a block past the announced size.
    let mut corrupt = put(TransferScope::Data, "bad.bin", &data);
    let last = corrupt.len() - 1;
    corrupt[last].0 = Request::PutFileEnd {
        sha256: sha256_hex(b"other"),
    };
    let block = big(6, BLOCK_BYTES);
    let patch = |sha: String, index: u64| {
        vec![
            hello(),
            (
                Request::PatchBlocksBegin {
                    target: DbTarget::Main,
                    total_size: BLOCK_BYTES as u64 + 10,
                },
                Vec::new(),
            ),
            (
                Request::PatchBlock {
                    index,
                    raw_len: block.len() as u32,
                    sha256: sha,
                },
                compress(&block).unwrap(),
            ),
            (Request::PatchBlocksEnd, Vec::new()),
        ]
    };
    let mut cases: Vec<Vec<(Request, Vec<u8>)>> = vec![
        [vec![hello()], corrupt].concat(),
        patch(sha256_hex(b"other"), 0),
        patch(sha256_hex(&block), 1),
    ];
    // Paths that leave their directory, locks, and database files.
    for (scope, rel) in [
        (TransferScope::Data, "../escape"),
        (TransferScope::Data, "a/../../escape"),
        (TransferScope::Data, "/absolute"),
        (TransferScope::Home, ".."),
        (TransferScope::Config, "./a/../b"),
        (TransferScope::Data, "controller.lock"),
        (TransferScope::Data, "daemon-start.lock"),
        (TransferScope::Data, "mj.sqlite3"),
        (TransferScope::SessionWiki, "other.db"),
    ] {
        cases.push([vec![hello()], put(scope, rel, b"x")].concat());
    }
    cases.push(vec![
        hello(),
        (
            Request::RemoveFile {
                scope: TransferScope::Data,
                rel_path: "../outside".into(),
            },
            Vec::new(),
        ),
    ]);
    cases.push(put(TransferScope::Data, "before-hello", b"x"));
    for case in cases {
        let (responses, outcome) = exchange(world.receiver(None), case).await;
        assert!(is_error(&responses) && outcome.is_err(), "{responses:?}");
    }
    assert!(!world.b_data.join("escape").exists());
    assert!(!stage.join("raw/data/bad.bin").exists());

    // A staged symlink never redirects a later file: the link is replaced by a
    // directory inside the stage.
    let mut requests = vec![
        hello(),
        (
            Request::PutFileStart {
                scope: TransferScope::Home,
                rel_path: "link".into(),
                kind: FileKind::Symlink {
                    target: outside.clone(),
                },
            },
            Vec::new(),
        ),
    ];
    requests.extend(put(TransferScope::Home, "link/evil", b"evil"));
    requests.push((Request::Done, Vec::new()));
    let (_, outcome) = exchange(world.receiver(None), requests).await;
    outcome.unwrap();
    assert!(!outside.join("evil").exists());
    assert!(stage.join("raw/home/link").is_dir());
    assert_eq!(
        std::fs::read(stage.join("raw/home/link/evil")).unwrap(),
        b"evil"
    );

    // A destination that already has a store refuses every write.
    write(&world.b_config.join("config.toml"), b"");
    let mut requests = vec![hello()];
    requests.extend(put(TransferScope::Data, "late", b"x"));
    let (responses, outcome) = exchange(world.receiver(None), requests).await;
    let Some(Response::HelloAck(ack)) = responses.first() else {
        panic!("{responses:?}")
    };
    assert!(matches!(
        ack.destination_status,
        DestinationStatus::ExistingStore {
            has_config: true,
            ..
        }
    ));
    assert!(is_error(&responses) && outcome.is_err());
    assert!(!stage.join("raw/data/late").exists());
}

#[tokio::test]
async fn a_home_target_with_a_parent_component_is_rejected() {
    let world = World::new();
    let mut plan = world.plan();
    plan.home_copies[0].destination = world.b_home.join("../escape");
    let (sender_read, sender_write, _, receiver_read, receiver_write) = pipe();
    let receiver = spawn_receiver(world.receiver(None), receiver_read, receiver_write);
    let mut sender = Sender::new(plan, sender_read, sender_write);
    let error = sender.phase1().await.err().expect("must fail").to_string();
    assert!(error.contains("escapes"), "{error}");
    drop(sender);
    receiver.await.unwrap().unwrap();
    assert!(!world.b_data.join("../escape").exists());
}

fn failing_at(step: PublicationStep) -> PublicationHook {
    Arc::new(move |reached| {
        if reached == step {
            Err(anyhow!("injected failure at {reached:?}"))
        } else {
            Ok(())
        }
    })
}

async fn attempt(
    world: &World,
    fence: &str,
    hook: Option<PublicationHook>,
) -> (Result<SenderOutcome>, Vec<&'static str>) {
    let (sender_read, sender_write, _, receiver_read, receiver_write) = pipe();
    let receiver = spawn_receiver(world.receiver(hook), receiver_read, receiver_write);
    let mut hooks = Hooks::new(world, world.fence(fence));
    let result = run_migration_sender(world.plan(), sender_read, sender_write, &mut hooks).await;
    receiver.await.unwrap().unwrap();
    (result, hooks.log)
}

#[tokio::test]
async fn publication_failures_leave_the_destination_fenced_and_a_retry_completes() {
    let world = World::new();
    let a_db = DataLayout::at(&world.a_data).database();
    let b_db = DataLayout::at(&world.b_data).database();
    let rollback_log = ["stop", "recheck", "fence", "build", "clear", "rollback"];

    // Before the rewritten store is set aside.
    let (result, log) = attempt(
        &world,
        "t1",
        Some(failing_at(PublicationStep::BeforeStoreRename)),
    )
    .await;
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("injected failure"), "{error}");
    assert_eq!(log, rollback_log);
    assert_eq!(
        read_store_fence_file(&a_db).unwrap(),
        None,
        "the source is unfenced"
    );
    assert!(!b_db.exists());

    // Dependencies are published, the store is not yet in place.
    let (result, _) = attempt(
        &world,
        "t1",
        Some(failing_at(PublicationStep::BeforeStorePlacement)),
    )
    .await;
    assert!(result.is_err());
    assert!(!b_db.exists());
    assert!(world.b_home.join(".codex-personal/auth.json").exists());

    assert!(world.b_data.join("unknown-user-file").exists());
    // A file removed on the source after the rolled-back attempt must not
    // survive the next attempt.
    std::fs::remove_file(world.a_data.join("unknown-user-file")).unwrap();

    // The store is in place and still fenced; a new source fence replaces the
    // rolled-back attempt, which was stale.
    let (result, _) = attempt(
        &world,
        "t2",
        Some(failing_at(PublicationStep::BeforeUnfence)),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        read_store_fence_file(&b_db).unwrap(),
        Some(world.fence("t2"))
    );
    let refused = save_session_to(&b_db, &session()).unwrap_err();
    assert!(
        format!("{refused:#}").to_lowercase().contains("fence"),
        "a normal open must refuse the fenced store: {refused:#}"
    );
    assert!(world.b_wiki.exists());

    // Retry under the same fence resumes and activates.
    let (result, log) = attempt(&world, "t2", None).await;
    let outcome = result.unwrap();
    assert_eq!(log.last(), Some(&"activated"));
    assert!(outcome.summary.sessionwiki);
    assert_eq!(read_store_fence_file(&b_db).unwrap(), None);
    let mut two = session();
    two.id = "two".into();
    save_session_to(&b_db, &two).unwrap();
    assert!(!DataLayout::at(&world.b_data).migrate_stage().exists());
    assert_eq!(count_rows(&world.b_wiki), 600);
    assert_eq!(count_rows(&b_db), 3000);
    assert!(!world.b_data.join("unknown-user-file").exists());
}
