mod common;

use std::fs;
use std::process::Command;

#[test]
fn top_level_failure_is_written_to_a_private_per_run_log() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config = root.path().join("config");
    let storage = common::DaemonStorage::new(root, config.clone(), data.clone());
    let root_path = storage.path().to_path_buf();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    common::own_test_daemons(&mut command)
        .args(["checkpoint", "--session", "definitely-missing"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", config);

    let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();

    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("daemon.json")).unwrap()).unwrap();
    let daemon_pid = metadata["pid"].as_u64().unwrap() as u32;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown session definitely-missing"));
    let logs = fs::read_dir(data.join("logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(
        logs.len(),
        2,
        "checkpoint starts the database-owning daemon"
    );
    assert!(logs.iter().all(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("mj-") && name.ends_with(".log"))
    }));
    let (command_log, _) = logs
        .iter()
        .map(|path| (path, fs::read_to_string(path).unwrap()))
        .find(|(_, contents)| contents.contains("command=\"checkpoint\""))
        .expect("one log belongs to the checkpoint client");
    let contents = fs::read_to_string(command_log).unwrap();
    assert!(contents.contains("Mjolnir started"));
    assert!(contents.contains("command=\"checkpoint\""));
    assert!(contents.contains("Mjolnir exited with an error"));
    assert!(contents.contains("unknown session definitely-missing"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            logs.iter()
                .map(|path| fs::metadata(path).unwrap().permissions().mode() & 0o777)
                .collect::<Vec<_>>(),
            vec![0o600; logs.len()]
        );
    }
    drop(storage);
    assert!(!root_path.exists(), "fixture storage was not removed");
    assert_daemon_gone(daemon_pid);
}

fn assert_daemon_gone(pid: u32) {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    assert!(
        system
            .process(pid)
            .is_none_or(|process| process.status() == sysinfo::ProcessStatus::Zombie),
        "fixture daemon {pid} survived its storage"
    );
}

#[test]
fn a_panicking_checkpoint_fixture_stops_its_daemon_before_removing_storage() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().to_path_buf();
    let data = root.path().join("data");
    let config = root.path().join("config");
    let storage = common::DaemonStorage::new(root, config.clone(), data.clone());
    let output = mj_core::subprocess::run_with_input(
        common::own_test_daemons(
            Command::new(env!("CARGO_BIN_EXE_mj"))
                .args(["checkpoint", "--session", "definitely-missing"])
                .env("MJ_DATA_DIR", &data)
                .env("MJ_CONFIG_DIR", &config),
        ),
        &[],
    )
    .unwrap();
    assert!(!output.status.success());
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("daemon.json")).unwrap()).unwrap();
    let daemon_pid = metadata["pid"].as_u64().unwrap() as u32;
    let panic = std::panic::catch_unwind(move || {
        let _storage = storage;
        panic!("exercise fixture unwinding");
    });
    assert!(panic.is_err());
    assert!(!root_path.exists(), "fixture storage was not removed");
    assert_daemon_gone(daemon_pid);
}
