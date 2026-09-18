use super::*;
use base64::Engine as _;
use mj_core::hex::lower_hex;

use std::io::{Read, Write};
use std::process::Command;
use std::sync::Mutex;

use crate::archive::{
    CanonicalExecutionState, CanonicalQueuedCommandKind, CanonicalQueuedPrompt,
    CanonicalSessionState, CanonicalTranscriptItem, GitOutput,
};

const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
const NATIVE: &str = "0190aabb-ccdd-7eef-9000-abcdef012345";

/// Runs real Git, but counts repair fetches and can make them fail without
/// reaching a network remote.
struct RecordingGit {
    fetch_failure: bool,
    fetches: Mutex<usize>,
}

impl RecordingGit {
    fn forwarding() -> Self {
        Self {
            fetch_failure: false,
            fetches: Mutex::new(0),
        }
    }

    fn with_fetch_failure() -> Self {
        Self {
            fetch_failure: true,
            fetches: Mutex::new(0),
        }
    }

    fn fetches(&self) -> usize {
        *self.fetches.lock().unwrap()
    }
}

impl GitCommandRunner for RecordingGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        if command
            .arguments
            .first()
            .is_some_and(|first| first == "fetch")
        {
            *self.fetches.lock().unwrap() += 1;
            if self.fetch_failure {
                return Ok(GitOutput {
                    status: 128,
                    stdout: Vec::new(),
                    stderr: b"fatal: could not read from remote repository".to_vec(),
                });
            }
        }
        SystemGit.run(repository, command)
    }
}

#[test]
fn empty_native_artifacts_allowed_only_for_unprompted_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let error = collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false)
        .unwrap_err()
        .to_string();
    assert_eq!(error, "no session artifacts found");
    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
    assert!(artifacts.is_empty());
}
#[test]
fn codex_collection_ignores_malformed_unrelated_rollouts() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions/2026/08/10");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("rollout-unrelated.jsonl"), b"{malformed\n").unwrap();
    let selected = sessions.join("rollout-renamed.jsonl");
    fs::write(
        &selected,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{NATIVE}\"}}}}\n"),
    )
    .unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(
        artifacts[0].relative_path,
        PathBuf::from("sessions/2026/08/10/rollout-renamed.jsonl")
    );
}

/// Unix milliseconds encoded in `NATIVE`, a UUIDv7.
const NATIVE_CREATED_MS: i64 = 0x0190_aabb_ccdd;
const OTHER_NATIVE: &str = "0190aabb-ccdd-7eef-9000-ffffffffffff";
const THIRD_NATIVE: &str = "0190aabb-ccdd-7eef-9000-eeeeeeeeeeee";

fn write_legacy_rollout(path: &Path, session_id: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{session_id}\"}}}}\n"),
    )
    .unwrap();
}

fn write_modern_rollout(path: &Path, id: &str, session_id: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
            path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"session_id\":\"{session_id}\"}}}}\n"
            ),
        )
        .unwrap();
}

#[test]
fn codex_collection_keeps_only_the_root_thread_from_a_session_tree() {
    let temp = tempfile::tempdir().unwrap();
    let archived_root = temp
        .path()
        .join("archived_sessions/rollout-renamed-root.jsonl");
    let child = temp.path().join("sessions/2026/08/10/rollout-child.jsonl");
    let descendant = temp
        .path()
        .join("sessions/2026/08/10/rollout-descendant.jsonl");
    write_modern_rollout(&archived_root, NATIVE, NATIVE);
    write_modern_rollout(&child, OTHER_NATIVE, NATIVE);
    write_modern_rollout(&descendant, THIRD_NATIVE, NATIVE);

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();

    assert_eq!(
        artifacts
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![PathBuf::from(
            "archived_sessions/rollout-renamed-root.jsonl"
        )]
    );
}

#[test]
fn codex_modern_rollout_never_falls_back_to_the_session_tree_id() {
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("sessions/2026/08/10/rollout-child.jsonl");
    write_modern_rollout(&child, OTHER_NATIVE, NATIVE);

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();

    assert!(artifacts.is_empty());
}

fn set_modified_ms(path: &Path, millis: i64) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(millis as u64))
        .unwrap();
}

#[test]
fn uuid_v7_timestamp_decodes_only_version_seven_uuids() {
    assert_eq!(uuid_v7_timestamp_ms(NATIVE), Some(NATIVE_CREATED_MS));
    assert_eq!(uuid_v7_timestamp_ms(SESSION), Some(0x018f_9dd2_a3b4));
    assert_eq!(
        uuid_v7_timestamp_ms("0190aabb-ccdd-4eef-9000-abcdef012345"),
        None
    );
    for malformed in [
        "",
        "not-a-uuid",
        "0190aabb-ccdd-7eef-9000-abcdef01234",
        "0190aabb-ccdd-7eef-9000-abcdef012345-extra",
        "0190AABB-CCDD-7EEF-9000-ABCDEF012345",
        "0190aabbccdd7eef9000abcdef012345",
        "0190aabg-ccdd-7eef-9000-abcdef012345",
    ] {
        assert_eq!(uuid_v7_timestamp_ms(malformed), None, "{malformed}");
    }
}

#[test]
fn codex_content_probe_skips_rollouts_older_than_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let rollout = temp.path().join("sessions/2026/08/10/rollout-fork.jsonl");
    write_legacy_rollout(&rollout, NATIVE);

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 1);

    // Three days before the session's own UUIDv7 creation time, so past the
    // 48 hour skew slack the floor allows.
    set_modified_ms(&rollout, NATIVE_CREATED_MS - 72 * 3600 * 1000);
    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
    assert!(artifacts.is_empty());
}

#[test]
fn codex_name_matched_rollout_is_collected_whatever_its_mtime() {
    let temp = tempfile::tempdir().unwrap();
    let rollout = temp
        .path()
        .join("sessions/2026/08/10")
        .join(format!("rollout-{NATIVE}.jsonl"));
    write_legacy_rollout(&rollout, OTHER_NATIVE);
    set_modified_ms(&rollout, 1_000_000_000_000);

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 1);
}

#[test]
fn codex_scan_cache_makes_a_negative_probe_verdict_permanent() {
    let temp = tempfile::tempdir().unwrap();
    let foreign = temp
        .path()
        .join("sessions/2026/08/10/rollout-foreign.jsonl");
    write_legacy_rollout(&foreign, OTHER_NATIVE);
    let mut cache = CodexScanCache::empty(NATIVE);

    let artifacts = collect_native_artifacts_cached(
        HarnessKind::Codex,
        temp.path(),
        NATIVE,
        true,
        Some(&mut cache),
    )
    .unwrap();
    assert!(artifacts.is_empty());
    assert!(
        cache
            .not_ours
            .contains(Path::new("sessions/2026/08/10/rollout-foreign.jsonl"))
    );

    write_legacy_rollout(&foreign, NATIVE);
    let later = temp.path().join("sessions/2026/08/10/rollout-later.jsonl");
    write_legacy_rollout(&later, NATIVE);
    let artifacts = collect_native_artifacts_cached(
        HarnessKind::Codex,
        temp.path(),
        NATIVE,
        true,
        Some(&mut cache),
    )
    .unwrap();
    assert_eq!(
        artifacts
            .iter()
            .map(|artifact| artifact.relative_path.clone())
            .collect::<Vec<_>>(),
        vec![PathBuf::from("sessions/2026/08/10/rollout-later.jsonl")]
    );
}

#[test]
fn codex_probe_stops_at_the_first_session_meta_header() {
    let temp = tempfile::tempdir().unwrap();
    let rollout = temp.path().join("sessions/2026/08/10/rollout-fork.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(
        &rollout,
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{OTHER_NATIVE}\"}}}}\n\
                 {{\"type\":\"event_msg\"}}\n\
                 {{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{NATIVE}\"}}}}\n\
                 {{malformed\n"
        ),
    )
    .unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Codex, temp.path(), NATIVE, true).unwrap();
    assert!(artifacts.is_empty());
}

#[test]
fn codex_export_rebuilds_a_corrupt_scan_cache_and_records_verdicts() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let cache_path = spec.relay_root.join(CODEX_SCAN_CACHE_FILE);
    fs::write(&cache_path, b"{not json").unwrap();
    write_legacy_rollout(
        &spec
            .harness_home
            .join("sessions/2026/08/09/rollout-x.jsonl"),
        OTHER_NATIVE,
    );

    export_checkpoint(&spec).unwrap();

    let cache: CodexScanCache = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(cache.session_id, NATIVE);
    assert_eq!(
        cache.not_ours,
        BTreeSet::from([PathBuf::from("sessions/2026/08/09/rollout-x.jsonl")])
    );
    assert_eq!(
        load_codex_scan_cache(&spec.relay_root, OTHER_NATIVE)
            .not_ours
            .len(),
        0
    );
}

#[test]
fn prompt_detection_reads_the_materialized_transcript() {
    let mut snapshot = CanonicalSessionSnapshot {
        event_frontier: 1,
        event_frontier_digest: "a".repeat(64),
        session: CanonicalSessionState {
            execution: CanonicalExecutionState::Idle,
            last_activity_at_ms: Some(1),
            session_title: None,
            configuration: Default::default(),
        },
        transcript: Vec::new(),
        queued_prompts: Vec::new(),
    };
    assert!(!canonical_session_contains_prompt(&snapshot));
    snapshot.transcript.push(CanonicalTranscriptItem {
        stable_id: "user-1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 1,
        last_changed_at_ms: 1,
        body: CanonicalTranscriptBody::User {
            content: vec![json!({"type": "text", "text": "hi"})],
        },
    });
    assert!(canonical_session_contains_prompt(&snapshot));
}

