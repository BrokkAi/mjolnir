use std::io::{Seek, SeekFrom};
use std::sync::Barrier;

use super::*;

struct CollectionGit {
    delta_count: u64,
    payload_barrier: Option<Barrier>,
}

impl CollectionGit {
    fn new(delta_count: u64, concurrent_payloads: bool) -> Self {
        Self {
            delta_count,
            payload_barrier: concurrent_payloads.then(|| Barrier::new(4)),
        }
    }
}

impl GitCommandRunner for CollectionGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        let arguments = command
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        let (status, stdout) = match arguments.first().map(|argument| argument.as_ref()) {
            // The collected directory is its checkout's top level.
            Some("rev-parse")
                if arguments
                    .iter()
                    .any(|argument| argument == "--show-toplevel") =>
            {
                (0, format!("{}\n\n", repository.display()).into_bytes())
            }
            Some("rev-parse")
                if arguments
                    .iter()
                    .any(|argument| argument == "--absolute-git-dir") =>
            {
                (
                    0,
                    format!("{}\n{}\n", repository.display(), repository.display()).into_bytes(),
                )
            }
            Some("show-ref") => (1, Vec::new()),
            // Only the checkout itself: no linked worktrees.
            Some("worktree") => (
                0,
                format!("worktree {}\n", repository.display()).into_bytes(),
            ),
            Some("rev-parse")
                if arguments
                    .iter()
                    .any(|argument| argument.starts_with("refs/hel/")) =>
            {
                (1, Vec::new())
            }
            Some("for-each-ref") if arguments.iter().any(|argument| argument == "refs/mj/") => {
                (0, Vec::new())
            }
            Some("remote") => (0, b"https://token@github.com/example/repo.git\n".to_vec()),
            Some("rev-parse") => (0, format!("{}\n", "b".repeat(40)).into_bytes()),
            Some("merge-base") => (0, format!("{}\n", "b".repeat(40)).into_bytes()),
            Some("for-each-ref") => (0, format!("{}\n", "c".repeat(40)).into_bytes()),
            Some("symbolic-ref") => (0, b"feature/hel\n".to_vec()),
            Some("rev-list") => (0, format!("{}\n", self.delta_count).into_bytes()),
            Some("bundle") => {
                self.payload_barrier.as_ref().unwrap().wait();
                (0, b"bundle".to_vec())
            }
            Some("diff") => {
                if let Some(barrier) = &self.payload_barrier {
                    barrier.wait();
                }
                if arguments.iter().any(|argument| argument == "--cached") {
                    (0, b"staged".to_vec())
                } else {
                    (0, b"unstaged".to_vec())
                }
            }
            Some("ls-files") => {
                if let Some(barrier) = &self.payload_barrier {
                    barrier.wait();
                }
                (0, b"note.txt\0.env\0".to_vec())
            }
            Some("config") => (1, Vec::new()),
            other => return Err(anyhow!("unexpected Git command: {other:?}")),
        };
        Ok(GitOutput {
            status,
            stdout,
            stderr: Vec::new(),
        })
    }
}

use crate::test_support::{git, git_line};

pub(super) fn initialize_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Hel Test"]);
    git(path, &["config", "user.email", "hel@example.test"]);
}

pub(super) fn clone_repository(parent: &Path, origin: &Path, name: &str) -> PathBuf {
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
        origin_delta_pack: None,
        metadata: RepositoryMetadata {
            origin_delta: None,
            review_baseline_ref: None,
            review_refs_metadata_only: false,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            id: id.to_string(),
            relative_destination: PathBuf::from(id),
            checkout_subdirectory: None,
            origin: format!("https://github.com/example/{id}.git"),
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            branch: Some("feature/hel".to_string()),
        },
        committed_bundle: format!("bundle-{id}").into_bytes(),
        staged_patch: format!("staged-{id}").into_bytes(),
        unstaged_patch: format!("unstaged-{id}").into_bytes(),
        untracked_tar: tar_with_file("scripts/tool.sh", b"#!/bin/sh\n", 0o755),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    }
}

fn checkpoint_bundle(head: &str, contents: impl Into<Vec<u8>>) -> CheckpointRepositoryBundle {
    CheckpointRepositoryBundle {
        metadata: RepositoryMetadata {
            origin_delta: None,
            review_baseline_ref: None,
            review_refs_metadata_only: false,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            id: "project".into(),
            relative_destination: "project".into(),
            checkout_subdirectory: None,
            origin: "https://github.com/example/project.git".into(),
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
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

pub(super) fn input() -> ArchiveInput {
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
            command_ledger: None,
            assessment_state: None,
            notification_stack: None,
            native_use_evidence: None,
            event_frontier: 4,
            event_frontier_digest: "a".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: Some(104),
                session_title: Some("Forge Hel".into()),
                configuration: mj_core::state::SessionConfiguration {
                    values: BTreeMap::from([(
                        "reasoning_effort".into(),
                        serde_json::json!("high"),
                    )]),
                    ..Default::default()
                },
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
            mailbox: None,
        },
        native_artifacts: vec![NativeArtifact {
            relative_path: PathBuf::from("sessions/native-1/rollout.jsonl"),
            data: (b"native transcript".to_vec()).into(),
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
fn streaming_verification_does_not_retain_large_noncanonical_payloads() {
    const LARGE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.hel.zip");
    let mut archive_input = input();
    archive_input.repositories.clear();
    archive_input.native_artifacts = vec![NativeArtifact {
        relative_path: PathBuf::from("sessions/native-1/large-rollout.jsonl"),
        data: (vec![b'x'; LARGE_PAYLOAD_BYTES]).into(),
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
    // A preparation cancellation reaches readers cloned for parallel verification.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let archive = open_archive_cancellable(&path, Some(cancelled.clone()), None).unwrap();
    let mut parallel_reader = archive.clone();
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    let mut entry = parallel_reader.by_index(0).unwrap();
    assert!(entry.read_to_end(&mut Vec::new()).is_err());
    assert!(verify_archive_streaming_cancellable(&path, cancelled).is_err());
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
    archive_input.native_artifacts[0].data = native.clone().into();
    archive_input.repositories[0].untracked_tar = untracked.clone();
    (archive_input, native, untracked)
}

// Hard-won: 4cc32efd0: last-part EOF checking preceded completion of earlier compression.
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

    // Force the schedule where the last part finishes before the source grows
    // and the earlier parts finish. Completion belongs to ordered assembly.
    let source_path = directory.path().join("growing-native.jsonl");
    fs::write(&source_path, native.repeat(4)).unwrap();
    let mut file_input = archive_input.clone();
    file_input.native_artifacts[0].data = NativeArtifactData::File(source_path.clone());
    let (_, payloads) = prepare_archive_with_part_size(&file_input, TEST_PART_BYTES).unwrap();
    let payload = payloads
        .iter()
        .find(|payload| payload.descriptor.path == native_path)
        .unwrap();
    let mut offset = 0;
    let entries: Vec<_> = payload
        .descriptor
        .parts
        .iter()
        .map(|part| {
            let entry = StreamEntry {
                payload,
                name: &part.path,
                size: part.size,
                digest: &part.sha256,
                offset,
            };
            offset += part.size;
            entry
        })
        .collect();
    let mut compressed = Vec::new();
    for entry in entries.iter().rev() {
        compressed.push(Some(
            compress_stream_entry(entry, directory.path()).unwrap(),
        ));
        if compressed.len() == 1 {
            OpenOptions::new()
                .append(true)
                .open(&source_path)
                .unwrap()
                .write_all(b"late source append\n")
                .unwrap();
        }
    }
    compressed.reverse();
    let mut output = File::create(directory.path().join("growing.hel.zip")).unwrap();
    let mut writer = zip::ZipWriter::new(&mut output);
    let error = append_stream_entries(&mut writer, &entries, compressed).unwrap_err();
    assert!(format!("{error:#}").contains("payload source grew during export"));
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

#[test]
fn verified_payload_reader_refuses_descriptor_over_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bounded.hel.zip");
    let verified = write_archive_atomic(&path, &input()).unwrap();
    let descriptor = verified
        .manifest
        .payloads
        .iter()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap();

    let error =
        read_verified_checkpoint_payload(&path, descriptor, descriptor.size - 1).unwrap_err();
    assert!(format!("{error:#}").contains("exceeds read limit"));
}

#[test]
fn verified_payload_reader_rejects_tampered_non_sharded_digest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("digest.hel.zip");
    let verified = write_archive_atomic(&path, &input()).unwrap();
    let mut descriptor = verified
        .manifest
        .payloads
        .iter()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap()
        .clone();
    assert!(descriptor.parts.is_empty());
    descriptor.sha256 = "0".repeat(64);

    let error = read_verified_checkpoint_payload(&path, &descriptor, descriptor.size).unwrap_err();
    assert!(format!("{error:#}").contains("SHA-256 mismatch"));
}

#[test]
fn verified_payload_reader_reassembles_sharded_payload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sharded-reader.hel.zip");
    let (archive_input, native, _) = sharded_input();
    write_archive_installed_with_part_size(&path, &archive_input, TEST_PART_BYTES).unwrap();
    let manifest = read_checkpoint_manifest(&path).unwrap();
    let descriptor = manifest
        .payloads
        .iter()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap();
    assert!(!descriptor.parts.is_empty());

    assert_eq!(
        read_verified_checkpoint_payload(&path, descriptor, native.len() as u64).unwrap(),
        native
    );
}

#[test]
fn verified_payload_reader_refuses_incomplete_shard_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sharded-coverage.hel.zip");
    let (archive_input, _, _) = sharded_input();
    write_archive_installed_with_part_size(&path, &archive_input, TEST_PART_BYTES).unwrap();
    let manifest = read_checkpoint_manifest(&path).unwrap();
    let mut descriptor = manifest
        .payloads
        .iter()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap()
        .clone();
    assert!(!descriptor.parts.is_empty());
    descriptor.parts.pop();

    let error = read_verified_checkpoint_payload(&path, &descriptor, descriptor.size).unwrap_err();
    assert!(format!("{error:#}").contains("payload parts do not cover declared size"));
}

/// Replaces a tar payload in an already prepared archive, resharding it so
/// the manifest and the payload body stay consistent.
fn replace_tar_payload(
    manifest: &mut ArchiveManifest,
    payloads: &mut [PendingPayload<'_>],
    role: PayloadRole,
    tar: Vec<u8>,
    part_bytes: usize,
) {
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
    let malicious = tar_with_file(".git/config", &b"unsafe\n".repeat(2_000), 0o600);
    assert!(malicious.len() > TEST_PART_BYTES);
    replace_tar_payload(
        &mut manifest,
        &mut payloads,
        PayloadRole::GitUntrackedTar {
            repository_id: "hel".into(),
        },
        malicious,
        TEST_PART_BYTES,
    );
    let mut file = File::create(&path).unwrap();
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);

    let error = format!("{:#}", verify_archive_streaming(&path).unwrap_err());
    assert!(error.contains("inside a Git directory"), "{error}");
}

/// A restore writes the operation state tar into the Git directory, so the
/// verifier must refuse anything in it that is not Git operation state: a
/// hook written there would run on the restored clone's next Git command.
#[test]
fn streaming_verification_refuses_operation_state_that_is_not_git_operation_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsafe-operation-state.hel.zip");
    let mut archive_input = input();
    let repository = &mut archive_input.repositories[0];
    repository.metadata.saved_refs =
        BTreeMap::from([("refs/heads/feature/hel".into(), "b".repeat(40))]);
    repository.metadata.git_operation = Some(SavedGitOperation {
        unmerged: Vec::new(),
        missing_worktree_paths: Vec::new(),
        objects: vec!["c".repeat(40)],
    });
    repository.operation_payloads = Some(GitOperationPayloads {
        conflicted_worktree_tar: tar::Builder::new(Vec::new()).into_inner().unwrap(),
        state_tar: tar_with_file(
            "MERGE_HEAD",
            format!("{}\n", "c".repeat(40)).as_bytes(),
            0o644,
        ),
    });
    let (mut manifest, mut payloads) = prepare_archive(&archive_input).unwrap();
    replace_tar_payload(
        &mut manifest,
        &mut payloads,
        PayloadRole::GitOperationStateTar {
            repository_id: "hel".into(),
        },
        tar_with_file("hooks/post-checkout", b"#!/bin/sh\n", 0o755),
        PAYLOAD_PART_BYTES,
    );
    let mut file = File::create(&path).unwrap();
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);

    let error = format!("{:#}", verify_archive_streaming(&path).unwrap_err());
    assert!(error.contains("not Git operation state"), "{error}");
}

/// A restore writes the untracked and conflicted tars into the working tree
/// and removes the missing unmerged paths there, so none of them may reach
/// into the Git directory: a planted hook would run on the restored clone's
/// next Git command, and a removed `.git/index` would wreck the clone.
// Hard-won: 5a3457a30: restored working-tree paths could write into or remove from `.git`
#[test]
fn streaming_verification_refuses_working_tree_paths_inside_the_git_directory() {
    let directory = tempfile::tempdir().unwrap();
    for tampered in ["untracked", "conflicted", "missing"] {
        let mut archive_input = input();
        let repository = &mut archive_input.repositories[0];
        repository.metadata.git_operation = Some(SavedGitOperation {
            unmerged: Vec::new(),
            missing_worktree_paths: Vec::new(),
            objects: vec!["c".repeat(40)],
        });
        repository.operation_payloads = Some(GitOperationPayloads {
            conflicted_worktree_tar: tar::Builder::new(Vec::new()).into_inner().unwrap(),
            state_tar: tar_with_file(
                "MERGE_HEAD",
                format!("{}\n", "c".repeat(40)).as_bytes(),
                0o644,
            ),
        });
        let (mut manifest, mut payloads) = prepare_archive(&archive_input).unwrap();
        match tampered {
            "untracked" => replace_tar_payload(
                &mut manifest,
                &mut payloads,
                PayloadRole::GitUntrackedTar {
                    repository_id: "hel".into(),
                },
                tar_with_file(".git/hooks/post-checkout", b"#!/bin/sh\n", 0o755),
                PAYLOAD_PART_BYTES,
            ),
            "conflicted" => replace_tar_payload(
                &mut manifest,
                &mut payloads,
                PayloadRole::GitConflictedWorktreeTar {
                    repository_id: "hel".into(),
                },
                tar_with_file(".GIT/hooks/post-checkout", b"#!/bin/sh\n", 0o755),
                PAYLOAD_PART_BYTES,
            ),
            _ => {
                let index = PathBuf::from(".git/index");
                manifest.repositories[0].metadata.git_operation = Some(SavedGitOperation {
                    unmerged: vec![SavedUnmergedEntry {
                        path: index.clone(),
                        stage: 2,
                        mode: "100644".into(),
                        object: "c".repeat(40),
                    }],
                    missing_worktree_paths: vec![index],
                    objects: vec!["c".repeat(40)],
                });
            }
        }
        let path = directory.path().join(format!("{tampered}.hel.zip"));
        let mut file = File::create(&path).unwrap();
        write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
        drop(file);

        let error = format!("{:#}", verify_archive_streaming(&path).unwrap_err());
        assert!(
            error.contains("inside a Git directory"),
            "{tampered}: {error}"
        );
    }
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
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
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
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
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
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
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

/// The manifest an independent clone with saved refs and a stash, and no
/// unfinished Git operation, is written with. Archives that carry no operation
/// must stay readable by every build that reads clone refs, so this layout may
/// not change when operation capture does.
#[test]
fn clone_manifest_without_a_git_operation_matches_golden() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("clone.hel.zip");
    let mut archive_input = input();
    let metadata = &mut archive_input.repositories[0].metadata;
    metadata.saved_refs = BTreeMap::from([
        ("refs/heads/feature/hel".into(), "b".repeat(40)),
        ("refs/tags/v1".into(), "c".repeat(40)),
    ]);
    metadata.stash_stack = vec![SavedStashEntry {
        commit: "d".repeat(40),
        message: "On feature/hel: draft".into(),
    }];
    write_archive_atomic(&path, &archive_input).unwrap();
    let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let mut manifest = String::new();
    archive
        .by_name(MANIFEST_PATH)
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "clone-archive-manifest",
        &manifest,
    );
}

