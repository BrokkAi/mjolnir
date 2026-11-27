#![cfg(unix)]

mod common;

use std::{
    fs,
    net::TcpListener,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Sshd {
    _directory: tempfile::TempDir,
    child: Child,
    key: PathBuf,
    known_hosts: PathBuf,
    port: u16,
}
impl Drop for Sshd {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            mj_core::subprocess::terminate_process_group(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}
fn run(command: &mut Command) -> std::process::Output {
    mj_core::subprocess::run_with_input(command, &[]).expect("run command")
}
fn success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn sshd() -> Option<Sshd> {
    let directory = tempfile::tempdir().unwrap();
    let host = directory.path().join("host");
    let key = directory.path().join("client");
    for path in [&host, &key] {
        success(&run(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)));
    }
    let authorized = directory.path().join("authorized_keys");
    fs::copy(key.with_extension("pub"), &authorized).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let known_hosts = directory.path().join("known_hosts");
    let public_key = fs::read_to_string(host.with_extension("pub")).unwrap();
    fs::write(&known_hosts, format!("[127.0.0.1]:{port} {public_key}")).unwrap();
    let config = directory.path().join("sshd_config");
    fs::write(&config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAcceptEnv HOME SESSIONWIKI_DATA MJ_DAEMON_OWNER_PID MJ_CHAOS_ISOLATED MJ_TEST_HOOK MJ_TEST_HOOK_DIR\n", host.display(), directory.path().join("pid").display(), authorized.display())).unwrap();
    let log = fs::File::create(directory.path().join("sshd.log")).unwrap();
    let mut command = Command::new("/usr/sbin/sshd");
    command
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    let child = mj_core::subprocess::spawn_child(&mut command).expect("start sshd");
    let mut server = Sshd {
        _directory: directory,
        child,
        key,
        known_hosts,
        port,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(server);
        }
        if let Some(status) = server.child.try_wait().unwrap() {
            let log =
                fs::read_to_string(server._directory.path().join("sshd.log")).unwrap_or_default();
            if cfg!(target_os = "macos") {
                eprintln!("unprivileged sshd unavailable: {status}: {log}");
                return None;
            }
            panic!("sshd exited {status}: {log}");
        }
        assert!(Instant::now() < deadline, "sshd did not listen");
        thread::sleep(Duration::from_millis(20));
    }
}
struct World {
    a: common::DaemonStorage,
    b: common::DaemonStorage,
    sshd: Sshd,
    external_wiki: bool,
}
impl World {
    fn new() -> Option<Self> {
        let sshd = sshd()?;
        let aroot = tempfile::tempdir().unwrap();
        let broot = tempfile::tempdir().unwrap();
        fs::create_dir_all(aroot.path().join("config")).unwrap();
        fs::write(
            aroot.path().join("config/config.toml"),
            "version = 1\n[phone]\nenabled = false\n",
        )
        .unwrap();
        let adata = aroot.path().join("data");
        let aconfig = aroot.path().join("config");
        let bdata = broot.path().join("data");
        let bconfig = broot.path().join("config");
        // sshd sets HOME from the account database after accepting the client
        // environment. Set it in this temporary launcher before execing the real binary.
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
        let launcher = broot.path().join("receive-mj");
        std::os::unix::fs::symlink(
            mj_core::test_hooks::script_only_fake_dispatcher(),
            &launcher,
        )
        .unwrap();
        fs::write(
            broot.path().join("receive-mj.script"),
            format!(
                "export HOME={}\nexec {} \"$@\"\n",
                quote(broot.path().to_str().unwrap()),
                quote(env!("CARGO_BIN_EXE_mj"))
            ),
        )
        .unwrap();
        let a = common::DaemonStorage::new(aroot, aconfig, adata);
        let b = common::DaemonStorage::new(broot, bconfig, bdata);
        Some(Self {
            a,
            b,
            sshd,
            external_wiki: false,
        })
    }
    fn wiki_dir(&self, destination: bool) -> PathBuf {
        let root = if destination {
            self.b.path()
        } else {
            self.a.path()
        };
        root.join(if self.external_wiki {
            "wiki"
        } else {
            "data/sessionwiki"
        })
    }
    fn command(&self, destination: bool) -> Command {
        let root = if destination {
            self.b.path()
        } else {
            self.a.path()
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
        common::own_test_daemons(&mut command).args([
            "--instance",
            if destination {
                "migration-b"
            } else {
                "migration-a"
            },
        ]);
        common::AgentIsolation::under(root).apply(&mut command);
        command
            .env("MJ_DATA_DIR", root.join("data"))
            .env("MJ_CONFIG_DIR", root.join("config"))
            .env("SESSIONWIKI_DATA", self.wiki_dir(destination))
            .env("MJOLNIR_NO_UPDATE_CHECK", "1");
        command
    }
    fn endpoint(&self, destination: bool) -> std::process::Output {
        run(self
            .command(destination)
            .args(["daemon", "endpoint", "--json"]))
    }
    fn migration_command(&self, extra: &[&str], hook: Option<&str>) -> Command {
        let mut cmd = self.command(false);
        cmd.args([
            "migrate",
            "127.0.0.1",
            "--disconnect-local",
            "--source-host",
            "mj-migration-source.invalid",
            "--source-machine-id",
            "migration-source",
            "--dest-instance",
            "migration-b",
            "--remote-command",
            self.b.path().join("receive-mj").to_str().unwrap(),
        ]);
        cmd.arg("--dest-data-dir").arg(self.b.path().join("data"));
        cmd.arg("--dest-config-dir")
            .arg(self.b.path().join("config"));
        for arg in [
            "-i".to_owned(),
            self.sshd.key.display().to_string(),
            "-p".to_owned(),
            self.sshd.port.to_string(),
            "-o".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            "-o".to_owned(),
            format!("UserKnownHostsFile={}", self.sshd.known_hosts.display()),
            "-o".to_owned(),
            format!(
                "SetEnv=HOME={} SESSIONWIKI_DATA={} MJ_DAEMON_OWNER_PID={}{}",
                self.b.path().display(),
                self.wiki_dir(true).display(),
                std::process::id(),
                hook.map_or_else(String::new, |name| format!(
                    " MJ_CHAOS_ISOLATED=1 MJ_TEST_HOOK={name} MJ_TEST_HOOK_DIR={}",
                    self.b.path().join("hooks").display()
                ))
            ),
        ] {
            cmd.arg("--ssh-arg").arg(arg);
        }
        cmd.args(extra);
        cmd
    }
    fn migrate(&self, extra: &[&str]) -> std::process::Output {
        run(&mut self.migration_command(extra, None))
    }
}

fn seed_local_sessions(world: &World) {
    use mj_controller::database::{save_session_to, test_session};
    use mj_core::{
        config::TargetTemplate,
        state::{TargetLocator, TargetRuntimeSettings},
    };
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    let data = world.a.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let shared = world.a.path().join(".codex-shared");
    let whole = world.a.path().join(".codex-whole");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(&whole).unwrap();
    fs::write(shared.join("auth.json"), br#"{"token":"shared"}"#).unwrap();
    for id in ["linked-auth", "linked-profile"] {
        let worker = data.join("workers").join(id);
        fs::create_dir_all(&worker).unwrap();
        if id == "linked-auth" {
            fs::create_dir_all(worker.join("profile")).unwrap();
            symlink(shared.join("auth.json"), worker.join("profile/auth.json")).unwrap();
        } else {
            symlink(&whole, worker.join("profile")).unwrap();
        }
        let mut session = test_session(id, "bundle");
        session.checkpoint = None;
        session.target_template_id = "localhost".into();
        session.target_runtime = Some(Arc::new(TargetRuntimeSettings::from(
            &TargetTemplate::LocalBare,
        )));
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: worker,
        }));
        save_session_to(&data.join("mj.sqlite3"), &session).unwrap();
    }
    let connection = rusqlite::Connection::open(data.join("mj.sqlite3")).unwrap();
    mj_controller::database::pin_session_memory_key_with(
        &connection,
        "linked-auth",
        "pinned-project-key",
    )
    .unwrap();
}