#[test]
fn native_allowlist_excludes_credentials_and_other_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let session = temp
        .path()
        .join("sessions/workspace")
        .join(format!("session_{NATIVE}"));
    fs::create_dir_all(session.join("agents/main")).unwrap();
    fs::write(session.join("state.json"), b"state").unwrap();
    fs::write(session.join("agents/main/wire.jsonl"), b"events").unwrap();
    fs::create_dir_all(session.join("agents/main/tasks/bash-noise")).unwrap();
    fs::write(
        session.join("agents/main/tasks/bash-noise/output.log"),
        b"tool output",
    )
    .unwrap();
    fs::write(
        session.join("agents/main/wire.jsonl.bak-before-edit"),
        b"backup",
    )
    .unwrap();
    fs::create_dir_all(session.join("logs")).unwrap();
    fs::write(session.join("logs/kimi-code.log"), b"log").unwrap();
    fs::write(session.join("credentials.json"), b"secret").unwrap();
    let other = temp.path().join("sessions/workspace/other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("state.json"), b"other").unwrap();
    fs::write(
            temp.path().join("workspaces.json"),
            r#"{"version":1,"deleted_workspace_ids":[],"workspaces":{"workspace":{"root":"/work/app","name":"app"}}}"#,
        )
        .unwrap();
    fs::write(
            temp.path().join("session_index.jsonl"),
            format!(
                "{{\"sessionId\":\"{NATIVE}\",\"workDir\":\"/work/app\",\"sessionDir\":\"/sessions/workspace/session_{NATIVE}\"}}\n{{\"sessionId\":\"other\"}}\n"
            ),
        )
        .unwrap();
    let artifacts =
        collect_native_artifacts(HarnessKind::Kimi, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 4);
    assert!(
        artifacts
            .iter()
            .any(|artifact| { artifact.relative_path.as_path() == Path::new("workspaces.json") })
    );
    let index = artifacts
        .iter()
        .find(|artifact| artifact.relative_path.as_path() == Path::new("session_index.jsonl"))
        .unwrap();
    assert!(std::str::from_utf8(&index.data).unwrap().contains(NATIVE));
    assert!(!std::str::from_utf8(&index.data).unwrap().contains("other"));
    assert!(artifacts.iter().all(|artifact| {
        !artifact
            .relative_path
            .to_string_lossy()
            .contains("credentials")
    }));
    assert!(artifacts.iter().all(|artifact| {
        let path = artifact.relative_path.to_string_lossy();
        !path.contains("tasks") && !path.contains(".bak") && !path.contains("logs")
    }));
}

#[test]
fn grok_allowlist_collects_one_session_directory_without_runtime_state() {
    const NATIVE: &str = "01a00c3a-553f-71e0-95ab-aa04396d3ad7";
    let temp = tempfile::tempdir().unwrap();
    let session = temp.path().join("sessions/%2Fhome%2Fme%2Fapp").join(NATIVE);
    fs::create_dir_all(&session).unwrap();
    for name in [
        "chat_history.jsonl",
        "events.jsonl",
        "prompt_context.json",
        "summary.json",
        "system_prompt.txt",
    ] {
        fs::write(session.join(name), b"payload").unwrap();
    }
    fs::write(session.join("summary.json.lock"), b"").unwrap();
    let other = temp
        .path()
        .join("sessions/%2Fhome%2Fme%2Fapp/01a00c40-55c5-78b0-85c8-ac1b99985fd0");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("summary.json"), b"other").unwrap();
    fs::write(temp.path().join("sessions/session_search.sqlite"), b"index").unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Grok, temp.path(), NATIVE, false).unwrap();

    let paths = artifacts
        .iter()
        .map(|artifact| artifact.relative_path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "chat_history.jsonl",
            "events.jsonl",
            "prompt_context.json",
            "summary.json",
            "system_prompt.txt",
        ]
        .map(|name| format!("sessions/%2Fhome%2Fme%2Fapp/{NATIVE}/{name}"))
    );
}

#[test]
fn muse_checkpoint_keeps_selected_child_streams_without_credentials_or_other_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".data/muse/sessions/2026/09/07");
    let selected = root.join(NATIVE);
    fs::create_dir_all(selected.join("subagent/child")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(selected.join("session.jsonl"), vec![b'x'; 128 * 1024]).unwrap();
    fs::write(selected.join("session.peer-history.sqlite3"), b"derived").unwrap();
    fs::write(selected.join("runtime.lock"), b"ephemeral").unwrap();
    fs::write(
        selected.join("subagent/child/session.jsonl"),
        b"child history",
    )
    .unwrap();
    fs::write(root.join("other/session.jsonl"), b"unrelated").unwrap();
    fs::write(temp.path().join("auth.json"), b"secret").unwrap();
    let artifacts =
        collect_native_artifacts(HarnessKind::Muse, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 2);
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact.data.len() == 128 * 1024)
    );
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact.data == b"child history")
    );
    let restored = tempfile::tempdir().unwrap();
    for artifact in artifacts {
        let path = restored.path().join(&artifact.relative_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &artifact.data).unwrap();
    }
    assert_eq!(
        collect_native_artifacts(HarnessKind::Muse, restored.path(), NATIVE, false)
            .unwrap()
            .len(),
        2
    );
    assert!(!restored.path().join("auth.json").exists());
    assert!(
        collect_native_artifacts(HarnessKind::Muse, restored.path(), "missing", false).is_err()
    );
}

#[test]
fn muse_import_normalizes_selected_tree_to_worker_storage() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("private/muse");
    let root = home.join(".data/muse/sessions/2026/09/08");
    let session = root.join(NATIVE);
    fs::create_dir_all(session.join("subagent/child")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(session.join("session.jsonl"), b"root").unwrap();
    fs::write(session.join("subagent/child/session.jsonl"), b"child").unwrap();
    fs::write(session.join("runtime.lock"), b"lock").unwrap();
    fs::write(root.join("other/session.jsonl"), b"other").unwrap();
    let artifacts = collect_import_native_artifacts(
        HarnessKind::Muse,
        &home,
        NATIVE,
        &session.join("session.jsonl"),
    )
    .unwrap();
    assert_eq!(artifacts.len(), 2);
    assert_eq!(
        artifacts[0].relative_path,
        PathBuf::from(format!(
            ".data/muse/sessions/2026/09/08/{NATIVE}/session.jsonl"
        ))
    );
    assert_eq!(artifacts[1].data, b"child");
    assert!(
        collect_import_native_artifacts(
            HarnessKind::Muse,
            &home,
            "wrong",
            &session.join("session.jsonl")
        )
        .is_err()
    );
    assert_eq!(fs::read(session.join("session.jsonl")).unwrap(), b"root");
}

#[test]
fn restore_rewrites_grok_cwd_key_and_session_summary_for_target_workspace() {
    const NATIVE: &str = "01a00c3a-553f-71e0-95ab-aa04396d3ad7";
    let repositories = vec![crate::archive::RepositoryManifest {
        metadata: crate::archive::RepositoryMetadata {
            id: "app".into(),
            relative_destination: "app".into(),
            origin: "owner/app".into(),
            push_urls: Vec::new(),
            remote_workspace: false,
            base_commit: "a".repeat(40),
            head_commit: "a".repeat(40),
            branch: Some("main".into()),
        },
        committed_bundle_path: "repositories/app/committed.bundle".into(),
        staged_patch_path: "repositories/app/staged.patch".into(),
        unstaged_patch_path: "repositories/app/unstaged.patch".into(),
        untracked_tar_path: "repositories/app/untracked.tar".into(),
    }];
    let path = restored_native_relative_path(
        HarnessKind::Grok,
        Path::new(&format!(
            "sessions/%2Fhome%2Fjonathan%2FProjects%2Fapp/{NATIVE}/summary.json"
        )),
        target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
    )
    .unwrap();
    assert_eq!(
        path,
        PathBuf::from(format!("sessions/%2Fworkspace%2Fapp/{NATIVE}/summary.json"))
    );

    let summary = restored_native_artifact_bytes(
            HarnessKind::Grok,
            &path,
            br#"{"info":{"id":"01a00c3a","cwd":"/home/jonathan/Projects/app"},"grok_home":"/home/jonathan/.grok","num_messages":3}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
    let summary: Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["info"]["cwd"], "/workspace/app");
    assert_eq!(summary["grok_home"], "/profiles/imported");
    assert_eq!(summary["num_messages"], 3);

    // Transcript files travel unchanged.
    let history = restored_native_artifact_bytes(
        HarnessKind::Grok,
        Path::new(&format!(
            "sessions/%2Fworkspace%2Fapp/{NATIVE}/chat_history.jsonl"
        )),
        b"{\"role\":\"user\"}\n",
        target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        Path::new("/profiles/imported"),
    )
    .unwrap();
    assert_eq!(history, b"{\"role\":\"user\"}\n");
}

#[test]
fn grok_cwd_key_url_encodes_short_paths_and_hashes_long_ones() {
    assert_eq!(
        grok_cwd_key(Path::new("/home/jonathan")),
        "%2Fhome%2Fjonathan"
    );
    assert_eq!(
        grok_cwd_key(Path::new("/workspace/app")),
        "%2Fworkspace%2Fapp"
    );
    // Unreserved characters survive; everything else is percent-encoded.
    assert_eq!(
        grok_cwd_key(Path::new("/a-b_c.d~e/f g")),
        "%2Fa-b_c.d~e%2Ff%20g"
    );
    let long = Path::new(
        "/Users/test/\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}",
    );
    let key = grok_cwd_key(long);
    assert!(key.len() <= 255);
    assert!(
        !key.starts_with("%2F"),
        "long paths use the hash form: {key}"
    );
    assert!(key.starts_with("workspace-"), "unslugifiable leaf: {key}");
}

#[test]
fn claude_allowlist_collects_transcript_and_session_subtree_only() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("projects/-workspace-app");
    let subagents = project.join(NATIVE).join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
    fs::write(subagents.join("agent-a.jsonl"), b"subagent").unwrap();
    fs::write(project.join("other-session.jsonl"), b"other").unwrap();
    fs::write(project.join("settings.json"), b"secret config").unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
    assert_eq!(artifacts.len(), 2);
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact.relative_path.ends_with(format!("{NATIVE}.jsonl")))
    );
    assert!(
        artifacts
            .iter()
            .any(|artifact| { artifact.relative_path.ends_with("subagents/agent-a.jsonl") })
    );
}

