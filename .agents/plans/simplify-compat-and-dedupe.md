# Simplify Mjolnir without cutting scope

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Mjolnir (the `mj` command) has picked up code for scenarios that no longer matter, such as upgrading databases from before 2.0, talking to workers running a relay protocol from months ago, or reading configs written by a newer build. It also has the same logic in several places, and some of those copies have already drifted apart. This plan removes that code **without removing any feature**. Afterward, every user-facing feature still works as before. There is less code to maintain, and fewer duplicate copies to drift apart and cause bugs.

Nothing new appears for a user. Two things show the change has worked. First, the full test suite and clippy still pass while the line count drops by several thousand lines (`git diff --stat` per milestone). Second, the end-to-end checks listed under Validation still pass: sessions start, resume, checkpoint, import, and show quota on the local, docker, and ssh targets.

Two larger merges were found but are out of scope for this plan. They are tracked as GitHub issues #1043 ("Route all TUI work through the daemon") and #1044 ("Collapse the three daemon transports into one HTTP API").

## Progress

- [x] (2026-09-16) Audit of mj-controller, the worker/core crates, and the UI crates, done as three parallel read-only audits.
- [x] (2026-09-16) The user set the compatibility floor at 2.8.0 and ruled out scope cuts.
- [x] (2026-09-16) Filed #1043 and #1044.
- [x] (2026-09-16) Milestone 1: delete dead code. Daemon protocol raised to 20 because `CreateSessionRequest` lost `allow_dirty_local`.
- [x] (2026-09-16) Milestone 2a, database part: `mj-controller/src/database/baseline.sql` creates a new store at revision 33 and replaces migration steps 1–33. Stores older than revision 33 are refused.
- [x] (2026-09-16) Milestone 2a, remaining: loading a newer build's config read-only, the `raw-localhost` rename, the 1.x `state.json` import, the two-part web login cookie, and the Ctrl-G/Ctrl-Q "moved" notices are removed.
- [x] (2026-09-16) Milestone 2b: the worker checks the relay protocol once, when a request arrives, and serves only the current version. The controller checks each request's minimum protocol once, in `WorkerClient::call_with_timeout`. Checkpoint export uses the shared staging-command loop, and the fallback that uploaded the spec file is gone. Journal span boundary digests come from one helper.
- [x] (2026-09-16) Milestone 3, part 1: one set of ssh, scp and container-exec builders in `mj-core/src/targets/ssh.rs` and `targets.rs`. `locator_command` builds every exec-style per-target command (architecture probe, worker stop/liveness/probe/last words, digest, launch refresh, reconnect, checkpoint cleanup). Remote upload staging uses `REMOTE_UPLOAD_STAGING`.
- [x] (2026-09-16) Milestone 3, part 2: `mj_core::bounded_frame::read_bounded_frame` replaces four async frame readers (each caller keeps its own meaning for a partial frame at end of stream). `SessionState::as_str`/`from_stored`, `TargetTemplate::kind_name` and `TargetLocator::kind_name` replace four hand-written maps. One `mj_core::config::sync_directory` replaces four copies.
- [ ] Milestone 4: merge duplicated feature paths.
- [ ] Milestone 5: larger merges that keep behavior the same.
- [ ] Milestone 6: hand the Kimi quota token refresh to the Kimi CLI.

## Surprises & Discoveries

- Observation: The controller's `scp_command_spec` passed an SSH target's `-p PORT` straight to `scp`, where `-p` means "preserve file times". The checkpoint-transfer copy rewrote it to `-P`. With `extra_args = ["-p", "2222"]`, every controller-side upload (worker binary, launch config, checkpoint spec, reviewer profile) would treat `2222` as a local source file. Found by reading the code; not reproduced against a live host.
  Evidence: `mj-controller/src/checkpoint_transfer.rs` `scp_command` vs `mj-controller/src/controller.rs` `scp_command_spec` before commit "Build per-target commands in one place". The new test `scp_translates_the_ssh_port_option_and_is_tagged_with_its_destination` pins the shared behavior.
- Observation: The journal rewrite path stores `Some("")` as the previous-record digest for a format v2 record. The other two places that build this value store `None`.
  Evidence: `mj-worker/src/relay/journal.rs` `rewrite_relay_journal` sets `Some(first.previous_digest)`. The ack and floor checks that run first currently hide the difference.