fn seed_rich_store(world: &World) {
    use mj_controller::database::EnvironmentPlacement;
    use mj_core::state::TargetLocator;
    let data = world.a.path().join("data");
    let db_path = data.join("mj.sqlite3");
    let archive = data.join("sessions/archive.hel.zip");
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, b"managed checkpoint fixture").unwrap();
    let mut suspended = mj_controller::database::test_session("archived", "bundle");
    suspended.target_template_id = "localhost".into();
    suspended.target = None;
    suspended.checkpoint.as_mut().unwrap().archive_path = archive.clone();
    mj_controller::database::save_session_to(&db_path, &suspended).unwrap();
    let mut podman = mj_controller::database::test_session("podman", "bundle");
    podman.checkpoint = None;
    podman.target_template_id = "podman".into();
    podman.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: "fixture-container".into(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    mj_controller::database::save_session_to(&db_path, &podman).unwrap();
    let db = rusqlite::Connection::open(db_path).unwrap();
    db.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('fixture-env','podman','reclaimed',?1)", [serde_json::to_string(&EnvironmentPlacement::from_session(&podman)).unwrap()]).unwrap();
    db.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,resource_id,target_template_id) VALUES ('fixture-lease','fixture-env','podman','local-podman','fixture-container','podman')", []).unwrap();
    db.execute("UPDATE environments SET target_lease_id='fixture-lease' WHERE environment_id='fixture-env'", []).unwrap();
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    let historical_root = data.join("workers/historical-worker");
    fs::create_dir_all(historical_root.join("profile")).unwrap();
    symlink(
        world.a.path().join(".codex-shared/auth.json"),
        historical_root.join("profile/auth.json"),
    )
    .unwrap();
    db.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,workspace,target_template_id) VALUES ('historical-bare','fixture-env','historical-worker','local-bare',?1,'localhost')", [historical_root.as_os_str().as_bytes()]).unwrap();
    for (id, phase) in [("linked-auth", "completed"), ("linked-profile", "failed")] {
        let target = TargetLocator::LocalBare {
            worker_root: data.join("workers").join(id),
        };
        let op = serde_json::json!({
            "operation_id": format!("move-{id}"), "selection": {"session_id":id,"target_template_id":"localhost"},
            "source_profile_id":"codex", "source_target_template_id":"localhost", "source_target":target,
            "source_additional_mounts":[], "destination_target":null, "configuration_fingerprint":"fixture",
            "source_snapshot":{"archive_path":archive,"sha256":"a".repeat(64)},
            "checkpoint":suspended.checkpoint, "queue":"start", "phase":phase,
            "queue_admission_started":false,"queue_admission_finished":false,"cancellation_requested":false,
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        });
        let _: mj_core::state::MoveOperation = serde_json::from_value(op.clone()).unwrap();
        db.execute(
            "INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES (?1,?2,?3)",
            rusqlite::params![id, format!("move-{id}"), op.to_string()],
        )
        .unwrap();
    }
    let target = TargetLocator::LocalBare {
        worker_root: data.join("workers/linked-auth"),
    };
    db.execute("INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase) VALUES ('linked-auth','fixture-restart',?1,'build','prepared')", [serde_json::to_string(&target).unwrap()]).unwrap();
    db.execute_batch("INSERT INTO host_snapshot_cache(session_id,host,sha256,path,written_at) VALUES ('linked-auth','local','a','/a','now');
        INSERT OR REPLACE INTO host_container_sizes(host,cpus,memory_bytes) VALUES ('local',1,1);
        INSERT OR IGNORE INTO mount_history(host,source,ordinal) VALUES ('local',X'2F61',0);
        INSERT INTO detached_drafts(draft_id,workspace_id,session_id,source,owner_pid,saved_at,text,revision) VALUES ('fixture-draft','default','linked-auth','terminal',123,'now','draft',1);").unwrap();
    let identity =
        serde_json::to_string(&mj_core::repository::RepositoryIdentity::Local("/a".into()))
            .unwrap();
    db.execute("INSERT OR REPLACE INTO project_locations(host,directory,checkout_root,repository_root,identity_json,seen_at) VALUES ('local',X'2F61',X'2F61',X'2F61',?1,'now')", [identity]).unwrap();
    let ec2 = world.a.path().join("config/ec2/fixture");
    fs::create_dir_all(&ec2).unwrap();
    fs::write(
        ec2.join("resources.json"),
        r#"{"state":"active","instance_id":"i-fixture","region":"test-region"}"#,
    )
    .unwrap();
    fs::write(ec2.join("key"), "key fixture").unwrap();
    fs::write(ec2.join("known_hosts"), "known hosts fixture").unwrap();
}

