# In-place Move post-retirement login harvesting and authoritative native continuity (#1698, #1699, #1700)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository keeps its ExecPlan standard in `.agents/PLANS.md`; maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Three related ownership splits were surfaced during `#1673` (staged in-place Move / worker reload) review. First (#1700), the controller computes native session continuity via `Config::native_continuity` (requiring matching harness AND model service, clearing `native_session_id` on cross-service switches such as Codex OpenAI to Codex DeepSeek), but `mj-worker/src/acp/session.rs` recomputes continuity during `ReloadLaunchConfig` by comparing only `HarnessKind`, allowing the worker to resume the old native thread while the controller injected a cross-service handoff. Second (#1698), `mj-controller/src/controller/move_session.rs` harvests the copied Codex login before sending `RestartHarnessInPlace` and retiring the source ACP bridge; any rotating token refresh between that read and bridge exit is overwritten by the reload. Simply reordering in the controller cannot fix it because the worker's reload commit destroys the old staged home before returning. Third (#1699), lifecycle harvesting covers only Codex through a 186-line Codex-only SSH/container scraper in `subagent_park.rs`, discarding refreshed copied OAuth grants for Kimi, Grok, Muse, and Antigravity even though `mj-core::credentials` and `credential_sync.rs` already support generic freshness and locked `ProfileLogin` adoption for all five harnesses.

After this change, the worker's post-reap reload boundary is the single safe owner for preserving retired copied logins before destructive overlay, the daemon's existing `credential_sync` / `ProfileLogin` path adopts retired logins generically across all five refreshable copied harnesses (deleting the Codex-only scraper), and planned ACP reloads obey `launch.native_session_id` directly with zero duplicate continuity predicate. The combined guarantee is gated behind relay protocol 93 while retaining backward compatibility with pre-93 workers and existing persisted candidate/WAL formats.

## Progress

