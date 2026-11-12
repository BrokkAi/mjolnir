//! Native history belongs to the client; durable imported sessions belong to the daemon.
mod common;

use mj_core::config::{Config, TargetTemplate};
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn run(command: &mut Command) -> String {
    let output = mj_core::subprocess::run_with_input(command, &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn cli_import_uploads_client_history_to_separate_daemon_store() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("client-home");
    let client_data = root.path().join("client-data");
    let daemon_data = root.path().join("daemon-data");
    let daemon_config = root.path().join("daemon-config");
    let client_config = root.path().join("client-config");
    for path in [
        &home,
        &client_data,
        &daemon_data,
        &daemon_config,
        &client_config,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    // This pre-SSH connector checks the client store's schema for host placement.
    // It is independent of the daemon store and receives no imported archives.
    mj_controller::database::save_state_to(
        &client_data.join("mj.sqlite3"),
        &mj_core::state::State::default(),
    )
    .unwrap();
    let isolation = common::AgentIsolation::under(&home);
    let custom_home = home.join("custom-codex");
    let mut config = Config::default();
    config.phone.bind = "127.0.0.1:0".into();
    config.phone.tailscale_detect = false;
    config.save_to(&client_config.join("config.toml")).unwrap();
    config.profiles.insert(
        "custom".into(),
        mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Codex,
            custom_home.clone(),
        ),
    );
    config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    config.bundles.insert(
        "existing".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "repo".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "repo".into(),
                local: Some(home.join("project")),
                github: None,
                destination: "repo".into(),
                git_ref: None,
            }],
        },
    );
    config.save_to(&daemon_config.join("config.toml")).unwrap();
    let storage = common::DaemonStorage::new(root, daemon_config.clone(), daemon_data.clone());
    let command = |data: &Path, config: &Path| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mj"));
        isolation.apply(&mut cmd);
        common::own_test_daemons(&mut cmd)
            .args(["--instance", "import-upload"])
            .env("MJ_DATA_DIR", data)
            .env("MJ_CONFIG_DIR", config)
            .env("MJOLNIR_NO_UPDATE_CHECK", "1");
        cmd
    };
    run(command(&daemon_data, &daemon_config).arg("sessions"));
    assert!(storage.wait_until_owned(Instant::now() + Duration::from_secs(30)));
    fs::copy(
        daemon_data.join("daemon.json"),
        client_data.join("daemon.json"),
    )
    .unwrap();
    fs::copy(daemon_data.join("api-token"), client_data.join("api-token")).unwrap();
    let metadata =
        serde_json::from_slice(&fs::read(daemon_data.join("daemon.json")).unwrap()).unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        mj_client::daemon::DaemonClient::connect(metadata)
            .await
            .unwrap()
            .create_workspace("imports".into())
            .await
            .unwrap();
    });
    let source = home.join("project");
    fs::create_dir(&source).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["commit", "--allow-empty", "-m", "base"],
    ] {
        run(Command::new("git")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .arg("-C")
            .arg(&source)
            .args(args));
    }
    let native_id = "11111111-2222-4333-8444-555555555555";
    let native = home.join(".codex/sessions/2026/09/13/rollout.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&native, format!("{}\n{}\n",
        serde_json::json!({"timestamp":"2026-09-13T12:00:00Z","type":"session_meta","payload":{"id":native_id,"cwd":source,"history_mode":"paginated"}}),
        serde_json::json!({"timestamp":"2026-09-13T12:00:01Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"import from client"}]}}})
    )).unwrap();
    let destination = storage.path().join("daemon-project");
    let output = run(command(&client_data, &client_config)
        .args([
            "import", "codex", "--latest", "--bundle", "existing", "--path",
        ])
        .arg(&destination));
    assert!(output.contains("Imported"), "{output}");
    assert!(!client_data.join("sessions").exists());
    let archives = fs::read_dir(daemon_data.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(archives.len(), 1);
    let archive = mj_checkpoint::archive::read_archive_verified(&archives[0]).unwrap();
    assert_eq!(
        archive.manifest.repositories[0].metadata.origin,
        destination.to_string_lossy()
    );
    let id = &archive.manifest.session.id;
    let output =
        run(command(&client_data, &client_config).args(["sessions", "--session", id, "--json"]));
    assert!(output.contains(id), "{output}");
    let metadata =
        serde_json::from_slice(&fs::read(daemon_data.join("daemon.json")).unwrap()).unwrap();
    let record = tokio::runtime::Runtime::new().unwrap().block_on(async {
        mj_client::daemon::DaemonClient::connect(metadata)
            .await
            .unwrap()
            .session_record(id.clone())
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(record.project_directory.as_ref(), Some(&destination));
    assert_eq!(
        record.checkpoint.as_ref().unwrap().archive_path,
        archives[0]
    );
    assert_eq!(
        record.project.as_ref().unwrap().bundle.repositories[0]
            .local
            .as_ref(),
        Some(&destination)
    );
    let config = Config::load_from(&daemon_config.join("config.toml")).unwrap();
    assert_eq!(
        config.bundles[&archive.manifest.bundle.id].repositories[0]
            .local
            .as_ref(),
        Some(&destination)
    );
    assert_eq!(
        config.bundles["existing"].repositories[0].local.as_ref(),
        Some(&source)
    );
    fs::create_dir_all(custom_home.join("sessions/2026/09/13")).unwrap();
    let custom_native = custom_home.join("sessions/2026/09/13/rollout.jsonl");
    fs::rename(&native, &custom_native).unwrap();
    fs::remove_dir_all(home.join(".codex")).unwrap();
    let output = run(command(&client_data, &client_config)
        .args([
            "import",
            "codex",
            "--session",
            native_id,
            "--profile",
            "custom",
            "--path",
        ])
        .arg(&destination));
    assert!(
        output.contains(&custom_native.display().to_string()),
        "{output}"
    );
    assert_eq!(
        fs::read_dir(daemon_data.join("sessions")).unwrap().count(),
        2
    );
    assert!(!client_data.join("sessions").exists());
}
