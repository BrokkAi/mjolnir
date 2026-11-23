# Unify Daemon Transports into One HTTP API and Finish TUI Daemon Routing (#1043, #1044)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository keeps its ExecPlan standard in `.agents/PLANS.md`; maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir's daemon currently maintains three overlapping client transports for the same underlying control-plane operations: a custom length-prefixed TCP protocol (`DaemonAction`, 139 variants in `mj-client/src/daemon.rs` and `mj-daemon/src/daemon/actions.rs`), a cookie-only `/api/*` HTTP surface for the web viewer (`mj-server/src/server/routes.rs`), and a bearer-or-cookie `/api/v1/*` HTTP surface for CLI commands, subagents, and newer browser flows (`mj-server/src/server/api/routes.rs`). In addition, while the TUI no longer opens or migrates SQLite directly (the original hazard in `#1043`), the dashboard in `mj-cli/src/dashboard/session_state.rs` still runs two local `Controller` process probes (`session_working_context` and `session_git_status`), and `mj-cli` still imports shared data types from `mj-controller`.

After this plan is complete, Mjolnir has one HTTP control-plane API under `/api/v1/*` served unconditionally on a local loopback listener whenever `mj daemon-run` is running (even when `[phone].enabled` is false or Tailscale TLS is unavailable), while the optional external/Tailscale phone listener mounts the exact same Axum router. The legacy `/api/*` route group and duplicate HTTP handlers are deleted. Plain request/response, upload, streaming, and interactive client flows migrate from custom TCP frames onto `/api/v1/*`, leaving the framed TCP listener with only the five frozen cross-version management actions (`Ping`, `Status`, `Stop`, `PrepareUpgrade`, `UpgradeBlockers`) needed to upgrade already-running older daemons. The dashboard runs no local `Controller` probes. Every milestone leaves the codebase measurably smaller and simpler than before, and paves a single `/api/v1/sessions/{id}/attachments` upload path for both TUI and Web ahead of `#935`.

## Progress

- [x] (2026-11-23 16:40Z) Audited `#1043` and `#1044` against `master` (`723305327`), verified `DaemonMetadata`/`DaemonStatus` `deny_unknown_fields` constraint, `[phone].enabled` HTTP server coupling, and `/api/*` vs `/api/v1/*` overlap.
- [x] (2026-11-23 16:50Z) Milestone 1: Collapsed `/api/*` into `/api/v1/*`, kept `/api/v1/settings*` cookie-only (`require_session`), deleted duplicate `preview_container_upgrade` / `stop_background_task` / tool-detail routes, unified `FailureBody` into `ErrorBody`, hardened `viewer_client_id` revocation checks, and migrated viewer JS, Rust tests, docs, Python e2e callers, and Playwright specs (291 `brokk-mj-server`, 150 `server_runtime`, 97 Node, 150 Playwright deterministic tests passed).
- [x] (2026-11-23 17:13Z) Milestone 2: Started unconditional loopback HTTP API (`api-endpoint.json`, mode 0600, per-start loopback bearer token scoped strictly to the loopback listener) independent of `[phone].enabled` and Tailscale, initialized report backend and persisted API credential unconditionally, cleaned up `api-endpoint.json` in both epilogue and forced-exit watchdog, and switched local host `ApiClient` to direct `api-endpoint.json` discovery.
- [x] (2026-11-23 17:56Z) Milestone 2.5: Published host-loopback container HTTP port (`CONTAINER_API_PORT = 8788`), `MJ_DAEMON_API_TOKEN`, host `api-endpoint.json` with owner-checked cleanup, SSH dual `-L` HTTP API forwarding on `SshLease`, unified `DaemonClient::direct_api_endpoint`, and shared `mj_client::http::HttpDaemonClient` (verified with Host, Container fake-engine, and real disposable localhost SSH phone-disabled scenarios).
- [ ] Milestone 3: Migrate TCP `DaemonAction` families and the two remaining `#1043` dashboard git/working-context probes onto `/api/v1/*` in clean domain slices, deleting migrated TCP variants and handlers after each slice.
  - [x] (2026-11-23 18:49Z) Slice 3a-1: Migrated & deleted 14 TCP `DaemonAction` variants (`InstallAttachment`, `StartTurnReview`, dead `ResolveTurnReview`, `StopBackgroundTask`, `PreviewContainerUpgrade`, `UpgradeContainer`, `CancelContainerUpgrade`, and all 7 `Wiki*` actions) onto `/api/v1/*` via `mj_client::http::HttpDaemonClient`, unified TUI and Web image attachment upload on `POST /api/v1/sessions/{id}/attachments`, and moved the remaining 2 `#1043` local dashboard probes (`session_working_context`, `session_git_status`) behind `/api/v1/sessions/{id}/working-context` and `/api/v1/sessions/{id}/git-status` (deleting `live_controller_snapshot`).
  - [x] (2026-11-23 19:17Z) Slice 3a-2: Migrated & deleted 21 TCP `DaemonAction` variants (all 12 Workspace/Draft/ReadReceipt actions including dead `DeleteWorkspace`, plus `PreflightNewSession`, `CompletePath`, `DiscoverProjects`, `ProjectCatalog`, `CreateProject`, `RemoveProject`, `GitIdentity`, `SetGitIdentity`, and `PrepareMoveSession`) and 11 unused `DaemonReply` variants onto `/api/v1/*`, gating the 5 private terminal draft/receipt routes on `loopback_authenticated` and guarding mutations with upgrade-gate admission (allowing `DELETE /workspaces/{id}/close` during drain).
  - [x] (2026-11-23 20:08Z) Slice 3b-0: Deleted 6 redundant `ServerState` `mpsc` channels (`bundle_tx`, `receipt_tx`, `preflight_tx`, `move_preparation_tx`, `client_state_tx`, `background_task_stop_tx`), their 3 `*_done` reply relays, and dead request structs in favor of direct async methods on `SubagentBackend` / `ApiBackend`, cutting 529 net lines across `mj-server` and `mj-daemon/src/server_runtime/run.rs`.
  - [x] (2026-11-23 20:51Z) Slice 3b-1: Migrated & deleted 27 TCP `DaemonAction` variants (Session State/Read Views + Session Flags/Layouts/Sidebar/Away/Notifications, including dead `Snapshot` and `MarkSessionTargetMissing`) and 16 unused `DaemonReply` variants onto `/api/v1/*` (`DashboardBackend`, `HttpDaemonClient`, loopback-authenticated terminal routes, detached upgrade-admitted mutations, and single-attempt keep-alive `try_attach`/non-blocking `detach`).
  - [x] (2026-11-23 21:43Z) Slice 3b-2: Migrated & deleted 25 TCP `DaemonAction` variants (Prompt/Turn/Elicitation/Queue, Session Lifecycle/Moves, Audio Transcription, and streaming Session Import) and 15 unused `DaemonReply` variants onto terminal-authenticated `/api/v1/*`, and deleted TCP `UploadChunk`, `DaemonReply::BlobUploaded`, `chunk_bytes`, `upload_and_consume*`, and `serve.rs`'s connection-scoped upload temp-file registry.
  - [ ] Slice 3b-3: Migrate & delete remaining config/setup/quotas/profiles/targets/mounts/recovery request/response `DaemonAction` variants onto `/api/v1/*`.
