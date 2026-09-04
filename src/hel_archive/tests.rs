use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};
use std::sync::{Barrier, Mutex};

use super::*;

#[derive(Default)]
struct FakeGit {
    outputs: Mutex<VecDeque<GitOutput>>,
    commands: Mutex<Vec<GitCommand>>,
}

impl FakeGit {
    fn with_outputs(outputs: impl IntoIterator<Item = GitOutput>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into_iter().collect()),
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<GitCommand> {
        self.commands.lock().unwrap().clone()
    }
}

impl GitCommandRunner for FakeGit {
    fn run(&self, _repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        self.commands.lock().unwrap().push(command.clone());
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("unexpected Git command: {:?}", command.arguments))
    }
}

struct CollectionGit {
    delta_count: u64,
    payload_barrier: Option<Barrier>,
    commands: Mutex<Vec<GitCommand>>,
}

impl CollectionGit {
    fn new(delta_count: u64, concurrent_payloads: bool) -> Self {
        Self {
            delta_count,
            payload_barrier: concurrent_payloads.then(|| Barrier::new(4)),
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<GitCommand> {
        self.commands.lock().unwrap().clone()
    }
}

impl GitCommandRunner for CollectionGit {
    fn run(&self, _repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        self.commands.lock().unwrap().push(command.clone());
        let arguments = command
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        let stdout = match arguments.first().map(|argument| argument.as_ref()) {
            Some("remote") => b"https://token@github.com/example/repo.git\n".to_vec(),
            Some("rev-parse") => format!("{}\n", "b".repeat(40)).into_bytes(),
            Some("merge-base") => format!("{}\n", "b".repeat(40)).into_bytes(),
            Some("for-each-ref") => format!("{}\n", "c".repeat(40)).into_bytes(),
            Some("symbolic-ref") => b"feature/hel\n".to_vec(),
            Some("rev-list") => format!("{}\n", self.delta_count).into_bytes(),
            Some("bundle") => {
                self.payload_barrier.as_ref().unwrap().wait();
                b"bundle".to_vec()
            }
            Some("diff") => {
                if let Some(barrier) = &self.payload_barrier {
                    barrier.wait();
                }
                if arguments.iter().any(|argument| argument == "--cached") {
                    b"staged".to_vec()
                } else {
                    b"unstaged".to_vec()
                }
            }
            Some("ls-files") => {
                if let Some(barrier) = &self.payload_barrier {
                    barrier.wait();
                }
                b"note.txt\0.env\0".to_vec()
            }
            other => return Err(anyhow!("unexpected Git command: {other:?}")),
        };
        Ok(git_ok(stdout))
    }
}

fn git_ok(stdout: impl Into<Vec<u8>>) -> GitOutput {
    GitOutput {
        status: 0,
        stdout: stdout.into(),
        stderr: Vec::new(),
    }
}

fn git(repository: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_line(repository: &Path, arguments: &[&str]) -> String {
    String::from_utf8(git(repository, arguments))
        .unwrap()
        .trim()
        .to_owned()
}

fn initialize_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Hel Test"]);
    git(path, &["config", "user.email", "hel@example.test"]);
}

fn clone_repository(parent: &Path, origin: &Path, name: &str) -> PathBuf {
    let destination = parent.join(name);
    git(
        parent,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            destination.to_str().unwrap(),
        ],
    );
    git(&destination, &["config", "user.name", "Hel Test"]);
    git(&destination, &["config", "user.email", "hel@example.test"]);
    destination
}

fn commit_file(repository: &Path, name: &str, contents: &[u8], message: &str) {
    fs::write(repository.join(name), contents).unwrap();
    git(repository, &["add", "."]);
    git(repository, &["commit", "-qm", message]);
}

fn tar_with_file(path: &str, contents: &[u8], mode: u32) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut output);
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
        builder.finish().unwrap();
    }
    output
}

fn repository(id: &str) -> RepositorySnapshot {
    RepositorySnapshot {
        metadata: RepositoryMetadata {
            id: id.to_string(),
            relative_destination: PathBuf::from(id),
            origin: format!("https://github.com/example/{id}.git"),
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            branch: Some("feature/hel".to_string()),
        },
        committed_bundle: format!("bundle-{id}").into_bytes(),
        staged_patch: format!("staged-{id}").into_bytes(),
        unstaged_patch: format!("unstaged-{id}").into_bytes(),
        untracked_tar: tar_with_file("scripts/tool.sh", b"#!/bin/sh\n", 0o755),
    }
}

fn checkpoint_bundle(head: &str, contents: impl Into<Vec<u8>>) -> CheckpointRepositoryBundle {
    CheckpointRepositoryBundle {
        metadata: RepositoryMetadata {
            id: "project".into(),
            relative_destination: "project".into(),
            origin: "https://github.com/example/project.git".into(),
            base_commit: String::new(),
            head_commit: head.into(),
            branch: Some("main".into()),
        },
        committed_bundle: contents.into(),
    }
}

#[test]
fn checkpoint_bundle_header_reports_declared_prerequisites_without_importing_pack() {
    let first = "a".repeat(40);
    let second = "B".repeat(40);
    let head = "c".repeat(40);
    let contents = format!(
        "# v2 git bundle\n-{first} first base\n-{second} second base\n{head} HEAD\n\nPACKnot-read"
    );

    assert_eq!(
        checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, contents)).unwrap(),
        [first, second.to_ascii_lowercase()]
    );
}

#[test]
fn checkpoint_bundle_header_accepts_v3_sha256_and_self_contained_bundles() {
    let prerequisite = "a".repeat(64);
    let head = "b".repeat(64);
    let contents = format!(
        "# v3 git bundle\n@object-format=sha256\n-{prerequisite} base\n{head} HEAD\n\nPACKnot-read"
    );
    assert_eq!(
        checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, contents)).unwrap(),
        [prerequisite]
    );

    let self_contained = format!("# v2 git bundle\n{head} HEAD\n\nPACKnot-read");
    assert!(
        checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, self_contained))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn checkpoint_bundle_header_rejects_metadata_mismatch_and_malformed_payloads() {
    let head = "b".repeat(40);
    let other = "c".repeat(40);
    let mismatch = format!("# v2 git bundle\n{other} HEAD\n\nPACKnot-read");
    let error = checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, mismatch))
        .unwrap_err()
        .to_string();
    assert!(error.contains("HEAD does not match"), "{error}");

    let malformed = format!("# v2 git bundle\n{head} HEAD\n\nnot-a-pack");
    let error = checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, malformed))
        .unwrap_err()
        .to_string();
    assert!(error.contains("no pack payload"), "{error}");
}

#[test]
fn checkpoint_without_a_bundle_uses_its_head_as_the_source_boundary() {
    let head = "b".repeat(40);
    assert_eq!(
        checkpoint_bundle_prerequisites(&checkpoint_bundle(&head, Vec::new())).unwrap(),
        [head]
    );
}