#[test]
fn claude_allowlist_collects_project_memory_for_the_session_slug_only() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("projects/-workspace-app");
    let nested = project.join("memory/notes");
    fs::create_dir_all(&nested).unwrap();
    fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
    fs::write(project.join("memory/root.md"), b"root memory").unwrap();
    fs::write(nested.join("deep.md"), b"nested memory").unwrap();

    // Memory belonging to an unrelated project in the same harness home.
    let other = temp.path().join("projects/-workspace-other/memory");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("leak.md"), b"other project memory").unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
    let paths: Vec<_> = artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect();
    assert!(paths.contains(&PathBuf::from(format!(
        "projects/-workspace-app/{NATIVE}.jsonl"
    ))));
    assert!(paths.contains(&PathBuf::from("projects/-workspace-app/memory/root.md")));
    assert!(paths.contains(&PathBuf::from(
        "projects/-workspace-app/memory/notes/deep.md"
    )));
    assert!(
        !paths
            .iter()
            .any(|path| path.starts_with("projects/-workspace-other")),
        "unrelated project memory leaked: {paths:?}"
    );
}

#[test]
fn claude_project_memory_skips_secret_like_names() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("projects/-workspace-app");
    fs::create_dir_all(project.join("memory")).unwrap();
    fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
    fs::write(project.join("memory/settings.json"), b"secret config").unwrap();
    fs::write(project.join("memory/.env"), b"TOKEN=1").unwrap();
    fs::write(project.join("memory/keep.md"), b"safe memory").unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
    let paths: Vec<_> = artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect();
    assert!(paths.contains(&PathBuf::from("projects/-workspace-app/memory/keep.md")));
    assert!(!paths.contains(&PathBuf::from(
        "projects/-workspace-app/memory/settings.json"
    )));
    assert!(!paths.contains(&PathBuf::from("projects/-workspace-app/memory/.env")));
}

/// Collection and the archive gate read one shared rule set, so every
/// credential name is dropped while walking the session subtree instead of
/// failing the archive write afterwards.
#[test]
fn claude_allowlist_skips_credential_names_in_the_session_subtree() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("projects/-workspace-app");
    let session = project.join(NATIVE);
    fs::create_dir_all(&session).unwrap();
    fs::write(project.join(format!("{NATIVE}.jsonl")), b"transcript").unwrap();
    fs::write(session.join("notes.jsonl"), b"kept").unwrap();
    for name in [
        ".credentials.json",
        "auth.toml",
        "vendor-credentials.json",
        "vendor_credentials.json",
    ] {
        fs::write(session.join(name), b"secret").unwrap();
    }

    let artifacts =
        collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, false).unwrap();
    let paths: Vec<_> = artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect();
    let expected = [
        PathBuf::from(format!("projects/-workspace-app/{NATIVE}.jsonl")),
        PathBuf::from(format!("projects/-workspace-app/{NATIVE}/notes.jsonl")),
    ];
    assert_eq!(
        paths.len(),
        expected.len(),
        "credential names leaked into the native artifacts: {paths:?}"
    );
    for path in expected {
        assert!(paths.contains(&path), "{path:?} was not collected");
    }
}

#[test]
fn claude_project_memory_is_skipped_without_a_session_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("projects/-workspace-app");
    fs::create_dir_all(project.join("memory")).unwrap();
    fs::write(project.join("memory/root.md"), b"root memory").unwrap();

    let artifacts =
        collect_native_artifacts(HarnessKind::Claude, temp.path(), NATIVE, true).unwrap();
    assert!(artifacts.is_empty(), "collected {artifacts:?}");
}

#[test]
fn claude_project_slug_matches_captured_local_rollout_fixtures() {
    // These cwd/directory pairs come from the local Claude home used to
    // establish the import format. Dots are substituted just like slash.
    assert_eq!(
        claude_project_slug(Path::new("/home/jonathan/Projects/mjolnir/.mjolnir/repro")),
        "-home-jonathan-Projects-mjolnir--mjolnir-repro"
    );
    assert_eq!(
        claude_project_slug(Path::new("/tmp/mj-live-transcript.59w2Hg")),
        "-tmp-mj-live-transcript-59w2Hg"
    );
}

#[test]
fn restore_rewrites_claude_project_artifacts_for_target_workspace() {
    let repositories = vec![crate::archive::RepositoryManifest {
        metadata: crate::archive::RepositoryMetadata {
            id: "app".into(),
            relative_destination: "app".into(),
            origin: "owner/app".into(),
            push_urls: Vec::new(),
            remote_workspace: false,
            base_commit: "a".repeat(40),
            head_commit: "a".repeat(40),
            branch: Some("main".into()),
        },
        committed_bundle_path: "repositories/app/committed.bundle".into(),
        staged_patch_path: "repositories/app/staged.patch".into(),
        unstaged_patch_path: "repositories/app/unstaged.patch".into(),
        untracked_tar_path: "repositories/app/untracked.tar".into(),
    }];
    let path = restored_native_relative_path(
        HarnessKind::Claude,
        Path::new("projects/-home-me-app/session.jsonl"),
        target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
    )
    .unwrap();
    assert_eq!(path, PathBuf::from("projects/-workspace-app/session.jsonl"));

    // Project memory rides along under the rewritten slug.
    let memory = restored_native_relative_path(
        HarnessKind::Claude,
        Path::new("projects/-home-me-app/memory/foo.md"),
        target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
    )
    .unwrap();
    assert_eq!(
        memory,
        PathBuf::from("projects/-workspace-app/memory/foo.md")
    );
}

#[test]
fn restore_rewrites_kimi_workspace_and_state_for_target_workspace() {
    let repositories = vec![crate::archive::RepositoryManifest {
        metadata: crate::archive::RepositoryMetadata {
            id: "app".into(),
            relative_destination: "app".into(),
            origin: "owner/app".into(),
            push_urls: Vec::new(),
            remote_workspace: false,
            base_commit: "a".repeat(40),
            head_commit: "a".repeat(40),
            branch: Some("main".into()),
        },
        committed_bundle_path: "repositories/app/committed.bundle".into(),
        staged_patch_path: "repositories/app/staged.patch".into(),
        unstaged_patch_path: "repositories/app/unstaged.patch".into(),
        untracked_tar_path: "repositories/app/untracked.tar".into(),
    }];
    let path = restored_native_relative_path(
            HarnessKind::Kimi,
            Path::new(
                "sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json",
            ),
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
        )
        .unwrap();
    assert_eq!(
        path,
        PathBuf::from(
            "sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json",
        )
    );
    let state = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            &path,
            br#"{"workDir":"/home/jonathan/Projects/kimi-code","cwd":"/home/jonathan/Projects/kimi-code"}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
    let state: Value = serde_json::from_slice(&state).unwrap();
    assert_eq!(state["workDir"], "/workspace/app");
    assert_eq!(state["cwd"], "/workspace/app");
    let registry = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            Path::new("workspaces.json"),
            br#"{"version":1,"deleted_workspace_ids":[],"workspaces":{"wd_kimi-code_78153cfca00c":{"root":"/home/jonathan/Projects/kimi-code","name":"kimi-code"}}}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
    let registry: Value = serde_json::from_slice(&registry).unwrap();
    assert_eq!(
        registry["workspaces"]["wd_app_af7e243d70b1"]["root"],
        "/workspace/app"
    );
    assert!(
        registry["workspaces"]
            .get("wd_kimi-code_78153cfca00c")
            .is_none()
    );

    let index = restored_native_artifact_bytes(
            HarnessKind::Kimi,
            Path::new("session_index.jsonl"),
            br#"{"sessionId":"session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2","workDir":"/home/jonathan/Projects/kimi-code","sessionDir":"/home/jonathan/.kimi-code/sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2"}"#,
            target_primary_cwd("app", &repositories, Path::new("/workspace")).as_deref(),
            Path::new("/profiles/imported"),
        )
        .unwrap();
    let index: Value = serde_json::from_slice(&index).unwrap();
    assert_eq!(index["workDir"], "/workspace/app");
    assert_eq!(
        index["sessionDir"],
        "/profiles/imported/sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2"
    );
}

use crate::test_support::git_line as git;

fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
    let worker_root = temp.join("worker");
    fs::create_dir_all(&worker_root).unwrap();
    let harness_home = temp.join("codex");
    let native = harness_home.join("sessions/2026/08/09");
    fs::create_dir_all(&native).unwrap();
    fs::write(native.join(format!("rollout-{NATIVE}.jsonl")), b"native").unwrap();
    let workspace = temp.join("workspace");
    let repository = workspace.join("app");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init"]);
    git(&repository, &["config", "user.email", "hel@example.test"]);
    git(&repository, &["config", "user.name", "Hel Test"]);
    fs::write(repository.join("README.md"), b"hello").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "base"]);
    git(
        &repository,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/app.git",
        ],
    );
    let base = git(&repository, &["rev-parse", "HEAD"]);
    let output = worker_root.join("source.hel.zip");
    (
        CheckpointExportSpec {
            protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
            session: SessionManifest {
                id: SESSION.into(),
                title: "test".into(),
                harness_kind: HarnessKind::Codex,
                profile_id: "codex-1".into(),
                native_session_id: NATIVE.into(),
                created_at: "2026-08-09T00:00:00Z".into(),
                checkpointed_at: "2026-08-09T00:01:00Z".into(),
                hel_version: "0.1.0".into(),
                relay_version: "0.1.0".into(),
                adapter_version: "test".into(),
            },
            target: TargetManifest {
                template_id: "local".into(),
                target_kind: "podman".into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: "bundle".into(),
                primary_repository: "app".into(),
            },
            relay_root: worker_root,
            harness_home,
            workspace_root: workspace,
            repositories: vec![CheckpointRepositorySpec {
                id: "app".into(),
                relative_destination: "app".into(),
                capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
                origin_override: None,
            }],
            canonical_session: CanonicalSessionSnapshot {
                event_frontier: 1,
                event_frontier_digest: "a".repeat(64),
                session: CanonicalSessionState {
                    execution: CanonicalExecutionState::Idle,
                    last_activity_at_ms: Some(1),
                    session_title: Some("test".into()),
                    configuration: Default::default(),
                },
                transcript: vec![CanonicalTranscriptItem {
                    stable_id: "user-1".into(),
                    position: 1,
                    latest_content_event_ordinal: None,
                    created_at_ms: 1,
                    last_changed_at_ms: 1,
                    body: CanonicalTranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "hello"})],
                    },
                }],
                queued_prompts: vec![CanonicalQueuedPrompt {
                    command_id: "queued-1".into(),
                    kind: CanonicalQueuedCommandKind::Prompt,
                    content: vec![json!({"type": "text", "text": "next"})],
                    queued_at_ms: 2,
                }],
            },
            output_path: output.clone(),
        },
        output,
    )
}