#[test]
fn streaming_verification_rejects_noncanonical_corruption() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-native.hel.zip");
    let mut archive_input = input();
    archive_input.repositories.clear();
    archive_input.native_artifacts[0].data = b"native payload".to_vec().into();
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
    let malicious = tar_with_file(".git/config", b"unsafe", 0o600);
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
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);

    let error = verify_archive_streaming(&path).unwrap_err();
    assert!(format!("{error:#}").contains("inside a Git directory"));
}

// Hard-won: 0d7aa0f1e: streaming restore skipped the whole native digest before mutating targets.
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

    // Keep every part intact but corrupt the whole native digest. Every
    // selective reader must reject it before the caller can mutate a target.
    let (archive_input, _, _) = sharded_input();
    let (mut manifest, payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    let native = manifest
        .payloads
        .iter_mut()
        .find(|payload| matches!(payload.role, PayloadRole::NativeArtifact { .. }))
        .unwrap();
    assert!(!native.parts.is_empty());
    native.sha256 = "0".repeat(64);
    let relative = match &native.role {
        PayloadRole::NativeArtifact { relative_path } => relative_path.clone(),
        _ => unreachable!(),
    };
    let mut file = File::create(&path).unwrap();
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);
    for result in [
        verify_archive_streaming(&path).map(|_| ()),
        read_archive_repositories(&path).map(|_| ()),
    ] {
        assert!(format!("{:#}", result.unwrap_err()).contains("SHA-256 mismatch"));
    }
    let workspace = directory.path().join("workspace");
    let home = directory.path().join("harness");
    let relay = directory.path().join("relay");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(home.join(&relative).parent().unwrap()).unwrap();
    fs::write(home.join(&relative), b"existing native state").unwrap();
    fs::write(workspace.join("keep"), b"existing work").unwrap();
    for restore_native in [false, true] {
        let error = crate::checkpoint::restore_checkpoint(
            &crate::checkpoint::CheckpointRestoreSpec {
                report_root: None,
                archive_path: path.clone(),
                workspace_root: workspace.clone(),
                relay_root: relay.clone(),
                harness_home: home.clone(),
                restore_repositories: true,
                restore_native,
                queue_policy: crate::checkpoint::QueueRestorePolicy::Discard,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256 mismatch"));
        assert_eq!(
            fs::read(home.join(&relative)).unwrap(),
            b"existing native state"
        );
        assert_eq!(fs::read(workspace.join("keep")).unwrap(), b"existing work");
        assert!(!relay.exists());
        assert!(!workspace.join("hel").exists());
    }
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
        write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
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
fn repository_metadata_defaults_network_fields_for_older_archives() {
    let metadata = serde_json::json!({
        "id": "repo",
        "relative_destination": "repo",
        "origin": "https://github.com/example/repo.git",
        "base_commit": "",
        "head_commit": "b".repeat(40),
        "branch": null
    });
    let parsed: RepositoryMetadata = serde_json::from_value(metadata).unwrap();
    assert!(parsed.push_urls.is_empty());
    assert!(!parsed.remote_workspace);
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
        .values
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
fn traversal_is_rejected_before_destination_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.hel.zip");
    fs::write(&path, b"existing archive").unwrap();
    let mut unsafe_input = input();
    unsafe_input.native_artifacts[0].relative_path = PathBuf::from("../auth.json");
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
    let tar = build_untracked_tar(
        source.path(),
        &paths,
        UntrackedFiles::Include,
        &Default::default(),
        &|_| Ok(()),
    )
    .unwrap();
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

    let error = build_untracked_tar(
        source.path(),
        paths,
        UntrackedFiles::Include,
        &Default::default(),
        &|progress| {
            let GitSnapshotProgress::UntrackedFile { current, total, .. } = progress;
            seen.lock().unwrap().push((current, total));
            ensure!(current < 2, "cancelled by test");
            Ok(())
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("cancelled by test"));
    assert_eq!(*seen.lock().unwrap(), vec![(1, 2), (2, 2)]);
}

#[test]
fn git_collection_is_abstracted_and_redacts_origin() {
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
    assert_eq!(
        paths,
        vec![PathBuf::from("note.txt"), PathBuf::from(".env")]
    );
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
}

/// Checkpoint work runs with nobody watching the terminal it inherits, so
/// a Git child that would ask for a password or a host key has to fail
/// instead of holding the checkpoint open until its deadline.
// Hard-won: 4e68009d: unattended checkpoint Git could wedge on a credential or host-key prompt.
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
            .or_else(|| { Some(mj_core::local_git::NON_INTERACTIVE_GIT_SSH_COMMAND.to_owned()) }),
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

#[test]
fn managed_clone_snapshot_restores_secondary_branch_and_full_stash_stack() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "tracked.txt", b"base\n", "base");
    let base = git_line(&origin, &["rev-parse", "HEAD"]);
    let source = clone_repository(directory.path(), &origin, "source");
    git(&source, &["switch", "-q", "-c", "side"]);
    let mut value = 1_u64;
    let large: Vec<u8> = (0..100_000)
        .map(|_| {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            value as u8
        })
        .collect();
    commit_file(&source, "side.bin", &large, "side work");
    let side_tip = git_line(&source, &["rev-parse", "HEAD"]);
    git(&source, &["tag", "-a", "side-tag", "-m", "side tag"]);
    git(&source, &["switch", "-q", "main"]);
    commit_file(&source, "main.txt", b"main work\n", "main work");
    fs::write(source.join("tracked.txt"), b"first stash\n").unwrap();
    git(&source, &["stash", "push", "-q", "-m", "first"]);
    fs::write(source.join("tracked.txt"), b"second stash\n").unwrap();
    git(&source, &["stash", "push", "-q", "-m", "second"]);
    let stash_latest = git_line(&source, &["rev-parse", "stash@{0}"]);
    let stash_older = git_line(&source, &["rev-parse", "stash@{1}"]);
    fs::write(source.join("tracked.txt"), b"working change\n").unwrap();

    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(base),
            origin_override: None,
        },
    )
    .unwrap();
    assert_eq!(
        snapshot
            .metadata
            .stash_stack
            .iter()
            .map(|entry| entry.commit.clone())
            .collect::<Vec<_>>(),
        vec![stash_latest.clone(), stash_older.clone()]
    );
    assert!(snapshot.metadata.stash_stack[0].message.contains("second"));
    assert_eq!(snapshot.metadata.saved_refs["refs/heads/side"], side_tip);
    assert!(!snapshot.committed_bundle.is_empty());

    let destination = clone_repository(directory.path(), &origin, "restored");
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
    assert_eq!(
        git(&destination, &["status", "--porcelain=v1", "-z"]),
        git(&source, &["status", "--porcelain=v1", "-z"])
    );
    assert_eq!(
        git(&destination, &["rev-parse", "HEAD"]),
        git(&source, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&destination, &["ls-files", "--stage", "-z"]),
        git(&source, &["ls-files", "--stage", "-z"])
    );
    assert_eq!(
        fs::read(destination.join("main.txt")).unwrap(),
        b"main work\n"
    );
    assert_eq!(
        git_line(&destination, &["rev-parse", "refs/heads/side"]),
        side_tip
    );
    assert_eq!(
        fs::read(destination.join("side.bin")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        git_line(&destination, &["rev-parse", "stash@{0}"]),
        stash_latest
    );
    assert_eq!(
        git_line(&destination, &["rev-parse", "stash@{1}"]),
        stash_older
    );
    let stash_list = git_line(&destination, &["stash", "list"]);
    assert!(stash_list.contains("second") && stash_list.contains("first"));
    assert_eq!(
        fs::read(destination.join("tracked.txt")).unwrap(),
        b"working change\n"
    );
    assert_eq!(
        git_line(&destination, &["rev-parse", "refs/tags/side-tag"]),
        git_line(&source, &["rev-parse", "refs/tags/side-tag"])
    );
}

/// The scope name of a main worktree's session-start refs, and a private ref
/// that names a bare tree or a tag rather than a commit.
const MAIN_SCOPE: &str = "refs/mj/review-baseline";
const NON_COMMIT_TIP: &str = "refs/mj/non-commit-tip";

