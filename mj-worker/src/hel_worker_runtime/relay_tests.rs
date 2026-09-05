use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, Implementation, SessionUpdate, TextContent,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::*;
use hel::hel_acp::{CommandRequest, RuntimeEvent};
use hel::hel_config::ExecutionPolicy;
use hel::hel_elicitation::ElicitationResponse;
use hel::hel_worker::{
    DurableRelay, RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayCommand, RelayErrorCode,
    RelayExecutionState, RelayObservation, RelayProtocolError, RelayRequest, RelayRequestEnvelope,
    RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload,
};
use hel::hel_worker_launch::ProjectMemoryMcpDelivery;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

struct TestRuntimeEventSender(mpsc::Sender<RuntimeEvent>);

impl TestRuntimeEventSender {
    fn send(&self, event: RuntimeEvent) -> std::result::Result<(), ()> {
        self.0.try_send(event).map_err(|_| ())
    }
}

fn runtime_event_channel() -> (TestRuntimeEventSender, mpsc::Receiver<RuntimeEvent>) {
    let (sender, receiver) = mpsc::channel(unix::ACP_EVENT_CHANNEL_CAPACITY);
    (TestRuntimeEventSender(sender), receiver)
}

#[test]
fn login_path_discovery_uses_the_marked_result_after_profile_chatter() {
    assert_eq!(
        unix::parse_login_path(
            b"profile greeting\n__HEL_LOGIN_PATH__=/old/bin\nmore chatter\n__HEL_LOGIN_PATH__=/opt/node/bin:/usr/bin\n"
        )
        .as_deref(),
        Some("/opt/node/bin:/usr/bin")
    );
    assert!(unix::parse_login_path(b"profile greeting only\n").is_none());
    assert!(unix::parse_login_path(b"__HEL_LOGIN_PATH__=\n").is_none());
    assert!(unix::parse_login_path(b"__HEL_LOGIN_PATH__=/bin\t/other\n").is_none());
    assert!(unix::parse_login_path(b"__HEL_LOGIN_PATH__=\xff\n").is_none());
}

#[test]
fn login_path_discovery_sources_the_profile_but_captures_only_path() {
    use hel::hel_targets::{BoundedProcessExecutor, CommandExecutor};

    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".profile"),
        b"printf 'profile chatter\\n'\nPATH=/profile-only/bin:$PATH\nexport PATH\n",
    )
    .unwrap();
    let mut command = unix::login_path_discovery_command();
    command
        .env
        .insert("HOME".into(), home.path().to_string_lossy().into_owned());

    let output = BoundedProcessExecutor::new(std::time::Duration::from_secs(5))
        .execute(&command)
        .unwrap();

    assert_eq!(output.status, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("profile chatter"));
    assert!(
        unix::parse_login_path(&output.stdout)
            .as_deref()
            .is_some_and(|path| path.starts_with("/profile-only/bin:"))
    );
}

/// Channel a served client uses to report that durable state became
/// unwritable. Most fixtures only need somewhere for that report to go.
fn fatal_reports() -> (mpsc::Sender<anyhow::Error>, mpsc::Receiver<anyhow::Error>) {
    mpsc::channel(1)
}

fn launch_config(profile_home: &str) -> WorkerLaunchConfig {
    WorkerLaunchConfig {
        session_id: SESSION_ID.into(),
        harness: HarnessKind::Codex,
        bridge_command: "codex-acp".into(),
        bridge_args: Vec::new(),
        harness_runtime: hel::hel_worker_launch::HarnessRuntimePolicy::Ambient,
        environment: BTreeMap::from([("CODEX_HOME".into(), profile_home.into())]),
        cwd: ".local/share/hel/workspaces/session/repo".into(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::Unconstrained,
    }
}

fn test_credentials() -> std::result::Result<CredentialEndpoint, String> {
    credential_endpoint(&launch_config("/profile"))
}

fn codex_credentials(last_refresh: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": { "access_token": "access", "refresh_token": "refresh" },
        "last_refresh": last_refresh,
    }))
    .unwrap()
}

fn install_request(bytes: &[u8]) -> RelayRequest {
    use base64::Engine as _;
    RelayRequest::InstallCredentials {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

fn skills_install_request(bytes: &[u8]) -> RelayRequest {
    use base64::Engine as _;
    RelayRequest::InstallSkills {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

fn skills_state_of(payload: RelayResponsePayload) -> hel::hel_skills::SkillsSyncState {
    let RelayResponsePayload::SkillsState {
        present,
        fingerprint,
    } = payload
    else {
        panic!("expected a skills state payload, got {payload:?}");
    };
    hel::hel_skills::SkillsSyncState {
        present,
        fingerprint,
    }
}

fn github_token_state_of(
    payload: RelayResponsePayload,
) -> hel::hel_credentials::GithubTokenSnapshot {
    let RelayResponsePayload::GithubTokenState {
        present,
        fingerprint,
    } = payload
    else {
        panic!("expected a GitHub token state payload, got {payload:?}");
    };
    hel::hel_credentials::GithubTokenSnapshot {
        present,
        fingerprint,
    }
}

#[test]
fn github_token_requests_install_and_remove_connection_only_state() {
    use base64::Engine as _;

    let home = tempfile::tempdir().unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();
    let absent = github_token_state_of(
        unix::apply_credential_request(&endpoint, &RelayRequest::GithubTokenState).unwrap(),
    );
    assert!(!absent.present);

    let installed = github_token_state_of(
        unix::apply_credential_request(
            &endpoint,
            &RelayRequest::InstallGithubToken {
                data: base64::engine::general_purpose::STANDARD.encode(b"fresh-token"),
            },
        )
        .unwrap(),
    );
    assert_eq!(
        installed,
        hel::hel_credentials::GithubTokenSnapshot::of("fresh-token")
    );
    let removed = github_token_state_of(
        unix::apply_credential_request(&endpoint, &RelayRequest::RemoveGithubToken).unwrap(),
    );
    assert!(!removed.present);
}

#[test]
fn github_cli_wrapper_reads_each_live_token_and_clears_stale_environment() {
    use std::os::unix::fs::PermissionsExt;

    let worker = tempfile::tempdir().unwrap();
    let real = tempfile::tempdir().unwrap();
    let real_bin = real.path().join("bin");
    std::fs::create_dir(&real_bin).unwrap();
    let real_gh = real_bin.join("gh");
    std::fs::write(
        &real_gh,
        b"#!/bin/sh\nprintf '%s|%s\\n' \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&real_gh, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut environment = BTreeMap::from([(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", real_bin.display()),
    )]);
    unix::configure_github_cli(worker.path(), &mut environment).unwrap();
    let token_path = worker.path().join("github-token");
    hel::hel_credentials::remove_github_token(&token_path).unwrap();

    let invoke = |expected_token: Option<&str>| {
        match expected_token {
            Some(token) => {
                hel::hel_credentials::write_github_token(&token_path, token.as_bytes()).unwrap();
            }
            None => hel::hel_credentials::remove_github_token(&token_path).unwrap(),
        }
        let mut command = std::process::Command::new(worker.path().join("bin/gh"));
        command
            .env_clear()
            .env("PATH", environment.get("PATH").unwrap())
            .env("GH_TOKEN", "stale-token")
            .env("GITHUB_TOKEN", "also-stale");
        let output = hel::hel_subprocess::run_with_input(&mut command, &[]).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    assert_eq!(invoke(Some("first-token")), "first-token|unset\n");
    assert_eq!(invoke(Some("rotated-token")), "rotated-token|unset\n");
    assert_eq!(invoke(None), "unset|unset\n");
}

#[test]
fn github_cli_wrapper_survives_harness_login_shells_and_git_helpers() {
    use std::os::unix::fs::PermissionsExt;

    let worker = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let real = tempfile::tempdir().unwrap();
    let real_bin = real.path().join("bin");
    std::fs::create_dir(&real_bin).unwrap();
    let real_gh = real_bin.join("gh");
    std::fs::write(
        &real_gh,
        br#"#!/bin/sh
if [ "${1-}" = auth ] && [ "${2-}" = git-credential ]; then
    cat >/dev/null
    if [ "${3-}" = get ]; then
        printf 'username=x-access-token\npassword=%s\n' "${GH_TOKEN-unset}"
    fi
else
    printf '%s|%s\n' "${GH_TOKEN-unset}" "${GITHUB_TOKEN-unset}"
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&real_gh, std::fs::Permissions::from_mode(0o700)).unwrap();

    let original_bash_env = home.path().join("original-bash-env");
    std::fs::write(&original_bash_env, b"export ORIGINAL_BASH_ENV=preserved\n").unwrap();
    std::fs::write(
        home.path().join(".bash_profile"),
        format!(
            "PATH={}:{}\nexport PATH\n",
            real_bin.display(),
            "/usr/bin:/bin"
        ),
    )
    .unwrap();

    let mut environment = BTreeMap::from([
        (
            "PATH".into(),
            format!("{}:/usr/bin:/bin", real_bin.display()),
        ),
        ("HOME".into(), home.path().to_string_lossy().into_owned()),
        (
            "BASH_ENV".into(),
            original_bash_env.to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "user.name".into()),
        ("GIT_CONFIG_VALUE_0".into(), "Harness User".into()),
    ]);
    unix::configure_github_cli(worker.path(), &mut environment).unwrap();
    let configured_once = environment.clone();
    unix::configure_github_cli(worker.path(), &mut environment).unwrap();
    assert_eq!(environment, configured_once);
    hel::hel_credentials::write_github_token(
        &worker.path().join("github-token"),
        b"synchronized-test-token",
    )
    .unwrap();

    let mut direct = std::process::Command::new("/bin/bash");
    direct
        .args([
            "-lc",
            "printf '%s|%s\\n' \"$(command -v gh)\" \"${ORIGINAL_BASH_ENV-unset}\"; gh auth status",
        ])
        .env_clear()
        .envs(&environment);
    let output = hel::hel_subprocess::run_with_input(&mut direct, &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}|preserved\nsynchronized-test-token|unset\n",
            worker.path().join("bin/gh").display()
        )
    );

    let mut git = std::process::Command::new("/bin/bash");
    git.args([
        "-lc",
        "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill",
    ])
    .env_clear()
    .envs(&environment);
    let output = hel::hel_subprocess::run_with_input(&mut git, &[]).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("username=x-access-token\n"), "{stdout}");
    assert!(
        stdout.contains("password=synchronized-test-token\n"),
        "{stdout}"
    );
}

#[test]
fn skills_state_reports_an_empty_home_then_a_synced_tree() {
    let home = tempfile::tempdir().unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();

    let empty = skills_state_of(
        unix::apply_credential_request(&endpoint, &RelayRequest::SkillsState).unwrap(),
    );
    assert!(!empty.present);

    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), b"review").unwrap();
    let state = skills_state_of(
        unix::apply_credential_request(&endpoint, &RelayRequest::SkillsState).unwrap(),
    );
    let expected = hel::hel_skills::collect_skills(HarnessKind::Codex, home.path()).unwrap();
    assert!(state.present);
    assert_eq!(state.fingerprint, expected.fingerprint());
}

