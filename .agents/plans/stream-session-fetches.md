# Stream session fetches and retry across daemon replacement

This plan follows `.agents/PLANS.md` and must remain current as implementation proceeds.

## Purpose / Big Picture


A user can fetch a large file or directory without allocating its full payload in the worker, daemon, receiving worker, or CLI. A daemon restart cancels its temporary work promptly and the accepted MCP request runs again under the next daemon. The CLI automatically reconnects and retries its read after daemon replacement. Destination files appear only when complete. Automatic retry from the beginning is sufficient; byte-range resume is not required.

## Progress


- [x] (2026-11-15) Trace source framing, shared process execution, API transport, destination storage, and durable delegation replay.
- [x] (2026-11-15) Replace fetch payload vectors with anonymous temporary files and independent-position readers.
- [x] (2026-11-15) Stream worker stdout to disk through the existing supervised executor and stream disk input into destination storage.
- [x] (2026-11-15) Stream HTTP response bodies and CLI downloads; cancel preparation and response delivery on daemon shutdown.
- [x] (2026-11-15) Real CLI and MCP 64 MiB transfers retry after daemon restart; source/receiver peaks 20,443,136 / 20,004,864 bytes; restart 2.77 seconds. Existing archive, HTTP stream, and subprocess cancellation regressions passed. Post-fix checks are recorded below.
- [x] (2026-11-15) Initial full validation and restart acceptance passed; committed and merged upstream.
- [x] (2026-11-15) Review fixes passed focused fetch/pipe tests, real restart plus staging cleanup, full affected-crate suites with the isolated mailbox timing rerun, formatting, and workspace Clippy. Prepare the reviewed commits for publication.

## Surprises & Discoveries


The MCP delegation coordinator already excludes Fetch from handoff admission (`mj-controller/src/daemon/delegation.rs`). Its prepared request and completed result are persisted independently; unfinished requests are replayed after restart. Its jobs are aborted on shutdown, but existing blocking fetch subprocesses do not receive cancellation when the async future is dropped. The old transfer wire format is one JSON metadata line followed by payload bytes and needs no protocol change. `ArchiveFile` in `mj-checkpoint/src/archive.rs` already reads shared file handles at explicit offsets, avoiding shared-cursor races between cloned handles.

## Decision Log


- Decision: use anonymous disk-backed temporary files and compatible metadata-line framing, with an optional length field.
  Rationale: payload size no longer determines process RAM, handles clean up anonymous spools on process termination, and older workers can still read the wire header.
  Date/Author: 2026-11-15, Codex.

- Decision: extend the existing process supervisor and ArchiveFile reader rather than create separate child-process or shared-cursor implementations.
  Rationale: SSH refusal/start-marker behavior and process-group cleanup remain centralized; each reader owns an explicit offset into an immutable file slice.
  Date/Author: 2026-11-15, Codex.

- Decision: cancellation belongs to the transfer future; durable delegation remains the one owner of replay after restart.
  Rationale: dropping the future cancels its subprocesses, no transfer holds daemon admission, and no new transfer database or schema is needed. A retry from the beginning meets the user's requirement.
  Date/Author: 2026-11-15, Codex.

- Decision: reconnect after a transport interruption, then retry only for a new daemon identity or an explicit safe-to-retry shutdown response. Leave errors from the same healthy daemon, storage errors, and unrelated HTTP refusals visible.
  Rationale: checking identity before a fresh API probe races HTTP shutdown while the old control socket still answers. API admission now refuses fresh probes once shutdown starts; comparing identity only after a successful reconnect therefore distinguishes a replacement from a persistent body failure without a guessed retry limit. The real manual-restart test also showed that the old generic 503 needed an explicit retry code.
  Date/Author: 2026-11-15, Codex.


## Context and Orientation


`mj-checkpoint/src/archive/fetch.rs` owns path containment, directory ZIP generation, metadata framing, and atomic destination publication. `mj-worker/src/main.rs` exposes these as `worker fetch` and `worker store-fetch`. `mj-core/src/targets.rs` runs local, SSH, and container commands with process-group cleanup, parallel pipe draining, SSH refusal retry, and cancellation. `mj-controller/src/server_runtime/api.rs` invokes those commands; `mj-controller/src/server/api/files.rs` serves the result over HTTP. `mj-cli/src/api_client.rs` downloads it and `mj-cli/src/api_commands.rs` publishes a local file or stdout. A spool is an anonymous temporary file used to pass payloads between these stages without keeping them in RAM. The daemon owns no irreplaceable transfer state: durable delegation records plus source workers suffice to repeat an interrupted transfer.