// Hard-won: 0fefbee48: review refs disappeared when a checkpoint moved its checkout.
#[test]
fn review_refs_round_trip_with_non_commit_tips_and_restore_conflicts() {
    for metadata_only in [false, true] {
        for tagged in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let origin = directory.path().join("origin");
            initialize_repository(&origin);
            commit_file(&origin, "tracked", b"base\n", "base");
            let head = git_line(&origin, &["rev-parse", "HEAD"]);
            let source = clone_repository(directory.path(), &origin, "source");
            fs::write(source.join("tracked"), b"baseline only\n").unwrap();
            let tree = crate::test_support::fixture_tree_with_edit(&source, "tracked");
            git(&source, &["checkout", "--", "tracked"]);
            let point = if tagged {
                record_review_point(
                    &SystemGit,
                    &source,
                    &ReviewPoint {
                        tree: tree.clone(),
                        head: Some(head.clone()),
                        reflog: None,
                        pending: Vec::new(),
                        session: Some("moving".into()),
                    },
                )
                .unwrap()
            } else {
                tree.clone()
            };
            let reviewed = format!(
                "refs/mj/reviewed/{}",
                git_line(&source, &["rev-parse", "HEAD^{tree}"])
            );
            let overridden = reviewed.replace("refs/mj/reviewed/", "refs/mj/overridden/");
            let start = format!(
                "refs/mj/session-start/{}/{}",
                digest_bytes(b"moving"),
                digest_bytes(MAIN_SCOPE.as_bytes())
            );
            for (name, object) in [
                (reviewed.as_str(), &head),
                (overridden.as_str(), &head),
                (start.as_str(), &head),
                (NON_COMMIT_TIP, &point),
            ] {
                git(&source, &["update-ref", name, object]);
            }
            let spec = GitCollectionSpec {
                id: "repo".into(),
                relative_destination: "repo".into(),
                history: GitHistoryMode::CloneFrom(head.clone()),
                origin_override: None,
            };
            let snapshot = if metadata_only {
                collect_git_metadata_snapshot(&SystemGit, &source, &spec)
            } else {
                collect_git_snapshot(&SystemGit, &source, &spec)
            }
            .unwrap();
            assert_eq!(snapshot.committed_bundle.is_empty(), metadata_only);
            let destination = if metadata_only {
                for name in snapshot.metadata.saved_refs.keys() {
                    git(&source, &["update-ref", "-d", name]);
                }
                source.clone()
            } else {
                clone_repository(directory.path(), &origin, "restored")
            };
            restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
            for (name, object) in [
                (reviewed.as_str(), &head),
                (overridden.as_str(), &head),
                (start.as_str(), &head),
                (NON_COMMIT_TIP, &point),
            ] {
                assert_eq!(git_line(&destination, &["rev-parse", name]), *object);
            }
            assert_eq!(
                git(&destination, &["show", &format!("{tree}:tracked")]),
                b"baseline only\n"
            );
            // A reviewed tree may be recorded at a different equivalent commit;
            // another session's start must remain untouched during restore.
            git(
                &destination,
                &["commit", "--allow-empty", "-qm", "equivalent reviewed tree"],
            );
            let later = git_line(&destination, &["rev-parse", "HEAD"]);
            git(&destination, &["update-ref", &reviewed, &later]);
            git(&destination, &["update-ref", &overridden, &later]);
            git(&destination, &["update-ref", &start, &later]);
            let other_start = format!("refs/mj/session-start/{}/other", digest_bytes(b"other"));
            git(&destination, &["update-ref", &other_start, &later]);
            if tagged {
                let newer = record_review_point(
                    &SystemGit,
                    &destination,
                    &ReviewPoint {
                        tree: tree.clone(),
                        head: Some(later.clone()),
                        reflog: None,
                        pending: Vec::new(),
                        session: Some("moving".into()),
                    },
                )
                .unwrap();
                git(&destination, &["update-ref", NON_COMMIT_TIP, &newer]);
            }
            let mut conflicting = snapshot.clone();
            conflicting
                .metadata
                .saved_refs
                .insert(other_start.clone(), head.clone());
            super::git::restore_git_snapshot_for_session(
                &SystemGit,
                &destination,
                &conflicting,
                Some("moving"),
                true,
            )
            .unwrap();
            assert_eq!(git_line(&destination, &["rev-parse", &reviewed]), later);
            assert_eq!(git_line(&destination, &["rev-parse", &overridden]), later);
            assert_eq!(git_line(&destination, &["rev-parse", &start]), head);
            assert_eq!(git_line(&destination, &["rev-parse", &other_start]), later);
            // A tip the destination already holds is not replaced.
            let tip = git_line(&destination, &["rev-parse", NON_COMMIT_TIP]);
            assert_eq!(tip == point, !tagged, "{tip}");
        }
    }
}

#[test]
fn session_starts_and_explicit_review_bases_follow_worktree_names() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "tracked", b"base\n", "base");
    let base = git_line(&origin, &["rev-parse", "HEAD"]);
    let linked = directory.path().join("linked");
    git(
        &origin,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "session",
            linked.to_str().unwrap(),
        ],
    );
    let scope = session_start_scope(&SystemGit, &linked).unwrap();
    assert_eq!(scope, "refs/mj/worktrees/linked/review-baseline");
    let start = format!(
        "refs/mj/session-start/{}/{}",
        digest_bytes(b"moving"),
        digest_bytes(scope.as_bytes())
    );
    git(&linked, &["update-ref", &start, &base]);
    commit_file(&linked, "reviewed.txt", b"reviewed\n", "reviewed");
    let reviewed = git_line(&linked, &["rev-parse", "HEAD"]);
    git(
        &linked,
        &["update-ref", &push_review_base_ref(&scope), &reviewed],
    );
    let snapshot = collect_git_snapshot(
        &SystemGit,
        &linked,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(base.clone()),
            origin_override: Some(origin.to_string_lossy().into_owned()),
        },
    )
    .unwrap();
    assert_eq!(
        snapshot.metadata.review_baseline_ref.as_deref(),
        Some(scope.as_str())
    );
    let destination = clone_repository(directory.path(), &origin, "destination");
    super::git::restore_git_snapshot_for_session(
        &SystemGit,
        &destination,
        &snapshot,
        Some("moving"),
        false,
    )
    .unwrap();
    let relocated_start = format!(
        "refs/mj/session-start/{}/{}",
        digest_bytes(b"moving"),
        digest_bytes(MAIN_SCOPE.as_bytes())
    );
    assert_eq!(
        git_line(&destination, &["rev-parse", &relocated_start]),
        base
    );
    let relocated_base = push_review_base_ref(MAIN_SCOPE);
    assert_eq!(
        git_line(&destination, &["rev-parse", &relocated_base]),
        reviewed
    );
    // A selection already made in the destination belongs to that worktree.
    git(&destination, &["update-ref", &relocated_base, &base]);
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
    assert_eq!(
        git_line(&destination, &["rev-parse", &relocated_base]),
        base
    );
}

// Hard-won: 38f041c03: a metadata-only capture whose only refs/mj ref was a dropped pin restored as a full snapshot.
#[test]
fn a_metadata_only_capture_without_review_refs_never_restores_contents() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "tracked", b"base\n", "base");
    let source = clone_repository(directory.path(), &origin, "source");
    let captured = git_line(&source, &["rev-parse", "HEAD"]);
    let tree = git_line(&source, &["rev-parse", "HEAD^{tree}"]);
    git(&source, &["update-ref", MAIN_SCOPE, &tree]);
    let snapshot = collect_git_metadata_snapshot(
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
    assert!(snapshot.metadata.saved_refs.is_empty());
    assert!(snapshot.metadata.review_refs_metadata_only);

    // The user keeps working; a restore with contents must not move them back.
    commit_file(&source, "tracked", b"later\n", "later");
    let later = git_line(&source, &["rev-parse", "HEAD"]);
    assert_ne!(later, captured);
    restore_git_snapshot(&SystemGit, &source, &snapshot).unwrap();
    assert_eq!(git_line(&source, &["rev-parse", "HEAD"]), later);
    assert_eq!(std::fs::read(source.join("tracked")).unwrap(), b"later\n");
}

// Hard-won: 0c78ec89f: capture and restore migrated, copied or wrote review baseline pins in the user's repository.
#[test]
fn capture_and_restore_leave_a_repositorys_old_review_pins_alone() {
    for metadata_only in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin");
        initialize_repository(&origin);
        commit_file(&origin, "tracked", b"base\n", "base");
        let head = git_line(&origin, &["rev-parse", "HEAD"]);
        let source = clone_repository(directory.path(), &origin, "source");
        let linked = directory.path().join("linked");
        git(
            &source,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "session",
                linked.to_str().unwrap(),
            ],
        );
        let point = |session: &str| {
            record_review_point(
                &SystemGit,
                &source,
                &ReviewPoint {
                    tree: git_line(&source, &["rev-parse", "HEAD^{tree}"]),
                    head: Some(head.clone()),
                    reflog: None,
                    pending: Vec::new(),
                    session: Some(session.into()),
                },
            )
            .unwrap()
        };
        let current = point("moving");
        let legacy = point("legacy");
        let start = format!(
            "refs/mj/session-start/{}/{}",
            digest_bytes(b"moving"),
            digest_bytes(MAIN_SCOPE.as_bytes())
        );
        // The pins an older release left, under both names, and a start that
        // is live state.
        let before = [
            (MAIN_SCOPE, &current),
            ("refs/mj/worktrees/linked/review-baseline", &current),
            ("refs/hel/review-baseline", &legacy),
            ("refs/hel/worktrees/linked/review-baseline", &legacy),
            (start.as_str(), &head),
        ];
        for (name, object) in before {
            git(&source, &["update-ref", name, object]);
        }
        let refs = |repository: &Path| {
            git_line(
                repository,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            )
        };
        let before_capture = refs(&source);
        let spec = GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(head.clone()),
            origin_override: None,
        };

        let snapshot = if metadata_only {
            collect_git_metadata_snapshot(&SystemGit, &source, &spec)
        } else {
            collect_git_snapshot(&SystemGit, &source, &spec)
        }
        .unwrap();

        assert_eq!(
            refs(&source),
            before_capture,
            "capture wrote to the repository"
        );
        assert!(snapshot.metadata.saved_refs.contains_key(&start));
        assert!(
            snapshot
                .metadata
                .saved_refs
                .keys()
                .all(|name| !name.contains("review-baseline")),
            "a pin was carried: {:?}",
            snapshot.metadata.saved_refs
        );
        let destination = if metadata_only {
            source.clone()
        } else {
            clone_repository(directory.path(), &origin, "restored")
        };
        let before_restore = refs(&destination);
        super::git::restore_git_snapshot_for_session(
            &SystemGit,
            &destination,
            &snapshot,
            Some("moving"),
            true,
        )
        .unwrap();
        let after_restore = refs(&destination);
        if metadata_only {
            assert_eq!(
                after_restore, before_restore,
                "restore wrote to the repository"
            );
        } else {
            assert_eq!(
                after_restore
                    .lines()
                    .filter(|line| line.contains("review-baseline"))
                    .count(),
                0,
                "restore wrote a pin into the destination"
            );
            assert_eq!(git_line(&destination, &["rev-parse", &start]), head);
        }
    }
}

// Hard-won: 0c78ec89f: an old checkpoint that carried a review baseline pin must still restore without writing it.
#[test]
fn an_old_checkpoint_carrying_a_review_baseline_pin_restores_without_it() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "tracked", b"base\n", "base");
    let head = git_line(&origin, &["rev-parse", "HEAD"]);
    let source = clone_repository(directory.path(), &origin, "source");
    let start = format!(
        "refs/mj/session-start/{}/{}",
        digest_bytes(b"moving"),
        digest_bytes(MAIN_SCOPE.as_bytes())
    );
    git(&source, &["update-ref", &start, &head]);
    let mut snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(head.clone()),
            origin_override: None,
        },
    )
    .unwrap();
    // What an older release wrote: the pin among the saved refs, whose object
    // may have been garbage-collected since.
    let missing = "0".repeat(40);
    snapshot
        .metadata
        .saved_refs
        .insert(MAIN_SCOPE.to_owned(), missing.clone());
    snapshot.metadata.saved_refs.insert(
        "refs/mj/worktrees/linked/review-baseline".to_owned(),
        missing,
    );
    snapshot.metadata.review_baseline_ref = Some(MAIN_SCOPE.to_owned());

    let destination = clone_repository(directory.path(), &origin, "restored");
    super::git::restore_git_snapshot_for_session(
        &SystemGit,
        &destination,
        &snapshot,
        Some("moving"),
        true,
    )
    .unwrap();

    assert_eq!(git_line(&destination, &["rev-parse", &start]), head);
    assert_eq!(
        git_line(
            &destination,
            &[
                "for-each-ref",
                "refs/mj/review-baseline",
                "refs/mj/worktrees"
            ]
        ),
        ""
    );
}