#[test]
fn staged_checkpoint_preserves_export_contents_and_defers_canonical_history() {
    let temp = tempfile::tempdir().unwrap();
    let (export_spec, legacy_path) = fixture(temp.path());
    let large_native = export_spec
        .harness_home
        .join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
    fs::write(&large_native, vec![b'n'; 128 * 1024]).unwrap();
    export_checkpoint(&export_spec).unwrap();

    let stage_path = export_spec.relay_root.join("checkpoint-stage-test");
    let capture_spec = CheckpointCaptureSpec {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        session: export_spec.session.clone(),
        target: export_spec.target.clone(),
        bundle: export_spec.bundle.clone(),
        relay_root: export_spec.relay_root.clone(),
        harness_home: export_spec.harness_home.clone(),
        workspace_root: export_spec.workspace_root.clone(),
        repositories: export_spec.repositories.clone(),
        allow_empty_native: false,
        stage_path: stage_path.clone(),
        refresh_existing: false,
    };
    let capture_json = serde_json::to_vec(&capture_spec).unwrap();
    assert!(
        !String::from_utf8_lossy(&capture_json).contains("queued-1"),
        "canonical history crossed the barrier in the capture request"
    );
    let captured = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
    assert!(!captured.reused_native);
    assert!(captured.native_bytes >= 128 * 1024);
    assert!(stage_path.join(CHECKPOINT_STAGE_MANIFEST).is_file());
    let native_stage = stage_path.join("native/00000000");
    let native_stage_modified = fs::metadata(&native_stage).unwrap().modified().unwrap();
    let mut refresh_spec = capture_spec.clone();
    refresh_spec.refresh_existing = true;
    let refreshed = capture_checkpoint(&refresh_spec, &SystemGit).unwrap();
    assert!(refreshed.reused_native);
    assert_eq!(refreshed.native_bytes, captured.native_bytes);
    assert_eq!(
        fs::metadata(&native_stage).unwrap().modified().unwrap(),
        native_stage_modified,
        "barrier catch-up rewrote unchanged native history"
    );

    let staged_path = export_spec.relay_root.join("staged.hel.zip");
    let pack_spec = CheckpointPackSpec {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        relay_root: export_spec.relay_root.clone(),
        stage_path: stage_path.clone(),
        canonical_session: export_spec.canonical_session.clone(),
        output_path: staged_path.clone(),
    };
    let packed = pack_checkpoint(&pack_spec).unwrap();
    assert_eq!(
        packed.event_frontier,
        export_spec.canonical_session.event_frontier
    );
    assert!(!stage_path.exists(), "consumed stage was not removed");

    let legacy = read_archive_verified(&legacy_path).unwrap();
    let staged = read_archive_verified(&staged_path).unwrap();
    assert_eq!(staged.manifest, legacy.manifest);
    assert_eq!(staged.payloads, legacy.payloads);
}

#[test]
fn prestage_catch_up_recaptures_native_history_that_changed_before_the_barrier() {
    let temp = tempfile::tempdir().unwrap();
    let (export_spec, _) = fixture(temp.path());
    let stage_path = export_spec.relay_root.join("checkpoint-stage-test");
    let mut capture_spec = CheckpointCaptureSpec {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        session: export_spec.session,
        target: export_spec.target,
        bundle: export_spec.bundle,
        relay_root: export_spec.relay_root,
        harness_home: export_spec.harness_home,
        workspace_root: export_spec.workspace_root,
        repositories: export_spec.repositories,
        allow_empty_native: false,
        stage_path,
        refresh_existing: false,
    };
    let first = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
    assert!(!first.reused_native);
    let rollout = capture_spec
        .harness_home
        .join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
    fs::write(&rollout, b"native changed before barrier").unwrap();

    capture_spec.refresh_existing = true;
    let refreshed = capture_checkpoint(&capture_spec, &SystemGit).unwrap();
    assert!(!refreshed.reused_native);
    assert!(refreshed.native_bytes > first.native_bytes);
}

#[test]
#[ignore = "timing measurement against MJ_CHECKPOINT_BENCH_ARCHIVE"]
fn checkpoint_packaging_throughput() {
    let source = std::env::var_os("MJ_CHECKPOINT_BENCH_ARCHIVE")
        .map(PathBuf::from)
        .expect("set MJ_CHECKPOINT_BENCH_ARCHIVE");
    let read_started = std::time::Instant::now();
    let archive = read_archive_verified(&source).unwrap();
    let canonical_session = archive.canonical_session().unwrap();
    let native_artifacts = archive
        .manifest
        .payloads
        .iter()
        .filter_map(|descriptor| {
            let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
                return None;
            };
            Some(NativeArtifact {
                relative_path: relative_path.clone(),
                data: archive.payload(descriptor).unwrap().to_vec(),
                mode: descriptor.mode,
            })
        })
        .collect::<Vec<_>>();
    let repositories = archive
        .manifest
        .repositories
        .iter()
        .map(|repository| archived_repository_snapshot(&archive, repository).unwrap())
        .collect::<Vec<_>>();
    let payload_bytes = native_artifacts
        .iter()
        .map(|artifact| artifact.data.len() as u64)
        .chain(repositories.iter().flat_map(|repository| {
            [
                repository.committed_bundle.len() as u64,
                repository.staged_patch.len() as u64,
                repository.unstaged_patch.len() as u64,
                repository.untracked_tar.len() as u64,
            ]
        }))
        .sum::<u64>()
        + serde_json::to_vec(&canonical_session).unwrap().len() as u64;
    let read_elapsed = read_started.elapsed();
    let stage_fixture = tempfile::tempdir().unwrap();
    let relay_root = stage_fixture.path().join("worker");
    let harness_home = stage_fixture.path().join("harness");
    let workspace_root = stage_fixture.path().join("workspace");
    let repository_root = workspace_root.join("app");
    fs::create_dir_all(&relay_root).unwrap();
    fs::create_dir_all(&harness_home).unwrap();
    fs::create_dir_all(&repository_root).unwrap();
    for artifact in &native_artifacts {
        write_private_file(
            &harness_home,
            &artifact.relative_path,
            &artifact.data,
            artifact.mode,
        )
        .unwrap();
    }
    git(&repository_root, &["init"]);
    git(
        &repository_root,
        &["config", "user.email", "hel@example.test"],
    );
    git(&repository_root, &["config", "user.name", "Hel Test"]);
    fs::write(repository_root.join("README.md"), b"benchmark").unwrap();
    git(&repository_root, &["add", "."]);
    git(&repository_root, &["commit", "-m", "benchmark"]);
    let capture_spec = CheckpointCaptureSpec {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        session: archive.manifest.session.clone(),
        target: archive.manifest.target.clone(),
        bundle: archive.manifest.bundle.clone(),
        relay_root,
        harness_home,
        workspace_root,
        repositories: vec![CheckpointRepositorySpec {
            id: "app".into(),
            relative_destination: "app".into(),
            capture: CheckpointRepositoryCapture::MetadataOnly,
            origin_override: None,
        }],
        allow_empty_native: false,
        stage_path: stage_fixture.path().join("worker/checkpoint-stage"),
        refresh_existing: false,
    };
    let prestage_started = std::time::Instant::now();
    capture_checkpoint(&capture_spec, &SystemGit).unwrap();
    let prestage_elapsed = prestage_started.elapsed();
    let catch_up_started = std::time::Instant::now();
    capture_checkpoint(
        &CheckpointCaptureSpec {
            refresh_existing: true,
            ..capture_spec
        },
        &SystemGit,
    )
    .unwrap();
    let catch_up_elapsed = catch_up_started.elapsed();
    let output_directory = tempfile::tempdir().unwrap();
    let output = output_directory.path().join("benchmark.hel.zip");
    let pack_started = std::time::Instant::now();
    write_archive_hashed(
        &output,
        &ArchiveInput {
            session: archive.manifest.session,
            target: archive.manifest.target,
            bundle: archive.manifest.bundle,
            canonical_session,
            native_artifacts,
            repositories,
        },
    )
    .unwrap();
    eprintln!(
        "checkpoint benchmark: payload_bytes={payload_bytes} read_ms={} prestage_ms={} catch_up_ms={} pack_ms={}",
        read_elapsed.as_millis(),
        prestage_elapsed.as_millis(),
        catch_up_elapsed.as_millis(),
        pack_started.elapsed().as_millis()
    );
}