- Observation: The attachment request gate in `mj-worker/src/worker_runtime/unix.rs` hardcodes protocol `8..=` and returns `InvalidRequest`. The other five copies of the gate return `IncompatibleProtocol`.
- Observation: Kimi OAuth access tokens live for 900 seconds, so a quota display that never refreshes would show "login expired" most of the time.
  Evidence: the `expires_in: 900` field in a Kimi credentials file.

## Decision Log

- Decision: Milestone 3 leaves three duplicates alone: the relay journal validation loops, the `zai_usage`/`muse_usage` HTTP body readers, and the database's target-kind column projection.
  Rationale: The validation loops check different things (some enforce ordinal continuity, one enforces only the v1 chain link), so merging them would change validation. The body readers are two 12-line loops over `reqwest` streams, and `mj-core` does not depend on `reqwest`. The database projection maps a different locator type into several columns at once.
  Date/Author: 2026-09-16, agent.
- Decision: File transfers into and out of targets keep their per-target code for now. Only exec-style commands move to `locator_command`.
  Rationale: The transfer sites differ in meaning, not just spelling. Some copy a directory's contents and others the directory itself, and they differ in ownership fixups, atomic renames, and content-addressed caching. They cannot be checked end to end against docker, podman-over-ssh or EC2 in this environment, so a shared copy helper would risk untested behavior changes. The drift that was real (the scp port flag and two upload staging directories) is fixed.
  Date/Author: 2026-09-16, agent.
- Decision: The controller keeps accepting older worker protocols at hello. It does not require exactly 13.
  Rationale: The worker-upgrade coordinator (`mj-controller/src/worker_upgrade.rs`, `controller/worker_restart.rs::upgrade_session_worker`) replaces an outdated worker only after it connects, leases the connection, and reads a quiet snapshot. Stop and checkpoint also need a connection. Refusing older workers at hello would strand every session still running a pre-2.8 worker. So the protocol logic was merged instead: one worker-side rule (`mj_core::relay::protocol::relay_protocol_rejection`, current version only), and one controller-side check that refuses a request an older worker cannot decode with the same `IncompatibleProtocol` code the worker would use. Eight call-site checks and the checkpoint pre-check are gone.
  Date/Author: 2026-09-16, agent.
- Decision: Keep journal record format v1, `LegacyFlaggedObservation`, the `native_continuity_lost` field, `adopt_unqueued_queue_commands`, the `activity_turn_started_at_ms` backfill, and the historical image-count leniency.
  Rationale: Each one reads stored content that a 2.8+ install can still hold, and each is covered by an event digest or a stored snapshot. v1 records stay in a journal until a checkpoint acknowledges past them. The flagged `session_opened` encoding was written by commits 0f070506 through e6ed54ed, all inside the 2.8.0 range, and by development builds. Dropping a serialized field changes how existing records re-serialize, so their digests would stop matching (the same failure the protocol-13 note in `mj-core/src/relay.rs` describes). The one real defect, the rewrite path storing `Some("")` as a span's boundary digest, is fixed by sharing `span_previous_digest`.
  Date/Author: 2026-09-16, agent.
- Decision: Milestone 2a keeps shims that read old *content* rather than serve old *installs*. These are the `[startup]` section and top-level `show_stopped_sessions` in config files, config versions 1–8, `WirePolicy::Legacy`/`force_unrestricted_mode` in launch specs stored inside checkpoint archives, and `LEGACY_HANDOFF_PREAMBLE` in stored transcripts.
  Rationale: The 2.8.0 floor says nothing about how old a file's contents are. A config file is rewritten only when a setting changes, and archives and transcripts are never rewritten, so a current 2.8+ install can still hold this content. With `deny_unknown_fields`, dropping the config aliases would stop such a config from loading.
  Date/Author: 2026-09-16, agent.
- Decision: Deferred three planned shim removals.
  Rationale:
  - `ViewerPromptImage.data_base64` inline images: the web API still accepts them, and many image-limit tests are written against them, so removing them saves little now.
  - `inspector_move_preparation` in `server.rs`: it duplicates the image stripping in `controller/move_session.rs`, and it is the only place that behavior is tested. It moves to Milestone 4 as a DRY merge, together with a test at the daemon boundary.
  - `incompatible_resume_targets` is not a shim: request validation uses it.
  Date/Author: 2026-09-16, agent.