// Hard-won: 7414cc74: updating the checked-out branch first staged added files as deletions.
#[test]
fn managed_clone_restore_matches_head_index_and_worktree() {
    for detached in [false, true] {
        for dirty in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let origin = directory.path().join("origin");
            initialize_repository(&origin);
            commit_file(&origin, "tracked", b"base\n", "base");
            commit_file(&origin, "deleted", b"remove me\n", "old file");
            let base = git_line(&origin, &["rev-parse", "HEAD"]);
            let source = clone_repository(directory.path(), &origin, "source");
            commit_file(&source, "added", b"new committed file\n", "add file");
            git(&source, &["rm", "deleted"]);
            git(&source, &["commit", "-qm", "delete file"]);
            if detached {
                git(&source, &["checkout", "--detach"]);
            }
            if dirty {
                fs::write(source.join("tracked"), b"staged\n").unwrap();
                git(&source, &["add", "tracked"]);
                fs::write(source.join("tracked"), b"unstaged\n").unwrap();
                fs::write(source.join("untracked"), b"untracked\n").unwrap();
            }
            let snapshot = collect_git_snapshot(
                &SystemGit,
                &source,
                &GitCollectionSpec {
                    id: "repo".into(),
                    relative_destination: "repo".into(),
                    history: GitHistoryMode::CloneFrom(base),
                    origin_override: None,
                },
            )
            .unwrap();
            let destination = clone_repository(directory.path(), &origin, "destination");
            restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
            for args in [
                vec!["rev-parse", "HEAD"],
                vec!["status", "--porcelain=v1", "-z"],
                vec!["ls-files", "--stage", "-z"],
                vec!["diff", "--binary"],
                vec!["diff", "--cached", "--binary"],
                vec!["rev-parse", "--abbrev-ref", "HEAD"],
            ] {
                assert_eq!(
                    git(&destination, &args),
                    git(&source, &args),
                    "{args:?}, detached={detached}, dirty={dirty}"
                );
            }
            for path in ["tracked", "added"] {
                assert_eq!(
                    fs::read(destination.join(path)).unwrap(),
                    fs::read(source.join(path)).unwrap()
                );
            }
            assert!(!destination.join("deleted").exists());
            if dirty {
                assert_eq!(
                    fs::read(destination.join("untracked")).unwrap(),
                    b"untracked\n"
                );
            }
        }
    }
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
        format!("{unfetched:#}").contains("import committed delta bundle"),
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

// Hard-won: #1384: a registered nested worktree failed with an unidentified untracked file type.
#[test]
fn nested_worktree_checkpoint_names_the_directory_and_explicit_scratch_exclusion() {
    let source = tempfile::tempdir().unwrap();
    initialize_repository(source.path());
    commit_file(source.path(), "tracked.txt", b"base\n", "base");
    let base = git_line(source.path(), &["rev-parse", "HEAD"]);
    // An independent clone is a different repository: its work cannot travel
    // in this one's snapshot. (A linked worktree of this repository can.)
    let nested = ".mj/csharp-parity/bin-src";
    git(source.path(), &["clone", "--no-hardlinks", ".", nested]);
    fs::write(
        source.path().join(nested).join("valuable.txt"),
        b"nested work\n",
    )
    .unwrap();
    let spec = GitCollectionSpec {
        id: "repo".into(),
        relative_destination: "repo".into(),
        history: GitHistoryMode::DeltaFrom(base),
        origin_override: None,
    };
    let error = collect_git_snapshot(&SystemGit, source.path(), &spec).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("nested Git repository directory"),
        "{message}"
    );
    assert!(
        message.contains(&source.path().join(nested).display().to_string()),
        "{message}"
    );
    assert!(message.contains(".git/info/exclude"), "{message}");
    assert!(
        message.contains("preserve its work separately"),
        "{message}"
    );
    fs::write(
        source.path().join(".git/info/exclude"),
        format!("/{nested}/\n"),
    )
    .unwrap();
    let snapshot = collect_git_snapshot(&SystemGit, source.path(), &spec).unwrap();
    let mut untracked = tar::Archive::new(Cursor::new(snapshot.untracked_tar));
    assert!(untracked.entries().unwrap().next().is_none());
    assert_eq!(
        fs::read(source.path().join(nested).join("valuable.txt")).unwrap(),
        b"nested work\n"
    );
}

// Hard-won: b63fedbf31: checkpoint filename filtering silently discarded untracked files.
#[test]
fn system_git_round_trip_restores_commits_index_worktree_and_untracked() {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "-q", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Hel Test"]);
    git(source.path(), &["config", "user.email", "hel@example.test"]);
    // Exercise every fixture file regardless of the host's global ignore list.
    git(source.path(), &["config", "core.excludesFile", ""]);
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
    let configuration_files = ["config.json", ".env", "auth.toml", "credentials/notes.txt"];
    fs::create_dir_all(source.path().join("credentials")).unwrap();
    for relative in configuration_files {
        fs::write(source.path().join(relative), relative.as_bytes()).unwrap();
    }

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
    for relative in configuration_files {
        assert_eq!(
            fs::read(destination.join(relative)).unwrap(),
            relative.as_bytes(),
            "untracked file {relative} was not preserved"
        );
    }
}