#[test]
fn install_skills_replaces_the_session_tree_and_reports_the_new_state() {
    let canonical = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(canonical.path().join("skills/review")).unwrap();
    std::fs::write(canonical.path().join("skills/review/SKILL.md"), b"v1").unwrap();
    let archive = hel::hel_skills::collect_skills(HarnessKind::Codex, canonical.path()).unwrap();

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/stale")).unwrap();
    std::fs::write(home.path().join("skills/stale/SKILL.md"), b"old").unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();

    let state = skills_state_of(
        unix::apply_credential_request(&endpoint, &skills_install_request(&archive.encode()))
            .unwrap(),
    );
    assert_eq!(state, archive.state());
    assert_eq!(
        std::fs::read(home.path().join("skills/review/SKILL.md")).unwrap(),
        b"v1"
    );
    assert!(!home.path().join("skills/stale").exists());

    let empty = hel::hel_skills::SkillsArchive::default();
    let state = skills_state_of(
        unix::apply_credential_request(&endpoint, &skills_install_request(&empty.encode()))
            .unwrap(),
    );
    assert!(!state.present);
    assert!(!home.path().join("skills").exists());
}

#[test]
fn install_skills_rejects_garbage_and_leaves_the_tree_untouched() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills")).unwrap();
    std::fs::write(home.path().join("skills/keep.md"), b"keep").unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();

    let error = unix::apply_credential_request(
        &endpoint,
        &skills_install_request(b"garbage-with-enough-length"),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("magic"), "{error:#}");
    assert_eq!(
        std::fs::read(home.path().join("skills/keep.md")).unwrap(),
        b"keep"
    );
}

#[test]
fn non_credential_requests_are_not_served_by_the_home_handler() {
    let error = unix::apply_credential_request(&test_credentials().unwrap(), &RelayRequest::Status)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("credential, GitHub token, or skills"),
        "{error:#}"
    );
}

#[tokio::test]
async fn credential_exchange_stays_on_the_connection_and_out_of_relay_state() {
    use base64::Engine as _;

    let temp = tempfile::tempdir().unwrap();
    let relay_root = temp.path().join("relay");
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(&relay_root, SESSION_ID, "1.0.0").unwrap(),
    ));
    let endpoint = credential_endpoint(&launch_config(&temp.path().to_string_lossy())).unwrap();
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx,
        Ok(endpoint),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();
    let bytes = codex_credentials("2026-08-05T02:51:00.864587231Z");
    let request = RelayRequestEnvelope {
        request_id: "install-credentials".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: install_request(&bytes),
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();

    let response: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let RelayResponseBody::Ok {
        payload:
            RelayResponsePayload::CredentialState {
                present,
                fingerprint,
                freshness_epoch_ms,
            },
    } = response.body
    else {
        panic!("credential install failed: {:?}", response.body);
    };
    assert!(present);
    assert_eq!(
        fingerprint,
        hel::hel_credentials::credential_fingerprint(&bytes)
    );
    assert_eq!(freshness_epoch_ms, Some(1_785_898_260_864));
    assert_eq!(std::fs::read(temp.path().join("auth.json")).unwrap(), bytes);

    let read = RelayRequestEnvelope {
        request_id: "read-credentials".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::ReadCredentials,
    };
    let mut encoded = serde_json::to_vec(&read).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let RelayResponseBody::Ok {
        payload: RelayResponsePayload::Credentials { data },
    } = response.body
    else {
        panic!("credential read failed: {:?}", response.body);
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .unwrap(),
        bytes
    );

    {
        let relay = relay.lock().unwrap();
        assert_eq!(relay.latest_ordinal(), 0);
        assert!(
            relay
                .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
                .unwrap()
                .is_empty()
        );
        let persisted = std::fs::read_to_string(relay_root.join("relay-state.json")).unwrap();
        assert!(!persisted.contains(&request.request_id));
    }

    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn protocol_v1_cannot_respond_to_elicitation() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path().join("relay"), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (commands_tx, mut commands_rx) = mpsc::channel(1);
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay,
        wake_tx,
        test_credentials(),
        Some(commands_tx),
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();
    let request = RelayRequestEnvelope {
        request_id: "elicit-v1".into(),
        protocol_version: 1,
        request: RelayRequest::RespondElicitation {
            elicitation_id: "form-1".into(),
            response: hel::hel_elicitation::ElicitationResponse::Cancel,
        },
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();

    let response: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let RelayResponseBody::Error { error } = response.body else {
        panic!("protocol v1 must not answer elicitations, got {response:?}");
    };
    assert_eq!(error.code, RelayErrorCode::IncompatibleProtocol);
    assert!(commands_rx.try_recv().is_err());

    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn incompatible_protocol_cannot_read_or_mutate_credentials() {
    let home = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(home.path().join("relay"), SESSION_ID, "1.0.0").unwrap(),
    ));
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();
    let original = codex_credentials("2026-08-05T02:51:00Z");
    hel::hel_credentials::write_credential_file(endpoint.harness, &endpoint.marker, &original)
        .unwrap();
    let replacement = codex_credentials("2026-08-06T02:51:00Z");
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay,
        wake_tx,
        Ok(endpoint),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();

    for (request_id, request) in [
        ("read-with-v0", RelayRequest::ReadCredentials),
        ("install-with-v0", install_request(&replacement)),
    ] {
        let envelope = RelayRequestEnvelope {
            request_id: request_id.into(),
            protocol_version: 0,
            request,
        };
        let mut encoded = serde_json::to_vec(&envelope).unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
        let response: RelayResponseEnvelope =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(response.protocol_version, 0);
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    retryable: false,
                    ..
                }
            }
        ));
    }

    assert_eq!(
        std::fs::read(home.path().join("auth.json")).unwrap(),
        original
    );
    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();
}

#[test]
fn installed_credentials_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();
    unix::apply_credential_request(
        &endpoint,
        &install_request(&codex_credentials("2026-08-05T02:51:00Z")),
    )
    .unwrap();

    let mode = std::fs::metadata(home.path().join("auth.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn installing_kimi_credentials_uses_its_fixed_nested_marker() {
    let home = tempfile::tempdir().unwrap();
    let mut config = launch_config(&home.path().to_string_lossy());
    config.harness = HarnessKind::Kimi;
    config.environment = BTreeMap::from([(
        "KIMI_CODE_HOME".to_owned(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let endpoint = credential_endpoint(&config).unwrap();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "access_token": "access",
        "expires_at": 1_755_000_000,
    }))
    .unwrap();

    unix::apply_credential_request(&endpoint, &install_request(&bytes)).unwrap();

    assert_eq!(
        std::fs::read(home.path().join("credentials/kimi-code.json")).unwrap(),
        bytes
    );
}

#[test]
fn absent_reads_and_invalid_installs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();

    let error =
        unix::apply_credential_request(&endpoint, &RelayRequest::ReadCredentials).unwrap_err();
    assert!(format!("{error:#}").contains("no"), "{error:#}");

    let error =
        unix::apply_credential_request(&endpoint, &install_request(b"not json")).unwrap_err();
    assert!(format!("{error:#}").contains("JSON"), "{error:#}");

    let oversized = vec![b'a'; hel::hel_credentials::MAX_CREDENTIAL_BYTES + 1];
    let error =
        unix::apply_credential_request(&endpoint, &install_request(&oversized)).unwrap_err();
    assert!(format!("{error:#}").contains("limit"), "{error:#}");
}

#[test]
fn installing_over_a_symlink_leaves_the_link_target_untouched() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = home.path().join("stolen.json");
    std::fs::write(&elsewhere, b"{}").unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.path().join("auth.json")).unwrap();
    let endpoint = credential_endpoint(&launch_config(&home.path().to_string_lossy())).unwrap();

    let error = unix::apply_credential_request(
        &endpoint,
        &install_request(&codex_credentials("2026-08-05T02:51:00Z")),
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"{}");
}

#[test]
fn a_launch_config_without_a_harness_home_cannot_serve_credentials() {
    let mut config = launch_config("/profile");
    config.environment.clear();

    let error = credential_endpoint(&config).unwrap_err();

    assert!(error.contains("CODEX_HOME"), "{error}");
}

#[test]
fn launch_wires_require_the_new_baseline_shape() {
    let launch = launch_config("profile-home");
    let mut retired = serde_json::to_value(&launch).unwrap();
    retired
        .as_object_mut()
        .unwrap()
        .insert("recover_native_session".into(), serde_json::json!(true));
    assert!(serde_json::from_value::<WorkerLaunchConfig>(retired).is_err());

    let mut incomplete = serde_json::to_value(&launch).unwrap();
    incomplete.as_object_mut().unwrap().remove("bridge_args");
    assert!(serde_json::from_value::<WorkerLaunchConfig>(incomplete).is_err());

    let mut missing_policy = serde_json::to_value(&launch).unwrap();
    missing_policy
        .as_object_mut()
        .unwrap()
        .remove("execution_policy");
    assert!(serde_json::from_value::<WorkerLaunchConfig>(missing_policy).is_err());

    let mut legacy_policy = serde_json::to_value(&launch).unwrap();
    let legacy_policy_object = legacy_policy.as_object_mut().unwrap();
    legacy_policy_object.remove("execution_policy");
    legacy_policy_object.insert("force_unrestricted_mode".into(), serde_json::json!(true));
    let mut legacy_policy = serde_json::from_value::<WorkerLaunchConfig>(legacy_policy).unwrap();
    assert_eq!(
        legacy_policy.execution_policy,
        ExecutionPolicy::Unconstrained
    );
    assert!(!legacy_policy.environment.contains_key("INITIAL_AGENT_MODE"));
    super::enforce_execution_policy(&mut legacy_policy);
    assert_eq!(
        legacy_policy
            .environment
            .get("INITIAL_AGENT_MODE")
            .map(String::as_str),
        Some("agent-full-access")
    );

    let mut legacy = serde_json::to_value(&launch).unwrap();
    for field in ["additional_directories", "native_session_id"] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let parsed = serde_json::from_value::<WorkerLaunchConfig>(legacy).unwrap();
    assert!(parsed.additional_directories.is_empty());
    assert!(parsed.native_session_id.is_none());
    assert_eq!(parsed.execution_policy, ExecutionPolicy::Unconstrained);

    let supervisor = AcpSupervisorSpec {
        command: "codex-acp".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: ".".into(),
        harness_lease: None,
    };
    let mut incomplete = serde_json::to_value(&supervisor).unwrap();
    incomplete.as_object_mut().unwrap().remove("environment");
    assert!(serde_json::from_value::<AcpSupervisorSpec>(incomplete).is_err());
}

fn prompt(text: &str) -> RelayCommand {
    RelayCommand::Prompt {
        prompt: vec![ContentBlock::Text(TextContent::new(text))],
    }
}

fn submit(relay: &mut DurableRelay, command_id: &str, command: RelayCommand) {
    let response = relay.handle(RelayRequestEnvelope {
        request_id: format!("submit-{command_id}"),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: command_id.into(),
            command,
        },
    });
    assert!(
        matches!(
            &response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Accepted { .. }
            }
        ),
        "relay command was not accepted: {response:?}"
    );
}

/// An out-of-band ACP command: it shares the dispatch channel but never
/// reaches durable relay state.
fn elicitation_request() -> CommandRequest {
    let (resolved, _answer) = tokio::sync::oneshot::channel();
    CommandRequest::ResolveElicitation {
        elicitation_id: "out-of-band".into(),
        response: ElicitationResponse::Decline,
        resolved,
    }
}

/// Whether a runtime warning reached durable state. A coordinator that
/// parked on a command send stops draining runtime events, so this stays
/// false forever once that happens.
fn recorded_warning(relay: &Arc<Mutex<DurableRelay>>, message: &str) -> bool {
    relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap()
        .iter()
        .any(|event| {
            matches!(
                &event.observation,
                RelayObservation::Warning { message: recorded } if recorded == message
            )
        })
}