- [ ] Milestone 4: Move streaming/interactive/SSH daemon paths (`RuntimeChanges`, `LoginTerminal`, SSH `ssh -L` forwarding) onto HTTP `/api/v1/*`, reduce TCP `DaemonAction` to the five frozen upgrade-management variants, and clean up remaining `mj-controller` type imports in `mj-cli`.

## Surprises & Discoveries

- Observation: `mj-client/src/daemon.rs` marks both `DaemonMetadata` (lines 57–74, persisted in `daemon.json`) and `DaemonStatus` (lines 1747–1755) with `#[serde(deny_unknown_fields)]`.
  Evidence: Adding a new field such as `api_address` to `daemon.json` would break older `mj` binaries trying to read `daemon.json` during an upgrade or rollback check. Publishing the loopback HTTP endpoint in a separate sibling file (`api-endpoint.json`, without `deny_unknown_fields`) avoids breaking existing `daemon.json` readers completely.

- Observation: Today `spawn_phone_server` in `mj-daemon/src/daemon/process.rs:796-808` is conditional on `config.phone.enabled` and `mj-daemon/src/server_runtime/run.rs:59` awaits `resolve_server_args` (which can run Tailscale TLS certificate setup) before serving HTTP.
  Evidence: Local CLI and TUI callers cannot rely on HTTP until the daemon binds an unconditional loopback listener (`127.0.0.1:0`) immediately at startup, separate from the optional external/Tailscale phone bind.

- Observation: `mj-server/src/server/api/routes.rs:135-169` (`require_api_auth`) already accepts either the bearer API token or the signed viewer cookie and renews the cookie when authenticated via cookie.
  Evidence: The web viewer (`mj-server/src/web/viewer.js`) already calls `/api/v1/options`, `/api/v1/wiki/*`, `/api/v1/sessions/{id}/history`, `/api/v1/sessions/{id}/diff`, `/api/v1/sessions/{id}/suspend-preflight`, and `/api/v1/sessions/{id}/{action}` using its existing session cookie. No auth redesign is needed to collapse `/api/*` into `/api/v1/*`; only `/api/v1/settings*` needs to stay on a cookie-only (`require_session`) sub-router so bearer tokens never gain secret-editing access.