fn input() -> ArchiveInput {
    ArchiveInput {
        session: SessionManifest {
            id: "session-1".into(),
            title: "Forge Hel".into(),
            harness_kind: HarnessKind::Codex,
            profile_id: "codex-1".into(),
            native_session_id: "native-1".into(),
            created_at: "2026-08-09T10:00:00Z".into(),
            checkpointed_at: "2026-08-09T10:05:00Z".into(),
            hel_version: "0.1.0".into(),
            relay_version: "0.1.0".into(),
            adapter_version: "0.1.0".into(),
        },
        target: TargetManifest {
            template_id: "podman-rust".into(),
            target_kind: "local_podman".into(),
            details: BTreeMap::from([("image".into(), "fedora:latest".into())]),
        },
        bundle: BundleManifest {
            id: "hel".into(),
            primary_repository: "hel".into(),
        },
        canonical_session: CanonicalSessionSnapshot {
            event_frontier: 4,
            event_frontier_digest: "a".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: Some(104),
                session_title: Some("Forge Hel".into()),
                configuration: BTreeMap::from([(
                    "reasoning_effort".into(),
                    serde_json::json!("high"),
                )]),
            },
            transcript: vec![
                CanonicalTranscriptItem {
                    stable_id: "user-1".into(),
                    position: 1,
                    latest_content_event_ordinal: None,
                    created_at_ms: 100,
                    last_changed_at_ms: 100,
                    body: CanonicalTranscriptBody::User {
                        content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                    },
                },
                CanonicalTranscriptItem {
                    stable_id: "agent-2".into(),
                    position: 2,
                    latest_content_event_ordinal: Some(2),
                    created_at_ms: 101,
                    last_changed_at_ms: 103,
                    body: CanonicalTranscriptBody::Agent {
                        chunks: vec![serde_json::json!({
                            "content": {"type": "text", "text": "hi"}
                        })],
                        streaming: false,
                    },
                },
            ],
            queued_prompts: vec![CanonicalQueuedPrompt {
                command_id: "prompt-4".into(),
                kind: CanonicalQueuedCommandKind::Prompt,
                content: vec![serde_json::json!({"type": "text", "text": "next"})],
                queued_at_ms: 104,
            }],
        },
        native_artifacts: vec![NativeArtifact {
            relative_path: PathBuf::from("sessions/native-1/rollout.jsonl"),
            data: b"native transcript".to_vec(),
            mode: 0o600,
        }],
        repositories: vec![repository("hel"), repository("worker")],
    }
}