- Decision: Keep the database compatibility floor (`schema_compatibility`, `read_schema_state`) and the writer's schema re-read before each write. Only the migration steps are replaced.
  Rationale: The repository's CLAUDE.md "Testing Guidelines" require every migration to be classified as compatible or breaking and the minimum compatible revision to be raised for breaking ones. The floor is how that rule works, and it lets an older build keep using a store that a newer compatible build migrated. Removing it would contradict a standing repository rule, so it is out of scope for "no downgrades".
  Date/Author: 2026-09-16, agent.
- Decision: `baseline.sql` is generated from a store created by the migration steps, then reformatted. It is checked two ways: `pragma table_xinfo`, `foreign_key_list`, `index_xinfo`, STRICT flags and SQL text all match a store the migration steps created, and they also match the maintainer's live store, which is at revision 33 and was upgraded step by step since August.
  Rationale: 2.8.0 shipped revision 33, so the baseline must equal what those steps produced, including the `'deepseek'`/`'zcode'` values that CHECK constraints still allow. The default workspace row the steps inserted is part of the baseline.
  Date/Author: 2026-09-16, agent.
- Decision: Milestone 1 keeps four audit candidates.
  Rationale:
  - `Controller::resume_session_with_options` is the entry point of the podman import end-to-end test (`mj-cli/tests/import_e2e.rs`).
  - `TranscriptSnapshot::browser_transcript`/`browser_tail`/`transcript_snapshot` in mj-chat are how dozens of projection behavior tests reach the shared `mj_client::transcript::browser_transcript`.
  - `format_activity_clock` is the label code Milestone 4 consolidates onto.
  - `DashboardState::handle_key`/`handle_mouse` are the TUI tests' input entry points, not legacy wrappers.
  Date/Author: 2026-09-16, agent.
- Decision: Removing `allow_dirty_local` from `CreateSessionRequest` raises `mj_client::daemon::PROTOCOL_VERSION` from 19 to 20.
  Rationale: The field had no serde default, so a daemon from before the change would reject a new client's create request. The version handshake makes that mismatch explicit instead.
  Date/Author: 2026-09-16, agent.
- Decision: The compatibility floor is 2.8.0. A database, config, journal, or worker from before 2.8.0 does not need to upgrade in place.
  Rationale: The user chose this. Protocol 13 first shipped in 2.8.0 (2026-09-15). The controller already replaces stale workers.
  Date/Author: 2026-09-16, user.
- Decision: No feature is removed. The desktop app, Windows and Android code, every update channel, web port-conflict recovery, the TUI second opinion, spinners and themes, orphan worker adoption, and journal corruption recovery all stay.
  Rationale: The user wants the same scope implemented more cleanly.
  Date/Author: 2026-09-16, user.
- Decision: Kimi quota token refresh moves to the Kimi CLI (start `kimi acp` briefly). mj's Rust copy of proper-lockfile and its lock-takeover handling are deleted. If `kimi acp` does not refresh an expired token, fall back to a plain lock with take, heartbeat, and release.
  Rationale: Claude, Codex, and Grok quota already let their own CLI refresh the login. Copying another program's lock protocol drifts when that program changes.
  Date/Author: 2026-09-16, user and agent.
- Decision: The TUI-through-daemon merge and the transport merge each get their own ExecPlan later.
  Rationale: Each is high-risk and touches most crates.
  Date/Author: 2026-09-16, user.

## Outcomes & Retrospective

Not started.

## Context and Orientation

The repository is a Rust workspace. It has these main parts:

- **Controller** (`mj-controller`): the long-running daemon. It owns the SQLite store (`mj-controller/src/database.rs`, schema in `mj-controller/src/database/schema.rs`) and the web server (`server.rs`, `server/api.rs`, `server_runtime.rs`). It launches and supervises workers (`controller/*.rs`, `worker_client.rs`, `session_manager.rs`) and polls quota (`quota.rs` plus the `*_usage.rs` modules).
- **Worker** (`mj-worker`): the process that runs one coding-agent harness (Claude, Codex, Grok, Kimi, Muse) on a target. A *target* is where a worker runs: local, a docker/podman container, an ssh host, or AWS.
- **Relay:** the worker records every session event in a *relay journal* (`mj-worker/src/relay.rs`, `relay/journal.rs`) and speaks the *relay protocol* to the controller over a stream of newline-delimited JSON. The protocol version is `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs`, currently 13.
- **Shared types** live in `mj-core` (`config.rs`, `targets.rs`, `relay/snapshot.rs`, `relay/protocol.rs`).
- **Clients:** the TUI is `mj-tui`, `mj-chat`, and `mj-cli/src/dashboard*`. The web viewer's JavaScript is served by the controller. `mj-client` holds code shared by clients.

