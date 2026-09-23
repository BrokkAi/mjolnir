# Select and report the session diff baseline


This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


Issue #1109 concerns a session launched at commit B but intentionally moved to older commit A before doing work. `mj diff --session ID --base A --json` must show only that work and report the resolved base and current HEAD commit. Without `--base`, the recorded launch baseline remains authoritative. HEAD means the currently checked-out commit; the patch also includes staged, unstaged, and untracked work.

## Progress


- [x] (2026-09-21 08:43Z) Confirmed the missing CLI/API option and inspected Git, worker, HTTP, and CLI paths.
- [x] (2026-09-21) Implemented validated comparison and metadata in the archive Git helper and worker command.
- [x] (2026-09-21) Connected HTTP query options and CLI flags and documented the interface.
- [x] (2026-09-21) Added and passed isolated Git, HTTP, CLI query, and worker argument tests; Clippy, formatting and diff checks pass.
- [x] (2026-09-21) Full dev-profile `cargo test`, Clippy, formatting, and diff checks passed; reviewed default compatibility, revision validation, and metadata propagation.
- [x] (2026-09-21 09:20Z) Opened and self-reviewed PR #1114; all 18 GitHub checks passed and the PR merged as `1aef031c`, closing #1109.

## Surprises & Discoveries


The worker already accepts `--base`, but the controller chooses it exclusively from the managed worktree record. The HTTP endpoint returns a raw patch and patch export reuses it. Installed older workers need to retain the existing plain-patch command shape; metadata will require the new explicit worker `--json` flag, whose absence is already reported through the established unsupported-worker refusal.

## Decision Log


2026-09-21: Preserve the default HTTP patch response and add `base` and `json` query parameters. Pass a `DiffOptions` value through the existing backend method. Return the existing patch string for ordinary requests and serialized `SessionDiff` for JSON requests. Parse structured output at the HTTP/CLI boundaries so malformed output cannot masquerade as valid metadata. Keep the public Rust `session_diff` helper returning a patch and share its implementation with `session_diff_details`. Validate revisions with Git's `--verify --end-of-options` and require a commit, returning a refusal for an unknown base. No lifecycle baseline or database change is needed.

## Context and Orientation


`mj-checkpoint/src/archive/git.rs` generates the patch by comparing a baseline tree to a captured working tree. `mj-worker/src/main.rs` runs this helper on the session host. The daemon, which is the long-running controller process, invokes that worker through the existing supervised background subprocess path in `mj-controller/src/server_runtime/api.rs`. `mj-controller/src/server/api/files.rs` serves the HTTP endpoint using the backend trait in `subagent_backend.rs`. `mj-cli/src/api_client.rs` makes HTTP requests; `api_commands.rs` parses flags and prints results. Existing tests use disposable Git repositories and HTTP fakes, requiring no live session data.

## Plan of Work


Milestone 1 adds serializable `SessionDiff { diff, base, head }` in the archive Git module and exports it with `session_diff_details`. Resolve the selected/default base to a commit before capture, obtain HEAD, and compare the base tree with the full worktree. The worker gains optional `--json` without changing its ordinary stdout patch. A disposable A/B/detached-C repository test must prove committed, staged, unstaged and untracked inclusion, unrelated-history exclusion with override, unchanged default, and invalid-base refusal.

Milestone 2 adds `DiffOptions` beside API types, forwards the optional override ahead of recorded worktree defaults, and requests worker JSON only when required. Preserve patch export by passing default options. Extend the CLI with `--base` and have `--json` return the worker's resolved fields. Update CLI/API documentation and behavior tests for query forwarding, JSON content type/fields, plain patch compatibility, and refusal mapping.

Milestone 3 reviews the full diff, validates locally, commits on the issue branch, opens a PR with `Fixes #1109`, reviews the published PR, fixes findings, and merges only once every GitHub check passes.

## Concrete Steps


Work in `/Users/ryansvihla/code/mjolnir` on `fix/1109-diff-baseline`. Run focused tests for the archive diff and HTTP diff routes, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. All Cargo tests run outside the sandbox. Any manual application invocation must use `--instance issue-1109`; never use the default instance.

## Validation and Acceptance


The A/B/C test should show the explicit A comparison omits B's unrelated file, while the default B comparison includes its removal. Both report exact commit IDs and preserve the recorded launch base. Unknown and option-looking revisions fail before worktree capture. HTTP tests must show base query forwarding and JSON fields without changing the default `text/x-diff` response or patch export. Full dev-profile Rust tests and Clippy must pass; CI must be green for the exact PR head before merge.

## Idempotence and Recovery


Tests create temporary repositories and existing isolated stores. No migration, dependency change, or mutation of live session configuration is authorized or necessary. Git capture retains its existing behavior; isolating capture objects is separately tracked by #1083/#1082. Older workers explicitly refuse JSON metadata until upgraded, rather than inventing baseline values.

## Artifacts and Notes


The initial source path is `DiffArgs -> ApiClient::diff -> files::diff -> SubagentBackend::diff -> worker diff -> archive::session_diff`. The default baseline must remain unchanged at every layer.

## Interfaces and Dependencies


Use existing Serde, Axum Query/Json, Reqwest query encoding, GitCommandRunner, and supervised worker subprocess helpers. `SessionDiff` contains String fields `diff`, `base`, and `head`. `DiffOptions` contains optional String `base` and bool `json` with false default. No new package or shared runtime dependency is needed.

## Outcomes & Retrospective


Implementation and validation are complete. The Git test verifies exact resolved IDs, explicit versus default comparisons, all four categories of work, unchanged index/recorded baseline, and invalid revision refusal. HTTP/CLI tests verify metadata and revision query encoding. Full Rust tests, Clippy, formatting, and diff checks pass; logs are in `target/issue-1109-tests.log` and `target/issue-1109-clippy.log`. PR #1114 passed all 18 GitHub checks and merged as `1aef031c`, closing #1109. The preceding issue #1075 was merged as PR #1113 after all 18 CI checks passed; this branch starts from that merge.

Revision 2026-09-21: Created the plan before implementing the cross-layer interface change, emphasizing default response compatibility and unchanged lifecycle baselines.

Revision 2026-09-21: Recorded implementation and focused validation; kept full-suite and PR completion pending. Explicit base arguments use `--base=value` so option-looking input reaches Git validation as data.

Revision 2026-09-21: Recorded passing full local validation and pre-PR review. No dependency or database changes were needed.

Revision 2026-09-21 09:20Z: Recorded successful CI and merge during the next issue-workflow checkpoint.