// Hard-won: #1542: adding base retention must preserve pinless legacy archive restores.
#[test]
fn remote_workspace_restore_keeps_published_history_push_urls_and_dirty_state() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "base.txt", b"base\n", "base");
    commit_file(&origin, "dirty.txt", b"initial\n", "track dirty fixture");
    let base = git_line(&origin, &["rev-parse", "HEAD"]);
    let upstream = directory.path().join("upstream.git");
    git(
        directory.path(),
        &["init", "--bare", "-q", upstream.to_str().unwrap()],
    );
    let source = clone_repository(directory.path(), &origin, "source");
    git(
        &source,
        &[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            upstream.to_str().unwrap(),
        ],
    );
    git(&source, &["switch", "-q", "-c", "mj/remote-session"]);
    git(
        &source,
        &["config", "--local", "mj.remoteWorkspace", "true"],
    );
    git(&source, &["config", "--local", "mj.baseCommit", &base]);
    commit_file(
        &source,
        "session.txt",
        b"published session\n",
        "session work",
    );
    let head = git_line(&source, &["rev-parse", "HEAD"]);
    git(
        &source,
        &["push", "-q", "origin", "HEAD:refs/heads/mj/remote-session"],
    );
    // A fetch or push may advance an origin-tracking ref after publication.
    // The managed checkpoint still measures from its immutable launch base.
    git(&source, &["update-ref", "refs/remotes/origin/main", &head]);

    // The transport fixtures above are local; archived managed provenance
    // uses the network destinations a real provisioned workspace carries.
    git(
        &source,
        &[
            "remote",
            "set-url",
            "origin",
            "https://fetch.example.test/repo.git",
        ],
    );
    git(
        &source,
        &[
            "config",
            "--replace-all",
            "remote.origin.pushurl",
            "https://push.example.test/repo.git",
        ],
    );
    let large = vec![b'x'; 128 * 1024];
    fs::write(source.join("staged.txt"), &large).unwrap();
    git(&source, &["add", "staged.txt"]);
    fs::write(source.join("dirty.txt"), &large).unwrap();
    fs::write(source.join("untracked.txt"), &large).unwrap();

    let mut snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: PathBuf::from("repo"),
            history: GitHistoryMode::DeltaFrom(base.clone()),
            origin_override: None,
        },
    )
    .unwrap();
    snapshot.metadata.remote_workspace = true;
    assert_eq!(snapshot.metadata.base_commit, base);
    assert_eq!(
        snapshot.metadata.push_urls,
        vec!["https://push.example.test/repo.git".to_owned()]
    );
    assert!(!snapshot.committed_bundle.is_empty());
    assert!(snapshot.staged_patch.len() > 64 * 1024);
    assert!(snapshot.unstaged_patch.len() > 64 * 1024);
    assert!(snapshot.untracked_tar.len() > 64 * 1024);

    let destination = clone_repository(directory.path(), &origin, "restored");
    restore_git_snapshot(&SystemGit, &destination, &snapshot).unwrap();
    assert_eq!(git_line(&destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_line(&destination, &["config", "remote.origin.url"]),
        "https://fetch.example.test/repo.git"
    );
    assert_eq!(
        git_line(
            &destination,
            &["config", "--get-all", "remote.origin.pushurl"]
        ),
        "https://push.example.test/repo.git"
    );
    assert_eq!(
        git_line(&destination, &["config", "mj.remoteWorkspace"]),
        "true"
    );
    assert_eq!(git_line(&destination, &["config", "mj.baseCommit"]), base);
    assert_eq!(
        git_line(
            &destination,
            &["rev-parse", mj_core::targets::HistoryCutoff::BASE_REF]
        ),
        base
    );
    assert_eq!(
        git_line(&destination, &["config", "push.default"]),
        "current"
    );
    assert_eq!(
        git_line(&destination, &["config", "push.autoSetupRemote"]),
        "true"
    );
    assert_eq!(fs::read(destination.join("staged.txt")).unwrap(), large);
    assert_eq!(fs::read(destination.join("dirty.txt")).unwrap(), large);
    assert_eq!(fs::read(destination.join("untracked.txt")).unwrap(), large);
    assert_eq!(
        remote_workspace_base(&SystemGit, &destination).unwrap(),
        Some(base.clone())
    );
    let repeated = collect_git_snapshot(
        &SystemGit,
        &destination,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::DeltaFrom(base),
            origin_override: None,
        },
    )
    .unwrap();
    assert_eq!(repeated.metadata.head_commit, head);
    assert!(!repeated.committed_bundle.is_empty());
    assert_eq!(repeated.staged_patch, snapshot.staged_patch);
    assert_eq!(repeated.unstaged_patch, snapshot.unstaged_patch);
    // A pre-retention archive can restore HEAD while its unrelated launch
    // commit is no longer available at origin or in the archived delta.
    let abandoned = clone_repository(directory.path(), &origin, "old-launch");
    commit_file(
        &abandoned,
        "abandoned.txt",
        b"old launch\n",
        "abandoned launch base",
    );
    let missing_base = git_line(&abandoned, &["rev-parse", "HEAD"]);
    let mut legacy_snapshot = snapshot.clone();
    legacy_snapshot.metadata.base_commit = missing_base.clone();
    assert!(legacy_snapshot.metadata.saved_refs.is_empty());
    let mut legacy_input = input();
    legacy_input.bundle.primary_repository = "repo".into();
    legacy_input.repositories = vec![legacy_snapshot];
    let legacy_archive = directory.path().join("old-workspace.hel.zip");
    write_archive_atomic(&legacy_archive, &legacy_input).unwrap();
    let legacy_workspace = directory.path().join("legacy-restored");
    fs::create_dir_all(&legacy_workspace).unwrap();
    let legacy_destination = clone_repository(&legacy_workspace, &origin, "repo");
    let missing = SystemGit
        .run(
            &legacy_destination,
            &GitCommand {
                arguments: vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    format!("{missing_base}^{{commit}}").into(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(missing.status, 1);
    crate::checkpoint::restore_repositories(&legacy_archive, &legacy_workspace, &SystemGit)
        .unwrap();
    assert_eq!(git_line(&legacy_destination, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_line(&legacy_destination, &["config", "mj.baseCommit"]),
        missing_base
    );
    assert_eq!(
        git_line(&legacy_destination, &["config", "mj.remoteWorkspace"]),
        "true"
    );
    assert_eq!(
        fs::read(legacy_destination.join("dirty.txt")).unwrap(),
        large
    );
    let absent_ref = SystemGit
        .run(
            &legacy_destination,
            &GitCommand {
                arguments: vec![
                    "show-ref".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    mj_core::targets::HistoryCutoff::BASE_REF.into(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(absent_ref.status, 1);
}

fn object_files(root: &Path) -> Vec<PathBuf> {
    fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(&entry.path(), files);
            } else {
                files.push(entry.path());
            }
        }
    }
    let mut files = Vec::new();
    collect(&root.join(".git/objects"), &mut files);
    files.sort_unstable();
    files
}

#[test]
fn empty_tree_id_diffs_without_writing_an_object_in_either_git_object_format() {
    for format in ["sha1", "sha256"] {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path();
        git(root, &["init", "-q", &format!("--object-format={format}")]);
        git(root, &["config", "user.name", "Mjolnir Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        commit_file(root, "tracked.txt", b"base\n", "base");

        let current = git_line(root, &["rev-parse", "HEAD^{tree}"]);
        let before = object_files(root);
        let empty = empty_tree_id(&SystemGit, root).unwrap();
        let patch = diff_between_trees(&SystemGit, root, None, &current).unwrap();
        assert!(
            patch.contains("new file mode") && patch.contains("+base"),
            "{patch}"
        );
        assert_eq!(
            numstat_between_trees(&SystemGit, root, &empty, &current).unwrap(),
            b"1\t0\ttracked.txt\0"
        );
        assert_eq!(object_files(root), before, "{format}");
    }
}

#[test]
fn a_session_diff_shows_tracked_and_untracked_work_against_the_recorded_base() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());
    commit_file(repository.path(), "tracked.txt", b"base\n", "base");
    let base = git_line(repository.path(), &["rev-parse", "HEAD"]);
    git(repository.path(), &["config", "mj.remoteWorkspace", "true"]);
    git(repository.path(), &["config", "mj.baseCommit", &base]);

    // Work the agent committed, work it left staged, and a new file it never
    // told Git about all belong in what the session produced.
    commit_file(repository.path(), "tracked.txt", b"changed\n", "work");
    fs::write(repository.path().join("untracked.txt"), b"new\n").unwrap();

    let details = session_diff_details(&SystemGit, repository.path(), None, None, &[]).unwrap();
    assert!(
        details.diff.contains("tracked.txt |")
            && details.diff.contains("untracked.txt (untracked)"),
        "the diff covers committed and untracked work: {}",
        details.diff
    );
    assert_eq!(details.head_descends_from_base, Some(true));

    // Amending the base commit itself leaves HEAD on history the base is not
    // part of, so the diff carries changes the session did not write.
    git(repository.path(), &["reset", "--hard", &base]);
    git(repository.path(), &["commit", "--amend", "-m", "rewritten"]);
    assert_ne!(git_line(repository.path(), &["rev-parse", "HEAD"]), base);
    let rewritten = session_diff_details(&SystemGit, repository.path(), None, None, &[]).unwrap();
    assert_eq!(rewritten.head_descends_from_base, Some(false));
}

// Hard-won: #1083: mj diff hashed untracked files into the user's .git/objects
#[test]
fn a_session_diff_from_a_subdirectory_covers_the_checkout_without_writing_objects() {
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    initialize_repository(root);
    let text = "-tracked.txt";
    #[cfg(unix)]
    let special = ":(glob)literal[1].txt";
    #[cfg(not(unix))]
    let special = "literal[1].txt";
    commit_file(root, text, b"base\n", "base");
    commit_file(root, special, b"second\n", "second");
    commit_file(root, "blob.dat", b"\0before", "binary");
    commit_file(root, ".gitignore", b"ignored.txt\n", "ignore");
    // Allow a textconv cache in the repository; diff must not run the driver.
    commit_file(
        root,
        ".gitattributes",
        b"-tracked.txt diff=example\n",
        "attributes",
    );
    git(root, &["config", "diff.example.textconv", "echo converted"]);
    git(root, &["config", "diff.example.cachetextconv", "true"]);
    git(root, &["config", "diff.autoRefreshIndex", "true"]);
    fs::File::options()
        .write(true)
        .open(root.join(".gitignore"))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    let docs = root.join("docs");
    fs::create_dir(&docs).unwrap();
    fs::write(docs.join("draft.md"), b"untracked in docs\n").unwrap();
    fs::write(root.join("ignored.txt"), b"ignored\n").unwrap();
    for number in (0..501).rev() {
        fs::write(root.join(format!("note-{number:03}.txt")), b"untracked\n").unwrap();
    }
    let edited: String = (0..2500)
        .map(|i| format!("edited line {i:04} from the root even when invoked in docs\n"))
        .collect();
    fs::write(root.join(text), edited).unwrap();
    fs::write(root.join(special), b"changes to a literal special path\n").unwrap();
    fs::write(root.join("blob.dat"), b"\0after").unwrap();
    let before = object_files(root);
    let index = fs::read(root.join(".git/index")).unwrap();

    let summary = session_diff_details(&SystemGit, &docs, Some("HEAD"), None, &[]).unwrap();
    assert_eq!(summary.untracked_total, Some(502));
    assert_eq!(summary.untracked_files.len(), 500);
    assert_eq!(summary.untracked_files.first().unwrap(), "docs/draft.md");
    assert_eq!(summary.untracked_files.last().unwrap(), "note-498.txt");
    assert!(summary.diff.contains("docs/draft.md (untracked)"));
    assert!(summary.diff.contains("... and 2 more untracked files"));
    assert!(!summary.diff.contains("ignored.txt"));
    assert!(summary.diff.contains("3 files changed"));
    let mut files = summary.tracked_files;
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let mut expected_files = [
        DiffStatFile {
            path: text.into(),
            insertions: Some(2500),
            deletions: Some(1),
            binary: false,
        },
        DiffStatFile {
            path: special.into(),
            insertions: Some(1),
            deletions: Some(1),
            binary: false,
        },
        DiffStatFile {
            path: "blob.dat".into(),
            insertions: None,
            deletions: None,
            binary: true,
        },
    ];
    expected_files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(files, expected_files);

    let patch = session_diff_details(
        &SystemGit,
        &docs,
        Some("HEAD"),
        None,
        &[text.into(), special.into(), "blob.dat".into()],
    )
    .unwrap();
    let expected = git(
        root,
        &[
            "-c",
            "diff.autoRefreshIndex=false",
            "--literal-pathspecs",
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
            text,
            special,
            "blob.dat",
        ],
    );
    assert!(expected.len() > 64 * 1024);
    assert_eq!(patch.diff.as_bytes(), expected);
    assert!(patch.diff.contains("GIT binary patch"));
    assert_eq!(patch.untracked_total, None);
    assert!(patch.tracked_files.is_empty() && patch.untracked_files.is_empty());

    let exported = session_diff_patch(&SystemGit, &docs, Some("HEAD"), None).unwrap();
    assert_eq!(exported, patch.diff);

    for requested in [
        vec!["docs/draft.md".into(), text.into()],
        vec![String::new()],
    ] {
        let error =
            session_diff_details(&SystemGit, &docs, Some("HEAD"), None, &requested).unwrap_err();
        assert!(
            matches!(&error, SessionExportError::Refused(_)),
            "{error:?}"
        );
        if !requested[0].is_empty() {
            assert!(error.to_string().contains("docs/draft.md"));
            assert!(error.to_string().contains("mj fetch"));
        }
    }
    assert_eq!(object_files(root), before);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
}

#[test]
fn a_session_diff_includes_submodule_pointer_changes_despite_git_ignore_settings() {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("upstream");
    initialize_repository(&origin);
    commit_file(&origin, "source.txt", b"first\n", "first");
    let first = git_line(&origin, &["rev-parse", "HEAD"]);
    commit_file(&origin, "source.txt", b"second\n", "second");
    let second = git_line(&origin, &["rev-parse", "HEAD"]);

    let root = temp.path().join("parent");
    initialize_repository(&root);
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--",
            origin.to_str().unwrap(),
            "vendor",
        ],
    );
    let vendor = root.join("vendor");
    git(&vendor, &["checkout", "--detach", &first]);
    git(&root, &["add", "--", "vendor"]);
    git(&root, &["commit", "-qm", "record initial submodule"]);
    let base = git_line(&root, &["rev-parse", "HEAD"]);
    let before = object_files(&root);
    let paths = ["vendor".into()];

    for setting in ["diff.ignoreSubmodules", "submodule.vendor.ignore"] {
        git(&root, &["config", setting, "all"]);
        git(&vendor, &["checkout", "--detach", "--force", &first]);
        fs::write(vendor.join("source.txt"), b"modified\n").unwrap();
        fs::write(vendor.join("untracked.txt"), b"untracked\n").unwrap();

        let unchanged = session_diff_details(&SystemGit, &root, Some(&base), None, &[]).unwrap();
        assert!(unchanged.diff.is_empty(), "{setting}: {unchanged:?}");
        assert!(
            unchanged.tracked_files.is_empty(),
            "{setting}: {unchanged:?}"
        );
        let unchanged = session_diff_details(&SystemGit, &root, Some(&base), None, &paths).unwrap();
        assert!(unchanged.diff.is_empty(), "{setting}: {unchanged:?}");
        let unchanged = session_diff_patch(&SystemGit, &root, Some(&base), None).unwrap();
        assert!(unchanged.is_empty(), "{setting}: {unchanged}");

        git(&vendor, &["checkout", "--detach", "--force", &second]);
        fs::write(vendor.join("source.txt"), b"modified\n").unwrap();
        let summary = session_diff_details(&SystemGit, &root, Some(&base), None, &[]).unwrap();
        assert!(summary.diff.contains("vendor |"), "{setting}: {summary:?}");
        assert_eq!(
            summary.tracked_files,
            [DiffStatFile {
                path: "vendor".into(),
                insertions: Some(1),
                deletions: Some(1),
                binary: false,
            }],
            "{setting}"
        );
        let selected = session_diff_details(&SystemGit, &root, Some(&base), None, &paths)
            .unwrap()
            .diff;
        assert!(!selected.contains("-dirty"), "{setting}: {selected}");
        assert!(
            selected.contains(&format!("-Subproject commit {first}")),
            "{selected}"
        );
        assert!(
            selected.contains(&format!("+Subproject commit {second}")),
            "{selected}"
        );
        let exported = session_diff_patch(&SystemGit, &root, Some(&base), None).unwrap();
        assert_eq!(exported, selected, "{setting}");
        assert_eq!(object_files(&root), before, "{setting}");

        git(&root, &["config", "--unset", setting]);
    }
}

#[test]
fn a_session_diff_can_select_an_older_detached_base_without_changing_the_launch_base() {
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    initialize_repository(root);
    commit_file(root, "task.txt", b"original\n", "A");
    let task_base = git_line(root, &["rev-parse", "HEAD"]);
    commit_file(root, "unrelated.txt", b"unrelated history\n", "B");
    let launch_base = git_line(root, &["rev-parse", "HEAD"]);
    git(root, &["config", "mj.remoteWorkspace", "true"]);
    git(root, &["config", "mj.baseCommit", &launch_base]);
    git(root, &["checkout", "--detach", &task_base]);
    commit_file(root, "task.txt", b"committed task\n", "C");
    let head = git_line(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("staged.txt"), b"staged work\n").unwrap();
    git(root, &["add", "staged.txt"]);
    fs::write(root.join("task.txt"), b"committed task\nunstaged work\n").unwrap();
    fs::write(root.join("untracked.txt"), b"untracked work\n").unwrap();

    // A symbolic revision is resolved, not merely echoed into metadata.
    let chosen = session_diff_details(
        &SystemGit,
        root,
        Some("HEAD^"),
        None,
        &["task.txt".into(), "staged.txt".into()],
    )
    .unwrap();
    assert_eq!(chosen.base, task_base);
    assert_eq!(chosen.head, head);
    assert_eq!(chosen.head_descends_from_base, Some(true));
    for line in ["+committed task", "+unstaged work", "+staged work"] {
        assert!(
            chosen.diff.contains(line),
            "{line} missing: {}",
            chosen.diff
        );
    }
    assert!(!chosen.diff.contains("unrelated.txt"), "{}", chosen.diff);
    let default =
        session_diff_details(&SystemGit, root, None, None, &["unrelated.txt".into()]).unwrap();
    assert_eq!(default.base, launch_base);
    assert_eq!(default.head, head);
    // The session left the launch base behind, so its diff reverts the
    // unrelated commit the base carried.
    assert_eq!(default.head_descends_from_base, Some(false));
    assert!(
        default.diff.contains("-unrelated history"),
        "{}",
        default.diff
    );
    assert_eq!(git_line(root, &["config", "mj.baseCommit"]), launch_base);
    assert_eq!(
        git_line(root, &["diff", "--cached", "--name-only"]),
        "staged.txt"
    );

    for invalid in ["missing-base", "--all", "", "HEAD^{tree}"] {
        let error = session_diff_details(&SystemGit, root, Some(invalid), None, &[]).unwrap_err();
        assert!(matches!(error, SessionExportError::Refused(_)), "{error:?}");
    }
}

#[test]
fn a_session_diff_falls_back_to_the_commit_its_branch_was_created_at() {
    let repository = tempfile::tempdir().unwrap();
    initialize_repository(repository.path());
    commit_file(repository.path(), "tracked.txt", b"base\n", "base");
    let base = git_line(repository.path(), &["rev-parse", "HEAD"]);
    git(repository.path(), &["checkout", "-q", "-b", "mj/session-1"]);
    commit_file(repository.path(), "tracked.txt", b"changed\n", "work");

    let patch = session_diff(
        &SystemGit,
        repository.path(),
        None,
        Some("mj/session-1"),
        &["tracked.txt".into()],
    )
    .unwrap();
    assert!(
        patch.contains("-base") && patch.contains("+changed"),
        "with no recorded base the branch reflog names the session's start: {patch}"
    );

    // The same repository with no base and no branch cannot answer at all.
    let error = session_diff(&SystemGit, repository.path(), None, None, &[]).unwrap_err();
    assert!(
        format!("{error:#}").contains("no session base recorded"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        git_line(repository.path(), &["rev-parse", "HEAD~1"]),
        base,
        "the diff reads the repository without moving it"
    );
}

#[test]
fn pushing_a_session_branch_reaches_origin_and_reports_a_missing_remote() {
    let parent = tempfile::tempdir().unwrap();
    let repository = parent.path().join("work");
    initialize_repository(&repository);
    commit_file(&repository, "tracked.txt", b"base\n", "base");

    let error = push_branch(&SystemGit, &repository, "review/one").unwrap_err();
    assert!(
        matches!(error, PushBranchError::NoRemote),
        "a repository with no remote refuses before running a push: {error}"
    );

    let remote = parent.path().join("origin.git");
    git(
        parent.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repository,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );

    let pushed = push_branch(&SystemGit, &repository, "review/one").unwrap();
    assert_eq!(pushed.remote, "origin");
    assert_eq!(pushed.branch, "review/one");
    assert_eq!(
        git_line(&remote, &["rev-parse", "refs/heads/review/one"]),
        git_line(&repository, &["rev-parse", "HEAD"]),
        "the pushed branch names the session's current commit"
    );
}

#[test]
fn a_session_file_read_stays_inside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(root.path().join("inside.txt"), b"visible\n").unwrap();
    fs::write(outside.path().join("secret.txt"), b"private\n").unwrap();

    assert_eq!(
        read_session_file(root.path(), Path::new("inside.txt")).unwrap(),
        b"visible\n"
    );

    let error = read_session_file(root.path(), Path::new("../secret.txt")).unwrap_err();
    assert!(
        format!("{error:#}").contains(".."),
        "unexpected error: {error:#}"
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("link"))
            .unwrap();
        let error = read_session_file(root.path(), Path::new("link")).unwrap_err();
        assert!(
            format!("{error:#}").contains("leaves the session workspace"),
            "a symlink out of the workspace is refused: {error:#}"
        );
    }
}

/// A person who asked for the wrong path has to be able to see where the read
/// looked, because the directory differs per target kind (#1079).
// Hard-won: f870729d: a reported file-export lookup bug lacked the searched directory in its refusal.
#[test]
fn a_missing_session_file_refusal_names_the_directory_it_searched() {
    let root = tempfile::tempdir().unwrap();
    let error = read_session_file(root.path(), Path::new("secret.txt")).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("secret.txt is not in the session workspace"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains(&root.path().display().to_string()),
        "the refusal names the directory it searched: {message}"
    );
}

// Hard-won: c61f0d6fe: Git 2.25 echoed --path-format=absolute, so the capture started from HEAD instead of the real index.
#[test]
fn archive_round_trip_preserves_clear_boundary_and_requires_context_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cleared.hel.zip");
    let mut input = input();
    input.canonical_session.transcript[1].stable_id = "context-cleared:reset".into();
    let verified = write_archive_atomic(&path, &input).unwrap();
    assert_eq!(
        verified.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_CONTEXT
    );
    assert_eq!(verified.canonical_session, input.canonical_session);
    assert_eq!(verified.canonical_session.current_context_start(), 2);
    input
        .canonical_session
        .transcript
        .push(CanonicalTranscriptItem {
            stable_id: "message:received-one".into(),
            position: 3,
            latest_content_event_ordinal: None,
            created_at_ms: 300,
            last_changed_at_ms: 300,
            body: CanonicalTranscriptBody::Message {
                event: Box::new(mj_core::mailbox::MailboxEvent {
                    key: "received-one".into(),
                    source: "session_message".into(),
                    wake: true,
                    created_at_ms: 200,
                    body: mj_core::mailbox::MailboxEventBody::SessionMessage {
                        from: mj_core::mailbox::Sender::Session {
                            id: "sender-session".into(),
                            title: "Reviewer".into(),
                        },
                        text: "All of the message.\nIncluding 支持 Unicode and the second line."
                            .into(),
                    },
                }),
            },
        });
    let verified = write_archive_atomic(&path, &input).unwrap();
    assert_eq!(
        verified.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_MESSAGES
    );
    assert_eq!(verified.canonical_session, input.canonical_session);
    assert_eq!(verified.canonical_session.current_context_start(), 2);
    let (mut manifest, payloads) = prepare_archive(&input).unwrap();
    manifest.schema_version = ARCHIVE_SCHEMA_VERSION_CONTEXT;
    let downgraded = directory.path().join("message-downgraded.hel.zip");
    let mut file = File::create(&downgraded).unwrap();
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);
    let error = read_archive_verified(&downgraded).unwrap_err();
    assert!(format!("{error:#}").contains("message transcript entries require archive schema 9"));
}

