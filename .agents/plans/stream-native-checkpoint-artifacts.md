# Stream native harness files through checkpoints so long sessions can Move

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

A Mjolnir session's checkpoint (a verified ZIP "recovery copy" written by `mj-checkpoint` and checked by the controller) carries the harness's own files, called native artifacts: for Claude Code, the transcript `projects/<slug>/<session-id>.jsonl` and its session subtree; for Codex, Kimi, Grok, Muse, OpenCode, Antigravity and DSH, their equivalents. Move, Suspend, Resume and recovery all depend on writing and restoring that checkpoint.

Today every native file is read whole into memory and two guessed caps stop large ones: `MAX_NATIVE_FILE` (1 GiB per file) and `MAX_NATIVE_TOTAL` (8 GiB) in `mj-checkpoint/src/checkpoint.rs`, plus `MAX_PAYLOAD_BYTES` (8 GiB) and `MAX_ARCHIVE_BYTES` (32 GiB) in `mj-checkpoint/src/archive.rs`. On 2026-11-14 a quota Move of the live session 07d292ca700ead3ba5c751211efb3228 failed on every attempt with "export target checkpoint failed with status 1: native artifact is too large" because its Claude transcript had grown to 1,089,679,186 bytes. The session cannot leave its exhausted profile. AGENTS.md forbids caps nobody measured; these exist only because of the in-memory design.

After this change a session whose native files are many GiB checkpoints, verifies, transfers and restores with memory bounded by a small buffer per file in flight, not by file size. To see it work: run the Hard-won test below, then retry Move on 07d292ca… after installation and watch it complete.


## Progress

- [x] (2026-11-14) Milestone 1: disk-backed native bodies in capture and pack; hashes and part plan computed incrementally; ZIP written without retaining every compressed body.
- [x] (2026-11-14) Milestone 2: restore streams native parts from the verified ZIP into securely created destination files.
- [x] (2026-11-14) Milestone 3: remove the native and payload/archive size caps; Hard-won test; full validation; commit.


- [x] (2026-11-14) Review fixes: verify whole omitted native digests before returning trusted state; selective repository/SessionWiki readers; conversion spool; shared parsed ZIP reader.
- [x] (2026-11-14) Validate all review fixes.
- [ ] Commit review fixes and complete independent review.

## Surprises & Discoveries

- (child investigation, 2026-11-14) `prepare_archive_view_with_part_size` in `mj-checkpoint/src/archive.rs` borrows full byte slices, hashes them, plans 16 MiB shards, and `write_zip` compresses every entry into `Vec` buffers concurrently, keeping all of them until assembly. Making only the source disk-backed would still leave export memory proportional to the session. Restore uses `read_archive_verified`, which loads every payload, then reassembles shards into one native `Vec`.
- `verify_archive_streaming` already verifies hashes without keeping bodies; the controller's Move path already uses it.


## Decision Log

- Decision: keep the stored checkpoint format byte-compatible (same manifest, part naming, 16 MiB shard plan, compression, ZIP/ZIP64 layout). Only change how bytes move. Rationale: checkpoints from the current release must restore, and other daemon/worker versions read these archives across upgrades; no protocol or schema change is then needed. Date: 2026-11-14.
- Decision: the child may change `mj_core::archive` types, including `NativeArtifact` (currently `{ relative_path, data: Vec<u8>, mode }`), to support a body that is either owned small bytes or a validated open file source read by range. This is the shared owner of the format, so the representation belongs there. Callers that construct small runtime-owned artifacts (goal state, memory) keep using owned bytes. Date: 2026-11-14.
- Decision: compression may stay parallel only if compressed output is spooled to a private temporary file in the staging directory rather than memory; otherwise compress sequentially in streaming fashion. Choose whichever is simpler to prove correct; record the choice here. Date: 2026-11-14.
- Decision: remove `MAX_NATIVE_FILE`, `MAX_NATIVE_TOTAL`, and `MAX_PAYLOAD_BYTES`/`MAX_ARCHIVE_BYTES` where they only guard memory. Keep `MAX_MANIFEST_BYTES` (the manifest is parsed in memory and is small by construction) and the ZIP64 threshold, which comes from `zip::ZIP64_BYTES_THR`. Any remaining limit must be a documented format limit with a comment naming its source. Date: 2026-11-14.
- Decision: SQLite native databases (`sqlite_native_database_snapshot`) may stay in memory if they are small by nature; state that in a comment. Kimi/DSH transformations on restore that rewrite content may keep an in-memory path only for those formats, with a comment; Claude transcripts need only path remapping and must copy directly. The Muse parser limit `MAX_NATIVE_FILE_BYTES` in `mj-checkpoint/src/native/muse.rs` is separate and stays unless tracing shows it on the Move path. Date: 2026-11-14.


