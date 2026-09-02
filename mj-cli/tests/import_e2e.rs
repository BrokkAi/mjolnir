//! Host-only test for importing and natively resuming a vanilla Claude rollout.
//!
//! It is intentionally ignored: it needs a signed-in `claude`, a container
//! runtime, and the agent-development image. Run it through
//! `scripts/test-import-e2e.sh` on the host.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use hel::hel_archive::read_archive_verified;
use hel::hel_config::{
    CONFIG_VERSION, ContainerTemplate, HarnessKind, HarnessProfile, HelConfig, ProjectBundle,
    ProjectRepository, TargetTemplate,
};
use hel::hel_state::{HelState, MaterializedSession, SessionState, TranscriptBody};
use hel::hel_worker::RelayCommand;
use mj_controller::hel_controller::Controller;
use mj_controller::hel_session_manager::{StandaloneSession, new_command_id};

fn mj_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_mj") {
        return PathBuf::from(path);
    }
    fallback_mj_binary(&std::env::current_exe().expect("resolve current test executable"))
}

fn fallback_mj_binary(test_executable: &Path) -> PathBuf {
    let mut path = test_executable.to_owned();
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.join("mj")
}

#[test]
fn import_e2e_defaults_resolve_to_mjolnir_paths() {
    assert_eq!(
        fallback_mj_binary(Path::new(
            "/workspace/target/x86_64-unknown-linux-musl/debug/deps/import_e2e-deadbeef"
        )),
        PathBuf::from("/workspace/target/x86_64-unknown-linux-musl/debug/mj")
    );

    // This test moved into `mj-cli` with the crate split, so the manifest
    // directory is now the crate, not the repository.
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory sits inside the repository root");
    for (runner, expected_repository) in [
        ("test-import-e2e.sh", "BrokkAi/mjolnir"),
        ("test-kimi-import-e2e.sh", "MoonshotAI/kimi-code"),
        ("test-grok-import-e2e.sh", "BrokkAi/mjolnir"),
        ("test-codex-import-e2e.sh", "BrokkAi/mjolnir"),
    ] {
        let script = fs::read_to_string(repository_root.join("scripts").join(runner)).unwrap();
        assert!(script.contains("/mjolnir/import-e2e"), "{runner}");
        assert!(script.contains("data/mjolnir"), "{runner}");
        assert!(script.contains("debug/mj"), "{runner}");
        assert!(
            script.contains("localhost/mjolnir/agent-dev:latest"),
            "{runner}"
        );
        assert!(script.contains(expected_repository), "{runner}");
        for legacy_default in ["BrokkAi/hel", "localhost/hel", "data/hel", "debug/hel"] {
            assert!(
                !script.contains(legacy_default),
                "{runner}: {legacy_default}"
            );
        }
    }
}

#[test]
#[ignore = "requires a signed-in Claude, Podman, and the Mjolnir agent-development image"]
fn imported_claude_session_resumes_natively() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(imported_claude_session_resumes_natively_async())
        .unwrap();
}