- Observation: `/api/sessions/{id}/history` (prompt history search, `viewer.js:8746`) and `/api/v1/sessions/{id}/history` (paginated transcript history, `viewer.js:2326`) have different semantics under the same path suffix; likewise `/api/events` (viewer snapshot SSE) and `/api/v1/events` (typed API event stream) carry different payloads.
  Evidence: Unifying into `/api/v1/*` requires distinct, unambiguous names (`/api/v1/sessions/{id}/prompt-history` and `/api/v1/viewer/events`) rather than blindly stripping `/api` into `/api/v1`.

- Observation: Most of `#1043` is already complete on `master`. `mj-tui` does not depend on `mj-controller` or `mj-store`, `load_state_migrating` and production `Controller::load` in `mj-cli/src` are gone, and lifecycle/remote-repair preflight already go through the daemon.
  Evidence: Only two local process probes remain in the dashboard (`mj-cli/src/dashboard/session_state.rs:357-401`), plus non-dashboard CLI startup/import/EC2 utilities and re-exported data types.

## Decision Log

- Decision: Publish the always-on loopback HTTP endpoint in a separate `api-endpoint.json` file alongside `daemon.json` rather than adding a field to `DaemonMetadata`.
  Rationale: `DaemonMetadata` uses `#[serde(deny_unknown_fields)]`, so adding any field to `daemon.json` breaks older `mj` clients reading the file. A dedicated `api-endpoint.json` (with `0600` permissions, storing `address`, `token`, `pid`, `instance_id`, and `protocol_version`, without `deny_unknown_fields`) preserves full backward compatibility for `daemon.json` while letting new clients connect to HTTP in zero extra round-trips.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Collapse `/api/*` into `/api/v1/*` with no legacy `/api/*` compatibility aliases (keeping `/auth/*` and static viewer assets at root, and keeping `/api/v1/settings*` strictly cookie-only via `require_session`).
  Rationale: Project rules forbid adding parallel alias routes for stale browser pages (a browser reload serves the matching `viewer.js`), while `/api/v1/*` is the documented public API contract whose existing endpoints must remain intact. Keeping `/api/v1/settings*` behind `require_session` preserves the security invariant that bearer tokens cannot inspect or edit settings.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Keep the five frozen cross-version TCP management actions (`Ping`, `Status`, `Stop`, `PrepareUpgrade`, `UpgradeBlockers`) on the framed TCP listener until older daemons are no longer in service.
  Rationale: Automatic upgrades require a new `mj` binary to discover, inspect blockers on, prepare handoff with, and stop an already-running older daemon speaking an earlier protocol version. Removing all non-frozen feature actions (~134 variants) delivers almost all of the simplification while keeping upgrades seamless.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Land each milestone (and each domain slice of Milestone 3) as its own reviewed, pushed checkpoint on `master`.
  Rationale: Transport consolidation touches `mj-server`, `mj-daemon`, `mj-client`, `mj-cli`, `mj-chat`, and `tests/e2e/web/`. Small reviewed checkpoints keep every diff easy to reason about, prevent long-lived merge conflicts, and guarantee the tree compiles and stays strictly simpler after every commit.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Move Container and SSH HTTP endpoint forwarding (`api-endpoint.json`) ahead of Milestone 3 (as Milestone 2.5) rather than waiting for Milestone 4, and scope `loopback_token` strictly to the loopback HTTP listener (leaving it empty on the external phone listener).
  Rationale: Auditing Milestone 3a revealed that if Container and SSH placements do not publish/forward `api-endpoint.json` when `[phone].enabled = false`, every Milestone 3 caller would need a temporary HTTP-with-TCP-fallback path. Publishing the container API port and forwarding the remote `api_address` inside the existing SSH `-L` tunnel first lets every Milestone 3 slice delete its TCP `DaemonAction` variants immediately across all placements. Keeping `loopback_token` off `serve_phone` ensures the per-start loopback secret is never accepted over an external network interface.
  Date/Author: 2026-11-23 / Coordinator

## Outcomes & Retrospective

- **Milestone 1 (complete):** Removed the 76-line duplicate `/api/*` route group in `mj-server/src/server/routes.rs`, deleted duplicate `preview_container_upgrade` and `stop_background_task` handlers and duplicate `FailureBody`, and mounted every HTTP endpoint under `/api/v1` (with `/api/v1/settings*` strictly cookie-only via `require_session`). Also hardened `viewer_client_id` to validate via `authenticated_viewer` (checking revocations) so pairing a bearer token with a revoked cookie cannot spoof a viewer identity. All 291 `brokk-mj-server`, 150 `server_runtime`, 97 Node, and 150 Playwright deterministic tests pass.
- **Milestone 2 (complete):** Split the daemon's HTTP API startup (`server_runtime::start_api`) from the external phone listener (`server_runtime::serve_phone`). The loopback HTTP API now binds and publishes mode-0600 `api-endpoint.json` before `daemon.json` regardless of `[phone].enabled` or Tailscale availability, initializes the report backend and persisted API credential unconditionally, scopes its per-start `loopback_token` strictly to the loopback listener, and removes `api-endpoint.json` on normal and watchdog exit. Local host `ApiClient` connects directly via `api-endpoint.json` with zero TCP `WebViewerAccess`/`ApiCredential` calls, and `mj api-token` works with phone disabled.