#[test]
fn native_use_evidence_uses_schema_15_with_and_without_origin_delta_and_rejects_downgrade() {
    let directory = tempfile::tempdir().unwrap();
    let evidence = mj_core::relay::NativeSessionUseEvidence {
        version: 1,
        native_session_id: "native-1".into(),
        opened_ordinal: Some(2),
        usage: mj_core::relay::NativeSessionUse::Unknown,
    };

    // Without origin_delta: writes schema 15, reads back accurately, rejects downgrade to 13.
    let mut archive_input = input();
    archive_input.canonical_session.native_use_evidence = Some(evidence.clone());
    let path = directory.path().join("native-use.hel.zip");
    let verified = write_archive_atomic(&path, &archive_input).unwrap();
    assert_eq!(
        verified.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_NATIVE_USE
    );
    assert_eq!(
        verified.canonical_session.native_use_evidence,
        Some(evidence.clone())
    );

    let (mut manifest, payloads) = prepare_archive(&archive_input).unwrap();
    manifest.schema_version = ARCHIVE_SCHEMA_VERSION_MAILBOX;
    let downgraded = directory.path().join("native-use-downgraded.hel.zip");
    let mut file = File::create(&downgraded).unwrap();
    write_zip(&mut file, directory.path(), &manifest, &payloads).unwrap();
    drop(file);
    let error = read_archive_verified(&downgraded).unwrap_err();
    assert!(
        format!("{error:#}").contains("native-session-use evidence requires archive schema 15"),
        "unexpected error: {error:#}"
    );

    // With origin_delta: also writes schema 15 and verifies cleanly.
    let mut delta_input = input();
    delta_input.canonical_session.native_use_evidence = Some(evidence.clone());
    let repo = &mut delta_input.repositories[0];
    repo.committed_bundle.clear();
    repo.staged_patch.clear();
    repo.unstaged_patch.clear();
    repo.untracked_tar.clear();
    repo.metadata.saved_refs.clear();
    repo.metadata.stash_stack.clear();
    let origin_url = repo.metadata.origin.clone();
    let head_commit = repo.metadata.head_commit.clone();
    repo.metadata.origin_delta = Some(Box::new(WorkspaceSnapshotMetadata {
        origin_url: Some(origin_url),
        object_format: "sha1".into(),
        notices: Vec::new(),
        head: SnapshotHead::Branch {
            name: "main".into(),
            commit: head_commit.clone(),
        },
        refs: BTreeMap::from([("refs/heads/main".into(), head_commit)]),
        branch_tracking: BTreeMap::new(),
        stash: Vec::new(),
        working_state: None,
        working_state_skipped: false,
        has_uncommitted_state: false,
        worktrees: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        prerequisites: Vec::new(),
        history_cutoff: false,
    }));
    let delta_path = directory.path().join("native-use-origin-delta.hel.zip");
    let delta_verified = write_archive_atomic(&delta_path, &delta_input).unwrap();
    assert_eq!(
        delta_verified.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_NATIVE_USE
    );
    assert_eq!(
        delta_verified.canonical_session.native_use_evidence,
        Some(evidence)
    );
}

fn empty_tar() -> Vec<u8> {
    let mut body = Vec::new();
    tar::Builder::new(&mut body).finish().unwrap();
    body
}

fn uncompressed_untracked_excess(manifest: &ArchiveManifest) -> u64 {
    manifest
        .payloads
        .iter()
        .filter(|payload| {
            matches!(
                payload.role,
                PayloadRole::GitUntrackedTar { .. }
                    | PayloadRole::GitLinkedWorktreeUntrackedTar { .. }
            ) && payload.size > 1024
        })
        .map(|payload| payload.size - 1024)
        .sum()
}

fn compressed_untracked_entries(path: &Path) -> u64 {
    let file = fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).unwrap();
        if entry.name().contains("untracked.tar") {
            total += entry.compressed_size();
        }
    }
    total
}

fn origin_delta_repository(repository: &mut RepositorySnapshot) {
    repository.committed_bundle.clear();
    repository.staged_patch.clear();
    repository.unstaged_patch.clear();
    repository.untracked_tar.clear();
    repository.operation_payloads = None;
    repository.linked_worktrees.clear();
    repository.metadata.saved_refs.clear();
    repository.metadata.stash_stack.clear();
    repository.metadata.git_operation = None;
    let origin = repository.metadata.origin.clone();
    let head = repository.metadata.head_commit.clone();
    let branch = repository
        .metadata
        .branch
        .clone()
        .unwrap_or_else(|| "main".into());
    repository.metadata.origin_delta = Some(Box::new(WorkspaceSnapshotMetadata {
        origin_url: Some(origin),
        object_format: "sha1".into(),
        notices: Vec::new(),
        head: SnapshotHead::Branch {
            name: branch,
            commit: head.clone(),
        },
        refs: BTreeMap::from([("refs/heads/main".into(), head)]),
        branch_tracking: BTreeMap::new(),
        stash: Vec::new(),
        working_state: None,
        working_state_skipped: false,
        has_uncommitted_state: false,
        worktrees: Vec::new(),
        initialized_submodules: Vec::new(),
        index_gitlinks: Vec::new(),
        remote_refs_stale: false,
        prerequisites: Vec::new(),
        history_cutoff: false,
    }));
}

