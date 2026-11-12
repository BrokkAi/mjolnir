# Weight subscription scheduling by monthly dollars

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A Claude $100 Max account must receive half the assignments of a $200 Max account at equal remaining percentage and reset timing. The user explicitly chose monthly dollars as a capacity proxy because advertised five-hour capacity multipliers misrepresent relative weekly capacity. Existing quota exhaustion, model scopes, reset pacing, and paid-credit priority remain authoritative.

## Progress

- [x] (2026-11-12) Claimed issue #1519 and verified the host's Claude credentials distinguish Max 5x and Max 20x.
- [x] Added optional monthly USD list-price metadata to quota reports and propagated it to the shared candidate scheduler.
- [x] Validated compilation and extended captured-credential, cache-persistence, and deterministic assignment regressions.
- [x] Changed model-first session creation to weighted draws with and without explicit effort, and extended the public API regression.
- [x] Full client (57 including integration), controller (2574), and TUI (581) suites passed; 50 focused checks passed and the corrected CLI regression passed separately. Final workspace Clippy, formatting, and diff checks passed.
- [ ] Commit, review, and push to origin/master.

## Surprises & Discoveries

Claude profile `.claude5` carries `default_claude_max_5x`; the other inspected host logins carry `default_claude_max_20x`. The existing `subscriptionType` alone says only `max` and cannot distinguish them. Candidate scoring is shared by launches, reviewers, and recovery. Unknown plans currently have no capacity metadata.

## Decision Log

Use $100 and $200 list prices rather than 5 and 20 multipliers, as explicitly requested by the user. Recognize Claude Pro's $20 monthly list price as well. Store price on `mj_client::quota::ProfileQuota`, the existing serialized quota report, so persistence and every consumer agree; use optional serde metadata to preserve older readers and cached reports. No store migration is required because old readers ignore this optional scheduling metadata and updates may safely replace it. Known prices multiply the existing score by price/100; absent or zero prices retain the historical unit weight. The user also requested `mj new --model` without a profile to load balance. The CLI already accepts that syntax, but its session source used deterministic ranking. Switch both effort-selection paths to the shared weighted draw and exercise both paths through repeated isolated HTTP requests, retaining explicit-profile selection. Detection for other providers is outside this Claude-account change; their existing weights remain in effect. On macOS reuse the existing asynchronous Keychain reader after a successful CLI quota query. Refresh retries use the credentials actually used for the successful API request.

## Outcomes & Retrospective

Monthly-dollar weighting is implemented for recognized Claude plans, and both model-first session selection paths use the same weighted draw as reviewers and children. All required checks passed. The new API regression originally stalled because its fake snapshot omitted the second account; adding that account and observing early API responses corrected the fixture, and its rerun passed in 0.07 seconds. Nothing is installed into the live instance by this task.

## Context and Orientation

`mj-controller/src/claude_usage.rs` reads Claude OAuth credentials and usage. `mj-controller/src/quota.rs` publishes its result as `ProfileQuota` from `mj-client/src/quota.rs`. `mj-controller/src/profile_scheduler.rs` converts reports into candidates, ranks them, and performs weighted random draws. `mj-controller/src/server/api/subagents.rs` contains existing deterministic distribution tests. `mj-controller/src/database/quota_cache.rs` persists reports as JSON.

## Plan of Work

First propagate monthly price metadata from the login tier into reports and candidates. Then multiply only the included-subscription score by relative price; paid credit balances remain their own dollar weights. Extend existing query, persistence, and distribution regressions rather than adding small example tests. Finish with normal mbx-backed development checks and repository review/push.

## Concrete Steps

From the repository root, run `cargo fmt --all -- --check`, targeted `cargo test -p brokk-mj-controller --lib -- claude_usage:: server::api::subagents:: database::quota_cache::`, then the full touched-crate suites and `cargo clippy --all-targets -- -D warnings` at completion. Every cargo test runs with elevated permissions; tests retain their existing temporary homes and stores. No test binary operates on the default live instance. Do not redirect Cargo output or install a new binary.

## Validation and Acceptance

The deterministic distribution regression must select the $200 account about half the time against two $100 accounts, each selected about a quarter, when all three report 90% remaining. Paid funds must still sort behind included quota and their own selection weights must remain independent of subscription price. HTTP quota tests must read the actual tier metadata without launching Claude when credentials are valid. Cache tests must preserve price across reopen and accept changed prices on a fresh report.

## Idempotence and Recovery

All code changes and tests are reversible. Optional report metadata is readable by older versions and absent in old caches. Unknown tiers retain historical weights. Tests use isolated temporary stores and fake HTTP services; no live login is changed. Revert the implementation commit to undo this feature.

## Artifacts and Notes

Tracking issue: https://github.com/BrokkAi/mjolnir/issues/1519. Live credentials were inspected only for subscriptionType and rateLimitTier; tokens were not printed.

## Interfaces and Dependencies

Add `monthly_price_usd: Option<u32>` to `ProfileQuota`, `ClaudeUsageReport`, and the subscription variant of `CandidateQuota`. Reuse serde, Tokio filesystem I/O, the existing Keychain login source, quota cache, and weighted sampling implementation; introduce no dependencies or configuration.

Plan update (2026-11-12): included the user's CLI follow-up. The initial final suite began before that request; validate the later session-selection changes with focused API tests and incremental workspace Clippy rather than repeating unrelated suites.

Validation evidence: full controller suite 2574 passed / 19 ignored, client 56 library + 1 integration passed / 1 ignored, TUI 581 passed / 2 ignored. Focused follow-up validated 50 checks before the incomplete fixture stalled; after correcting it, its single rerun passed. Final `cargo clippy --all-targets -- -D warnings` passed.
