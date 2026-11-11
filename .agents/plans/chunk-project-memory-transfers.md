# Transfer project memory across bounded relay messages

This ExecPlan is maintained according to `.agents/PLANS.md`. Its progress and decisions describe issue #1500.

## Purpose / Big Picture

A session must be able to synchronize project memory and Move even when its memory tree exceeds the relay's 8 MiB message limit. The reported session `d1af5b48898e8d2a9f970b26631bb81a` fails before Move because its worker tries to return both memory trees in one message. Split memory transfers across messages while preserving a consistent snapshot and complete-tree replacement, then push and install the fix as explicitly requested by the user.

The earlier auto-Move, reconnect, retry classification, and daemon result-publication changes are already pushed and installed as d73304e73. This plan is a separate second delivery. Do not alter source files or Git HEAD during either installer build.

## Progress

- [x] (2026-11-11) Claimed issue #1500 and diagnosed the live worker without modifying its session or memory.
- [x] (2026-11-11) Verified the failure is an oversized relay response rather than disk exhaustion, OOM, or permissions.
- [x] (2026-11-11 16:27Z) Completed the first installation and verified the new live daemon before starting this implementation.
- [x] (2026-11-11 16:30Z) Assigned disjoint core/worker and controller implementation slices, plus independent compatibility and upgrade investigation.
- [x] (2026-11-11) Defined protocol 59 action/result types and completed the controller implementation, including paged additive installation and conditional replacement.
- [x] (2026-11-11) Implemented protocol-gated, connection-owned snapshot download and complete-tree upload, with original additive/CAS semantics.
- [x] (2026-11-11) Controller focused real-pipe compatibility and oversized-transfer tests passed: 2 tests, 12.20 seconds runtime.
- [x] (2026-11-11) Worker real-socket oversized transfer, old-protocol compatibility, and disconnected-upload regressions passed.
- [x] (2026-11-11) Reviewed the integrated implementation and independently audited connection ownership, commit/CAS, launch compatibility, and upgrade admission.
- [x] (2026-11-11) Completed the full touched-crate development tests once, repaired/reran only failures, and passed final workspace clippy, formatting, and diff checks.
- [ ] Commit, complete push review, merge upstream if needed, and push origin/master.
- [ ] Freeze source and Git HEAD, install the second build, hand off the daemon, and verify worker upgrade and oversized-memory transport.

## Surprises & Discoveries

The worker on target morannon remains alive after the controller sees EOF. Its own log repeatedly reports `relay response frame is too large`, at the same times as ProjectMemorySnapshot disconnects. Its baseline and replica directories each occupy about 7.9 million bytes; the encoded pair exceeds the 8 MiB frame budget. Existing protocol 44 gzip compression is already enabled, so adding compression alone cannot resolve this defect.

The affected worker is in an ssh-podman container on morannon, not the EC2 host whose root volume was enlarged. Its filesystem has ample space and the container is not OOM-killed. Do not restart or destroy this session as a substitute for correcting transport.

Ordinary worker upgrade reserves the idle worker and replaces its binary without first synchronizing project memory or making a checkpoint archive. Independent inspection confirms `upgrade_session_worker` stages the binary, takes idle reservation and swap admission, swaps/reconnects, and then only sets the memory target. Existing isolated tests exercise the reservation/verification portion, not the full swap; extending those tests with a legacy-58 peer that rejects all memory operations proves only that portion. Keep that verification limit explicit.

The legacy snapshot path treats InvalidState as an absent memory endpoint and disables synchronization. New transfer-state errors must not take that fallback. Independent inspection found no managed protocol-59 mismatch requiring it: `launch.rs::project_memory_sync_target` and `WorkerLaunchConfig.project_memory` use the same session.no_project_memory flag; normal worker replacement regenerates and installs the current launch config rather than carrying an omitted legacy field forward. Thus the controller retains that suppression only for older protocols. Discovery workers with no memory endpoint have no managed memory target either.

## Decision Log

