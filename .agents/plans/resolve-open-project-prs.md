# Resolve the overlapping project-selection pull requests

This ExecPlan follows `.agents/PLANS.md` and must remain current during implementation.

## Purpose / Big Picture

Resolve the three open pull requests without losing useful work. Users should get the browsable GitHub/folder project picker from #1147, optional browser multi-repository selection from #1145, and the independently reviewed terminal appearance from #1148. The older competing project-selection implementation can then be closed as superseded.

## Progress

- [x] (2026-09-25 14:16Z) Inspected all three PRs, their changes, CI and conflicts. Claimed #1145 and #1147.
- [x] Integrated #1147 into the current master working tree; retained master's dynamic wizard step numbering and profile terminology in the render conflict.
- [x] (2026-09-25 14:23Z) Repaired the browser disclosure race and preserved optional browser repository groups. All 51 Node tests and 110 deterministic browser tests pass (three existing lab-dependent skips).
- [x] (2026-09-25 14:28Z) Validated the project integration: all workspace Rust suites passed with a corrected PTY rerun (10 tests), documentation tests passed, Clippy and formatting passed, Python fixtures parsed.
- [x] (2026-09-25 14:28Z) Committed the project-picker integration on master as `85b8f6bd`.
- [x] (2026-09-25 14:32Z) Prepared #1148 without conflicts. The complete combined workspace test run passed, including 802 TUI tests, 11 PTY tests, upgrade regressions and doc tests; all-target Clippy and formatting also passed.
- [x] (2026-09-25 14:34Z) Published `85b8f6bd`; GitHub confirms #1147 merged. Closed superseded #1145 after preserving its browser groups and removed both coordination labels.
- [x] (2026-09-25 14:35Z) All 15 PR #1148 checks passed for unchanged head `7d087f36`, including the 40-minute macOS job.
- [ ] Publish the terminal merge after #1148's final macOS job completes, remove its coordination label, and verify no open PR remains.

## Surprises & Discoveries

The two project PRs independently replace the same bundle-creation wizard. Neither is already integrated into master. Both conflict only in wizard titles against current master. #1147 includes the more complete discovery service and multi-repository HTTP endpoint but only single-repository browser creation. #1145 adds the missing browser grouping, but its older terminal and browser picker should not replace #1147.

#1147 CI reports browser folder-path controls disappearing during edits and a transient missing tab during layout measurement. Its Windows failures predate master's Windows fixes. #1148's 14 completed jobs passed, with macOS still running at initial inspection.

## Decision Log

Use #1147 as the project-selection foundation and adapt #1145's optional browser repository list to it. Preserve current master behavior when resolving conflicts. Keep all local commits on the current master branch, as required by AGENTS.md. The user's request to merge PRs authorizes publishing these merge results. Do not create or switch branches.

Keep single-repository selection immediate; only explicitly enabling multi-repository selection should collect sources before opening the project. The first source is primary and removal updates that order. Use the existing grouped `/api/bundles` endpoint, which already validates and persists the exact selection atomically. Publish the validated project-selection milestone independently while the terminal PR's final CI completes, so the resolved duplicate does not remain open unnecessarily.

## Outcomes & Retrospective

Both applicable PRs are integrated and the combined result passes the complete workspace test run, including all 802 TUI tests and 11 PTY tests. All-target Clippy, formatting, diff checks, Python fixture syntax, 51 Node tests and 110 deterministic browser tests pass. The browser suite retains its three pre-existing lab-dependent skips. The Windows failure from #1147 was `hidden_context_block` being unused on Windows; master already contains its fix in `905610dd` and subsequent test platform guards in `32c3435b`. #1147 is merged on GitHub and #1145 is closed with its unique browser capability preserved. Only the terminal PR's final macOS CI job and remote publication remain. Success means no open PR remains from the initial set.

## Context and Orientation

`mj-controller/src/web/viewer.js` owns the browser new-session draft, project discovery, and creation calls. `viewer.css` styles those controls. `tests/e2e/web/new-session.spec.js` uses an isolated HTTP fixture to test real browser interactions. `mj-controller/src/server/handlers.rs` accepts single or multiple sources. `mj-tui/src/wizards/render.rs` renders terminal wizard titles, with a dynamic count that omits a sole target. The terminal styling PR is independent except for shared rendering tests and screenshots.

## Plan of Work

Resolve #1147's title conflict by preserving dynamic counts and using project-oriented wording. Ensure the browser records an opened folder disclosure synchronously before a render can replace it. Adapt #1145's ordered source list, duplicate guard, removal, and group submission to the newer project browser. Preserve drafts across errors, source tabs, Back, snapshots, and pending creation. Extend existing behavior tests to cover successful grouped launch, retry, primary removal, duplicate selection, and cancellation. Run the complete browser suite and workspace Rust tests and Clippy before committing. Inspect #1148's remaining CI, merge it when green, and run necessary integration checks before publishing to origin/master.

## Concrete Steps

Work in `/home/ryan/code/mjolnir`. The merge of `origin/codex/browsable-project-picker` is in progress. Stage only the resolved and adapted files; the merge has staged the PR's other files. Validate with `cargo fmt --all -- --check`, `NO_COLOR= cargo test -- --quiet`, `cargo clippy --all-targets -- -D warnings`, and `npm test --prefix tests/e2e/web`. Run Cargo tests outside the restricted sandbox. Existing tests use isolated directories and named instances; do not invoke a new build against the default daemon. Commit the validated merge, then integrate the terminal refresh after confirming its exact head passes all CI checks. Publish using a normal fast-forward push to upstream; fetch and integrate concurrent changes if needed.

## Validation and Acceptance

The browser should select one repository immediately by default. Enabling multiple repositories should allow selection from GitHub, folders or a pasted source, show which repository is primary, retain the list through failures, and launch the exact ordered group. Editing an expanded folder path must preserve its visibility and focus through discovery cancellation and snapshot refresh. Existing full browser and Rust suites must pass. GitHub must report #1147 and #1148 merged and #1145 closed after unique functionality is preserved.

## Idempotence and Recovery

Inspect `git status` before each mutation because other work may run concurrently. Do not force-push, delete branches, reset unrelated changes, or switch branches. If validation fails, fix the cause and rerun the affected checks before publication. A failed normal push is safe to retry only after fetching and integrating the new upstream tip. Query remote PR states after mutations to verify results.

## Artifacts and Notes

Initial master: `7cff9e91`. #1147 head: `a43bf76f`. #1145 head: `c7ac0494`. #1148 head: `7d087f36`. PR #1147 CI run `36119408734` shows the browser disclosure regression. #1148 CI run `36143946827` tracks final platform validation.

## Interfaces and Dependencies

Reuse the existing project discovery endpoint and cancellable requests. Group creation uses `POST /api/bundles` with `{ sources: [...] }`; no schema change, dependency, worker protocol change, or crate is needed. All long-running discovery remains in supervised tasks.

Created on 2026-09-25 to record the integration and validation required by the requested PR cleanup. Updated after the full browser suite passed and master-compatible terminal wording was retained.