- **Milestone 2.5 (complete):** Added `MJ_DAEMON_API_TOKEN`, container second loopback HTTP port (`CONTAINER_API_PORT = 8788`), host `api-endpoint.json` publication/adoption and owner-checked cleanup across every stop/stale-record path, SSH `DaemonEndpoint` additive `api_address`/`api_token` with dual `-L` forwarding on the single `SshLease`, unified `DaemonClient::direct_api_endpoint`, and shared typed `mj_client::http::HttpDaemonClient` (preserving zero `mj-client` dependency in `mj-worker`/`mj-worker-bin`). Real disposable localhost SSH HTTP scenario passes with `[phone].enabled = false` before and after remote restart.

- **Milestone 3 Slice 3a-1 (complete):** Deleted 14 TCP `DaemonAction` variants (`InstallAttachment`, `StartTurnReview`, dead `ResolveTurnReview`, `StopBackgroundTask`, 3 container-upgrade actions, 7 Wiki actions) and their TCP handlers across `mj-client`/`mj-daemon`, unified TUI and Web image upload on `POST /api/v1/sessions/{id}/attachments`, and eliminated the last two local dashboard probes (`session_working_context`, `session_git_status`) via `GET /api/v1/sessions/{id}/working-context` and `/git-status` (deleting `live_controller_snapshot`).

- **Milestone 3 Slice 3a-2 (complete):** Deleted 21 TCP `DaemonAction` variants (Workspaces, Drafts, ReadReceipts, Preflight, Paths, Projects/Bundles, GitIdentity, MovePrepare) and 11 unused `DaemonReply` variants, shrinking the repo net LOC while adding loopback-auth gating on terminal draft/receipt routes and upgrade-gate admission on workspace/project writes.

- **Milestone 3 Slice 3b-0 (complete):** Removed 6 redundant `ServerState` `mpsc` channels (`bundle_tx`, `receipt_tx`, `preflight_tx`, `move_preparation_tx`, `client_state_tx`, `background_task_stop_tx`), their `*_done` reply relays, and dead request types, routing HTTP handlers directly to async `SubagentBackend` methods while preserving concurrency semaphores, upgrade-gate admission, and cancel-on-disconnect semantics (-529 net LOC across `mj-server` and `mj-daemon/src/server_runtime/run.rs`).

- **Milestone 3 Slice 3b-1 (complete):** Deleted 27 TCP `DaemonAction` variants (Session State/Read Views + Session Flags/Layouts/Sidebar/Away/Notifications) and 16 unused `DaemonReply` variants, routing dashboard startup, stored reads, session tail, search, native-agent history (`full=true` terminal-authenticated), operator presence, terminal attach/detach, layout persistence, and session settings through `/api/v1/*` (`DashboardBackend` + `HttpDaemonClient`), with single-attempt keep-alive `try_attach` and non-blocking `detach` during handoffs.

- **Milestone 3 Slice 3b-2 (complete):** Deleted 25 TCP `DaemonAction` variants (Prompt/Turn/Elicitation/Queue, Session Lifecycle/Moves, Audio Transcription, streaming Session Import, and `UploadChunk`), 15 unused `DaemonReply` variants, `chunk_bytes`, `upload_and_consume*`, and `serve.rs`'s connection-scoped upload temp-file registry (~280 lines deleted from `serve.rs`), mounting terminal-authenticated `/api/v1/*` routes with streaming disk-backed import upload/verification outside admission, late detached import registration admission, cancel-on-drain support for `CancelLifecycle`, and single-attempt lifecycle wait observation.

Milestones 3 (Slice 3b-3) and 4 remain.

## Context and Orientation

The Mjolnir daemon (`mj daemon-run`) runs out of `mj-daemon/src/daemon/process.rs`. At startup it unconditionally binds a local TCP listener (`127.0.0.1:0` unless `MJ_DAEMON_LISTEN` is set), generates a random hex token, and writes `daemon.json` (`DaemonMetadata`, defined in `mj-client/src/daemon.rs:57-74`). Clients connect to this framed TCP endpoint via `DaemonClient` (`mj-client/src/daemon.rs`) and send `RequestEnvelope { protocol_version, token, action: DaemonAction }`. The daemon dispatches incoming frames in `mj-daemon/src/daemon/serve.rs` and `mj-daemon/src/daemon/actions.rs`. Five actions (`Ping`, `Status`, `Stop`, `PrepareUpgrade`, `UpgradeBlockers`) form the frozen cross-version management subset (`serve.rs:178-215`); every other action requires `protocol_version == PROTOCOL_VERSION`.