## Plan of Work


First make FetchedPath own a shared anonymous file on the instance data filesystem rather than bytes. Keep internal spools under `MJ_DATA_DIR/fetch-spool`; final user copies retain their documented temporary-directory location. Its readers use the existing ArchiveFile implementation with independent offsets. Add streaming metadata read/write methods and atomic streamed destination writes that verify the declared length before rename. Archive directly into a seekable temporary file with ZIP64 and preserve symlink containment and ordering. Update existing fixtures to construct spools from readers.

Then add a file-output entry point to CancellableProcessExecutor. Keep the current process supervisor and collector, including SSH start-marker stripping and error propagation. Every retry truncates and resets its output file before the attempt; only explicitly refused SSH sessions are retried. In the controller, keep source subprocess stdout on disk, decode the metadata line, and stream remaining content into a payload spool. Run blocking I/O off async worker threads. Destination subprocess input uses execute_with_stdin with the payload reader. Drop guards cancel all transfer subprocesses when their owning future is aborted. Do not acquire daemon admission for transfers.

Serve payload spools as HTTP body chunks, including Content-Length so interrupted downloads are detectable. Race source preparation and body delivery against server shutdown. The CLI downloads chunks to a temporary file and publishes only after successful EOF; on an explicit shutdown refusal or a transport error, reconnect with backoff. Repeat the GET only after an explicit refusal or a changed daemon identity; the reconnected API must have passed a fresh probe that shutdown admission refuses. Stdout is written only after a complete download, so retry cannot duplicate a partial stdout prefix.

## Milestones


The first milestone is disk-backed worker transfer. A worker subprocess produces and receives an artifact larger than the old 16 MiB cap, with framing and truncated-input behavior preserved; the whole payload must not occur in a production Vec. The second milestone connects the daemon and CLI to those streams, keeps shutdown responsive, and demonstrates retry after replacement. The third milestone validates the integrated change and publishes the reviewed commits.

## Concrete Steps


From the repository root, use normal mbx-managed Cargo storage. Run focused checks outside the restricted sandbox, then the full touched-crate suites and workspace Clippy once after the last changes. Use package names `brokk-mj-core`, `brokk-mj-checkpoint`, `brokk-mj-controller`, `brokk-mj-worker`, and `brokk-mjolnir`. Run every executable invocation in an isolated named instance or the tests' existing isolated directories. Never use the live default store or daemon. Run `cargo fmt --all -- --check` and `git diff --check` before committing. Record exact successful commands and results here.

## Validation and Acceptance


Extend the real worker subprocess regression to stream a file beyond pipe capacity and 16 MiB, preserve bytes, and leave an old destination unchanged after truncation. Test executor file output against real child processes with output larger than 64 KiB, SSH refusal/start-marker behavior, and cancellation. Extend HTTP route tests to verify streamed bodies, Content-Length, and shutdown cancellation. Test CLI incomplete-response retry against replacement and atomic output publication; no arbitrary mutation retries are permitted. Verify durable MCP fetch replay uses the same request id after aborted execution and holds no handoff admission. Acceptance is a complete large file after a daemon restart with no manual repeated fetch and no delay waiting for that transfer before replacement.

## Idempotence and Recovery


All temporary payload files are owned by file handles and can be discarded. Named destination staging files are renamed only after complete input and length verification; ordinary errors preserve old files. A daemon restart retries from source under the existing durable request id. Unrelated errors remain visible. No schema migration, live installation, or database reset is part of this change.

## Artifacts and Notes


Tracking issue: #1544. Prior size-cap removal is already on master. Earlier validation timings on this contended host ranged from four to thirteen minutes; avoid redundant builds during iterative edits.

## Interfaces and Dependencies


Reuse tempfile, zip, tokio, futures, reqwest, ArchiveFile, CancellableProcessExecutor, and existing durable delegation storage. FetchedPath exposes a Read + Seek + Send reader and length; source and destination framing stays compatible. No new crate, persistent schema, size cap, or daemon-held transfer admission is needed.

## Outcomes & Retrospective


