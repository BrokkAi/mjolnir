//! Stand-ins the staging tests, and the tests of the crates above that
//! stage a worker, build on.

use super::*;
use std::path::Path;

pub fn stamped_worker(body: &[u8]) -> Vec<u8> {
    mj_core::test_hooks::enable_test_fixtures();
    let mut bytes = body.to_vec();
    bytes.extend_from_slice(mj_core::worker_build::worker_build_stamp().as_bytes());
    bytes
}

pub struct PodmanInstallFixture {
    pub _root: tempfile::TempDir,
    pub worker_binary: PathBuf,
    pub launch_config: PathBuf,
    pub ownership: PathBuf,
    pub profile_stage: PathBuf,
    pub locator: targets::TargetLocator,
}

pub fn podman_install_fixture() -> PodmanInstallFixture {
    let root = tempfile::tempdir().unwrap();
    let worker_binary = root.path().join("hel");
    std::fs::write(&worker_binary, stamped_worker(b"worker-binary-bytes")).unwrap();
    let launch_config = root.path().join("launch.json");
    std::fs::write(&launch_config, b"{}").unwrap();
    let ownership = root.path().join("ownership.json");
    std::fs::write(&ownership, b"{}").unwrap();
    let profile_stage = root.path().join("profile");
    std::fs::create_dir_all(&profile_stage).unwrap();
    PodmanInstallFixture {
        _root: root,
        worker_binary,
        launch_config,
        ownership,
        profile_stage,
        locator: targets::TargetLocator::SshPodman {
            borrowed_from: None,
            ssh: SshTarget {
                destination: "user@example.test".into(),
                ssh_args: Vec::new(),
            },
            container_id: "a".repeat(64),
            workspace_storage: Default::default(),
        },
    }
}

/// An OpenCode 2 database holding one session and one API-key login with
/// `key`, in the `credential` table layout OpenCode 2.1 creates, replacing any
/// file at `path`.
pub fn write_opencode_database(path: &Path, key: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let database = rusqlite::Connection::open(path).unwrap();
    database
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL,
                 value text NOT NULL, connector_id text, method_id text, active integer,
                 time_created integer NOT NULL, time_updated integer NOT NULL);
             CREATE TABLE session_v2 (id text PRIMARY KEY, title text);
             INSERT INTO session_v2 VALUES ('ses_fixture', 'native history');",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO credential VALUES ('cred_fixture', 'deepseek', 'DeepSeek', ?1, NULL, NULL, 1, 1, 1)",
            [format!(r#"{{"type":"key","key":"{key}"}}"#)],
        )
        .unwrap();
}

/// A login as each harness writes it. A higher `generation` is a fresher copy
/// of the same grant, which is what the credential sync orders copies by.
pub fn login_bytes(kind: HarnessKind, generation: i64) -> Vec<u8> {
    let login = match kind {
        HarnessKind::Codex => serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{generation}"),
                "refresh_token": format!("refresh-{generation}"),
            },
            "last_refresh": format!("2026-09-{:02}T09:00:00Z", 10 + generation),
        }),
        HarnessKind::Claude => serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("access-{generation}"),
                "refreshToken": format!("refresh-{generation}"),
                "expiresAt": 1_790_000_000_000_i64 + generation * 1000,
            }
        }),
        HarnessKind::Kimi => serde_json::json!({
            "access_token": format!("access-{generation}"),
            "refresh_token": format!("refresh-{generation}"),
            "expires_at": 1_790_000_000_i64 + generation,
        }),
        HarnessKind::Grok => serde_json::json!({
            "https://auth.x.ai::1": {
                "key": format!("access-{generation}"),
                "refresh_token": format!("refresh-{generation}"),
                "expires_at": format!("2026-10-{:02}T09:00:00Z", 10 + generation),
            }
        }),
        HarnessKind::Muse => serde_json::json!({
            "schema_version": 1,
            "providers": {"meta": {
                "access_token": format!("access-{generation}"),
                "refresh_token": format!("refresh-{generation}"),
                "expires_at": 1_790_000_000_i64 + generation,
                "mechanism": "oauth",
            }}
        }),
        // A staged OpenCode home carries its logins as `auth export` prints
        // them.
        HarnessKind::OpenCode => serde_json::json!([{
            "id": "cred_anthropic",
            "integrationID": "anthropic",
            "label": "OAuth",
            "active": true,
            "value": {
                "type": "oauth",
                "refresh": format!("refresh-{generation}"),
                "access": format!("access-{generation}"),
                "expires": 1_790_000_000_000_i64 + generation * 1000,
            },
        }]),
        HarnessKind::Antigravity => serde_json::json!({"token": {
            "access_token": format!("access-{generation}"), "refresh_token": "stable-refresh",
            "expiry": format!("2026-09-{:02}T09:00:00Z", 10 + generation),
        }}),
        HarnessKind::Dsh => panic!("DSH keeps no login; its key comes from DEEPSEEK_API_KEY"),
    };
    serde_json::to_vec(&login).unwrap()
}