#[test]
fn full_migration_and_dry_run_over_real_sshd() {
    let Some(world) = World::new() else { return };
    seed_local_sessions(&world);
    fs::create_dir_all(world.a.path().join("personal-profile")).unwrap();
    fs::write(
        world.a.path().join("personal-profile/auth.json"),
        "profile-secret",
    )
    .unwrap();
    fs::create_dir_all(world.a.path().join("secrets")).unwrap();
    fs::write(world.a.path().join("secrets/token"), "github-secret").unwrap();
    fs::write(world.a.path().join("config/config.toml"), "version = 1\n[phone]\nenabled = false\n[github]\nsession_token_path = '~/secrets/token'\n[profiles.personal]\nkind = 'codex'\nhome = '~/personal-profile'\n").unwrap();
    success(&world.endpoint(false));
    seed_rich_store(&world);
    let identity = fs::read(world.a.path().join("data/instance-identity")).unwrap();
    fs::write(
        world.a.path().join("data/custom-user-note.txt"),
        vec![42; 1024 * 1024 + 1],
    )
    .unwrap();
    let wiki = world.a.path().join("data/sessionwiki/index.db");
    fs::create_dir_all(wiki.parent().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(&wiki).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS migration_marker(value TEXT); INSERT INTO migration_marker VALUES ('copied');").unwrap();
    drop(connection);
    let known_hosts = format!(
        "--source-ssh-arg=UserKnownHostsFile={}",
        world
            .a
            .path()
            .join("config/ec2/fixture/known_hosts")
            .display()
    );
    let destination_entries: Vec<_> = fs::read_dir(world.b.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let dry = world.migrate(&[
        "--dry-run",
        "--include-sessionwiki",
        "--source-ssh-arg=-o",
        &known_hosts,
    ]);
    success(&dry);
    let stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(
        stderr.contains(&format!("Destination home: {}", world.b.path().display())),
        "{stderr}"
    );
    assert!(!world.b.path().join("data").exists());
    assert!(!world.b.path().join("config").exists());
    let entries_after: Vec<_> = fs::read_dir(world.b.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries_after, destination_entries);
    success(&world.endpoint(false));
    success(&world.migrate(&["--include-sessionwiki", "--source-ssh-arg=-o", &known_hosts]));
    let wiki =
        rusqlite::Connection::open(world.b.path().join("data/sessionwiki/index.db")).unwrap();
    let marker: String = wiki
        .query_row("SELECT value FROM migration_marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(marker, "copied");
    drop(wiki);
    assert_eq!(
        fs::read(world.b.path().join("data/instance-identity")).unwrap(),
        identity
    );
    assert_eq!(
        fs::read(world.b.path().join("data/custom-user-note.txt"))
            .unwrap()
            .len(),
        1024 * 1024 + 1
    );
    success(&world.endpoint(true));
    assert_eq!(
        fs::read(world.b.path().join("personal-profile/auth.json")).unwrap(),
        b"profile-secret"
    );
    assert_eq!(
        fs::read(world.b.path().join("secrets/token")).unwrap(),
        b"github-secret"
    );
    let config =
        mj_core::config::Config::load_from(&world.b.path().join("config/config.toml")).unwrap();
    assert_eq!(
        config.profiles["personal"].home,
        world.b.path().join("personal-profile")
    );
    assert_eq!(
        config.github.session_token_path.as_deref(),
        Some(world.b.path().join("secrets/token").as_path())
    );
    let machine = &config.machines["migration-source"];
    let mj_core::config::Machine::Ssh { ssh, .. } = machine else {
        panic!("source machine must use SSH")
    };
    assert!(ssh.extra_args.contains(&format!(
            "UserKnownHostsFile={}",
            world
                .b
                .path()
                .join("config/ec2/fixture/known_hosts")
                .display()
        )));
    assert_eq!(
        fs::read(world.b.path().join("data/sessions/archive.hel.zip")).unwrap(),
        b"managed checkpoint fixture"
    );
    assert!(world.b.path().join("config/ec2/fixture/key").is_file());
    assert!(
        world
            .b
            .path()
            .join("config/ec2/fixture/known_hosts")
            .is_file()
    );
    assert!(
        world
            .b
            .path()
            .join("config/ec2/fixture/resources.json")
            .is_file()
    );
    let db = rusqlite::Connection::open(world.b.path().join("data/mj.sqlite3")).unwrap();
    let lease_kind: String = db
        .query_row(
            "SELECT kind FROM environment_workers WHERE lease_id='fixture-lease'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lease_kind, "ssh-podman");
    let restart_target: String = db
        .query_row(
            "SELECT target_json FROM worker_restart_intents WHERE session_id='linked-auth'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(matches!(
        serde_json::from_str::<mj_core::state::TargetLocator>(&restart_target).unwrap(),
        mj_core::state::TargetLocator::SshHostBare { .. }
    ));
    let owner: Option<i64> = db
        .query_row(
            "SELECT owner_pid FROM detached_drafts WHERE draft_id='fixture-draft'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner, None);
    for id in ["linked-auth", "linked-profile"] {
        let json: String = db
            .query_row(
                "SELECT operation_json FROM session_moves WHERE session_id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let op: mj_core::state::MoveOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(
            op.checkpoint.unwrap().archive_path,
            world.b.path().join("data/sessions/archive.hel.zip")
        );
        assert_eq!(
            op.source_snapshot.unwrap().archive_path,
            world.a.path().join("data/sessions/archive.hel.zip")
        );
    }
    let snapshot_count: i64 = db
        .query_row(
            "SELECT count(*) FROM host_snapshot_cache WHERE host='local'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshot_count, 0);
    let old_host_count: i64 = db
        .query_row(
            "SELECT count(*) FROM host_container_sizes WHERE host='mj-migration-source.invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_host_count, 1);
    let history_count: i64 = db
        .query_row(
            "SELECT count(*) FROM mount_history WHERE host='mj-migration-source.invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(history_count, 1);
    let project_count: i64 = db
        .query_row(
            "SELECT count(*) FROM project_locations WHERE host='mj-migration-source.invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(project_count, 1);
    assert_eq!(
        mj_controller::database::read_pinned_memory_key_with(&db, "linked-auth")
            .unwrap()
            .as_deref(),
        Some("pinned-project-key")
    );
    let state =
        mj_controller::database::load_state_with(&db, &world.b.path().join("data/sessions"))
            .unwrap();
    assert!(matches!(
        state.sessions["linked-auth"].target.as_deref(),
        Some(mj_core::state::TargetLocator::SshHostBare { .. })
    ));
    assert!(
        !fs::symlink_metadata(
            world
                .a
                .path()
                .join("data/workers/linked-auth/profile/auth.json")
        )
        .unwrap()
        .file_type()
        .is_symlink()
    );
    assert!(
        !fs::symlink_metadata(
            world
                .a
                .path()
                .join("data/workers/historical-worker/profile/auth.json")
        )
        .unwrap()
        .file_type()
        .is_symlink()
    );
    assert!(
        fs::symlink_metadata(world.a.path().join("data/workers/linked-profile/profile"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let old = world.endpoint(false);
    assert!(!old.status.success(), "source must stay fenced");
}

#[cfg(feature = "test-hooks")]
struct RunningChild(Child);
#[cfg(feature = "test-hooks")]
impl Drop for RunningChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            mj_core::subprocess::terminate_process_group(self.0.id() as i32, libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
#[cfg(feature = "test-hooks")]
fn wait_child(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "child did not finish");
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn crash_windows_and_competing_b_autostarter() {
    for hook in ["migrate_before_store_placement", "migrate_before_unfence"] {
        let Some(world) = World::new() else { return };
        success(&world.endpoint(false));
        let hooks = world.b.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let mut cmd = world.migration_command(&[], Some(hook));
        let log = fs::File::create(world.a.path().join("migration-test.log")).unwrap();
        cmd.stdout(Stdio::null()).stderr(log).process_group(0);
        let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
        let marker = hooks.join(format!("{hook}.reached"));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !marker.exists() {
            if let Some(status) = migration.0.try_wait().unwrap() {
                panic!(
                    "migration exited before {hook}: {status}: {}",
                    fs::read_to_string(world.a.path().join("migration-test.log"))
                        .unwrap_or_default()
                );
            }
            assert!(Instant::now() < deadline, "receiver did not reach {hook}");
            thread::sleep(Duration::from_millis(20));
        }
        let mut competing = if hook == "migrate_before_unfence" {
            let mut cmd = world.command(true);
            cmd.args(["daemon", "endpoint", "--json"])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            cmd.process_group(0);
            let mut child = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
            thread::sleep(Duration::from_millis(300));
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "B client must wait for the start lock"
            );
            assert!(!world.b.path().join("data/daemon.json").exists());
            Some(child)
        } else {
            assert!(!world.b.path().join("data/mj.sqlite3").exists());
            None
        };
        let pid: i32 = fs::read_to_string(marker)
            .unwrap()
            .trim()
            .strip_prefix("pid=")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        assert!(!wait_child(&mut migration.0).success());
        if let Some(child) = &mut competing {
            assert!(
                !wait_child(&mut child.0).success(),
                "B client must refuse the fenced store"
            );
            assert!(!world.b.path().join("data/daemon.json").exists());
        }
        assert!(
            !world.endpoint(false).status.success(),
            "A must stay fenced"
        );
        if hook == "migrate_before_store_placement" {
            let config = world.b.path().join("config/config.toml");
            let saved = world.b.path().join("original-config");
            fs::rename(&config, &saved).unwrap();
            fs::write(&config, "external replacement").unwrap();
            let refusal = world.migrate(&["--dry-run"]);
            assert!(
                !refusal.status.success(),
                "an externally replaced destination must be refused"
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), "external replacement");
            fs::remove_file(&config).unwrap();
            fs::rename(saved, config).unwrap();
        }
        success(&world.migrate(&[]));
        success(&world.endpoint(true));
        assert!(!world.endpoint(false).status.success());
    }
}

#[test]
fn rollback_and_destination_refusals() {
    let Some(mut world) = World::new() else {
        return;
    };
    world.external_wiki = true;
    seed_local_sessions(&world);
    success(&world.endpoint(false));
    let a_wiki = world.wiki_dir(false).join("index.db");
    let b_wiki = world.wiki_dir(true).join("index.db");
    for path in [&a_wiki, &b_wiki] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        let (journal_mode, value) = if path == &a_wiki {
            ("WAL", "source-data")
        } else {
            ("DELETE", "destination-data")
        };
        db.execute_batch(&format!(
            "PRAGMA journal_mode={journal_mode}; CREATE TABLE marker(value TEXT);"
        ))
        .unwrap();
        db.execute("INSERT INTO marker VALUES (?1)", [value])
            .unwrap();
    }
    let db = rusqlite::Connection::open(&b_wiki).unwrap();
    db.execute_batch("BEGIN; SELECT * FROM marker;").unwrap();
    let output = world.migrate(&["--include-sessionwiki", "--overwrite-sessionwiki"]);
    assert!(
        !output.status.success(),
        "a locked index must refuse publication"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("SessionWiki index is currently in use"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    success(&world.endpoint(false));
    assert!(
        fs::symlink_metadata(
            world
                .a
                .path()
                .join("data/workers/linked-auth/profile/auth.json")
        )
        .unwrap()
        .file_type()
        .is_symlink()
    );
    assert!(!world.b.path().join("data/mj.sqlite3").exists());
    assert!(
        world
            .b
            .path()
            .join("data/.mj-migrate-stage/publication.json")
            .is_file()
    );
    let retry = world.migrate(&["--include-sessionwiki", "--overwrite-sessionwiki"]);
    success(&retry);
    assert!(String::from_utf8_lossy(&retry.stderr).contains("unfinished migration attempt"));
    success(&world.endpoint(true));
    assert!(!world.endpoint(false).status.success());
    let migrated_wiki = rusqlite::Connection::open(&b_wiki).unwrap();
    let marker: String = migrated_wiki
        .query_row("SELECT value FROM marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(marker, "source-data");
    drop(migrated_wiki);

    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    fs::create_dir_all(world.b.path().join("config")).unwrap();
    let existing = world.b.path().join("config/config.toml");
    fs::write(&existing, "leave me alone").unwrap();
    let output = world.migrate(&["--dry-run"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--dest-instance"));
    assert_eq!(fs::read_to_string(&existing).unwrap(), "leave me alone");
    fs::remove_file(existing).unwrap();
    fs::create_dir_all(world.b.path().join("data")).unwrap();
    let existing_db = world.b.path().join("data/mj.sqlite3");
    fs::write(&existing_db, "existing database").unwrap();
    let output = world.migrate(&["--dry-run"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--dest-instance"));
    assert_eq!(
        fs::read_to_string(existing_db).unwrap(),
        "existing database"
    );
    success(&world.endpoint(false));

    let Some(world) = World::new() else { return };
    fs::create_dir_all(world.a.path().join("personal-profile")).unwrap();
    fs::write(world.a.path().join("personal-profile/auth.json"), "source").unwrap();
    fs::write(world.a.path().join("config/config.toml"), "version = 1\n[phone]\nenabled = false\n[profiles.personal]\nkind = 'codex'\nhome = '~/personal-profile'\n").unwrap();
    success(&world.endpoint(false));
    fs::create_dir_all(world.b.path().join("personal-profile")).unwrap();
    let collision = world.b.path().join("personal-profile/auth.json");
    fs::write(&collision, "destination").unwrap();
    let output = world.migrate(&["--dry-run"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(fs::read_to_string(collision).unwrap(), "destination");
    success(&world.endpoint(false));
}

/// Dry-run SSH probes must not create cache or known-hosts files on the destination.
#[test]
fn dry_run_source_probe_keeps_destination_untouched() {
    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let known_hosts = &world.sshd.known_hosts;
    let mut cmd = world.command(false);
    cmd.args([
        "migrate",
        "127.0.0.1",
        "--dry-run",
        "--source-host",
        "127.0.0.1",
        "--source-machine-id",
        "probe-source",
        "--dest-instance",
        "migration-b",
        "--remote-command",
    ])
    .arg(world.b.path().join("receive-mj"))
    .arg("--dest-data-dir")
    .arg(world.b.path().join("data"))
    .arg("--dest-config-dir")
    .arg(world.b.path().join("config"));
    for arg in [
        "-i".to_owned(),
        world.sshd.key.display().to_string(),
        "-p".to_owned(),
        world.sshd.port.to_string(),
        "-o".to_owned(),
        "StrictHostKeyChecking=yes".to_owned(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
    ] {
        cmd.arg("--ssh-arg").arg(arg);
    }
    for arg in [
        "-i".to_owned(),
        world.sshd.key.display().to_string(),
        "-p".to_owned(),
        world.sshd.port.to_string(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
    ] {
        cmd.arg("--source-ssh-arg").arg(arg);
    }
    success(&run(&mut cmd));
    assert!(
        !world.b.path().join("data").exists(),
        "dry-run created destination data"
    );
    assert!(
        !world.b.path().join("config").exists(),
        "dry-run created destination config"
    );
    // An actual migration honors an explicit SSH policy supplied by the user.
    let mut cmd = world.command(false);
    cmd.args([
        "migrate",
        "127.0.0.1",
        "--source-host",
        "127.0.0.1",
        "--source-machine-id",
        "probe-source",
        "--dest-instance",
        "migration-b",
        "--no-start",
        "--remote-command",
    ])
    .arg(world.b.path().join("receive-mj"))
    .arg("--dest-data-dir")
    .arg(world.b.path().join("data"))
    .arg("--dest-config-dir")
    .arg(world.b.path().join("config"));
    for arg in [
        "-i".to_owned(),
        world.sshd.key.display().to_string(),
        "-p".to_owned(),
        world.sshd.port.to_string(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
    ] {
        cmd.arg("--ssh-arg").arg(arg);
    }
    for arg in [
        "-i".to_owned(),
        world.sshd.key.display().to_string(),
        "-p".to_owned(),
        world.sshd.port.to_string(),
        "-o".to_owned(),
        "StrictHostKeyChecking=no".to_owned(),
        "-o".to_owned(),
        "UserKnownHostsFile=/dev/null".to_owned(),
    ] {
        cmd.arg("--source-ssh-arg").arg(arg);
    }
    success(&run(&mut cmd));
    assert!(world.b.path().join("data/mj.sqlite3").is_file());
    assert!(!world.b.path().join("data/daemon.json").exists());
}
