# Share advertised model identity across profile selection

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture

The configured `sonnet/high` subagent choice must draw from both Claude profiles advertising `sonnet` (display name `Sonnet 5.5`) and the Gemini/Antigravity profile advertising `claude-sonnet-5-5`. Both run the same model but require different launch values. Quota, settings, reviews, and usage must share the existing catalog interpretation rather than maintaining alias tables separately.

## Progress

- [x] (2026-11-13) Diagnosed exact-value matching and verified the live catalogs and full Antigravity Claude quota.
- [x] (2026-11-13) Located `mj-core/src/usage/model_catalog.rs`, the existing advertised-alias resolver.
- [x] (2026-11-13) Promoted catalog interpretation to `mj-core/src/model_catalog.rs`, preserving pricing normalization separately.
- [x] (2026-11-13) Resolved one identity before ranking and carried each advertised launch value through effort discovery, health lookup, and launch. Removed the separate omitted-effort selector.
- [x] (2026-11-13) Shared matching and identity grouping with capability settings, reviewer discovery, and autoconfiguration.
- [x] (2026-11-13) Added recorded-catalog regressions for scoped quota, both launch values, stale versions, context variants, unknown IDs, per-profile counting, and a public API golden.
- [x] (2026-11-13) Focused regressions passed: 2 controller tests and 6 core tests; reviewed the generated public API golden.
- [ ] Complete review, full touched-crate suites, and workspace Clippy; Clippy running.
- [ ] Commit, request review, address findings, and push the current branch to origin/master.

## Surprises & Discoveries

The existing resolver uses the price table to decide whether a model value is already a full ID and strips context hints for pricing. General identity must not depend on whether a price has been entered. Explicit context variants must remain separate for selection. The current scheduler has shared entry points for subagents, reviewers, the oracle, and unpinned session launches, so correcting that shared service covers all four.

The public session-start API discarded the selected model and forwarded the original requested alias in its follow-up. It now forwards the selected profile's launch value. Ready reviewer profiles must be filtered for refused logins before resolving aliases, otherwise a rejected account can decide which moving-alias version the remaining profiles must run.

The first focused compile took 7m33s under shared host load. It captured a source snapshot before the pricing adapter's context normalization correction; the existing pricing regression detected exactly that difference. The correction was already on disk, and the next focused run uses the completed implementation.

The second focused run passed all eight selected tests. Its API golden shows HTTP 201 and `model=claude-sonnet-5-5` on Gemini when Claude is exhausted, and `model=sonnet` on Claude when Gemini's Claude pool is exhausted, both with explicit high effort and with omitted effort. Final validation must use the finished tree because a last health-probe ordering correction and deterministic family-fallback tie break were made during this compile.

## Decision Log

Decision: Keep one general catalog module and retain a separate pricing adapter that calls it. Rationale: interpretation belongs to the advertised catalog; price availability must not determine selection. Date: 2026-11-13.

Decision: Resolve an alias once against the combined available catalogs, choosing its newest advertised identity deterministically when profiles disagree, before resolving profile launch values. Rationale: a request must select one version, not mix whichever version each profile's alias currently runs. Explicit IDs never select another version. Date: 2026-11-13.

Decision: Preserve raw advertised launch values and existing storage and wire formats. Rationale: launch and effort probes must speak each harness's vocabulary; no migration or daemon installation is needed to deliver the source change. Date: 2026-11-13.

## Outcomes & Retrospective

Implementation and validation pending. No live settings or installation will be changed by this work.

## Context and Orientation

`mj-core/src/usage/model_catalog.rs` resolves Claude aliases from `SessionConfigChoice` values and names. `mj-core/src/profile_capabilities.rs` publishes the single capability cache to settings. `mj-core/src/review/autoconfigure.rs` merges model offers for reviewer selection. `mj-controller/src/server/api/subagents.rs` selects profiles and efforts for children, reviews, and sessions. `mj-controller/src/profile_scheduler.rs` ranks profiles against model-scoped quota windows. `mj-controller/src/server_runtime/profile_catalog.rs` and `api.rs` discover profiles, including a fast ready-only reviewer path. `mj-controller/src/controller/profile_config.rs` builds subagent choices.

Here an identity means the actual advertised model version, such as `claude-sonnet-5-5`; a launch value means the exact value the selected profile's harness accepts, such as `sonnet`. An offer pairs those values with a configured profile. A capability snapshot is the existing read-only publication of the daemon's discovered models and efforts.

## Plan of Work

Move reusable interpretation into `mj-core/src/model_catalog.rs`. Give it shared request resolution and offer matching functions. Pricing delegates alias interpretation to that module and retains its deliberate context-premium exclusions. Resolve requested aliases over the available model lists before weighted ranking, then preserve the matching launch value when probing efforts and creating sessions. Score quota using the resolved model identity. Use the same interpretation in settings and review paths, including grouping aliases once per profile for autoconfiguration. Preserve unknown opaque IDs and explicit context variants.

## Concrete Steps

Work in the current repository and current branch. Do not alter Cargo target storage or mbx configuration. Use `cargo fmt --all`, focused `cargo test -p brokk-mj-core -p brokk-mj-controller <filters>` during implementation, then full touched-crate suites and `cargo clippy --all-targets -- -D warnings` on the dev profile after the final round. Every cargo test invocation runs outside the restricted sandbox. Existing automated tests retain their isolated directories; any manual new-build invocation uses `--instance model-identity-test`.

## Validation and Acceptance

The regression must show a `sonnet/high` request selecting Antigravity with nonempty Claude-scoped quota over an exhausted Claude profile, and launching `claude-sonnet-5-5`. Reversing quota must launch `sonnet` on Claude. A profile offering Sonnet 5 cannot satisfy an explicit Sonnet 5.5 request. Duplicate aliases on one profile must not double-count that profile. Context variants must not satisfy an explicitly different context request. Existing usage report and review goldens must continue to pass, except intentional model-offer grouping changes whose expected output is reviewed.

## Idempotence and Recovery

All source edits are reversible and no persistent store changes are required. Run checks through the normal mbx-backed Cargo setup. Stage only files changed for this task. If origin/master advances, fetch and merge it into the current branch, resolve conflicts, and push without rebasing. Do not install without a user request.

## Artifacts and Notes

Observed live input: `[subagents.subtle] model = "sonnet", effort = "high"`; Claude advertises `sonnet` with name `Sonnet 5.5`; Gemini advertises `claude-sonnet-5-5` with name `Claude Sonnet 5.5`. Gemini's Claude pool has 100% remaining in both weekly and five-hour windows and is scoped `claude,gpt`.

## Interfaces and Dependencies

Use `SessionConfigChoice` and existing `ProfileConfig` lists as inputs to the general catalog module; keep `ModelCatalogs` for usage as a thin caller. Shared matching returns an advertised choice by resolved identity. Profile selection carries identity and launch value together, using existing controller candidate types or a small adjacent resolved-offer type. No new crate or external dependency is needed.

Initial plan recorded 2026-11-13 after the user approved the shared-identity design.

Updated 2026-11-13 with completed source work, regression coverage, the API follow-up discovery, and ready-profile resolution ordering.

Updated 2026-11-13 with the eight passing focused checks and the reviewed API golden. Workspace Clippy and final full validation remain pending.
