# Unify reviewer settings and provider-aware Auto selection

This ExecPlan follows `.agents/PLANS.md` and must remain current throughout implementation.

## Purpose / Big Picture

Turn review and plan second opinion share Settings → Review. Auto chooses another provider where possible using available quota and fixed model families. Single-profile installations can review with their own credentials in a separate conversation. Second opinion starts without a profile/model/effort picker. Specialist models are selected independently of the main reviewer.

## Progress

- [x] 2026-09-19: Inspected selection, quota, worker configuration, UI, persistence, and session discovery; agreed policy with the user.
- [x] 2026-09-19: Implement shared selection, quota reuse, model resolution, and specialist launch settings.
- [x] 2026-09-19: Integrate turn review, Settings, and baseline capture.
- [x] 2026-09-19: Replace second-opinion picker and remembered choices with shared settings.
- [x] 2026-09-19: Add behavior tests, update documentation, run required validation before upstream integration.
- [ ] Commit on current branch, merge upstream as needed, and push to configured upstream as requested.

## Surprises & Discoveries

Turn review rejects the primary profile, but second opinion already allows it. Reviewer processes have private homes/journals under the primary worker and no session records, so profile identity must never be used to hide ordinary user sessions. Utility inference accepts unknown quota and API billing; Claude can reuse its quota classification without needing a utility inference backend.

## Decision Log

2026-09-19, user: Prefer a different provider, then another same-provider profile, then primary. Within each group use healthy/reserve/unknown quota, provider order Codex/Claude/DeepSeek/Kimi, remaining quota, and profile ID. Known exhausted quota excludes candidates. Other providers remain manual-only where reviewer capabilities permit.

2026-09-19, user: Auto main models are newest Astra/medium, Fable/medium, DeepSeek Flash/max, and Kimi K-series/max. Specialist models are newest Luna/xhigh with fast mode if possible, Sonnet/xhigh, DeepSeek Flash/high, otherwise main settings. Specialist overrides apply even to manual selection. Resolve actual advertised family IDs; efforts remain exact.

2026-09-19, user: Both review flows use shared reviewer settings. Remove the second-opinion selector and workspace defaults. Auto must be explicitly labeled and explained in Settings, though absence of a profile remains its storage representation. Automation remains opt-in. User authorized merge and push after completion.

## Outcomes & Retrospective

Shared resolution, specialist policy, Settings Auto, direct second-opinion preparation, and cancellation are implemented. Focused review tests pass. Full-suite validation passes with `NO_COLOR` unset; upstream integration remains. No database migration was needed.

## Context and Orientation

`mj-core/src/config.rs` owns the review configuration. `mj-controller/src/utility_llm.rs` contains quota freshness/classification and model version ordering. `mj-controller/src/review_host/` runs turn review; `mj-controller/src/review_settings.rs` discovers supported model/effort choices on real workers. `mj-worker/src/worker_runtime/reviewer.rs` runs separate reviewer processes and applies configuration. `mj-chat/src/chat/active/reviewer.rs` and `second_opinion.rs` implement plan-review startup and rendering. `mj-tui/src/review_settings.rs` edits the shared settings. A reviewer is a supervised process attached to a primary worker, not a main session.

## Plan of Work

Milestone 1 introduces shared resolved reviewer settings and provider/model policy, reusing utility quota rules and model ordering. Resolve Auto asynchronously using target-advertised capabilities. Skip unusable Auto candidates before review starts; explicit selections fail visibly. Main and specialist choices remain fixed for the running review. Best-effort fast mode is requested through the existing ACP configuration selector and rejection is reported.

Milestone 2 routes turn review through this resolver, removes the primary-profile refusal, labels Auto in Settings with policy details, and makes baseline capture work for manual review even with automation off. Auto disables/clears manual overrides; invalid hand-written combinations receive an actionable error. Update status and resolved identity display. All network, filesystem, and subprocess work stays off UI and host loops with cancellation and bounded cleanup.

Milestone 3 replaces plan-review selection with immediate preparation, retry, and cancel states. Resolve current shared settings for each new request; start the reviewer before consuming the original plan approval. Preserve transfer, implement-original, cancel, and already-open review restoration. Retire workspace-default reads/writes and picker-only code while leaving the historical database table intact: no database migration. Preserve private reviewer homes and main-session exclusion by construction, not profile filtering.

Milestone 4 adds focused behavior coverage and documentation, validates, commits, merges upstream changes if required, and pushes. The current branch is `hel3`, tracking `origin/master`; do not create branches or rebase. Fetch upstream before final merge/push, merge into the current branch if it diverged, and validate any changed merge result.

## Concrete Steps

From `/home/jonathan/Projects/hel3`, run focused package tests outside the sandbox while implementing. Finish with `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Use normal build storage, never `/tmp`. Review `git diff --check` and stage only changed files. Commit coherent validated changes on the current branch. Push `HEAD:master` to origin after merging any necessary upstream changes; never force-push.

## Validation and Acceptance

Tests must demonstrate quota/provider ordering, unknown/API/exhausted quota, different-provider preference, same-provider and primary fallbacks, DeepSeek identity despite a Codex harness, newest model families, exact effort handling, manual specialist overrides, and fast-mode success/absence/rejection. Settings tests must show Auto explicitly and persist it correctly. Second-opinion tests must show direct startup, stale-result rejection, retry/cancel, settings refresh, and restoration without old defaults. Isolation tests must show reviewer transcripts outside configured profile-home scans and reviewer activity absent from main-session rows. Required tests/clippy must pass before publication; record exact outcomes here.

## Idempotence and Recovery

No live-store migration or destructive cleanup is required. Keep historical defaults tables untouched, but stop consuming them. Existing named review profiles remain valid. Existing workers without review baselines retain the existing explicit restart/resume diagnostic. Provisional discovery processes use existing bounded cleanup. Failed preparation restores the plan decision or releases turn holds. Do not change model/provider during an active review.

## Artifacts and Notes

`cargo test review -- --nocapture` passed across the workspace, including 59 controller and 48 worker tests. `env -u NO_COLOR cargo test -q` passed the full workspace. The initial run failed only color assertions because the runner sets `NO_COLOR=1`; no product change was needed. The latest focused review run passed 60 controller and 48 worker tests, including the new native-session isolation test. Clippy identified one collapsible conditional, now fixed. Upstream gained commit `71ff0559`; merge it into `hel3` and validate the combined result before pushing to `origin/master`.

## Interfaces and Dependencies

Add shared typed resolved settings in `mj-core::review` containing profile ID, main model/effort, specialist model/effort, Auto/fallback provenance, and best-effort fast-mode preference. Add a controller-owned async resolver consumed by the turn host and the client session backend for second opinion. Extend `ReviewerLaunchConfig` with a defaulted optional fast-mode request. Use existing quota collection, provider detection, ACP discovery, and subprocess supervision; no new crate.

Revision note: Initial executable plan created from the approved conversation on 2026-09-19.

Revision note (2026-09-19): Implemented all three functional milestones. New requests use relay protocol 14 so unsupported workers are refused before decoding. Generation-scoped cleanup prevents cancelled second-opinion preparations from stopping replacements. Each newly requested second opinion starts a fresh conversation, while persisted active workflows retain their generation; this avoids carrying a previous reviewer identity across settings changes. Preparation remains visible during ordered persistence and is cancellable. Existing native-session discovery scans configured profile homes, while reviewer homes/journals remain worker-private and create no session records.
