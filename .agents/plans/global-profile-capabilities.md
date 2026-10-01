# Share and proactively hydrate profile capabilities


This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture


Settings and Move must immediately reuse models and efforts after background hydration, including after Native/Mjolnir switches, dialog reopening, and session changes. The daemon (the persistent controller process) owns one catalog. Startup and profile-definition changes populate it without blocking UI rendering; consumers join existing work. Profiles change only when IDs, enabled status, harness, home, or configured environment sources change. Defaults, eligibility, credentials, files inside homes, and elapsed time do not invalidate successful capabilities. Settings also must not show the underlying composer's terminal cursor through its modal.

## Progress


- [x] (2026-10-01) Inspected catalog, persistence, Settings, Move, browser, runtime feed, and cursor ownership. Agreed eager models and efforts and definition-based invalidation with user.
- [x] (2026-10-01) Implement authoritative catalog and persistent identity; hydrate all models and efforts with shared attempts and bounded retry.
- [x] (2026-10-01) Publish snapshots to TUI and browser and hydrate unsaved profile definitions through the same owner.
- [x] (2026-10-01) Remove dialog caches and fix cursor focus ownership.
- [x] (2026-10-01) Add counting/gated cache, persistent identity, Settings, Move, Node browser, runtime publication, and terminal cursor regressions.
- [x] (2026-10-01) Focused cursor and running/queued/replacement cancellation regressions pass. Final dev-profile all-target Clippy, formatting, JavaScript syntax, and diff whitespace checks pass.
- [x] (2026-10-01) Complete full dev-profile Cargo suite: all tests pass, including 2,098 controller and 923 TUI tests and isolated daemon startup/upgrade and terminal termination checks.
- [ ] Commit the validated implementation on hel2, integrate origin/master (which advanced through release 2.25.0 during validation), validate the combined result, and push to origin/master (explicit user authorization).

## Surprises & Discoveries


Settings and Move store independent capability caches inside dialogs. The browser clears its request state on policy changes. `ProfileCatalog::check_inputs` hashes whole harness-home config/model/settings files and resolved credentials during reads, and increments one global generation; one changed hash invalidates concurrent reads for unrelated profiles. The four host Codex profiles resolve to the same config file. `DashboardState::prompt_has_focus` checks only the retained pane focus, ignoring modal ownership, so the background composer places a visible terminal cursor through Settings. Persistent capabilities expire after 24 hours. The installed Ratatui version keeps test cursor visibility and backend writer access behind unstable features; the cursor regression therefore observes actual Crossterm show/hide sequences through a small shared output writer. A default-workspace test build passes; the optional desktop workspace member needs unavailable GTK/glib development libraries, so validation uses the documented default members. No build storage was redirected. Settings draft parsing also resolved environment references and could read secret files during projection; a scoped sources-only deserializer prevents that I/O while preserving normal background credential resolution.

## Decision Log


The user chose eager hydration of all model-specific efforts and invalidation on installation-definition changes as well as membership changes. Use configured environment sources, not refreshed secret values, in identity. Keep successful persistent entries without TTL and use a new fingerprint prefix for one-time cold population. Keep all capability knowledge in the daemon catalog; control surfaces retain read-only snapshots outside dialogs. Reuse existing profile subprocess supervision, SQLite table, and runtime/viewer metadata publications. No schema or worker wire migration is needed. Hydration is restartable background preparation and must not hold upgrade admission. On 2026-10-01 the user explicitly authorized pushing completion to origin/master; remain on hel2, with no branch changes, rebase, or PR.

## Outcomes & Retrospective


The implementation now uses profile-definition entry ownership and immutable runtime publications. Saved profiles hydrate on startup/reload; unsaved Settings definitions use a short ensure request against the same owner. Models remain selectable while other efforts hydrate. Successful persistent rows no longer expire. Modal mode now owns composer focus. Config projections deserialize environment sources without reading credentials on the UI loop; background requests resolve those sources normally. Definition cancellation now reaches the shared subprocess supervisor: queued probes refuse before taking the profile lock, running probes receive their own cancellation flag, and replacement definitions do not inherit the retired flag. The complete implementation suite and final all-target Clippy pass; no live/default instance has been used. Upstream integration and delivery remain. The workspace was clean at start; the normal target symlink uses /mnt/optane/mbx-targets and remains unchanged.

## Context and Orientation


`mj-core/src/config/harness.rs` defines the shared profile discovery-input projection; `mj-core/src/subagent.rs` holds selection choices. `mj-controller/src/server_runtime/profile_catalog.rs` owns default and model-specific pending/ready discoveries. `mj-controller/src/controller/profile_config.rs` supervises probes and persists them through `mj-controller/src/database/profile_cache.rs`. `mj-client/src/runtime_feed.rs` carries daemon publications; `mj-controller/src/daemon/feed.rs` captures them, and the remote dashboard worker distributes them to `mj-cli/src/dashboard.rs`. Viewer metadata flows to `mj-controller/src/web/viewer.js`. `mj-tui/src/setup.rs` and `mj-tui/src/wizards/subagents.rs` currently cache independently. `mj-tui/src/dashboard_workspaces.rs` owns the prompt-focus predicate used by combined rendering.

