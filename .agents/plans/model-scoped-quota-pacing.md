# Model-scoped quota pacing

## Purpose

Sub-agent selection should match the quota available to the model being started and account for how soon each quota window resets. Claude reports both account-wide and model-scoped weekly limits; keeping them separate prevents an exhausted Fable allowance from making every Claude model look exhausted. The observable result is that a Claude Fable request can prefer a profile with a healthier Fable window, while another model on that profile is ranked using only applicable windows.

## Repository orientation

`mj-client/src/quota.rs` defines the serialized quota window shared across the daemon and clients. `mj-controller/src/claude_usage.rs`, `codex_usage.rs`, and `muse_usage.rs` parse provider readings; `mj-controller/src/quota.rs` converts them into the shared type. `mj-controller/src/quota/recovery.rs` is the existing home for pure quota-window helpers. `profile_health.rs` extends model-specific failure marks from those windows. `server_runtime/api.rs` builds candidates, while `server/api/subagents.rs` ranks and draws them. TUI and web quota output receive the shared data or a viewer projection.

## Design and implementation

Add optional duration and model-scope fields to `QuotaWindow`, retaining serde defaults so persisted older readings remain readable. Keep all-model Claude weekly readings labelled `Week`; retain each scoped weekly reading independently with a capitalized scope label and lowercase scope value. Carry fixed or provider-reported durations from Codex, Claude, and Muse where available. Other providers can leave duration unknown.

Add `window_applies` and `quota_score` beside the recovery helpers. Scope matching is ASCII-case-insensitive substring matching against the model value. For applicable windows, compute remaining quota from the reported percentage or from used/limit, then score it against reset time and duration, capped at four; combine windows by their minimum score. A known report with no windows means usage-priced and scores 1.0; a missing/error report remains unknown.

Store a candidate's optional window list instead of a precomputed percentage so selection can score the requested model. Pass model and one captured current epoch time through draw and rank calls. Weighted draws use `ln(u) / score`, with zero and unknown scores last. Deterministic ranking orders positive scores descending, then zero, then unknown, followed by the existing anchor and profile-id tie breakers. Profile-health reset calculations filter windows by the marked model both when recording and when receiving refreshed quota.

## Milestones

First, extend the shared data model and provider parsers/conversions, keeping scoped Claude windows distinct. Compile-driven updates to every Rust constructor and any viewer mirror must preserve the existing rendering of distinct labels. Next, add scoring and model-aware health/ranking behavior, then update selection fixtures and behavior tests. Finally, run formatting, the full controller/client/TUI tests, workspace clippy, and diff checks; inspect the changed files and record the Claude model-value evidence.

## Validation

From the repository root, run `cargo fmt --check`, `cargo test -p brokk-mj-controller`, tests for the `mj-client` and `mj-tui` packages, `cargo clippy --workspace --all-targets -- -D warnings`, and `git diff --check`. Cargo test commands must use the repository's normal mbx build caching and run outside the restricted sandbox. Tests should verify reset pacing, stale resets, caps, scoped filtering, both Claude payload shapes, selection ordering, and profile-health filtering. Check the web package only if its quota type is mirrored there.

## Progress

- [x] Read `AGENTS.md`, `PLANS.md`, and locate quota parsing, conversion, health, and selection paths.
- [x] Extend the quota contract and provider parsers/conversions.
- [x] Implement model-scoped scoring and health/ranking integration.
- [x] Update and run requested validation; review final diff.

## Surprises & discoveries

- The Claude model catalog contains concrete values such as `claude-fable-5-1` and the `fable` family alias; both contain `fable`. A generic value such as `default` can appear in unresolved usage attribution, so substring scope matching intentionally cannot identify Fable from that value.
- The web viewer has no TypeScript mirror of `QuotaWindow`; its controller projection is `ViewerQuotaWindow`. The parent clarified that model-scoped windows are daemon-only, so both the published client quota snapshot and viewer projection strip them while daemon reports and cache retain them.

## Decision log

- Keep scoped Claude windows in a vector in `ClaudeUsageReport` rather than maintaining a single weekly value. This preserves every `weekly_scoped` limit, including future scope names, while preserving the all-model weekly window independently.
- Treat an error report as unknown and a successful usage-priced report as score 1.0. An empty successful report without the API pricing marker remains score-unknown.
- Review change: `SubagentCandidate.quota` is a `CandidateQuota` enum (`Unknown`, `UsagePriced`, `Windows`) instead of `Option<Vec<QuotaWindow>>` with an empty list meaning usage-priced. A later change that filters windows could otherwise turn a subscription profile into a usage-priced one without anyone noticing.
- Strip scoped windows through one report-copy helper at each user-facing publication boundary. Internal poller reports and stored cache entries retain scoped windows for model-aware ranking and health.

## Outcomes & retrospective

`cargo test -p brokk-mj-controller` completed with 2,131 passing, 17 ignored, and 11 transient failures in checkpoint, worker relay, sub-agent readiness, and daemon responsiveness tests. Every failed test passed when rerun alone; the logs are in `.mj/agents/3b2209f093653b640089be0df87f0991/controller-reruns.txt` and the adjacent per-test logs. `cargo test -p brokk-mj-client -p brokk-mj-tui` passed (42 client tests plus its process probe, and 557 TUI tests; 2 TUI tests ignored). `cargo fmt --check`, `git diff --check`, and clippy for the three touched Rust packages passed. Workspace clippy stopped before linting because this environment lacks GTK system development libraries required by the desktop package.