Separately, when `[phone].enabled` is true (`process.rs:796-808`), the daemon spawns the HTTP/web server via `mj-daemon/src/server_runtime/run.rs::run_server`. That task first resolves Tailscale/TLS arguments (`server_runtime/args.rs`), loads cookie signing credentials and the persisted API bearer token, builds the Axum router in `mj-server/src/server/routes.rs::router`, and binds the configured phone listener (`axum_server`). Inside `routes.rs`, two separate API trees are mounted:

1. A cookie-only `protected` router (`require_session`) mounting 29 `/api/*` routes (`routes.rs:128-202`), including `/api/actions` (a tagged `ControllerAction` enum in `mj-server/src/server/actions.rs`), `/api/settings*`, `/api/snapshot`, `/api/events`, `/api/conversations/{id}`, `/api/sessions/{id}/attachments`, `/api/sessions/{id}/draft`, `/api/sessions/{id}/client-state`, `/api/sessions/{id}/history`, `/api/sessions/{id}/container-upgrade`, `/api/sessions/{id}/background-tasks/stop`, `/api/sessions/{id}/items/{stable_id}/detail`, `/api/preflight/*`, `/api/paths/complete`, `/api/projects*`, `/api/git-identity`, `/api/moves/prepare`, `/api/bundles`, `/api/workspaces/{id}/read`, and `/api/push/*`.
2. A nested `/api/v1` router (`mj-server/src/server/api/routes.rs:3-128`) protected by `require_api_auth` (accepting either the persisted bearer token or a valid viewer cookie) mounting 53 resource endpoints backed by `SubagentBackend` (`mj-daemon/src/server_runtime/api.rs`).

The web frontend lives in `mj-server/src/web/viewer.js`, `mj-server/src/web/push-sync.js`, and `mj-server/src/web/service-worker.js`, with Rust handler tests in `mj-server/src/server/tests.rs` and `mj-server/src/server/api/tests.rs`, daemon runtime HTTP tests in `mj-daemon/src/server_runtime/tests.rs` and `mj-daemon/src/server_runtime/api/tests.rs`, and browser Playwright tests in `tests/e2e/web/`.

The CLI HTTP client lives in `mj-cli/src/api_client.rs` and currently discovers the daemon's HTTP base URL and bearer token by opening a framed TCP connection and sending `WebViewerAccess` and `ApiCredential`.

## Plan of Work

### Milestone 1: Collapse `/api/*` into `/api/v1/*` and Delete Duplicate HTTP Handlers

This milestone eliminates the duplicate HTTP route surface so the daemon serves one unified `/api/v1/*` API instead of separate `/api/*` and `/api/v1/*` route groups.

In `mj-server/src/server/routes.rs` and `mj-server/src/server/api/routes.rs`, remove the top-level `/api/*` `protected` router and move its non-duplicate routes directly into `/api/v1`:

- Delete the three routes that already exist identically on `/api/v1`: `/api/sessions/{session_id}/items/{stable_id}/detail` (use `/api/v1/sessions/{session_id}/items/{stable_id}/detail`), `/api/sessions/{session_id}/container-upgrade` (use `GET /api/v1/sessions/{session_id}/container-upgrade` and delete the duplicate `preview_container_upgrade` handler in `mj-server/src/server/handlers.rs`), and `/api/sessions/{session_id}/background-tasks/stop` (use `POST /api/v1/sessions/{session_id}/background-tasks/stop` and delete the duplicate `stop_background_task` handler in `handlers.rs` if redundant with `api::stop_background_task`, reconciling any response-shape difference cleanly in `viewer.js`).
- Mount `/settings`, `/settings/page`, `/settings/edit`, and `/settings/save` inside `/api/v1` on a dedicated sub-router layered with `require_session` (cookie-only) so bearer tokens are still refused with 401/403 and can never access or mutate config secrets.
- Mount the remaining viewer endpoints inside `/api/v1` (under `require_api_auth`, which already accepts both viewer cookie and bearer token):
  - `/snapshot` (`GET`)
  - `/viewer/events` (`GET`, renamed from `/api/events` to avoid colliding with `/api/v1/events`)
  - `/sessions/{session_id}/row` (`GET`)
  - `/sessions/{session_id}/notification-message` (`GET`)
  - `/sessions/{session_id}/client-state` (`GET`)
  - `/sessions/{session_id}/draft` (`PUT`)
  - `/sessions/{session_id}/prompt-history` (`GET`, renamed from `/api/sessions/{session_id}/history` to avoid colliding with `/api/v1/sessions/{session_id}/history` transcript history)
  - `/sessions/{session_id}/attachments` (`POST`)
  - `/conversations/{session_id}` (`GET`) and `/conversations/{session_id}/read` (`POST`)
  - `/workspaces/{workspace_id}/read` (`POST`)
  - `/bundles` (`POST`)
  - `/preflight/new` (`POST`) and `/preflight/resume` (`POST`)
  - `/paths/complete` (`POST`)
  - `/projects` (`GET`, `POST`) and `/projects/discover` (`POST`)
  - `/git-identity` (`GET`, `POST`)
  - `/moves/prepare` (`POST`)
  - `/push/key` (`GET`) and `/push/status` (`GET`)
  - `/actions` (`POST`) for browser actions; switch browser callers for `container-upgrade` upgrade/cancel, `background-tasks/stop`, and `items/{stable_id}/detail` to the typed `/api/v1` endpoints, and verify whether `viewer_client_id` / cookie-only assumptions in `client-state`, `draft`, `read`, or `actions` need graceful behavior when called with a bearer token (for example, `viewer_client_id` already returns `None` when no cookie is present, and `save_draft` returns 409 when no client identity exists).
