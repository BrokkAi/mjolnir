# Replace the Vanilla-JS Web Viewer with a Mobile-First React SPA

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This plan lives in `.agents/plans/web-ui-redesign.md` and must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir ships a browser interface ("web viewer") embedded inside the `brokk-mj-server` Rust crate so operators can check notifications, read live agent conversations, answer elicitations (structured questions and tool-permission prompts from agents), launch or move sessions, inspect quotas and targets, and edit daemon settings from a phone or desktop browser. Today that interface is an 11,602-line single-file vanilla JavaScript application (`mj-server/src/web/viewer.js`) with a 2,612-line stylesheet (`mj-server/src/web/viewer.css`). In daily phone use it suffers from three severe defects: scrolling in the conversation view jitters whenever an agent streams output or the on-screen keyboard moves; elicitations are crammed inside nested `30dvh`/`60dvh` scrollports with unformatted `<pre>` text, dropped option previews, low-contrast headings, and a focus takeover that hides the transcript; and cellular (off-Wi-Fi) performance is sluggish because every static asset revalidates on load, the boot sequence runs six sequential HTTP round trips, every SSE runtime tick triggers a conversation refetch even when another session changed, and re-opening a conversation discards its transcript and re-downloads all 512 live entries from scratch.

After this work, Mjolnir's web viewer is a clean, modular React 19 + TypeScript + Vite + Tailwind CSS v4 + Zustand single-page application authored under `web/` and compiled into pre-built static assets under `mj-server/src/web/dist/` that `brokk-mj-server` embeds directly at compile time with zero Node requirement during `cargo build` or at runtime. Users get a smooth, non-jittery mobile-first conversation experience with explicit end-follow semantics, memoized keyed rows, and throttled streaming Markdown; a spacious, readable single-scrollport elicitation dock and full-height sheet that renders Markdown and option previews without hiding or destroying layout on focus; and fast cellular operation through content-hashed immutable `/assets/*` caching, a cache-first service worker for hashed bundles, parallel boot fetches, an in-memory per-session transcript cache with incremental `?after_seq=` resumption, and an additive `conversation_seqs` field on SSE delta frames so idle conversations issue zero refetches. All nine existing Mjolnir views (Dashboard/Workspaces, Notifications Inbox, Conversation with Sub-agents/Reviewers/Diffs/Composer, New Session, Resume/Wiki, Move, Targets, Quota, and Settings) plus Auth and Web Push are preserved, and the legacy vanilla-JS files and brittle source-slicing tests are deleted.

## Progress

- [x] (2026-11-25 21:18Z) Completed read-only audits of `mj-server/src/web/`, `mj-server/src/server/`, build/CI/crates.io constraints, exact Rust JSON wire schemas, and OpenChamber mobile chat UX patterns.
- [x] (2026-11-25 21:19Z) Authored ExecPlan `.agents/plans/web-ui-redesign.md`.
- [x] (2026-11-25 21:40Z) Milestone 1: Created `web/` React+TypeScript+Vite+Tailwind+Zustand scaffold, complete Rust-verified `api/types.ts`, HTTP+SSE client, LRU transcript cache, Auth screen, responsive App Shell, cache-first service worker, and 45 Vitest unit tests (plus browser smoke check under strict CSP); also implemented additive `conversation_seqs` in `mj-server/src/server/viewer_feed.rs` and `handlers.rs` (322 `brokk-mj-server` tests pass, clippy clean).
- [x] (2026-11-25 21:58Z) Milestone 2: Implemented Tier 1 daily-driver views in parallel slices — (2A) Conversation view with explicit `following|detached` scroll engine, keyed memoized rows, throttled streaming Markdown, collapsible tool/shell/background-task/diff/review cards, lazy full tool-detail fetch, sub-agent/native-agent sheets, and mobile-safe composer (18 Vitest tests pass); (2B) Elicitation dock & full-height single-scrollport sheet with Markdown + option previews + companion roles + sessionStorage drafts, Notifications Inbox + AFK + Web Push, and Dashboard/Workspaces (17 Vitest tests pass).
- [x] (2026-11-25 21:56Z) Milestone 3: Implemented all code-split Tier 2 views in parallel slices — (3A) New Session wizard, Resume & Session Wiki, Move Session (7 Vitest tests pass); (3B) Targets, Quota, Settings + Git Identity, and Session Lifecycle modals (Suspend/ContainerSettings/Upgrade/Rename/WorkspacePicker/Destroy) (12 Vitest tests pass).
- [x] (2026-11-25 23:11Z) Milestone 4: Embedded pre-built `mj-server/src/web/dist/` via pure-Rust `mj-server/build.rs` with immutable `/assets/*` caching and strict CSP; deleted legacy `viewer.js`/`viewer.css`/`viewer.html`/`markdown.js`/`tool-output.js`/`push-sync.js`/`test-dom.js`, unused fonts, and legacy icons; cleaned up `viewer_source` slicing tests; built full strict-CSP Playwright deterministic suite (7/7 Node unit tests, 28/28 Playwright tests, 101/101 Vitest tests, 301/301 `brokk-mj-server` Rust tests).

## Surprises & Discoveries

- Observation: `mj-server/Cargo.toml` sets `publish = true` (published to crates.io with a 10 MiB compressed limit), and release jobs on Linux, macOS, and Windows plus `containers/Containerfile.daemon` compile `brokk-mj-server` without Node installed.
  Evidence: `.github/workflows/release.yml` lines 208, 397, 544 and `containers/Containerfile.daemon` run `cargo build --release --locked` with no Node toolchain; Node 24 is only installed in `mise.toml:18`, `Containerfile.agent-dev`, and the `web-viewer` job in `.github/workflows/ci.yml:414-435`.
  Impact: `mj-server/build.rs` must be pure Rust (scanning pre-built files in `mj-server/src/web/dist/` with `std::fs`) and must never shell out to Node or Vite. Pre-built `mj-server/src/web/dist/` is checked into Git (replacing the checked-in 487 KiB `viewer.js`), keeping `cargo build` and crates.io completely Node-free.

- Observation: Cellular slowness is driven primarily by sequential round trips, mandatory revalidation, and wasted refetches — not missing HTTP compression.
  Evidence: `mj-server/src/server/routes.rs:143` already wraps the router in `CompressionLayer::new().quality(CompressionLevel::Fastest)` (gzip + brotli for JSON and static assets, skipping `text/event-stream`). However, `assets.rs:175-191` sends `Cache-Control: no-cache` on every static file, `service-worker.js:44-66` is network-first for all assets, boot runs serial `/auth/session` → `/snapshot` → `/options` → SSE → `/row` → `/conversations/{id}`, `viewer.js:6779-6783` unconditionally refetches `/conversations/{id}` on every SSE `runtime` frame regardless of which session changed, and `viewer.js:10014-10067` opens conversations with no `after_seq`, re-downloading the whole 512-item window every time the user returns to a session.

