# Session sub-agent model override

## Purpose and context

Allow `mj new` and `mj move --subagent-model MODEL [--subagent-effort EFFORT]` to use one model for both straightforward and subtle children in that session. Configuration still owns default task choices and profile eligibility. The daemon owns discovery and quota ranking; the worker advertises the spawn tool. Existing legacy records must keep using configured task choices after upgrade.

## Progress

- [x] Read repository rules and inspect policy, CLI, discovery, spawn, Move and MCP paths.
- [x] Implement durable policy, resolution and validation.
- [x] Implement CLI and Move preservation, documentation and regressions.
- [x] Run requested validation and record evidence without committing.

## Decision Log

Use `Mjolnir { choice: Option<SubagentChoice> }`, flattening the optional choice for serialization. Deserialize current overrides separately from legacy modes so legacy model fields are discarded as before. Reject adaptive effort and effort without model in the current shape. Keep task_type required in the advertised MCP schema while defaulting missing input for cached older schemas. The parent explicitly owns review and commit.

## Surprises & Discoveries

Both Move surfaces currently normalize Mjolnir policies, so they must preserve current overrides while continuing to normalize legacy selectors. TUI availability can allow an override without global choices; server discovery remains authoritative.

## Implementation milestones

First update `mj-core/src/subagent.rs`, mechanical variant constructors and patterns, `mj-controller/src/server/api/subagents.rs` resolution, and `mj-controller/src/controller/profile_config.rs` validation. Plain Mjolnir JSON must stay exactly unchanged; overrides bypass missing global task settings while keeping eligibility and quota ranking. Extend existing serialization and resolution tests and isolated cached-profile validation coverage.

Then restore flags in `mj-cli/src/api_commands.rs` and `mj-cli/src/main.rs`, retain overrides in `mj-tui/src/wizards/subagents.rs` and `mj-controller/src/web/viewer.js`, and default cached MCP calls in `mj-worker/src/subagent_mcp.rs`. Update corresponding existing tests and the short CLI/API/configuration references. Extend the isolated fake-harness delegation script if economical.

Finally from the repository root run `cargo build --workspace --exclude brokk-mj-desktop`, `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings`, `cargo fmt --check`, and `cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui -p brokk-mj-worker`. All cargo tests run elevated and existing test isolation remains intact. Run `node --test tests/e2e/web/*.unit.test.mjs` and the delegation script with compiled mj/worker and a named instance if changed. Store logs in `.mj/agents/cf97e3ecc17bf6f3137c26c3446bd01e/`. Rerun timing failures serially. No Cargo storage/config changes and no commit.

## Outcomes & Retrospective

Implementation is complete. Targeted core contract tests passed (37), and elevated browser tests passed (56). Workspace build and final Clippy passed. Controller passed 1,982 tests (10 ignored); core passed 498 tests and 3 scenario tests. Browser passed 56 tests, and isolated fake-harness delegation passed with global task choices removed. The three TUI failures were mechanical: two golden Debug/action changes and one stale compiled unchanged-Move expectation; all targeted reruns passed. Final full CLI, TUI and worker suites passed: TUI 525 (2 ignored), worker 699 across lib/binary/integration (11 ignored), CLI 248 across unit/integration (4 ignored). The separate core/controller doc-test run passed (one existing core example ignored). All validation is complete; no outstanding failures or design decisions remain. Detailed evidence is in .mj/agents/cf97e3ecc17bf6f3137c26c3446bd01e/validation-summary.md. Changes remain uncommitted for parent review.

## Follow-up: independent task-type overrides (2026-10-12)

The original single-choice policy in this plan was superseded before this implementation. The accepted policy stores optional `straightforward` and `subtle` choices independently. Each task resolves its session override first and its matching global `[subagents.<type>]` choice otherwise. New records use nested per-type objects; the briefly shipped flattened `model`/`effort` shape is decoded as the same choice for both types, while legacy `all_models`, `single_model`, and booleans remain plain Mjolnir policies. The worker MCP tool contract remains unchanged. CLI flags are `--subagent-straightforward`, `--subagent-straightforward-effort`, `--subagent-subtle`, and `--subagent-subtle-effort`; retired shared flags are hidden and return migration guidance. See the implementation and regression tests in the core policy serde tests, controller selection/validation tests, CLI parser tests, TUI Move policy tests, browser Move unit test, and `tests/e2e/delegation.py` for the current specification and evidence.