Decision: Keep the existing relay frame limit and add protocol-gated chunked memory transfer. Rationale: increasing a global cap merely postpones failure and affects unrelated messages; project-memory size has no corresponding product limit. Date: 2026-11-11.

Decision: Capture baseline and replica once under the existing serialized project-memory endpoint, retaining immutable encoded bytes for that connection's download. Rationale: independent reads for successive pages could mix different memory versions. Date: 2026-11-11.

Decision: Buffer a complete upload in connection-owned state, decode it only when committed, and reuse the existing compare-and-replace operation. Rationale: a failed transfer must not install half a tree, lose deletions, or overwrite a replica changed by the harness. Dropping the connection releases its transfer state without a new registry or cleanup journal. Date: 2026-11-11.

Decision: Chunk serialized bytes rather than files, and derive chunk length from the actual serialized envelope and base64 expansion. Rationale: an individual file can exceed the frame limit; a guessed chunk cap does not state the actual protocol boundary. Include the newline in the frame budget. Date: 2026-11-11.

Decision: Retain the existing protocol 58 whole-message operations only as negotiated compatibility for older workers. New workers use the new operation; errors on it propagate rather than falling back silently. Rationale: workers continue across daemon upgrades, and idle worker replacement can supply the new capability without discarding their state. Date: 2026-11-11.

Decision: Preserve the existing rule that a worker serves only its current protocol. Remove an implementation-added exception that let a protocol-59 worker serve all supported protocol-58 requests. Rationale: maintaining old memory method shapes does not authorize a global relaxation of mutation/version admission. The controller uses legacy memory methods when it negotiates with an actual older worker; the new worker can still serve those method shapes at its current protocol. The existing refusal regression caught this unnecessary exception during full validation. Date: 2026-11-11.

Decision: Page additive installation as well as conditional replacement, preserving both existing operations exactly. Rationale: leaving the exposed additive-install method as a whole-frame upload on protocol 59 retains the same defect through another entry point, even though its current production callers are old-worker compatibility paths. Do not introduce client-side merge or conditional-replacement semantics to emulate an additive install. Date: 2026-11-11.

Decision: A controller transfer guard borrows the existing RelayClient and invalidates that connection through its existing abandoned/input ownership if an unfinished transfer future is dropped. Recoverable protocol errors explicitly abort and report abort failures. Rationale: cancellation must release worker buffers without a second independent connection-state flag or an asynchronously dropped cleanup task. Date: 2026-11-11.

## Outcomes & Retrospective

Implementation and validation are complete; push and the authorized second installation remain. No new database schema or persistent relay state is needed. The defect concerns transport size, so retaining one encoded transfer in memory is consistent with the existing complete-tree merge architecture. This plan does not redesign merge computation or add a speculative memory-size limit.

Controller implementation is complete and stable. `project_memory_wire_format_tracks_worker_protocol_and_decodes_both_forms` and `project_memory_transfer_pages_large_files_and_preserves_cas_and_cleanup` passed with consistent core/controller/worker package selection. The independent Python peer exercises protocols 43/44/58, protocol-59 paged reads and both upload kinds, a poorly compressible 12 MiB single file whose serialized snapshot and upload exceed 8 MiB, frame length checks, stale CAS, deletions, explicit abort/retry, abort failure, and future cancellation. Evidence: `.mj/agents/1cbd92b1e462d4831611d7744363a0df/memory-transport-test2.log`. Initial compilation caught an in-progress worker exhaustive match and unused argument; both were repaired before the passing run.

The broader affected-path run initially passed 15 controller tests and failed one older fixture that advertised the new current protocol without implementing its memory endpoint. The existing `relay_attach_does_not_probe_or_install_project_memory` now tests isolated protocol-58 and protocol-59 peers: neither ordinary attach nor polling touches memory; explicit sync disables an unavailable legacy endpoint but surfaces a new transfer error and retains the target. The sole failing test rerun passed, both branches, in 0.46 seconds. Evidence: `memory-attach-fixture.log` in the same controller report directory. No production workaround was added.

