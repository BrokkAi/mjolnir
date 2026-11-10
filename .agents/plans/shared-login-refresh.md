# Coordinate shared logins across Mjolnir daemons

This ExecPlan is a living document maintained under `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


Logins should remain usable when several Mjolnir background servers (daemons) share a native harness home. OAuth refresh tokens may rotate on use: two independent refreshes can spend the same old token, and copying an older credential file over a newer one can discard the only usable token. Issue #1485 tracks the user's report that logins become unavailable a few days after signing in. Diagnose the observed provider failures and make the proven concurrency defects impossible between participating Mjolnir operations, without touching live logins during investigation.

## Progress


- [x] (2026-11-10) Claim #1485 and delegate code, native Codex, and host evidence investigations.
- [x] (2026-11-10) Confirm a stale credential pull can overwrite a newer host file after an awaited worker request.
- [x] (2026-11-10) Verify pinned Codex 0.165.0 reloads credentials before refresh but has only an in-process semaphore, permitting simultaneous exchanges by independent daemons.
- [x] (2026-11-10) Confirm concurrent default/s48-dsh/s49/s48-judge traces use the same Claude homes. Claude native command diagnosis confirms refresh mode is supported; historical failure reason was discarded.
- [x] (2026-11-10) Settle destination-owned conditional installation under native refresh locks and a shared Codex transaction lock.
- [x] (2026-11-10) Implement shared guard, refresh consumers and asynchronous relay installs with isolated regressions.
- [x] (2026-11-10) Integrated review corrections preserve authoritative OpenCode exports and unconditional credential-symlink refusal; parent fixed a remaining test-helper argument mismatch after the sync handback.
- [x] (2026-11-10) Add safe Claude HTTP-rejection, DNS, connection-refusal and native-policy failure categories without echoing captured output.
- [x] (2026-11-10) First focused run compiled and passed 117 controller cases; three failures exposed fixture/result expectations and a subprocess-start issue.
- [ ] Correct the failed cases and review P1: reconcile retained newer destinations immediately, and carry immutable matched-login evidence into rejection tracking.
- [ ] Run final touched-crate suites, workspace clippy and formatting; review, commit and push.

## Surprises & Discoveries


The default daemon traces show generic Claude refresh failures for several profiles. They do not identify provider rejection reasons. The code intentionally treats refresh subprocess output as sensitive and currently reports only its exit status. No live credential contents or refresh requests were used in diagnosis. Concurrent default, s48-dsh, s49 and s48-judge traces on November 10 all reference configs selecting the same native profile homes. Twenty generic Claude refresh failures appeared across five roots within 72 hours; those observations do not prove token reuse. Adjacent network wording in two cases cannot be attributed as the refresh error because the refresh output itself is discarded.

Pinned Codex does perform a guarded reload before a forced refresh. Therefore discarding cached app-servers merely because the disk changed is not needed to prevent use of a previously cached refresh token in that RPC. The missing guarantee is serialization across processes from reread through network exchange and persistence. Native source snapshots are in `.mj/agents/d3667101abe4dd5c39547ca686237116/codex-0165/` and the primary source is `openai/codex`, tag `rust-v0.165.0`, `codex-rs/login/src/auth/manager.rs`, especially `refresh_token_with_reason`.

## Decision Log


Preserve successful native reload behavior and existing vendor-compatible Claude, Kimi and Muse locks. Do not add speculative client respawns or a global lock for unrelated logins. Identify synchronization by the actual credential slot rather than profile ID or daemon data directory, because multiple profiles and instances may point to the same source.

The initial investigation is read-only against live state. Regression tests must use synthetic credentials and isolated files; no login, restart, installation or token refresh against the person's accounts is authorized by this diagnostic step.

## Outcomes & Retrospective


Investigation has established two concurrency defects, but their causal link to the user's observed outages remains unproven. Shared ownership and destination-owned installation are implemented. Independent review found and corrected the symlink no-write case. Integrated review corrected OpenCode exports and a worker test-helper mismatch. Claude diagnostics now classify only exact native/Axios-shaped messages into fixed text; unknown messages retain the generic exit status. The secret-output regression covers both streams, token-bearing request dumps and invalid UTF-8. The first focused run passed 117 controller cases and failed three; core and worker tests were not reached. Early review on d8f21f06f found that a skipped stale install could produce an empty action result and cause rejection tracking to label a newer login refused. This is being corrected before rerunning the failing cases.

## Context and Orientation


`mj-controller/src/quota.rs::refresh_profile_with_cache_root` probes each profile's quota and refreshes expiring logins. Codex probes reuse an app-server process through `mj-controller/src/codex_usage.rs`. Its `account/read` RPC can request proactive refresh. `mj-controller/src/claude_usage.rs`, `kimi_auth.rs` and `muse_usage.rs` implement other provider paths, including native credential locks.

`mj-controller/src/worker_client/credential_sync.rs` exchanges credential copies with workers (the separate processes running agent sessions). `SessionCanonical::read` snapshots the host credential before asynchronous worker calls. `reconcile_credentials` chooses a push or pull using `mj-core/src/credentials.rs::reconcile`; a pull writes the returned bytes without rechecking the host file. The worker's `InstallCredentials` handler in `mj-worker/src/worker_runtime/unix/requests.rs` also replaces its file unconditionally. Atomic file replacement avoids partial reads but does not establish which refresh owns the newest login.

`mj-core/src/file_lock.rs::FileLock` owns an operating-system file lock and explicitly unlocks it on release, including when a fork inherited a descriptor. Existing async polling lock patterns must keep blocking filesystem work off UI and server loops. Credential destination writes reject symlinks for security; do not weaken that boundary accidentally while resolving credential identity.

## Plan of Work


First complete the host and provider evidence. Settle a per-slot transaction boundary that includes the final reread, refresh or reconcile decision, and persistence. Every participating writer must share that boundary; a lock only around the refresh RPC or a freshness check immediately before an unlocked write is insufficient. Keep independent credential slots concurrent. Document native harness writers outside an mj-owned lock as a limit unless the implementation shares their own lock protocol.

Delegate coherent implementation slices after assigning exact API ownership. Add regressions that drive real subprocess or relay boundaries and control the interleaving so a second daemon cannot refresh from the same snapshot or replace a newer credential after waiting. Reuse existing tests for ordinary quota queries and credential push/pull; do not add ordinary happy-path unit examples.

## Concrete Steps


Work from the repository root. Preserve ordinary mbx-managed Cargo storage and package selection. Run `cargo fmt --all -- --check` and `git diff --check`. Every `cargo test` must run with elevated sandbox permissions. During implementation run only new and affected tests. After the last round run the full suites for touched crates and `cargo clippy --all-targets -- -D warnings` once. Use stable selection `-p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker` for every focused and final test command so mbx does not rebuild differing feature graphs. Regression filters will be recorded after implementation. A dedicated validation child owns foreground test runs and their logs.

Commit only task-owned files on the current branch, request the repository's push review, resolve material findings, and push to `origin/master`. Merge upstream if it moved; never rebase or change branches. Dispatch the nightly workflow if the delivered change is platform sensitive.

## Validation and Acceptance


Synthetic isolated competing refreshers must not submit the same old grant, a delayed credential pull must preserve a host credential refreshed meanwhile, and unrelated homes must progress independently. Tests must fail before the correction and pass afterward. Native lock compatibility and cancellation must be tested if those mechanisms change. Successful tests prove those controlled interleavings, not the cause of the past live outage or a multi-day live observation.

## Idempotence and Recovery


No live credential repair is part of diagnosis. Repeating isolated tests is safe. Failed or cancelled transactions must release locks and preserve a complete credential file. Never remove a lock file while another process may retain its inode. Do not install a build or restart live daemons without a user instruction covering installation.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1485. Discovery handbacks reside in `.mj/agents/`; these are local evidence, not product documentation. The final plan will retain concise results so it remains useful without those temporary files.

## Interfaces and Dependencies


Reuse existing credential snapshots, native lock protocols, async background task supervision, shared subprocess helpers and `FileLock`. Add no workspace crate, database migration, protocol revision, retry budget, concurrency cap or timeout without concrete necessity. Expose `mj_core::credentials::CredentialWriteGuard::acquire(kind: HarnessKind, marker: &Path) -> Result<Self>` as an async constructor. It resolves the canonical home and takes the existing native lock protocol for Claude (two proper-lockfile directories), Kimi (OAuth sentinel plus proper-lockfile directory) and Muse (OS file lock), or an mj-owned sidecar file lock for Codex and remaining file-based harnesses. Native timing parameters retain their existing values; the new Codex lock has no guessed acquisition deadline.

Expose async `install_credentials_if_newer(kind: HarnessKind, path: &Path, bytes: &[u8]) -> Result<CredentialInstall>`, where `CredentialInstall` reports the actual destination `snapshot: CredentialSnapshot` and whether `installed: bool`. It validates input, acquires destination ownership, rereads, and replaces only when `reconcile(current, incoming) == SyncAction::Pull`. Preserve generic symlink rejection. Database-backed authoritative exports remain outside this conditional file installation policy.

Codex quota takes its guard before rereading expiry and holds it through the quota query, because queries may themselves refresh. Lock queue time does not consume the existing RPC timeout. Native refreshers use the shared guard without nesting their previous locks. The controller applies conditional installation to actual pulled bytes and consumes actual push response snapshots. The worker routes file installation outside the relay-state lock so waiting for a native refresher cannot block relay progress. No lock spans a network request to the other credential endpoint.

The new Codex lock coordinates participating mj operations only: independent native Codex processes do not take it. Claude explicit login also has policy checks after exchanging a token and before persisting it; this separate native failure mode is not established as the cause of the reported outage and is not being rewritten here.

Initial revision (2026-11-10): capture confirmed races, evidence limits and ownership requirements before implementation.

Design revision (2026-11-10): specify shared guard and destination-owned conditional installation APIs, preserve native protocols, and scope guarantees to participating writers. Native Codex guarded reload makes speculative client replacement unnecessary.

Evidence revision (2026-11-10): confirm concurrent daemon instances sharing native login homes; retain the distinction between code-proven races and the unproven historical cause. Assign one validation coordinator and stable package selection.

User clarification and integration revision (2026-11-10): Claude is worst affected, OpenAI is second, and Kimi usage is too sparse to judge. Add finite allowlisted Claude error categories without logging raw output, so future failures are distinguishable. Preserve authoritative OpenCode empty exports and reject marker symlinks even when reconciliation would skip a write.

Diagnostics revision (2026-11-10): finish safe fixed-text Claude categories and begin focused elevated validation. Preserve sensitive-output handling; do not claim to recover old discarded errors.

Review revision (2026-11-10): the user requested review concurrently with tests, authorizing candidate commit d8f21f06f before test completion. Accept review P1. Retry reconciliation only after an actual snapshot transition; carry the original unchanged matching snapshot in session outcomes and record that fingerprint, never whichever file happens to be current when the result is consumed. This removes the rejection-observation race as well as the stale-install empty-result case. No action or timestamp is fabricated to suppress rejection.
