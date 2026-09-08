# Import DSH and Muse native sessions with workspace relocation

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture

Users can discover external DSH and Muse sessions in mj's Import tab or with `mj import deepseek` (`dsh` alias) and `mj import muse`, adopt their native history and Git workspace as a stopped mj session, and resume the same native identity at a different workspace. Configure the local Muse profile using the installed harness and existing credentials. Target DSH 0.1.2-rc.1 and Muse 1.0.3-R2198.1. Do not push or release.

## Progress

- [x] (2026-09-08) Research import/checkpoint interfaces and native storage; inspect installed versions and sibling sources.
- [x] (2026-09-08) Prove isolated Muse echo session resumes with history at relocated metadata workspace when opaque records remain byte-exact.
- [x] (2026-09-08) Implement native readers, discovery, transcript projection, and restore transformations.
- [x] (2026-09-08) Integrate CLI/TUI import, isolated native artifact collection, and DSH runtime upgrade.
- [x] (2026-09-08) Configure local Muse; real tmux/native sessions imported, restored, and continued through ACP with remembered context and destination writes.
- [x] (2026-09-08) Fresh native DSH and Muse sessions appeared together in the tmux TUI Import tab; already-imported sessions were excluded. Implementation is ready for the required current-branch commit.
- [x] (2026-09-08) Review implementation and docs; core/controller/CLI/worker suites, focused import tests, authenticated Muse relocation test, formatting, and clippy passed.

## Surprises & Discoveries

Muse configuration and sessions have separate XDG roots. External sessions normally live in ~/.local/share/muse/sessions; mj workers use <profile-home>/.data/muse/sessions. Muse MSP resume has no workspace override. Updating copied plain runtime.session.metadata payload.record.workspace_root preserves the native ID and returns the destination workspace plus inline history. Reserializing opaque retained permission frames causes native validation errors even when JSON values appear equivalent; preserve those bytes. The successful probe lives in /tmp/mj-muse-relocate-probe-o0zldw77.

DSH 0.1.2-rc.1 is now installed and includes a bundled ACP profile. It uses format version 0. The sibling development tree has later generations, so its frozen historical codec and the installed release are the compatibility references. DSH requires both the workspace directory key and persisted header cwd to be relocated.

Real Muse sessions include subagent metadata declaring only provider/model, without a workspace field. These child records inherit the parent workspace and must remain unchanged. The top-level importer still requires an absolute workspace. Actual runtime payloads include schema v2 as well as v1. DSH session/new requires an explicit empty mcpServers list; resume omits it to retain native configuration. Clean npm validation showed --legacy-peer-deps omitted the required Cordis group plugin; removing the flag also passed clean Codex and Claude installs.

## Decision Log

Use the existing import/archive/state workflow, adding no crate or service. Readers and restore transformations share core modules so path and format interpretation is not duplicated. Native transcript projections match existing imports: visible messages/turns and reliable edit paths; full native tool/reasoning state remains in artifacts. Native sources are never mutated.

The user chose Muse relocation rather than the existing same-path restriction and specified DSH 0.1.2-rc.1. Switch the managed DSH command to its bundled `dsh --profile acp`, removing the separately pinned bridge. Retain Muse's one-workspace restriction. The Muse bridge repository may be changed if an actual integration defect requires it, but no new ACP import API is required.

## Outcomes & Retrospective

Native import and relocated continuation work for DSH 0.1.2-rc.1 and Muse 1.0.3-R2198.1. Both real native IDs were retained, remembered context survived, and tools wrote into the destination. All 27 existing source file hashes remained unchanged. Local profile `muse` now uses `~/.config/muse` with existing credentials; no Muse bridge source change was necessary. The standalone worker and CLI build. Core (867), controller (713), CLI (208), worker (109) unit tests passed, as did binary/integration/doc tests and all 58 focused import tests. The authenticated Muse checkpoint relocation test passed separately. Documentation built 24 pages and checked 1701 links. Formatting and default-workspace all-target clippy passed. No full container image was rebuilt; container pin consistency tests and clean exact managed npm installs validated the affected runtime inputs. The existing user modification to AGENTS.md must not be staged.

## Context and Orientation

`mj-controller/src/hel_import.rs` discovers native sessions, projects messages into canonical mj events, collects Git state, and publishes archives/stopped session records. `mj-cli/src/import.rs` exposes CLI commands and background TUI import; dashboard discovery runs per profile. `src/hel_checkpoint.rs` collects native artifacts and restores their paths/bytes. `src/hel_config.rs` defines harness launch arguments and profile home mapping. `src/hel_harness_runtime.rs`, worker npm lockfiles, and the container file carry managed pins.

Native state is the harness's own durable session log. Canonical history is mj's separate event transcript used for display. Import must capture both and preserve the original native ID for ACP resume. Existing Git safety acknowledgements, archive verification, cancellation, and profile/bundle selection remain authoritative.

## Plan of Work