## Plan of Work


Milestone 1 defines an opaque stable profile identity and a serializable snapshot with per-profile model choices and per-model effort states/errors. Replace read-time fingerprint invalidation with entry ownership by identity. Populate defaults first and all models next, reusing default-model effort data. Independent profiles run concurrently. Failures report useful context and retry with bounded backoff; shutdown cancels hydration. Preserve unchanged definitions and reject stale publication by exact entry ownership, without poisoning unrelated reads.

Milestone 2 publishes catalog changes through defaulted runtime and viewer fields and a short request to ensure valid unsaved draft installations are hydrating without adopting their policy as live. Settings, Move, and web derive eligible options from one shared projection and application-level snapshot, including while work is pending. Remove local caches and per-selection discovery requests; retain existing options APIs backed by the same owner. Consumers arriving during hydration wait for publications rather than spawning probes.

Milestone 3 makes prompt focus account for modal keyboard ownership, proves cursor visibility through rendered terminal tests, and validates the advertised behaviors in isolated configurations. Commit coherent validated changes on hel2, then push HEAD to origin/master without force. If upstream has advanced incompatibly, inspect before taking any Git operation outside the user's authorization.

## Concrete Steps


Work from /home/jonathan/Projects/mjolnir2. Use existing mbx Cargo storage. Run every cargo test outside the restricted sandbox with elevated permissions. Run cargo fmt --all -- --check, cargo test, cargo clippy --all-targets -- -D warnings, node --check mj-controller/src/web/viewer.js, and git diff --check. Use a named instance such as --instance global-profile-cache for all new-binary daemon/TUI/CLI tests; never target the default live instance. Stage only changed files and commit on hel2, then git push origin HEAD:master.

## Validation and Acceptance


Counting and gated probes prove startup hydration, all-model effort warming, sharing concurrent requests, retention across policy/eligibility edits, affected-only profile-definition changes, unsaved draft isolation, late-result safety, partial failures and backoff, and cancellation. Persistent tests prove old entries do not expire and home-file/credential changes do not invalidate them. TUI and Node browser tests prove toggles/reopening/session changes use published choices without additional discovery. Rendered Crossterm output tests assert no cursor behind Settings, a cursor in a focused modal editor, and normal composer cursor restoration on dismissal. Existing isolated upgrade regressions remain intact. Full dev-profile Cargo tests and all-target Clippy must pass before delivery.

## Idempotence and Recovery


All tests use existing temporary directories or a named isolated instance. Successful cache data is rebuildable and requires no migration. New opaque identities miss old rows once. Background hydration stops on daemon cancellation and resumes on next startup. Preserve unrelated workspace edits and normal build layout. Do not mutate the host's live configuration or store.

## Artifacts and Notes


Initial branch: hel2. Initial working tree: clean. Existing target symlink: /mnt/optane/mbx-targets/v1/052b140c6340497a8eb82f6fb27467cd6a2e929dce2b8ef8a9cb2a4899842c5d. Validation logs and final commit will be recorded here.

## Interfaces and Dependencies


Add a public `ProfileCapabilitiesSnapshot` and capability-state types in mj-core and defaulted snapshot fields in runtime/viewer metadata. The catalog alone owns shared pending attempts and retry decisions; snapshots are immutable views. Introduce a bounded daemon action ensuring draft definitions are warm. Use one pure eligible-options projection in mj-core for daemon/TUI consumption and equivalent browser projection from sanitized metadata. Preserve `SubagentOptions`, existing options endpoints, subprocess helpers, and SQLite schema.

Revision note: recorded approved design, confirmed invalidation and eager-effort choices, cursor finding, existing cache storage, and explicit push authorization.

Revision note: recorded implemented state ownership/publications, sources-only UI projections, added behavioral regressions, test-backend compatibility fixes, and default-workspace validation scope.

Revision note: cancellation review found that dropping a catalog attempt could leave its supervised subprocess queued behind the per-profile lock. Passed definition tokens through that supervisor and added a running/queued/replacement regression. The cursor regression now dismisses the editor, returns to the Settings root, and dismisses Settings before expecting composer restoration.

Validation update: final all-target Clippy passed in /mnt/optane/mjolnir2-profile-cache-clippy-verified.log. Focused cancellation and cursor checks passed in /mnt/optane/mjolnir2-profile-cache-retirement.log and /mnt/optane/mjolnir2-profile-cache-cursor.log. The full settled-source Cargo run is /mnt/optane/mjolnir2-profile-cache-tests-complete.log. Earlier runs exposed test-fixture mistakes that have been corrected; they are not passing final validation evidence.

Validation update: cargo test completed successfully (exit 0) in /mnt/optane/mjolnir2-profile-cache-tests-complete.log. Origin/master advanced to bd0c941d (release 2.25.0) while checks ran; preserve those commits by committing this validated checkpoint and merging origin/master on hel2, then validate the resulting integration before pushing without force.