- Reconcile error-body formatting on `require_api_auth` / `api_response_headers`: verify that `mj-server/src/web/viewer.js`'s `request()` helper handles `ApiFailure` JSON error responses (`{ error, code }`) and `ApiError` responses seamlessly (and check `401` handling so unauthenticated browser calls still trigger `showLogin()`). Even better, unify `ApiError` and `ApiFailure` wire shapes if they differ so all `/api/v1/*` handlers return one consistent JSON error format.
- Update every `/api/` path in `mj-server/src/web/viewer.js`, `mj-server/src/web/push-sync.js`, Rust tests (`mj-server/src/server/tests.rs`, `mj-daemon/src/server_runtime/tests.rs`), and Playwright tests (`tests/e2e/web/*.spec.js` and `tests/e2e/web/viewer.unit.test.mjs`) to use `/api/v1/...`. Keep `service-worker.js` bypassing `/api/` (which covers `/api/v1/`).

At the end of Milestone 1, zero non-v1 `/api/*` routes exist, duplicate handlers (`preview_container_upgrade`, duplicate `stop_background_task`, duplicate `tool_call_detail` route) are deleted, `mj-server` and `mj-daemon` HTTP tests pass, and `tests/e2e/web/` Playwright tests pass.

### Milestone 2: Always-On Loopback HTTP API (`api-endpoint.json`)

This milestone decouples the daemon's HTTP API from `[phone].enabled` and Tailscale TLS so every running daemon guarantees an immediate loopback HTTP `/api/v1/*` listener from startup.

- Define `ApiEndpointMetadata` in `mj-client/src/api_endpoint.rs` (or `mj-client/src/daemon.rs`, without `#[serde(deny_unknown_fields)]`): `address: SocketAddr`, `token: String`, `pid: u32`, `instance_id: String`, `protocol_version: u32`. Store atomically with `0600` permissions at `<data_dir>/api-endpoint.json`, cleaned up alongside `daemon.json` on clean daemon exit.
- Refactor `mj-daemon/src/server_runtime/run.rs` and `mj-daemon/src/daemon/process.rs`:
  - Split HTTP router construction and backend loop setup from external/Tailscale listener resolution (`resolve_server_args`).
  - Always load/create the persisted API bearer token, construct the shared Axum `Router` (accepting either the persisted `api_token` OR the per-start loopback `token` from `api-endpoint.json` in `require_api_auth`), bind a loopback `TcpListener` on `127.0.0.1:0` (or `MJ_DAEMON_API_LISTEN` if configured for containers), serve `router.clone()` on it immediately, and write `api-endpoint.json` before publishing `daemon.json` so any client that observes a live daemon immediately has a reachable local HTTP API.
  - When `config.phone.enabled` is true, spawn the external/Tailscale listener task asynchronously to run `resolve_server_args` and bind the external port/TLS serving `router.clone()` (preserving `WebViewerAccess`, `WebViewerStatus`, `RecoverWebViewer`, and Tailscale certificate renewal). When `config.phone.enabled` is false, skip only the external listener — local `/api/v1/*` stays up and healthy.
- Update `mj-cli/src/api_client.rs` so local daemon connections discover `http://<address>` and bearer `token` directly from `api-endpoint.json` (falling back to the legacy TCP `WebViewerAccess` + `ApiCredential` lookup only when talking to an older pre-Milestone-2 daemon or over SSH until Milestone 4).

At the end of Milestone 2, `mj api` and HTTP tests work even with `[phone].enabled = false`, local `ApiClient` connects with zero TCP round-trips, and `daemon.json` remains 100% compatible with older `mj` binaries.