#[test]
#[ignore = "timing measurement against a real Codex archive and harness home"]
fn codex_root_archive_throughput() {
    const BASELINE_MS: u128 = 46_124;
    let source = std::env::var_os("MJ_CHECKPOINT_BENCH_ARCHIVE")
        .map(PathBuf::from)
        .expect("set MJ_CHECKPOINT_BENCH_ARCHIVE");
    let harness_home = std::env::var_os("MJ_CHECKPOINT_BENCH_HARNESS_HOME")
        .map(PathBuf::from)
        .expect("set MJ_CHECKPOINT_BENCH_HARNESS_HOME");
    let metadata = verify_archive_streaming(&source).unwrap();
    assert_eq!(metadata.manifest.session.harness_kind, HarnessKind::Codex);
    assert!(
        metadata.manifest.payloads.iter().all(|payload| {
            !matches!(
                &payload.role,
                PayloadRole::GitBundle { .. }
                    | PayloadRole::GitStagedPatch { .. }
                    | PayloadRole::GitUnstagedPatch { .. }
                    | PayloadRole::GitUntrackedTar { .. }
            ) || payload.size == 0
        }),
        "benchmark source must use metadata-only repository capture"
    );

    // Prime the persistent negative verdicts exactly as a prior checkpoint
    // does. The timed run models the repeat-checkpoint path.
    let native_session_id = &metadata.manifest.session.native_session_id;
    let mut scan_cache = CodexScanCache::empty(native_session_id);
    let warm = collect_native_artifacts_cached(
        HarnessKind::Codex,
        &harness_home,
        native_session_id,
        false,
        Some(&mut scan_cache),
    )
    .unwrap();
    assert_eq!(
        warm.len(),
        1,
        "benchmark must capture only the root rollout"
    );
    drop(warm);

    let output_directory = tempfile::tempdir_in(source.parent().unwrap()).unwrap();
    let output = output_directory.path().join("export.hel.zip");
    let installed = output_directory.path().join("installed.hel.zip");
    let started = std::time::Instant::now();
    let collect_started = std::time::Instant::now();
    let native_artifacts = collect_native_artifacts_cached(
        HarnessKind::Codex,
        &harness_home,
        native_session_id,
        false,
        Some(&mut scan_cache),
    )
    .unwrap();
    let native_bytes = native_artifacts
        .iter()
        .map(|artifact| artifact.data.len() as u64)
        .sum::<u64>();
    let collect_ms = collect_started.elapsed().as_millis();
    let repositories = metadata
        .manifest
        .repositories
        .iter()
        .map(|repository| RepositorySnapshot {
            metadata: repository.metadata.clone(),
            committed_bundle: Vec::new(),
            staged_patch: Vec::new(),
            unstaged_patch: Vec::new(),
            untracked_tar: Vec::new(),
        })
        .collect::<Vec<_>>();
    let archive_started = std::time::Instant::now();
    let sha256 = write_archive_hashed_borrowed(
        &output,
        &metadata.manifest.session,
        &metadata.manifest.target,
        &metadata.manifest.bundle,
        &metadata.canonical_session,
        &native_artifacts,
        &repositories,
    )
    .unwrap();
    let archive_ms = archive_started.elapsed().as_millis();
    let copy_started = std::time::Instant::now();
    fs::copy(&output, &installed).unwrap();
    let copy_ms = copy_started.elapsed().as_millis();
    let checksum_started = std::time::Instant::now();
    let installed_sha256 = checkpoint_sha256(&installed).unwrap();
    let checksum_ms = checksum_started.elapsed().as_millis();
    let total_ms = started.elapsed().as_millis();
    assert_eq!(installed_sha256, sha256);
    eprintln!(
        "Codex root archive benchmark: native_bytes={native_bytes} archive_bytes={} collect_ms={collect_ms} archive_ms={archive_ms} copy_ms={copy_ms} checksum_ms={checksum_ms} total_ms={total_ms} target_ms={}",
        fs::metadata(&installed).unwrap().len(),
        BASELINE_MS / 10
    );
    assert!(
        total_ms <= BASELINE_MS / 10,
        "root-only repeat export took {total_ms}ms; 10x target is {}ms",
        BASELINE_MS / 10
    );
}

#[test]
fn checkpoint_collects_the_configured_memory_replica_for_non_claude_harnesses() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let memory_root = spec.harness_home.join("projects/replica/memory");
    fs::create_dir_all(&memory_root).unwrap();
    fs::write(memory_root.join("MEMORY.md"), "remember this").unwrap();
    mj_core::worker_launch::WorkerLaunchConfig {
        goal_resume_request: Default::default(),
        target_environment: Default::default(),
        seed_image_environment: false,
        run_mode: Default::default(),
        review_capture: false,
        session_id: SESSION.into(),
        harness: HarnessKind::Codex,
        authentication_marker: None,
        bridge_command: "codex-acp".into(),
        bridge_args: Vec::new(),
        harness_runtime: mj_core::worker_launch::HarnessRuntimePolicy::Ambient,
        environment: Default::default(),
        cwd: spec.workspace_root.join("app"),
        additional_directories: Vec::new(),
        native_session_id: Some(NATIVE.into()),
        subagent_tools: false,
        project_memory: Some(mj_core::worker_launch::ProjectMemoryLaunchConfig {
            project_key: "project".into(),
            root: memory_root,
            baseline_root: spec
                .harness_home
                .join("projects/replica/.hel-memory-baseline"),
            repository_roots: Default::default(),
            mcp_delivery: mj_core::worker_launch::ProjectMemoryMcpDelivery::Acp,
        }),
        execution_policy: mj_core::config::ExecutionPolicy::Unconstrained,
    }
    .write(&spec.relay_root.join("launch.json"))
    .unwrap();

    let artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        false,
        &NoNativeCheckpointState,
    )
    .unwrap();
    assert!(artifacts.iter().any(|artifact| {
        artifact.relative_path == Path::new("projects/replica/memory/MEMORY.md")
            && artifact.data == b"remember this"
    }));
}

#[test]
fn checkpoint_collects_memory_from_a_legacy_worker_launch_config() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let memory_root = spec.harness_home.join("projects/replica/memory");
    fs::create_dir_all(&memory_root).unwrap();
    fs::write(memory_root.join("MEMORY.md"), "legacy memory").unwrap();
    let legacy_launch = json!({
        "session_id": SESSION,
        "harness": "codex",
        "bridge_command": "codex-acp",
        "bridge_args": [],
        "environment": {},
        "cwd": spec.workspace_root.join("app"),
        "native_session_id": NATIVE,
        "project_memory": {
            "project_key": "project",
            "root": memory_root,
            "baseline_root": spec.harness_home.join("projects/replica/.hel-memory-baseline"),
            "repository_roots": {}
        },
        "force_unrestricted_mode": true
    });
    fs::write(
        spec.relay_root.join("launch.json"),
        serde_json::to_vec_pretty(&legacy_launch).unwrap(),
    )
    .unwrap();

    let artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        false,
        &NoNativeCheckpointState,
    )
    .unwrap();

    assert!(artifacts.iter().any(|artifact| {
        artifact.relative_path == Path::new("projects/replica/memory/MEMORY.md")
            && artifact.data == b"legacy memory"
    }));
}

#[test]
fn project_memory_replica_accepts_an_ssh_home_relative_path() {
    let relative = Path::new(".local/share/hel/profiles/session/projects/replica/memory");
    let home = PathBuf::from(std::env::var_os("HOME").expect("test HOME is missing"));

    assert_eq!(
        resolve_home_relative_target_path(relative).unwrap(),
        home.join(relative)
    );
    assert!(resolve_home_relative_target_path(Path::new("../memory")).is_err());
}

#[test]
fn a_checkout_restore_lands_on_the_branch_the_caller_names() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, archive_path) = fixture(temp.path());
    let repository = spec.workspace_root.join("app");
    let launch_base = git(&repository, &["rev-parse", "HEAD"]);
    let archived_origin = "https://archive-fetch.example.test/app.git";
    let archived_push_url = "https://archive-push.example.test/app.git";
    git(
        &repository,
        &["config", "--local", "remote.origin.url", archived_origin],
    );
    git(
        &repository,
        &[
            "config",
            "--local",
            "--replace-all",
            "remote.origin.pushurl",
            archived_push_url,
        ],
    );
    git(
        &repository,
        &["config", "--local", "push.default", "current"],
    );
    git(
        &repository,
        &["config", "--local", "push.autoSetupRemote", "true"],
    );
    git(
        &repository,
        &["config", "--local", "mj.remoteWorkspace", "true"],
    );
    git(
        &repository,
        &["config", "--local", "mj.baseCommit", &launch_base],
    );
    spec.repositories[0].capture = CheckpointRepositoryCapture::RemoteWorkspace;
    fs::write(repository.join("feature.txt"), b"session work").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "session work"]);
    fs::write(repository.join("README.md"), b"edited").unwrap();
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let archived_branch = git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]);
    export_checkpoint(&spec).unwrap();
    let archive = verify_archive_streaming(&archive_path).unwrap();
    assert!(archive.manifest.repositories[0].metadata.remote_workspace);
    assert_eq!(
        archive.manifest.repositories[0].metadata.origin,
        archived_origin
    );
    assert_eq!(
        archive.manifest.repositories[0].metadata.push_urls,
        vec![archived_push_url]
    );

    let host_origin = "https://host-fetch.example.test/app.git";
    let host_push_url = "https://host-push.example.test/app.git";
    git(
        &repository,
        &["config", "--local", "remote.origin.url", host_origin],
    );
    git(
        &repository,
        &[
            "config",
            "--local",
            "--replace-all",
            "remote.origin.pushurl",
            host_push_url,
        ],
    );
    git(
        &repository,
        &["config", "--local", "push.default", "simple"],
    );
    git(
        &repository,
        &["config", "--local", "push.autoSetupRemote", "false"],
    );
    git(
        &repository,
        &["config", "--local", "--unset-all", "mj.remoteWorkspace"],
    );
    git(
        &repository,
        &["config", "--local", "--unset-all", "mj.baseCommit"],
    );

    let checkout = temp.path().join("worktrees/session");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "mj/session",
            &checkout.to_string_lossy(),
            "HEAD~1",
        ],
    );

    let restored =
        restore_single_repository_onto_branch(&archive_path, &checkout, "mj/session", &SystemGit)
            .unwrap();

    assert_eq!(restored.as_deref(), Some(archived_branch.as_str()));
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "mj/session"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("README.md")).unwrap(),
        "edited",
        "the session's uncommitted work comes with it"
    );
    assert_eq!(
        git(
            &repository,
            &["config", "--local", "--get", "remote.origin.url"]
        ),
        host_origin,
        "host origin is unchanged by the raw restore"
    );
    assert_eq!(
        git(
            &repository,
            &["config", "--local", "--get-all", "remote.origin.pushurl"],
        ),
        host_push_url,
        "host push URL is unchanged by the raw restore"
    );
    assert_eq!(
        git(&repository, &["config", "--local", "--get", "push.default"]),
        "simple",
        "host push default is unchanged by the raw restore"
    );
    assert_eq!(
        git(
            &repository,
            &["config", "--local", "--get", "push.autoSetupRemote"],
        ),
        "false",
        "host automatic push setup is unchanged by the raw restore"
    );
    for key in ["mj.remoteWorkspace", "mj.baseCommit"] {
        let output = Command::new("git")
            .args(["config", "--local", "--get", key])
            .current_dir(&repository)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "host config unexpectedly has {key}"
        );
    }

    // Restoring onto the archived branch is exactly what the override
    // avoids: that branch is checked out in the user's own working tree.
    let error = restore_single_repository_onto_branch(
        &archive_path,
        &checkout,
        &archived_branch,
        &SystemGit,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("restore committed branch"),
        "{error:#}"
    );
    assert_eq!(
        git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "mj/session",
        "a rejected restore leaves the checkout on its session branch"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("README.md")).unwrap(),
        "edited",
        "a rejected restore leaves the worktree unchanged"
    );
}