Worker socket tests passed: `project_memory_transfer_pages_large_trees_and_commits_complete_uploads` (8.28 seconds), `protocol_59_worker_keeps_legacy_project_memory_methods_at_protocol_58`, and `project_memory_partial_upload_is_discarded_with_its_connection`. They cover a frozen pair larger than a frame, an individually oversized upload, both upload semantics, live commit-time CAS, deletions, interleaved connections, a long escaped response request ID, explicit abort, disconnect, and legacy feature refusal. Logs: `.mj/agents/f43916b1abbe4f018832fba19888038f/project-memory-transfer-tests.log`, `project-memory-protocol58-tests.log`, and `project-memory-disconnect-tests.log`. Final validation uses the unchanged package graph and TZ=UTC in `.mj/agents/4a58f6d94bc675fcf4acd5f24798cfc1/stage2-tests.log`.

The full touched-crate run completed once. Controller initially passed 2525 tests, failed 21, and ignored 19: all failures share the latch test peer advertising current protocol without a memory endpoint. The repair advertises the protocol that fake actually implements, 58, using its existing internal request/current-version translation and response/wire-version restoration. The controller owner reruns only the 21 failures and the changed attach fixture. Core passed 644 unit tests and 3 scenarios, with 3 unit tests ignored. Worker initially passed 816 library tests, failed the one strict-version refusal regression, and ignored 11; all 5 main tests and 24 integration tests passed. Documentation tests completed with one ignored core test. Workspace clippy found one `int_plus_one` assertion in the new socket helper, which is being simplified. The strict-version exception is being removed, and the original-method compatibility socket test is being revised to use the current envelope rather than claiming old-controller support. Only failures and affected protocol/memory tests will rerun; do not repeat entire suites.

Final validation resolved every failure. The 21 controller failures plus the changed attach test passed as 22 exact filters in 6.02 seconds (`fixture-failed-rerun.log` in the controller report directory). The unchanged old-worker-version refusal test passed after restoring the original production rule; the large-transfer, original-method/current-envelope compatibility, and disconnected-upload socket tests passed again against the final canonical clone-based handler. Logs in the worker owner's report directory are `protocol-current-only.log`, `project-memory-transfer-final.log`, `project-memory-legacy-shape-final.log`, and `project-memory-disconnect-final.log`. The complete results account for 2546 controller unit passes, 644 core unit passes plus 3 scenarios, 817 worker library passes, 5 worker main passes, and 24 worker integration passes, with the documented ignored cases retained.

An unrequested late no-clone experiment was reverted after the original worker test executable had been built. The final focused worker reruns above revalidated the canonical restored handler; no experiment remains in the source. Delegated restarts later failed or remained queued on node connections, so the parent ran final workspace clippy directly after all source edits were frozen. It exited 0; log `.mj/agents/4a58f6d94bc675fcf4acd5f24798cfc1/stage2-final-clippy-parent.log`. Final `cargo fmt --all -- --check` and `git diff --check` also exited 0. No full suite was repeated to confirm a fixture or assertion repair.

## Context and Orientation

The daemon is the control service that synchronizes a canonical project-memory tree with a worker's private replica. The replica is the memory files the coding harness can edit. A baseline records what the replica was seeded from; comparing baseline, replica, and canonical trees allows additions and deletions to merge. A tree version identifies complete contents and is used for compare-and-replace, meaning replacement proceeds only if the replica still matches the version the controller read.

`mj-core/src/relay/protocol.rs` defines newline-delimited JSON requests and responses. Its `ProjectMemoryWireTree` supports plain and gzip-encoded trees. `MAX_FRAME_BYTES` in `mj-core/src/relay.rs` bounds each message; locate the current protocol version and capability constants in that same relay module. `mj-controller/src/worker_client/relay.rs` exposes snapshot, install, and conditional replacement methods. `mj-controller/src/session_manager/standalone.rs::sync_project_memory` and the project-memory merge module coordinate canonical merging and replacement.