### Milestone 3: Migrate TCP `DaemonAction` Families and Remaining `#1043` TUI Probes onto `/api/v1/*`

With an unconditional loopback HTTP API live, migrate TUI and CLI callers off custom TCP `DaemonAction` variants onto a shared typed HTTP client in `mj-client` and delete each migrated variant from `mj-client/src/daemon.rs` and `mj-daemon/src/daemon/actions.rs`. Execute in cohesive, independently reviewable slices:

- Slice 3a (existing v1 overlaps + unified attachment upload):
  - Migrate attachment uploads from TCP `UploadChunk` + `InstallAttachment` (`mj-chat/src/chat/remote.rs`) onto `POST /api/v1/sessions/{session_id}/attachments` (raw body upload, adding an optional header or raw-mode flag for already-optimized bytes if needed so both TUI and Web share ONE upload path). Delete `UploadChunk` and `InstallAttachment` from TCP `DaemonAction` and `serve.rs`.
  - Migrate Workspaces & Drafts (`ListWorkspaces`, `ListWorkspaceDrafts`, `CreateWorkspace`, `RenameWorkspace`, `TouchWorkspace`, `CloseWorkspace`, `CancelWorkspaceClose`, `DeleteWorkspace`, `RecoverDraft`, `DiscardDrafts`, `PersistTerminalDraft`, `PersistReadReceipt`), Reviews (`StartTurnReview`, `ResolveTurnReview`), Wiki (`WikiSyncRequest`, `WikiSyncWatch` → SSE `/api/v1/wiki/sync/watch`, `WikiSearch`, `WikiBrief`, `WikiHits`, `WikiSession`, `WikiRestore`), Container upgrades (`PreviewContainerUpgrade`, `UpgradeContainer`, `CancelContainerUpgrade`), Background tasks (`StopBackgroundTask`), Preflight/Paths/Projects/GitIdentity/MovePrepare onto `/api/v1/*`. Delete the migrated variants from `DaemonAction` and `actions.rs`.
  - Move the two remaining `#1043` local dashboard probes (`session_working_context` and `session_git_status` in `mj-cli/src/dashboard/session_state.rs:357-401`) behind `/api/v1/sessions/{id}/working-context` and `/api/v1/sessions/{id}/git-status` executed by the daemon's background pool, removing direct `CancellableProcessExecutor` target probes from the dashboard.
- Slice 3b (remaining request/response control actions + backend simplification):
  - Collapse the seven separate `mpsc` channels in `ServerState` (`action_tx`, `bundle_tx`, `receipt_tx`, `preflight_tx`, `move_preparation_tx`, `client_state_tx`, `background_task_stop_tx`) and `SubagentBackend` into a cleaner unified daemon HTTP backend trait (`mj-server` calls the daemon runtime directly rather than juggling 7 channels + `DaemonAction`).
  - Migrate remaining request/response `DaemonAction` groups (session state/read views, prompt/turn/elicitation/queue mutations, lifecycle start/wait, configuration/setup/quotas/profiles/targets/mounts/recovery, audio transcription, session import, workspace layout/sidebar persistence, operator away/notifications) onto typed `/api/v1/*` endpoints and delete them from `DaemonAction` and `actions.rs`.

### Milestone 4: Streaming, Interactive PTY, SSH Forwarding, and Final Cleanup

- Replace TCP `RuntimeChanges` framed streaming with HTTP `/api/v1/runtime/changes` (SSE or chunked NDJSON stream with cursor/reconnect semantics matching `RuntimeFrame`).
- Replace TCP `LoginTerminal` bidirectional PTY framing with an HTTP WebSocket endpoint (`/api/v1/login-terminal`) using Axum's `ws` feature (and `tokio-tungstenite` on the client), preserving resize, stdin/stdout frames, and clean teardown.
- Update SSH remote daemon placement (`mj-client/src/daemon/ssh.rs` and `mj daemon endpoint --json`) to discover and forward `api-endpoint.json`'s `api_address` over `ssh -L` with bearer auth and `/api/v1/identity` verification (retaining a fallback to legacy TCP only for the frozen upgrade handshake).
- Delete all remaining non-frozen `DaemonAction` variants and dead framed-codec helpers from `mj-client/src/daemon.rs`, `mj-daemon/src/daemon/serve.rs`, and `mj-daemon/src/daemon/actions.rs`, leaving ONLY the five frozen upgrade-management variants (`Ping`, `Status`, `Stop`, `PrepareUpgrade`, `UpgradeBlockers`).
- Move shared data types still imported from `mj_controller::` in `mj-cli/src/` (`ApiEvent*`, `UsagePage`, `FinishedReviewResolution`, `AdditionalMount`, `ResumeRepositorySource*`, `ProjectDiscovery`, `WikiContinuation`) into `mj-core` / `mj-session-types` / `mj-client` so the dashboard drops its `Controller` struct wrapper (`#1043`).