- Observation: Virtualizing a mobile chat list with `@legendapp/list` or `@tanstack/react-virtual` without extensive iOS-specific patches (6 custom patches in OpenChamber covering momentum deferral, device-pixel rounding, same-frame resize shrink, and DOM ordering) actually *worsens* scroll jitter on iOS Safari.
  Evidence: OpenChamber's `packages/ui/src/components/chat/lib/scroll/DOCUMENTATION.md` and `bun-patches/@legendapp%2Flist@3.7.2.patch` document 224 height corrections / 54,687 px of jump in 12 seconds of iPhone scroll without per-item estimates and momentum-deferred corrections. Because Mjolnir's live window is bounded (`PROJECTION_TAIL_ITEMS = 512` items, with collapsed 1-line tool rows), keyed `React.memo` rows with lazy-mounted expanded bodies, `overflow-anchor: none`, throttled streaming Markdown, an explicit end-follow state machine, and single-frame `useLayoutEffect` prepend anchoring eliminate both DOM-replacement jitter and virtualizer estimate jitter.

- Observation: `mj-server/src/server/assets.rs:57-66` enforces a strict Content Security Policy (`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`).
  Evidence: Any library that injects inline `<style>` tags at runtime (such as certain toast libraries or CSS-in-JS runtimes) will trigger CSP violations. Production Tailwind CSS compiled by Vite into a static `/assets/*.css` file plus CSSOM inline styles (`element.style` via React `style={{...}}`) passes strict `style-src 'self'`.

- Observation: Elicitations are unreadable on phones for five compounding reasons in `viewer.js` and `viewer.css`.
  Evidence: `viewer.css:643-659` nests `<pre class="elicitation-message">` (`max-height: 30dvh; overflow-y: auto`) inside `#elicitations` (`max-height: 60dvh; overflow-y: auto`); `viewer.js:7982-7990` drops `ElicitationOption.preview` and renders descriptions as plain text; `viewer.css:2072-2079` reserves a 44px flex column for native radios and breaks words with `overflow-wrap: anywhere`; `viewer.css:620-624` colors headings with `--dim`; and `viewer.css:2226-2283` toggles `body.elicitation-focused` on input focus, hiding the transcript and header and swapping the scroll container mid-interaction.

- Observation: `conversation_rx` publishes independently of `browser.revisions` via `mj-daemon/src/daemon/browser.rs` (`install_browser_conversation` and `capture`).
  Evidence: Inspecting `mj-daemon/src/daemon/browser.rs` during Milestone 1 confirmed streaming transcript updates advance `conversation_rx` without necessarily bumping runtime revisions. Waiting on both `revisions.changed()` and `conversations.changed()` in `handlers.rs:events` and emitting same-cursor `ViewerFrame::Delta` frames carrying only `conversation_seqs` keeps streaming responsive without waking unrelated sessions.

- Observation: Signed-out boot probing `GET /api/v1/snapshot` directly returns HTTP 401, which Chromium prints as a console resource error (`Failed to load resource: 401`).
  Evidence: Milestone 1 headless Chromium smoke check against strict CSP passed with zero CSP or runtime JS errors; Milestone 4 Playwright specs that assert zero console errors must explicitly ignore only the expected 401 resource log on signed-out login tests.

- Observation: Playwright end-to-end tests caught three real bugs missed by jsdom unit tests: `navigate` racing with same-tick modal back-layer teardown (`history.back()` bouncing back after Destroy), Composer clearing newly typed text after an in-flight POST resolved, and CSS `overflow-x: auto` computing `overflow-y: auto` on `<pre>` and tables inside the elicitation sheet.
  Evidence: Fixed in `web/src/stores/useUiStore.ts`, `web/src/features/conversation/Composer.tsx`, `web/src/components/MarkdownContent.tsx`, and `web/src/features/elicitation/ElicitationField.tsx`, with regression tests added to `stores.test.ts` and `composer.test.tsx`.