`mj-worker/src/worker_runtime/unix.rs` accepts connections and owns connection lifetime. Its existing `ProjectMemoryEndpoint` serializes filesystem work. `mj-worker/src/worker_runtime/unix/requests.rs::apply_project_memory_request` reads and replaces the replica and baseline. `mj-worker/src/worker_runtime/relay_tests.rs` contains real worker relay fixtures. `mj-controller/src/controller/worker_restart.rs` performs normal idle worker replacement. Existing worker and daemon upgrade regressions must remain isolated from live data.

## Plan of Work

First extend the core request and response types with one project-memory transfer operation and a negotiated capability version after the current protocol 58. Define actions for beginning a snapshot, reading its bytes, beginning an upload, appending bytes, committing, and aborting. A download represents both baseline and replica captured together. An upload distinguishes unconditional installation from conditional replacement with an expected replica version. Keep all content connection-only: do not add it to relay events, snapshots of operational state, or the database.

Add a worker connection state with mutually exclusive idle, downloading, and uploading cases. The downloading case owns one immutable serialized payload, identity, and offset. The uploading case owns identity, expected length, next offset, replacement condition, and received bytes. Reject wrong identities or offsets. A commit verifies completion and decodes before asking the existing memory owner to apply replacement. The owner performs the version comparison and complete-tree installation together. Abort and disconnect discard the state. Shared encode/decode and chunk-sizing helpers belong with the wire types, avoiding separate interpretations in controller and worker.

Next change the controller memory methods to use the chunk operation when the negotiated version supports it. Keep their public semantic results unchanged so canonical reconciliation and its retry rules remain the owners of merge policy. Perform compression, decompression, and large serialization on blocking threads, as the current code requires. Propagate transport and commit errors with useful context. Do not send a new operation to an older worker.

Finally exercise both directions over actual relay framing with more than 8 MiB of encoded, poorly compressible data, including one file larger than a page. Check coherent reads, conditional replacement after concurrent replica edits, deletion propagation, interruption before commit, and compatibility negotiation. Review the complete source diff, then run final validation once. Push the reviewed result, freeze source and Git state, perform normal installation, and activate the installed daemon. Verify ordinary idle worker upgrade supplies the new protocol and the reported session can obtain an oversized snapshot without changing its memory contents.

## Concrete Steps

Run all commands from the repository root using normal mbx/Cargo configuration. Every cargo test runs elevated, because the real relay fixtures use sockets. Never redirect Cargo build outputs. Use the existing isolated test fixtures or a named `--instance memory-transfer-1500` for binary-based tests; never point a test build at the default instance.

    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker <focused-filter>
    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Retain the same package selection throughout focused and final validation to avoid unrelated Cargo feature-graph recompilation. Capture command output in the responsible child's report directory. Fix failures and rerun only the failed tests. Update this plan with actual test names, evidence, and counts before final delivery.

After committing only owned files, request the configured push review. Resolve or explicitly decline findings with evidence before pushing HEAD to origin/master. Merge upstream into the current branch if required; do not rebase or create a branch. After push, run normal `scripts/install.sh` elevated, with no source edits or commits until all builds exit. Invoke an ordinary installed `mj` command to trigger daemon handoff, then `mj daemon status` must report the new exact source revision and `This daemon runs this build.`

## Validation and Acceptance

A real controller and worker connection must download a baseline/replica pair whose encoded combined payload exceeds the unchanged 8 MiB limit, with exact contents and versions preserved. The controller must upload an equally oversized replacement, including a deletion, without exceeding any frame. A concurrent replica edit must reject conditional commit and leave the edited tree intact. A disconnected or incomplete upload must leave both replica and baseline unchanged. Snapshot pages must continue to represent the same captured pair even if the files subsequently change. The new controller's protocol-58 negotiation and legacy requests to an older worker remain valid; a current worker retains the original memory methods at its current version. Protocol 58 does not advertise the new transfer action, and the worker's strict current-version admission rule remains intact.