async fn imported_claude_session_resumes_natively_async() -> anyhow::Result<()> {
    let root = required_path("MJ_IMPORT_E2E_ROOT");
    let claude_home = required_path("CLAUDE_CONFIG_DIR");
    let repository = std::env::var("MJ_IMPORT_E2E_REPOSITORY")?;
    let image = std::env::var("MJ_IMPORT_E2E_IMAGE")?;
    let scratch = root.join("scratch");
    if scratch.exists() {
        fs::remove_dir_all(&scratch)?;
    }
    fs::create_dir_all(&root)?;
    run(Command::new("git")
        .args([
            "clone",
            &format!("https://github.com/{repository}.git"),
            "scratch",
        ])
        .current_dir(&root))?;
    run(Command::new("claude")
        .args([
            "-p",
            "append a line to notes.md and remember the word zanzibar",
        ])
        .current_dir(&scratch)
        .env("CLAUDE_CONFIG_DIR", &claude_home))?;

    let mut config = HelConfig {
        version: CONFIG_VERSION,
        newer_config_version: None,
        phone: Default::default(),
        review: Default::default(),
        profiles: BTreeMap::from([(
            "claude-e2e".into(),
            HarnessProfile {
                kind: HarnessKind::Claude,
                home: claude_home,
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        )]),
        bundles: BTreeMap::new(),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image,
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                },
            },
        )]),
    };
    config.bundles.insert(
        "scratch".into(),
        ProjectBundle {
            primary_repo: "scratch".into(),
            repositories: vec![ProjectRepository {
                id: "scratch".into(),
                github: Some(repository),
                local: None,
                destination: "scratch".into(),
                git_ref: None,
            }],
        },
    );
    config.save()?;
    HelState::default().save()?;

    let output = Command::new(mj_binary())
        .args(["import", "claude", "--latest", "--bundle", "scratch"])
        .env("CLAUDE_CONFIG_DIR", &config.profiles["claude-e2e"].home)
        .output()?;
    assert!(
        output.status.success(),
        "mj import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = HelState::load()?;
    let (session_id, imported) = state.sessions.iter().next().unwrap();
    assert_eq!(imported.state, SessionState::Stopped);
    let checkpoint = imported.checkpoint.as_ref().unwrap();
    let archive = read_archive_verified(&checkpoint.archive_path)?;
    assert_eq!(
        archive.manifest.session.native_session_id,
        imported.native_session_id.clone().unwrap()
    );
    let canonical = archive.canonical_session()?;
    assert!(!canonical.transcript.is_empty());

    let mut controller = Controller::load()?;
    let materialized = controller
        .resume_session_with_options(session_id, "claude-e2e", "podman", None, None)
        .await?;
    exercise_resumed_session(
        &controller,
        session_id,
        &materialized,
        ResumeExercise {
            expected_native_session_id: &archive.manifest.session.native_session_id,
            archived_frontier: canonical.event_frontier,
            prompt: "what word did I ask you to remember?",
            expected_reply: "zanzibar",
            attempts: 60,
            case_sensitive: false,
        },
    )
    .await?;
    controller.close_session(session_id).await?;
    let final_state = HelState::load()?;
    let checkpoint = final_state.sessions[session_id]
        .checkpoint
        .as_ref()
        .unwrap();
    read_archive_verified(&checkpoint.archive_path)?;
    Ok(())
}

#[test]
#[ignore = "requires a signed-in Kimi Code, Podman, and the Mjolnir agent-development image"]
fn imported_kimi_session_resumes_natively() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(imported_kimi_session_resumes_natively_async())
        .unwrap();
}