- [x] (2026-11-25 00:08Z) Mapped the `#1673` reload WAL, all harness credential shapes, `credential_sync` adoption, `subagent_park` Codex pull, `Config::native_continuity` callers, and protocol 92 baseline (`1096adabc`), and authored this ExecPlan.
- [ ] Implement `#1700` authoritative planned-reload continuity: compute `native_session_id` once when planning an in-place Move (persisting it in the candidate's `WorkerLaunchConfig.native_session_id`), remove repeated live-config continuity rechecks in `move_session.rs`, and remove the `HarnessKind`-only continuity predicate in `mj-worker/src/acp/session.rs` so planned `ReloadLaunchConfig` obeys `launch.native_session_id` directly (keeping unexpected same-config bridge recovery untouched).
- [ ] Implement `#1698` + `#1699` worker post-reap retired-login preservation and same-profile candidate reconciliation: fail closed if planned source bridge process-group reap fails; preserve the source copied login + immutable `{ profile_id, harness, operation_id, store_id }` into a private `0600` fsynced slot under the worker root before destructive `clear_paths` on both normal reload and startup WAL roll-forward; reconcile same-profile destination candidates against the preserved login using `mj-core::credentials` freshness rules before starting the replacement bridge.
- [ ] Implement daemon adoption + generic stopped-home harvest: add relay RPCs (behind protocol 93) to read and acknowledge/delete pending retired logins, drain them through the existing `credential_sync` / `ProfileLogin` locked `install_credentials_if_newer` path (nudged eagerly on Move completion, reconnect, and pre-destroy, and resumable across daemon restarts), generalize stopped-home harvesting in `subagent_park.rs` onto `mj-core` harness login descriptors, and delete the Codex-only `pull_staged_codex_login` scraper and Move's pre-retirement pull.
- [ ] Add targeted tricky-condition and cross-version compatibility tests, run strict Clippy and targeted suites in the foreground, and commit on the worktree's detached HEAD.

- [x] Worker planned reload now obeys `launch.native_session_id`; controller initializes the existing destination native ID when accepting an in-place Move and uses that frozen value when applying/recovering it. Extended the existing real Python-bridge reload test for same-harness explicit clearing.
- [x] Planned reload kill/reap errors now invalidate the restart result before destructive overlay.
- [ ] Remaining combined contract: protocol-93 gate/RPCs, durable retired snapshots and reconciliation, canonical adoption, generic stopped-home harvesting, full tricky-condition coverage. This checkpoint is not completion of #1698/#1699/#1700.

## Surprises & Discoveries

- Observation: Relay protocol 92 was recently claimed on `master` by `1096adabc` (`RELAY_SEARCH_HISTORY_PROTOCOL = 92`), so the new combined capability for this plan takes protocol 93.
  Evidence: `mj-core/src/relay.rs:224-226`.
- Observation: `ReloadCandidateManifest`, `ReloadSharedLogin`, and `PendingInPlaceReload` in `mj-core/src/worker_launch.rs:1278-1315` use `#[serde(deny_unknown_fields)]`. Adding fields to them requires explicit format/capability discipline so old workers never fail to parse extended manifests and new workers continue reading persisted v91 candidates/WALs.
  Evidence: `.mj/agents/1380cf0ecff96c072057123498bc2f75/1698_1699_1700_analysis.md` section "Continuity and compatibility".
- Observation: Planned bridge shutdown kill/wait errors in `mj-worker/src/acp.rs:960-1020` currently log warnings rather than returning errors, meaning destructive reload commit could proceed while a source bridge process is still alive and writing credentials.
  Evidence: `mj-worker/src/acp.rs:960-1020`.
- Observation: `mj-core/src/credentials.rs:255-415` and `mj-session-runtime/src/worker_client/credential_sync.rs:2926-3088` already implement generic freshness comparison and locked `ProfileLogin` adoption for Codex, Kimi, Grok, Muse, and Antigravity (while stripping refresh tokens only for Claude, treating OpenCode as push-only, and omitting DSH API keys).
  Evidence: `.mj/agents/1380cf0ecff96c072057123498bc2f75/1698_1699_1700_analysis.md` section "Harness credential matrix".

- Observation: Changing the existing replacement-bridge fixture from Claude to Codex requires advertising the `agent-full-access` ACP mode; otherwise the real startup path refuses the fixture before continuity can be asserted.
- Compatibility concern still to resolve: old persisted active Move operations have `destination_native_session_id = None` even when their original intent was continuous. The final protocol/operation recovery work must account for these records before release.

## Decision Log

- Decision: Do not add a separate continuity boolean to `ReloadLaunchConfig` or `RestartHarnessInPlace`; make the existing `WorkerLaunchConfig.native_session_id: Option<String>` authoritative for planned reloads and gate controller reliance on that guarantee behind relay protocol 93.
  Rationale: An independent boolean can disagree with `launch.native_session_id`. Making the existing `Option<String>` the single source of truth eliminates duplicate predicates while protocol 93 guarantees the worker honors it.
  Date/Author: 2026-11-25 / Coordinator.
- Decision: Keep retired-login preservation inside the worker root as private `0600` files (outside `clear_paths` and candidate cleanup) and drain them through the daemon's existing `credential_sync` / `ProfileLogin` locked `install_credentials_if_newer` path via two narrow relay requests (read/list pending retired logins and acknowledge/remove by ID), rather than carrying secret bytes in relay journal events or adding a second Move state machine.
  Rationale: Relay journals and checkpoints must never hold credential bytes. A durable private file on the worker survives daemon disconnects and restarts until acknowledged, and reusing `ProfileLogin` / `install_credentials_if_newer` keeps one freshness comparator and one canonical lock owner.
  Date/Author: 2026-11-25 / Coordinator.
- Decision: Do not strip refresh tokens for non-Claude harnesses in this change.
  Rationale: Only Claude has verified daemon-only refresh behavior (`#1490`/`#1491`); Codex, Kimi, Grok, Muse, and Antigravity have not been verified to work with stripped refresh tokens, and Grok/Antigravity have no proactive daemon refresher.
  Date/Author: 2026-11-25 / Coordinator.
- Decision: Gate the new combined contract behind `RELAY_RETIRED_LOGIN_AND_CONTINUITY_PROTOCOL = 93` while keeping `RELAY_MIN_PROTOCOL_VERSION = 1` and preserving pre-93 worker compatibility and pre-93 persisted candidate/WAL reads.
  Rationale: Matches `AGENTS.md` and `mj-controller/src/controller/move_session/in_place.rs:1-43`: older workers keep existing compatibility handling (and new workers still read older candidates/WALs cleanly) without raising the global minimum protocol.
  Date/Author: 2026-11-25 / Coordinator.

## Outcomes & Retrospective

Partial implementation checkpoint only. Do not ship as the combined protocol-93 contract. The relay remains protocol 92 until the durable credential-preservation/adoption implementation is present. Existing candidate and WAL formats are unchanged. Validation logs are in the assigned child report directory under `validation/`. All eight required filters passed: core credentials 40, core config 104, worker reload_commit 12, worker in_place_move 11, worker acp::session 23, session-runtime credential_sync 2, controller move_session 56, controller subagent_park 11. The extended real-bridge reload test passed separately (1 test) after adding the required Codex mode. Format check and the requested five-package Clippy check passed; a final worker-only Clippy pass covers the test extension. Commands used `mbx test`, `mbx clippy`, and `mbx fmt` (mbx's Cargo wrappers), dev profile, with elevated permissions. No daemon instance was launched.

## Context and Orientation

An in-place Move (`mj-controller/src/controller/move_session.rs` and `mj-controller/src/controller/move_session/in_place.rs`) switches a running session's profile, model, or settings without recreating its container or worker. The controller prepares a private candidate directory (`mj-worker-launch/src/worker_launch.rs:397-440`) containing a `ReloadCandidateManifest`, `launch.json`, `ownership.json`, and staged profile files, installs it on the target, and sends `RelayRequest::RestartHarnessInPlace` (`mj-core/src/relay/protocol.rs:325-337`) over the worker's relay socket. On the worker (`mj-worker/src/worker_runtime/unix.rs:1008-1240`), the request sends an internal `ReloadLaunchConfig` command to the ACP loop (`mj-worker/src/acp/session.rs:2029-2057`), which shuts down the current bridge (`mj-worker/src/acp.rs:960-1027`), emits `HarnessRestarting`, and hands off to `HarnessReloadTransition` (`mj-worker/src/worker_runtime/unix/dispatch.rs:1065-1188`) and `reload_commit.rs:122-337`, which fsyncs a `PendingInPlaceReload` WAL, runs `clear_paths`, copies the overlay, atomically replaces `ownership.json` and `launch.json`, and deletes the candidate. Startup recovery calls `roll_forward_pending_reload_if_any` (`mj-worker-bin/src/main.rs:711-725`).

Meanwhile, periodic credential convergence lives in `mj-session-runtime/src/worker_client/credential_sync.rs:2926-3088`, using `mj-core/src/config/harness.rs` for login file paths/schemes, `mj-core/src/credentials.rs` for JSON freshness rules, and `mj-core/src/credentials/locking.rs` for locked canonical `install_credentials_if_newer`.

## Plan of Work

### Part 1: `#1700` — Authoritative planned-reload native continuity

In `mj-controller/src/controller/move_session.rs`, compute native continuity (`Config::native_continuity`) once when planning the in-place Move and freeze that decision (and resulting destination `native_session_id: Option<String>` plus whether a cross-service handoff is needed) in the Move operation / prepared candidate `launch.json`, eliminating repeated re-evaluations against mutable live config at `:988`, `:2792`, and `:3705`. Verify every internal caller constructing `ReloadLaunchConfig` or staged `launch.json` sets `native_session_id` explicitly (preserving `Some(id)` when continuous and `None` when discontinuous).

In `mj-worker/src/acp/session.rs:2029-2057`, remove the `launch_config.harness == spec.harness` continuity guess during `ReloadLaunchConfig`. Instead, obey `launch_config.native_session_id.clone()` directly as the `resume_session` passed through `SessionRestart` -> `BridgeRestart` -> `spec.resume_session` (`mj-worker/src/acp.rs:841-842,1066-1100`). Do NOT alter unexpected same-config bridge crash recovery (`mj-worker/src/acp.rs:1108-1134`), which intentionally preserves `opened.native_session_id` without a `ReloadLaunchConfig`.

### Part 2: `#1698` + `#1699` — Post-reap retired-login preservation and same-profile reconciliation

First, tighten planned bridge retirement in `mj-worker/src/acp.rs:960-1020`: when retiring a bridge for a planned reload (`BridgeRestart` / `ReloadLaunchConfig`), if process-group kill or reap fails so a source writer might still be alive, return an explicit pre-commit reload failure rather than logging a warning and allowing destructive commit to proceed.

Second, define a narrow, shared login-file descriptor helper in `mj-core/src/credentials.rs` (built on existing `mj-core/src/config/harness.rs` login-file paths and `copies_omit_refresh_token` / auth-scheme rules) that answers for any `(HarnessKind, home_dir)` whether a staged home holds a harvestable regular-file copied login (covering Codex `auth.json`, Kimi `credentials/kimi-code.json`, Grok `auth.json`, Muse `auth.json`, Antigravity `antigravity-cli/antigravity-oauth-token`, plus legacy unstripped Claude `.credentials.json` carrying a refresh grant; skipping actual symlinks such as shared local Codex `auth.json`, stripped Claude copies, OpenCode push-only exports, DSH, and non-file/Keychain/env auth).

Third, integrate retired-login preservation into `mj-worker/src/worker_runtime/reload_commit.rs` (called from both normal `HarnessReloadTransition` in `unix/dispatch.rs` and `roll_forward_pending_reload_if_any` at worker startup):
- Before writing/applying destructive `clear_paths`, inspect the *source* `ownership.json` (which records the source `profile_id` and harness) and the source staged login file under the current live profile home.
- If a harvestable copied login regular file exists, atomically write a private `0600` retired-login record (e.g., under `<worker_root>/retired-logins/<operation_id>.json` via temp file + `chmod 0600` + fsync + rename + dir fsync) containing immutable metadata (`id`/`operation_id`, `profile_id`, `harness`, `login_relative_path`, and the captured credential bytes). Never overwrite an already-persisted retired-login record for the same `operation_id` on WAL replay, so a crash-recovery replay can never recapture destination bytes as source!
- Keep `<worker_root>/retired-logins/` completely outside `clear_paths` and candidate cleanup so neither overlay promotion nor `roll_forward_pending_reload_if_any` deletes it.
- Before launching the destination bridge, if the destination `ownership.json` has the **same `profile_id` and `harness`** as the preserved retired login and the destination staged login is a regular file (never following a shared local Codex symlink!), compare the preserved retired bytes against the candidate's staged login bytes using `mj-core::credentials` freshness comparison; if the retired bytes are strictly newer, update the promoted destination login file atomically (`0600` + fsync) so the replacement bridge does not start on a spent token.
- Preserve strict backward compatibility for `ReloadCandidateManifest` and `PendingInPlaceReload` (both have `#[serde(deny_unknown_fields)]`): avoid adding required fields to old formats, and if any optional field is needed, version or keep it strictly compatible so new workers still roll forward pre-93 WAL files and pre-93 candidates without deserialization errors.

### Part 3: Protocol 93 + daemon adoption via existing `credential_sync` + generic stopped-home harvest

In `mj-core/src/relay.rs` and `mj-core/src/relay/protocol.rs`, add `pub const RELAY_RETIRED_LOGIN_AND_CONTINUITY_PROTOCOL: u32 = 93;` and advance `RELAY_PROTOCOL_VERSION` to 93 (keeping `RELAY_MIN_PROTOCOL_VERSION = 1`). Add two narrow connection-only / non-journaled relay requests (never stored in relay journal events or checkpoint exports):
- `ReadRetiredLogins` (returns any pending `RetiredLoginSnapshot` entries from `<worker_root>/retired-logins/`, with `id`, `profile_id`, `harness`, and raw credential bytes).
- `AckRetiredLogin { id: String }` (removes the acknowledged retired-login file from `<worker_root>/retired-logins/` and fsyncs the directory; idempotent if already removed).

In `mj-session-runtime/src/worker_client/credential_sync.rs`, wire retired-login draining into the existing daemon `ProfileLogin` / `LoginAccess` owner:
- Provide a single adoption function that reads pending retired logins from a worker (when negotiated protocol >= 93), adopts each snapshot into its recorded `profile_id` using the existing locked `install_credentials_if_newer` / `ProfileLogin` adoption rules (handling macOS Keychain vs file automatically and preserving Claude grant rules), and sends `AckRetiredLogin { id }` only after canonical adoption succeeds (or deterministically decides no-op because canonical is already equal/newer, or the source profile no longer exists in config — logging clearly on unexpected adoption errors while leaving the file for retry).
- Invoke this drain eagerly right after in-place Move completes (`move_session.rs`), on worker reconnect / periodic `credential_sync` passes (so a daemon crash right after `RestartHarnessInPlace` still adopts the retired login on startup/reconnect), and before worker-root deletion on destroy (best-effort when worker is reachable).

In `mj-controller/src/controller/move_session.rs` and `mj-controller/src/controller/move_session/in_place.rs`:
- Delete the pre-retirement `pull_staged_codex_login` call at `move_session.rs:3685-3698` when talking to a protocol >= 93 worker (and check `in_place.rs` capability floor: require protocol >= 93 for in-place Move just as it currently requires >= 91, or keep clean explicit messaging for older workers consistent with `in_place.rs:1-43`).
- In `mj-controller/src/controller/subagent_park.rs:259-445`, replace the Codex-only `pull_staged_codex_login` with a generic stopped-home harvest helper driven by the shared `mj-core` harness login descriptor and locked `install_credentials_if_newer`, covering Codex, Kimi, Grok, Muse, Antigravity (and legacy unstripped Claude) while skipping symlinks, stripped Claude, OpenCode, and DSH — deleting the Codex-only special case.

## Concrete Steps

Work exclusively inside the detached worktree `/home/jonathan/Projects/mjolnir/.mj/clones/62f67a3f25ad58150b0b60eb2a982e94/.mj/worktrees/move-1698-1700`.

1. Inspect the exact current code in `mj-core/src/relay.rs`, `mj-core/src/relay/protocol.rs`, `mj-core/src/credentials.rs`, `mj-core/src/worker_launch.rs`, `mj-worker/src/acp.rs`, `mj-worker/src/acp/session.rs`, `mj-worker/src/worker_runtime/reload_commit.rs`, `mj-worker/src/worker_runtime/unix.rs`, `mj-worker/src/worker_runtime/unix/dispatch.rs`, `mj-session-runtime/src/worker_client/credential_sync.rs`, `mj-controller/src/controller/move_session.rs`, `mj-controller/src/controller/move_session/in_place.rs`, and `mj-controller/src/controller/subagent_park.rs`.
2. Implement Parts 1, 2, and 3 cleanly, ensuring Windows/non-Unix `#[cfg]` gates in `mj-worker` stay Clippy-clean!
3. Add targeted tricky-condition and protocol-integration tests:
   - Ordering race (#1698): source bridge writes a fresher token *after* quiescence / during retirement; verify post-reap preservation captures the fresher token, same-profile destination starts with the fresher token, and daemon `credential_sync` adopts it into canonical login and acknowledges deletion.
   - All five refreshable copied harnesses (#1699): verify Codex, Kimi, Grok, Muse, and Antigravity (plus legacy unstripped Claude) preserve and adopt generically, while local shared Codex symlinks, stripped Claude, OpenCode, and DSH are skipped. Verify generic stopped-home parking in `subagent_park.rs` covers non-Codex harnesses too.
   - Crash/replay safety: crash after retired-login snapshot + WAL commit before promotion, and after promotion before `AckRetiredLogin`; verify WAL roll-forward never overwrites the retired snapshot with destination bytes and daemon reconnect adopts + acks it.
   - Planned retirement failure: if source bridge reap fails, destructive overlay does not proceed.
   - Continuity (#1700): same-harness different-service reload (e.g. Codex OpenAI -> Codex DeepSeek) clears `resume_session` and obeys `launch.native_session_id = None`, while same-service preserves `Some(id)` and unexpected same-config crash restart still preserves active `native_session_id`.
   - Protocol/format compatibility: protocol 91/92 persisted candidate/WAL files still parse cleanly, and controller handles older (<93) workers explicitly without silently sending unhonored requests.
4. Run validation in the foreground (waiting for completion before handing back):

       cargo fmt --all -- --check
       cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime -p brokk-mj-worker-launch -p brokk-mj-session-runtime -p brokk-mj-controller --all-targets -- -D warnings
       cargo test -p brokk-mj-core --lib credentials::
       cargo test -p brokk-mj-core --lib config::tests::
       cargo test -p brokk-mj-worker-runtime --lib reload_commit
       cargo test -p brokk-mj-worker-runtime --lib acp::
       cargo test -p brokk-mj-session-runtime --lib credential_sync
       cargo test -p brokk-mj-controller --lib move_session
       cargo test -p brokk-mj-controller --lib subagent_park

5. Update this ExecPlan's living sections and commit all changes cleanly on the detached HEAD (staging only changed files). Do not push and do not create a branch.

## Validation and Acceptance

- A rotating token refreshed by Codex, Kimi, Grok, Muse, or Antigravity right up until bridge exit during an in-place Move is preserved before destructive overlay, reconciled into same-profile destination launches, adopted into the daemon's canonical profile store (surviving daemon restart until acknowledged), and removed once acknowledged.
- Switching a Codex session across model services (e.g. OpenAI to DeepSeek) via in-place Move never resumes the old native session ID on the worker while preserving same-service continuity and unexpected same-config restart recovery.
- Legacy pre-93 persisted candidate/WAL formats still deserialize and recover cleanly.

## Idempotence and Recovery

Retired-login preservation is write-once per `operation_id`, canonical adoption is freshness-monotonic (`install_credentials_if_newer`), and `AckRetiredLogin` is idempotent. Replaying any step after a daemon or worker crash produces the same canonical state with zero token loss.

## Artifacts and Notes

Detailed reconnaissance report is at `.mj/agents/1380cf0ecff96c072057123498bc2f75/1698_1699_1700_analysis.md`.

## Interfaces and Dependencies

In `mj-core/src/relay.rs`:

    pub const RELAY_RETIRED_LOGIN_AND_CONTINUITY_PROTOCOL: u32 = 93;
    pub const RELAY_PROTOCOL_VERSION: u32 = RELAY_RETIRED_LOGIN_AND_CONTINUITY_PROTOCOL;

In `mj-core/src/relay/protocol.rs`, add connection-only non-journaled `RelayRequest::ReadRetiredLogins` and `RelayRequest::AckRetiredLogin { id: String }` with matching `RelayResponsePayload` variants, gated on protocol >= 93.