## Concrete Steps

All Cargo commands run from the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/8e67145a871bdf36fa2456d198e2f93f` using the `mbx`-shimmed `cargo` in PATH (never setting `CARGO_TARGET_DIR`, running `cargo test` outside the sandbox so loopback TCP/Unix sockets succeed):

    cargo test -p brokk-mj-server
    cargo test -p brokk-mj-daemon server_runtime
    cargo clippy -p brokk-mj-server -p brokk-mj-daemon -p brokk-mj-client -p brokk-mjolnir --all-targets -- -D warnings

For browser viewer tests in `tests/e2e/web/`:

    node --test tests/e2e/web/viewer.unit.test.mjs
    npx --prefix tests/e2e/web playwright test

For Milestone 2–4 daemon integration tests, run targeted filters for daemon startup, upgrade handoff, API client, and dashboard suites, confirming every filter matches non-zero tests.

## Validation and Acceptance

- Milestone 1 acceptance: `rg "'/api/|\"/api/|\`/api/" mj-server/src/web` reports only `/api/v1/` paths (plus the `/api/` prefix guard in `service-worker.js`); `mj-server/src/server/routes.rs` mounts zero non-v1 `/api/*` routes; settings endpoints reject bearer-only requests and accept signed viewer cookies; `brokk-mj-server` tests, `brokk-mj-daemon` `server_runtime` tests, `tests/e2e/web/viewer.unit.test.mjs`, and Playwright specs pass.
- Milestone 2 acceptance: starting an isolated daemon (`mj --instance <test-name>`) with `[phone].enabled = false` creates `api-endpoint.json` alongside `daemon.json`, serves `GET /api/v1/options` on the loopback HTTP address using the `api-endpoint.json` bearer token, leaves `daemon.json` deserializable by the strict `DaemonMetadata` schema, and allows `mj api` to succeed with zero TCP `WebViewerAccess`/`ApiCredential` calls.
- Milestone 3 acceptance: each migrated domain works end-to-end over `/api/v1/*` with its TCP `DaemonAction` variants deleted, `mj-cli/src/dashboard/session_state.rs` runs zero local `CancellableProcessExecutor` git/working-context probes, and `POST /api/v1/sessions/{id}/attachments` serves both TUI and Web uploads.
- Milestone 4 acceptance: `DaemonAction` contains only the five frozen upgrade-management variants (`Ping`, `Status`, `Stop`, `PrepareUpgrade`, `UpgradeBlockers`), live daemon upgrade handoff tests pass across protocol versions, `LoginTerminal` and `RuntimeChanges` pass over HTTP, and net LOC across `mj-client/src/daemon.rs`, `mj-daemon/src/daemon/actions.rs`, `mj-daemon/src/daemon/serve.rs`, and `mj-server/src/server/` drops by thousands of lines.

## Idempotence and Recovery

Every milestone is additive-then-subtractive on `master` and passes pre-push review before moving to the next milestone. Because `daemon.json` is never modified and the five frozen TCP upgrade actions remain intact throughout all four milestones, rolling forward or back across any milestone checkpoint preserves automatic daemon upgrade handoff without user intervention.

## Artifacts and Notes

Architecture inventories gathered prior to drafting this plan are recorded in session audit notes (`transport-consolidation-map.md` and `http-unification-details.md`). Key counts at plan start (`723305327`): 139 TCP `DaemonAction` variants (`mj-client/src/daemon.rs` ~5,100 LOC, `mj-daemon/src/daemon/actions.rs` ~1,585 LOC, `serve.rs` ~889 LOC), 29 `/api/*` routes (`mj-server/src/server/routes.rs`), and 53 `/api/v1/*` routes (`mj-server/src/server/api/routes.rs`).

## Interfaces and Dependencies

- `mj-server/src/server/api/routes.rs`: single HTTP API router nested at `/api/v1`. Uses `require_api_auth` (bearer token or signed viewer cookie) for general endpoints and a nested `require_session` (cookie-only) sub-router for `/settings*`.
- `mj-client`: owns `ApiEndpointMetadata` (deserialized from `<data_dir>/api-endpoint.json`, without `deny_unknown_fields`) and the shared typed `HttpDaemonClient` used by CLI, TUI, and `mj-chat`, alongside the shrunken frozen-subset TCP `ManagementClient` for daemon upgrade handoff.
- `mj-daemon/src/daemon/process.rs` and `mj-daemon/src/server_runtime/run.rs`: construct the shared Axum `Router` unconditionally, bind `127.0.0.1:0` for `api-endpoint.json` before publishing `daemon.json`, and clone the same `Router` onto the optional external/Tailscale listener when `[phone].enabled` is true.
