# Simplify automatic reviewer selection

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Auto code review should be predictable: try GPT Terra, DeepSeek Flash, Claude Haiku, and Grok on subscriptions, then the same list on API billing, then the cheapest other advertised model. Select the newest advertised version of each preferred family and always require High effort. Within each billing tier prefer a different model family from the primary session. Explicit reviewer profiles retain their existing behavior.

## Progress

- [x] (2026-11-02) Read reviewer selection, profile discovery, quota classification, usage pricing, and baseline eligibility code.
- [x] (2026-11-03) Implement shared model identity/pricing, family priority, fixed High, billing order, primary-family avoidance, scoped quota checks, and arbitrary-profile baseline eligibility.
- [x] (2026-11-03) Update ACP/catalog ordering regressions and user documentation. Five focused reviewer regressions passed.
- [x] (2026-11-03) Full core/controller suites passed: controller 2,231 passed with 19 ignored; core 553 passed with 4 ignored; three Jev scenarios passed. Formatting and diff checks passed.
- [x] (2026-11-03) Workspace `cargo clippy --all-targets -- -D warnings` passed. Final reviewer-selection rerun passed all five tests after the unknown-price ordering correction. Source review is complete.

- [x] (2026-11-03) Committed the implementation as `f5026b9dc`, merged upstream without conflicts, and passed reviewer/quota checks and workspace Clippy on the combined tree.

## Surprises & Discoveries

The previous implementation chose providers before models and excluded every provider outside its fixed list unless a model was explicitly named. The new cheapest-model fallback requires every enabled profile to be eligible for a review baseline. Pricing and Claude alias resolution existed under `mj-controller/src/database/usage_report/`; both now live in `mj-core/src/usage/` and serve usage reports and reviewer selection. The previous docs incorrectly described API billing as healthy quota; the updated docs describe the implemented billing tiers.

## Decision Log

- Decision: Billing tier precedes avoiding the primary model family; avoid the same family within a tier.
  Rationale: A subscription candidate must still win over a metered API candidate. Terra versus Flash should alternate even when their advertised version IDs differ.
  Date/Author: 2026-11-02 / Codex.
- Decision: Auto effort is always the exact advertised value `high`, including session-named Auto models; explicit profile settings keep their overrides.
  Rationale: The user explicitly requested High for every Auto reviewer.
  Date/Author: 2026-11-02 / Codex.
- Decision: Compare fallback model prices by ordinary input plus output rates for an equal-token sample. Unknown prices follow known ones and are disclosed when chosen.
  Rationale: Future review token counts are unknown. Existing usage rates provide a reproducible estimate without maintaining duplicate prices.
  Date/Author: 2026-11-02 / Codex.

## Outcomes & Retrospective

The simpler selector and documentation are implemented. The full affected-crate suites, five reviewer regressions, workspace Clippy, formatting, and diff checks pass. Auto uses the requested priority list and fixed High effort. Shared pricing still drives usage reports; review baseline eligibility includes fallback-only profiles. No live configuration or process was changed. Delivery follows the repository policy: commit on the current branch and push to origin/master, merging concurrent upstream work when required.

## Context and Orientation

`mj-controller/src/review_selection.rs` resolves reviewer identities before the hidden child launches. `mj-core/src/review/settings.rs` owns shared policy and determines whether new sessions capture the baseline tree needed for later review. `mj-controller/src/server_runtime/profile_catalog.rs` is the daemon-owned cache of advertised model IDs and their efforts. `mj-controller/src/utility_llm.rs` supplies shared quota readings. Shared usage pricing and Claude alias resolution live in `mj-core/src/usage/pricing.rs` and `mj-core/src/usage/model_catalog.rs`; usage reports and reviewer selection both consume them.

## Plan of Work

First expose shared price and model identity helpers without changing usage-accounting behavior. Replace provider/group/quota-health ranking with model candidates: preferred subscription, preferred API, then other models by price. Read the primary model from its managed-session projection, resolve aliases through its catalog, and demote the matching family within a tier. Reject exhausted quota and unsupported High effort, keep discovery cancellable and concurrent, and preserve errors for the no-candidate message. Broaden baseline eligibility to enabled profiles because arbitrary advertised models can now serve as the fallback. Remove obsolete same-provider wording from review descriptions while keeping persisted review data readable.

Then update the existing ACP fixture and profile-catalog tests to cover newest-version ordering, subscription priority, family avoidance, price fallback, exact effort support, and unknown versus exhausted quota. Update `docs/src/content/docs/turn-review.md`, the website configuration reference and the repository skill configuration reference, and session-option descriptions that currently promise an Auto effort override.

## Concrete Steps

Run all commands from the repository root. Keep the existing mbx-owned Cargo target symlink and normal build configuration. Use elevated execution for every `cargo test`, as required by AGENTS.md. During implementation run focused `cargo test -p brokk-mj-controller review_selection` and any changed core tests. At the final validated tree run `cargo test -p brokk-mj-core -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`, using the dev profile. Run `cargo fmt --all -- --check` and `git diff --check`. Stage only this task's files, commit on the current branch, and push `HEAD:master`. If upstream advances, fetch and merge origin/master, then push without rebasing.

## Validation and Acceptance

Profile-catalog fixtures must show that a Terra primary chooses subscription Flash instead of Terra; a Flash primary chooses subscription Terra; a subscription preferred family beats an API preferred family; newest advertised family versions win; unsupported High efforts move to the next candidate; and only after the preferred families are unavailable do ordinary price estimates decide. Unknown prices must not count as zero. Existing review lifecycle and usage-accounting tests must continue to pass. Unit-test isolation uses the suite's existing temporary data directories; any binary/daemon invocation must specify `--instance simple-auto-reviewer`.

## Idempotence and Recovery

No database schema, live configuration, running worker, or daemon upgrade is needed. All changes are repository source files. Tests keep their existing isolation. A failed preparation can be cancelled and repeated, and candidate failures remain visible in the final error.

## Artifacts and Notes

Validation used the existing mbx-owned target directory with dev-profile checks. The full command `cargo test -p brokk-mj-core -p brokk-mj-controller` exited successfully. The focused command `cargo test -p brokk-mj-controller review_selection` passed all five tests, and `cargo clippy --all-targets -- -D warnings` passed for the workspace. No build layout was changed.

## Interfaces and Dependencies

Reuse `ProfileConfig`, `ProfileCatalog`, `ProfileQuota`, and `ResolvedReviewSettings`. Keep the single capability-cache owner; selection consumes its advertised choices instead of staging a separate profile. Shared usage pricing must remain the sole rate table. Do not introduce dependencies or crates.

Initial plan created after confirming the user's priority list and subsequent High-effort requirement.

2026-11-03: Implementation now reuses model-scoped quota matching and retains the known-profile-first discovery rule for a session-pinned model, so a broken unknown profile does not delay a confirmed match. Package names use the `brokk-` prefix; validation commands above reflect Cargo metadata.

2026-11-03: Final review clarified that known fallback prices precede unknown prices even when this repeats the primary model. Implementation commit `f5026b9dc` was merged cleanly with four concurrent upstream quota-runtime commits in `0617b2c46`. On the merged source, all five reviewer-selection tests and 72 quota tests passed (one live-service quota test ignored), followed by successful workspace Clippy and formatting checks.