#[test]
fn archive_round_trip_verifies_multi_repo_payloads_and_mode() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let verified = write_archive_atomic(&path, &input()).unwrap();
    assert_eq!(verified.manifest.repositories.len(), 2);
    assert_eq!(verified.canonical_session, input().canonical_session);
    assert_eq!(verified.manifest.schema_version, ARCHIVE_SCHEMA_VERSION);
    assert_eq!(verified.archive_sha256.len(), 64);
    let repository_bundles = verify_repository_bundles_streaming(&path).unwrap();
    assert_eq!(repository_bundles.archive_sha256, verified.archive_sha256);
    assert_eq!(
        read_checkpoint_repository_bundles(&path)
            .unwrap()
            .iter()
            .map(|repository| (
                repository.metadata.id.as_str(),
                repository.committed_bundle.as_slice(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("hel", b"bundle-hel".as_slice()),
            ("worker", b"bundle-worker".as_slice()),
        ]
    );
    assert_eq!(
        repository_bundles
            .repositories
            .iter()
            .map(|repository| (
                repository.metadata.id.as_str(),
                repository.committed_bundle.as_slice(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("hel", b"bundle-hel".as_slice()),
            ("worker", b"bundle-worker".as_slice()),
        ]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn archive_preparation_borrows_existing_payload_bodies() {
    let archive_input = input();
    let native = archive_input.native_artifacts[0].data.as_slice();
    let bundle = archive_input.repositories[0].committed_bundle.as_slice();
    let (_manifest, payloads) = prepare_archive(&archive_input).unwrap();

    let canonical = payloads
        .iter()
        .find(|payload| payload.descriptor.role == PayloadRole::CanonicalSession)
        .unwrap();
    assert!(matches!(&canonical.data, Cow::Owned(_)));

    let prepared_native = payloads
        .iter()
        .find(|payload| matches!(&payload.descriptor.role, PayloadRole::NativeArtifact { .. }))
        .unwrap();
    assert!(matches!(&prepared_native.data, Cow::Borrowed(_)));
    assert_eq!(prepared_native.data.as_ptr(), native.as_ptr());

    let prepared_bundle = payloads
        .iter()
        .find(|payload| {
            matches!(
                &payload.descriptor.role,
                PayloadRole::GitBundle { repository_id } if repository_id == "hel"
            )
        })
        .unwrap();
    assert!(matches!(&prepared_bundle.data, Cow::Borrowed(_)));
    assert_eq!(prepared_bundle.data.as_ptr(), bundle.as_ptr());
}

#[test]
fn streaming_verification_does_not_retain_large_noncanonical_payloads() {
    const LARGE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.hel.zip");
    let mut archive_input = input();
    archive_input.repositories.clear();
    archive_input.native_artifacts = vec![NativeArtifact {
        relative_path: PathBuf::from("sessions/native-1/large-rollout.jsonl"),
        data: vec![b'x'; LARGE_PAYLOAD_BYTES],
        mode: 0o600,
    }];

    let verified = write_archive_atomic(&path, &archive_input).unwrap();
    drop(archive_input);

    let native = verified
        .manifest
        .payloads
        .iter()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap();
    assert_eq!(native.size, LARGE_PAYLOAD_BYTES as u64);
    assert_eq!(verified.canonical_session, input().canonical_session);
    let retained_metadata_bytes = serde_json::to_vec(&verified.manifest).unwrap().len()
        + serde_json::to_vec(&verified.canonical_session)
            .unwrap()
            .len()
        + verified.archive_sha256.len();
    assert!(retained_metadata_bytes < LARGE_PAYLOAD_BYTES / 100);
}

const TEST_PART_BYTES: usize = 4096;

fn zip_entry_names(path: &Path) -> Vec<String> {
    let archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    archive.file_names().map(str::to_owned).collect()
}

fn zip_entry_method(path: &Path, name: &str) -> CompressionMethod {
    let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    archive.by_name(name).unwrap().compression()
}

/// Writes an archive whose native artifact and first untracked tar are both
/// larger than `TEST_PART_BYTES`, so the sharded paths are exercised without
/// allocating the production 16 MiB threshold.
fn sharded_input() -> (ArchiveInput, Vec<u8>, Vec<u8>) {
    let mut archive_input = input();
    let native = b"native rollout line\n".repeat(2_000);
    let untracked = tar_with_file(
        "notes/large.txt",
        &b"untracked payload line\n".repeat(1_000),
        0o644,
    );
    archive_input.native_artifacts[0].data = native.clone();
    archive_input.repositories[0].untracked_tar = untracked.clone();
    (archive_input, native, untracked)
}

#[test]
fn oversized_payloads_shard_into_parts_and_read_back_whole() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sharded.hel.zip");
    let (archive_input, native, untracked) = sharded_input();
    write_archive_installed_with_part_size(&path, &archive_input, TEST_PART_BYTES).unwrap();

    let native_path = "native/sessions/native-1/rollout.jsonl";
    let names = zip_entry_names(&path);
    assert!(
        !names.contains(&native_path.to_string()),
        "a sharded payload owns no entry of its own: {names:?}"
    );
    let part_names = names
        .iter()
        .filter(|name| name.starts_with(&format!("{native_path}{PAYLOAD_PART_SUFFIX}")))
        .count();
    assert_eq!(part_names, native.len().div_ceil(TEST_PART_BYTES));
    assert!(part_names > 1);

    let metadata = verify_archive_streaming(&path).unwrap();
    assert_eq!(
        metadata.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_SHARDED
    );
    assert_eq!(metadata.canonical_session, archive_input.canonical_session);

    let verified = read_archive_verified(&path).unwrap();
    assert_eq!(verified.archive_sha256, metadata.archive_sha256);
    assert_eq!(
        verified
            .payload_by_role(&PayloadRole::NativeArtifact {
                relative_path: PathBuf::from("sessions/native-1/rollout.jsonl"),
            })
            .unwrap(),
        native.as_slice()
    );
    assert_eq!(
        verified
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: "hel".into(),
            })
            .unwrap(),
        untracked.as_slice()
    );
    assert_eq!(
        verified.canonical_session().unwrap(),
        archive_input.canonical_session
    );
    assert!(
        verified
            .payloads
            .keys()
            .all(|path| !path.contains(PAYLOAD_PART_SUFFIX)),
        "restore consumers only ever see whole payload paths"
    );
    assert!(verified.payloads.contains_key(native_path));
}

#[test]
fn payload_parts_follow_the_threshold_and_never_split_stored_payloads() {
    let bundle = PayloadRole::GitBundle {
        repository_id: "hel".into(),
    };
    let artifact = PayloadRole::NativeArtifact {
        relative_path: PathBuf::from("rollout.jsonl"),
    };
    assert_eq!(payload_compression(&bundle), CompressionMethod::Stored);
    assert_eq!(payload_compression(&artifact), CompressionMethod::Zstd);

    let body = vec![b'x'; 10];
    assert!(
        plan_payload_parts(
            "repositories/hel/committed.bundle",
            &body,
            CompressionMethod::Stored,
            4,
        )
        .is_empty()
    );
    assert!(
        plan_payload_parts("native/rollout.jsonl", &body, CompressionMethod::Zstd, 10).is_empty(),
        "a payload at the threshold stays whole"
    );
    let parts = plan_payload_parts("native/rollout.jsonl", &body, CompressionMethod::Zstd, 4);
    assert_eq!(
        parts
            .iter()
            .map(|part| part.path.as_str())
            .collect::<Vec<_>>(),
        [
            "native/rollout.jsonl.helpart.00000",
            "native/rollout.jsonl.helpart.00001",
            "native/rollout.jsonl.helpart.00002",
        ]
    );
    assert_eq!(
        parts.iter().map(|part| part.size).collect::<Vec<_>>(),
        [4, 4, 2]
    );
    assert_eq!(parts[2].sha256, digest_bytes(&body[8..]));
}

#[test]
fn git_bundles_are_stored_and_other_payloads_use_zstandard() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stored-bundle.hel.zip");
    write_archive_atomic(&path, &input()).unwrap();

    assert_eq!(
        zip_entry_method(&path, "repositories/hel/committed.bundle"),
        CompressionMethod::Stored
    );
    assert_eq!(
        zip_entry_method(&path, CANONICAL_SESSION_PATH),
        CompressionMethod::Zstd
    );
    assert_eq!(
        zip_entry_method(&path, "repositories/hel/untracked.tar"),
        CompressionMethod::Zstd
    );

    let verified = read_archive_verified(&path).unwrap();
    assert_eq!(
        verified
            .payload_by_role(&PayloadRole::GitBundle {
                repository_id: "hel".into(),
            })
            .unwrap(),
        b"bundle-hel"
    );
}

/// Replaces a repository's untracked tar in an already prepared archive,
/// resharding it so the manifest and the payload body stay consistent.
fn replace_untracked_tar(
    manifest: &mut ArchiveManifest,
    payloads: &mut [PendingPayload<'_>],
    repository_id: &str,
    tar: Vec<u8>,
    part_bytes: usize,
) {
    let role = PayloadRole::GitUntrackedTar {
        repository_id: repository_id.to_string(),
    };
    let descriptor = manifest
        .payloads
        .iter_mut()
        .find(|payload| payload.role == role)
        .unwrap();
    descriptor.size = tar.len() as u64;
    descriptor.sha256 = digest_bytes(&tar);
    descriptor.parts =
        plan_payload_parts(&descriptor.path, &tar, CompressionMethod::Zstd, part_bytes);
    let descriptor = descriptor.clone();
    let payload = payloads
        .iter_mut()
        .find(|payload| payload.descriptor.path == descriptor.path)
        .unwrap();
    payload.descriptor = descriptor;
    payload.data = Cow::Owned(tar);
}

#[test]
fn streaming_verification_parses_a_sharded_untracked_tar_for_safety() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsafe-sharded-untracked.hel.zip");
    let (archive_input, _, _) = sharded_input();
    let (mut manifest, mut payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    let malicious = tar_with_file(".env", &b"secret\n".repeat(2_000), 0o600);
    assert!(malicious.len() > TEST_PART_BYTES);
    replace_untracked_tar(
        &mut manifest,
        &mut payloads,
        "hel",
        malicious,
        TEST_PART_BYTES,
    );
    let mut file = File::create(&path).unwrap();
    write_zip(&mut file, &manifest, &payloads).unwrap();
    drop(file);

    let error = format!("{:#}", verify_archive_streaming(&path).unwrap_err());
    assert!(error.contains("credential/config path"), "{error}");
}

#[test]
fn sharded_manifests_reject_reordered_or_missing_parts() {
    let directory = tempfile::tempdir().unwrap();
    let (archive_input, _, _) = sharded_input();

    let (mut manifest, payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    let descriptor = manifest
        .payloads
        .iter_mut()
        .find(|payload| !payload.parts.is_empty())
        .unwrap();
    descriptor.parts.swap(0, 1);
    let reordered = directory.path().join("reordered.hel.zip");
    let mut file = File::create(&reordered).unwrap();
    write_zip(&mut file, &manifest, &payloads).unwrap();
    drop(file);
    let error = format!("{:#}", verify_archive_streaming(&reordered).unwrap_err());
    assert!(error.contains("part 0 has unexpected path"), "{error}");

    let (mut manifest, payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    let descriptor = manifest
        .payloads
        .iter_mut()
        .find(|payload| !payload.parts.is_empty())
        .unwrap();
    descriptor.parts.pop().unwrap();
    let truncated = directory.path().join("truncated.hel.zip");
    let mut file = File::create(&truncated).unwrap();
    write_zip(&mut file, &manifest, &payloads).unwrap();
    drop(file);
    let error = format!("{:#}", verify_archive_streaming(&truncated).unwrap_err());
    assert!(error.contains("parts cover"), "{error}");
}

#[test]
fn readers_without_part_support_reject_sharded_archives() {
    // The schema-2 wire types as a build that predates sharding sees them.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyPayloadDescriptor {
        #[allow(dead_code)]
        path: String,
        #[allow(dead_code)]
        sha256: String,
        #[allow(dead_code)]
        size: u64,
        #[allow(dead_code)]
        mode: u32,
        #[allow(dead_code)]
        role: PayloadRole,
    }
    #[derive(Debug, Deserialize)]
    struct LegacyManifest {
        schema_version: u32,
        payloads: Vec<LegacyPayloadDescriptor>,
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sharded.hel.zip");
    let (archive_input, _, _) = sharded_input();
    write_archive_installed_with_part_size(&path, &archive_input, TEST_PART_BYTES).unwrap();
    let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let mut manifest_bytes = Vec::new();
    archive
        .by_name(MANIFEST_PATH)
        .unwrap()
        .read_to_end(&mut manifest_bytes)
        .unwrap();

    // Gate 1: the version an old build compares for equality against 2.
    let header: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(
        header["schema_version"],
        serde_json::json!(ARCHIVE_SCHEMA_VERSION_SHARDED)
    );

    // Gate 2: even ignoring the version, the old payload type cannot parse
    // a descriptor that carries parts.
    let error = serde_json::from_slice::<LegacyManifest>(&manifest_bytes).unwrap_err();
    assert!(
        error.to_string().contains("unknown field `parts`"),
        "{error}"
    );

    // Gate 3: an archive that claims schema 2 while carrying parts, or
    // schema 3 while carrying none, is rejected by this build too.
    let (mut manifest, payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    manifest.schema_version = ARCHIVE_SCHEMA_VERSION;
    let downgraded = directory.path().join("downgraded.hel.zip");
    let mut file = File::create(&downgraded).unwrap();
    write_zip(&mut file, &manifest, &payloads).unwrap();
    drop(file);
    let error = format!("{:#}", read_archive_verified(&downgraded).unwrap_err());
    assert!(
        error.contains("incompatible Mjolnir archive schema 2; this build requires schema 3"),
        "{error}"
    );

    // An archive with no sharded payload stays schema 2 and still parses
    // with the old wire types, so small sessions keep full compatibility.
    let whole = directory.path().join("whole.hel.zip");
    write_archive_atomic(&whole, &input()).unwrap();
    let mut archive = zip::ZipArchive::new(File::open(&whole).unwrap()).unwrap();
    let mut whole_manifest = Vec::new();
    archive
        .by_name(MANIFEST_PATH)
        .unwrap()
        .read_to_end(&mut whole_manifest)
        .unwrap();
    let legacy: LegacyManifest = serde_json::from_slice(&whole_manifest).unwrap();
    assert_eq!(legacy.schema_version, ARCHIVE_SCHEMA_VERSION);
    assert!(!legacy.payloads.is_empty());
}

#[test]
fn a_corrupt_part_fails_the_parallel_read_with_the_part_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-part.hel.zip");
    let (archive_input, _, _) = sharded_input();
    write_archive_installed_with_part_size(&path, &archive_input, TEST_PART_BYTES).unwrap();

    let corrupt = "native/sessions/native-1/rollout.jsonl.helpart.00001";
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let entry = archive.by_name(corrupt).unwrap();
    let data_start = entry.data_start();
    drop(entry);
    let mut file = archive.into_inner();
    file.seek(SeekFrom::Start(data_start)).unwrap();
    file.write_all(b"\xff\xff\xff\xff").unwrap();
    drop(file);

    for error in [
        verify_archive_streaming(&path).unwrap_err(),
        read_archive_verified(&path).unwrap_err(),
    ] {
        let error = format!("{error:#}");
        assert!(error.contains(corrupt), "{error}");
    }
}

#[test]
fn writing_the_same_input_twice_produces_identical_archives() {
    let directory = tempfile::tempdir().unwrap();
    let (archive_input, _, _) = sharded_input();
    let first = directory.path().join("first.hel.zip");
    let second = directory.path().join("second.hel.zip");
    write_archive_installed_with_part_size(&first, &archive_input, TEST_PART_BYTES).unwrap();
    write_archive_installed_with_part_size(&second, &archive_input, TEST_PART_BYTES).unwrap();
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert_eq!(zip_entry_names(&first), zip_entry_names(&second));
}

#[test]
fn streaming_verification_rejects_noncanonical_corruption() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-native.hel.zip");
    let mut archive_input = input();
    archive_input.repositories.clear();
    archive_input.native_artifacts[0].data = b"native payload".to_vec();
    write_archive_atomic(&path, &archive_input).unwrap();

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let entry = archive
        .by_name("native/sessions/native-1/rollout.jsonl")
        .unwrap();
    let data_start = entry.data_start();
    drop(entry);
    let mut file = archive.into_inner();
    file.seek(SeekFrom::Start(data_start)).unwrap();
    file.write_all(b"X").unwrap();
    drop(file);

    assert!(verify_archive_streaming(&path).is_err());
}

#[test]
fn streaming_verification_rejects_unsafe_extra_zip_entry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsafe-extra.hel.zip");
    let archive_input = input();
    let (manifest, payloads) = prepare_archive(&archive_input).unwrap();
    let file = File::create(&path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    writer
        .start_file(
            MANIFEST_PATH,
            SimpleFileOptions::default().unix_permissions(0o600),
        )
        .unwrap();
    writer
        .write_all(&serde_json::to_vec_pretty(&manifest).unwrap())
        .unwrap();
    for payload in payloads {
        writer
            .start_file(
                payload.descriptor.path,
                SimpleFileOptions::default().unix_permissions(payload.descriptor.mode),
            )
            .unwrap();
        writer.write_all(&payload.data).unwrap();
    }
    writer
        .start_file("../escape", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"unsafe").unwrap();
    writer.finish().unwrap();

    let error = verify_archive_streaming(&path).unwrap_err();
    assert!(format!("{error:#}").contains("unsafe ZIP entry path"));
}

#[test]
fn streaming_verification_parses_untracked_tar_for_safety() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsafe-untracked.hel.zip");
    let archive_input = input();
    let (mut manifest, mut payloads) = prepare_archive(&archive_input).unwrap();
    let malicious = tar_with_file(".env", b"secret", 0o600);
    let descriptor = manifest
        .payloads
        .iter_mut()
        .find(|payload| {
            matches!(
                payload.role,
                PayloadRole::GitUntrackedTar { ref repository_id }
                    if repository_id == "hel"
            )
        })
        .unwrap();
    descriptor.size = malicious.len() as u64;
    descriptor.sha256 = digest_bytes(&malicious);
    let payload = payloads
        .iter_mut()
        .find(|payload| payload.descriptor.path == descriptor.path)
        .unwrap();
    payload.descriptor = descriptor.clone();
    payload.data = Cow::Owned(malicious);
    let mut file = File::create(&path).unwrap();
    write_zip(&mut file, &manifest, &payloads).unwrap();
    drop(file);

    let error = verify_archive_streaming(&path).unwrap_err();
    assert!(format!("{error:#}").contains("credential/config path"));
}

#[test]
fn corruption_is_detected_by_payload_digest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    write_archive_atomic(&path, &input()).unwrap();

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let entry = archive.by_name(CANONICAL_SESSION_PATH).unwrap();
    let data_start = entry.data_start();
    drop(entry);
    let mut file = archive.into_inner();
    file.seek(SeekFrom::Start(data_start)).unwrap();
    file.write_all(b"X").unwrap();
    drop(file);

    assert!(read_archive_verified(&path).is_err());
}

#[test]
fn old_and_future_schemas_are_rejected_explicitly() {
    for schema_version in [1, ARCHIVE_SCHEMA_VERSION + 1] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.hel.zip");
        let archive_input = input();
        let (mut manifest, payloads) = prepare_archive(&archive_input).unwrap();
        manifest.schema_version = schema_version;
        let mut file = File::create(&path).unwrap();
        write_zip(&mut file, &manifest, &payloads).unwrap();
        drop(file);

        let error = read_archive_verified(&path).unwrap_err();
        let error = format!("{error:#}");
        assert!(
                error.contains(&format!(
                    "incompatible Mjolnir archive schema {schema_version}; this build requires schema {ARCHIVE_SCHEMA_VERSION}"
                )),
                "{error}"
            );
    }
}

