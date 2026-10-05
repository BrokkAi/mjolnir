# Add GitHub App credentials

This ExecPlan is a living document and follows `.agents/PLANS.md`. Keep its progress, discoveries, decisions, and retrospective current as implementation proceeds.

## Purpose / Big Picture

After this change, a Mjolnir controller can use a configured GitHub App to authenticate repository clones and pushes without placing the App private key or installation tokens in session checkpoints or the database. Owners may map directly to installation IDs or let the controller discover installations through GitHub. A session using more than one installation is rejected before creation. Operators and CI scripts can run `mj github-token --owner LOGIN` or `mj github-token --repo OWNER/REPO` to obtain a currently valid token through the daemon's shared cache.

The visible proof is a successful token command against an App installation, an actionable error when App configuration is absent, and tests showing token signing, refresh, lookup, session selection, and live relay delivery.

## Progress

- [x] (2026-10-05) Read repository instructions, the investigation note, and `.agents/PLANS.md`; confirmed the clean `master` checkout and its upstream lag.
- [x] (2026-10-05) Add optional `[github.app]` configuration and validate/serialize it without changing behavior when absent.
- [x] (2026-10-05) Add the controller-owned JWT/token provider, installation lookup, cache, and focused tests.
- [x] (2026-10-05) Integrate installation selection with session creation, provisioning, resume, credential sync, and export/token API behavior.
- [x] (2026-10-05) Add `mj github-token` and user-facing configuration/setup documentation.
- [x] (2026-10-05) Run all touched crate tests, `mbx clippy --all-targets -- -D warnings`, and `mbx fmt --all -- --check`; fix findings.
- [x] (2026-10-05) Review the final diff and commit the validated changes on the current branch; report the commit SHA in the agent handback.

## Surprises & Discoveries

- Observation: relay messages already support installing and removing a worker's GitHub token, and the worker reads the atomically replaced token file on each new `gh` or Git credential invocation.
  Evidence: `mj-controller/src/worker_client/credential_sync.rs` and the investigation note in `/home/jonathan/Projects/bifrost-ci-monitor/.mj/agents/5c1d9cd79473cb9278cfb742a166dad4/github-token-investigation.md`.
- Observation: controller token resolution currently has a context-free `controller_github_token()` and is repeated in provisioning, resume, and periodic credential reconciliation; the API already exposes typed request/response shapes and a daemon backend trait.
  Evidence: `mj-controller/src/controller/backend.rs`, `mj-controller/src/controller/resume.rs`, `mj-controller/src/worker_client/credential_sync.rs`, and `mj-controller/src/server/api/subagent_backend.rs`.
- Observation: adding an optional config field also requires updating hand-written `Config` literals in the TUI test support; workspace clippy identified the two complete literals.
  Evidence: `mj-tui/src/dialogs/tests.rs` and `mj-tui/src/test_support.rs`.

## Decision Log

- Decision: use the existing `ring` crypto implementation and `base64` crate for RS256 JWT signing and PEM decoding, instead of adding a JWT library. The workspace already uses ring through rustls and has base64 as a direct dependency.
  Rationale: this avoids a second crypto implementation and a new JWT dependency while keeping JWT claims and signing explicit and testable.
  Date/Author: 2026-10-05, Codex sub-agent.
- Decision: retain the existing environment/`gh auth token` lookup exactly when `[github.app]` is absent; App selection is based on the configured session bundle's GitHub owners and resolves all repository installations before accepting a multi-repository session.
  Rationale: this preserves existing setups while enforcing the decided one-installation-per-session v1 boundary.
  Date/Author: 2026-10-05, Codex sub-agent.
- Decision: serialize the decimal App ID as a string in the JWT `iss` claim.
  Rationale: GitHub's JWT examples encode `iss` as a string, including when it contains an identifier.
  Date/Author: 2026-10-05, Codex sub-agent.

## Outcomes & Retrospective

The controller now signs short-lived App JWTs, discovers or uses configured installation IDs, and shares cached installation tokens across provisioning, resume, periodic staged-worker sync, export, and the daemon token API. Session creation rejects bundles spanning multiple installations. The CLI token command uses that shared API, and documentation covers setup, CI usage, the one-installation limit, and local-bare token expiry. No database migration was needed; private keys and tokens remain outside persisted session state.

Validation passed on the final source: `mbx test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-client -p brokk-mjolnir -p brokk-mj-tui -- --test-threads=8`, `mbx clippy --all-targets -- -D warnings`, `mbx fmt --all -- --check`, and the docs `npm run check`. The four resume tests changed for the async token path and two timing-sensitive daemon tests also passed in isolated runs. The initial full test attempt exposed sync tests calling the async API through a helper without a Tokio runtime; those tests now run with Tokio. Its two daemon timing failures passed on isolated rerun. The JWT signing test verifies the RS256 signature and string issuer claim.