## Outcomes & Retrospective

The first >1 GiB export/verify/restore regression passed in 12.67 seconds (before extending the same test to cover capture/pack too). The fixture is a sparse Claude-shaped file with distinct bytes at both ends; restored size and SHA-256 must match. The extended regression covering export plus capture/pack and restore passed in 20.88 seconds. Full validation completed; see final evidence below. Build and test logs are temporary text files `/tmp/native-large-final.log`, `/tmp/native-full-tests-final.log`, and `/tmp/native-clippy-complete.log`; final results will be transcribed here.



## Context and Orientation

Capture: `mj-checkpoint/src/checkpoint/native_scan.rs` (`collect_native_tree`, `collect_claude_memory_tree`) selects files under the harness home and currently `fs::read`s them into `NativeArtifact`. `mj-checkpoint/src/checkpoint/capture.rs` stages them and `pack_checkpoint` reads staged files back with `read_staged_file(..., MAX_NATIVE_FILE)`. The archive itself is built in `mj-checkpoint/src/archive.rs` (`prepare_archive_view_with_part_size`, `write_zip`). The controller runs the exporter as a subprocess, `hel worker export-checkpoint --spec` (built by `checkpoint_transfer::export_command`, invoked from `mj-controller/src/controller/checkpoint/latched.rs`), for both the legacy export and the capture/pack path; both must use the disk-backed writer. Transfer already moves files and passes verified gates. Restore: `mj-checkpoint/src/checkpoint/restore.rs`, `read_archive_verified`, `restored_native_artifact_bytes`, `write_private_file`; `NativeCheckpointState.restore` takes `&[u8]` for runtime-owned goal artifacts and needs only an adapter, not transcript bodies.

A "part" or "shard" is a 16 MiB slice of one payload stored as its own ZIP entry with its own hash, alongside a whole-payload hash in the manifest.


## Plan of Work

Milestone 1 introduces the body representation in `mj_core::archive`, makes native scan record validated paths and metadata instead of bytes, has staging copy files by streaming into the private stage, computes whole and per-part hashes incrementally while reading, and writes ZIP entries from range readers without retaining all compressed bodies. Existing archive golden or round-trip tests must still pass unchanged, which proves the format did not move.

Milestone 2 changes restore to verify first (existing streaming verifier), then reopen the ZIP and stream each native file's ordered parts into a securely created destination file, applying the path remapping on the path only.

Milestone 3 removes the caps per the Decision Log and adds the test.


## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir-wt/native-stream` (detached worktree). Targeted runs while iterating, for example:

    cargo test -p <mj-core package name> archive
    cargo test -p <mj-checkpoint package name> checkpoint