async fn next_command(commands: &mut mpsc::Receiver<CommandRequest>) -> CommandRequest {
    tokio::time::timeout(std::time::Duration::from_secs(5), commands.recv())
        .await
        .expect("the relay coordinator stopped dispatching commands")
        .expect("the ACP command channel closed")
}

/// Wait for a coordinator-side condition. Everything this guards against
/// wedges permanently, so a generous deadline still fails fast enough.
async fn wait_until(mut condition: impl FnMut() -> bool, blocked: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "{blocked}");
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// Line a test up with the coordinator without asserting anything: a
/// condition that never holds must fail the test on what it broke rather
/// than on the rendezvous.
async fn wait_for_rendezvous(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !condition() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

fn assert_prompt(command: CommandRequest, expected_id: &str, expected_text: &str) {
    let CommandRequest::Prompt { request_id, prompt } = command else {
        panic!("expected ACP prompt command");
    };
    assert_eq!(request_id, expected_id);
    assert!(matches!(
        prompt.as_slice(),
        [ContentBlock::Text(text)] if text.text == expected_text
    ));
}

#[tokio::test]
async fn offline_prompt_queue_runs_serially_without_a_controller() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(&mut durable, "prompt-1", prompt("first"));
    submit(&mut durable, "prompt-2", prompt("second"));
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    let first = tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_prompt(first, "prompt-1", "first");
    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-1".into(),
            stop_reason: "end_turn".into(),
        })
        .unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_prompt(second, "prompt-2", "second");
    assert_eq!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.as_str()),
        Some("prompt-2")
    );

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-2".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn config_during_a_prompt_waits_but_cancel_dispatches_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(&mut durable, "active-prompt", prompt("running"));
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    assert_prompt(command_rx.recv().await.unwrap(), "active-prompt", "running");

    submit(
        &mut relay.lock().unwrap(),
        "config-while-running",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "later".into(),
        },
    );
    submit(
        &mut relay.lock().unwrap(),
        "cancel-while-running",
        RelayCommand::Cancel,
    );
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::Cancel { request_id, .. } if request_id == "cancel-while-running"
    ));

    event_tx
        .send(RuntimeEvent::CancelApplied {
            request_id: "cancel-while-running".into(),
        })
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err(),
        "configuration dispatched before the active prompt finished"
    );

    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "active-prompt".into(),
            stop_reason: "cancelled".into(),
        })
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::SetConfig { request_id, .. } if request_id == "config-while-running"
    ));
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "config-while-running".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancel_turn_interrupts_a_running_prompt_without_steering_or_cutting_a_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(&mut durable, "active-prompt", prompt("running"));
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    assert_prompt(command_rx.recv().await.unwrap(), "active-prompt", "running");

    submit(
        &mut relay.lock().unwrap(),
        "pending-checkpoint",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit(
        &mut relay.lock().unwrap(),
        "queued-prompt",
        prompt("leave queued"),
    );
    submit(
        &mut relay.lock().unwrap(),
        "cancel-turn",
        RelayCommand::CancelTurn,
    );
    wake_tx.try_send(()).unwrap();

    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::Cancel {
            request_id,
            steering_prompt: None,
        } if request_id == "cancel-turn"
    ));
    event_tx
        .send(RuntimeEvent::CancelApplied {
            request_id: "cancel-turn".into(),
        })
        .unwrap();
    wait_until(
        || {
            let relay = relay.lock().unwrap();
            let state = relay.operational_state();
            let cancel_completed = relay
                .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
                .unwrap()
                .iter()
                .any(|event| {
                    matches!(
                        &event.observation,
                        RelayObservation::CommandCompleted {
                            command_id,
                            outcome: hel::hel_worker::RelayCommandOutcome::Cancelled,
                        } if command_id == "cancel-turn"
                    )
                });
            cancel_completed
                && state.active_prompt.is_some()
                && state
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.command_id == "queued-prompt")
                && state.checkpoint_barrier.is_none()
        },
        "CancelApplied did not complete without admitting the pending checkpoint",
    )
    .await;

    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "active-prompt".into(),
            stop_reason: "cancelled".into(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.active_prompt.is_none() && state.checkpoint_ready.is_some()
    })
    .await;
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .queued_prompts
            .iter()
            .any(|queued| queued.command_id == "queued-prompt")
    );

    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn proxy_transport_does_not_expire_an_idle_connection() {
    let (mut controller, proxy_client) = tokio::io::duplex(1024);
    let (proxy_relay, mut relay_peer) = tokio::io::duplex(1024);
    let (client_read, client_write) = tokio::io::split(proxy_client);
    let (relay_read, relay_write) = tokio::io::split(proxy_relay);
    let proxy = tokio::spawn(unix::forward_proxy_streams(
        client_read,
        client_write,
        relay_read,
        relay_write,
    ));

    controller.write_all(b"request").await.unwrap();
    let mut request = [0_u8; 7];
    relay_peer.read_exact(&mut request).await.unwrap();
    assert_eq!(&request, b"request");
    relay_peer.write_all(b"response").await.unwrap();
    let mut response = [0_u8; 8];
    controller.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, b"response");

    tokio::time::advance(std::time::Duration::from_secs(16 * 60)).await;
    tokio::task::yield_now().await;
    assert!(!proxy.is_finished(), "idle proxy connection expired");

    controller.write_all(b"another").await.unwrap();
    let mut request = [0_u8; 7];
    relay_peer.read_exact(&mut request).await.unwrap();
    assert_eq!(&request, b"another");
    relay_peer.write_all(b"answer!!").await.unwrap();
    let mut response = [0_u8; 8];
    controller.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, b"answer!!");

    drop(relay_peer);
    drop(controller);
    proxy.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn proxy_transport_expires_while_waiting_for_its_first_request() {
    let (_controller, proxy_client) = tokio::io::duplex(1024);
    let (proxy_relay, _relay_peer) = tokio::io::duplex(1024);
    let (client_read, client_write) = tokio::io::split(proxy_client);
    let (relay_read, relay_write) = tokio::io::split(proxy_relay);
    let proxy = tokio::spawn(unix::forward_proxy_streams(
        client_read,
        client_write,
        relay_read,
        relay_write,
    ));

    tokio::time::advance(unix::PROXY_INITIAL_INPUT_TIMEOUT).await;
    proxy
        .await
        .expect("proxy task stopped cleanly")
        .expect("pre-handshake proxy timeout is clean shutdown");
}

#[tokio::test]
async fn prompt_dispatch_preserves_the_complete_acp_content_vector() {
    let temp = tempfile::tempdir().unwrap();
    let content = vec![
        ContentBlock::Text(TextContent::new("first block")),
        ContentBlock::Text(TextContent::new("second block")),
    ];
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "prompt-blocks",
        RelayCommand::Prompt {
            prompt: content.clone(),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay, event_rx, wake_rx, command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    let CommandRequest::Prompt { request_id, prompt } = command_rx.recv().await.unwrap() else {
        panic!("expected ACP prompt command");
    };
    assert_eq!(request_id, "prompt-blocks");
    assert_eq!(prompt, content);

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-blocks".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn same_priority_queue_entries_dispatch_in_acceptance_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "z-accepted-first",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "first".into(),
        },
    );
    submit(
        &mut durable,
        "a-accepted-second",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "second".into(),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay, event_rx, wake_rx, command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    // Queue entries run one at a time, so the second change reaches ACP
    // only after the first is terminal.
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetConfig { request_id, .. }
            if request_id == "z-accepted-first"
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err(),
        "the queued change dispatched while an earlier one was in flight"
    );
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "z-accepted-first".into(),
            message: "advance the queue".into(),
        })
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::SetConfig { request_id, .. }
            if request_id == "a-accepted-second"
    ));

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "a-accepted-second".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn dispatch_batch_does_not_outgrow_the_bounded_acp_command_channel() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    // A prompt and the cancel that targets it are both claimable at once,
    // so only the bounded channel limits the durable batch.
    submit(&mut durable, "prompt-first", prompt("running"));
    submit(&mut durable, "cancel-second", RelayCommand::Cancel);
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay, event_rx, wake_rx, command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    assert_prompt(command_rx.recv().await.unwrap(), "prompt-first", "running");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv(),)
            .await
            .is_err(),
        "the second command was claimed beyond the channel's durable dispatch capacity"
    );

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-first".into(),
            message: "advance the bounded batch".into(),
        })
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::Cancel { request_id, .. } if request_id == "cancel-second"
    ));
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "cancel-second".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

