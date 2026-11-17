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
- [x] (2026-11-10) Correct the failed cases and review P1: reconcile retained newer destinations immediately, and carry immutable matched-login evidence into rejection tracking.
- [x] (2026-11-10) Pass final touched-crate suites (2,513 controller, 644 core including integration scenarios, 813 worker), workspace clippy, formatting and diff checks.
- [x] (2026-11-10) Obtain clean review through 3088800ef; merge upstream without conflicts and push implementation as d3c6a0775 to origin/master.
- [x] (2026-11-10) Dispatch macOS/Windows workflow run 42807206682 on delivered master; its asynchronous result is separate from the passed local validation.

## Surprises & Discoveries


The default daemon traces show generic Claude refresh failures for several profiles. They do not identify provider rejection reasons. The code intentionally treats refresh subprocess output as sensitive and currently reports only its exit status. No live credential contents or refresh requests were used in diagnosis. Concurrent default, s48-dsh, s49 and s48-judge traces on November 10 all reference configs selecting the same native profile homes. Twenty generic Claude refresh failures appeared across five roots within 72 hours; those observations do not prove token reuse. Adjacent network wording in two cases cannot be attributed as the refresh error because the refresh output itself is discarded.

Pinned Codex does perform a guarded reload before a forced refresh. Therefore discarding cached app-servers merely because the disk changed is not needed to prevent use of a previously cached refresh token in that RPC. The missing guarantee is serialization across processes from reread through network exchange and persistence. Native source snapshots are in `.mj/agents/d3667101abe4dd5c39547ca686237116/codex-0165/` and the primary source is `openai/codex`, tag `rust-v0.165.0`, `codex-rs/login/src/auth/manager.rs`, especially `refresh_token_with_reason`.

## Decision Log


Preserve successful native reload behavior and existing vendor-compatible Claude, Kimi and Muse locks. Do not add speculative client respawns or a global lock for unrelated logins. Identify synchronization by the actual credential slot rather than profile ID or daemon data directory, because multiple profiles and instances may point to the same source.

The initial investigation is read-only against live state. Regression tests must use synthetic credentials and isolated files; no login, restart, installation or token refresh against the person's accounts is authorized by this diagnostic step.

## Outcomes & Retrospective


The confirmed participating-writer races are corrected and delivered to `origin/master` at `d3c6a0775` (implementation commits `d8f21f06f`, `42710631d`, `3088800ef`). Clean review covered all implementation changes; a later upstream-only merge had no conflicts. Full dev-profile touched-crate tests, workspace clippy with confirmed exit 0, formatting and diff checks passed. The platform workflow was dispatched separately. No installation, daemon restart, live token refresh or account repair was performed.

The historical Claude outage cannot be attributed conclusively because its detailed refresh errors were discarded. New failures can expose only fixed safe categories. The fix coordinates participating Mjolnir operations and native Claude/Kimi/Muse refresh locks; arbitrary native Codex writers and explicit Claude sign-in remain outside those guarantees. Independent review exposed an important second ownership boundary: rejection tracking now consumes the exact unchanged matched snapshot, rather than rereading a file that may have refreshed before result delivery.

## Context and Orientation


`mj-controller/src/quota.rs::refresh_profile_with_cache_root` probes each profile's quota and refreshes expiring logins. Codex probes reuse an app-server process through `mj-controller/src/codex_usage.rs`. Its `account/read` RPC can request proactive refresh. `mj-controller/src/claude_usage.rs`, `kimi_auth.rs` and `muse_usage.rs` implement other provider paths, including native credential locks.

Before this change, `mj-controller/src/worker_client/credential_sync.rs` exchanged credential copies with workers (the separate processes running agent sessions). `SessionCanonical::read` snapshots the host credential before asynchronous worker calls. `reconcile_credentials` chooses a push or pull using `mj-core/src/credentials.rs::reconcile`; a pull writes the returned bytes without rechecking the host file. The worker's `InstallCredentials` handler in `mj-worker/src/worker_runtime/unix/requests.rs` also replaces its file unconditionally. Atomic file replacement avoids partial reads but does not establish which refresh owns the newest login.

`mj-core/src/file_lock.rs::FileLock` owns an operating-system file lock and explicitly unlocks it on release, including when a fork inherited a descriptor. Existing async polling lock patterns must keep blocking filesystem work off UI and server loops. Credential destination writes reject symlinks for security; do not weaken that boundary accidentally while resolving credential identity.

## Plan of Work