## Context and Orientation

`mj-core/src/config.rs` owns the resolved user configuration and its serialized TOML shape. `mj-controller/src/controller/backend.rs` currently resolves the host's GitHub token; `provisioning.rs` and `resume.rs` consume it. `worker_client/credential_sync.rs` reconciles tokens into staged workers every 60 seconds. That existing relay path is the transport for renewed App tokens. The HTTP API lives under `mj-controller/src/server/api`; its `SubagentBackend` trait keeps expensive controller work off the Axum request loop. CLI parsing is in `mj-cli/src/main.rs`, typed API calls are in `mj-cli/src/api_client.rs`, and one-shot command behavior is in `mj-cli/src/api_commands.rs`. Human documentation is under `docs/src/content/docs`.

An installation access token is a short-lived GitHub credential for an App installation. The App JWT is a signed, short-lived proof that the controller owns the App's private key. Neither credential may be persisted in checkpoints or the database. A bundle is the configured set of repositories copied into one session.

## Plan of Work

Add a default-empty GitHub configuration section to the resolved and stored config types. The `[github.app]` table contains an App ID, a controller-host PEM path, and an owner-to-installation-ID map. Validate owner keys and positive IDs, and keep older configs valid.

Add a controller module that loads/parses RSA PEM keys, mints RS256 App JWTs with `iat` about 60 seconds in the past and `exp` no more than ten minutes ahead, resolves installation IDs from configuration or GitHub's repository/owner installation endpoints, and exchanges App JWTs for installation tokens. Cache installation IDs by owner and access tokens by installation ID; attempt refresh when fewer than ten minutes remain. Do not include credentials in errors or logs. Use injectable time and HTTP construction for tests.

Add a session-aware resolver that returns the legacy controller token if App credentials are absent. With App credentials, collect GitHub owner/repository pairs for the session bundle, resolve each installation, and reject the bundle if those repositories require different installations. Use the selected token for provisioning (including host Git cache setup), resume, and each session's periodic credential reconciliation. Preserve current local bare behavior: do not add it to periodic sync, and document that its App token can expire after at most one hour. Existing worker token files already supply the current credential to session export operations.

Add authenticated daemon API support for minting a token by owner or repository. Run config reads, installation lookup, and HTTP requests through supervised background work. Add the `mj github-token` CLI command with a required choice of `--owner` or `--repo`, then document App setup, minimum permissions, the session installation limit, the bare-session expiry limit, and the command.

Add behavior tests for default config compatibility, JWT claims/signature, lookup with mocked HTTP, cache and refresh timing, multi-installation rejection, relay delivery of a refreshed token, and the command/API behavior. Keep any new fixtures and tests within the owning crates.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir` on the existing `master` branch. Do not update from upstream, create a branch, or redirect Cargo output. Use the repository's `mbx` wrapper for Rust builds and tests. After implementation, run the full test suites for touched crates, then `mbx clippy --all-targets -- -D warnings` and `mbx fmt --all -- --check`. Review the diff and stage only files changed for this task before committing.

## Validation and Acceptance

The existing configuration fixture suite must continue to read and write configs without `[github.app]` unchanged. New provider tests must prove JWT timestamps and RSA signature validity, configured and discovered installations, token reuse above the refresh threshold, and refresh below it. Bundle validation must name the conflicting owners/installations and refuse a multi-installation session before its creation action is accepted. Credential-sync tests must observe a refreshed token delivered through the existing worker relay. API/CLI tests must show that token output is obtained from the daemon and that an absent App config produces a clear error. The touched crate suites, workspace clippy, and format check must all pass before commit.

## Idempotence and Recovery

Configuration changes are additive and require no database migration. The App private key remains a controller-host file and must never be copied into a worker or archive. Test invocations use isolated named instances or isolated test data as required by `AGENTS.md`. If a validation fails, fix the owning implementation and rerun the failed focused test before the final full validation. If commit preparation is interrupted, inspect `git status` and stage only this task's reviewed files.

## Artifacts and Notes

The background investigation, including the exact existing token-sync and worker-file paths, is at `/home/jonathan/Projects/bifrost-ci-monitor/.mj/agents/5c1d9cd79473cb9278cfb742a166dad4/github-token-investigation.md`.

## Interfaces and Dependencies

Expose a typed optional GitHub App config through `mj_core::config::Config`. In `mj-controller`, provide a controller-owned provider with operations equivalent to `token_for_repo(owner, repo)`, `token_for_owner(owner)`, and `installation_for_bundle(bundle)`. Keep the private key, HTTP transport, clock, and token cache inside the controller provider. Extend the existing daemon backend/API contract rather than letting the CLI read controller credentials directly. The only crypto addition is a direct workspace dependency on the already-used `ring` crate; use its RSA PKCS#1 SHA-256 signer and the existing `base64` crate for JWT encoding and PEM decoding.