First add independent DSH/Muse core storage modules and controller import-reader submodules. DSH reads plain or concatenated-zstd version-0 records and packed chunks, validates IDs/cwd and excludes child sessions from top-level discovery. Muse reads versioned record envelopes and retained-frame children, deriving visible conversation without replaying internal bookkeeping. Recognized metadata relocation changes only the necessary workspace values. Unknown incompatible formats produce contextual errors.

Then connect generic locate/read/scan dispatch and CLI commands. Resolve Muse external data roots separately from worker roots, and normalize copied artifacts into the existing worker archive layout. Capture a consistent selected native tree, rejecting concurrent changes instead of publishing a mismatched transcript/archive. Keep scanning and import off UI loops, report errors, and preserve cancellation cleanup.

Integrate restore transformations and replace Muse's blanket relocation rejection with supported-format validation. Upgrade the DSH package/lock/install identity/container command to 0.1.2-rc.1 and bundled ACP. Validate the real ACP implementation's controls and execution policy, rather than assuming compatibility with the old bridge.

Finally configure the local Muse profile without changing unrelated settings. Create isolated native sessions through tmux/headless harnesses, import through mj, relocate and continue. Test remembered context and destination tool writes, unchanged originals, permission behavior, and a subsequent checkpoint/resume. Update product documentation and commit validated coherent changes on the current branch.

## Concrete Steps

Work in /home/jonathan/Projects/hel3. Delegate DSH and Muse reader modules to Luna with distinct file ownership; primary owns integration and restore call sites. Use installed DSH modules under /home/jonathan/.nvm/versions/node/v24.15.0/lib/node_modules/@deepseek-ai/dsh as release authority, ../deepseek-harness as supporting source, and ../muse-acp plus `muse schema generate-json-schema` for Muse protocol evidence. Do not copy private logs or credentials into fixtures.

Run focused tests with elevated permissions: `cargo test -p brokk-mj-core hel_native`, `cargo test -p brokk-mj-controller hel_import`, and `cargo test -p brokk-mjolnir import`. Run `cargo fmt --all -- --check`, affected checkpoint/resume/runtime tests, `cargo clippy --all-targets -- -D warnings`, and the relevant standalone worker build. Use normal build storage, never /tmp for Cargo output. Coordinate test runs to avoid redundant compilation.

## Validation and Acceptance

Both CLI commands and the Import tab discover supported external sessions, show history, and create stopped records. Resume preserves native identity/history after moving directories, and subsequent tools operate only at the intended destination under the configured execution policy. Source logs/workspaces remain unchanged. Large compressed/streamed fixtures exceed 64 KiB. Test corrupt/unsupported input, duplicate IDs, child exclusion, repeated metadata, byte-exact opaque frames, cancellation, concurrent writes, and path encoding. Real Muse tests must include interactive tmux sessions and authenticated context recall; DSH tests use 0.1.2-rc.1 and its bundled ACP profile.

## Idempotence and Recovery

Use existing atomic archive publication and cancellation cleanup. Native source snapshots are read-only. Stop owning process groups before removing scratch profiles/workspaces. Retain useful failure evidence without exposing secrets. Configure Muse additively through the existing config mechanism. Do not modify unrelated user changes or push repositories.

## Artifacts and Notes

Planning probe: Muse 1.0.3 resumed an echo session after changing only its copied plain workspace metadata; returned destination workspace and five inline history items. This proves basic relocation feasibility, not authenticated tool/permission behavior, which remains a release acceptance test for this feature.

## Interfaces and Dependencies

Core native modules expose format readers and restore byte/path helpers. Controller submodules use existing LocatedNativeSession, NativeSessionListing, ClaudeTranscript, SessionScanProgress and event-building helpers. Add direct Rust zstd dependency for DSH decoding/encoding; preserve concatenated native frame semantics. No archive schema migration is intended: native artifact paths normalize to the existing harness-home-relative layout. Keep the CLI stable IDs deepseek/muse and provide dsh as a CLI alias.

## Validation evidence

Live fixtures were isolated under /tmp/mj-native-import-validation-_aac2rdi; no raw native logs or credentials are committed. CLI imported Muse 01a0811d-8e89-7403-9c88-0eb6edb3dbd9 and DSH session-1c0fc22a-b491-4f95-8fe0-1a42324ee671. `mj-worker worker restore-checkpoint --spec` restored both to new workspace/app paths. Real ACP session/resume plus session/prompt recalled the prior token and wrote continued-marker.txt only at the destination. `live_muse_checkpoint_restore_relocates_native_context_and_preserves_queued_work` additionally exercised mj’s own ACP client through checkpoint and resume. The default-workspace lint command intentionally excludes the optional desktop crate.

Revision (2026-09-08): updated implementation outcomes, live format discoveries, and reproducible validation evidence; completed fresh-session TUI discovery and prepared the validated current-branch commit.

Final review (2026-09-08): clean managed npm installs passed for DSH, Codex, and Claude; DSH bundled ACP initialization/new/close and both execution policies passed. Final native-reader tests (9) and managed-harness tests (8, one optional test ignored) passed after lint corrections. All changes are confined to mj; muse-acp remains unchanged. Local Muse configuration is intentionally retained.