#[test]
fn schema_two_wire_rejects_unknown_and_omitted_required_fields() {
    let mut canonical = serde_json::to_value(input().canonical_session).unwrap();
    canonical
        .as_object_mut()
        .unwrap()
        .insert("revision".into(), serde_json::json!(4));
    assert!(serde_json::from_value::<CanonicalSessionSnapshot>(canonical).is_err());

    let mut canonical = serde_json::to_value(input().canonical_session).unwrap();
    canonical["session"]
        .as_object_mut()
        .unwrap()
        .remove("configuration");
    assert!(serde_json::from_value::<CanonicalSessionSnapshot>(canonical).is_err());

    let mut target = serde_json::to_value(input().target).unwrap();
    target.as_object_mut().unwrap().remove("details");
    assert!(serde_json::from_value::<TargetManifest>(target).is_err());

    let mut metadata = serde_json::to_value(repository("repo").metadata).unwrap();
    metadata.as_object_mut().unwrap().remove("base_commit");
    assert!(serde_json::from_value::<RepositoryMetadata>(metadata).is_err());
}

#[test]
fn content_matching_ignores_the_frontier_and_the_activity_watermark() {
    let archived = input().canonical_session;
    let mut latched = archived.clone();
    // Checkpoint bookkeeping alone moves the frontier and the watermark.
    latched.event_frontier += 6;
    latched.event_frontier_digest = "b".repeat(64);
    latched.session.last_activity_at_ms = Some(9_999);

    assert!(archived.content_matches(&latched));
    assert!(latched.content_matches(&archived));
}

