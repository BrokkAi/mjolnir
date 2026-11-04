# Expose common tools to every session role

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Every session, including managed children, must receive `send_message`, `fetch`, and `request_review`. Only sessions configured for Mjolnir delegation receive ordinary child-management tools. Explicit reviews must work with every automatic review trigger and for managed children; automatic turn reviews continue to skip managed children.

## Progress

- [x] (2026-11-04) Separate push-gate instructions from delegation and enable explicit reviews for all automatic review triggers.
- [x] (2026-11-04) Locate the remaining managed-child restrictions in the tool catalog, review preparation, reviewer registration, and message delivery.
- [x] (2026-11-04) Extend common tools and managed-child request handling while preserving container ownership.
- [x] (2026-11-04) Extend existing wire, database registration, message delivery, and review-host regressions.
- [x] (2026-11-04) Validate implementation and prepare the current branch for required review and publication.

## Surprises & Discoveries

The original push gate promoted sessions to the full parent role. Separating it exposed older restrictions: managed children were denied explicit reviews during preparation and denied reviewer registration. Container locators also replaced the actual container owner with the direct parent, which is incorrect when that parent is itself borrowing a container. Child message senders were explicitly refused even though messaging is a common capability.

The first complete test run exposed the persisted-state validator's prohibition on nested relationships. Older builds reject nested reviewers on read, so migration 108 raises the compatibility floor before this new relationship can be saved. Worker capture also required a turn baseline even for explicit commit reviews; only turn captures now require that baseline. The first run had one unrelated continuation-timer timeout alongside the registration failure; rerun only those failures after correction.

## Decision Log

Allow reviewer-kind registration below managed children while retaining the prohibition on ordinary nested workers. Use the existing review host and durable reviewer ownership tree rather than routing child requests through another session. Preserve existing borrowed container ownership. Keep the existing restriction on automatic turn reviews for children, because those changes already belong to their parent's turn review.

Migration 108 is breaking because older state validators cannot read the newly permitted nested reviewer relationship. Advance the revision and compatibility floor atomically without rewriting previous migrations. Exercise only isolated databases; this task does not install or migrate the live daemon's store. Reuse checkout ownership traversal to reject relationship cycles.

## Context and Orientation

`mj-core/src/subagent.rs` defines the MCP roles and their tool names. `mj-worker/src/subagent_mcp.rs` supplies MCP schemas, instructions, and dispatch; profile registration and ACP delivery read the role tool names. `mj-controller/src/review_host/reviewer.rs` prepares explicit commit reviews and automatic turn reviews. `mj-controller/src/controller/subagents.rs` registers reviewer sessions and derives their target locator. `mj-controller/src/server_runtime/api/subagent_input.rs` authorizes session messages.

## Plan of Work

Add common tools to Child and FetchOnly roles and match their schemas to those names. Keep handback first for child report-budget handling. Allow explicit commit-review preparation for children, but continue skipping their turn reviews. Permit reviewer registration from managed children, retaining ordinary nested-worker refusal. Preserve the existing container owner when deriving another borrowed locator. Permit managed-child message senders while preserving recipient ownership restrictions.

Extend existing regression cases to exercise all role catalogs, child review calls, managed-child review verdict delivery, persisted nested reviewer registration, original container ownership, and child sender identity. Existing isolated test infrastructure remains authoritative; do not touch host default data.

## Milestones

The first milestone is common MCP registration and successful dispatch for every role. The second is successful child review preparation and durable reviewer registration without changing ordinary delegation authority. The final implementation milestone is passing validation and readiness for the repository's required review and push gate.

## Concrete Steps

Run focused checks with `cargo test -p brokk-mj-worker subagent_mcp::tests` and the changed controller test names. Run final validation outside the sandbox with `cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`. Use existing mbx storage, and run `cargo fmt --all -- --check` plus `git diff --check`. Commit only changed files on the current branch and call request_review before pushing.

## Validation and Acceptance

MCP tool listings for Child and FetchOnly must contain common tools without spawn, list_agents, or close. A managed child must request a commit review and receive the resulting verdict through its own mailbox. Reviewer registration must persist a reviewer beneath a managed child while worker registration there remains refused. A nested container borrower must pass normal locator validation with the original owner. Child messages must carry the real child sender identity.

## Idempotence and Recovery

Retain existing request-key deduplication, review persistence, lifecycle cancellation, and recipient authorization. Migration 108 changes the store compatibility contract without changing its tables; no wire-format change is needed. Tests use isolated stores and existing fake managers. Retry failed tests only, and repeat review if committed source changes.

## Artifacts and Notes

Final validation logs are `/tmp/mj-common-tools-final-tests.log` and `/tmp/mj-common-tools-final-clippy.log`. Focused correction evidence is in `/tmp/mj-common-tools-registration-rerun.log`, `/tmp/mj-common-tools-continuation-rerun.log`, and `/tmp/mj-common-tools-capture-rerun.log`.

## Interfaces and Dependencies

Keep `SubagentMcpRole::tool_names` the authoritative common tool contract. Reviewer registration continues to use `SubagentKind::Reviewer` and the existing review host. Rename the role predicate for push-gate eligibility so it cannot be confused with universal on-demand review availability.

## Outcomes & Retrospective

Every MCP role now exposes common messaging, fetch, and explicit review tools. Ordinary delegation remains restricted to the Mjolnir parent role. Explicit child commit reviews use the existing durable review machinery, preserve original container ownership, and reject ownership cycles. Automatic turn reviews still cover children through their parent. Migration 108 protects older daemons from a stored relationship they cannot read.

The complete core, worker, and controller suites passed, as did workspace Clippy and formatting. The initial registration failure exposed an older persisted-state restriction and was fixed at that owner. The continuation timeout passed both its isolated rerun and final full validation. No live daemon or store was upgraded. Commit, independent review, and publication proceed through the normal repository gate.