#[test]
fn a_workspace_restore_refuses_a_branch_checked_out_in_another_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, archive_path) = fixture(temp.path());
    let repository = spec.workspace_root.join("app");
    fs::write(repository.join("feature.txt"), b"session work").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "session work"]);
    let archived_branch = git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]);
    export_checkpoint(&spec).unwrap();

    // The restore destination is a sibling worktree of the same repository,
    // so the archived branch is already owned by the main checkout.
    let restore_root = temp.path().join("restore-workspace");
    let destination = restore_root.join("app");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "mj/session",
            &destination.to_string_lossy(),
            "HEAD~1",
        ],
    );
    let before = git(&destination, &["rev-parse", "HEAD"]);

    let error = restore_repositories(&archive_path, &restore_root, &SystemGit).unwrap_err();

    assert!(
        format!("{error:#}").contains("is checked out in another worktree"),
        "{error:#}"
    );
    assert_eq!(
        git(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "mj/session",
        "a rejected restore leaves the destination on its own branch"
    );
    assert_eq!(
        git(&destination, &["rev-parse", "HEAD"]),
        before,
        "a rejected restore leaves the destination checkout unchanged"
    );
    assert_eq!(
        git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]),
        archived_branch
    );
}

fn copy_archive_with_schema(source: &Path, destination: &Path, schema_version: u32) {
    let source = File::open(source).unwrap();
    let mut archive = zip::ZipArchive::new(source).unwrap();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mode = entry.unix_mode().unwrap_or(0o600);
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        if name == "manifest.json" {
            let mut manifest: Value = serde_json::from_slice(&body).unwrap();
            manifest["schema_version"] = json!(schema_version);
            body = serde_json::to_vec_pretty(&manifest).unwrap();
        }
        entries.push((name, mode, body));
    }
    let output = File::create(destination).unwrap();
    let mut writer = zip::ZipWriter::new(output);
    for (name, mode, body) in entries {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default().unix_permissions(mode),
            )
            .unwrap();
        writer.write_all(&body).unwrap();
    }
    writer.finish().unwrap();
}

fn copy_archive_with_canonical_session(
    source: &Path,
    destination: &Path,
    canonical_session: &CanonicalSessionSnapshot,
) {
    let source = File::open(source).unwrap();
    let mut archive = zip::ZipArchive::new(source).unwrap();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mode = entry.unix_mode().unwrap_or(0o600);
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        entries.push((name, mode, body));
    }

    let canonical_body = serde_json::to_vec_pretty(canonical_session).unwrap();
    let canonical_sha256 = lower_hex(Sha256::digest(&canonical_body));
    entries
        .iter_mut()
        .find(|(name, _, _)| name == "canonical/session.json")
        .unwrap()
        .2 = canonical_body.clone();
    let manifest_body = &mut entries
        .iter_mut()
        .find(|(name, _, _)| name == "manifest.json")
        .unwrap()
        .2;
    let mut manifest: Value = serde_json::from_slice(manifest_body).unwrap();
    let descriptor = manifest["payloads"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|payload| payload["path"] == "canonical/session.json")
        .unwrap();
    descriptor["size"] = json!(canonical_body.len());
    descriptor["sha256"] = json!(canonical_sha256);
    *manifest_body = serde_json::to_vec_pretty(&manifest).unwrap();

    let output = File::create(destination).unwrap();
    let mut writer = zip::ZipWriter::new(output);
    for (name, mode, body) in entries {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default().unix_permissions(mode),
            )
            .unwrap();
        writer.write_all(&body).unwrap();
    }
    writer.finish().unwrap();
}

fn assert_canonical_restore_rejected_before_mutation(
    spec: &CheckpointExportSpec,
    invalid_archive: &Path,
    canonical_session: &CanonicalSessionSnapshot,
    expected_error: &str,
) {
    copy_archive_with_canonical_session(&spec.output_path, invalid_archive, canonical_session);
    let readme = spec.workspace_root.join("app/README.md");
    let before = fs::read(&readme).unwrap();
    let relay_root = invalid_archive.with_extension("relay");
    let harness_home = invalid_archive.with_extension("harness");

    let error = restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: invalid_archive.to_path_buf(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: relay_root.clone(),
            harness_home: harness_home.clone(),
            restore_repositories: true,
            restore_native: true,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains(expected_error), "{error:#}");
    assert_eq!(fs::read(&readme).unwrap(), before);
    assert!(!relay_root.exists());
    assert!(!harness_home.exists());
}

/// A second opinion keeps its whole world — profile, native session and
/// relay — inside the primary worker root. A v1 checkpoint is single
/// session, so none of it may end up in the archive.
#[test]
fn a_checkpoint_excludes_everything_the_reviewer_owns() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let reviewer = spec.relay_root.join("reviewer");
    let reviewer_home = reviewer.join("profile");
    // A reviewer that ran: a staged profile with its own native rollout,
    // its own relay journal, and its own supervisor spec.
    let reviewer_native = reviewer_home.join("sessions/2026/08/09");
    fs::create_dir_all(&reviewer_native).unwrap();
    fs::write(
        reviewer_native.join("rollout-reviewer-native.jsonl"),
        b"reviewer native session",
    )
    .unwrap();
    fs::create_dir_all(reviewer.join("relay-journal")).unwrap();
    fs::write(
        reviewer.join("relay-journal/active.jsonl"),
        b"reviewer relay events",
    )
    .unwrap();
    fs::write(reviewer.join("relay-state.json"), b"reviewer relay state").unwrap();
    fs::write(reviewer.join("acp-supervisor.json"), b"reviewer bridge").unwrap();

    export_checkpoint(&spec).unwrap();

    let archive = read_archive_verified(&spec.output_path).unwrap();
    let native = archive
        .manifest
        .payloads
        .iter()
        .filter_map(|payload| match &payload.role {
            PayloadRole::NativeArtifact { relative_path } => {
                Some(relative_path.to_string_lossy().into_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        native.iter().all(|path| !path.contains("reviewer")),
        "a reviewer's files must stay out of the checkpoint: {native:?}"
    );
    let bytes = fs::read(&spec.output_path).unwrap();
    for secret in [
        b"reviewer native session".as_slice(),
        b"reviewer relay events".as_slice(),
        b"reviewer relay state".as_slice(),
    ] {
        assert!(
            !bytes.windows(secret.len()).any(|window| window == secret),
            "the archive must not carry the reviewer's content"
        );
    }
    // The primary's own native session is still captured, so this proves
    // exclusion rather than an export that captured nothing.
    assert!(
        !native.is_empty(),
        "the primary's native session must still be exported"
    );
}

#[test]
fn raw_project_export_keeps_git_metadata_without_git_contents() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.repositories[0].capture = CheckpointRepositoryCapture::MetadataOnly;
    let repository = spec.workspace_root.join("app");
    fs::write(repository.join("README.md"), b"dirty").unwrap();
    fs::write(repository.join("untracked.txt"), b"untracked").unwrap();

    export_checkpoint(&spec).unwrap();
    let archive = read_archive_verified(&spec.output_path).unwrap();
    let repository = &archive.manifest.repositories[0];
    assert_eq!(
        repository.metadata.base_commit,
        repository.metadata.head_commit
    );
    for role in [
        PayloadRole::GitBundle {
            repository_id: "app".into(),
        },
        PayloadRole::GitStagedPatch {
            repository_id: "app".into(),
        },
        PayloadRole::GitUnstagedPatch {
            repository_id: "app".into(),
        },
        PayloadRole::GitUntrackedTar {
            repository_id: "app".into(),
        },
    ] {
        assert!(archive.payload_by_role(&role).unwrap().is_empty());
    }
}

#[test]
fn session_delta_without_origin_refs_repairs_once_then_fails() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
    let git = RecordingGit::with_fetch_failure();

    let error = export_checkpoint_with_git(&spec, &git).unwrap_err();

    assert_eq!(git.fetches(), 1);
    let error = format!("{error:#}");
    assert!(
        error.contains("repository 'app' has no origin refs"),
        "{error}"
    );
    assert!(error.contains("refusing to bundle full history"), "{error}");
    assert!(error.contains("repair fetch failed"), "{error}");
}

#[test]
fn remote_workspace_capture_uses_immutable_marker_base_after_origin_moves() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    let repository = spec.workspace_root.join("app");
    let base = git(&repository, &["rev-parse", "HEAD"]);
    git(&repository, &["switch", "-q", "-c", "mj/remote-session"]);
    git(
        &repository,
        &["config", "--local", "mj.remoteWorkspace", "true"],
    );
    git(&repository, &["config", "--local", "mj.baseCommit", &base]);
    let push_url = "https://push.example.test/repo.git";
    git(
        &repository,
        &[
            "config",
            "--local",
            "--add",
            "remote.origin.pushurl",
            push_url,
        ],
    );
    fs::write(repository.join("session.txt"), b"published session\n").unwrap();
    git(&repository, &["add", "session.txt"]);
    git(&repository, &["commit", "-qm", "session work"]);
    let head = git(&repository, &["rev-parse", "HEAD"]);
    // Make origin-tracking history appear current. A managed checkpoint
    // must still carry the commit because it uses mj.baseCommit.
    git(
        &repository,
        &["update-ref", "refs/remotes/origin/main", &head],
    );

    spec.repositories[0].capture = CheckpointRepositoryCapture::RemoteWorkspace;
    let repositories =
        collect_checkpoint_repositories(&spec.workspace_root, &spec.repositories, &SystemGit)
            .unwrap();
    let snapshot = &repositories[0];
    assert!(snapshot.metadata.remote_workspace);
    assert_eq!(snapshot.metadata.base_commit, base);
    assert_eq!(snapshot.metadata.head_commit, head);
    assert_eq!(snapshot.metadata.push_urls, vec![push_url.to_owned()]);
    assert!(!snapshot.committed_bundle.is_empty());
}