/// The ACP command channel is shared: elicitation answers ride it beside
/// dispatched commands. Dispatch therefore holds
/// the transport capacity it claims against instead of counting free
/// slots, because a coordinator parked on a command send stops draining
/// ACP events, which stops the runtime that would have made room for the
/// command it is waiting on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_band_sends_cannot_park_the_dispatching_coordinator() {
    const COMMAND_CAPACITY: usize = 2;
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let out_of_band = command_tx.clone();
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    submit(
        &mut relay.lock().unwrap(),
        "prompt-warm-up",
        prompt("warm up"),
    );
    unix::wake_dispatch(&relay, &wake_tx).unwrap();
    assert_prompt(
        next_command(&mut command_rx).await,
        "prompt-warm-up",
        "warm up",
    );
    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-warm-up".into(),
            stop_reason: "end_turn".into(),
        })
        .unwrap();
    // Idle: the warm-up turn is durable and dispatch holds no capacity.
    wait_until(
        || {
            relay
                .lock()
                .unwrap()
                .operational_state()
                .active_prompt
                .is_none()
                && out_of_band.capacity() == COMMAND_CAPACITY
        },
        "the coordinator never finished the warm-up turn",
    )
    .await;

    // Hold the relay state lock to stop dispatch inside its claim: it has
    // already decided how much transport it may use, and nothing is
    // durable yet. That is the window an out-of-band send used to steal.
    let (claiming_tx, claiming_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let holder_relay = relay.clone();
    let holder = tokio::task::spawn_blocking(move || {
        let mut held = holder_relay.lock().expect("relay state lock poisoned");
        submit(&mut held, "prompt-batched", prompt("second turn"));
        submit(&mut held, "cancel-batched", RelayCommand::Cancel);
        claiming_tx
            .send(())
            .expect("the test stopped waiting for the claim");
        let _ = release_rx.blocking_recv();
    });
    claiming_rx.await.unwrap();
    // Wake dispatch by hand: the relay lock this test holds is exactly
    // what `wake_dispatch` would need to report a stopped coordinator.
    assert!(
        !matches!(
            wake_tx.try_send(()),
            Err(mpsc::error::TrySendError::Closed(()))
        ),
        "the relay coordinator stopped before the claim"
    );
    wait_for_rendezvous(|| out_of_band.capacity() == 0).await;

    // Out-of-band senders now compete for permits at reservation time.
    let out_of_band_attempts = [
        out_of_band.try_send(elicitation_request()),
        out_of_band.try_send(elicitation_request()),
    ];
    release_tx.send(()).unwrap();
    holder.await.unwrap();

    event_tx
        .send(RuntimeEvent::Warning {
            message: "still draining".into(),
        })
        .unwrap();
    wait_until(
        || recorded_warning(&relay, "still draining"),
        "an out-of-band send parked dispatch: the coordinator stopped draining ACP events",
    )
    .await;
    assert!(
        out_of_band_attempts
            .iter()
            .all(|attempt| matches!(attempt, Err(mpsc::error::TrySendError::Full(_)))),
        "dispatch must reserve transport capacity before it claims durable work"
    );
    assert_prompt(
        next_command(&mut command_rx).await,
        "prompt-batched",
        "second turn",
    );
    assert!(matches!(
        next_command(&mut command_rx).await,
        CommandRequest::Cancel { request_id, .. } if request_id == "cancel-batched"
    ));

    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn different_command_types_dispatch_in_acceptance_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "config-first",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "before-prompt".into(),
        },
    );
    submit(&mut durable, "prompt-second", prompt("after config"));
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay, event_rx, wake_rx, command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetConfig { request_id, .. } if request_id == "config-first"
    ));
    // The prompt waits for the configuration change accepted before it.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err(),
        "the prompt dispatched before the earlier configuration change finished"
    );
    event_tx
        .send(RuntimeEvent::ConfigApplied {
            request_id: "config-first".into(),
            key: "model".into(),
            value: "before-prompt".into(),
            config_options: Vec::new(),
        })
        .unwrap();
    assert_prompt(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "prompt-second",
        "after config",
    );

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-second".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn rejected_prompt_is_durable_and_does_not_stall_the_queue() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(&mut durable, "prompt-1", prompt("first"));
    submit(&mut durable, "prompt-2", prompt("second"));
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    assert_prompt(command_rx.recv().await.unwrap(), "prompt-1", "first");
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-1".into(),
            message: "agent rejected prompt".into(),
        })
        .unwrap();
    assert_prompt(command_rx.recv().await.unwrap(), "prompt-2", "second");
    let observations = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert!(observations.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandRejected {
            command_id,
            message,
            ..
        }
            if command_id == "prompt-1" && message == "agent rejected prompt"
    )));

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-2".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn set_session_mode_waits_for_idle_then_records_a_durable_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    submit(&mut relay.lock().unwrap(), "prompt-1", prompt("running"));
    wake_tx.try_send(()).unwrap();
    assert_prompt(command_rx.recv().await.unwrap(), "prompt-1", "running");

    submit(
        &mut relay.lock().unwrap(),
        "session-mode-1",
        RelayCommand::SetSessionMode {
            mode_id: "plan".into(),
        },
    );
    wake_tx.try_send(()).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err(),
        "the session mode dispatched before the active prompt finished"
    );

    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-1".into(),
            stop_reason: "end_turn".into(),
        })
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::SetSessionMode { request_id, mode_id }
            if request_id == "session-mode-1" && mode_id == "plan"
    ));
    event_tx
        .send(RuntimeEvent::SessionModeApplied {
            request_id: "session-mode-1".into(),
            mode_id: "plan".into(),
            config_options: Vec::new(),
            modes: None,
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.config.get("mode").map(String::as_str) == Some("plan")
    })
    .await;

    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_rejected_session_mode_change_reports_the_failure_and_leaves_the_mode_alone() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    submit(
        &mut relay.lock().unwrap(),
        "session-mode-1",
        RelayCommand::SetSessionMode {
            mode_id: "plan".into(),
        },
    );
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetSessionMode { request_id, mode_id }
            if request_id == "session-mode-1" && mode_id == "plan"
    ));
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "session-mode-1".into(),
            message: "set session mode to plan: no such mode".into(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| !state.config.contains_key("mode")).await;

    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn config_cancel_and_close_commands_have_durable_terminal_outcomes() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    submit(
        &mut relay.lock().unwrap(),
        "config-1",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "test-model".into(),
        },
    );
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetConfig { request_id, key, value }
            if request_id == "config-1" && key == "model" && value == "test-model"
    ));
    event_tx
        .send(RuntimeEvent::ConfigApplied {
            request_id: "config-1".into(),
            key: "model".into(),
            value: "test-model".into(),
            config_options: Vec::new(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.config.get("model").map(String::as_str) == Some("test-model")
    })
    .await;

    submit(&mut relay.lock().unwrap(), "prompt-1", prompt("running"));
    wake_tx.try_send(()).unwrap();
    assert_prompt(command_rx.recv().await.unwrap(), "prompt-1", "running");
    submit(&mut relay.lock().unwrap(), "cancel-1", RelayCommand::Cancel);
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::Cancel { request_id, .. } if request_id == "cancel-1"
    ));
    event_tx
        .send(RuntimeEvent::CancelApplied {
            request_id: "cancel-1".into(),
        })
        .unwrap();
    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-1".into(),
            stop_reason: "cancelled".into(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.execution == RelayExecutionState::Idle && state.active_prompt.is_none()
    })
    .await;

    submit(
        &mut relay.lock().unwrap(),
        "barrier-before-close",
        RelayCommand::BeginCheckpoint {
            reason: Some("close test".into()),
        },
    );
    wake_tx.try_send(()).unwrap();
    wait_for_relay_state(&relay, |state| state.checkpoint_ready.is_some()).await;
    let expected = relay
        .lock()
        .unwrap()
        .operational_state()
        .checkpoint_ready
        .unwrap();
    submit(
        &mut relay.lock().unwrap(),
        "close-01",
        RelayCommand::Close {
            barrier_command_id: "barrier-before-close".into(),
            expected,
        },
    );
    submit(
        &mut relay.lock().unwrap(),
        "complete-before-close",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "barrier-before-close".into(),
        },
    );
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::Close { request_id } if request_id == "close-01"
    ));
    event_tx
        .send(RuntimeEvent::CloseApplied {
            request_id: "close-01".into(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.execution == RelayExecutionState::Closed
    })
    .await;

    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
    let observations = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert!(observations.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandCompleted {
            command_id,
            outcome: hel::hel_worker::RelayCommandOutcome::Cancelled,
        } if command_id == "cancel-1"
    )));
    assert!(
        observations
            .iter()
            .any(|event| matches!(&event.observation, RelayObservation::Closed))
    );
}

async fn wait_for_relay_state(
    relay: &Arc<Mutex<DurableRelay>>,
    predicate: impl Fn(&hel::hel_worker::RelayOperationalState) -> bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if predicate(&relay.lock().unwrap().operational_state()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("relay state did not reach the expected condition");
}

#[test]
fn typed_acp_observations_are_journaled() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let mut in_flight = BTreeMap::new();
    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::Connected {
            agent_name: Some("test-agent".into()),
            agent_version: Some("1".into()),
            protocol_version: Some(ProtocolVersion::V1),
            capabilities: Some(Box::new(AgentCapabilities::default())),
            agent_info: Some(Implementation::new("test-agent", "1")),
        },
    )
    .unwrap();
    let update = SessionUpdate::AgentMessageChunk(
        ContentChunk::new(ContentBlock::Text(TextContent::new("hello"))).message_id("message-1"),
    );
    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::SessionUpdate {
            update: serde_json::to_value(update).unwrap(),
        },
    )
    .unwrap();

    let events = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.observation, RelayObservation::AgentInitialized { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.observation, RelayObservation::SessionUpdate { .. }))
    );
}

#[test]
fn a_harness_restart_gates_dispatch_until_the_session_is_configured_again() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let mut in_flight = BTreeMap::new();
    let mut session_configured = true;

    unix::record_runtime_event_and_track_configuration(
        &relay,
        &mut in_flight,
        RuntimeEvent::HarnessRestarting {
            message: "ACP bridge exited; reloading the native session".into(),
        },
        &mut session_configured,
    )
    .unwrap();
    assert!(
        !session_configured,
        "a restart must stop dispatch until the fresh bridge configures its session"
    );

    unix::record_runtime_event_and_track_configuration(
        &relay,
        &mut in_flight,
        RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        },
        &mut session_configured,
    )
    .unwrap();
    assert!(
        session_configured,
        "the fresh bridge's SessionConfigured must reopen dispatch"
    );
}

#[test]
fn harness_restarting_interrupts_in_flight_commands() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(&mut durable, "prompt-1", prompt("go"));
    let relay = Arc::new(Mutex::new(durable));
    let mut in_flight = BTreeMap::new();
    in_flight.insert("prompt-1".into(), prompt("go"));

    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::HarnessRestarting {
            message: "ACP bridge exited; reloading the native session".into(),
        },
    )
    .unwrap();

    assert!(
        in_flight.is_empty(),
        "the in-flight prompt must be interrupted"
    );
    let events = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::Warning { message } if message.contains("reloading the native session")
    )));
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandInterrupted { command_id, .. } if command_id == "prompt-1"
    )));
    let interrupted = events
        .iter()
        .position(|event| {
            matches!(
                event.observation,
                RelayObservation::CommandInterrupted { .. }
            )
        })
        .unwrap();
    let restarted = events
        .iter()
        .position(|event| matches!(event.observation, RelayObservation::SessionRestarted))
        .unwrap();
    assert!(interrupted < restarted);
}

#[test]
fn terminal_lifecycle_journals_a_fallback_tool_and_tail_capped_output() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let mut in_flight = BTreeMap::new();

    let ordinal_before_start = relay.lock().unwrap().operational_state().latest_ordinal;
    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::TerminalStarted {
            terminal_id: "term-1".into(),
            command: "cargo test".into(),
            started_at_ms: 1_000,
        },
    )
    .unwrap();
    let operational = relay.lock().unwrap().operational_state();
    assert_eq!(operational.latest_ordinal, ordinal_before_start + 1);
    assert_eq!(
        operational.active_agent_terminals,
        [hel::hel_worker::ActiveAgentTerminal {
            terminal_id: "term-1".into(),
            command: "cargo test".into(),
            started_at_ms: 1_000,
        }],
        "starting a terminal remains visible operationally"
    );
    let started_events = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert!(started_events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::SessionUpdate { update }
            if matches!(update.as_ref(), SessionUpdate::ToolCall(call)
                if hel::hel_acp::is_fallback_terminal_tool_call(call)
                    && call.title == "cargo test")
    )));

    // A build log the size of a real one: far past both the pipe buffer and
    // the journal cap, so only the tail can survive.
    let mut output = String::from("first line of the build log\n");
    while output.len() < 512 * 1024 {
        output.push_str("compiling something that says nothing useful\n");
    }
    output.push_str("error: the last line is the one that matters\n");
    let produced = output.len();

    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::TerminalClosed {
            terminal_id: "term-1".into(),
            output,
            truncated: false,
            exit_code: Some(101),
            signal: None,
        },
    )
    .unwrap();

    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .active_agent_terminals
            .is_empty(),
        "the provisional activity disappears as soon as the child exits"
    );

    let events = relay
        .lock()
        .unwrap()
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    let event = events
        .iter()
        .find(|event| matches!(event.observation, RelayObservation::TerminalOutput { .. }))
        .expect("the closed terminal is journaled");
    let RelayObservation::TerminalOutput {
        terminal_id,
        output,
        truncated,
        exit_code,
        signal,
    } = &event.observation
    else {
        unreachable!("matched a terminal observation above");
    };

    assert_eq!(terminal_id, "term-1");
    assert_eq!(*exit_code, Some(101));
    assert_eq!(*signal, None);
    assert!(*truncated, "dropping the head must be disclosed");
    assert!(
        output.ends_with("error: the last line is the one that matters\n"),
        "the tail of the output is what says how the command ended"
    );
    assert!(
        !output.contains("first line of the build log"),
        "the head is what gets dropped, not the tail"
    );
    assert!(output.contains("[mj dropped"), "the drop is disclosed");
    assert!(
        output.len() < produced,
        "the journal copy is capped below what the terminal produced"
    );
    assert!(
        serde_json::to_vec(event).unwrap().len() <= hel::hel_worker::RELAY_EVENT_BYTE_BUDGET,
        "the capped event fits a replay page without further clamping"
    );
}