fn snapshot_from_verified(
    archive: &VerifiedArchive,
    repository: &RepositoryManifest,
) -> RepositorySnapshot {
    let id = repository.metadata.id.clone();
    RepositorySnapshot {
        origin_delta_pack: None,
        metadata: repository.metadata.clone(),
        committed_bundle: archive
            .payload_by_role(&PayloadRole::GitBundle {
                repository_id: id.clone(),
            })
            .unwrap()
            .to_vec(),
        staged_patch: archive
            .payload_by_role(&PayloadRole::GitStagedPatch {
                repository_id: id.clone(),
            })
            .unwrap()
            .to_vec(),
        unstaged_patch: archive
            .payload_by_role(&PayloadRole::GitUnstagedPatch {
                repository_id: id.clone(),
            })
            .unwrap()
            .to_vec(),
        untracked_tar: archive
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: id.clone(),
            })
            .unwrap()
            .to_vec(),
        operation_payloads: repository.metadata.git_operation.as_ref().map(|_| {
            GitOperationPayloads {
                conflicted_worktree_tar: archive
                    .payload_by_role(&PayloadRole::GitConflictedWorktreeTar {
                        repository_id: id.clone(),
                    })
                    .unwrap()
                    .to_vec(),
                state_tar: archive
                    .payload_by_role(&PayloadRole::GitOperationStateTar {
                        repository_id: id.clone(),
                    })
                    .unwrap()
                    .to_vec(),
            }
        }),
        linked_worktrees: repository
            .linked_worktrees
            .iter()
            .enumerate()
            .map(|(index, worktree)| {
                let worktree_index = index as u32;
                LinkedWorktreeSnapshot {
                    metadata: worktree.metadata.clone(),
                    staged_patch: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeStagedPatch {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        })
                        .unwrap()
                        .to_vec(),
                    unstaged_patch: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeUnstagedPatch {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        })
                        .unwrap()
                        .to_vec(),
                    untracked_tar: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeUntrackedTar {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        })
                        .unwrap()
                        .to_vec(),
                    operation_payloads: None,
                }
            })
            .collect(),
    }
}

#[test]
fn purge_replaces_primary_and_linked_untracked_files_and_restores_without_them() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("origin");
    initialize_repository(&origin);
    commit_file(&origin, "base.txt", b"base\n", "base");
    let base = git_line(&origin, &["rev-parse", "HEAD"]);
    let source = clone_repository(directory.path(), &origin, "source");
    commit_file(&source, "first.txt", b"first\n", "first");
    fs::write(source.join("base.txt"), b"stashed tracked\n").unwrap();
    fs::write(source.join("stash-only.txt"), b"stashed untracked\n").unwrap();
    git(
        &source,
        &["stash", "push", "-u", "-q", "-m", "holds untracked"],
    );
    fs::write(source.join("staged.txt"), b"staged\n").unwrap();
    git(&source, &["add", "staged.txt"]);
    fs::write(source.join("base.txt"), b"unstaged edit\n").unwrap();
    fs::write(source.join("scratch.txt"), b"scratch\n").unwrap();
    git(&source, &["worktree", "add", "-q", "-b", "lane", "lane"]);
    fs::write(source.join("lane").join("lane-note.txt"), b"lane\n").unwrap();
    let head = git_line(&source, &["rev-parse", "HEAD"]);

    let snapshot = collect_git_snapshot(
        &SystemGit,
        &source,
        &GitCollectionSpec {
            id: "repo".into(),
            relative_destination: "repo".into(),
            history: GitHistoryMode::CloneFrom(base),
            origin_override: None,
        },
    )
    .unwrap();
    assert!(!snapshot.untracked_tar.is_empty());
    assert_eq!(snapshot.linked_worktrees.len(), 1);
    assert!(!snapshot.linked_worktrees[0].untracked_tar.is_empty());
    assert!(
        snapshot
            .metadata
            .stash_stack
            .iter()
            .any(|entry| entry.message.contains("holds untracked"))
    );

    let mut archive_input = input();
    archive_input.bundle.id = "repo".into();
    archive_input.bundle.primary_repository = "repo".into();
    archive_input.repositories = vec![snapshot];
    let source_archive = directory.path().join("legacy.hel.zip");
    let written = write_archive_atomic(&source_archive, &archive_input).unwrap();
    let schema = written.manifest.schema_version;
    assert_ne!(schema, ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA);
    let before = read_archive_verified(&source_archive).unwrap();
    let source_len = fs::metadata(&source_archive).unwrap().len();

    let destination = directory.path().join("purged.hel.zip");
    let dry_missing = directory.path().join("no-such-dir").join("dry.hel.zip");
    let dry = purge_untracked_from_archive(&source_archive, &dry_missing, true).unwrap();
    assert!(matches!(dry.kind, ArchiveUntrackedPurgeKind::DryRun));
    assert!(dry.bytes_reclaimed > 0);
    assert!(!directory.path().join("no-such-dir").exists());
    assert!(
        dry.stash_untracked_notes
            .iter()
            .any(|note| note.repository_id == "repo" && note.message.contains("holds untracked"))
    );

    let report = purge_untracked_from_archive(&source_archive, &destination, false).unwrap();
    let ArchiveUntrackedPurgeKind::Rewritten { archive_sha256 } = report.kind else {
        panic!("expected a rewritten archive, got {:?}", report.kind);
    };
    assert_eq!(report.schema_version, schema);
    let uncompressed = uncompressed_untracked_excess(&before.manifest);
    assert!(uncompressed > dry.bytes_reclaimed);
    assert!(uncompressed > report.bytes_reclaimed);
    assert_eq!(
        report.bytes_reclaimed,
        source_len.saturating_sub(fs::metadata(&destination).unwrap().len())
    );
    assert_eq!(report.stash_untracked_notes, dry.stash_untracked_notes);
    let streamed = verify_archive_streaming(&destination).unwrap();
    assert_eq!(streamed.archive_sha256, archive_sha256);
    assert_eq!(streamed.manifest.schema_version, schema);
    let after = read_archive_verified(&destination).unwrap();
    assert_eq!(after.archive_sha256, archive_sha256);
    assert!(fs::metadata(&destination).unwrap().len() < source_len);
    for role in [
        PayloadRole::GitBundle {
            repository_id: "repo".into(),
        },
        PayloadRole::GitStagedPatch {
            repository_id: "repo".into(),
        },
        PayloadRole::GitUnstagedPatch {
            repository_id: "repo".into(),
        },
        PayloadRole::GitLinkedWorktreeStagedPatch {
            repository_id: "repo".into(),
            worktree: 0,
        },
        PayloadRole::GitLinkedWorktreeUnstagedPatch {
            repository_id: "repo".into(),
            worktree: 0,
        },
        PayloadRole::CanonicalSession,
    ] {
        assert_eq!(
            before.payload_by_role(&role).unwrap(),
            after.payload_by_role(&role).unwrap(),
            "{role:?} changed"
        );
    }
    for role in [
        PayloadRole::GitUntrackedTar {
            repository_id: "repo".into(),
        },
        PayloadRole::GitLinkedWorktreeUntrackedTar {
            repository_id: "repo".into(),
            worktree: 0,
        },
    ] {
        let body = after.payload_by_role(&role).unwrap();
        assert_eq!(body, empty_tar().as_slice(), "{role:?} was not emptied");
    }

    let restored = snapshot_from_verified(&after, &after.manifest.repositories[0]);
    let destination_repo = clone_repository(directory.path(), &origin, "restored");
    restore_git_snapshot(&SystemGit, &destination_repo, &restored).unwrap();
    assert_eq!(git_line(&destination_repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read(destination_repo.join("first.txt")).unwrap(),
        b"first\n"
    );
    assert_eq!(
        fs::read(destination_repo.join("staged.txt")).unwrap(),
        b"staged\n"
    );
    assert_eq!(
        fs::read(destination_repo.join("base.txt")).unwrap(),
        b"unstaged edit\n"
    );
    assert_eq!(
        git(&destination_repo, &["diff", "--cached"]),
        git(&source, &["diff", "--cached"])
    );
    assert_eq!(git(&destination_repo, &["diff"]), git(&source, &["diff"]));
    assert!(!destination_repo.join("scratch.txt").exists());
    assert!(!destination_repo.join("lane").join("lane-note.txt").exists());
    assert!(destination_repo.join("lane").join(".git").is_file());
}

#[test]
fn purge_dry_run_and_already_clean_archives_do_not_write() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.hel.zip");
    write_archive_atomic(&source, &input()).unwrap();
    let destination = directory.path().join("dest.hel.zip");
    let dry = purge_untracked_from_archive(&source, &destination, true).unwrap();
    assert!(matches!(dry.kind, ArchiveUntrackedPurgeKind::DryRun));
    assert!(dry.bytes_reclaimed > 0);
    assert!(!destination.exists());

    for untracked in [Vec::new(), empty_tar()] {
        let mut clean = input();
        for repository in &mut clean.repositories {
            repository.untracked_tar = untracked.clone();
        }
        let clean_path = directory
            .path()
            .join(format!("clean-{}.hel.zip", untracked.len()));
        write_archive_atomic(&clean_path, &clean).unwrap();
        let clean_dest = directory
            .path()
            .join(format!("clean-{}-out.hel.zip", untracked.len()));
        let report = purge_untracked_from_archive(&clean_path, &clean_dest, false).unwrap();
        assert!(matches!(
            report.kind,
            ArchiveUntrackedPurgeKind::AlreadyClean
        ));
        assert_eq!(report.bytes_reclaimed, 0);
        assert!(!clean_dest.exists());
    }
}

#[test]
fn purge_reports_on_disk_bytes_for_a_compressible_untracked_payload() {
    let directory = tempfile::tempdir().unwrap();
    let mut archive_input = input();
    let repetitive = vec![b'a'; 64 * 1024];
    archive_input.repositories[0].untracked_tar = tar_with_file("pad.txt", &repetitive, 0o644);
    archive_input.repositories[1].untracked_tar.clear();
    let source = directory.path().join("compressible.hel.zip");
    write_archive_atomic(&source, &archive_input).unwrap();
    let before = verify_archive_streaming(&source).unwrap();
    let uncompressed = uncompressed_untracked_excess(&before.manifest);
    let removed = compressed_untracked_entries(&source);
    assert!(uncompressed > 60_000);
    assert!(removed < uncompressed);

    let dry =
        purge_untracked_from_archive(&source, &directory.path().join("dry.hel.zip"), true).unwrap();
    assert!(matches!(dry.kind, ArchiveUntrackedPurgeKind::DryRun));
    assert!(dry.bytes_reclaimed > 0);
    assert!(dry.bytes_reclaimed <= removed);
    assert!(removed - dry.bytes_reclaimed < 2048);
    assert!(dry.bytes_reclaimed < uncompressed);
    assert!(!directory.path().join("dry.hel.zip").exists());

    let old_len = fs::metadata(&source).unwrap().len();
    let destination = directory.path().join("purged.hel.zip");
    let report = purge_untracked_from_archive(&source, &destination, false).unwrap();
    let new_len = fs::metadata(&destination).unwrap().len();
    assert!(matches!(
        report.kind,
        ArchiveUntrackedPurgeKind::Rewritten { .. }
    ));
    assert_eq!(report.bytes_reclaimed, old_len.saturating_sub(new_len));
    assert!(report.bytes_reclaimed > 0);
    assert!(report.bytes_reclaimed < uncompressed);
}

#[test]
fn purge_reports_origin_delta_and_still_rewrites_a_mixed_archive() {
    let directory = tempfile::tempdir().unwrap();
    let mut full = input();
    for repository in &mut full.repositories {
        origin_delta_repository(repository);
    }
    let full_path = directory.path().join("origin.hel.zip");
    write_archive_atomic(&full_path, &full).unwrap();
    let full_dest = directory.path().join("origin-out.hel.zip");
    let report = purge_untracked_from_archive(&full_path, &full_dest, false).unwrap();
    assert!(matches!(
        report.kind,
        ArchiveUntrackedPurgeKind::AlreadyNewFormat
    ));
    assert!(!full_dest.exists());

    let mut mixed = input();
    origin_delta_repository(&mut mixed.repositories[0]);
    let mixed_path = directory.path().join("mixed.hel.zip");
    let written = write_archive_atomic(&mixed_path, &mixed).unwrap();
    assert!(written.manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA);
    let mixed_dest = directory.path().join("mixed-out.hel.zip");
    let purged = purge_untracked_from_archive(&mixed_path, &mixed_dest, false).unwrap();
    assert!(matches!(
        purged.kind,
        ArchiveUntrackedPurgeKind::Rewritten { .. }
    ));
    let verified = verify_archive_streaming(&mixed_dest).unwrap();
    assert_eq!(
        verified.manifest.schema_version,
        written.manifest.schema_version
    );
    assert!(
        verified.manifest.repositories[0]
            .metadata
            .origin_delta
            .is_some()
    );
    assert!(
        verified.manifest.repositories[1]
            .metadata
            .origin_delta
            .is_none()
    );
    let after = read_archive_verified(&mixed_dest).unwrap();
    assert_eq!(
        after
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: "worker".into(),
            })
            .unwrap(),
        empty_tar().as_slice()
    );
}