A *compatibility shim* is code whose only job is to accept data or peers from an older (or newer) version. A *DRY violation* ("don't repeat yourself") is logic implemented in more than one place.

Run every Cargo command from the repository root. Run `cargo test` outside the sandbox, on the dev profile.

## Plan of Work

### Milestone 1: delete dead code

Delete production code that only tests call. Before deleting each item, confirm with `grep -rn <name> --include=*.rs mj-* voice-worker` that every remaining caller is inside a `#[cfg(test)]` module. Then delete the tests that exercised only that code. The items:

- **Claude `/usage` screen-scraper:** in `mj-controller/src/claude_usage.rs`, `parse`, `parse_window`, `percentages`, `strip_ansi` and their helpers. Production reads usage through `query` and `parse_api_usage`.
- **Unused controller functions:**
  - `database.rs`: `workspace_for_session`, `load_materialized_transcript_after`, `load_materialized_transcript_tail`
  - `recovery_gate.rs`: `wait_idle`
  - `setup.rs`: `discover_harness_homes`, `harness_is_authenticated`
  - `import.rs`: the `*_config_home` functions
  - `controller/worker_binary.rs`: `diagnose_worker`
  - `controller/provisioning.rs`: `provision_session_controlled`, `provision_session_with`
  - `controller/resume.rs`: `resume_session_with_options`
- **Unused client functions:**
  - `mj-client/src/usage_format.rs`: `format_activity_clock`
  - `mj-tui/src/ingest.rs`: `rekey_session_operation`, `set_session_operation_stage`
  - `mj-chat/src/chat/transcript.rs`: `browser_transcript`, `browser_tail`, `transcript_snapshot`
  - `mj-tui/src/lib.rs`: the legacy `handle_key` and `handle_mouse` wrappers
- **Dirty-checkout confirmation:** the TUI's `Confirmation::DirtyLocal` and `show_dirty_local_confirmation` in `mj-tui/src/dialogs.rs`, plus the `allow_dirty_local` plumbing through mj-tui, mj-cli, and the controller. The controller already ignores the field. The web viewer's `dirty_ack` is a separate mechanism and stays.
- **`WorkerSessionSummary`** in `mj-core/src/relay/types.rs`.
- **Unreachable block:** `if !terminated` in `mj-worker/src/relay/journal.rs`.
- **Orphaned doc comment:** in `mj-worker/src/storage.rs`.

Where an audit listed a function but a production caller turns up, keep it and record that in Surprises.

### Milestone 2a: database, config, and small shims

- **Database migrations:**
  - Read the schema version that 2.8.0 shipped: `git show v2.8.0:mj-controller/src/database/schema.rs`.
  - In `migrate_schema`, replace every step below that version with one baseline that creates the 2.8.0 schema directly. Keep the later steps.
  - A store older than the baseline fails with an error saying it was created by an mj older than 2.8.0 and must be upgraded through 2.8 or 2.9 first, or started fresh.
  - Delete the downgrade compatibility floor and the schema re-read that `database.rs` runs before every queued write (`writer_schema_state`). The daemon's exclusive store guard already prevents another build from migrating underneath it.
  - Delete `migrate_legacy_state` (the `state.json` import).
  - Classify the change as breaking for pre-2.8 stores, next to the code.
- **Config** (`mj-core/src/config.rs`):
  - Delete read-only loading of a config written by a newer version: `load_newer`, the `salvage*` functions, `newer_version*`, `ensure_writable`, `newer_build_notice`, the `newer_config_version` field, and the TUI notice in `mj-tui/src/actions.rs`. A config with a newer version becomes a hard error telling the user to upgrade mj.
  - Delete `migrate_legacy_localhost_target` (and its call in `mj-controller/src/daemon.rs`), `discard_legacy_startup`, and `show_stopped_sessions`.
