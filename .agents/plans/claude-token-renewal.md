# Renew Claude quota credentials explicitly

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation progresses.

## Purpose / Big Picture

Mjolnir must renew an expired Claude access token without requiring the user to open Claude or sign in again. The quota reader currently runs `claude -p /usage`, whose successful exit does not guarantee token renewal. Use Claude's documented `auth login` refresh-token exchange instead, with the stored scopes and optional client identifier. Keep credentials inside the child environment and out of arguments and error messages.

## Progress

- [x] (2026-10-29) Confirmed the documented refresh-token environment variables and their implementation in installed Claude 2.1.312.
- [x] (2026-10-29) Implemented explicit exchange, coordination with Claude's refresh locks, and post-exchange credential verification.
- [x] (2026-10-29) Extended isolated regressions for exchange, concurrency, failure, cancellation, and legacy-lock interoperability; all 19 Claude tests pass.
- [x] (2026-10-29) Ran the complete controller suite and workspace Clippy; all tests pass after rerunning four unrelated readiness/resume timing failures serially.
- [ ] Commit and push master, then run `scripts/install.sh`.

## Surprises & Discoveries

Claude's normal refresh takes both `<profile>/.oauth_refresh.lock` and `<canonical-profile>.lock` using empty lock directories and modification-time heartbeats. Its explicit `auth login` exchange does not acquire those refresh locks. Mjolnir has multiple quota readers, so changing only the command would permit competing exchanges of a rotating refresh token.

The existing Kimi quota adapter already implements the same directory-lock protocol with different timing and paths. Reuse its implementation rather than adding another interpretation of this protocol.

## Decision Log

- Decision: Extract the existing directory-lock mechanism into a controller-private helper parameterized by path, heartbeat, stale age, and wait limit; retain Kimi's existing behavior.
  Rationale: Claude and Kimi need the same lock protocol, with provider-specific paths and timing. Claude 2.1.312 uses a 60-second stale window and a 5-second heartbeat.
  Date/Author: 2026-10-29, Codex.
- Decision: Reread credentials under the lock before exchanging and after the CLI finishes, and keep lock ownership with the supervised blocking process work.
  Rationale: Another process may have already renewed the token, and cancellation must not release the lock while its child can still rotate credentials.
  Date/Author: 2026-10-29, Codex.

## Context and Orientation

`mj-controller/src/claude_usage.rs` reads the profile's `.credentials.json`, queries Anthropic's quota endpoint, and tries renewal after expiry or an HTTP rejection. Its colocated tests use temporary profile directories, a local HTTP server, and a hand-written subprocess executor. `mj-controller/src/quota.rs` owns Kimi's existing directory lock and its tests are in `mj-controller/src/quota/tests.rs`. Shared subprocess execution is implemented by `mj-core/src/targets.rs`; use its cancellation guard and timeout, not raw child-process plumbing. Cargo is routed through mbx and its existing target directory must be retained.

## Plan of Work

First extract the directory lock without changing Kimi's paths or timing. Then change Claude's renewal to acquire both Claude locks, reread credentials, and run `claude auth login` with `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_REFRESH_TOKEN`, `CLAUDE_CODE_OAUTH_SCOPES`, and optional `CLAUDE_CODE_OAUTH_CLIENT_ID`. Never launch browser login when refresh metadata is unusable. Reuse a usable token that changed while waiting. Verify that a successful CLI actually leaves a usable replacement; retain recovery when a competing process renewed credentials despite a failed CLI.

## Milestones

The implementation milestone ends when the existing expiry and HTTP-rejection tests exercise the explicit exchange, and overlapping readers exchange once. The validation milestone ends with the controller suite and workspace Clippy passing in the dev profile. The delivery milestone ends with the change committed and pushed to origin/master and the requested installer succeeding.

## Concrete Steps

From the repository root run `cargo fmt --all -- --check`, focused `cargo test -p brokk-mj-controller claude_usage::tests`, the complete `cargo test -p brokk-mj-controller`, and `cargo clippy --all-targets -- -D warnings`. Run all Cargo tests outside the restricted sandbox. Tests retain temporary directories; any new binary exercise uses `--instance claude-token-renewal`. Commit only changed files, push to origin/master, then run `scripts/install.sh` outside the sandbox.

## Validation and Acceptance

An expired login with refresh metadata runs the explicit CLI exchange once and queries quota with the new access token. Concurrent readers reuse the replacement instead of spending its refresh token. The CLI receives the original scopes and optional client identifier. A zero exit without renewal reports a refresh failure. Existing CLI-output parsing and Kimi lock regressions continue to pass. The installer reports installed binaries and the new build version.

## Idempotence and Recovery

Lock waits and child execution are bounded. Release every acquired lock after the exchange, including failures, and let the next scheduled quota poll retry failures. Never manually rewrite live credentials or force live daemon replacement during testing. If upstream moves, fetch and merge before pushing, as required by AGENTS.md.

## Artifacts and Notes

The initial live diagnosis found a credential renewal at 00:37:09 UTC and successful quota polling at 00:38:26 UTC. This change removes dependence on opening Claude to trigger that renewal.

## Interfaces and Dependencies

Use existing Tokio filesystem/background-task support and `CancellableProcessExecutor`. Add no dependency or workspace crate. Keep directory-lock implementation private to the controller. Use Path and PathBuf for all lock paths.

## Outcomes & Retrospective

The 19 Claude tests pass, including explicit exchange, failure, concurrency, cancellation, and legacy-lock coordination. The complete controller run passed 2,193 tests and ignored 18, with four unrelated worker readiness/resume timeouts; those four all passed on a serial rerun in 6.14 seconds. Workspace `cargo clippy --all-targets -- -D warnings`, rustfmt, and diff checks passed. Push and installation are pending.

Plan created after verifying that the explicit CLI exchange needs external coordination with Claude's normal refresh path.

Implementation update: extracted the Kimi directory-lock code, serialized heartbeat against release, and transferred lock ownership into the blocking child execution so cancellation cannot expose a second exchange. Focused tests passed in the dev profile.

Validation update: completed the controller suite once and reran only its four failing timing tests serially. No unrelated source changes were needed. Workspace Clippy and formatting checks are clean.