#[test]
fn a_fast_terminal_cannot_be_resurrected_by_a_late_start_event() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let mut in_flight = BTreeMap::new();

    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::TerminalClosed {
            terminal_id: "term-1".into(),
            output: String::new(),
            truncated: false,
            exit_code: Some(0),
            signal: None,
        },
    )
    .unwrap();
    unix::record_runtime_event(
        &relay,
        &mut in_flight,
        RuntimeEvent::TerminalStarted {
            terminal_id: "term-1".into(),
            command: "true".into(),
            started_at_ms: 1_000,
        },
    )
    .unwrap();

    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .active_agent_terminals
            .is_empty()
    );
}

/// Claude Code re-invokes itself when a background task it started finishes.
/// The coordinator must hand a prompt typed during that turn straight to the
/// adapter, which queues it and answers it at the next turn boundary, while a
/// checkpoint barrier waits for the turn to settle.
#[tokio::test]
async fn a_self_started_turn_holds_a_barrier_but_not_a_prompt() {
    fn agent_output(text: &str, message_id: &str) -> RuntimeEvent {
        RuntimeEvent::SessionUpdate {
            update: serde_json::to_value(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                    .message_id(message_id),
            ))
            .unwrap(),
        }
    }

    /// The `usage_update` the Claude adapter sends when an SDK turn ends.
    fn settle_marker(origin: &str) -> RuntimeEvent {
        let mut usage = agent_client_protocol::schema::v1::UsageUpdate::new(10, 200);
        usage.meta = Some(
            serde_json::from_value(serde_json::json!({
                "_claude/origin": {"kind": origin},
            }))
            .unwrap(),
        );
        RuntimeEvent::SessionUpdate {
            update: serde_json::to_value(SessionUpdate::UsageUpdate(usage)).unwrap(),
        }
    }

    fn harness_turn_open(relay: &Arc<Mutex<DurableRelay>>) -> bool {
        relay
            .lock()
            .unwrap()
            .operational_state()
            .harness_turn
            .is_some()
    }

    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    durable.set_harness_turn_policy(hel::hel_worker::HarnessTurnPolicy::ClaudeAdapter);
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    // Agent output with no prompt in flight: the harness picked its work back
    // up on its own.
    event_tx
        .send(agent_output("the tests passed", "resumed-1"))
        .unwrap();
    wait_until(
        || harness_turn_open(&relay),
        "agent output at idle did not open a turn",
    )
    .await;
    assert_eq!(
        relay.lock().unwrap().operational_state().execution,
        RelayExecutionState::Running
    );

    // A prompt typed during that turn goes out at once.
    submit(
        &mut relay.lock().unwrap(),
        "prompt-mid-turn",
        prompt("also look at this"),
    );
    wake_tx.try_send(()).unwrap();
    assert_prompt(
        next_command(&mut command_rx).await,
        "prompt-mid-turn",
        "also look at this",
    );
    assert!(
        harness_turn_open(&relay),
        "dispatching a prompt does not end the turn the harness started"
    );

    // The prompt result is itself a turn boundary, so the turn it interrupted
    // is over. A fresh cycle opens the next one.
    event_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-mid-turn".into(),
            stop_reason: "end_turn".into(),
        })
        .unwrap();
    wait_until(
        || !harness_turn_open(&relay),
        "a prompt result did not settle the harness turn",
    )
    .await;
    event_tx
        .send(agent_output("and now the second task", "resumed-2"))
        .unwrap();
    wait_until(
        || harness_turn_open(&relay),
        "the second cycle did not open a turn",
    )
    .await;

    // A checkpoint barrier waits for that turn instead of cutting through it.
    submit(
        &mut relay.lock().unwrap(),
        "barrier-mid-turn",
        RelayCommand::BeginCheckpoint {
            reason: Some("recovery copy".into()),
        },
    );
    wake_tx.try_send(()).unwrap();
    // Long enough for the coordinator to run a claim cycle on that wake, so a
    // barrier it would have admitted is admitted before this assertion.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .checkpoint_barrier
            .is_none(),
        "a checkpoint barrier was admitted while the harness was working"
    );

    event_tx.send(settle_marker("task-notification")).unwrap();
    wait_until(
        || {
            relay
                .lock()
                .unwrap()
                .operational_state()
                .checkpoint_barrier
                .as_deref()
                == Some("barrier-mid-turn")
        },
        "the checkpoint barrier was not admitted once the harness turn settled",
    )
    .await;
    let state = relay.lock().unwrap().operational_state();
    assert!(state.harness_turn.is_none());
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(state.last_harness_turn_started_ordinal.is_some());

    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn checkpoint_waits_for_current_session_configuration_then_stays_local() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "checkpoint-1",
        RelayCommand::BeginCheckpoint {
            reason: Some("test".into()),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .checkpoint_barrier
            .is_none(),
        "checkpoint became ready before the current ACP session was configured"
    );
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if relay
                .lock()
                .unwrap()
                .operational_state()
                .checkpoint_barrier
                .as_deref()
                == Some("checkpoint-1")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err()
    );
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn checkpoint_waits_for_an_in_flight_config_command() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "config-before",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "test-model".into(),
        },
    );
    submit(
        &mut durable,
        "barrier-after-config",
        RelayCommand::BeginCheckpoint {
            reason: Some("after config".into()),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));

    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetConfig { request_id, .. } if request_id == "config-before"
    ));
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .checkpoint_barrier
            .is_none(),
        "checkpoint became ready before the ACP config command completed"
    );
    event_tx
        .send(RuntimeEvent::ConfigApplied {
            request_id: "config-before".into(),
            key: "model".into(),
            value: "test-model".into(),
            config_options: Vec::new(),
        })
        .unwrap();
    // ConfigApplied carries the ACP response's complete configuration.
    // The coordinator must durably materialize its SessionConfigured
    // observation before admitting the waiting checkpoint.
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let (state, configured_events) = {
                let relay = relay.lock().unwrap();
                let state = relay.operational_state();
                let configured_events = relay
                    .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
                    .unwrap()
                    .iter()
                    .filter(|event| {
                        matches!(
                            event.observation,
                            RelayObservation::SessionConfigured { .. }
                        )
                    })
                    .count();
                (state, configured_events)
            };
            if state.checkpoint_barrier.as_deref() == Some("barrier-after-config")
                && configured_events == 2
            {
                let ready = state.checkpoint_ready.expect("checkpoint is ready");
                assert_eq!(ready.ordinal, state.latest_ordinal);
                assert_eq!(ready.digest, state.latest_digest);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    relay
        .lock()
        .unwrap()
        .cancel_checkpoint_barrier_on_disconnect("barrier-after-config")
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

/// A checkpoint hands ACP dispatch back as soon as its archive exists, then
/// keeps using the same connection for the transfer. When that connection
/// finally drops, the released barrier is already terminal: cancelling it
/// again would push a spurious interruption into the transcript.
#[tokio::test]
async fn a_released_checkpoint_barrier_is_not_cancelled_when_its_connection_drops() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "released-barrier",
        RelayCommand::BeginCheckpoint {
            reason: Some("early release".into()),
        },
    );
    submit(
        &mut durable,
        "queued-during-transfer",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::Text(TextContent::new("later"))],
        },
    );
    assert_eq!(durable.claim_pending_commands(true).unwrap().len(), 1);
    durable.record_checkpoint_ready("released-barrier").unwrap();
    let floor_before = durable.operational_state().recovery_floor_ordinal;
    let relay = Arc::new(Mutex::new(durable));

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let served = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx,
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let request = RelayRequestEnvelope {
        request_id: "release-request".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "release-command".into(),
            command: RelayCommand::ReleaseCheckpoint {
                barrier_command_id: "released-barrier".into(),
            },
        },
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    client.write_all(&encoded).await.unwrap();
    let response = BufReader::new(&mut client)
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let response: RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    assert!(
        matches!(
            &response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Accepted { command_id, .. }
            } if command_id == "release-command"
        ),
        "relay did not accept the release: {:?}",
        response.body
    );

    // Dropping the connection is exactly what the worker treats as the
    // controller disappearing.
    drop(client);
    served.await.unwrap().unwrap();

    let mut relay = relay.lock().unwrap();
    let state = relay.operational_state();
    assert!(state.checkpoint_barrier.is_none());
    assert_eq!(state.recovery_floor_ordinal, floor_before);
    assert!(
        !relay
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .iter()
            .any(|event| matches!(
                &event.observation,
                RelayObservation::CommandInterrupted { command_id, .. }
                    if command_id == "released-barrier"
            )),
        "the dropped connection cancelled a barrier it had already released"
    );
    let next = relay.claim_pending_commands(true).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].command_id, "queued-during-transfer");
}

#[tokio::test]
async fn checkpoint_wake_records_already_queued_runtime_events_first() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, _command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| state.latest_ordinal >= 1).await;

    event_tx
        .send(RuntimeEvent::Warning {
            message: "queued before checkpoint wake".into(),
        })
        .unwrap();
    submit(
        &mut relay.lock().unwrap(),
        "checkpoint-after-queued-event",
        RelayCommand::BeginCheckpoint {
            reason: Some("ordering test".into()),
        },
    );
    wake_tx.try_send(()).unwrap();
    wait_for_relay_state(&relay, |state| state.checkpoint_ready.is_some()).await;

    {
        let relay_state = relay.lock().unwrap();
        let state = relay_state.operational_state();
        let ready = state.checkpoint_ready.expect("checkpoint is ready");
        assert_eq!(ready.ordinal, state.latest_ordinal);
        assert_eq!(ready.digest, state.latest_digest);
        let events = relay_state
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        let warning_ordinal = events
            .iter()
            .find_map(|event| match &event.observation {
                RelayObservation::Warning { message }
                    if message == "queued before checkpoint wake" =>
                {
                    Some(event.ordinal)
                }
                _ => None,
            })
            .expect("queued warning was recorded");
        assert!(warning_ordinal < ready.ordinal);
    }

    relay
        .lock()
        .unwrap()
        .cancel_checkpoint_barrier_on_disconnect("checkpoint-after-queued-event")
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn checkpoint_wake_is_not_starved_by_a_runtime_event_flood() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "checkpoint-during-event-flood",
        RelayCommand::BeginCheckpoint {
            reason: Some("event flood fairness".into()),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = mpsc::channel(8);
    event_tx
        .try_send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    for sequence in 0..7 {
        event_tx
            .try_send(RuntimeEvent::Warning {
                message: format!("queued flood event {sequence}"),
            })
            .unwrap();
    }
    let (wake_tx, wake_rx) = mpsc::channel(1);
    wake_tx.try_send(()).unwrap();
    let (command_tx, _command_rx) = mpsc::channel(1);
    let flood_tx = event_tx.clone();
    let flood = tokio::spawn(async move {
        let mut sequence = 7_u64;
        loop {
            if flood_tx
                .send(RuntimeEvent::Warning {
                    message: format!("live flood event {sequence}"),
                })
                .await
                .is_err()
            {
                return;
            }
            sequence += 1;
        }
    });
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if relay
                .lock()
                .unwrap()
                .operational_state()
                .checkpoint_ready
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime event flood starved the checkpoint wake");

    flood.abort();
    let _ = flood.await;
    relay
        .lock()
        .unwrap()
        .cancel_checkpoint_barrier_on_disconnect("checkpoint-during-event-flood")
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).await.unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn checkpoint_freezes_effectful_commands_submitted_after_the_barrier() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "barrier-before-config",
        RelayCommand::BeginCheckpoint {
            reason: Some("freeze later work".into()),
        },
    );
    submit(
        &mut durable,
        "config-after",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "later-model".into(),
        },
    );
    let relay = Arc::new(Mutex::new(durable));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));

    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.checkpoint_barrier.as_deref() == Some("barrier-before-config")
    })
    .await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), command_rx.recv())
            .await
            .is_err(),
        "a post-barrier ACP command was dispatched before checkpoint completion"
    );

    submit(
        &mut relay.lock().unwrap(),
        "complete-checkpoint",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "barrier-before-config".into(),
        },
    );
    wake_tx.try_send(()).unwrap();
    assert!(matches!(
        command_rx.recv().await.unwrap(),
        CommandRequest::SetConfig { request_id, .. } if request_id == "config-after"
    ));

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "config-after".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

