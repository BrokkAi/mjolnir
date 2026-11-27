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
        Some(Self { a, b, sshd })
    }
    fn wiki_dir(&self, destination: bool) -> PathBuf {
        let root = if destination {
            self.b.path()
        } else {
            self.a.path()
        };
        root.join("data/sessionwiki")
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
        self.migration_to("127.0.0.1", extra, hook)
    }
    fn migration_to(&self, destination: &str, extra: &[&str], hook: Option<&str>) -> Command {
        self.migration_with_source(destination, extra, hook, true)
    }
    fn migration_with_source(
        &self,
        destination: &str,
        extra: &[&str],
        hook: Option<&str>,
        disconnected: bool,
    ) -> Command {
        self.migration_with_paths(
            destination,
            extra,
            hook,
            disconnected,
            "migration-b",
            &self.b.path().join("data"),
            &self.b.path().join("config"),
        )
    }
    #[allow(clippy::too_many_arguments, reason = "test both destination instances")]
    fn migration_with_paths(
        &self,
        destination: &str,
        extra: &[&str],
        hook: Option<&str>,
        disconnected: bool,
        instance: &str,
        data_dir: &std::path::Path,
        config_dir: &std::path::Path,
    ) -> Command {
        let mut cmd = self.command(false);
        cmd.args(["migrate", destination]);
        if disconnected {
            cmd.args([
                "--disconnect-local",
                "--source-host",
                "mj-migration-source.invalid",
            ]);
        }
        cmd.args([
            "--source-machine-id",
            "migration-source",
            "--dest-instance",
            instance,
            "--remote-command",
            self.b.path().join("receive-mj").to_str().unwrap(),
        ]);
        cmd.arg("--dest-data-dir").arg(data_dir);
        cmd.arg("--dest-config-dir").arg(config_dir);
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
    let ec2 = world.a.path().join("config/ec2/fixture");
    fs::create_dir_all(&ec2).unwrap();
    fs::write(ec2.join("key"), "key fixture").unwrap();
    fs::write(ec2.join("known_hosts"), "known hosts fixture").unwrap();
    let identity = serde_json::to_string(&ec2.join("key").to_string_lossy()).unwrap();
    let known_hosts_arg = serde_json::to_string(&format!(
        "UserKnownHostsFile={}",
        ec2.join("known_hosts").display()
    ))
    .unwrap();
    fs::write(world.a.path().join("config/config.toml"), format!("version = 1\n[phone]\nenabled = false\n[github]\nsession_token_path = '~/secrets/token'\n[profiles.personal]\nkind = 'codex'\nhome = '~/personal-profile'\n[machines.ec2-box]\nkind = 'ssh'\nhost = 'ec2-box.invalid'\nidentity_file = {identity}\nextra_args = ['-o', {known_hosts_arg}]\n")).unwrap();
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
    let log_dir = world.a.path().join("data/logs");
    let mut logs_before: Vec<_> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    logs_before.sort();
    let dry = world.migrate(&[
        "--dry-run",
        "--include-sessionwiki",
        "--source-ssh-arg=-o",
        &known_hosts,
    ]);
    success(&dry);
    let stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(
        stderr.contains("Skipping preflight SSH probe for machine ec2-box"),
        "{stderr}"
    );
    let mut logs_after: Vec<_> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    logs_after.sort();
    assert_eq!(logs_after, logs_before, "dry run changed source logs");
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
    let migration = world.migrate(&["--include-sessionwiki", "--source-ssh-arg=-o", &known_hosts]);
    success(&migration);
    assert!(
        String::from_utf8_lossy(&migration.stderr)
            .contains("Skipping preflight SSH probe for machine ec2-box")
    );
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
    let mj_core::config::Machine::Ssh {
        ssh: third_host, ..
    } = &config.machines["ec2-box"]
    else {
        panic!("third host must use SSH");
    };
    assert_eq!(
        third_host.identity_file.as_deref(),
        Some(world.b.path().join("config/ec2/fixture/key").as_path())
    );
    assert!(third_host.extra_args.contains(&format!(
            "UserKnownHostsFile={}",
            world
                .b
                .path()
                .join("config/ec2/fixture/known_hosts")
                .display()
        )));
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
fn empty_destination_resumes_only_matching_migration_fence() {
    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let source_db = world.a.path().join("data/mj.sqlite3");
    for (destination, note) in [("other-host", "mj migrate"), ("127.0.0.1", "manual")] {
        success(&run(world.command(false).args([
            "daemon",
            "fence",
            "--to",
            destination,
            "--note",
            note,
        ])));
        let fence = mj_controller::database::read_store_fence_file(&source_db)
            .unwrap()
            .unwrap();
        let result = world.migrate(&[]);
        assert!(!result.status.success(), "unrelated fence must be refused");
        let after = mj_controller::database::read_store_fence_file(&source_db)
            .unwrap()
            .unwrap();
        assert!(after.matches_snapshot(&fence));
        assert!(!world.b.path().join("data/mj.sqlite3").exists());
    }
    success(&run(world.command(false).args(["daemon", "unfence"])));
    success(&world.endpoint(false));
    let hooks = world.a.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let log = world.a.path().join("fenced-migration.log");
    let mut cmd = world.migration_command(&[], None);
    cmd.env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "migrate_after_fence")
        .env("MJ_TEST_HOOK_DIR", &hooks)
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .process_group(0);
    let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
    let marker = hooks.join("migrate_after_fence.reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        if let Some(status) = migration.0.try_wait().unwrap() {
            panic!(
                "migration exited before fencing: {status}: {}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        assert!(Instant::now() < deadline, "migration did not fence source");
        thread::sleep(Duration::from_millis(20));
    }
    mj_core::subprocess::terminate_process_group(migration.0.id() as i32, libc::SIGKILL);
    assert!(!wait_child(&mut migration.0).success());
    let stage = world.b.path().join("data/.mj-migrate-stage");
    let id = fs::read_to_string(stage.join("stage-id")).unwrap();
    assert_eq!(id.len(), 32);
    let fence = mj_controller::database::read_store_fence_file(&source_db)
        .unwrap()
        .unwrap();
    assert_eq!(
        fence.note.as_deref(),
        Some(format!("mj migrate stage={id}").as_str())
    );
    assert!(!stage.join("publication.json").exists());

    let other_data = world.b.path().join("other-data");
    let other_config = world.b.path().join("other-config");
    let other_migration = || {
        world.migration_with_paths(
            "127.0.0.1",
            &[],
            None,
            true,
            "migration-other",
            &other_data,
            &other_config,
        )
    };
    let refusal = run(&mut other_migration());
    assert!(
        !refusal.status.success(),
        "empty second destination must be refused"
    );
    assert!(!other_data.exists());
    assert!(!other_config.exists());
    let other_id = other_data.join(".mj-migrate-stage/stage-id");
    fs::create_dir_all(other_id.parent().unwrap()).unwrap();
    fs::write(&other_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    assert_ne!(fs::read_to_string(&other_id).unwrap(), id);
    let refusal = run(&mut other_migration());
    assert!(
        !refusal.status.success(),
        "different staging identity must be refused"
    );
    assert_eq!(
        fs::read_to_string(other_id).unwrap(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert!(!other_data.join("mj.sqlite3").exists());
    let after = mj_controller::database::read_store_fence_file(&source_db)
        .unwrap()
        .unwrap();
    assert!(after.matches_snapshot(&fence));

    let result = world.migrate(&[]);
    success(&result);
    assert!(String::from_utf8_lossy(&result.stderr).contains("resuming cutover"));
    success(&world.endpoint(true));
    assert!(!world.endpoint(false).status.success());
    let refusal = run(&mut other_migration());
    assert!(
        !refusal.status.success(),
        "the activated source must not be migrated to a second destination"
    );
    assert!(!other_data.join("mj.sqlite3").exists());
}

#[cfg(feature = "test-hooks")]
#[test]
fn activated_publication_with_fresh_sessionwiki_resumes_without_overwrite() {
    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let source_wiki = world.wiki_dir(false).join("index.db");
    fs::create_dir_all(source_wiki.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&source_wiki).unwrap();
    db.execute_batch("CREATE TABLE marker(value TEXT); INSERT INTO marker VALUES ('source');")
        .unwrap();
    drop(db);

    let hooks = world.b.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let mut cmd =
        world.migration_command(&["--include-sessionwiki"], Some("migrate_before_unfence"));
    let log = world.a.path().join("wiki-migration-test.log");
    cmd.stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .process_group(0);
    let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
    let marker = hooks.join("migrate_before_unfence.reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        if let Some(status) = migration.0.try_wait().unwrap() {
            panic!(
                "migration exited before activation: {status}: {}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "receiver did not reach activation"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let pid: i32 = fs::read_to_string(marker)
        .unwrap()
        .trim()
        .strip_prefix("pid=")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    assert!(!wait_child(&mut migration.0).success());

    let dest_db = world.b.path().join("data/mj.sqlite3");
    let fence = mj_controller::database::read_store_fence_file(&dest_db)
        .unwrap()
        .unwrap();
    mj_controller::database::clear_store_fence_file(&dest_db, &fence).unwrap();
    let dest_wiki = world.wiki_dir(true).join("index.db");
    fs::rename(
        dest_wiki.with_file_name("index.db.mj-migrate-pending"),
        &dest_wiki,
    )
    .unwrap();
    let stage = world.b.path().join("data/.mj-migrate-stage");
    assert!(stage.join("publication.json").exists());
    let result = world.migrate(&["--include-sessionwiki"]);
    success(&result);
    assert!(!stage.exists());
    success(&world.endpoint(true));
    assert!(!world.endpoint(false).status.success());
    let db = rusqlite::Connection::open(dest_wiki).unwrap();
    let value: String = db
        .query_row("SELECT value FROM marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "source");
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
    let Some(world) = World::new() else { return };
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
    fs::write(
        a_wiki.parent().unwrap().join("index.db.bak-v1"),
        "source backup",
    )
    .unwrap();
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
    assert_ne!(
        fs::read(world.wiki_dir(true).join("index.db.bak-v1"))
            .ok()
            .as_deref(),
        Some(b"source backup".as_slice())
    );

    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let a_wiki = world.wiki_dir(false).join("index.db");
    fs::write(
        a_wiki.parent().unwrap().join("index.db.bak-v1"),
        "source backup",
    )
    .unwrap();
    let b_wiki = world.wiki_dir(true).join("index.db");
    fs::create_dir_all(b_wiki.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&b_wiki).unwrap();
    db.execute_batch(
        "CREATE TABLE marker(value TEXT); INSERT INTO marker VALUES ('destination-data');",
    )
    .unwrap();
    drop(db);
    success(&world.migrate(&["--skip-sessionwiki", "--no-start"]));
    assert!(!world.wiki_dir(true).join("index.db.bak-v1").exists());
    success(&world.endpoint(true));
    let db = rusqlite::Connection::open(&b_wiki).unwrap();
    let marker: String = db
        .query_row("SELECT value FROM marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(marker, "destination-data");
    assert_ne!(
        fs::read(world.wiki_dir(true).join("index.db.bak-v1"))
            .ok()
            .as_deref(),
        Some(b"source backup".as_slice())
    );
    drop(db);

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

#[cfg(feature = "test-hooks")]
#[test]
fn config_change_during_presync_rolls_back_source() {
    let Some(world) = World::new() else { return };
    seed_local_sessions(&world);
    success(&world.endpoint(false));
    let hooks = world.a.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let log_path = world.a.path().join("config-change.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut cmd = world.migration_command(&[], None);
    cmd.env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "migrate_after_phase1")
        .env("MJ_TEST_HOOK_DIR", &hooks)
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
    let reached = hooks.join("migrate_after_phase1.reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() {
        if let Some(status) = migration.0.try_wait().unwrap() {
            panic!(
                "migration exited before phase 1 hook: {status}: {}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "migration did not reach phase 1 hook"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let config = world.a.path().join("config/config.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n# changed while migration was syncing\n");
    fs::write(&config, contents).unwrap();
    fs::write(hooks.join("migrate_after_phase1.continue"), "continue").unwrap();
    assert!(!wait_child(&mut migration.0).success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("config.toml changed during Phase 1 pre-sync"),
        "{log}"
    );
    assert!(!world.b.path().join("data/mj.sqlite3").exists());
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
    success(&world.endpoint(false));
}

#[cfg(feature = "test-hooks")]
#[test]
fn local_session_appearing_during_presync_requires_source_probe() {
    use mj_core::{
        config::TargetTemplate,
        state::{TargetLocator, TargetRuntimeSettings},
    };
    use std::sync::Arc;
    let Some(world) = World::new() else { return };
    let key = world.a.path().join("config/remote-key");
    fs::write(&key, "fixture key").unwrap();
    let key = serde_json::to_string(&key.to_string_lossy()).unwrap();
    let mut config = format!(
        "version = {}\n[phone]\nenabled = false\n[machines.remote]\nkind = 'ssh'\nhost = 'remote.invalid'\nidentity_file = {key}\n",
        mj_core::config::CONFIG_VERSION
    );
    for name in ["localhost", "podman", "docker", "apple-container"] {
        config.push_str(&format!(
            "[targets.{name}]\nkind = 'bare'\nmachine = 'remote'\n"
        ));
    }
    fs::write(world.a.path().join("config/config.toml"), config).unwrap();
    success(&world.endpoint(false));
    let hooks = world.a.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let log_path = world.a.path().join("new-placement.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut cmd = world.migration_with_source("127.0.0.1", &[], None, false);
    cmd.env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "migrate_after_phase1")
        .env("MJ_TEST_HOOK_DIR", &hooks)
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
    let reached = hooks.join("migrate_after_phase1.reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() {
        if let Some(status) = migration.0.try_wait().unwrap() {
            panic!(
                "migration exited before phase 1 hook: {status}: {}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "migration did not reach phase 1 hook"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let data = world.a.path().join("data");
    let mut session = mj_controller::database::test_session("appeared-local", "bundle");
    session.checkpoint = None;
    session.target_template_id = "appeared-local".into();
    session.target_runtime = Some(Arc::new(TargetRuntimeSettings::from(
        &TargetTemplate::LocalBare,
    )));
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: data.join("workers/appeared-local"),
    }));
    mj_controller::database::save_session_to(&data.join("mj.sqlite3"), &session).unwrap();
    fs::write(hooks.join("migrate_after_phase1.continue"), "continue").unwrap();
    assert!(!wait_child(&mut migration.0).success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("a local session or environment appeared during Phase 1 pre-sync"),
        "{log}"
    );
    assert!(!world.b.path().join("data/mj.sqlite3").exists());
    success(&world.endpoint(false));
    let db = rusqlite::Connection::open(data.join("mj.sqlite3")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM sessions WHERE session_id='appeared-local'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        mj_controller::database::read_store_fence(&db)
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn local_engine_appearing_during_presync_requires_engine_probe() {
    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let hooks = world.a.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let log_path = world.a.path().join("new-engine.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut cmd = world.migration_with_source("127.0.0.1", &[], None, false);
    cmd.args(["--source-host", "127.0.0.1"]);
    for arg in [
        "-i".to_owned(),
        world.sshd.key.display().to_string(),
        "-p".to_owned(),
        world.sshd.port.to_string(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", world.sshd.known_hosts.display()),
    ] {
        cmd.arg("--source-ssh-arg").arg(arg);
    }
    cmd.env("MJ_CHAOS_ISOLATED", "1")
        .env("MJ_TEST_HOOK", "migrate_after_phase1")
        .env("MJ_TEST_HOOK_DIR", &hooks)
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    let mut migration = RunningChild(mj_core::subprocess::spawn_child(&mut cmd).unwrap());
    let reached = hooks.join("migrate_after_phase1.reached");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() {
        if let Some(status) = migration.0.try_wait().unwrap() {
            panic!(
                "migration exited before phase 1 hook: {status}: {}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "migration did not reach phase 1 hook"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let data = world.a.path().join("data");
    let mut owner = mj_controller::database::test_session("fixture-owner", "bundle");
    owner.target = None;
    owner.target_runtime = None;
    owner.checkpoint = None;
    owner.target_template_id = "fixture-remote".into();
    mj_controller::database::save_session_to(&data.join("mj.sqlite3"), &owner).unwrap();
    let db = rusqlite::Connection::open(data.join("mj.sqlite3")).unwrap();
    let placement = mj_controller::database::EnvironmentPlacement::from_session(&owner);
    db.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('fixture-env','fixture-owner','reclaimed',?1)", [serde_json::to_string(&placement).unwrap()]).unwrap();
    db.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,kind,resource_id,target_template_id) VALUES ('fixture-lease','fixture-env','fixture-worker','local-podman','fixture-container','podman')", []).unwrap();
    drop(db);
    fs::write(hooks.join("migrate_after_phase1.continue"), "continue").unwrap();
    assert!(!wait_child(&mut migration.0).success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("a local podman session or environment appeared during Phase 1 pre-sync"),
        "{log}"
    );
    assert!(!world.b.path().join("data/mj.sqlite3").exists());
    success(&world.endpoint(false));
    let db = rusqlite::Connection::open(data.join("mj.sqlite3")).unwrap();
    assert!(
        mj_controller::database::read_store_fence(&db)
            .unwrap()
            .is_none()
    );
}

#[test]
fn unavailable_aws_ingress_check_warns_and_continues() {
    let Some(world) = World::new() else { return };
    success(&world.endpoint(false));
    let ec2 = world.a.path().join("config/ec2/fixture");
    fs::create_dir_all(&ec2).unwrap();
    fs::write(
        ec2.join("resources.json"),
        r#"{"state":"active","instance_id":"i-unavailable","region":"test-region"}"#,
    )
    .unwrap();
    let tools_dir = world.a.path().join("fake-tools");
    fs::create_dir(&tools_dir).unwrap();
    mj_core::test_hooks::install_fake_command(
        &tools_dir,
        "aws",
        "printf 'fixture AWS unavailable\\n' >&2\nexit 1\n",
    );
    let mut cmd = world.migration_to("203.0.113.10", &["--dry-run", "--no-aws-modify"], None);
    cmd.arg("--ssh-arg")
        .arg("-o")
        .arg("--ssh-arg")
        .arg("HostName=127.0.0.1");
    cmd.env(
        "PATH",
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", tools_dir.display()),
    );
    let output = run(&mut cmd);
    success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not check EC2 SSH ingress for i-unavailable in test-region"),
        "{stderr}"
    );
    assert!(stderr.contains("203.0.113.10/32"), "{stderr}");
    assert!(!world.b.path().join("data").exists());
    success(&world.endpoint(false));

    let config = world.a.path().join("config/config.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n[machines.blocked]\nkind = 'ssh'\nhost = 'blocked.invalid'\n");
    fs::write(config, contents).unwrap();
    let mut cmd = world.migration_to("203.0.113.10", &["--dry-run", "--no-aws-modify"], None);
    cmd.arg("--ssh-arg")
        .arg("-o")
        .arg("--ssh-arg")
        .arg("HostName=127.0.0.1");
    cmd.env(
        "PATH",
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", tools_dir.display()),
    );
    let output = run(&mut cmd);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ingress = stderr
        .find("could not check EC2 SSH ingress")
        .expect("missing ingress warning");
    let probe = stderr
        .find("destination SSH probes failed")
        .expect("missing SSH probe failure");
    assert!(ingress < probe, "{stderr}");
    assert!(stderr.contains("blocked.invalid"), "{stderr}");
}

#[test]
fn third_hosts_without_copied_credentials_are_still_probed() {
    let Some(world) = World::new() else { return };
    let key = world.a.path().join("config/keys/copied");
    fs::create_dir_all(key.parent().unwrap()).unwrap();
    fs::write(&key, "key fixture").unwrap();
    let identity = serde_json::to_string(&key.to_string_lossy()).unwrap();
    fs::write(world.a.path().join("arg-key"), "argument key").unwrap();
    let known_hosts = world.a.path().join("config/ec2/fixture/known_hosts");
    fs::create_dir_all(known_hosts.parent().unwrap()).unwrap();
    fs::write(&known_hosts, "known hosts fixture").unwrap();
    let known_arg =
        serde_json::to_string(&format!("-oUserKnownHostsFile={}", known_hosts.display())).unwrap();
    fs::write(world.a.path().join("config/config.toml"), format!("version = 1\n[phone]\nenabled = false\n[machines.copied]\nkind = 'ssh'\nhost = 'copied.invalid'\nidentity_file = {identity}\n[machines.arg-key]\nkind = 'ssh'\nhost = 'arg-key.invalid'\nextra_args = ['-i', '~/arg-key']\n[machines.known-only]\nkind = 'ssh'\nhost = 'known-only.invalid'\nextra_args = [{known_arg}]\n[machines.ambient]\nkind = 'ssh'\nhost = 'ambient.invalid'\n")).unwrap();
    success(&world.endpoint(false));
    let output = world.migrate(&["--dry-run"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Skipping preflight SSH probe for machine copied"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Skipping preflight SSH probe for machine arg-key"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Skipping preflight SSH probe for machine known-only"),
        "{stderr}"
    );
    assert!(stderr.contains("ambient.invalid"), "{stderr}");
    assert!(!world.b.path().join("data").exists());
}
