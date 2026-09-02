use std::fs;
use std::process::Command;

#[test]
fn top_level_failure_is_written_to_a_private_per_run_log() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config = root.path().join("config");
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    command
        .args(["checkpoint", "--session", "definitely-missing"])
        .env("MJ_DATA_DIR", &data)
        .env("MJ_CONFIG_DIR", config);

    let output = hel::hel_subprocess::run_with_input(&mut command, &[]).unwrap();

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
}