#[test]
fn content_matching_rejects_new_transcript_queue_or_title_content() {
    let archived = input().canonical_session;

    let mut extra_transcript = archived.clone();
    extra_transcript.transcript.push(CanonicalTranscriptItem {
        stable_id: "user-3".into(),
        position: 3,
        latest_content_event_ordinal: None,
        created_at_ms: 105,
        last_changed_at_ms: 105,
        body: CanonicalTranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": "again"})],
        },
    });
    assert!(!archived.content_matches(&extra_transcript));

    let mut extra_prompt = archived.clone();
    extra_prompt.queued_prompts.push(CanonicalQueuedPrompt {
        command_id: "prompt-5".into(),
        kind: CanonicalQueuedCommandKind::Prompt,
        content: vec![serde_json::json!({"type": "text", "text": "later"})],
        queued_at_ms: 105,
    });
    assert!(!archived.content_matches(&extra_prompt));

    let mut retitled = archived.clone();
    retitled.session.session_title = Some("Reforge Hel".into());
    assert!(!archived.content_matches(&retitled));

    let mut reconfigured = archived.clone();
    reconfigured
        .session
        .configuration
        .insert("reasoning_effort".into(), serde_json::json!("low"));
    assert!(!archived.content_matches(&reconfigured));
}

#[test]
fn canonical_session_rejects_duplicate_or_out_of_frontier_items() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.transcript[1].stable_id = "user-1".into();
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("duplicate canonical transcript item id"));

    invalid = input();
    invalid.canonical_session.transcript[1].position = 5;
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("beyond event frontier"));
    assert!(!path.exists());
}

#[test]
fn canonical_session_requires_a_valid_latest_agent_content_ordinal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.transcript[1].latest_content_event_ordinal = None;
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("has no latest content ordinal"));

    invalid = input();
    invalid.canonical_session.transcript[1].latest_content_event_ordinal = Some(5);
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("invalid latest content ordinal"));

    invalid = input();
    invalid.canonical_session.transcript[0].latest_content_event_ordinal = Some(1);
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("non-agent transcript item"));
}

#[test]
fn canonical_session_rejects_a_stream_still_open_at_the_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.transcript[1].body = CanonicalTranscriptBody::Agent {
        chunks: vec![serde_json::json!({
            "content": {"type": "text", "text": "partial"}
        })],
        streaming: true,
    };

    let error = write_archive_atomic(&path, &invalid).unwrap_err();

    assert!(format!("{error:#}").contains("still streaming at the checkpoint barrier"));
    assert!(!path.exists());
}

#[test]
fn canonical_session_rejects_non_idle_execution_at_the_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.session.execution =
        CanonicalExecutionState::Running { started_at_ms: 105 };

    let error = write_archive_atomic(&path, &invalid).unwrap_err();

    assert!(format!("{error:#}").contains("not idle at the checkpoint barrier"));
    assert!(!path.exists());
}

#[test]
fn canonical_session_rejects_an_invalid_event_frontier_digest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.event_frontier_digest = "A".repeat(64);

    let error = write_archive_atomic(&path, &invalid).unwrap_err();

    assert!(format!("{error:#}").contains("64 lowercase hexadecimal characters"));
    assert!(!path.exists());

    invalid = input();
    invalid.canonical_session.event_frontier_digest = EVENT_FRONTIER_GENESIS_DIGEST.into();
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("frontier and digest are inconsistent"));
    assert!(!path.exists());
}

#[test]
fn canonical_session_rejects_unrestorable_queued_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    let mut invalid = input();
    invalid.canonical_session.queued_prompts[0].content.clear();

    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("has no content"));
    assert!(!path.exists());

    invalid.canonical_session.queued_prompts[0].content =
        vec![serde_json::json!({"type": "not_an_acp_content_block"})];
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("has invalid ACP content block 0"));
    assert!(!path.exists());

    invalid = input();
    invalid.canonical_session.queued_prompts[0].kind = CanonicalQueuedCommandKind::SetConfig {
        key: "model".into(),
        value: "  ".into(),
    };
    let error = write_archive_atomic(&path, &invalid).unwrap_err();
    assert!(format!("{error:#}").contains("is incomplete"));
    assert!(!path.exists());
}

#[test]
fn queued_entries_written_before_config_changes_load_as_prompts() {
    let stored: CanonicalQueuedPrompt = serde_json::from_value(serde_json::json!({
        "command_id": "queued-1",
        "content": [{"type": "text", "text": "hello"}],
        "queued_at_ms": 5,
    }))
    .unwrap();
    assert_eq!(stored.kind, CanonicalQueuedCommandKind::Prompt);
    // A prompt entry still serializes exactly as it did before.
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        serde_json::json!({
            "command_id": "queued-1",
            "content": [{"type": "text", "text": "hello"}],
            "queued_at_ms": 5,
        })
    );

    let config = CanonicalQueuedPrompt {
        command_id: "queued-2".into(),
        kind: CanonicalQueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        },
        content: vec![serde_json::json!({"type": "text", "text": "/model sonnet"})],
        queued_at_ms: 6,
    };
    let encoded = serde_json::to_value(&config).unwrap();
    assert_eq!(
        serde_json::from_value::<CanonicalQueuedPrompt>(encoded).unwrap(),
        config
    );
}