- **Small shims:**
  - In `mj-controller/src/server.rs`: the two-part legacy login cookie and viewers without an identity; `ViewerPromptImage.data_base64`; `incompatible_resume_targets`; and the "older daemon" guard in the move-preparation handler.
  - `LEGACY_HANDOFF_PREAMBLE` in `compaction.rs`.
  - `WirePolicy::Legacy` and `force_unrestricted_mode` in the worker launch code.
  - The `authentication_marker` fallback.
  - The generation-0 reviewer staging home and the legacy reviewer archive names.
  - The Kimi `LEGACY_TASK_*` event aliases.
  - The Codex `session_id` checkpoint fallback.
  - The inline image-count leniency for old workers.
  - The Ctrl-G/Ctrl-Q "moved to Alt" notices in `mj-tui/src/lib.rs` and `mj-chat/src/chat.rs`.
  - The old `hel` binary name in `worker_sibling_names`.
  - `suppress_duplicate_standalone_terminal_output` in `mj-client/src/transcript.rs`. Delete it only after confirming that 2.8.0 no longer writes the duplicate it hides.

### Milestone 2b: protocol, journal, and worker export

- **Relay protocol:** accept exactly `RELAY_PROTOCOL_VERSION`.
  - Delete `RELAY_MIN_PROTOCOL_VERSION`, `RelayRequest::minimum_protocol`, and `RelayCommand::minimum_protocol`.
  - Delete the per-request version gates in `mj-worker/src/worker_runtime/unix.rs`, `mj-worker/src/relay.rs`, and the controller (`worker_client.rs`, `controller/checkpoint.rs`, `session_manager.rs`).
  - Keep one version check at hello. When the controller sees a mismatch, it treats the worker as outdated and replaces it through the existing stale-worker refresh path (`refresh_remote_worker_binary_if_stale` and `worker_upgrade.rs`).
- **Worker-export fallbacks:** delete the ladder in `controller/checkpoint.rs` that retries a checkpoint export by uploading the spec file and then swapping the worker binary, chosen by matching error text (`export_spec_stdin_unsupported`, `staging_protocol_unsupported`).
- **Wire fields and journal formats:**
  - Delete `native_continuity_lost` everywhere. It is always false.
  - Delete `LegacyFlaggedObservation`. No release wrote that encoding.
  - Delete journal record format v1: the v1 constants and serde defaults, the v1 digest arm, the v1 `previous_digest` chain checks in `relay.rs`, and `file_first_previous_digest`. This removes the `Some("")` inconsistency noted in Surprises.
  - Delete `adopt_unqueued_queue_commands` and the `activity_turn_started_at_ms` backfill in `relay.rs`.

### Milestone 3: merge duplicated target and process plumbing

- **Target commands:**
  - `mj_core::targets::command_on_locator` already maps a command onto every target kind. Make every hand-written copy of that match call it instead:
    - in `controller/worker_binary.rs`: `target_architecture`, `stop_worker_command`, `worker_liveness_command`, `worker_binary_probe_failure`, `worker_last_words`
    - in `controller/checkpoint.rs`: `upload_checkpoint_spec`
    - in `controller/reviewer.rs`: the command dispatch
    - in `checkpoint_transfer.rs`: the command dispatch
  - Delete the duplicate `ssh_command_spec` and the second copies of `ssh_command` and `container_exec`.
  - Use `container_engine()` instead of re-matching the engine with `unreachable!`.
  - Add one "copy a file into a target" helper next to `command_on_locator`.
- **Remote paths:** put the worker's remote path layout in one mj-core module. Choose one remote staging directory in place of the three in use today.
- **Line reader:** add one length-limited line reader in mj-core, based on the one in `relay/protocol.rs`, and use it in `codex_usage.rs`, `grok_usage.rs`, `worker_client.rs`, and `worker_runtime/unix.rs`. Use one bounded body reader for `zai_usage.rs` and `muse_usage.rs`. A partial line at end of input is an error.
- **Enum names:** give the mj-core session-state and target-kind enums `as_str` and `FromStr`, and delete the hand-written string maps in the controller and TUI.
- **Journal file operations:**
  - Keep one `sync_directory`, shared by mj-worker and mj-checkpoint.
  - Keep one helper that rotates the active journal to an empty file.
  - Keep one loop that validates the first record and then the chain.

### Milestone 4: merge duplicated feature paths