Disk-backed payload ownership, supervised file output, streamed HTTP bodies, atomic receiver writes, and automatic CLI retry are implemented. The real restart regression passed both CLI and MCP 64 MiB transfers without another fetch request, with 20,443,136 bytes source peak RSS, 20,004,864 bytes receiver peak RSS, and a 2.77-second restart. The test artifacts are in `target/reliability-artifacts/fetch-restart-seed-1544-3340249`. Full suites passed 176 checkpoint and 2,659 controller tests; the core suite passed 651 and its one unrelated Antigravity installer ETXTBSY failure passed on targeted rerun (652 total passing core tests). Worker suites, all 257 CLI tests, CLI daemon/terminal integration suites, the post-fix HTTP route, and final Clippy also passed. Initial acceptance passed; the review fixes recorded below now require their focused regressions and a second review before publication.

Plan created after tracing the full fetch pipeline and incorporating the restart requirement, 2026-11-15.

Implementation update, 2026-11-15: preserve the worker wire spool as a slice behind its metadata line rather than copying the payload a second time. Extend the existing explicit-offset ArchiveFile reader with a starting offset and bounded length. This avoids extra disk work during cancellation and gives every reader its own position. Added source-ready and destination-ready isolated hooks to prove restart while a transfer is pending; test code also records worker peak RSS.

Restart regression update, 2026-11-15: a real manual restart returned a generic 503 rather than an upgrade code. Fetch preparation now returns an explicit safe-to-retry shutdown code for either restart. HTTP body interruption is retried as a repeatable GET without checking daemon identity: the old control socket can still answer during HTTP shutdown, so that check would create a race. HTTP status refusals and disk errors remain terminal.

Validation update, 2026-11-15: `python3 tests/e2e/fetch_restart.py --mj target/debug/mj --worker target/debug/mj-worker --instance fetch-stream-restart` passed with the measurements recorded above. The initial fixture needed to wait for asynchronous provisioning before using the new session; it now does so and fails promptly if a transfer exits before its hook.

Final lint update, 2026-11-15: `cargo clippy --all-targets -- -D warnings` completed successfully against the final source. Core installer retry passed. The worker/CLI suites and the post-fix HTTP route rerun subsequently passed; no implementation validation remains.

Completion evidence, 2026-11-15: all full touched-crate suites are accounted for (checkpoint 176; controller 2,659; core 652 including the focused ETXTBSY rerun; worker 852 library + 5 binary and integration suites; CLI 257 and all integration suites). The manual-restart/streamed-shutdown HTTP regression passed on the final code. `cargo fmt --all -- --check`, Python compilation checks, `git diff --check`, and workspace Clippy passed. No live installation was performed.

Review fixes, 2026-11-15: the live restart fixture confirmed that a killed receiver left a 64 MiB NamedTempFile behind. Destination writes now use a stable per-destination staging path under an OS file lock. A replay must acquire that lock before reclaiming the old partial, so it cannot race or delete a surviving writer; unrelated destinations remain concurrent. Internal anonymous spools now live under the instance data filesystem's `fetch-spool` directory, avoiding memory-backed `/tmp`. Fixture constructors pass their own isolated spool files explicitly. The CLI compares daemon identity only after reconnecting through a fresh API probe, and API admission refuses probes once shutdown begins; same-daemon body errors therefore surface instead of looping. Pipe collectors now have one Running/Finished result owner that the process supervisor polls before waiting for the child. A failed spool terminates the process group and propagates the original I/O error.

Review validation update, 2026-11-15: the extended real restart test passed: CLI and MCP each transferred 67,108,864 bytes, source/receiver peak RSS was 20,561,920 / 19,976,192 bytes, and restart took 0.493 seconds. The new assertion found no remaining payload staging files. Artifact root: `target/reliability-artifacts/fetch-restart-seed-1544-2616494`. Both file-output tests passed, including the real RLIMIT_FSIZE write failure with a producer that otherwise sleeps indefinitely. Workspace Clippy and focused fetch tests passed. The final full run passed checkpoint 176, controller 2,659, and core 653 tests; one unrelated worker mailbox test missed its two-second lease timing assumption and is being rerun alone while the remaining suites continue.

Final review-fix validation, 2026-11-15: full suites completed with only `expired_mailbox_hook_lease_starts_idle_wake_without_another_request` missing its pre-existing two-second timing assumption; that exact case passed on isolated rerun. Every other suite, including CLI startup/PTY integration and doctests, passed. This accounts for checkpoint 176, controller 2,659, core 653, worker 852 library + 5 binary plus integrations, and CLI 257 plus integrations. No additional implementation or validation remains before review and push.