#[test]
fn traversal_and_credentials_are_rejected_before_destination_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    fs::write(&path, b"existing archive").unwrap();
    let mut unsafe_input = input();
    unsafe_input.native_artifacts[0].relative_path = PathBuf::from("../auth.json");
    assert!(write_archive_atomic(&path, &unsafe_input).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing archive");

    unsafe_input.native_artifacts[0].relative_path = PathBuf::from("auth.json");
    assert!(write_archive_atomic(&path, &unsafe_input).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing archive");
}

#[test]
fn close_verification_blocks_teardown_on_archive_failure() {
    let directory = tempfile::tempdir().unwrap();
    let mut unsafe_input = input();
    unsafe_input.repositories[0].metadata.origin =
        "https://secret@github.com/example/hel.git".into();
    let result = checkpoint_for_close(&directory.path().join("x.hel.zip"), &unsafe_input);
    assert!(!result.teardown_allowed());
    assert!(matches!(result, CloseVerification::Blocked { .. }));
}

#[test]
fn untracked_tar_preserves_executable_mode_and_safe_symlink() {
    let source = tempfile::tempdir().unwrap();
    fs::create_dir_all(source.path().join("scripts")).unwrap();
    let tool = source.path().join("scripts/tool");
    fs::write(&tool, b"tool").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("tool", source.path().join("scripts/current")).unwrap();
    }
    #[cfg(unix)]
    let paths = b"scripts/tool\0scripts/current\0".to_vec();
    #[cfg(not(unix))]
    let paths = b"scripts/tool\0".to_vec();
    let tar = build_untracked_tar(source.path(), &paths, &|_| Ok(())).unwrap();
    validate_untracked_tar(&tar).unwrap();

    let destination = tempfile::tempdir().unwrap();
    restore_untracked_tar(destination.path(), &tar).unwrap();
    assert_eq!(
        fs::read(destination.path().join("scripts/tool")).unwrap(),
        b"tool"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(destination.path().join("scripts/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(destination.path().join("scripts/current")).unwrap(),
            PathBuf::from("tool")
        );
    }
}

#[test]
fn untracked_tar_progress_can_cancel_before_opening_the_next_file() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("first.txt"), b"first").unwrap();
    let paths = b"first.txt\0missing.txt\0";
    let seen = std::sync::Mutex::new(Vec::new());

    let error = build_untracked_tar(source.path(), paths, &|progress| {
        let GitSnapshotProgress::UntrackedFile { current, total, .. } = progress;
        seen.lock().unwrap().push((current, total));
        ensure!(current < 2, "cancelled by test");
        Ok(())
    })
    .unwrap_err();

    assert!(error.to_string().contains("cancelled by test"));
    assert_eq!(*seen.lock().unwrap(), vec![(1, 2), (2, 2)]);
}

#[test]
fn malicious_untracked_tar_is_rejected() {
    assert!(validate_archive_relative_path(Path::new("../escape")).is_err());
    for path in [
        ".env",
        ".credentials.json",
        "auth.toml",
        "vendor_credentials.json",
        "vendor-credentials.json",
        "nested/.credentials.json",
    ] {
        let tar = tar_with_file(path, b"secret", 0o600);
        assert!(
            validate_untracked_tar(&tar).is_err(),
            "untracked tar accepted '{path}'"
        );
    }
}

/// The untracked payload is built from paths Git reports, so credential
/// files a repository never tracked must be dropped as the tar is built,
/// not merely rejected later.
#[test]
fn untracked_tar_omits_credential_files_from_the_worktree() {
    let source = tempfile::tempdir().unwrap();
    fs::create_dir_all(source.path().join("nested")).unwrap();
    for name in [
        ".credentials.json",
        "auth.toml",
        "vendor_credentials.json",
        "vendor-credentials.json",
        "nested/.credentials.json",
        "note.txt",
    ] {
        fs::write(source.path().join(name), b"payload").unwrap();
    }
    let paths = b".credentials.json\0auth.toml\0vendor_credentials.json\0\
vendor-credentials.json\0nested/.credentials.json\0note.txt\0";

    let tar = build_untracked_tar(source.path(), paths, &|_| Ok(())).unwrap();
    validate_untracked_tar(&tar).unwrap();

    let mut archive = tar::Archive::new(Cursor::new(&tar));
    let entries: Vec<_> = archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect();
    assert_eq!(entries, vec![PathBuf::from("note.txt")]);
}

#[test]
fn unsafe_zip_entry_is_rejected_even_without_extraction() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsafe.hel.zip");
    {
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"no").unwrap();
        writer.finish().unwrap();
    }
    assert!(read_archive_verified(&path).is_err());
    assert!(!directory.path().join("escape").exists());
}

#[test]
fn git_collection_is_abstracted_redacts_origin_and_skips_credentials() {
    let repository = tempfile::tempdir().unwrap();
    fs::write(repository.path().join("note.txt"), b"keep").unwrap();
    fs::write(repository.path().join(".env"), b"SECRET=nope").unwrap();
    let runner = CollectionGit::new(0, false);
    let snapshot = collect_git_snapshot(
        &runner,
        repository.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: PathBuf::from("repo"),
            history: GitHistoryMode::DeltaFrom("a".repeat(40)),
            origin_override: None,
        },
    )
    .unwrap();
    assert_eq!(
        snapshot.metadata.origin,
        "https://github.com/example/repo.git"
    );
    let mut archive = tar::Archive::new(Cursor::new(&snapshot.untracked_tar));
    let paths: Vec<_> = archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect();
    assert_eq!(paths, vec![PathBuf::from("note.txt")]);
    assert_eq!(runner.commands().len(), 9);
}

#[test]
fn git_collection_can_omit_untracked_files_without_losing_tracked_changes() {
    let repository = tempfile::tempdir().unwrap();
    fs::write(repository.path().join("note.txt"), b"untracked").unwrap();
    let runner = CollectionGit::new(0, false);
    let snapshot = collect_git_snapshot_with_progress(
        &runner,
        repository.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: PathBuf::from("repo"),
            history: GitHistoryMode::DeltaFrom("a".repeat(40)),
            origin_override: None,
        },
        false,
        &|_| Ok(()),
    )
    .unwrap();

    assert_eq!(snapshot.staged_patch, b"staged");
    assert_eq!(snapshot.unstaged_patch, b"unstaged");
    assert!(snapshot.untracked_tar.is_empty());
    assert!(runner.commands().iter().all(|command| {
        command
            .arguments
            .first()
            .is_none_or(|argument| argument != "ls-files")
    }));
}

#[test]
fn git_collection_builds_independent_payloads_concurrently() {
    let repository = tempfile::tempdir().unwrap();
    fs::write(repository.path().join("note.txt"), b"keep").unwrap();
    fs::write(repository.path().join(".env"), b"SECRET=nope").unwrap();
    let runner = CollectionGit::new(1, true);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();

    let snapshot = pool
        .install(|| {
            collect_git_snapshot(
                &runner,
                repository.path(),
                &GitCollectionSpec {
                    id: "repo".into(),
                    relative_destination: PathBuf::from("repo"),
                    history: GitHistoryMode::DeltaFrom("a".repeat(40)),
                    origin_override: None,
                },
            )
        })
        .unwrap();

    assert_eq!(snapshot.committed_bundle, b"bundle");
    assert_eq!(snapshot.staged_patch, b"staged");
    assert_eq!(snapshot.unstaged_patch, b"unstaged");
    assert_eq!(runner.commands().len(), 10);
}