(look up package names in each crate's Cargo.toml; `mj-core` is `brokk-mj-core`). At the end, once: full suites of every touched crate and `cargo clippy --all-targets -- -D warnings`, dev profile, outside the sandbox.


## Validation and Acceptance

Add exactly one new test, marked directly above its attribute:

    // Hard-won: move-a618a54a: a 1 GiB native-file cap made a quota Move of a long Claude session fail

It creates a Claude-shaped native file larger than 1 GiB (sparse, so disk stays cheap), checkpoints and restores it through the same functions the Move path uses, and compares a hash of the restored file with the original. If it takes more than about a minute, record the time here and propose an alternative to the parent rather than shrinking it. Existing round-trip and compatibility tests must pass unchanged.


## Idempotence and Recovery

All work is in a detached worktree; no live store or daemon is touched. Tests use their own temporary directories.


## Interfaces and Dependencies

No new crates. `NativeArtifact` (or its replacement) must let callers construct owned-bytes bodies for small runtime state and file-backed bodies for scanned harness files; readers must be able to open a body as a reader over a byte range.

## Implementation notes (2026-11-14)

NativeArtifact now owns a NativeArtifactData enum: Bytes for generated runtime records and SQLite snapshots, File for native disk sources. Native file hashing reads one 16 MiB part buffer per Rayon worker; ZIP output compresses entries sequentially to bound memory without a compressed-body spool. Hashes are checked again as ZIP entries are written so source mutation cannot publish an inconsistent archive. Restore verifies first, retains canonical/repository and runtime-owned auxiliary records, then copies native entries through secure destination creation with whole-file digest verification. A single NativeRelocation decision identifies the specific Kimi/Grok/Muse/DSH records and Antigravity databases that require format-specific materialization, as authorized; the remaining files stream for every harness. Muse's parser cap remains separate; Claude Move never invokes it. Checkpoint transfer has no whole-archive fs::read path and already calls streaming verification.

No stored manifest, schema, protocol, part naming, compression method or ZIP64 threshold changed. Older readers retain their preexisting resource limits, but archives within those limits remain mutually readable. New readers remove memory-only native/payload/archive caps. Repository and canonical payload materialization remains outside the native-file memory improvement.

Mechanical fixes during development: adapt owned artifact constructors to enum conversion; adapt existing tests to read disk-backed artifacts; preserve Kimi's JSONL index selection while reading lines through BufReader.

Validation update: the extended regression was run directly from the completed Cargo test executable while the broader Cargo build finished. It passed with a 20.865-second measured body (20.88-second test runner total). Intermediate compilation fixes included qualifying std::path::Path in the new disk reader and adapting the relay attachment test to NativeArtifactData. One Clippy needless borrow was removed.

Final review refinement: native_artifact_relocation now owns the content-rewrite decision for both raw restore and provider rewriting. This prevents ordinary Kimi/Grok transcript files from being materialized just because other records from that provider need relocation. Restore also computes the verified archive hash through a streaming reader instead of leaving an empty metadata field.

## Final validation evidence

The final checkpoint suite passed 173 tests (2 ignored) in 19.04 seconds, with the final shared relocation decision. The earlier full touched-crate invocation passed checkpoint 173/2 in 23.02 seconds, controller 2642/27 in 202.45 seconds, and core 650/4 in 15.91 seconds. Worker completed 846 passing and 11 ignored, with two baseline failures: work_facts::tests::the_status_and_the_subject_reads_run_together_and_agree_on_the_commit reported that its rendezvous scripts did not run together; review::hook::tests::hook_accepts_an_overridden_tree_and_a_reinstall_replaces_the_old_script hung in a local git push. Git waited for receive-pack while receive-pack waited on Git's pipe; an older unrelated worker test process was already in the same state. This run's process group was terminated after diagnosing the deadlock, causing the hook assertion to report -1 rather than success. No working files were removed before process teardown.

Both failures passed when rerun individually from the completed worker test executable: work_facts in 0.05 seconds and the hook test in 0.31 seconds. They are outside the edited source paths and were not changed. Evidence: /tmp/native-full-tests-final.log (full run and both failure details), /tmp/native-workfacts-rerun.log, /tmp/native-hook-rerun.log. The final checkpoint-only suite is /tmp/native-checkpoint-final.log. Workspace cargo clippy --all-targets -- -D warnings passed on the dev profile in /tmp/native-clippy-last.log. The >1 GiB direct regression passed in 20.88 seconds in /tmp/native-large-final.log. All Cargo tests ran outside the sandbox with existing isolated temporary directories; no live instance was opened.

No source changes were needed for the baseline failures. Formatting and git diff --check passed for the changed files. All requested implementation work is complete; commit and independent review remain the final acceptance steps. Native body memory is bounded by one part buffer per hash worker and ZIP codec buffers; manifest/part metadata grows with the number of files and parts. SQLite snapshots, canonical/repository data, and actual provider records that require parsing still materialize as described above. No live Move retry or installation was performed by this child.

## Independent review round 1

All findings are accepted. The original streamed verifier checked individual parts but skipped the whole digest when it discarded them. This could let restore mutate repositories and relay state before discovering an invalid native digest, or accept it entirely when native restore was disabled. The verifier now streams omitted sharded payloads in order through the same size/digest checking copier before returning any trusted state. The existing corruption test now preserves valid native part hashes but corrupts the whole hash and asserts that both native-enabled and native-disabled restores leave native, repository, and relay targets unchanged.

Remaining production full-archive consumers were repository-only resume, SessionWiki indexing, and checkout conversion. Repository-only restore now retains only canonical/repository payloads. SessionWiki uses the canonical projection from streaming verification. Conversion verifies without retaining native payloads, streams them to a private temporary directory, and writes the replacement archive from disk sources; cleanup follows ownership of that directory. The large-file regression also checks that the repository-only reader retains no native body. The old public materializing reader remains for callers explicitly asking for all bytes, but no production consumer in this repository uses it.

CheckpointPayloadReader owns one parsed ZIP directory for an entire restore or conversion. Each payload copy reuses it. Whole-digest verification likewise reuses the verifier's parsed directory, including its cancellation-aware file reader. No finding was overridden; no format or protocol changed. Review-fix validation passed: the full checkpoint suite completed 173 passed / 2 ignored in 31.03 seconds, and controller completed 2642 passed / 27 ignored in 249.46 seconds (no failures). Workspace Clippy passed. The extended corruption case passed in 0.21 seconds, the conversion integration case in 0.96 seconds, and the >1 GiB regression including repository-only retention and whole-digest checks in 31.77 seconds. Logs: /tmp/native-review-full.log, /tmp/native-review-clippy.log, /tmp/native-review-targeted.log, /tmp/native-review-conversion-targeted.log, and /tmp/native-review-large.log. The earlier Cargo conversion filter matched no test names; the actual conversion integration case was then run by its exact name from the completed test executable. No sources changed in core or worker during this round, so their earlier full suites were not repeated.

Review-fix outcome: every independent finding was fixed. No finding needs an override or a parent design decision. The stored format is unchanged. Verification does an ordered streaming pass for discarded sharded payloads in addition to checking each part; the measured large-file regression remains below the one-minute threshold. Conversion uses private disk scratch space rather than native-file heap storage. Commit and review acceptance follow this recorded validation.

## Independent review round 2

The remaining finding was accepted: Muse import compared two NativeArtifact lists to detect concurrent source changes, but file-backed bodies only compared their paths. Muse import now copies its initial native collection to a private disk snapshot, compares that captured content against the second collection with bounded buffered readers, and keeps the snapshot alive until the archive is published. Later harness writes cannot change the archived bytes that were paired with the preview. Other harness imports and the checkpoint format are unchanged.

The existing changed-preview/cancellation integration test now also changes the live Muse log at the WritingArchive progress callback, then verifies that the archive preserves the original validated bytes. No new test function or cap was added. Only controller import implementation and its existing integration test changed in this round; targeted validation and workspace Clippy passed. The final full controller suite completed 2641 passed / 27 ignored with one unchanged baseline test failure, daemon::tests::startup_delivery_reservation_wait_does_not_prevent_move_cancellation. It observed the refusal receipt before the late draft was visible and failed the assertion at daemon/tests.rs:11712; the same test passed alone in 0.67 seconds. The expanded Muse import test passed in that full run. Logs: /tmp/native-muse-targeted.log (targeted import test, 0.59 seconds), /tmp/native-muse-full.log (231.94-second full run and baseline failure), /tmp/native-muse-baseline-rerun.log (isolated pass), and /tmp/native-muse-clippy-final.log (clean final workspace Clippy). The publication-race case uses a log larger than 128 KiB, crossing the 64 KiB pipe/buffer boundary required by the repository guidelines.

Round 2 outcome: the Muse capture is owned by a private TempDir until import returns; every archive body points to that capture rather than the mutable source. Content comparison uses buffered readers and preserves modes and selected paths. The existing import test covers stale previews, cancellation, and a late source write without adding a new test function. No additional format, schema, protocol or core/worker changes were needed. All review findings are fixed; the final commit is ready for independent review. Baseline daemon draft propagation was not changed.

## Independent review round 3

The remaining finding was accepted: the capture-to-publication gap affected every native import, not only Muse's explicit stability check. The private disk snapshot is now created for all imported native artifacts at the collection point. Each import owns that snapshot through archive publication, preserving the old byte-vector capture semantics while keeping memory bounded. Muse retains its additional preview/content validation. Cancellation is checked before copying each artifact, and snapshot cleanup remains scoped to import completion or failure.

The existing Codex canonical import integration test now carries a native log larger than 128 KiB, appends harness output at WritingArchive, and checks that the archived native bytes remain exactly the original capture while the source has grown. No new test function or stored contract was added. Only controller import implementation and an existing Codex integration test changed; targeted validation, the controller suite and workspace Clippy passed. The targeted Codex case completed in 0.52 seconds; the full controller suite completed 2642 passed / 27 ignored / no failures in 250.95 seconds. Logs: /tmp/native-import-all-targeted.log, /tmp/native-import-all-full.log, and /tmp/native-import-all-clippy.log. The earlier daemon cancellation baseline also passed in this full run. No additional source changes were needed after these results.

Round 3 outcome: all native import bodies are now owned snapshots, closing the capture-to-publication race for every harness. The ownership change is in the importer, where the canonical/native pairing is decided; no equality implementation performs hidden I/O. Native file buffers remain bounded, caps remain removed, and the stored format is unchanged. Every independent finding has been fixed without overrides. The final implementation commit and review request follow this completed validation; review verdicts are recorded by the review service and final parent handback.
