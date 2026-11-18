# Session instruction delivery validation

Worktree: `~/Projects/mjolnir-wt/session-instructions`, detached from `903cac6f6`. The superseded memory re-send worktree is read-only at local reference `a77d745aeff926867ad94e585cca4540ad65170c`; none of its relay slot, revision bump, compaction recognition, refresher or publication-phase machinery is included here.

## Implemented behavior

Each ACP open composes project memory (except Claude), offered MCP guidance, and the durable session role. Standard append is used when advertised; Antigravity's vendor append remains a string inside merged metadata. Kimi, OpenCode and Grok replace only the marked section of their session-owned native instruction file before opening. Existing profile and environment instructions remain outside that section. Native writes use atomic replacement and a worker-owned write lease retained by the I/O thread, so cancellation cannot let an older write overtake a replacement.

An unavailable index emits a visible warning with path and cause and supplies the guidance/path block with an explicit unavailable line. Older non-native-file adapters without either append capability open with a visible harness/version warning; they receive no file or user-message fallback. An advertised append is never silently retried without instructions.

MCP initialize replies no longer contain `instructions`. Tool descriptions remain, including specialized reviewer and brief-writer handback descriptions. Child, reviewer and oracle assignments retain task-specific input and the report directory; standing authority and review rubric travel through composition. Reviewer output is plain text, matching the existing consumer. Existing `subagent_kind` and `reviewer_launch.role` launch fields provide roles on new/resumed/moved launches. Task-summary readers accept both historical handback prefixes and the new directory-only note.

## Compatibility

Relay state remains revision 27. No new RuntimeEvent variants or persisted fields were added. `ContextCleared.memory` stays in the existing wire/journal shape and its historical reader remains; new workers produce `None`. Legacy `pending_prompt_context` still delivers once through the generic slot, as specified, with no migration. `AgentInitialized` serializes capabilities without the newly enabled optional system-prompt field, preserving protocol-71 event digests for both journal formats; negotiation still uses the original initialize response.

The file-copy safety check traced `stage_profile`, `copy_profile_entry_within`, `target_profile_home`, `install_local_profile_stage` and `shared_profile_login`: instruction symlinks are dereferenced into per-session copies; only credentials can be linked to the user profile. The worker uses the harness home helper for OpenCode's nested XDG layout.

## Checks

All Cargo commands ran outside the sandbox, on the dev profile, in this worktree. No daemon/default instance was started or changed.

- `cargo test -p brokk-mj-core relay::snapshot`: 9 passed, including the two-format legacy digest regression. Log: `memory-instructions-snapshot-tests.log`.
- `cargo test -p brokk-mj-core a_task_summary_is_one_line_of_the_parents_own_instructions`: 1 passed. Log: `memory-instructions-task-summary-tests.log`.
- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed. Recorded lifecycle coverage uses all eight harnesses, new/load/resume/clear, unavailable indexes, and older-adapter warnings without fallback. Native-file preservation covers repeated replacement and non-UTF-8 user bytes. The FIFO regression proves canceled reads do not hold Tokio shutdown open. Five role goldens compare with checked-in text. Final log: `memory-instructions-worker-final-tests.log`.
- `cargo test -p brokk-mj-worker-runtime subagent_mcp::tests`: 39 passed. Log: `memory-instructions-mcp-tests.log`.
- `cargo test -p brokk-mj-worker-runtime memory_mcp::tests`: 1 passed, through the real socket transport. Log: `memory-instructions-history-tests.log`.
- `MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-review lanes::tests`: 3 passed, one obsolete assertion failed because it expected the moved rubric in the packet. Updated that assertion to require its absence; reran only `review_prompt_contains_tree_ids_and_diff_command_without_standing_rubric`: 1 passed. Logs: `memory-instructions-review-tests.log`, `memory-instructions-review-fix.log`.
- `cargo test -p brokk-mj-review golden_review_prompt_without_and_with_a_brief`: 1 passed against the updated checked-in packet goldens. Log: `memory-instructions-review-golden-tests.log`.
- `cargo test -p brokk-mj-controller reviewer_children_use_the_resolved_profile_without_delegation_policy_or_capacity`: 1 passed in its existing isolated child process. Log: `memory-instructions-controller-tests.log`.
- `cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime -p brokk-mj-controller -p brokk-mj-review --all-targets -- -D warnings`: passed. Log: `memory-instructions-clippy.log`.
- Final `cargo clippy -p brokk-mj-worker-runtime --all-targets -- -D warnings` after the write-lease/history-test changes: passed. Log: `memory-instructions-worker-final-clippy.log`.
- `cargo fmt --all` and `git diff --check`: clean.

Initial compilation exposed the missing ACP schema import and new role-field values in existing test/discovery fixtures; both were fixed before the successful test runs. Earlier compiler output is retained in the untracked `memory-instructions-check*.log` and `memory-instructions-worker-tests*.log` files.

## Deferred acceptance

Milestone 3 has not been started. Claude's fixture is explicitly source-derived from pinned bridge 0.108.0; the other harness lifecycle fixtures are recorded. Automatic approval review rejected the proposed live Claude probe under the original no-Milestone-3 scope, and the parent directed source-derived coverage instead. The prepared, unexecuted script is `/tmp/claude-session-instructions-probe.py` for parent-owned live acceptance. No credentials appear in fixtures or repository files.

Known live behavior from the approved plan remains: Codex accepts a changed append on reopen but may keep the original thread's instructions. The implementation supplies current text on every open without claiming the provider updates its stored baseline. Kimi live acceptance remains subject to its weekly quota. All `memory-*.log` files are intentionally untracked.