- Observation: A 3× repeat run of `conversation-scroll.spec.js` surfaced a timing race between `contentVersion`'s `requestAnimationFrame` (~16 ms) and `useThrottledValue`'s 100 ms trailing Markdown commit: when the throttle deferred rendering past the rAF, end-follow and tall-reply pause measured the old short DOM and never re-ran when the tall content painted.
  Evidence: Fixed structurally in `web/src/features/conversation/useConversationScroll.ts` and `TranscriptFeed.tsx` by observing the feed's inner content wrapper with `ResizeObserver` and clamping `scrollTop` to the streaming reply's top edge on tall-reply pause (preserving `paused` across the browser's follow-up `scroll` event), with a regression test in `scroll.test.tsx`. Verified 25/25 passing under `--repeat-each=5`.

## Decision Log

- Decision: Build a new Mjolnir-native React 19 + TypeScript + Vite + Tailwind CSS v4 + Zustand SPA under `web/` instead of forking OpenChamber.
  Rationale: OpenChamber's UI is ~647k non-test LOC with ~520 files coupled directly to `@opencode/client` and OpenCode 2.4.1 wire frames, backed by a ~204k-LOC Express server assuming one local OpenCode process. Mjolnir is a multi-host, multi-harness Rust daemon with its own control-plane surfaces (workspaces, sub-agents, moves, targets, quotas, wiki restore, daemon settings). Writing a focused ~6k-LOC Mjolnir-native SPA keeps the compiled bundle tiny, preserves single-binary embedding in `brokk-mj-server`, and lets us adopt OpenChamber's proven mobile UX patterns (edge-swipe drawer, back-layer stack, single-scrollport question dock/sheet, `overflow-anchor: none`, explicit follow state machine, throttled streaming Markdown) without dragging in OpenCode or Node at runtime.
  Date/Author: 2026-11-25 / Parent

- Decision: Use npm (matching `mise.toml` Node 24 and `tests/e2e/web/package-lock.json`) and check built output into `mj-server/src/web/dist/`, embedded via a pure-Rust `mj-server/build.rs`.
  Rationale: Keeps `cargo build`, `mbx`, `crates.io` publication, Docker daemon image builds, and cross-platform release builds completely independent of Node, while avoiding edits to `.github/workflows/` (which repo-scoped PATs cannot push). `tests/e2e/web`'s existing CI job (`ci.yml:414-435`) already has Node 24 + Playwright and continues to validate the shipped `mj-server/src/web/dist/` bundle automatically.
  Date/Author: 2026-11-25 / Parent

- Decision: Add an optional, additive `conversation_seqs: BTreeMap<String, u64>` map to `ViewerFrame::Snapshot` and `ViewerFrame::Delta` (populated from `state.conversation_rx` inside the existing `viewer_events` SSE loop) rather than adding a second SSE stream per conversation.
  Rationale: `CompressionLayer` skips `text/event-stream`, so streaming full Markdown bodies over SSE would increase cellular bandwidth 4-5x. Meanwhile `GET /api/v1/conversations/{id}?after_seq=N&presentation_key=K` is already brotli-compressed and already supports incremental `after_seq` filtering. Publishing only changed `(session_id, latest_seq)` pairs on the existing `runtime` SSE stream costs a few bytes per tick and lets the SPA skip 100% of unnecessary conversation fetches when other sessions tick or when returning to an unchanged cached session.
  Date/Author: 2026-11-25 / Parent

- Decision: Render the transcript with keyed `React.memo` rows, lazy-mounted expanded tool/diff bodies, `overflow-anchor: none`, an explicit `'following' | 'detached'` scroll state machine, and synchronous `useLayoutEffect` prepend compensation instead of an unpatched JS virtualizer.
  Rationale: Avoids iOS Safari momentum-scroll height-estimate jumps while completely fixing the root causes of current scroll jitter (`replaceChildren` DOM nuking, unkilled browser `overflow-anchor`, `body` height resizing on keyboard inset, and `do/while` history scroll loops). Row components keep clean boundaries so a virtualizer can be dropped in later only if real measurements show DOM size becoming a bottleneck.
  Date/Author: 2026-11-25 / Parent

- Decision: Use `react-markdown` + `remark-gfm` + `rehype-sanitize` + `lucide-react` + Tailwind CSS v4 (compiled statically) with zero runtime CSS-in-JS or inline `<style>` injection, and route-level `React.lazy()` code splitting for Tier 2 views.
  Rationale: Keeps the initial mobile boot chunk lean (<150 KiB brotli) for cellular loads, preserves `mj-server`'s strict `style-src 'self'; script-src 'self'` CSP with zero exceptions, and eliminates unused fonts (`rajdhani*`, `staatliches*`) while serving `jetbrains-mono` as a content-hashed immutable asset.
  Date/Author: 2026-11-25 / Parent

- Decision: Do not add backwards-compatibility shims for legacy `/viewer.js` routes or stale tabs, and do not build complex reconnect/handoff retry machinery for UI-only follow-up streams (such as wiki sync watch).
  Rationale: Per Mjolnir engineering policy, a page reload recovers stale browser tabs, and UI follow-ups show an error and recover on reopen. Real compatibility is preserved on `/api/v1/*` and `/auth/*` wire contracts.
  Date/Author: 2026-11-25 / Parent

## Outcomes & Retrospective

All four milestones are complete and validated end-to-end:

- Replaced the 11,602-line `mj-server/src/web/viewer.js` vanilla-JS monolith, `viewer.css`, `markdown.js`, `tool-output.js`, unused fonts, and brittle string-slicing tests with a modular React 19 + TypeScript + Vite 6 + Tailwind CSS v4 + Zustand 5 SPA under `web/` compiled into pre-built static assets under `mj-server/src/web/dist/` (~936 KB total uncompressed; ~101 KiB gzipped initial shell with route-level code splitting).
- Embedded `mj-server/src/web/dist/` into `brokk-mj-server` via a pure-Rust `mj-server/build.rs` with zero Node dependency during Cargo builds, releases, or crates.io packaging, serving `/assets/*` with `Cache-Control: public, max-age=31536000, immutable`, `/service-worker.js` with `no-store`, and strict `default-src 'none'; script-src 'self'; style-src 'self'` CSP.
- Added additive `conversation_versions` (`{ latest_seq, presentation_key }`) tracking to `ViewerFrame::Snapshot` and `ViewerFrame::Delta` (`mj-server/src/server/viewer_feed.rs`, `handlers.rs`) and an in-memory 16-session LRU transcript cache in `useTranscriptStore` (preserving exact server Rich-presentation row order) so cellular clients avoid `/auth/session` and `/options` on boot, skip conversation fetches on unrelated SSE deltas, wake immediately on same-sequence topology changes, and render unchanged revisited conversations instantaneously with zero requests.
- Fixed conversation scroll jitter with keyed memoized rows, throttled streaming Markdown, `overflow-anchor: none`, explicit `following | detached` state transitions, tall-reply auto-follow pause clamped to the reply start, visibility/focus-gated read receipts, and synchronous `useLayoutEffect` prepend compensation; replaced nested `30dvh`/`60dvh` `<pre>` elicitations with a single-scrollport dock and full-height Markdown + preview sheet preserving drafts across peeks and handling all companion roles.
- Validated with `web/` (`tsc --noEmit`, 129/129 Vitest tests, `vite build`), `brokk-mj-server` (`mbx clippy --all-targets -- -D warnings`, 302/302 Rust tests, Node-less `mbx check`, `mbx check -p brokk-mj-daemon`), and `tests/e2e/web` (7/7 Node unit tests, 28/28 strict-CSP Playwright deterministic tests, plus 25/25 passing on `--repeat-each=5` of `conversation-scroll.spec.js`).

## Context and Orientation

Mjolnir's HTTP server and embedded web UI live in the `brokk-mj-server` crate (`mj-server/`). The server (`mj-server/src/server/`) serves two classes of routes:

1. Static browser assets (`mj-server/src/server/assets.rs` and `routes.rs:118-133`), currently embedded via `include_str!` / `include_bytes!` from `mj-server/src/web/`, `mj-server/src/icons/`, and `mj-server/src/fonts/`. Every response passes through `upgrade_admission` (returning `503` with `retry-after: 1` and `x-mj-upgrade: pending` during a daemon upgrade handoff), `security_headers` (enforcing strict CSP and `Cache-Control: no-store` on `/api/` and `/auth/`), `finalize_static_etag` (recomputing strong SHA-256 ETags after compression and answering `If-None-Match` with `304`), and `CompressionLayer` (gzip + brotli).
2. Live auth and API routes: `/auth/session` (`GET`/`POST`/`DELETE`), `/auth/login?token=` (`GET`), and `/api/v1/*` (`mj-server/src/server/api/routes.rs`, `handlers.rs`). The browser authenticates via a 6-digit viewer code or QR login token and receives an `HttpOnly; SameSite=Strict` HMAC session cookie.

Real-time state flows through `GET /api/v1/viewer/events?format=changes&since=<incarnation>:<seq>` (`mj-server/src/server/handlers.rs:1170` and `viewer_feed.rs`), which emits SSE events named `runtime` whose JSON `data` is an internally tagged `ViewerFrame` (`kind`: `"snapshot"` | `"delta"` | `"reset_required"`). The snapshot format is produced by `mj-server/src/server/viewer_wire.rs`: live sessions receive full `ViewerSession` rows; non-live/suspended sessions receive summary rows (`detail: false` plus `queued_prompt_count`, `pending_elicitation_count`, `pending_elicitation_ids`), and five repeated fields (`available_commands`, `capabilities`, `config_options`, `compatible_resume_targets`, `incompatible_resume_targets`) are interned by 16-hex SHA-256 key (`{field}_ref` pointing into `snapshot.interned` / `delta.interned`). Full detail for a summary row is fetched via `GET /api/v1/sessions/{id}/row` (returning `{ revision, row }` with interned refs already inlined).

A session's live conversation transcript is projected server-side into `BrowserTranscript` (`mj-client/src/web.rs:12-73`) covering up to 512 stored items (`PROJECTION_TAIL_ITEMS`) and served at `GET /api/v1/conversations/{id}?after_seq=&presentation_key=` (`handlers.rs:748-796`). Each `BrowserTranscriptEntry` has a stable identity (`stable_id ?? "ordinal:" + id`), `updated_seq`, `role`, `label`, `lines`, `disclosures` (pre-structured collapsed/expanded sections), `glyph`, `tone`, `tool_status`, `diffstats`, and `tool_detail_trimmed`. Older items before the 512-item window page via `GET /api/v1/sessions/{id}/history?before_position=&before_id=` (`turns.rs:17-44`). Full trimmed tool output is fetched lazily via `GET /api/v1/sessions/{id}/items/{stable_id}/detail`.

Most browser mutations go through `POST /api/v1/actions` with a `ControllerAction` JSON body (`mj-server/src/server/actions.rs`, internally tagged by `"action"` in kebab-case with `deny_unknown_fields`), returning HTTP `202` with an empty body on acceptance (except `set-container-settings`, which may return a `ContainerUpgradePreview` JSON object). Session destroy uses `POST /api/v1/sessions/{id}/destroy` because `ControllerAction::Destroy` is `#[serde(skip)]`. Browser controls are gated strictly by `ViewerSessionCapabilities` flags (`mj-server/src/server/viewer_types.rs:1315-1358`), never re-derived client-side.

Elicitations (structured questions or tool-permission prompts) are `mj_core::elicitation::ElicitationRequest` values (`mj-core/src/elicitation.rs:28-40`) delivered in `session.pending_elicitations` on detail rows and answered via `POST /api/v1/actions` with `action: "respond-elicitation"`. Each field has `id`, `title`, optional `description`, `required`, `secret`, optional companion fields `custom_answer_for` / `custom_answer_option` (where absent = Alternative, `"\u001falongside"` = Alongside, and any other option value = Detail), and a flattened `kind` (`text`, `single_select`, `multi_select`, `boolean`, `integer`, `number`). Select options have `{ value, title, description?, preview? }`.

Browser tests live in `tests/e2e/web/` (`package.json`, `playwright.config.js`). CI job `web-viewer` runs `cd tests/e2e/web && npm ci && npm test`, which executes Node unit tests and Playwright's `deterministic` project (all `*.spec.js` except `reliability.spec.js`, mocking `/api/v1` and `/auth` routes against the static viewer assets). The live end-to-end test `tests/e2e/web/reliability.spec.js` (`project: lab`) is driven by `tests/e2e/run-browser-reliability.sh` against a real `mj` daemon. Rust tests in `mj-server/src/server/tests.rs` test HTTP routes, auth, compression, caching, CSP, wire projection, and currently also slice JS functions out of `viewer.js` via `viewer_source(...)`.

## Plan of Work

### Milestone 1: SPA Foundation, Typed API/Store Layer, Auth, and Mobile-First Shell

Create the standalone React 19 + TypeScript + Vite 6 + Tailwind CSS v4 + Zustand 5 workspace under `web/` (using npm, Node 24). Configure Vite (`web/vite.config.ts`) to emit production builds directly into `../mj-server/src/web/dist` with content-hashed assets under `/assets/`, no inline scripts or styles (strict CSP compliance), and static public files copied from `web/public/` (`manifest.webmanifest`, `service-worker.js`, PWA icons copied from `mj-server/src/icons/`). Bundle `jetbrains-mono.woff2` (imported via CSS from `web/src/assets/fonts/jetbrains-mono.woff2` so Vite hashes it under `/assets/` with immutable caching).

Author complete, strict TypeScript wire types in `web/src/api/types.ts` verified directly against the Rust source (`mj-server/src/server/viewer_types.rs`, `actions.rs`, `handlers.rs`, `turns.rs`, `mj-client/src/web.rs`, `mj-core/src/elicitation.rs`, `mj-core/src/targets.rs`, `mj-core/src/activity.rs`, `mj-core/src/suspend_consent.rs`, `mj-client/src/runtime_feed.rs`), including the exact `AdditionalMount` wire normalization (`access ?? (read_only ? 'ro' : 'cow')` on read, archive-compatible `{ source, destination, read_only, access? }` on write).

Build the HTTP + SSE transport layer in `web/src/api/client.ts` and `web/src/api/events.ts`:
- Typed `apiFetch` helper handling JSON errors, `401` (transitions auth state to signed-out), `503` with `x-mj-upgrade: pending` (clean upgrade banner + retry), and `POST /api/v1/actions` (handling empty `202` and `set-container-settings` JSON previews).
- Zero-waste boot sequence: skip `/auth/session` and `/options` on initial load; fire `GET /api/v1/snapshot` directly (and if the initial hash route is `#/conversation/{id}`, fire `GET /api/v1/conversations/{id}` in parallel!). When `/snapshot` returns `401`, render the Auth login view. Open `EventSource('/api/v1/viewer/events?format=changes&since=...')` using the snapshot's cursor immediately after snapshot hydration.
- Interning resolver (`resolveRowInterned`) cleanly expanding `*_ref` fields from the store's `interned` map when merging `Snapshot` and `Delta` frames.
- SSE reconnect with exponential backoff (250ms to 30s) passing `since=<incarnation>:<sequence>` and pausing reconnect timers cleanly while the tab is hidden, then immediately resuming when `visibilitychange` returns to visible.

Build Zustand stores under `web/src/stores/`:
- `useSessionStore.ts`: auth state (`checking | signed_out | signed_in`), connection state (`connecting | live | reconnecting | upgrading`), snapshot metadata (`workspaces`, `profiles`, `targets`, `bundles`, `capacity`, `notices`, `launch_failures`, `operator_away`, `server_time_ms`, etc.), `sessionsById`, `detailLoadingIds`, `interned`, `conversationSeqs` (latest server `latest_seq` per session from SSE), and helpers (`applySnapshot`, `applyDelta`, `ensureSessionDetail`, `dispatchAction`).
- `useTranscriptStore.ts`: in-memory LRU cache (capacity 16 sessions) storing per-session `{ entriesById, orderedIds, latestSeq, presentationKey, windowStartSeq, olderItems, historyCursor, hasMoreHistory, loading, loadingOlder, error, pendingLocalPrompts }`. Opening a cached session displays immediately at 0 ms and fetches `?after_seq=<latestSeq>&presentation_key=<key>` only if `serverSeq` is unknown or strictly greater than `latestSeq`. Incoming SSE deltas trigger a coalesced delta fetch ONLY for the currently active conversation AND only when `delta.conversation_seqs[activeId] > cached.latestSeq` (or when a session transitions out of `transitioning` / gains `conversation_available`).
- `useUiStore.ts`: hash router state (`notifications`, `workspace`, `conversation`, `new`, `resume`, `move`, `targets`, `quota`, `settings`), theme (`system | dark | light`, persisted in `localStorage` without inline pre-paint scripts so CSP stays clean), mobile drawer open state, back-layer stack (`registerBackLayer` / `closeTopmostBackLayer` wired to `popstate` and Escape), and lightweight non-CSP-violating toast queue.

Build the Auth screen (`web/src/features/auth/LoginView.tsx`) supporting 6-digit viewer code submission (`POST /auth/session`), lockout messaging (`429`), and QR-link error notice, plus the responsive App Shell (`web/src/components/shell/AppShell.tsx`, `MobileHeader.tsx`, `NavigationDrawer.tsx`, `DesktopSidebar.tsx`, `BottomNav.tsx`, `useEdgeSwipe.ts`, `useVisualViewportInset.ts`). Write `web/public/service-worker.js` using cache-first for `/assets/*`, network-first with cached fallback for `/`, bypass for `/api/` and `/auth/`, and full Web Push `push` + `notificationclick` support.

At the end of Milestone 1, `cd web && npm install && npm run typecheck && npm test && npm run build` succeeds and produces `mj-server/src/web/dist/index.html` and hashed `/assets/*` bundles with passing Vitest tests covering interning, SSE delta merging, LRU transcript caching, and `conversation_seqs` fetch gating.

### Milestone 2: Tier 1 Daily-Driver Views (Conversation, Elicitations, Inbox, Dashboard)

Implement Tier 1 across two parallel, file-disjoint implementation slices so the user's highest-priority surfaces receive focused engineering attention.

**Slice 2A — Conversation View, Non-Jittery Scroll Engine, Markdown/Tool/Diff Rendering, and Mobile Composer (`web/src/features/conversation/`):**
- Scroll engine (`useConversationScroll.ts` + `TranscriptFeed.tsx`): fixed `h-dvh overflow-hidden flex flex-col` layout; scroller with `overflow-y: auto; overflow-anchor: none; overscroll-behavior: contain; min-width: 1px`. Explicit state machine (`'following' | 'detached'`). Detaches on upward user scroll outside a narrow 32px bottom band OR immediately before any tool/disclosure card expands. While `'following'`, schedules at most one `requestAnimationFrame` scroll-to-bottom per height update, and automatically stops advancing past the top of an active streaming reply taller than the viewport (showing a floating "Jump to bottom" pill with unread/streaming indicator). Fixed 48px "Load earlier messages" header slot so button visibility never shifts content; older page prepends capture `scrollHeight` and `scrollTop` before commit and restore `scrollTop + deltaHeight` synchronously inside `useLayoutEffect`. Debounced `POST /conversations/{id}/read` receipt when `'following'`.
- Keyed memoized rows (`TranscriptRow.tsx`, `MarkdownContent.tsx`, `ToolCard.tsx`, `DiffView.tsx`): `React.memo` keyed by `entry.stable_id ?? "ordinal:" + entry.id`, skipping re-renders when `updated_seq` and local expansion state are unchanged. Throttled streaming Markdown (`useThrottledValue` at 100ms, never dropping text) via `react-markdown` + `remark-gfm` + `rehype-sanitize` with clean code blocks + one-tap copy button. Tool cards render as crisp 1-line rows with status dot, label, duration, and structured `diffstats` pills (`+N -M`); tapping expands in place (`overflow-anchor: none`), renders structured `disclosures`, and lazily loads `GET /sessions/{id}/items/{stable_id}/detail` when `tool_detail_trimmed` is true (rendering full output and `diffs` in a horizontal `<pre>` scroller with line colors).
- Conversation header & panels (`ConversationView.tsx`, `SubagentsSheet.tsx`, `SessionDiffSheet.tsx`, `SessionInfoSheet.tsx`, `ActiveShellsBar.tsx`, `BackgroundTasksBar.tsx`, `ReviewBanner.tsx`, `QueuedPromptsBar.tsx`): capability-gated header actions for Sub-agents/Reviewers (including native sub-agent history via `/native-agents/{child}/history`), Session Diff (`/sessions/{id}/diff?json=true`, file list + unified diff), Rename, Suspend, Restart, Move, Container Settings/Upgrade, and Destroy. Inline cards for active user shells (with Cancel), running background tasks (with Stop), review verdict/roles (with Start Review), and queued prompts (with Remove).
- Mobile-safe Composer (`Composer.tsx`, `CommandPalette.tsx`, `PromptHistoryModal.tsx`): never mutates `body` height; uses `useVisualViewportInset` cleanly on the composer dock without resizing or remounting the textarea across keyboard focus transitions. Supports image & file attachments (`POST /sessions/{id}/attachments?mode=image|file` gated by `prompt_images_supported`), debounced server draft sync (`GET /client-state`, `PUT /draft`), `/command` and `!shell` completion palette from `session.available_commands`, prompt history search (`/prompt-history`), model/effort/config selector (`set-config` respecting `policy_config_keys`), Plan Mode toggle (`set-plan-mode`), optimistic local pending row matched by `command_id`, and Turn Controls (`interrupt-turn`, `interrupt-all`, `turn-control`, `cancel`).

**Slice 2B — Elicitation Sheet/Dock, Notifications Inbox, Dashboard/Workspaces, and Web Push (`web/src/features/elicitation/`, `web/src/features/notifications/`, `web/src/features/dashboard/`, `web/src/features/push/`):**
- Elicitations (`ElicitationDock.tsx`, `ElicitationSheet.tsx`, `ElicitationField.tsx`, `elicitationDrafts.ts`): completely replaces the nested `30dvh`/`60dvh` `<pre>` layout and `body.elicitation-focused` hack. When `session.pending_elicitations` is non-empty, renders an above-composer dock showing high-contrast title, pending count, and a prominent button to open the full-height single-scrollport `ElicitationSheet` (and on phones `< 640px`, auto-opens the sheet once per new elicitation ID unless the user taps "Peek transcript" to collapse it back to the dock while preserving drafts). Inside the sheet: single scrollable body + flex-pinned non-overlapping footer (`Back` / `Next` / `Submit` / `Decline` / `Cancel`); Markdown rendering for `request.message`, `request.description`, `field.description`, and every `option.description`; dedicated collapsible monospace preview block for `option.preview`; full-width tappable option cards with a 20px custom radio/checkbox indicator, `break-words` typography, optional "Recommended" badge, and clean companion custom-answer textareas handling all three Mjolnir `CompanionRole` semantics (Alternative, Alongside `"\u001falongside"`, and Detail). Validates required fields, min/max lengths, numeric bounds, and multi-select item limits before submitting `POST /api/v1/actions` (`respond-elicitation`). Drafts persist in `sessionStorage` per `(session_id, elicitation_id)` across connection drops and transcript peeks.
- Notifications Inbox (`NotificationsView.tsx`): shows all sessions needing attention (`needs_input`, `important`, `failed`, active `notification_card`), grouped clearly with badge, timestamp, preview, lazy full-message expansion (`GET /sessions/{id}/notification-message`), one-tap Jump to Conversation (or Answer Elicitation directly), Dismiss (`dismiss-notification`), Ignore toggle (`set-session-notifications-ignored`), AFK / Operator Away toggle (`set-operator-away` + periodic `record-operator-activity`), and Web Push toggle/test banner (`web/src/features/push/useWebPush.ts` talking to `/push/key`, `/push/status`, and `push-subscribe`/`push-renew`/`push-unsubscribe`/`push-test`).
- Dashboard & Workspaces (`DashboardView.tsx`, `SessionCard.tsx`): workspace selector (including virtual Sub-agents and Reviewers workspaces matching Mjolnir's model), live/starting/suspending/failed session cards with status dot, activity detail, attention badge, unread count, elicitation/queue indicators, sub-agent count badge, quick open, and capability-gated overflow actions. Also displays runtime `notices` and `launch_failures`.

At the end of Milestone 2, `cd web && npm run typecheck && npm test && npm run build` succeeds with Vitest unit tests covering elicitation companion roles/validation/drafts, scroll-follow state transitions, transcript deduplication/merging, and markdown/tool rendering.

### Milestone 3: Tier 2 Code-Split Views (New, Resume/Wiki, Move, Targets, Quota, Settings, Lifecycle Dialogs)

Implement all Tier 2 views as `React.lazy()` route chunks in two parallel, file-disjoint slices so the initial mobile conversation bundle stays minimal:

**Slice 3A — New Session, Resume & Session Wiki, and Move Session (`web/src/features/new-session/`, `web/src/features/resume/`, `web/src/features/move/`):**
- New Session (`NewSessionView.tsx`): lazy-loads `GET /api/v1/options` and `GET /api/v1/projects`; supports project selection, discovery (`POST /projects/discover`), adding/removing project bundles (`POST /bundles`, `DELETE /bundles/{id}`), directory path completion (`POST /paths/complete`), profile/target selection with availability notices, container CPU/memory sizing with host capacity limits, additional mounts editor emitting exact `AdditionalMountRepr` JSON, delegation policy (`native` / `mjolnir` with `/profiles/{id}/subagent-options` / `none`), oracle & review toggles, managed worktree & branch/base/at/no-later-history options, `POST /api/v1/preflight/new` with dirty-repo acknowledgement checkboxes, and `ControllerAction::New` submission.
- Resume & Session Wiki (`ResumeView.tsx`): tab/toggle between Suspended Sessions (list of resumable sessions, profile/target compatibility, `POST /api/v1/preflight/resume` with `RawConversionPreview` confirmation, queue disposition `start`/`discard`, container size/mounts, `ControllerAction::Resume`) and Session Wiki (`POST /wiki/sync` + `GET /wiki/sync/watch` with clean error-and-reopen semantics, `GET /wiki/search`, session brief `/brief` & hit transcript `/hits` viewer, and `POST /wiki/sessions/{id}/restore`).
- Move Session (`MoveView.tsx`): target/profile/resource-allocation/mount/subagent/oracle selection, `POST /api/v1/moves/prepare` showing destination checks, transfer route preview, container upgrade/raw conversion/suspend preflight warnings, keep-dir skip checkboxes (`skip_keep_paths`), interruption acknowledgement, and `ControllerAction::Move` submission.

**Slice 3B — Targets, Quota, Settings, Git Identity, and Lifecycle Dialogs (`web/src/features/targets/`, `web/src/features/quota/`, `web/src/features/settings/`, `web/src/features/lifecycle/`):**
- Targets (`TargetsView.tsx`): target cards showing availability verdicts, availability notes, CPU/memory/storage meters from `snapshot.capacity`, container host limits, and Refresh Capacity / Check Availability actions.
- Quota (`QuotaView.tsx`): per-profile quota windows with progress bars, percent used / remaining, exhaustion-before-reset warnings, live reset countdowns (`five_hour` vs `long` styles), banked resets, stale/error states, and Refresh Quota action.
- Settings (`SettingsView.tsx`): cookie-authenticated server-driven settings navigator over `GET /api/v1/settings`, `POST /api/v1/settings/page`, `POST /api/v1/settings/edit`, and `POST /api/v1/settings/save` rendering all `SettingsControl` variants (`page`, `checkbox`, `choice`, `text`, `read_only`, `hidden`), breadcrumb navigation, add/remove entries, notices/errors, and save/discard controls; plus Git Identity card (`GET/POST /api/v1/git-identity`).
- Lifecycle Dialogs (`SuspendDialog.tsx`, `ContainerSettingsDialog.tsx`, `DestroyDialog.tsx`, `RenameDialog.tsx`, `WorkspacePickerDialog.tsx`): Suspend calls `POST /sessions/{id}/lifecycle/suspend-preflight`, renders untracked files, nested repos, in-flight git operations, blockers, and children-not-handed-back warnings, and submits `ControllerAction::Suspend` with exact PascalCase `UntrackedConsent`/`OperationConsent`; Container Settings/Upgrade supports previewing `set-container-settings` and confirming `upgrade-container` / `cancel-container-upgrade`; Destroy calls `POST /api/v1/sessions/{id}/destroy` after explicit confirmation.

At the end of Milestone 3, all nine views compile cleanly into code-split chunks in `mj-server/src/web/dist/`, and `cd web && npm run typecheck && npm test && npm run build` passes.

### Milestone 4: Backend Cutover, Legacy Cleanup, Verification, and Review

1. **Additive `conversation_seqs` in `mj-server/src/server/viewer_feed.rs` and `handlers.rs`:**
   - Pass current session `(id -> latest_seq)` snapshot from `state.conversation_rx` into `ViewerFeed::encode`.
   - Include `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")] conversation_seqs: BTreeMap<String, u64>` on `ViewerFrame::Snapshot` (all current sessions) and `ViewerFrame::Delta` (only sessions whose `latest_seq` changed since the previous frame).
   - Add a Rust unit test in `viewer_feed.rs` confirming unchanged sessions are omitted from delta `conversation_seqs` and changed sessions are included.

2. **Pure-Rust Embedded Asset Pipeline (`mj-server/build.rs`, `assets.rs`, `routes.rs`):**
   - Add `mj-server/build.rs` (zero external dependencies) that walks `src/web/dist/`, emits `cargo:rerun-if-changed=src/web/dist`, infers MIME types (`.html`, `.js`, `.css`, `.svg`, `.png`, `.woff2`, `.webmanifest`, `.json`), assigns cache policy (`Immutable` for paths under `/assets/`, `NoStore` for `/service-worker.js`, `NoCache` otherwise), and writes `$OUT_DIR/web_assets.rs` containing `include_bytes!` entries.
   - Update `mj-server/src/server/assets.rs` to serve `/` and `/login` from embedded `/index.html`, `/assets/{*path}` with `Cache-Control: public, max-age=31536000, immutable` (plus strong ETag), and root PWA files (`/manifest.webmanifest`, `/service-worker.js`, `/icon*.png`, `/icon.svg`, `/apple-touch-icon.png`) while keeping `CONTENT_SECURITY_POLICY`, `finalize_static_etag`, and `security_headers` intact.
   - Update `mj-server/src/server/routes.rs` to replace the legacy `/viewer.js`, `/viewer.css`, `/markdown.js`, `/tool-output.js`, `/push-sync.js`, `/fonts/jetbrains-mono.woff2` routes with `/assets/{*path}` and root asset routes.

3. **Delete Legacy Frontend Files & Brittle Slicing Tests:**
   - Remove `mj-server/src/web/viewer.js`, `viewer.css`, `viewer.html`, `markdown.js`, `tool-output.js`, `push-sync.js`, `test-dom.js`, `manifest.webmanifest`, `service-worker.js` (top-level legacy copies; the built ones now live in `mj-server/src/web/dist/`), and unused fonts `mj-server/src/fonts/rajdhani-*.woff2`, `staatliches-400.woff2` (and `jetbrains-mono.woff2` once copied into `web/src/assets/fonts/`).
   - Remove the `viewer_source(...)` string-slicing tests in `mj-server/src/server/tests.rs` (lines 1624-1668 and their call sites that parse `viewer.js` text), while preserving and updating all Rust HTTP/security/compression/caching/API/golden tests (and adding explicit assertions that `/assets/*.js` serves `Cache-Control: public, max-age=31536000, immutable`, `/service-worker.js` serves `no-store`, `/` serves `no-cache` + strict CSP, and brotli/gzip compression works).
   - Replace `tests/e2e/web/viewer*.unit.test.mjs` (which sliced `viewer.js`) with a lean check in `tests/e2e/web/` (or run `web/` Vitest tests from `tests/e2e/web/package.json`? Keep `tests/e2e/web/package.json`'s `npm test` self-contained without needing `web/node_modules` installed in CI `web-viewer` job, because `ci.yml:419` only runs `npm ci --prefix tests/e2e/web`! So `tests/e2e/web/` unit tests test `dist/` asset integrity + `lab-env.js` helpers with zero extra deps, while Playwright tests exercise the full compiled `mj-server/src/web/dist/` app!).

4. **Upgrade Playwright Deterministic + Lab Specs (`tests/e2e/web/`):**
   - Create a shared static-dist mock server helper (`tests/e2e/web/dist-server.js`) that serves `mj-server/src/web/dist/` with the EXACT `CONTENT_SECURITY_POLICY` and `Cache-Control` headers `mj-server` sets, so every Playwright spec verifies real CSP compliance (failing on any `console.error` or CSP violation!).
   - Rewrite the deterministic Playwright specs to cover the full new SPA AND add targeted high-value regression tests for the user's three priorities:
     - `conversation-scroll.spec.js` (phone 390×844 viewport): streaming updates do not rebuild earlier rows or yank a detached reader; expanding a tool card releases follow and keeps the card header anchored; prepending earlier history preserves viewport position without jump; streaming past viewport height pauses auto-follow and displays the jump-to-bottom pill.
     - `elicitation.spec.js` (phone 390×844 viewport): single scroll container (no nested scrollports), Markdown rendered in prompt and option descriptions, `option.preview` rendered, high-contrast title, full-width tappable options without mid-word breaks, focusing custom textarea does not hide transcript container or crash layout, companion Alternative/Alongside/Detail submission sends valid `respond-elicitation` payloads, and draft survives peek/reopen.
     - `cellular-performance.spec.js`: boot does not request `/auth/session` or `/api/v1/options`; SSE delta for *another* session triggers zero `GET /conversations/{id}` requests; SSE delta advancing `conversation_seqs[activeId]` triggers exactly one incremental `GET /conversations/{id}?after_seq=...`; switching away and back to an unchanged cached session triggers zero conversation fetches; hashed `/assets/*` served with `immutable`.
     - Core surface specs: `auth-and-boot.spec.js`, `dashboard-and-inbox.spec.js`, `composer-and-plan-mode.spec.js` (attachments, queued prompts, plan mode, model picker, commands), `new-session.spec.js`, `resume-and-wiki.spec.js`, `move-targets-quota-settings.spec.js`.
     - Update `reliability.spec.js` selectors/flows to work against the new SPA while preserving the `lab-env.js` contract.

5. **Validation, Review, and Push:**
   - Run `cd web && npm run typecheck && npm test && npm run build`.
   - Run `cargo test -p brokk-mj-server` (elevated, outside sandbox, per AGENTS.md).
   - Run `cd tests/e2e/web && npm test` (elevated, outside sandbox, using bundled Playwright Chromium).
   - Verify bundle size of `mj-server/src/web/dist/` (confirm well under crates.io 10 MiB limit and initial boot chunk < 200 KiB brotli).
   - Commit cleanly on `master`, run `request_review`, resolve any P1s/real regressions (avoiding endless P2 churn per repo policy), and push to `origin/master`.

## Concrete Steps

All commands run from the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/3e575a3b0fb9e0ff96ce6a225fec75da` unless prefixed with `cd`.

1. Milestone 1 bootstrap:
       cd web && npm install
       cd web && npm run typecheck && npm test && npm run build

2. Milestones 2–3 iterative verification (run by owning slice workers or parent after integration):
       cd web && npm run typecheck && npm test && npm run build

3. Milestone 4 full verification (elevated for `cargo test` and Playwright):
       cd web && npm run typecheck && npm test && npm run build
       cargo test -p brokk-mj-server
       cd tests/e2e/web && npm test

4. Bundle size verification:
       du -sh mj-server/src/web/dist && ls -lh mj-server/src/web/dist/assets

## Validation and Acceptance

Acceptance is verified by concrete observable behaviors:

1. **Zero Node at Cargo build time:** Running `PATH=/usr/bin:/bin cargo check -p brokk-mj-server` (without Node on `PATH`) succeeds cleanly using the checked-in `mj-server/src/web/dist/`.
2. **Rust server suite green:** `cargo test -p brokk-mj-server` passes with 0 failures, verifying `ViewerFrame` `conversation_seqs` deltas, immutable `/assets/*` caching, `no-store` service worker, brotli/gzip compression, auth, and strict CSP.
3. **Frontend unit & type checks green:** `cd web && npm run typecheck && npm test && npm run build` passes with 0 TypeScript errors and all Vitest tests green.
4. **Playwright deterministic suite green with zero CSP/console errors:** `cd tests/e2e/web && npm test` passes every spec under headless Chromium, specifically proving:
   - Conversation view on a 390×844 phone viewport keeps scroll position steady across streaming deltas, tool card expansion, and older history prepends.
   - Elicitation sheet renders Markdown prompts, option descriptions, and `option.preview` inside a single scroll container with full-width options, preserves drafts on peek/reopen, and submits exact `ElicitationResponse` payloads for all field kinds and companion roles.
   - Cellular request budget holds: zero `/auth/session` or `/options` fetches on signed-in boot, zero conversation refetches on unrelated SSE deltas, zero conversation refetches on switching back to an unchanged cached session, and `immutable` caching on `/assets/*`.
   - All 9 Mjolnir views render cleanly without horizontal overflow on phone (390×844) and desktop (1280×800), with strict CSP enforced and zero console/page errors.

## Idempotence and Recovery

`web/` builds cleanly and deterministically into `mj-server/src/web/dist/` (`vite build` sets `emptyOutDir: true`). Re-running `npm run build` is completely idempotent. Until Milestone 4 cuts over `mj-server/src/server/assets.rs` and `routes.rs`, the existing Rust crate and legacy tests remain completely intact and runnable. If any Milestone 4 step fails mid-way, `git checkout -- mj-server/` restores the server cleanly without losing `web/`.

## Artifacts and Notes

Key reference files from reconnaissance (stored in agent report directories, distilled into this ExecPlan):
- Wire contract sources: `mj-server/src/server/viewer_types.rs`, `viewer_wire.rs`, `viewer_feed.rs`, `handlers.rs`, `actions.rs`, `turns.rs`, `assets.rs`, `routes.rs`, `mj-client/src/web.rs`, `mj-core/src/elicitation.rs`.
- AdditionalMount wire rule (`mj-core/src/targets.rs:406-445`): read `mount.access ?? (mount.read_only ? 'ro' : 'cow')`; write `{ source, destination, read_only: true }` (ro), `{ source, destination, read_only: false }` (cow), `{ source, destination, read_only: false, access: 'rw' }` (rw) because serde uses `deny_unknown_fields`.
- `ControllerAction::Destroy` is `#[serde(skip)]`; destroy always uses `POST /api/v1/sessions/{id}/destroy`.
- `UntrackedConsent` and `OperationConsent` serialize PascalCase externally tagged (`"None"`, `"Any"`, `{"Shown": [...]}`).

## Interfaces and Dependencies

Frontend package (`web/package.json`):
- Runtime dependencies: `react` (^19.1), `react-dom` (^19.1), `zustand` (^5.0), `react-markdown` (^10.1), `remark-gfm` (^4.0), `rehype-sanitize` (^6.0), `lucide-react` (^0.511).
- Dev dependencies: `vite` (^6.3), `@vitejs/plugin-react` (^4.5), `tailwindcss` (^4.1), `@tailwindcss/vite` (^4.1), `typescript` (^5.8), `@types/react`, `@types/react-dom`, `vitest` (^3.1), `@testing-library/react` (^16.3), `@testing-library/jest-dom`, `jsdom`.

Rust generated asset interface (`mj-server/build.rs` → `$OUT_DIR/web_assets.rs`):

    pub(super) enum AssetCache {
        Immutable,
        NoCache,
        NoStore,
    }

    pub(super) struct EmbeddedAsset {
        pub(super) path: &'static str,
        pub(super) content_type: &'static str,
        pub(super) cache: AssetCache,
        pub(super) body: &'static [u8],
    }

    pub(super) fn lookup_asset(path: &str) -> Option<&'static EmbeddedAsset>;

Rust additive SSE delta field (`mj-server/src/server/viewer_feed.rs`):

    ViewerFrame::Snapshot {
        cursor: RuntimeCursor,
        snapshot: Box<Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        conversation_seqs: BTreeMap<String, u64>,
    }

    ViewerFrame::Delta {
        from: RuntimeCursor,
        cursor: RuntimeCursor,
        metadata: Map<String, Value>,
        sessions: Vec<(String, Option<Value>)>,
        detail_changed: Vec<String>,
        interned: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        conversation_seqs: BTreeMap<String, u64>,
    }

Frontend core store contracts (`web/src/stores/`):
- `useSessionStore`: owns auth, SSE lifecycle, snapshot metadata, interning table, `sessionsById`, `conversationSeqs`, `ensureSessionDetail(id)`, `dispatchAction(action)`, `refreshSnapshot()`.
- `useTranscriptStore`: owns per-session LRU transcript cache (max 16), `openConversation(id)`, `syncConversation(id, opts)`, `loadEarlierHistory(id)`, `addOptimisticPrompt(id, cmdId, text)`, `invalidateSession(id)`.
- `useUiStore`: owns parsed hash route, navigation (`navigate`), active workspace selection, theme, mobile drawer state, back-layer stack (`registerBackLayer`, `closeTopmostBackLayer`), and toast notifications.
