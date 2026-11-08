# Deliver DSH and OpenCode mailboxes during active turns

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Oracle answers and child handbacks must reach a working lead before its current turn ends. Series 48 recorded delays of hours even though answers were already ready. DSH can receive input through its existing ACP steering extension. OpenCode 2.2.6 can extend tool results through its plugin API. Use those native interfaces with the existing worker-owned durable mailbox; keep ordinary idle wake behavior.

## Progress

- [x] (2026-11-07) Trace launch and delivery; claim issue #1473. Confirm DSH uses BrokkAi/dsh-acp and OpenCode 2.2.6 uses the new plugin API.
- [x] (2026-11-08 04:46Z) Implement DSH steering of durable mailbox wake prompts and OpenCode post-tool plugin staging.
- [x] (2026-11-08 04:50Z) Validate ordering, retries, native-child isolation, and actual model-visible mid-turn delivery in isolated instances. The two real pinned harnesses each consumed a 96 KiB message exactly once in request 2 of the original turn. Clippy passes. Worker library tests: 777 passed, 11 ignored, one unrelated work-facts failure passed on its isolated retry. All 20 remaining integration tests pass.
- [x] Commit initial implementation as c3c23d510 and merge current 2.69.0 master as f8b7d6a0d.
- [x] Correct the review finding about OpenCode acknowledgement ownership: retain stable per-event synthetic messages in OpenCode before acknowledging a relay lease.
- [x] Final dev-profile `cargo test -p brokk-mj-core -p brokk-mj-worker`, workspace Clippy with warnings denied, formatting and Python syntax checks pass. Both real pinned harnesses pass again with a clean environment at `/tmp/mjmb1473i/`.
- [ ] Complete final review of the durable OpenCode bridge, push origin/master, install locally, confirm daemon handoff, and report findings to session 1ea309c3a68c04ca5e3e64aeda6ee07a.

## Surprises & Discoveries

Mjolnir's `dsh` profile launches `@brokkai/dsh-acp`, an independent Rust harness rather than DeepSeek's Cordis-based native harness. The existing adapter already advertises `_session/steering`, applies input after generation and tool execution, and persists it. No adapter enhancement is necessary. The locally checked-out OpenCode 1.x plugin API is obsolete for the pinned 2.2.6: the latter exports a default plugin object with `id` and `setup`, and registers `tool.hook("execute.after", callback)`. Its real shell tool is named `shell`, whereas DSH exposes `run_command` with a separate executable and argument list.

The new relay regression caught steering at a newly promoted prompt before that prompt had been dispatched. Automatic steering now requires an in-flight prompt, using the existing durable dispatch state rather than another flag.

Push review found that acknowledging the hook subprocess's pipe write preceded actual OpenCode retention. The final bridge instead calls OpenCode's durable `session.synthetic` API with stable per-event IDs, and acknowledges only after all calls complete. The worker's drain reply adds per-event rendering and keys without changing journal formats; older hook clients continue to consume the existing combined text. An expired lease can be combined with new events, so IDs derive from each producer event key and native session ID, never from the lease or batch.

## Decision Log

Use existing durable wake prompts and steering for DSH, and the existing mailbox-hook lease protocol plus OpenCode's durable native inbox for OpenCode. This avoids a second Mjolnir delivery queue and new journal or database formats. Use `resume: false` so the current ACP-owned turn consumes steered input without starting an unowned native turn. Keep Claude and Codex delivery behavior intact. User explicitly requested direct implementation, then push and local installation before the final report; do not delegate implementation.

## Outcomes & Retrospective

Both delivery paths are implemented and verified against the actual pinned harnesses. DSH needs no upstream change or release. The OpenCode plugin leaves tool results intact, skips native children using the real `parentID` API field, and uses the shared worker IPC transport so long Unix socket paths work. The contract regression covers failed retention, lost acknowledgements, batch changes on retry, and Unicode payloads larger than a pipe buffer. Final review, push and installation remain.

## Context and Orientation

`mj-worker/src/relay/commands.rs` owns mailbox admission, claims, idle wakes, and steering decisions under one relay lock. `mj-worker/src/relay.rs` selects harness delivery policy. Wake commands already carry rendered mailbox content and participate in the durable prompt queue. `mj-worker/src/worker_runtime/subagents.rs` stages session-private harness configuration; `mj-worker/src/worker_runtime/unix.rs` calls it before launch. `mj-worker/src/mailbox_hook.rs` exposes the existing drain-and-ack command and the new split-phase mailbox transport used by OpenCode. `mj-core/src/relay/protocol.rs` adds per-event keys and rendering to the drain reply. OpenCode's `session.get` returns `parentID`, allowing native children to be excluded; `session.synthetic` admits and deduplicates durable native inbox entries.

## Plan of Work

First allow the DSH relay to create and automatically steer a durable mailbox wake while a prompt is running and the connected harness advertises steering. Preserve FIFO ordering, cancellation and checkpoint barriers, and do not retry uncertain or returned steering into the same turn. Existing journal replay and idle prompt promotion remain responsible for recovery.