/// A served request that cannot be persisted because teardown removed the
/// worker root has to stop the daemon; answering on from memory would keep
/// a closed session apparently alive.
#[tokio::test]
async fn a_removed_worker_root_reports_a_fatal_failure_to_the_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker-root");
    let mut durable = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    durable
        .record_observation(RelayObservation::Warning {
            message: "before teardown".into(),
        })
        .unwrap();
    let through = durable.latest_ordinal();
    let digest = durable.latest_digest().to_owned();
    let relay = Arc::new(Mutex::new(durable));
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (fatal_tx, mut fatal_rx) = fatal_reports();
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let served = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx,
        test_credentials(),
        None,
        fatal_tx,
    ));
    std::fs::remove_dir_all(&root).unwrap();

    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();
    let request = RelayRequestEnvelope {
        request_id: "acknowledge-after-teardown".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Acknowledge {
            through_ordinal: through,
            through_digest: digest,
        },
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(
        matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    ..
                }
            }
        ),
        "{:?}",
        response.body
    );

    let report = fatal_rx.recv().await.expect("the daemon must be told");
    assert!(format!("{report:#}").contains("was removed"), "{report:#}");
    assert!(
        !root.exists(),
        "serving a request recreated the worker root"
    );

    drop(writer);
    drop(lines);
    served.await.unwrap().unwrap();
}

#[tokio::test]
async fn client_disconnect_releases_checkpoint_and_runs_queued_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx.clone(),
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();

    let begin = RelayRequestEnvelope {
        request_id: "begin-checkpoint".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "checkpoint-1".into(),
            command: RelayCommand::BeginCheckpoint {
                reason: Some("test disconnect".into()),
            },
        },
    };
    let mut encoded = serde_json::to_vec(&begin).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = lines.next_line().await.unwrap().unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if relay
                .lock()
                .unwrap()
                .operational_state()
                .checkpoint_barrier
                .as_deref()
                == Some("checkpoint-1")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let queued_prompt = RelayRequestEnvelope {
        request_id: "queue-prompt".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "prompt-1".into(),
            command: prompt("runs after disconnect"),
        },
    };
    let mut encoded = serde_json::to_vec(&queued_prompt).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = lines.next_line().await.unwrap().unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    assert!(matches!(response.body, RelayResponseBody::Ok { .. }));

    writer.shutdown().await.unwrap();
    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();

    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .checkpoint_barrier
            .is_none()
    );
    assert_prompt(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "prompt-1",
        "runs after disconnect",
    );
    assert!(
        relay
            .lock()
            .unwrap()
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .iter()
            .any(|event| matches!(
                &event.observation,
                RelayObservation::CommandInterrupted { command_id, .. }
                    if command_id == "checkpoint-1"
            ))
    );

    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-1".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn relay_client_rejects_unknown_envelope_fields_without_disconnect() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay,
        wake_tx,
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
            .write_all(
                b"{\"request_id\":\"retired\",\"protocol_version\":1,\"controller_store_id\":\"old\",\"request\":{\"method\":\"status\"}}\n",
            )
            .await
            .unwrap();
    let rejected: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(
        rejected.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidRequest,
                ..
            }
        }
    ));

    let status = RelayRequestEnvelope {
        request_id: "valid-after-invalid".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Status,
    };
    let mut encoded = serde_json::to_vec(&status).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let accepted: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(
        accepted.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(_)
        }
    ));

    writer.shutdown().await.unwrap();
    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_unknown_relay_method_is_named_in_its_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay,
        wake_tx,
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
            .write_all(
                b"{\"request_id\":\"future\",\"protocol_version\":1,\"request\":{\"method\":\"subscribe\",\"params\":{\"after_seq\":0}}}\n",
            )
            .await
            .unwrap();
    let rejected: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(rejected.request_id, "future");
    let RelayResponseBody::Error { error } = rejected.body else {
        panic!("an unknown method must be rejected");
    };
    assert_eq!(error.code, RelayErrorCode::InvalidRequest);
    assert!(
        error.message.contains("does not support method") && error.message.contains("subscribe"),
        "{}",
        error.message
    );

    // The connection is a protocol boundary, not a casualty of the
    // rejection: the next request is still served.
    let status = RelayRequestEnvelope {
        request_id: "after-unknown-method".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Status,
    };
    let mut encoded = serde_json::to_vec(&status).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let accepted: RelayResponseEnvelope =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(matches!(
        accepted.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(_)
        }
    ));

    writer.shutdown().await.unwrap();
    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn disconnect_between_close_and_checkpoint_completion_dispatches_close() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx.clone(),
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let mut lines = BufReader::new(reader).lines();

    let begin = RelayRequestEnvelope {
        request_id: "begin-close-checkpoint".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "barrier-before-close".into(),
            command: RelayCommand::BeginCheckpoint {
                reason: Some("disconnect race".into()),
            },
        },
    };
    let mut encoded = serde_json::to_vec(&begin).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = lines.next_line().await.unwrap().unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
    wait_for_relay_state(&relay, |state| state.checkpoint_ready.is_some()).await;
    let expected = relay
        .lock()
        .unwrap()
        .operational_state()
        .checkpoint_ready
        .unwrap();

    let close = RelayRequestEnvelope {
        request_id: "queue-exact-close".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "close-after-barrier".into(),
            command: RelayCommand::Close {
                barrier_command_id: "barrier-before-close".into(),
                expected,
            },
        },
    };
    let mut encoded = serde_json::to_vec(&close).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = lines.next_line().await.unwrap().unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    assert!(matches!(response.body, RelayResponseBody::Ok { .. }));

    writer.shutdown().await.unwrap();
    drop(writer);
    drop(lines);
    server_task.await.unwrap().unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        CommandRequest::Close { request_id } if request_id == "close-after-barrier"
    ));
    event_tx
        .send(RuntimeEvent::CloseApplied {
            request_id: "close-after-barrier".into(),
        })
        .unwrap();
    wait_for_relay_state(&relay, |state| {
        state.execution == RelayExecutionState::Closed
    })
    .await;
    event_tx.send(RuntimeEvent::Stopped).unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn relay_v1_client_disconnect_does_not_own_command_execution() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (event_tx, event_rx) = runtime_event_channel();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let coordinator = tokio::spawn(unix::run_relay_coordinator(
        relay.clone(),
        event_rx,
        wake_rx,
        command_tx,
    ));
    event_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(unix::serve_client(
        server,
        relay,
        wake_tx.clone(),
        test_credentials(),
        None,
        fatal_reports().0,
    ));
    let (reader, mut writer) = client.into_split();
    let request = RelayRequestEnvelope {
        request_id: "submit".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "prompt-1".into(),
            command: prompt("continues offline"),
        },
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    drop(writer);
    drop(reader);
    // The response write may win or lose the close race; command
    // execution must not depend on it.
    let _ = server_task.await.unwrap();

    assert_prompt(
        tokio::time::timeout(std::time::Duration::from_secs(1), command_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "prompt-1",
        "continues offline",
    );
    event_tx
        .send(RuntimeEvent::CommandRejected {
            request_id: "prompt-1".into(),
            message: "test shutdown".into(),
        })
        .unwrap();
    drop(event_tx);
    drop(wake_tx);
    coordinator.await.unwrap().unwrap();
}

#[tokio::test]
async fn closed_relay_stays_attachable_after_the_acp_runtime_stops() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "barrier-before-close",
        RelayCommand::BeginCheckpoint {
            reason: Some("close test".into()),
        },
    );
    let claimed = durable.claim_pending_commands(true).unwrap();
    assert!(matches!(
        claimed.as_slice(),
        [hel::hel_worker::ClaimedRelayCommand {
            command_id,
            command: RelayCommand::BeginCheckpoint { .. },
            ..
        }] if command_id == "barrier-before-close"
    ));
    durable
        .record_checkpoint_ready("barrier-before-close")
        .unwrap();
    let expected = durable
        .operational_state()
        .checkpoint_ready
        .expect("checkpoint is ready");
    submit(
        &mut durable,
        "close-command",
        RelayCommand::Close {
            barrier_command_id: "barrier-before-close".into(),
            expected,
        },
    );
    submit(
        &mut durable,
        "complete-checkpoint",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "barrier-before-close".into(),
        },
    );
    let claimed = durable.claim_pending_commands(true).unwrap();
    assert!(matches!(
        claimed.as_slice(),
        [hel::hel_worker::ClaimedRelayCommand {
            command_id,
            command: RelayCommand::Close { .. },
            ..
        }] if command_id == "close-command"
    ));
    durable
        .record_command_completed(
            "close-command",
            hel::hel_worker::RelayCommandOutcome::Closed,
        )
        .unwrap();
    durable
        .record_observation(RelayObservation::Closed)
        .unwrap();
    let relay = Arc::new(Mutex::new(durable));

    let socket = temp.path().join("closed-relay.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    drop(wake_rx);
    let (fatal_tx, fatal_rx) = fatal_reports();
    let terminal = tokio::spawn(unix::serve_terminal_relay(
        listener,
        relay.clone(),
        wake_tx,
        test_credentials(),
        unix::ProjectMemoryEndpoint::default(),
        fatal_tx,
        fatal_rx,
    ));
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let request = RelayRequestEnvelope {
        request_id: "attach-closed-relay".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Attach {
            after_ordinal: 0,
            after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        },
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = BufReader::new(reader)
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let response: hel::hel_worker::RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    let state = match response.body {
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached { state, .. },
        } => state,
        body => panic!("closed relay did not accept attach: {body:?}"),
    };
    assert_eq!(state.execution, RelayExecutionState::Closed);

    writer.shutdown().await.unwrap();
    terminal.abort();
    assert!(terminal.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn daemon_restart_serves_a_closed_relay_without_starting_acp() {
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    submit(
        &mut durable,
        "barrier-before-restart-close",
        RelayCommand::BeginCheckpoint {
            reason: Some("closed restart test".into()),
        },
    );
    let barrier = durable.claim_pending_commands(true).unwrap();
    assert!(matches!(
        barrier.as_slice(),
        [hel::hel_worker::ClaimedRelayCommand {
            command_id,
            command: RelayCommand::BeginCheckpoint { .. },
            ..
        }] if command_id == "barrier-before-restart-close"
    ));
    durable
        .record_checkpoint_ready("barrier-before-restart-close")
        .unwrap();
    let expected = durable
        .operational_state()
        .checkpoint_ready
        .expect("checkpoint is ready");
    submit(
        &mut durable,
        "close-before-daemon-restart",
        RelayCommand::Close {
            barrier_command_id: "barrier-before-restart-close".into(),
            expected,
        },
    );
    submit(
        &mut durable,
        "complete-before-daemon-restart",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "barrier-before-restart-close".into(),
        },
    );
    let close = durable.claim_pending_commands(true).unwrap();
    assert!(matches!(
        close.as_slice(),
        [hel::hel_worker::ClaimedRelayCommand {
            command_id,
            command: RelayCommand::Close { .. },
            ..
        }] if command_id == "close-before-daemon-restart"
    ));
    durable
        .record_command_completed(
            "close-before-daemon-restart",
            hel::hel_worker::RelayCommandOutcome::Closed,
        )
        .unwrap();
    let closed_frontier = durable.latest_ordinal();
    drop(durable);

    let mut config = launch_config("profile-home-that-must-not-be-used");
    config.bridge_command = temp.path().join("missing-acp-bridge");
    config.cwd = temp.path().to_owned();
    let root = temp.path().to_owned();
    let daemon = tokio::spawn(unix::run_daemon(root.clone(), config));
    let stream = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match tokio::net::UnixStream::connect(root.join("control.sock")).await {
                Ok(stream) => break stream,
                Err(_) if daemon.is_finished() => {
                    panic!("closed relay daemon stopped during startup")
                }
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    let (reader, mut writer) = stream.into_split();
    let request = RelayRequestEnvelope {
        request_id: "status-after-closed-restart".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Status,
    };
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    writer.write_all(&encoded).await.unwrap();
    let response = BufReader::new(reader)
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let response: RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    let state = match response.body {
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(state),
        } => state,
        body => panic!("closed relay did not serve status after restart: {body:?}"),
    };
    assert_eq!(state.execution, RelayExecutionState::Closed);
    assert_eq!(state.latest_ordinal, closed_frontier);
    assert!(!root.join("acp-supervisor.json").exists());
    // Session teardown reads this file to stop the daemon before it
    // deletes the root out from under it.
    assert_eq!(
        std::fs::read_to_string(root.join(WORKER_PID_FILE))
            .unwrap()
            .trim(),
        std::process::id().to_string()
    );

    writer.shutdown().await.unwrap();
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn daemon_restart_records_one_marker_after_recovering_in_flight_work() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let mut relay = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    submit(&mut relay, "prompt-before-restart", prompt("keep working"));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    drop(relay);

    let mut config = launch_config(temp.path().join("profile").to_str().unwrap());
    config.cwd = temp.path().to_owned();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        unix::run_daemon(root.clone(), config),
    )
    .await
    .expect("the scripted worker child must stop")
    .expect_err("the test executable is not an ACP supervisor");
    assert!(!format!("{result:#}").is_empty());

    let reopened = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    let events = reopened
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    let interrupted = events
        .iter()
        .position(|event| {
            matches!(
                &event.observation,
                RelayObservation::CommandInterrupted { command_id, .. }
                    if command_id == "prompt-before-restart"
            )
        })
        .expect("restart recovery interrupts the old prompt");
    let markers = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.observation, RelayObservation::SessionRestarted))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1);
    assert!(interrupted < markers[0]);
    assert_eq!(
        reopened.operational_state().execution,
        RelayExecutionState::Idle
    );
}