First complete the host and provider evidence. Settle a per-slot transaction boundary that includes the final reread, refresh or reconcile decision, and persistence. Every participating writer must share that boundary; a lock only around the refresh RPC or a freshness check immediately before an unlocked write is insufficient. Keep independent credential slots concurrent. Document native harness writers outside an mj-owned lock as a limit unless the implementation shares their own lock protocol.

Delegate coherent implementation slices after assigning exact API ownership. Add regressions that drive real subprocess or relay boundaries and control the interleaving so a second daemon cannot refresh from the same snapshot or replace a newer credential after waiting. Reuse existing tests for ordinary quota queries and credential push/pull; do not add ordinary happy-path unit examples.

## Concrete Steps


Work from the repository root. Preserve ordinary mbx-managed Cargo storage and package selection. Run `cargo fmt --all -- --check` and `git diff --check`. Every `cargo test` must run with elevated sandbox permissions. During implementation run only new and affected tests. After the last round run the full suites for touched crates and `cargo clippy --all-targets -- -D warnings` once. Use stable selection `-p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker` for every focused and final test command so mbx does not rebuild differing feature graphs. Regression filters will be recorded after implementation. A dedicated validation child owns foreground test runs and their logs.

Commit only task-owned files on the current branch, request the repository's push review, resolve material findings, and push to `origin/master`. Merge upstream if it moved; never rebase or change branches. Dispatch the nightly workflow if the delivered change is platform sensitive.

## Validation and Acceptance


Synthetic isolated competing refreshers must not submit the same old grant, a delayed credential pull must preserve a host credential refreshed meanwhile, and unrelated homes must progress independently. The concurrency regressions exercise interleavings that the prior unguarded code could not preserve; the final runs pass. A separate automated run of all regressions against the old source was not performed. Native lock compatibility and cancellation must be tested if those mechanisms change. Successful tests prove those controlled interleavings, not the cause of the past live outage or a multi-day live observation.

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

Final validation and delivery revision (2026-11-10): the full touched-crate command was `mbx test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker`; all suites passed. `mbx clippy --all-targets -- -D warnings` completed with confirmed exit 0 (a cached repeat resolved the first wrapper losing its exit status), and `mbx fmt --all -- --check` passed. Decisive local logs are `.mj/agents/8de4033e4019b0a364525fdb1b3eccc0/full-touched-crates.log` and `workspace-clippy-final.log`; platform run is https://github.com/BrokkAi/mjolnir/actions/runs/42807206682. Code is pushed; this documentation-only completion record publishes the finished plan without changing the validated implementation.

## #1490: the daemon becomes the only Claude refresher

Progress (2026-11-10): implemented, validation recorded below.

Cause. #1485 serialized writers of one home, but Claude Code refreshes its own copy five minutes before `expiresAt` under a lock that covers only its own config directory, and every staged copy carried the rotating refresh token. Two copies, or a copy and the host, spent the same single-use token, and a refresh inside a copy nobody pulled back left the host holding a spent one.

Decisions.
- Every Claude copy handed to a harness omits `claudeAiOauth.refreshToken` (the key is removed; `""` means "login ended" to Claude Code, so it is never written and an existing `""` is left alone). `credential_copy_for_harness` in `mj-core/src/credentials.rs` is the one interpretation. Staging (`stage_profile`, which sessions, reviewers, sub-agents and probes all use, and the macOS Keychain stage) and every sync push call it. Other harnesses are unchanged.
- Sync: the desired copy is the stripped host login (`session_copy_action` in `worker_client/credential_sync.rs`). A session copy is pulled only when it is strictly newer than the host and holds a refresh token (an in-session `/login`, or a pre-upgrade copy that refreshed itself); the adopted login is then pushed back stripped. A copy without a refresh token never replaces the host, enforced again under the destination lock by `install_credentials_if_newer` (policy `Canonical`). Otherwise the copy is pushed whenever it differs from the desired bytes.
- Worker delivery uses `install_session_credentials` (policy `SessionCopy`): newer copies install, and so does a same-expiry copy without a refresh token, so pre-upgrade copies converge at equal `expiresAt`.
- Mixed versions: no protocol change. An old worker keeps the newer-only rule, so a same-expiry stripped push is declined; the controller treats that as a debug-level non-event (only when the expiry is equal), and the copy converges at the next daemon refresh, whose later expiry the old worker accepts, because that delivery is stripped as well. An old daemon with a new worker still pushes its full copy, which the worker takes only if newer. Until a worker is replaced, its Claude Code may still refresh a copy that kept its token; the new daemon pulls that (strictly newer, holds a token) and pushes it back stripped.
- The daemon refreshes the host grant when under `GRANT_REFRESH_LEAD` (4 h, half of the observed 8 h access lifetime) remains, under the native lock with a reread (`claude_usage::refresh_grant_if_due`, `quota::refresh_claude_grant`). The quota poller (`pollers/quota.rs`) owns the decision, runs it before each probe cycle and independently of the usage API's rate-limit hold, and wakes at the due time; a failed refresh waits one probe interval. On macOS the same refresher renews the Keychain login (#1491, below).
- Logging: sync logs direction, profile, session and old/new `expiresAt` (no token bytes); a daemon refresh logs the profile and the new `expiresAt`; failures keep the existing safe categories.