Then add a small shipped JavaScript plugin for OpenCode 2.2.6. Stage it under the worker root and register its directory in the worker's `OPENCODE_CONFIG_CONTENT` through the existing JSONC helpers. This preserves user plugin entries and avoids editing the person's harness home. Skip sessions with `parentID`, lease events using `worker mailbox-request`, persist each through OpenCode's synthetic inbox with a deterministic ID, then acknowledge that lease. The shared Rust socket transport preserves existing IPC deadlines and long-path support. Child output pipes are drained concurrently. Staging must preserve user plugins and work for bare and container worker roots.

## Concrete Steps

Work in this repository on its current master branch. Use focused dev-profile tests while iterating. Run Cargo tests outside the restricted sandbox and retain the existing mbx build layout. Add no Cargo target-directory overrides. Verify formatting and run `cargo clippy --all-targets -- -D warnings` plus final affected-crate validation after implementation. Review the final diff, stage only changed files, commit, and push `HEAD:master`. If upstream moved, fetch and merge it rather than rebasing.

After pushing, run `scripts/install.sh` in a separate command. Do not edit source or commit while installation builds. Run an ordinary installed `mj` command to trigger daemon handoff, inspect the installed version and daemon identity, and report exact evidence and limitations to the requesting session.

## Validation and Acceptance

Extend relay ordering and recovery coverage with a DSH mailbox event delivered during a running prompt, including a second event and a returned steer. Verify the events appear once and the original prompt remains running. Exercise the OpenCode plugin with its real pinned binary and a recorded HTTP model server in a named isolated instance, proving a mailbox message appears in the next model request before turn completion. Include a native child case so the child cannot drain its parent's mailbox, and more than 64 KiB of hook content to verify pipe draining. Existing isolated test configurations must remain isolated.

## Idempotence and Recovery

The relay's existing event keys deduplicate retries, mailbox-hook leases return unacknowledged content, and steering keeps uncertain prompts durably queued. Plugin staging replaces only its own artifact. No live store migration or release is required. Local installation is explicitly authorized and uses the repository's atomic binary replacement script.

## Artifacts and Notes

Research sources are the v2.2.6 OpenCode tag, especially `packages/plugin/src/promise/{plugin,tool,session}.ts` and `packages/schema/src/tool.ts`, and BrokkAi/dsh-acp `docs/steering.md`. Temporary extracts are under `/tmp/mj-mailbox-harness-sources/`. The evaluation evidence is in the two report paths supplied by the requester.

Run the reproducible acceptance fixture with `python3 tests/e2e/mailbox_harnesses.py --worker target/debug/mj-worker --opencode /path/to/opencode-2.2.6 --dsh /path/to/dsh-acp-0.5.0 --cache /path/to/harness-cache-parent --root /tmp/a-new-short-directory --instance mailbox-recorded`. Each harness must print that its mailbox reached request 2 of the original turn exactly once. The root must be short enough for a Unix socket. Evidence from this run is `/tmp/mjmb1473e/open-code/result.json` and `/tmp/mjmb1473g/dsh/result.json`, with full model requests beside each. Check logs are `/tmp/mj1473-clippy.log`, `/tmp/mj1473-worker-tests.log`, `/tmp/mj1473-work-facts-rerun.log`, and `/tmp/mj1473-worker-integration.log`.

## Interfaces and Dependencies

Use existing `RelayCommand::MailboxWake`, `RelayCommand::Steer`, `ClaimedSteeringPrompt`, `DrainMailbox` and `AckMailbox` interfaces. OpenCode 2.2.6 uses a default `{ id, setup(context) }` JavaScript export; `context.tool.hook("execute.after", callback)` receives `sessionID`, `status` and a mutable `result` or `error`, and `context.session.get` supplies the session's `parentID`. Do not add a dependency or new crate.

The plugin calls `context.session.synthetic({sessionID, id, text, delivery: "steer", resume: false})`. OpenCode's inbox reconciles IDs against both pending input and already-promoted messages. The new additive drain-response `contexts` field carries `MailboxHookContext {key, text}` entries. Existing clients ignore this field. `worker mailbox-request --socket <path>` takes one `DrainMailbox` or `AckMailbox` JSON request on stdin and returns the existing serialized response payload; it shares `mailbox_hook.rs::send_request` with the command hooks.

Revision note (2026-11-07): initial plan records confirmed harness APIs and Jonathan's implementation and installation instructions.

Revision note (2026-11-08): record the implemented worker-owned plugin registration, the dispatch-order correction, and completed real-harness and Rust validation.

Revision note (2026-11-08): replace pipe-write acknowledgement with durable native retention and per-event retry IDs after the P1 review finding. Final touched-crate tests and Clippy pass. Both real pinned harnesses pass with the final bridge and clean test environment at `/tmp/mjmb1473i/{open-code,dsh}/result.json`.