#[tokio::test]
async fn first_daemon_start_does_not_record_a_restart_marker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let mut config = launch_config(temp.path().join("profile").to_str().unwrap());
    config.cwd = temp.path().to_owned();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        unix::run_daemon(root.clone(), config),
    )
    .await
    .expect("the scripted worker child must stop")
    .expect_err("the test executable is not an ACP supervisor");
    assert!(!format!("{result:#}").is_empty());

    let reopened = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    assert!(
        !reopened
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .iter()
            .any(|event| matches!(event.observation, RelayObservation::SessionRestarted))
    );
}

#[tokio::test]
async fn restored_relay_seed_records_a_restart_marker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    std::fs::write(
        hel::hel_worker::restored_relay_seed_path(&root),
        serde_json::to_vec(&hel::hel_worker::RestoredRelaySeed {
            event_frontier: 0,
            event_frontier_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            queued_prompts: Vec::new(),
        })
        .unwrap(),
    )
    .unwrap();
    let mut config = launch_config(temp.path().join("profile").to_str().unwrap());
    config.cwd = temp.path().to_owned();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        unix::run_daemon(root.clone(), config),
    )
    .await
    .expect("the scripted worker child must stop")
    .expect_err("the test executable is not an ACP supervisor");
    assert!(!format!("{result:#}").is_empty());

    let reopened = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    let markers = reopened
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.observation, RelayObservation::SessionRestarted))
        .count();
    assert_eq!(markers, 1);
}

#[tokio::test]
async fn acp_supervisor_notices_child_exit_while_a_descendant_holds_stdout_open() {
    let temp = tempfile::tempdir().unwrap();
    let spec = AcpSupervisorSpec {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 30 & exit 17".into()],
        environment: Default::default(),
        cwd: temp.path().to_owned(),
        harness_lease: None,
    };
    let (supervisor_stdin, _held_stdin) = tokio::io::duplex(64);

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        unix::run_acp_supervisor_with_streams(spec, supervisor_stdin, tokio::io::sink()),
    )
    .await
    .expect("the supervisor must observe the bridge process itself")
    .expect_err("the bridge exits unsuccessfully");
    assert!(
        format!("{error:#}").contains("exit status: 17"),
        "unexpected supervisor error: {error:#}"
    );
}

#[tokio::test]
async fn acp_bridge_keeps_the_configured_github_wrapper_and_drops_inherited_tokens() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("gh");
    std::fs::write(&wrapper, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let observed = temp.path().join("observed");
    let script = format!(
        "printf '%s\\n%s|%s\\n' \"$(command -v gh)\" \"${{GH_TOKEN-unset}}\" \"${{GITHUB_TOKEN-unset}}\" > {}",
        hel::hel_targets::posix_quote(&observed.to_string_lossy())
    );
    let spec = AcpSupervisorSpec {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        environment: BTreeMap::from([
            ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
            ("GH_TOKEN".into(), "stale-token".into()),
            ("GITHUB_TOKEN".into(), "also-stale".into()),
        ]),
        cwd: temp.path().to_owned(),
        harness_lease: None,
    };
    let (supervisor_stdin, _held_stdin) = tokio::io::duplex(64);

    unix::run_acp_supervisor_with_streams(spec, supervisor_stdin, tokio::io::sink())
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(observed).unwrap(),
        format!("{}\nunset|unset\n", wrapper.display())
    );
}

/// Opening the relay recovers its journal in place, so a second daemon has
/// to detect the live one before it can rewrite files the first is using.
#[tokio::test]
async fn a_live_worker_stops_a_second_daemon_before_it_touches_durable_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let mut relay = DurableRelay::open(&root, SESSION_ID, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "recorded by the live worker".into(),
        })
        .unwrap();
    drop(relay);

    // A torn tail is exactly what a startup recovery would rewrite.
    let journal = root
        .join(hel::hel_worker::RELAY_JOURNAL_DIR)
        .join("active.jsonl");
    let mut torn = std::fs::read(&journal).unwrap();
    torn.extend_from_slice(b"{\"ordinal\":2,\"truncated\"");
    std::fs::write(&journal, &torn).unwrap();
    let exit_record = root.join("worker-exit.json");
    std::fs::write(&exit_record, b"{\"reason\":\"earlier life\"}").unwrap();
    let _live = tokio::net::UnixListener::bind(root.join("control.sock")).unwrap();

    let error = unix::run_daemon(root.clone(), launch_config("/profile"))
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("a worker is already running"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        torn,
        "the second daemon recovered a live worker's journal"
    );
    assert!(
        exit_record.exists(),
        "the live worker's exit record was cleared"
    );
    assert!(
        !root.join(WORKER_PID_FILE).exists(),
        "the second daemon claimed a root it does not own"
    );
}

/// Teardown needs the PID of the daemon that owns the root right now, not
/// of one that died earlier.
#[test]
fn the_worker_pidfile_replaces_a_previous_daemons_claim() {
    let temp = tempfile::tempdir().unwrap();
    let pidfile = temp.path().join(WORKER_PID_FILE);
    std::fs::write(&pidfile, "999999999\n").unwrap();

    unix::write_worker_pidfile(temp.path(), std::process::id()).unwrap();

    assert_eq!(
        std::fs::read_to_string(&pidfile).unwrap().trim(),
        std::process::id().to_string()
    );
}

#[tokio::test]
async fn oversized_response_is_rejected_before_writing() {
    let (server, _client) = tokio::net::UnixStream::pair().unwrap();
    let (_, mut writer) = server.into_split();
    let response = RelayResponseEnvelope {
        request_id: "oversized".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        body: RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                message: "x".repeat(hel::hel_worker::MAX_FRAME_BYTES),
                retryable: false,
                detail: None,
            },
        },
    };

    let error = unix::write_response(&mut writer, &response)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("response frame is too large"));
}

#[tokio::test]
async fn request_line_limit_is_enforced_while_the_line_is_read() {
    let mut exact = BufReader::new(&b"12345678\nnext\n"[..]);
    assert_eq!(
        unix::read_bounded_line(&mut exact, 8).await.unwrap(),
        Some("12345678".into())
    );
    assert_eq!(
        unix::read_bounded_line(&mut exact, 8).await.unwrap(),
        Some("next".into())
    );

    let mut oversized = BufReader::with_capacity(4, &b"123456789-without-a-newline"[..]);
    let error = unix::read_bounded_line(&mut oversized, 8)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("request frame is too large"));
}

#[test]
fn repeated_dispatch_wakes_coalesce_to_one_pending_token() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap(),
    ));
    let (wake_tx, mut wake_rx) = mpsc::channel(1);

    for _ in 0..10_000 {
        unix::wake_dispatch(&relay, &wake_tx).unwrap();
    }
    assert_eq!(wake_rx.try_recv(), Ok(()));
    assert!(matches!(
        wake_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn coordinator_failure_aborts_peer_and_preserves_the_cause() {
    let mut peer = tokio::spawn(std::future::pending::<()>());
    let error = unix::abort_peer_and_return(
        &mut peer,
        anyhow::anyhow!("original coordinator failure"),
        "relay coordinator failed",
    )
    .await
    .unwrap_err();

    assert!(peer.is_finished());
    assert!(format!("{error:#}").contains("original coordinator failure"));
}

#[test]
fn resume_prefers_explicit_identity_then_relay_identity() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION_ID, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "native-relay".into(),
            resumed: false,
        })
        .unwrap();
    let mut config = launch_config("/var/lib/hel/profiles/session");
    assert_eq!(
        unix::select_resume_session(&config, &relay).as_deref(),
        Some("native-relay")
    );
    config.native_session_id = Some("native-explicit".into());
    assert_eq!(
        unix::select_resume_session(&config, &relay).as_deref(),
        Some("native-explicit")
    );
}

/// A history that seals several journal segments and overflows one replay
/// page, so an attach against it really reads and decompresses from disk.
fn paged_relay_history(root: &Path, events: usize) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION_ID, "1.0.0").unwrap();
    for index in 0..events {
        relay
            .record_observation(RelayObservation::Warning {
                message: format!("{index:04}:{}", "x".repeat(64 * 1024)),
            })
            .unwrap();
    }
    relay
}