/// Checkpoint work runs with nobody watching the terminal it inherits, so
/// a Git child that would ask for a password or a host key has to fail
/// instead of holding the checkpoint open until its deadline.
#[test]
fn system_git_children_cannot_stop_on_a_prompt() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());

    // A `!` alias runs through the shell with the environment Git handed
    // its children, so this reports what a real fetch would inherit.
    let output = SystemGit
        .run(
            repository.path(),
            &GitCommand {
                arguments: ["-c", "alias.helenv=!printenv", "helenv"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                stdin: Vec::new(),
                env: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(
        output.status,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let environment = String::from_utf8(output.stdout).unwrap();
    let value = |name: &str| {
        environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map(str::to_owned)
    };
    assert_eq!(value("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(value("GIT_NO_LAZY_FETCH").as_deref(), Some("1"));
    assert_eq!(
        value("GIT_SSH_COMMAND"),
        std::env::var("GIT_SSH_COMMAND")
            .ok()
            .or_else(|| Some(NON_INTERACTIVE_GIT_SSH_COMMAND.to_owned())),
        "an operator's own SSH command wins; otherwise SSH runs in batch mode"
    );
}

#[test]
fn metadata_snapshot_requires_git_head_but_omits_repository_contents() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q", "-b", "main"]);
    git(repository.path(), &["config", "user.name", "Hel Test"]);
    git(
        repository.path(),
        &["config", "user.email", "hel@example.test"],
    );
    fs::write(repository.path().join("tracked.txt"), b"base\n").unwrap();
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "-qm", "base"]);
    fs::write(repository.path().join("tracked.txt"), b"dirty\n").unwrap();
    fs::write(repository.path().join("untracked.txt"), b"untracked\n").unwrap();

    let snapshot = collect_git_metadata_snapshot(
        &SystemGit,
        repository.path(),
        &GitCollectionSpec {
            id: "project".into(),
            relative_destination: "project".into(),
            history: GitHistoryMode::NoBundle,
            origin_override: None,
        },
    )
    .unwrap();

    assert_eq!(snapshot.metadata.base_commit, snapshot.metadata.head_commit);
    assert!(snapshot.committed_bundle.is_empty());
    assert!(snapshot.staged_patch.is_empty());
    assert!(snapshot.unstaged_patch.is_empty());
    assert!(snapshot.untracked_tar.is_empty());

    fs::remove_dir_all(repository.path().join(".git")).unwrap();
    let error = collect_git_metadata_snapshot(
        &SystemGit,
        repository.path(),
        &GitCollectionSpec {
            id: "project".into(),
            relative_destination: "project".into(),
            history: GitHistoryMode::NoBundle,
            origin_override: None,
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("repository has no valid Git HEAD"));
}

#[test]
fn session_delta_bundles_commits_missing_from_every_origin_ref() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "base.txt", b"base\n", "base");
    git(&origin, &["checkout", "-q", "-b", "release"]);
    commit_file(&origin, "release.txt", b"release\n", "release");
    git(&origin, &["checkout", "-q", "main"]);
    let source = clone_repository(directory.path(), &origin, "source");
    commit_file(&source, "first.txt", b"first\n", "first");
    commit_file(&source, "second.txt", b"second\n", "second");
    let head = git_line(&source, &["rev-parse", "HEAD"]);

    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::SessionDelta,
            origin_override: None,
        },
    )
    .unwrap();

    assert!(!snapshot.committed_bundle.is_empty());
    assert_eq!(snapshot.metadata.base_commit, "");
    assert_eq!(snapshot.metadata.head_commit, head);

    let destination = clone_repository(directory.path(), &origin, "restored");
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
    assert_eq!(git_line(&destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_line(
            &destination,
            &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"]
        ),
        "2"
    );
    assert_eq!(
        fs::read(destination.join("release.txt"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

/// Provisioning must fetch a local repository's history before restoring
/// into it: the delta bundle carries only the commits origin lacks.
#[test]
fn session_delta_restore_requires_the_fetched_origin_history() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "base.txt", b"base\n", "base");
    let source = clone_repository(directory.path(), &origin, "source");
    commit_file(&source, "session.txt", b"session\n", "session work");
    let head = git_line(&source, &["rev-parse", "HEAD"]);
    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::SessionDelta,
            origin_override: None,
        },
    )
    .unwrap();

    let destination = directory.path().join("target");
    initialize_repository(&destination);
    let unfetched = restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap_err();
    assert!(
        format!("{unfetched:#}").contains("fetch committed delta bundle"),
        "{unfetched:#}"
    );

    git(
        &destination,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&destination, &["fetch", "-q", "origin"]);
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();

    assert_eq!(git_line(&destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read(destination.join("session.txt")).unwrap(),
        b"session\n"
    );
}

#[test]
fn session_delta_errors_without_origin_refs() {
    let source = tempfile::tempdir().unwrap();
    initialize_repository(source.path());
    commit_file(source.path(), "base.txt", b"base\n", "base");

    let error = collect_git_snapshot(
        &SystemGit,
        source.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::SessionDelta,
            origin_override: None,
        },
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("repository has no origin refs to delta against"),
        "{error:#}"
    );
}

#[test]
fn delta_from_errors_when_the_base_is_unresolvable() {
    let source = tempfile::tempdir().unwrap();
    initialize_repository(source.path());
    commit_file(source.path(), "old.txt", b"old\n", "old root");

    let missing = collect_git_snapshot(
        &SystemGit,
        source.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::DeltaFrom("refs/hel/missing".into()),
            origin_override: None,
        },
    )
    .unwrap_err();
    assert!(
        format!("{missing:#}").contains("delta base refs/hel/missing is unresolvable"),
        "{missing:#}"
    );

    let old_head = git_line(source.path(), &["rev-parse", "HEAD"]);
    git(source.path(), &["checkout", "-q", "--orphan", "rewritten"]);
    git(source.path(), &["rm", "-q", "-rf", "."]);
    commit_file(source.path(), "new.txt", b"new root\n", "new root");
    let unrelated = collect_git_snapshot(
        &SystemGit,
        source.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::DeltaFrom(old_head.clone()),
            origin_override: None,
        },
    )
    .unwrap_err();
    assert!(
        format!("{unrelated:#}").contains(&format!("delta base {old_head} is unresolvable")),
        "{unrelated:#}"
    );
}

#[test]
fn no_bundle_snapshot_carries_dirty_state_without_committed_history() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "tracked.txt", b"base\n", "base");
    commit_file(&origin, "dirty.txt", b"clean\n", "clean");
    let source = clone_repository(directory.path(), &origin, "source");
    fs::write(source.join("staged.txt"), b"staged\n").unwrap();
    git(&source, &["add", "staged.txt"]);
    fs::write(source.join("dirty.txt"), b"dirty\n").unwrap();
    fs::write(source.join("new.txt"), b"untracked\n").unwrap();
    let head = git_line(&source, &["rev-parse", "HEAD"]);

    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::NoBundle,
            origin_override: Some("mj-local:repo".into()),
        },
    )
    .unwrap();

    assert!(snapshot.committed_bundle.is_empty());
    assert_eq!(snapshot.metadata.origin, "mj-local:repo");
    assert_eq!(snapshot.metadata.base_commit, head);
    assert!(!snapshot.staged_patch.is_empty());
    assert!(!snapshot.unstaged_patch.is_empty());
    assert!(!snapshot.untracked_tar.is_empty());

    let destination = clone_repository(directory.path(), &origin, "restored");
    git(&destination, &["checkout", "-q", "--detach", "HEAD"]);
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();

    assert_eq!(git_line(&destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_line(&destination, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(fs::read(destination.join("dirty.txt")).unwrap(), b"dirty\n");
    assert_eq!(
        fs::read(destination.join("new.txt")).unwrap(),
        b"untracked\n"
    );
    let status = String::from_utf8(git(&destination, &["status", "--short"])).unwrap();
    assert!(status.contains("A  staged.txt"), "{status}");
    assert!(status.contains(" M dirty.txt"), "{status}");
    assert!(status.contains("?? new.txt"), "{status}");
}

#[test]
fn restore_without_a_bundle_reports_an_unreachable_commit_actionably() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "base.txt", b"base\n", "base");
    let source = clone_repository(directory.path(), &origin, "source");
    commit_file(&source, "local.txt", b"local\n", "local only");
    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::NoBundle,
            origin_override: None,
        },
    )
    .unwrap();

    let destination = clone_repository(directory.path(), &origin, "restored");
    let error = restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap_err();

    assert!(
        format!("{error:#}").contains("must be reachable from the repository's origin"),
        "{error:#}"
    );
}