#[test]
fn purge_keeps_schema_3_when_only_the_untracked_tar_was_sharded() {
    let directory = tempfile::tempdir().unwrap();
    let mut archive_input = input();
    let large = tar_with_file("big.bin", &vec![b'x'; TEST_PART_BYTES], 0o644);
    assert!(large.len() > TEST_PART_BYTES);
    archive_input.repositories[0].untracked_tar = large;
    let source = directory.path().join("sharded.hel.zip");
    write_archive_installed_with_part_size(&source, &archive_input, TEST_PART_BYTES).unwrap();
    let before = verify_archive_streaming(&source).unwrap();
    assert_eq!(
        before.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_SHARDED
    );
    assert!(before.manifest.payloads.iter().any(|payload| {
        matches!(payload.role, PayloadRole::GitUntrackedTar { .. }) && payload.parts.len() > 1
    }));
    let destination = directory.path().join("sharded-purged.hel.zip");
    let old_len = fs::metadata(&source).unwrap().len();
    let uncompressed = uncompressed_untracked_excess(&before.manifest);
    let removed = compressed_untracked_entries(&source);
    let dry =
        purge_untracked_from_archive(&source, &directory.path().join("sharded-dry.hel.zip"), true)
            .unwrap();
    assert!(matches!(dry.kind, ArchiveUntrackedPurgeKind::DryRun));
    assert!(dry.bytes_reclaimed > 0);
    assert!(dry.bytes_reclaimed <= removed);
    assert!(removed - dry.bytes_reclaimed < 2048);
    assert!(dry.bytes_reclaimed < uncompressed);
    let report = purge_untracked_from_archive(&source, &destination, false).unwrap();
    assert!(matches!(
        report.kind,
        ArchiveUntrackedPurgeKind::Rewritten { .. }
    ));
    assert_eq!(
        report.bytes_reclaimed,
        old_len.saturating_sub(fs::metadata(&destination).unwrap().len())
    );
    assert!(report.bytes_reclaimed > 0);
    assert!(report.bytes_reclaimed < uncompressed);
    let after = verify_archive_streaming(&destination).unwrap();
    assert_eq!(
        after.manifest.schema_version,
        ARCHIVE_SCHEMA_VERSION_SHARDED
    );
    let untracked = after
        .manifest
        .payloads
        .iter()
        .find(|payload| {
            payload.role
                == PayloadRole::GitUntrackedTar {
                    repository_id: "hel".into(),
                }
        })
        .unwrap();
    assert_eq!(untracked.size, 1024);
    assert!(untracked.parts.len() > 1);
    let read = read_archive_verified(&destination).unwrap();
    assert_eq!(
        read.payload_by_role(&PayloadRole::GitUntrackedTar {
            repository_id: "hel".into(),
        })
        .unwrap(),
        empty_tar().as_slice()
    );
}

#[test]
fn purge_preserves_operation_state_and_other_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let mut archive_input = input();
    let object = "a".repeat(40);
    archive_input.repositories[0].metadata.git_operation = Some(SavedGitOperation {
        unmerged: vec![SavedUnmergedEntry {
            path: PathBuf::from("conflict.txt"),
            stage: 2,
            mode: "100644".into(),
            object: object.clone(),
        }],
        missing_worktree_paths: Vec::new(),
        objects: vec![object],
    });
    archive_input.repositories[0].operation_payloads = Some(GitOperationPayloads {
        conflicted_worktree_tar: tar_with_file("conflict.txt", b"<<<<<<<\n", 0o644),
        state_tar: tar_with_file("MERGE_HEAD", b"abc\n", 0o644),
    });
    let source = directory.path().join("operation.hel.zip");
    write_archive_atomic(&source, &archive_input).unwrap();
    let before = read_archive_verified(&source).unwrap();
    let destination = directory.path().join("operation-purged.hel.zip");
    let report = purge_untracked_from_archive(&source, &destination, false).unwrap();
    assert_eq!(report.schema_version, before.manifest.schema_version);
    let after = read_archive_verified(&destination).unwrap();
    assert_eq!(
        after.manifest.schema_version,
        before.manifest.schema_version
    );
    for role in [
        PayloadRole::GitOperationStateTar {
            repository_id: "hel".into(),
        },
        PayloadRole::GitConflictedWorktreeTar {
            repository_id: "hel".into(),
        },
        PayloadRole::GitBundle {
            repository_id: "hel".into(),
        },
        PayloadRole::GitStagedPatch {
            repository_id: "hel".into(),
        },
        PayloadRole::NativeArtifact {
            relative_path: PathBuf::from("sessions/native-1/rollout.jsonl"),
        },
    ] {
        assert_eq!(
            before.payload_by_role(&role).unwrap(),
            after.payload_by_role(&role).unwrap(),
            "{role:?} changed"
        );
    }
    assert_eq!(
        after
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: "hel".into(),
            })
            .unwrap(),
        empty_tar().as_slice()
    );
}

#[test]
fn purge_leaves_an_existing_destination_archive_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.hel.zip");
    write_archive_atomic(&source, &input()).unwrap();
    let source_bytes = fs::read(&source).unwrap();
    let destination = directory.path().join("dest.hel.zip");
    fs::write(&destination, b"keep-me").unwrap();
    let error = purge_untracked_from_archive(&source, &destination, false).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("existing"), "{message}");
    assert_eq!(fs::read(&destination).unwrap(), b"keep-me");
    assert_eq!(fs::read(&source).unwrap(), source_bytes);
}

#[test]
fn purge_detects_untracked_stash_entries_in_a_sharded_bundle_without_buffering_blobs() {
    let directory = tempfile::tempdir().unwrap();
    // Identical bytes zlib-compress to a single shard. A counter does not, so
    // the bundle crosses several part boundaries while its blob stays large.
    let blob: Vec<u8> = (0..200 * 1024).map(|index| (index * 17) as u8).collect();
    let (bundle, stash_commit) = bundle_with_untracked_stash(&blob);
    let mut archive_input = input();
    archive_input.repositories[0].committed_bundle = bundle;
    archive_input.repositories[0].metadata.stash_stack = vec![SavedStashEntry {
        commit: stash_commit,
        message: "holds untracked".into(),
    }];
    let (mut manifest, mut payloads) =
        prepare_archive_with_part_size(&archive_input, TEST_PART_BYTES).unwrap();
    let bundle_index = payloads
        .iter()
        .position(|payload| {
            payload.descriptor.role
                == PayloadRole::GitBundle {
                    repository_id: "hel".into(),
                }
        })
        .unwrap();
    let data = payloads[bundle_index].data.as_ref();
    // Stored bundles are not split by the writer. Cut this one explicitly so
    // the reader has to walk every shard in order.
    let part_bytes = (data.len() / 3).max(1);
    assert!(data.len() > part_bytes);
    let path = payloads[bundle_index].descriptor.path.clone();
    let parts = data
        .chunks(part_bytes)
        .enumerate()
        .map(|(index, chunk)| PayloadPartDescriptor {
            path: payload_part_path(&path, index),
            sha256: digest_bytes(chunk),
            size: chunk.len() as u64,
        })
        .collect::<Vec<_>>();
    assert!(parts.len() > 1);
    payloads[bundle_index].descriptor.parts = parts.clone();
    manifest
        .payloads
        .iter_mut()
        .find(|payload| payload.path == path)
        .unwrap()
        .parts = parts;

    let source = directory.path().join("sharded-bundle.hel.zip");
    let mut temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    restrict_archive_permissions(temporary.path()).unwrap();
    write_zip(
        temporary.as_file_mut(),
        directory.path(),
        &manifest,
        &payloads,
    )
    .unwrap();
    temporary.as_file_mut().sync_all().unwrap();
    temporary
        .persist(&source)
        .map_err(|error| error.error)
        .unwrap();
    restrict_archive_permissions(&source).unwrap();
    let verified = verify_archive_streaming(&source).unwrap();
    assert!(verified.manifest.payloads.iter().any(|payload| {
        payload.role
            == PayloadRole::GitBundle {
                repository_id: "hel".into(),
            }
            && payload.parts.len() > 1
    }));

    let destination = directory.path().join("sharded-out.hel.zip");
    purge::reset_bundle_discard_stats();
    let report = purge_untracked_from_archive(&source, &destination, true).unwrap();
    assert!(matches!(report.kind, ArchiveUntrackedPurgeKind::DryRun));
    assert!(!destination.exists());
    assert!(
        report.stash_untracked_notes.iter().any(|note| {
            note.repository_id == "hel" && note.message.contains("holds untracked")
        }),
        "{:?}",
        report.stash_untracked_notes
    );
    let (peak, total) = purge::bundle_discard_stats();
    assert!(peak > 0 && peak <= 8 * 1024, "discard peak {peak}");
    assert!(
        total >= blob.len() as u64,
        "discarded {total} bytes of a {} byte blob",
        blob.len()
    );
}

fn bundle_with_untracked_stash(blob: &[u8]) -> (Vec<u8>, String) {
    let commit = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 0000000000000000000000000000000000000001\n\
parent 0000000000000000000000000000000000000002\n\
parent 0000000000000000000000000000000000000003\n\
author T <t@t> 1 +0000\n\
committer T <t@t> 1 +0000\n\
\n\
holds untracked\n";
    let oid = purge::git_object_id("commit", commit, false);
    let mut pack = b"PACK".to_vec();
    pack.extend_from_slice(&2_u32.to_be_bytes());
    pack.extend_from_slice(&2_u32.to_be_bytes());
    push_stored_object(&mut pack, 1, commit);
    push_stored_object(&mut pack, 3, blob);
    pack.extend_from_slice(&[0_u8; 20]);
    let mut bundle = b"# v2 git bundle\n\n".to_vec();
    bundle.extend_from_slice(&pack);
    (bundle, oid)
}

fn push_stored_object(pack: &mut Vec<u8>, typ: u8, body: &[u8]) {
    let mut size = body.len();
    let mut first = (typ << 4) | ((size & 0x0f) as u8);
    size >>= 4;
    if size > 0 {
        first |= 0x80;
    }
    pack.push(first);
    while size > 0 {
        let mut byte = (size & 0x7f) as u8;
        size >>= 7;
        if size > 0 {
            byte |= 0x80;
        }
        pack.push(byte);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, body).unwrap();
    pack.extend(encoder.finish().unwrap());
}

#[test]
fn older_archives_without_terminal_detail_metadata_still_restore_full_output() {
    use mj_core::archive::CanonicalTerminalOutput;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.hel.zip");
    let mut legacy = input();
    let output = "terminal log line\n".repeat(8 * 1024);
    legacy
        .canonical_session
        .transcript
        .push(CanonicalTranscriptItem {
            stable_id: "terminal:legacy".into(),
            position: 3,
            latest_content_event_ordinal: None,
            created_at_ms: 104,
            last_changed_at_ms: 104,
            body: CanonicalTranscriptBody::TerminalOutput {
                record: CanonicalTerminalOutput {
                    terminal_id: "legacy".into(),
                    output: output.clone(),
                    truncated: false,
                    exit_code: Some(1),
                    signal: None,
                },
                output_line_count: None,
                first_error_line: None,
                detail_trimmed: false,
            },
        });
    write_archive_atomic(&path, &legacy).unwrap();
    let mut zip = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let raw: serde_json::Value =
        serde_json::from_reader(zip.by_name("canonical/session.json").unwrap()).unwrap();
    let old_body = &raw["transcript"][2]["body"];
    assert_eq!(old_body["record"]["output"], output);
    for field in ["output_line_count", "first_error_line", "detail_trimmed"] {
        assert!(old_body.get(field).is_none(), "{field}");
    }

    let archive = verify_archive_streaming(&path).unwrap();
    assert_eq!(
        archive.canonical_session.transcript[2],
        legacy.canonical_session.transcript[2]
    );
}