macOS (#1491): the host login lives in the Keychain, not in the profile home, and the design applies there too. `HarnessKind::login_store(host, home)` (`mj-core/src/config/harness.rs`) is the one owner of where a profile's login is: a file at the marker, or the Keychain item `Claude Code-credentials` with Claude Code's locks under `~/.claude`. Credential sync, the grant refresher, staging and the authentication check all ask it. `copies_omit_refresh_token` is true on every host.
- The pinned 2.1.321 bundle names the Keychain item per config directory (`Claude Code-credentials-<sha256(dir)[:8]>` when `CLAUDE_CONFIG_DIR` is set, plain otherwise), reads the item first and then `<config dir>/.credentials.json`, and takes its refresh locks per config directory (`~/.claude` when unset). mj runs profile commands on macOS (`mj login`, quota probes, the grant renewal) with the variable unset, so every macOS Claude profile with a native login shares the one plain item and the `~/.claude` locks. Per-profile Keychain items are out of scope.
- Every session runs with `CLAUDE_CONFIG_DIR` set to its staged home, so Claude Code never sees the plain item there. Staging therefore writes the Keychain login as a stripped `.credentials.json` for every target, local bare ones included (before, local macOS sessions had no login at all). Claude Code's own SDK does the same when it resumes a session on macOS.
- The Keychain item is the only host login for Claude on macOS. A `.credentials.json` there is not a login: a profile home's (earlier releases pulled session copies into it), or `~/.claude/.credentials.json`, which Claude Code writes only when its Keychain write fails for good. Such files are ignored by staging, sync, the refresher and the authentication check, and a Mac without the item shows the profile as signed out (`mj login` fixes it). Staging makes the session's `.credentials.json` exactly the stripped Keychain login, or removes the copy `stage_profile` made from the profile home when the Keychain has none (or is locked).
- Sync reads the Keychain once per pass and pushes stripped copies as on Linux. It also pulls as on Linux (#1491 review): a pre-upgrade container or remote session, or an old worker mid-rollout, still holds a full copy; when it refreshes, Anthropic rotates the shared single-use refresh token and the newer one exists only in that session. Only a strictly newer copy that holds a refresh token is adopted (`canonical_login_is_replaced_by`, the rule of `InstallPolicy::Canonical`), and the session gets the stripped copy back. The write holds Claude Code's `~/.claude` locks, rereads the Keychain under them, and goes to `security -i` on stdin (`add-generic-password -U -a <account> -s "Claude Code-credentials" -X <hex>`), never into argv, with Claude Code's own account rule (`$USER` if it matches `^[a-zA-Z0-9._-]+$`, else `claude-code-user`) so the reader and writer address the item Claude Code uses (`claude_login::write_claude_keychain_login`, read back to verify). A Keychain write that fails, such as a locked one, is a failed harness-login step; it never falls back to a file. A locked Keychain (`security` exit 36) is also a failed harness-login step on read, not "no login".
- The refresher renews the Keychain login under Claude Code's `~/.claude` locks, rereads under the lock, runs the managed `claude auth login` with `CLAUDE_CONFIG_DIR` removed from the child's environment, and rereads the Keychain to check the access token changed. It never uses a container for this (the image cannot write the Keychain). A Keychain it cannot read starts no exchange and is reported as a failure, because spending the refresh token while the login cannot be saved would lose it.
- Needs a Mac to confirm: that `claude auth login` with `CLAUDE_CODE_OAUTH_REFRESH_TOKEN` and the variable unset updates the plain item in place, that `security find-generic-password -a <account> -w` raises no prompt for the daemon's process, and that `security -i` with `add-generic-password -U -a <account> ...` updates Claude Code's item in place (no second item, and Claude Code still reads it) when the daemon's `$USER` matches Claude Code's.

Out of scope: setup-token keying by home, Codex, any probe that hunts workers for fresher tokens.
