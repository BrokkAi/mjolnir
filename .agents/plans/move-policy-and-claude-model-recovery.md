# Preserve Move policy and Claude model selection

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users must be able to select native sub-agents in Move and retry a failed sealed Move without reconstructing hidden settings. Claude model selection must keep the selectable value accepted by the harness instead of replacing it with a transcript model identifier during unrelated configuration changes. These fixes follow recovery of session 0bf0983d553751026807d97d54857ede.

## Progress

- [x] (2026-10-06) Recover the live session with native sub-agents and opus, preserving original recovery data.
- [x] Trace Move selection and Claude configuration ownership.
- [x] (2026-10-06) Fix Move policy selection and retained retry settings. Controller suite passed (1875); TUI suite passed except the expected golden action change, which passed after updating and rechecking its fixture. Workspace Clippy passed. Commit this coherent checkpoint.
- [x] (2026-10-06) Fix effort-response model contamination and persist canonical aliases after successful startup restoration. Core tests passed (456 plus 2 integration tests), configuration regressions passed (15), worker binary/integration tests passed, and workspace plus final worker Clippy passed. Commit this coherent checkpoint.
- [x] (2026-10-06) Finish formatting, Clippy, unit, integration, binary, and documentation checks; push both fixes. Commits 652af909 and 45eb2acc reached origin/master through merge def2a40d.

## Surprises & Discoveries

The failed Move stored no sub-agent override; omission means retaining the session policy, not inheriting the destination profile. The user explicitly selected Native on the TUI review; the lack of CLI flags is unrelated. Retry resolves missing fields from the source instead of the sealed destination, and equivalent omitted/explicit policies compare differently. The TUI already forwards an explicitly changed draft and invalidates stale preparations.

Claude reports concrete model IDs in transcripts and picker aliases in its catalogue. The pinned bridge permits its current value even when absent from picker options. `AcceptedSessionConfig::remember` copies the reported model when setting effort, which can replace an accepted alias with the transcript spelling. Live and durable updates both use this helper.

## Decision Log

Keep the sealed Move as the owner of retry settings and reject an explicitly different effective policy. The TUI draft owns its displayed policy and must send it explicitly. Preserve accepted model identity on effort changes; configuration advertisements describe effective state but do not authorize replacing a user choice. Do not introduce a heuristic mapping between Claude model families.

## Outcomes & Retrospective

The TUI now sends its displayed policy explicitly and checks that confirmation matches it. The daemon takes omitted retry defaults from the sealed Move and accepts legacy equivalent policy spellings. Controller tests passed (1875), and the TUI tests passed (521) after the expected golden action update. Live recovery is complete; no further live session mutation is needed. Model implementation is complete. The full worker unit run passed 635 cases and found two failures: the expanded raw-ID fake affected the pin test, fixed by making that behavior opt-in; and a separate checkpoint ownership test failed once under parallel load and passed twice in isolation. All affected configuration tests (15), worker binary and integration targets, core tests, formatting, and Clippy passed. Documentation tests passed for all four touched crates. Both code fixes are pushed to origin/master.

## Context and Orientation

The daemon is the controller owning durable session state. `mj-controller/src/controller/move_session.rs` prepares and seals Move selections. The TUI dispatch in `mj-cli/src/dashboard/actions.rs` forwards draft preparation and confirmed selections to the daemon. The TUI produces selections in `mj-tui/src/wizards/dashboard/resume.rs`. `mj-core/src/acp.rs` owns accepted model/effort values; the worker applies them over ACP, the harness request protocol, in `mj-worker/src/acp/session_config.rs` and `session.rs`.

## Plan of Work

The user selected Native in the TUI review, so CLI changes are outside this task. Send the TUI draft policy explicitly rather than deriving a delta from live session state, and reject a prepared selection that differs from the displayed policy. Resolve omitted retry fields from the sealed record before normal source inheritance. Normalize policy equality by its effective value so legacy records remain retryable. Extend existing isolated Move regressions and TUI stale-preparation coverage.

The original archive records an explicit model selection of claude-fable-5-1. Isolated SDK probes with 0.84.0 and 0.86.0, including the original cached model catalogue, currently accept both the full ID and fable. The 0.84.0 SDK advertises the full ID while 0.86.0 advertises fable, both resolving to the same model. This does not reproduce the original API refusal; do not claim that the effort-response defect caused that particular incident. Preserve the accepted model when effort changes and extend the real ACP subprocess regression with a bridge that reports a raw model ID on effort responses. Verify live and journal-recovered selections remain aliases across restarts.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Use normal mbx Cargo storage and elevated test commands. During each round run affected tests with `cargo test -p brokk-mj-controller <filter>`, `cargo test -p brokk-mjolnir <filter>`, or `cargo test -p brokk-mj-worker <filter>`. Once edits finish run the full suites of touched crates, recheck only failing tests after fixture corrections, complete any targets Cargo skipped after a failure, and `cargo clippy --all-targets -- -D warnings`, plus `cargo fmt --all -- --check` and `git diff --check`. Commit coherent fixes on the current branch and push upstream as requested.

## Validation and Acceptance

An isolated failed Move must prepare again using omitted profile/target/policy, yielding the same sealed selection, and reject a different policy. The Native TUI selection must reach daemon preparation and confirmation with that explicit policy. A stale TUI preparation must not erase a later native choice. Claude's accepted alias must survive an effort response carrying a concrete transcript name, its durable relay update, and worker restart. All test instances retain their isolated configuration/data directories; no development binary may control the default instance.

## Idempotence and Recovery

There are no database migrations or dependency changes. Preserve unrelated changes. Checkpoints and live sessions are not edited as part of implementation. Failed tests are rerun individually after correction; full suites run once after the last implementation round.

## Artifacts and Notes

Live recovery verified session state running, profile claude, worker launch subagents native, model opus, and effort high. Original recovery artifacts remain under the user's Mjolnir recovery directory.

## Interfaces and Dependencies

Reuse `MoveSelection`, `SubagentPolicy`, and `AcceptedSessionConfig`; add no crates or wire fields. Retry defaults remain controlled by the daemon's sealed selection, and the shared accepted-configuration helper remains the sole owner of persistence interpretation.

Revision: created after live recovery, documenting both authorized fixes and push authorization.

Revision: user confirmed explicit Native selection in the TUI. Discarded unrelated CLI work; the TUI now sends its draft policy explicitly and refuses an inconsistent confirmation. Installed SDK probes reproduced the catalogue spelling difference but both IDs were accepted, so the original API refusal remains unexplained.

Revision: Move implementation validated with the full controller and TUI suites, including the expected golden request change. The model fix additionally persists aliases accepted during startup, after session readiness, and checks the recovered journal value.

Revision: model identity regressions and remaining binary/integration targets passed. The original Fable API refusal remains unreproduced; the code fixes the verified ownership and canonicalization defects without inventing model-family mappings. Documentation checks completed and both code fixes were pushed.

Revision: completion recorded. The first push was rejected because upstream advanced. Fetched and merged origin/master normally (def2a40d), confirmed the upstream changes touched none of the fix files, and pushed successfully. No rebase, branch change, or force push was used.
