# Support ten optimized photo attachments

## Purpose / Big Picture

People can attach ten photographs in terminal, desktop, or browser chat without exceeding the one MiB command limit. Each photograph is optimized to at most 700 KiB, stored separately, and delivered to the agent as an ordinary ACP image. ACP is the protocol connecting the worker to the coding agent. The relay is the durable command queue between controller and worker.

This living plan follows `.agents/PLANS.md`.

## Progress

- [x] Grounded current transport, composer, snapshot, and checkpoint paths; user approved separate attachment storage and pushing after completion.
- [x] Implement shared bounded optimization and content-addressed storage.
- [x] Implement transfer, admission, ACP resolution, and archive recovery; finish review fixes for cache repair and retry deduplication.
- [x] Connect shared terminal/desktop chat and browser attachment flows; native library tests (417) and browser attachment tests (4) pass.
- [ ] Validate integrated behavior, commit on current branch, and push upstream.

## Surprises & Discoveries

Public operational state already contains prompt summaries. Private snapshots retain three copies of a queued command, making an inline ten-image prompt exceed the existing 16 MiB snapshot budget. The web currently retains attachments only in the open browser, while native drafts contain inline bytes. The baseline worker negotiated versions 1 through 7; this implementation adds version 8.

Native UI and controller daemon share the local session filesystem, so native background tasks install directly into the controller store without a daemon upload RPC. Remote workers use per-image relay RPCs. User-message echoes are filtered at the ACP observation boundary to keep base64 out of replay.

Full desktop workspace checking needs GTK/WebKit development libraries absent on this host; default workspace checks cover shared chat. Upstream advanced to v2.1.4 with TUI refresh and session-first web resume changes during implementation; integrate on master before final validation.

## Decision Log

The user chose ten images, automatic optimization to 700 KiB, all chat surfaces, and separate storage. Keep frame budgets unchanged. Image references are internal image blocks carrying a reserved `mj-attachment:` URI and empty data; ordinary inline ACP images remain readable. Only the worker dispatch boundary resolves these references to base64. This preserves existing prompt ordering and legacy serialized blocks without replacing every ACP content consumer.

Retain optimized blobs for the lifetime of their session. An installed blob is immutable and verified by SHA-256; incomplete uploads are temporary files. No eager rewrite of historical journals is allowed. Require protocol 8 for attachment references and uploads.

Use archive schema 4 for attachment artifacts and snapshot format 4 so old readers refuse incompatible storage. Restore blobs even for cross-harness moves. Installation and deletion share a cross-process lock and persistent deletion marker, preventing late tasks from recreating deleted sessions.

Missing image caches must not block text history projection: log failures while admission and dispatch still require verified bytes. Existing move recovery retains the destination when queue admission fails; explicitly configured old workers require upgrading before retrying image queue admission. Normal worker acquisition tracks the controller release.

## Outcomes & Retrospective

Implemented the three functional milestones. Ten 700 KiB images reach both ordinary ACP prompts and steering through a 64 KiB duplex pipe, with aggregate messages exceeding 9 MiB. Worker socket tests demonstrate protocol gating, idempotent blob upload, and no journal bytes for transfers. Optimizer tests cover five image behaviors; checkpoint tests cover ten-image roundtrip and missing/corrupt data. Native chat has 417 passing tests, server has 80, browser unit tests have 15, and focused browser attachment tests have four. Full integration checks and upstream integration remain in progress.

## Context and Orientation

`src/hel_worker/snapshot.rs` defines commands and durable state; `src/hel_worker/protocol.rs` defines connection operations. `mj-worker/src/hel_worker_runtime/unix.rs` dispatches commands and serves connections. `mj-controller/src/hel_worker_client.rs` is the remote client. Native chat uses `mj-chat/src/hel_chat/attachments.rs` and `hel_clipboard.rs`; browser chat uses `mj-controller/src/web/viewer.js` and `hel_server.rs`. `src/hel_checkpoint.rs` collects and restores portable native artifacts.

## Plan of Work

### Milestone 1: bounded images and immutable storage

Add `src/hel_attachment.rs` for references, bounds, controller/worker stores, atomic verified writes, and resolution helpers. Add `mj-controller/src/hel_image.rs` for JPEG/PNG/WebP optimization, orientation, transparent PNG, JPEG quality 90/85/80 followed by downscaling. Bound sources to 64 MiB and decoded allocations to 256 MiB. Use existing image 0.25 dependency with only needed codecs. Focused tests must demonstrate bounded real image outputs and corrupt input rejection.

### Milestone 2: reliable delivery and recovery

Add presence, upload, and read connection operations without journaling the bytes. Upload one optimized image at a time, check digest and metadata, reuse existing files, and submit only when all references exist. New reference commands require protocol 8. Resolve images in background immediately before ACP dispatch, reporting missing/corrupt blobs rather than dropping images. Store only references in new commands and native drafts. Include session blobs in checkpoint/export/import/move artifacts and validate restored data. Old inline prompts continue to load.

### Milestone 3: chat surfaces

Normalize clipboard and file inputs off UI loops. Shared native chat gains `/attach <path>`; web retains multiple file picking and paste. Surface processing/error state immediately, cap retained images at ten, preserve ordering and drafts on failure, ignore late canceled results. Limit processing to two operations per client. Browser attachments keep their existing nonpersistent draft semantics and explanation.

### Milestone 4: verification and publication

Test ten near-limit images reaching fake ACP intact, reconnect and restart, missing uploads and duplicate retries, queue editing, and checkpoint/move recovery. Check frames and snapshots remain bounded. Run browser tests and required Rust checks from `/home/jonathan/Projects/hel`:

    cargo test
    cargo clippy --all-targets -- -D warnings

Every cargo test must run with elevated permissions because tests use sockets. Build outputs stay in normal configured storage. Review staged diffs, commit only task-owned changes on the current branch, and push to upstream as authorized.

## Validation and Acceptance

Ten photographs each optimized to at most 716800 bytes must reach a fake ACP agent in selection order. The eleventh is refused without losing existing attachments. New prompt commands, replay pages, and private snapshots must not contain base64 image bytes. A restart, queued edit, or move must retain every referenced image. Cancellation and processing in another session must remain responsive.

## Idempotence and Recovery

Digest-addressed writes are idempotent. Publish files atomically only after verification. Failed submission preserves its command ID and draft. Keep legacy readers and reject unsupported worker versions with an upgrade instruction. Never delete owning-process files to stop work.

## Interfaces and Dependencies

Core attachment references carry digest, encoded size, MIME type and dimensions; public store helpers use Path/PathBuf. Controller image optimization returns bytes, MIME type, width, and height. UI handlers use supervised tasks; neither filesystem nor image codecs run on render loops. Agent assignments own disjoint files and report APIs before integration.

## Artifacts and Notes

Initial plan recorded from the approved conversation; implementation evidence will be appended as milestones complete.

Revision (2026-09-07): Recorded implemented behavior, focused validation, shared native storage, storage version gates, deletion coordination, and upstream integration requirements.