The normal isolated upgrade path must reach the new worker without invoking a memory snapshot first. Final core, controller, and worker development tests and workspace clippy must pass, with any preexisting or platform-specific limitations recorded. After installation, verify the live daemon identity and an appropriately idle source's new negotiated worker protocol. A read-only memory snapshot is acceptable live verification; do not publish memory contents into logs or modify the user's memory to construct a test.

## Idempotence and Recovery

Reads are immutable snapshots; aborted downloads have no filesystem side effects. Upload append operations validate identity and offset, and incomplete data is never installed. A lost connection discards its upload; restarting the controller sync reads fresh versions and follows the existing merge policy. Do not replay an unacknowledged mutation as if its commit were known not to have occurred; preserve existing replica-version checks. No migration or manual reset is required. Automatic worker upgrade must preserve active work and wait for idle admission rather than interrupting a busy harness.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1500. The decisive worker log is `/var/lib/hel/workers/d1af5b48898e8d2a9f970b26631bb81a/worker.log` inside morannon container `mj-d1af5b48898e-200fc7`. Errors include the ProjectMemorySnapshot EOF at 15:38:25 UTC and corresponding `relay response frame is too large` warning. The earlier first-install evidence lives in `.mj/agents/4a58f6d94bc675fcf4acd5f24798cfc1/first-install-d73304e73.log` and `first-install-metadata.log`.

## Interfaces and Dependencies

Reuse serde_json, gzip, base64, the current frame serializer, the existing ProjectMemoryEndpoint filesystem owner, `ProjectMemorySnapshot`, `TreeVersion`, and `ReplicaReplaceOutcome`. Add no dependencies. `RELAY_PROJECT_MEMORY_TRANSFER_PROTOCOL` is 59; the relay minimum remains unchanged. Gate the new method consistently with existing connection-only requests.

`RelayRequest::ProjectMemoryTransfer { action: ProjectMemoryTransferAction }` carries BeginSnapshot, ReadSnapshotPage with transfer_id and offset, BeginUpload with kind and total_bytes, AppendUpload with transfer_id/offset/base64 chunk, CommitUpload, or Abort. `ProjectMemoryUploadKind` is InstallSnapshot or ReplaceTree with expected_replica. `RelayResponsePayload::ProjectMemoryTransfer { result: ProjectMemoryTransferResult }` returns SnapshotStarted with transfer_id/total_bytes, SnapshotPage with identity/offset/next_offset/complete/chunk, UploadStarted, UploadProgress, Installed, Replaced with ReplicaReplaceOutcome, or Aborted. Transfer identities and offsets are u64.

`ProjectMemoryWireSnapshot::encode` serializes the frozen baseline/replica pair using the existing compressed-tree representation; `decode` reconstructs both snapshots. Core chunk-capacity helpers take the request ID and account for serialized envelope overhead, base64 expansion, and the newline. Download response capacity uses the actual request ID, including JSON escaping, because an independent peer need not use the controller's canonical ID format. Upload request capacity uses the maximum width of the controller's own generated ID, obtained through `RelayClient::request_id_for` in `worker_client/exchange.rs`, the same formatter used for actual requests. Core does not independently define the controller's ID shape.

Revision note (2026-11-11): Created the second-delivery plan after the authorized first installation completed, recording proven size failure, disjoint ownership, transport design, compatibility constraints, and observable acceptance.

Revision note (2026-11-11): Recorded the concrete wire contract, additive-install coverage, controller cancellation ownership, independent upgrade evidence and its test limitation, and actual response-ID frame accounting found during integrated review.

Revision note (2026-11-11): Recorded the verified launch-configuration invariant behind legacy-only absence handling, and centralized request-ID formatting after review found a duplicated producer format in core sizing.

Revision note (2026-11-11): Recorded the once-only full validation results, shared fake-worker repairs, and removal of an unnecessary global protocol exception exposed by the existing worker-version invariant.