- **MCP stdio servers:** the three MCP stdio servers in mj-worker (`memory_mcp.rs`, `review/mcp.rs`, `subagent_mcp.rs`) share one serve loop with concurrent dispatch. Merge the unix-socket clients `send_dispatch` and `send`.
- **Importers:** delete the per-harness `import_*_session` wrappers in `mj-controller/src/import.rs` and route every harness, including Claude, through `import_native_session`. Collapse the CLI dispatch in `mj-cli/src/import.rs`. Merge the three import e2e scripts into one script that takes the harness as a parameter.
- **Crate-split traits:** delete `SubagentBackend`, `ExportRuntime`, and `SessionStateSource`, which date from when the API and daemon were separate crates. Handlers call `RuntimeState` directly.
- **Wrapper stacks:** fold `X` / `X_controlled` / `X_with_manager` / `_inner` stacks into one function with an optional control argument.
- **Activity labels:** session activity and lifecycle labels come from one place, `mj-client/src/usage_format.rs`, using the TUI's wording. The controller sends the label with its start time to the web viewer, whose JavaScript only formats the clock.

### Milestone 5: larger merges that keep behavior the same

Do each item as its own commit.

- **Composer drafts** use one per-client store for TUI and web, with detached-draft recovery kept. `sessions.draft_input` is removed by a migration that copies existing drafts.
- **Background work:** the relay's per-harness background-work maps become one map keyed by a namespaced id, with small per-harness adapters.
- **Dictation:** `voice-worker` only captures audio and posts the WAV to the daemon's dictation endpoint.
- **Imports** emit relay observations instead of rebuilding tool calls from chat entries. Stop and record a finding if mj-chat still needs the entry model.
- **Self-updater:** it runs `install.sh` with a pinned version instead of keeping its own copy of asset selection and checksum handling. `install.sh` must require a checksum.

### Milestone 6: Kimi quota refresh

- **What changes:** `mj-controller/src/quota.rs` stops refreshing Kimi OAuth tokens itself. For each quota poll, start `kimi acp` with the profile's home and environment, send `initialize` (and `session/new` if that is what makes Kimi check its token), and stop it within a time limit. Then read `credentials/kimi-code.json` and query `/usages`. On an HTTP 401, do this once more, then report that the login expired. Follow the Grok pattern in `grok_usage.rs`.
- **Delete:** `KimiRefreshLock` and its constants, `ensure_fresh_kimi_token`, `save_kimi_credentials`, `decide_kimi_refresh_persist`, `KimiLockLoss`, `KIMI_OAUTH_CLIENT_ID`, and their tests.
- **Check before deleting:** run the check under Validation against a copy of a Kimi home. If Kimi does not refresh on ACP startup, keep mj's refresh with a plain lock (take, heartbeat, release) and record that in the Decision Log.

## Concrete Steps

From the repository root, after each milestone:

    cargo clippy --all-targets -- -D warnings
    cargo test
    git diff --stat

Commit only the files you changed, on `master`.

## Validation and Acceptance

Every milestone must pass `cargo clippy --all-targets -- -D warnings` and `cargo test` on the dev profile. Then check the milestone's behavior:

- **Milestone 2a:**
  - Open a store migrated by 2.8.0 with the new build. `sqlite3 <store> .schema` must match a store created fresh by the new build.
  - A store from before 2.8.0 must fail with the clear error.
  - Use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.
- **Milestone 2b:** start a worker from 2.8.0 under the new controller. It must be replaced and the session must resume.
- **Milestone 3:** run the `tests/e2e` checks for the local, docker, and ssh targets: upload, checkpoint export, worker stop, and liveness. Stream tests for the shared line reader must use more than 64KB of data.
- **Milestone 4:** import a session for each harness. MCP tool calls for memory, review, and subagents work in a live session. Web and TUI activity labels match.
- **Milestone 5:**
  - A draft survives detach and reattach in both the TUI and the web viewer.
  - Background tasks appear and can be stopped for Claude, Codex, and Kimi.
  - TUI dictation works under WSLg.
  - Updating from 2.9.0 works with both curl and npm installs.
- **Milestone 6:** make a temp copy of a Kimi home and set `expires_at` in the past. After the quota poll, the file must hold a rotated token pair and the dashboard must show Kimi windows. Never run this against the real `~/.kimi-code`: the refresh token is single-use, so ask the user before spending it.

## Idempotence and Recovery

Each milestone is one or more commits that can be reverted on their own. Milestone 2a drops upgrade support for stores older than 2.8.0. Test it only against isolated config and data directories, never the live store.

## Interfaces and Dependencies

At the end of Milestone 3, `mj-core` exposes:

- `targets::command_on_locator`, used by every per-target command
- a single copy-into-target helper
- a single remote path layout module
- a single bounded line reader

At the end of Milestone 4, `mj-worker` has one MCP stdio serve function used by all three MCP servers.