async fn imported_kimi_session_resumes_natively_async() -> anyhow::Result<()> {
    let kimi_home = required_path("KIMI_CODE_HOME");
    let native_session_id = std::env::var("MJ_IMPORT_E2E_KIMI_SESSION")?;
    let repository = std::env::var("MJ_IMPORT_E2E_KIMI_REPOSITORY")?;
    let image = std::env::var("MJ_IMPORT_E2E_IMAGE")?;
    let config = HelConfig {
        version: CONFIG_VERSION,
        newer_config_version: None,
        phone: Default::default(),
        review: Default::default(),
        profiles: BTreeMap::from([(
            "kimi-e2e".into(),
            HarnessProfile {
                kind: HarnessKind::Kimi,
                home: kimi_home.clone(),
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        )]),
        bundles: BTreeMap::from([(
            "kimi-source".into(),
            ProjectBundle {
                primary_repo: "kimi-source".into(),
                repositories: vec![ProjectRepository {
                    id: "kimi-source".into(),
                    github: Some(repository),
                    local: None,
                    destination: "kimi-source".into(),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image,
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                },
            },
        )]),
    };
    config.save()?;
    HelState::default().save()?;

    let output = Command::new(mj_binary())
        .args([
            "import",
            "kimi",
            "--session",
            &native_session_id,
            "--bundle",
            "kimi-source",
        ])
        .env("KIMI_CODE_HOME", &kimi_home)
        .output()?;
    assert!(
        output.status.success(),
        "mj import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = HelState::load()?;
    let (session_id, imported) = state.sessions.iter().next().unwrap();
    assert_eq!(imported.state, SessionState::Stopped);
    let checkpoint = imported.checkpoint.as_ref().unwrap();
    let archive = read_archive_verified(&checkpoint.archive_path)?;
    assert_eq!(
        archive.manifest.session.native_session_id,
        imported.native_session_id.clone().unwrap()
    );
    let canonical = archive.canonical_session()?;

    let mut controller = Controller::load()?;
    let materialized = controller
        .resume_session_with_options(session_id, "kimi-e2e", "podman", None, None)
        .await?;
    exercise_resumed_session(
        &controller,
        session_id,
        &materialized,
        ResumeExercise {
            expected_native_session_id: &archive.manifest.session.native_session_id,
            archived_frontier: canonical.event_frontier,
            prompt: "Reply exactly KIMI_NATIVE_RESUME_OK.",
            expected_reply: "KIMI_NATIVE_RESUME_OK",
            attempts: 120,
            case_sensitive: true,
        },
    )
    .await?;
    controller.close_session(session_id).await?;
    let final_state = HelState::load()?;
    read_archive_verified(
        &final_state.sessions[session_id]
            .checkpoint
            .as_ref()
            .unwrap()
            .archive_path,
    )?;
    Ok(())
}

#[test]
#[ignore = "requires a signed-in Grok Build, Podman, and the Mjolnir agent-development image"]
fn imported_grok_session_resumes_natively() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(imported_grok_session_resumes_natively_async())
        .unwrap();
}

async fn imported_grok_session_resumes_natively_async() -> anyhow::Result<()> {
    let grok_home = required_path("GROK_HOME");
    let native_session_id = std::env::var("MJ_IMPORT_E2E_GROK_SESSION")?;
    let repository = std::env::var("MJ_IMPORT_E2E_GROK_REPOSITORY")?;
    let image = std::env::var("MJ_IMPORT_E2E_IMAGE")?;
    let config = HelConfig {
        version: CONFIG_VERSION,
        newer_config_version: None,
        phone: Default::default(),
        review: Default::default(),
        profiles: BTreeMap::from([(
            "grok-e2e".into(),
            HarnessProfile {
                kind: HarnessKind::Grok,
                home: grok_home.clone(),
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        )]),
        bundles: BTreeMap::from([(
            "grok-source".into(),
            ProjectBundle {
                primary_repo: "grok-source".into(),
                repositories: vec![ProjectRepository {
                    id: "grok-source".into(),
                    github: Some(repository),
                    local: None,
                    destination: "grok-source".into(),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image,
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                },
            },
        )]),
    };
    config.save()?;
    HelState::default().save()?;

    let output = Command::new(mj_binary())
        .args([
            "import",
            "grok",
            "--session",
            &native_session_id,
            "--bundle",
            "grok-source",
        ])
        .env("GROK_HOME", &grok_home)
        .output()?;
    assert!(
        output.status.success(),
        "mj import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = HelState::load()?;
    let (session_id, imported) = state.sessions.iter().next().unwrap();
    assert_eq!(imported.state, SessionState::Stopped);
    let checkpoint = imported.checkpoint.as_ref().unwrap();
    let archive = read_archive_verified(&checkpoint.archive_path)?;
    assert_eq!(
        archive.manifest.session.native_session_id,
        imported.native_session_id.clone().unwrap()
    );
    let canonical = archive.canonical_session()?;

    let mut controller = Controller::load()?;
    let materialized = controller
        .resume_session_with_options(session_id, "grok-e2e", "podman", None, None)
        .await?;
    exercise_resumed_session(
        &controller,
        session_id,
        &materialized,
        ResumeExercise {
            expected_native_session_id: &archive.manifest.session.native_session_id,
            archived_frontier: canonical.event_frontier,
            prompt: "Reply exactly GROK_NATIVE_RESUME_OK.",
            expected_reply: "GROK_NATIVE_RESUME_OK",
            attempts: 120,
            case_sensitive: true,
        },
    )
    .await?;
    controller.close_session(session_id).await?;
    let final_state = HelState::load()?;
    read_archive_verified(
        &final_state.sessions[session_id]
            .checkpoint
            .as_ref()
            .unwrap()
            .archive_path,
    )?;
    Ok(())
}

struct ResumeExercise<'a> {
    expected_native_session_id: &'a str,
    archived_frontier: u64,
    prompt: &'a str,
    expected_reply: &'a str,
    attempts: usize,
    case_sensitive: bool,
}

async fn exercise_resumed_session(
    controller: &Controller,
    session_id: &str,
    materialized: &MaterializedSession,
    exercise: ResumeExercise<'_>,
) -> anyhow::Result<()> {
    let new_system_messages = materialized.transcript.iter().filter_map(|item| {
        if item.position <= exercise.archived_frontier {
            return None;
        }
        let TranscriptBody::System { text } = &item.body else {
            return None;
        };
        Some(text.as_str())
    });
    let messages = new_system_messages.collect::<Vec<_>>();
    assert!(
        messages.contains(&"harness session resumed"),
        "resume messages: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.starts_with("warning:")),
        "resume messages: {messages:?}"
    );

    let command = controller.reconnect_command(session_id)?;
    let mut relay = StandaloneSession::connect_command(&command, session_id).await?;
    let snapshot = relay.sync().await?;
    assert_eq!(
        snapshot.operational.native_session_id.as_deref(),
        Some(exercise.expected_native_session_id)
    );
    let before_prompt = snapshot.materialized.applied_event_ordinal;
    relay
        .submit(
            new_command_id("import-e2e-prompt")?,
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new(
                    exercise.prompt.to_owned(),
                ))],
            },
        )
        .await?;

    let mut reply = String::new();
    for _ in 0..exercise.attempts {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let snapshot = relay.sync().await?;
        reply = agent_text_after(&snapshot.materialized, before_prompt);
        let found = if exercise.case_sensitive {
            reply.contains(exercise.expected_reply)
        } else {
            reply
                .to_ascii_lowercase()
                .contains(&exercise.expected_reply.to_ascii_lowercase())
        };
        if found {
            return Ok(());
        }
    }
    anyhow::bail!(
        "reply did not contain {:?}: {reply}",
        exercise.expected_reply
    )
}

fn agent_text_after(materialized: &MaterializedSession, after_ordinal: u64) -> String {
    materialized
        .transcript
        .iter()
        .filter(|item| item.position > after_ordinal)
        .filter_map(|item| match &item.body {
            TranscriptBody::Agent { chunks, .. } => chunks
                .iter()
                .find_map(|chunk| chunk.pointer("/content/text")?.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is required")))
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "{:?} failed: {}",
        command,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
#[ignore = "requires a signed-in Codex, Podman, and the Mjolnir agent-development image"]
fn imported_codex_session_resumes_natively() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(imported_codex_session_resumes_natively_async())
        .unwrap();
}

async fn imported_codex_session_resumes_natively_async() -> anyhow::Result<()> {
    let codex_home = required_path("CODEX_HOME");
    let native_session_id = std::env::var("MJ_IMPORT_E2E_CODEX_SESSION")?;
    let repository = std::env::var("MJ_IMPORT_E2E_CODEX_REPOSITORY")?;
    let image = std::env::var("MJ_IMPORT_E2E_IMAGE")?;
    let config = HelConfig {
        version: CONFIG_VERSION,
        newer_config_version: None,
        phone: Default::default(),
        review: Default::default(),
        profiles: BTreeMap::from([(
            "codex-e2e".into(),
            HarnessProfile {
                kind: HarnessKind::Codex,
                home: codex_home.clone(),
                executable: None,
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        )]),
        bundles: BTreeMap::from([(
            "codex-source".into(),
            ProjectBundle {
                primary_repo: "codex-source".into(),
                repositories: vec![ProjectRepository {
                    id: "codex-source".into(),
                    github: Some(repository),
                    local: None,
                    destination: "codex-source".into(),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image,
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                },
            },
        )]),
    };
    config.save()?;
    HelState::default().save()?;

    let output = Command::new(mj_binary())
        .args([
            "import",
            "codex",
            "--session",
            &native_session_id,
            "--bundle",
            "codex-source",
        ])
        .env("CODEX_HOME", &codex_home)
        .output()?;
    assert!(
        output.status.success(),
        "mj import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = HelState::load()?;
    let (session_id, imported) = state.sessions.iter().next().unwrap();
    assert_eq!(imported.state, SessionState::Stopped);
    let checkpoint = imported.checkpoint.as_ref().unwrap();
    let archive = read_archive_verified(&checkpoint.archive_path)?;
    assert_eq!(
        archive.manifest.session.native_session_id,
        imported.native_session_id.clone().unwrap()
    );
    let canonical = archive.canonical_session()?;

    let mut controller = Controller::load()?;
    let materialized = controller
        .resume_session_with_options(session_id, "codex-e2e", "podman", None, None)
        .await?;
    exercise_resumed_session(
        &controller,
        session_id,
        &materialized,
        ResumeExercise {
            expected_native_session_id: &archive.manifest.session.native_session_id,
            archived_frontier: canonical.event_frontier,
            prompt: "Reply exactly CODEX_NATIVE_RESUME_OK.",
            expected_reply: "CODEX_NATIVE_RESUME_OK",
            attempts: 120,
            case_sensitive: true,
        },
    )
    .await?;
    controller.close_session(session_id).await?;
    let final_state = HelState::load()?;
    read_archive_verified(
        &final_state.sessions[session_id]
            .checkpoint
            .as_ref()
            .unwrap()
            .archive_path,
    )?;
    Ok(())
}