fn attach_frame(request_id: &str, after_ordinal: u64, after_digest: &str) -> Vec<u8> {
    let mut encoded = serde_json::to_vec(&RelayRequestEnvelope {
        request_id: request_id.to_owned(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Attach {
            after_ordinal,
            after_digest: after_digest.to_owned(),
        },
    })
    .unwrap();
    encoded.push(b'\n');
    encoded
}

/// Records small observations through the relay lock until it is stopped,
/// reporting how many landed and the worst time it ever waited for the
/// lock. The wait is the contention this test is about; the append's own
/// fsync is deliberately left out of it.
struct LiveRecorder {
    stop: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<(u64, std::time::Duration)>,
}

impl LiveRecorder {
    fn start(relay: Arc<Mutex<DurableRelay>>) -> Self {
        // Sample the lock often enough that a page read cannot hide in the
        // gaps, but append rarely enough that the fsync per event neither
        // dominates the sampling nor grows the active segment far enough
        // to seal it under the reader.
        const SAMPLES_PER_RECORD: u64 = 8;
        const RECORD_LIMIT: u64 = 512;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recorder_stop = stop.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut samples = 0_u64;
            let mut recorded = 0_u64;
            let mut worst_wait = std::time::Duration::ZERO;
            while !recorder_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let waiting = std::time::Instant::now();
                let mut guard = relay.lock().expect("relay state lock poisoned");
                worst_wait = worst_wait.max(waiting.elapsed());
                if samples.is_multiple_of(SAMPLES_PER_RECORD) && recorded < RECORD_LIMIT {
                    guard
                        .record_observation(RelayObservation::Warning {
                            message: format!("live-{recorded}"),
                        })
                        .unwrap();
                    recorded += 1;
                }
                drop(guard);
                samples += 1;
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            (recorded, worst_wait)
        });
        Self { stop, task }
    }

    async fn stop(self) -> (u64, std::time::Duration) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.task.await.unwrap()
    }
}

/// Controllers are supposed to come and go without perturbing the session.
/// A catch-up over a long offline history reads page after page from disk
/// and decompresses sealed segments; doing that under the relay lock stops
/// the coordinator from recording ACP events, and once its bounded channel
/// fills the agent's turn stops with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaying_controller_does_not_stall_live_event_recording() {
    let temp = tempfile::tempdir().unwrap();
    let durable = paged_relay_history(temp.path(), 80);
    let frontier = durable.latest_ordinal();
    let relay = Arc::new(Mutex::new(durable));

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let (wake_tx, _wake_rx) = mpsc::channel(1);
    let served = tokio::spawn(unix::serve_client(
        server,
        relay.clone(),
        wake_tx,
        test_credentials(),
        None,
        fatal_reports().0,
    ));

    let recorder = LiveRecorder::start(relay.clone());
    client
        .write_all(&attach_frame("catch-up", 0, RELAY_EVENT_GENESIS_DIGEST))
        .await
        .unwrap();
    let response = BufReader::new(&mut client)
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let (recorded, worst_wait) = recorder.stop().await;

    let response: RelayResponseEnvelope = serde_json::from_str(&response).unwrap();
    let RelayResponseBody::Ok {
        payload:
            RelayResponsePayload::Attached {
                events,
                through_ordinal,
                ..
            },
    } = response.body
    else {
        panic!("catch-up attach failed: {:?}", response.body);
    };
    assert!(
        !events.is_empty() && through_ordinal < frontier,
        "this history should need several pages: {through_ordinal} of {frontier}"
    );

    // What one page costs to assemble, measured on this machine with
    // nothing else running. Serving it under the relay lock would make a
    // recorder wait about this long; serving it off the lock costs a
    // recorder only the plan capture.
    let page_cost = std::time::Instant::now();
    let _ = unix::handle_request(
        &relay,
        RelayRequestEnvelope {
            request_id: "page-cost".into(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            },
        },
    )
    .await
    .unwrap();
    let page_cost = page_cost.elapsed();
    assert!(
        page_cost >= std::time::Duration::from_millis(10),
        "a page assembled in {page_cost:?}; too cheap to say anything about contention"
    );
    assert!(recorded > 0, "no event was recorded during the replay");
    assert!(
        worst_wait * 3 < page_cost,
        "recording waited {worst_wait:?} for a page that takes {page_cost:?} to assemble: \
             the page is being read under the relay lock"
    );

    drop(client);
    served.await.unwrap().unwrap();
}

/// Sealing the active segment moves events into a file no captured replay
/// plan names, and a busy session seals one per megabyte of transcript. A
/// controller catching up through that must still get its pages: the relay
/// plans again against the journal as it now stands instead of handing the
/// controller a failure it did nothing to cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catch_up_completes_while_the_journal_keeps_sealing_under_it() {
    let temp = tempfile::tempdir().unwrap();
    let durable = paged_relay_history(temp.path(), 96);
    let target = durable.latest_ordinal();
    let generation = durable.journal_generation();
    let relay = Arc::new(Mutex::new(durable));

    // Events large enough to seal a segment every few appends, written as
    // fast as the durable path allows.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_relay = relay.clone();
    let writer_stop = stop.clone();
    let writer = tokio::task::spawn_blocking(move || {
        let mut written = 0_u64;
        while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) && written < 24 {
            writer_relay
                .lock()
                .expect("relay state lock poisoned")
                .record_observation(RelayObservation::Warning {
                    message: format!("{written:04}:{}", "y".repeat(256 * 1024)),
                })
                .unwrap();
            written += 1;
        }
        written
    });

    let mut cursor = (0_u64, RELAY_EVENT_GENESIS_DIGEST.to_owned());
    let mut pages = 0_usize;
    while cursor.0 < target {
        let body = unix::handle_request(
            &relay,
            RelayRequestEnvelope {
                request_id: format!("sealing-page-{pages}"),
                protocol_version: RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Attach {
                    after_ordinal: cursor.0,
                    after_digest: cursor.1.clone(),
                },
            },
        )
        .await
        .unwrap()
        .body;
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    through_ordinal,
                    through_digest,
                    ..
                },
        } = body
        else {
            panic!("page {pages} of a catch-up under a sealing journal failed: {body:?}");
        };
        assert!(
            through_ordinal > cursor.0,
            "page {pages} made no progress from event {}",
            cursor.0
        );
        cursor = (through_ordinal, through_digest);
        pages += 1;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.await.unwrap();

    assert!(
        relay
            .lock()
            .expect("relay state lock poisoned")
            .journal_generation()
            > generation,
        "the journal never sealed, so this run proved nothing"
    );
}

/// Recording throughput during a full catch-up, measured with the page
/// read inside and outside the relay lock so both policies are timed in
/// one process. Run with
/// `cargo test --lib
/// hel_worker_runtime::relay_tests::catch_up_recording_throughput
/// -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing measurement, not a behavior assertion"]
async fn catch_up_recording_throughput() {
    const HISTORY_EVENTS: usize = 200;

    async fn catch_up(defer_page_reads: bool) -> (u64, std::time::Duration, usize) {
        let temp = tempfile::tempdir().unwrap();
        let durable = paged_relay_history(temp.path(), HISTORY_EVENTS);
        let frontier = durable.latest_ordinal();
        let genesis = RELAY_EVENT_GENESIS_DIGEST.to_owned();
        let relay = Arc::new(Mutex::new(durable));
        let recorder = LiveRecorder::start(relay.clone());

        let started = std::time::Instant::now();
        let mut cursor = (0_u64, genesis);
        let mut pages = 0_usize;
        while cursor.0 < frontier {
            let envelope = RelayRequestEnvelope {
                request_id: format!("page-{pages}"),
                protocol_version: RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Attach {
                    after_ordinal: cursor.0,
                    after_digest: cursor.1.clone(),
                },
            };
            let body = if defer_page_reads {
                unix::handle_request(&relay, envelope).await.unwrap().body
            } else {
                // The coupled policy: the whole read runs under the lock.
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .handle(envelope)
                    .body
            };
            let RelayResponseBody::Ok {
                payload:
                    RelayResponsePayload::Attached {
                        through_ordinal,
                        through_digest,
                        ..
                    },
            } = body
            else {
                panic!("catch-up page {pages} (defer={defer_page_reads}) failed: {body:?}");
            };
            cursor = (through_ordinal, through_digest);
            pages += 1;
        }
        let elapsed = started.elapsed();
        let (recorded, _) = recorder.stop().await;
        (recorded, elapsed, pages)
    }

    for round in 0..2 {
        let (coupled, coupled_elapsed, pages) = catch_up(false).await;
        let (deferred, deferred_elapsed, _) = catch_up(true).await;
        println!(
            "round {round}: {pages} pages | under the lock {coupled} events in \
                 {coupled_elapsed:?} | off the lock {deferred} events in {deferred_elapsed:?}"
        );
    }
}

#[test]
fn relative_paths_are_resolved_before_the_bridge_changes_directory() {
    let mut config = launch_config(".local/share/hel/profiles/session");
    resolve_relative_harness_home(&mut config, Path::new("/home/ubuntu"));
    assert_eq!(
        config.environment["CODEX_HOME"],
        "/home/ubuntu/.local/share/hel/profiles/session"
    );
    assert_eq!(
        resolve_relative_worker_root(
            ".local/share/hel/workers/session".into(),
            Path::new("/home/ubuntu"),
        ),
        Path::new("/home/ubuntu/.local/share/hel/workers/session")
    );
}

#[test]
fn project_memory_connection_requests_round_trip_replica_and_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let memory = ProjectMemoryLaunchConfig {
        project_key: "project".into(),
        root: directory.path().join("replica"),
        baseline_root: directory.path().join("baseline"),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::Acp,
    };
    let baseline = hel::hel_project_memory::ProjectMemoryStore::new(&memory.baseline_root);
    baseline
        .install_snapshot(&hel::hel_project_memory::ProjectMemorySnapshot {
            files: BTreeMap::from([("/MEMORY.md".into(), "base".into())]),
        })
        .unwrap();
    let replica = hel::hel_project_memory::ProjectMemoryStore::new(&memory.root);
    replica
        .install_snapshot(&hel::hel_project_memory::ProjectMemorySnapshot {
            files: BTreeMap::from([("/MEMORY.md".into(), "changed".into())]),
        })
        .unwrap();

    let payload =
        unix::apply_project_memory_request(&memory, &RelayRequest::ProjectMemorySnapshot).unwrap();
    let RelayResponsePayload::ProjectMemorySnapshot {
        baseline: captured_baseline,
        replica: captured_replica,
    } = payload
    else {
        panic!("unexpected project-memory payload")
    };
    assert_eq!(captured_baseline.files["/MEMORY.md"], "base");
    assert_eq!(captured_replica.files["/MEMORY.md"], "changed");

    unix::apply_project_memory_request(
        &memory,
        &RelayRequest::InstallProjectMemorySnapshot {
            snapshot: captured_replica.clone(),
        },
    )
    .unwrap();
    assert_eq!(baseline.snapshot().unwrap(), captured_replica);
}

#[tokio::test]
async fn abandoned_project_memory_requests_do_not_overlap_blocking_io() {
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
    let first_gate = gate.clone();
    let first = tokio::spawn(async move {
        unix::run_serialized_project_memory_io(&first_gate, move || {
            first_started_tx.send(()).unwrap();
            release_first_rx.recv().unwrap();
            Ok(())
        })
        .await
    });
    first_started_rx.await.unwrap();

    // Model a controller dropping its request future after its transport
    // timeout. The blocking operation survives that cancellation.
    first.abort();
    let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
    let second_gate = gate.clone();
    let second = tokio::spawn(async move {
        unix::run_serialized_project_memory_io(&second_gate, move || {
            second_started_tx.send(()).unwrap();
            Ok(())
        })
        .await
    });
    let mut second_started_rx = std::pin::pin!(second_started_rx);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            &mut second_started_rx,
        )
        .await
        .is_err(),
        "abandoning a request released its in-flight filesystem permit"
    );

    release_first_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut second_started_rx)
        .await
        .expect("the queued request did not start after prior I/O completed")
        .unwrap();
    second.await.unwrap().unwrap().unwrap();
}