#[test]
fn invalid_canonical_session_is_rejected_before_repository_repair() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
    spec.canonical_session.session.execution =
        CanonicalExecutionState::Running { started_at_ms: 3 };
    let git = RecordingGit::with_fetch_failure();

    let error = export_checkpoint_with_git(&spec, &git).unwrap_err();

    assert!(format!("{error:#}").contains("not idle at the checkpoint barrier"));
    assert_eq!(git.fetches(), 0);
    assert!(!spec.output_path.exists());
}

#[test]
fn session_delta_export_succeeds_when_the_repair_fetch_restores_origin_refs() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.repositories[0].capture = CheckpointRepositoryCapture::SessionDelta;
    let repository = spec.workspace_root.join("app");
    let origin = temp.path().join("origin.git");
    git(
        &spec.workspace_root,
        &["clone", "-q", "--bare", "app", origin.to_str().unwrap()],
    );
    git(
        &repository,
        &["remote", "set-url", "origin", origin.to_str().unwrap()],
    );
    fs::write(repository.join("later.txt"), b"later").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-qm", "later"]);
    let git_runner = RecordingGit::forwarding();

    export_checkpoint_with_git(&spec, &git_runner).unwrap();

    assert_eq!(git_runner.fetches(), 1);
    let archive = read_archive_verified(&spec.output_path).unwrap();
    assert_eq!(archive.manifest.repositories[0].metadata.base_commit, "");
    assert!(
        !archive
            .payload_by_role(&PayloadRole::GitBundle {
                repository_id: "app".into(),
            })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn raw_project_without_git_metadata_fails_checkpoint_export() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.repositories[0].capture = CheckpointRepositoryCapture::MetadataOnly;
    fs::remove_dir_all(spec.workspace_root.join("app/.git")).unwrap();

    let error = export_checkpoint(&spec).unwrap_err();

    assert!(format!("{error:#}").contains("repository has no valid Git HEAD"));
}

#[test]
fn export_reports_phase_timings_to_the_controller() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let git_runner = RecordingGit::forwarding();

    let exported = export_checkpoint_with_git(&spec, &git_runner).unwrap();

    let timings = exported.timings.expect("export reports its phase timings");
    assert!(
        timings.total_ms
            >= timings
                .native_ms
                .max(timings.repositories_ms)
                .max(timings.archive_ms),
        "{timings:?}"
    );
}

#[test]
fn target_checkpoint_from_a_worker_without_timings_still_decodes() {
    let target = serde_json::from_value::<TargetCheckpoint>(json!({
        "path": "/worker/checkpoint.hel.zip",
        "sha256": "abc",
        "event_frontier": 7,
        "event_frontier_digest": "def"
    }))
    .unwrap();

    assert_eq!(target.timings, None);
    // The field is also absent from the wire form when nothing measured it.
    let encoded = serde_json::to_value(&target).unwrap();
    assert!(encoded.get("timings").is_none(), "{encoded}");
}

#[test]
fn checkpoint_wire_requires_the_new_capture_mode_and_rejects_legacy_fields() {
    let legacy = serde_json::from_value::<CheckpointRepositorySpec>(json!({
        "id": "app",
        "relative_destination": "app",
        "base_commit": "HEAD"
    }));
    assert!(legacy.is_err());

    let repository: CheckpointRepositorySpec = serde_json::from_value(json!({
        "id": "app",
        "relative_destination": "app",
        "capture": { "mode": "delta_from", "base_commit": "HEAD" },
        "origin_override": null
    }))
    .unwrap();
    assert_eq!(
        repository.capture,
        CheckpointRepositoryCapture::DeltaFrom {
            base_commit: "HEAD".into()
        }
    );

    let mixed = serde_json::from_value::<CheckpointRepositorySpec>(json!({
        "id": "app",
        "relative_destination": "app",
        "capture": { "mode": "session_delta" },
        "origin_override": null,
        "session_delta": true
    }));
    assert!(mixed.is_err());

    // The compatibility reset does not reinterpret the old event frontier.
    let target = serde_json::from_value::<TargetCheckpoint>(json!({
        "path": "/worker/checkpoint.hel.zip",
        "sha256": "abc",
        "event_sequence": 7,
        "full_history_fallbacks": ["app"]
    }));
    assert!(target.is_err());

    let restore = json!({
        "archive_path": "/relay/checkpoint.hel.zip",
        "workspace_root": "/workspace",
        "relay_root": "/relay",
        "harness_home": "/harness",
        "restore_repositories": true,
        "restore_native": true,
        "discard_queued_prompts": false
    });
    assert!(serde_json::from_value::<CheckpointRestoreSpec>(restore.clone()).is_ok());
    let mut missing_flag = restore.clone();
    missing_flag
        .as_object_mut()
        .unwrap()
        .remove("discard_queued_prompts");
    assert!(serde_json::from_value::<CheckpointRestoreSpec>(missing_flag).is_err());
    let mut retired_root = restore;
    retired_root
        .as_object_mut()
        .unwrap()
        .insert("worker_root".into(), json!("/legacy"));
    assert!(serde_json::from_value::<CheckpointRestoreSpec>(retired_root).is_err());
}

#[test]
fn checkpoint_export_wire_uses_relay_root_and_rejects_retired_worker_root() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let mut value = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        value["protocol_version"],
        CHECKPOINT_EXPORT_PROTOCOL_VERSION
    );
    assert!(value.get("relay_root").is_some());
    assert!(value.get("worker_root").is_none());

    let mut unversioned = value.clone();
    unversioned
        .as_object_mut()
        .unwrap()
        .remove("protocol_version");
    assert!(serde_json::from_value::<CheckpointExportSpec>(unversioned).is_err());

    value
        .as_object_mut()
        .unwrap()
        .insert("worker_root".into(), json!("/legacy"));
    assert!(serde_json::from_value::<CheckpointExportSpec>(value).is_err());
}

#[test]
fn checkpoint_export_rejects_an_unsupported_protocol_before_interpreting_paths() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.protocol_version = CHECKPOINT_EXPORT_PROTOCOL_VERSION + 1;
    spec.relay_root = PathBuf::from("relative/worker");

    let error = export_checkpoint(&spec).unwrap_err();

    assert_eq!(
        error.to_string(),
        "unsupported checkpoint export protocol version 3; worker supports 2"
    );
}

#[test]
fn checkpoint_round_trips_the_latched_materialized_session() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.canonical_session.event_frontier = 9;
    spec.canonical_session.transcript[0].position = 7;

    let target = export_checkpoint(&spec).unwrap();
    assert_eq!(target.event_frontier, 9);
    assert_eq!(
        read_checkpoint_session(&spec.output_path).unwrap(),
        spec.canonical_session
    );
}

fn restore_into(temp: &Path, spec: &CheckpointExportSpec, discard_queued_prompts: bool) -> PathBuf {
    let relay_root = temp.join(format!("restored-relay-{discard_queued_prompts}"));
    restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: relay_root.clone(),
            harness_home: temp.join("restored-harness"),
            restore_repositories: false,
            restore_native: false,
            discard_queued_prompts,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap();
    relay_root
}

fn restored_seed(relay_root: &Path) -> mj_core::relay::RestoredRelaySeed {
    serde_json::from_slice(&fs::read(mj_core::relay::restored_relay_seed_path(relay_root)).unwrap())
        .unwrap()
}

#[test]
fn restore_seeds_the_relay_frontier_and_can_discard_queued_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();

    let kept = restored_seed(&restore_into(temp.path(), &spec, false));
    assert_eq!(kept.event_frontier, spec.canonical_session.event_frontier);
    assert_eq!(
        kept.event_frontier_digest,
        spec.canonical_session.event_frontier_digest
    );
    assert_eq!(kept.queued_prompts, spec.canonical_session.queued_prompts);

    let relay_root = restore_into(temp.path(), &spec, true);
    let discarded = restored_seed(&relay_root);
    assert_eq!(
        discarded.event_frontier,
        spec.canonical_session.event_frontier
    );
    assert!(discarded.queued_prompts.is_empty());
    assert!(!relay_root.join("events.jsonl").exists());
}

fn image_checkpoint_fixture(
    temp: &Path,
) -> (
    CheckpointExportSpec,
    mj_core::attachment::AttachmentRef,
    Vec<u8>,
    PathBuf,
) {
    let (mut spec, _) = fixture(temp);
    let bytes = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFgAI/yZmaPdoAAAAASUVORK5CYII=")
            .unwrap();
    let reference =
        mj_core::attachment::AttachmentRef::new(&bytes, "image/png".into(), 1, 1).unwrap();
    let store = mj_core::attachment::AttachmentStore::worker(&spec.relay_root);
    store.install(&reference, &bytes).unwrap();
    let image_block = serde_json::to_value(reference.content_block()).unwrap();
    spec.canonical_session.queued_prompts[0].content = vec![image_block; 10];
    export_checkpoint(&spec).unwrap();
    assert_eq!(
        read_archive_verified(&spec.output_path)
            .unwrap()
            .manifest
            .schema_version,
        crate::archive::ARCHIVE_SCHEMA_VERSION_ATTACHMENTS
    );

    let restored_relay = temp.join("restored-image-relay");
    restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: restored_relay.clone(),
            harness_home: temp.join("restored-image-harness"),
            restore_repositories: false,
            restore_native: false,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap();

    (spec, reference, bytes, restored_relay)
}