#[test]
fn git_restore_routes_patches_through_injected_runner() {
    let destination = tempfile::tempdir().unwrap();
    let runner = FakeGit::with_outputs([
        // The worktree guard reads HEAD first; naming the snapshot's own
        // branch means it is already the checkout's branch.
        git_ok("feature/hel\n"),
        git_ok(Vec::new()),
        git_ok(Vec::new()),
        git_ok(Vec::new()),
        git_ok(Vec::new()),
    ]);
    let mut snapshot = repository("repo");
    snapshot.committed_bundle.clear();
    snapshot.untracked_tar = tar_with_file("new.sh", b"echo hi\n", 0o755);
    restore_git_snapshot(&runner, destination.path(), &snapshot).unwrap();
    let commands = runner.commands();
    assert_eq!(commands.len(), 5);
    assert_eq!(commands[3].stdin, b"staged-repo");
    assert_eq!(commands[4].stdin, b"unstaged-repo");
    assert_eq!(
        fs::read(destination.path().join("new.sh")).unwrap(),
        b"echo hi\n"
    );
}

#[test]
fn system_git_round_trip_restores_commits_index_worktree_and_untracked() {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "-q", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Hel Test"]);
    git(source.path(), &["config", "user.email", "hel@example.test"]);
    git(
        source.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    fs::write(source.path().join("tracked.txt"), b"base\n").unwrap();
    fs::write(source.path().join("dirty.txt"), b"clean\n").unwrap();
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-qm", "base"]);
    let base = String::from_utf8(git(source.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_string();
    fs::write(source.path().join("tracked.txt"), b"committed\n").unwrap();
    git(source.path(), &["commit", "-qam", "delta"]);
    fs::write(source.path().join("staged.txt"), b"staged\n").unwrap();
    git(source.path(), &["add", "staged.txt"]);
    fs::write(source.path().join("dirty.txt"), b"dirty\n").unwrap();
    fs::write(source.path().join("new.txt"), b"untracked\n").unwrap();

    let snapshot = collect_git_snapshot(
        &SystemGit,
        source.path(),
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: PathBuf::from("repo"),
            history: GitHistoryMode::DeltaFrom(base.clone()),
            origin_override: None,
        },
    )
    .unwrap();
    assert!(!snapshot.committed_bundle.is_empty());
    assert_eq!(snapshot.metadata.base_commit, base);

    let destination_parent = tempfile::tempdir().unwrap();
    let destination = destination_parent.path().join("restore");
    git(
        destination_parent.path(),
        &[
            "clone",
            "-q",
            source.path().to_str().unwrap(),
            destination.to_str().unwrap(),
        ],
    );
    git(&destination, &["reset", "--hard", &base]);
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();

    assert_eq!(
        fs::read(destination.join("tracked.txt")).unwrap(),
        b"committed\n"
    );
    assert_eq!(fs::read(destination.join("dirty.txt")).unwrap(), b"dirty\n");
    assert_eq!(
        fs::read(destination.join("new.txt")).unwrap(),
        b"untracked\n"
    );
    let status = String::from_utf8(git(&destination, &["status", "--short"])).unwrap();
    assert!(status.contains("A  staged.txt"));
    assert!(status.contains(" M dirty.txt"));
    assert!(status.contains("?? new.txt"));
}

/// Enough text that the capture's patch crosses the 64 KiB pipe buffer a
/// child's stdout is drained through, which is where truncation and deadlock
/// bugs in the Git plumbing would show up.
fn large_text(marker: &str, lines: usize) -> String {
    (0..lines)
        .map(|line| format!("{marker} line {line} with enough text to make the patch large\n"))
        .collect()
}

#[test]
fn review_capture_sees_tracked_modified_and_untracked_changes_without_touching_the_index() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());
    commit_file(
        repository.path(),
        "tracked.txt",
        large_text("base", 800).as_bytes(),
        "base",
    );

    let baseline = capture_worktree_tree(&SystemGit, repository.path()).unwrap();
    let status_before = git(repository.path(), &["status", "--porcelain"]);

    fs::write(
        repository.path().join("tracked.txt"),
        large_text("changed", 800),
    )
    .unwrap();
    fs::write(
        repository.path().join("untracked.txt"),
        large_text("added", 800),
    )
    .unwrap();
    fs::write(
        repository.path().join("staged.txt"),
        large_text("staged", 800),
    )
    .unwrap();
    git(repository.path(), &["add", "staged.txt"]);

    let current = capture_worktree_tree(&SystemGit, repository.path()).unwrap();
    assert_ne!(
        baseline, current,
        "the capture must follow the working tree"
    );

    let patch =
        diff_between_trees(&SystemGit, repository.path(), Some(&baseline), &current).unwrap();
    assert!(
        patch.len() > 64 * 1024,
        "the fixture must exceed one pipe buffer, got {} bytes",
        patch.len()
    );
    assert!(
        patch.contains("tracked.txt"),
        "modified tracked file is in the patch"
    );
    assert!(
        patch.contains("untracked.txt"),
        "untracked file is in the patch"
    );
    assert!(patch.contains("staged.txt"), "staged file is in the patch");
    assert!(
        patch.contains("+changed line 799"),
        "the patch carries file contents"
    );

    let status_after = git(repository.path(), &["status", "--porcelain"]);
    assert_eq!(
        String::from_utf8_lossy(&status_before),
        "",
        "the repository starts clean"
    );
    assert!(
        String::from_utf8_lossy(&status_after).contains("A  staged.txt"),
        "capture leaves the real index exactly as the user left it: {}",
        String::from_utf8_lossy(&status_after)
    );
    assert_eq!(
        git_line(repository.path(), &["rev-parse", REVIEW_CAPTURE_REF]),
        current,
        "the capture ref pins the tree so gc cannot collect it"
    );
    assert!(
        !repository
            .path()
            .join(".git")
            .read_dir()
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("hel-review-index-")
            }),
        "the scratch index is removed after the capture"
    );
}

#[test]
fn review_capture_without_a_baseline_diffs_against_the_empty_tree() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());
    commit_file(repository.path(), "tracked.txt", b"base\n", "base");

    let current = capture_worktree_tree(&SystemGit, repository.path()).unwrap();
    let patch = diff_between_trees(&SystemGit, repository.path(), None, &current).unwrap();
    assert!(
        patch.contains("new file mode") && patch.contains("+base"),
        "an absent baseline renders the whole capture as additions: {patch}"
    );
}

#[test]
fn review_capture_ignores_files_git_ignores() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());
    commit_file(repository.path(), ".gitignore", b"ignored.txt\n", "ignore");

    let baseline = capture_worktree_tree(&SystemGit, repository.path()).unwrap();
    fs::write(repository.path().join("ignored.txt"), b"build output\n").unwrap();
    let current = capture_worktree_tree(&SystemGit, repository.path()).unwrap();

    assert_eq!(
        baseline, current,
        "ignored build output is not a change a review should see"
    );
}