#[test]
fn checkpoint_round_trips_ten_image_references_and_restores_the_blob() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, reference, bytes, restored_relay) = image_checkpoint_fixture(temp.path());
    let seed = restored_seed(&restored_relay);

    assert_eq!(
        seed.queued_prompts[0].content,
        spec.canonical_session.queued_prompts[0].content
    );
    let mut blocks: Vec<agent_client_protocol::schema::v1::ContentBlock> = seed.queued_prompts[0]
        .content
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        mj_core::attachment::references(&blocks).unwrap(),
        vec![reference.clone(); 10]
    );

    mj_core::attachment::AttachmentStore::worker(&restored_relay)
        .resolve(&mut blocks)
        .unwrap();
    for block in blocks {
        let agent_client_protocol::schema::v1::ContentBlock::Image(image) = block else {
            panic!("checkpoint image queue contained a non-image block");
        };
        assert!(image.uri.is_none());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image.data)
                .unwrap(),
            bytes
        );
    }
}

#[test]
fn restored_image_resolution_rejects_missing_and_corrupt_blobs() {
    let temp = tempfile::tempdir().unwrap();
    let (_, reference, bytes, restored_relay) = image_checkpoint_fixture(temp.path());
    let store = mj_core::attachment::AttachmentStore::worker(&restored_relay);
    let attachment_path = store.root().join(&reference.sha256);
    let queued = restored_seed(&restored_relay).queued_prompts[0]
        .content
        .clone();
    let mut blocks: Vec<agent_client_protocol::schema::v1::ContentBlock> = queued
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .unwrap();

    fs::remove_file(&attachment_path).unwrap();
    assert!(store.resolve(&mut blocks).is_err());

    let mut corrupt = bytes;
    corrupt[0] ^= 1;
    fs::write(&attachment_path, corrupt).unwrap();
    assert!(store.resolve(&mut blocks).is_err());
}

/// The relay seed must stay proportional to the queue, never to the
/// conversation: a long session used to write its whole transcript into the
/// target's relay root for three fields nobody else read.
#[test]
fn the_relay_seed_does_not_grow_with_the_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    let item = spec.canonical_session.transcript[0].clone();
    spec.canonical_session.transcript = (1..=20_000_u64)
        .map(|position| CanonicalTranscriptItem {
            stable_id: format!("user-{position}"),
            position,
            body: CanonicalTranscriptBody::User {
                content: vec![json!({"type": "text", "text": "x".repeat(256)})],
            },
            ..item.clone()
        })
        .collect();
    spec.canonical_session.event_frontier = 20_000;
    export_checkpoint(&spec).unwrap();

    let relay_root = restore_into(temp.path(), &spec, false);
    let seed = fs::metadata(mj_core::relay::restored_relay_seed_path(&relay_root))
        .unwrap()
        .len();

    assert!(
        seed < 4096,
        "the relay seed embedded the transcript: {seed} bytes"
    );
    assert_eq!(
        restored_seed(&relay_root).queued_prompts,
        spec.canonical_session.queued_prompts
    );
}

/// A restore seeds a relay that has none of its own state yet. Existing
/// state means the previous worker was never fully torn down, and the seed
/// would silently lose to it.
#[test]
fn restore_refuses_a_relay_root_that_already_holds_relay_state() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let relay_root = temp.path().join("occupied-relay");
    fs::create_dir_all(&relay_root).unwrap();
    fs::write(
        relay_root.join(mj_core::relay::RELAY_STATE_FILE),
        b"{\"format_version\":1}",
    )
    .unwrap();

    let error = restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: relay_root.clone(),
            harness_home: temp.path().join("restored-harness"),
            restore_repositories: false,
            restore_native: false,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("relay state already present"),
        "{error:#}"
    );
    assert!(!mj_core::relay::restored_relay_seed_path(&relay_root).exists());
}

/// `Path::exists` follows links, so a *dangling* symlink at the seed path
/// reads as "no file here" and used to send the write to the link target.
#[cfg(unix)]
#[test]
fn restore_refuses_to_seed_the_relay_through_a_dangling_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let relay_root = temp.path().join("symlinked-relay");
    fs::create_dir_all(&relay_root).unwrap();
    let outside = temp.path().join("outside/seed.json");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        relay_root.join(mj_core::relay::RESTORED_RELAY_SEED_FILE),
    )
    .unwrap();

    let error = restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root,
            harness_home: temp.path().join("restored-harness"),
            restore_repositories: false,
            restore_native: false,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    assert!(
        !outside.exists(),
        "the relay seed was written through the symlink"
    );
}

#[cfg(unix)]
#[test]
fn restore_refuses_a_native_artifact_under_a_symlinked_directory() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let harness_home = temp.path().join("symlinked-harness");
    fs::create_dir_all(&harness_home).unwrap();
    let outside = temp.path().join("outside-sessions");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, harness_home.join("sessions")).unwrap();

    let error = restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: temp.path().join("symlinked-native-relay"),
            harness_home,
            restore_repositories: false,
            restore_native: true,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("traverses a symlink"),
        "{error:#}"
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "the restore wrote through the symlinked directory"
    );
}

#[test]
fn restore_writes_native_artifacts_privately_under_the_harness_home() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    let child_relative = PathBuf::from("sessions/2026/08/09/rollout-child.jsonl");
    write_modern_rollout(
        &spec.harness_home.join(&child_relative),
        OTHER_NATIVE,
        NATIVE,
    );
    export_checkpoint(&spec).unwrap();
    let archive = read_archive_verified(&spec.output_path).unwrap();
    assert_eq!(archive.canonical_session().unwrap(), spec.canonical_session);
    assert_eq!(
        archive
            .manifest
            .payloads
            .iter()
            .filter_map(|payload| match &payload.role {
                PayloadRole::NativeArtifact { relative_path } => Some(relative_path.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![PathBuf::from(format!(
            "sessions/2026/08/09/rollout-{NATIVE}.jsonl"
        ))]
    );
    let harness_home = temp.path().join("restored-native-harness");

    restore_checkpoint(
        &CheckpointRestoreSpec {
            archive_path: spec.output_path.clone(),
            workspace_root: spec.workspace_root.clone(),
            relay_root: temp.path().join("restored-native-relay"),
            harness_home: harness_home.clone(),
            restore_repositories: false,
            restore_native: true,
            discard_queued_prompts: false,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap();

    let restored = harness_home.join(format!("sessions/2026/08/09/rollout-{NATIVE}.jsonl"));
    assert_eq!(fs::read(&restored).unwrap(), b"native");
    assert!(!harness_home.join(child_relative).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&restored).unwrap().permissions().mode() & 0o077,
            0,
            "restored native artifact is group- or world-accessible"
        );
    }
}

#[test]
fn incompatible_schema_is_rejected_before_restore_mutates_target() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let readme = spec.workspace_root.join("app/README.md");
    let before = fs::read(&readme).unwrap();

    for schema_version in [1, crate::archive::ARCHIVE_SCHEMA_VERSION + 1] {
        let incompatible = temp
            .path()
            .join(format!("incompatible-{schema_version}.hel.zip"));
        copy_archive_with_schema(&spec.output_path, &incompatible, schema_version);
        let relay_root = temp.path().join(format!("relay-{schema_version}"));
        let error = restore_checkpoint(
            &CheckpointRestoreSpec {
                archive_path: incompatible,
                workspace_root: spec.workspace_root.clone(),
                relay_root: relay_root.clone(),
                harness_home: temp.path().join("restored-harness"),
                restore_repositories: true,
                restore_native: true,
                discard_queued_prompts: false,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("incompatible Mjolnir archive schema"),
            "{error:#}"
        );
        assert_eq!(fs::read(&readme).unwrap(), before);
        assert!(!relay_root.exists());
    }
}

#[test]
fn non_idle_canonical_session_is_rejected_before_restore_mutates_target() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let mut invalid = spec.canonical_session.clone();
    invalid.session.execution = CanonicalExecutionState::Running { started_at_ms: 3 };

    assert_canonical_restore_rejected_before_mutation(
        &spec,
        &temp.path().join("non-idle.hel.zip"),
        &invalid,
        "not idle at the checkpoint barrier",
    );
}

#[test]
fn invalid_frontier_digest_is_rejected_before_restore_mutates_target() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();
    let mut invalid = spec.canonical_session.clone();
    invalid.event_frontier_digest = "A".repeat(64);

    assert_canonical_restore_rejected_before_mutation(
        &spec,
        &temp.path().join("invalid-digest.hel.zip"),
        &invalid,
        "64 lowercase hexadecimal characters",
    );
}

#[test]
fn unrestorable_queue_is_rejected_before_restore_mutates_target() {
    let temp = tempfile::tempdir().unwrap();
    let (spec, _) = fixture(temp.path());
    export_checkpoint(&spec).unwrap();

    let mut empty = spec.canonical_session.clone();
    empty.queued_prompts[0].content.clear();
    assert_canonical_restore_rejected_before_mutation(
        &spec,
        &temp.path().join("empty-prompt.hel.zip"),
        &empty,
        "has no content",
    );

    let mut malformed = spec.canonical_session.clone();
    malformed.queued_prompts[0].content = vec![json!({"type": "not_an_acp_block"})];
    assert_canonical_restore_rejected_before_mutation(
        &spec,
        &temp.path().join("malformed-prompt.hel.zip"),
        &malformed,
        "has invalid ACP content block 0",
    );
}

#[test]
fn parallel_repository_collection_preserves_manifest_order() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    git(&spec.workspace_root, &["clone", "-q", "app", "worker"]);
    let worker = spec.workspace_root.join("worker");
    let base = git(&worker, &["rev-parse", "HEAD"]);
    spec.repositories.push(CheckpointRepositorySpec {
        id: "worker".into(),
        relative_destination: "worker".into(),
        capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
        origin_override: None,
    });

    export_checkpoint(&spec).unwrap();
    let verified = read_archive_verified(&spec.output_path).unwrap();
    let repository_ids = verified
        .manifest
        .repositories
        .iter()
        .map(|repository| repository.metadata.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(repository_ids, ["app", "worker"]);
}
