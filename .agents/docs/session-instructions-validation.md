# Session instruction delivery validation

Worktree: `~/Projects/mjolnir-wt/session-instructions`, detached from `903cac6f6`. The superseded memory re-send worktree is read-only at local reference `a77d745aeff926867ad94e585cca4540ad65170c`; none of its relay slot, revision bump, compaction recognition, refresher or publication-phase machinery is included here.

## Implemented behavior

Each ACP open composes project memory (except Claude), offered MCP guidance, and the durable session role. Standard append is used when advertised; Antigravity's vendor append remains a string inside merged metadata. Kimi, OpenCode and Grok replace only the marked section of their session-owned native instruction file before opening. Existing profile and environment instructions remain outside that section. Native writes use atomic replacement and a worker-owned write lease retained by the I/O thread, so cancellation cannot let an older write overtake a replacement.

An unavailable index emits a visible warning with path and cause and supplies the guidance/path block with an explicit unavailable line. Older non-native-file adapters without either append capability open with a visible harness/version warning; they receive no file or user-message fallback. Automated brief writers are the exception: without a supported composed-instruction channel, delivery fails before any native open, and review continues without a brief through its existing failure path. An advertised append is never silently retried without instructions.

MCP initialize replies no longer contain `instructions`. Tool descriptions remain, including specialized reviewer and brief-writer handback descriptions. Child, reviewer and oracle assignments retain task-specific input and the report directory; oracle and brief-writer packets also retain one read-only task-scope sentence because their execution policy does not enforce read-only access. All other standing authority and the review rubric travel through composition. Reviewer output is plain text, matching the existing consumer. Existing `subagent_kind` and `reviewer_launch.role` launch fields provide roles on new/resumed/moved launches. Task-summary readers accept both historical handback prefixes and the new directory-only note.

## Compatibility

Relay state remains revision 27. No new RuntimeEvent variants or persisted fields were added. `ContextCleared.memory` stays in the existing wire/journal shape and its historical reader remains; new workers produce `None`. Legacy `pending_prompt_context` still delivers once through the generic slot, as specified, with no migration. `AgentInitialized` serializes capabilities without the newly enabled optional system-prompt field, preserving protocol-71 event digests for both journal formats; negotiation still uses the original initialize response.

The file-copy safety check traced `stage_profile`, `copy_profile_entry_within`, `target_profile_home`, `install_local_profile_stage` and `shared_profile_login`: instruction symlinks are dereferenced into per-session copies; only credentials can be linked to the user profile. The worker uses the harness home helper for OpenCode's nested XDG layout.

## Checks

All Cargo commands ran outside the sandbox, on the dev profile, in this worktree. No daemon/default instance was started or changed.

- `cargo test -p brokk-mj-core relay::snapshot`: 9 passed, including the two-format legacy digest regression. Log: `memory-instructions-snapshot-tests.log`.
- `cargo test -p brokk-mj-core a_task_summary_is_one_line_of_the_parents_own_instructions`: 1 passed. Log: `memory-instructions-task-summary-tests.log`.
- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed. The original run used all eight harnesses, new/load/resume/clear and unavailable indexes. Review later found that the older-adapter branch had no phase in the loop; that coverage defect was corrected and verified in the review round below. Native-file preservation covers repeated replacement and non-UTF-8 user bytes. The FIFO regression proves canceled reads do not hold Tokio shutdown open. Five role goldens compare with checked-in text. Final log: `memory-instructions-worker-final-tests.log`.
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

Known live behavior from the approved plan remains: Codex accepts a changed append on reopen but may keep the original thread's instructions. The implementation supplies current text on every open without claiming the provider updates its stored baseline. Reopened used Grok sessions likewise retain their original rule prelude, including pre-upgrade sessions that have no newly composed block; this accepted limitation is not warned about on every resume. Kimi live acceptance remains subject to its weekly quota. All `memory-*.log` files are intentionally untracked.


## First review corrections

Review of `903cac6..621276a` identified six defects plus the stalled-index concern. Oracle and brief-writer launches inherit the parent's execution policy rather than an enforced read-only mode (`controller/reviewer.rs`, `controller/subagents.rs`); their assignment packets now retain one shared `READ_ONLY_TASK_SCOPE` sentence on every adapter. Task summaries strip that safety prefix while retaining the actual question. The rest of the role remains exclusively in the composed section.

The Kimi recovery fixture now supplies an isolated staged home; the Muse MCP-grant fixture advertises standard append support; the daemon brief-writer expectation now checks the task packet and its read-only scope instead of the moved standing heading. The native writer escapes only its delimiter strings inside the payload, preventing an index quotation from closing the managed section. Its existing repeated-replacement test now quotes both markers and preserves unrelated tags and all user bytes outside the managed section. The lifecycle phase list now actually includes the unsupported-adapter case.

`ProjectMemoryStore::startup_index` now uses the existing confined reader. That shared reader opens Unix files nonblocking, checks the opened descriptor is regular, and refuses links, so a FIFO cannot wait for a writer. FIFO indexes open with the explicit unavailable section and warning. No read deadline was added for a hung regular-file filesystem: the parent explicitly declined that demand, and the generic-stall portion was recorded through `override_review` before committing fixes. A FIFO still simulates an uninterruptible syscall inside the shared off-runtime reader in the cancellation/shutdown test; the store itself now rejects special files.

All affected checks ran outside the sandbox on the dev profile, with no still-failing tests:

- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed, now exercising FIFO unavailability, unsupported-adapter warnings, delimiter escaping, role goldens and cancellation-safe runtime shutdown. Log: `memory-instructions-review-worker-tests.log`.
- `cargo test -p brokk-mj-worker-runtime dead_bridge_after_session_start_reloads_the_native_session`: 1 passed, reaching bridge exit and reload. Log: `memory-instructions-review-recovery-test.log`.
- `cargo test -p brokk-mj-worker-runtime muse_sessions_receive_mcp_servers_and_say_so_when_the_host_withholds_them`: 1 passed. Log: `memory-instructions-review-muse-test.log`.
- `cargo test -p brokk-mj-daemon a_brief_writer_hands_back_before_any_reviewer_starts_and_its_brief_reaches_the_reviewer`: 1 passed, reaching handback and reviewer startup. Log: `memory-instructions-review-daemon-test.log`.
- `cargo test -p brokk-mj-core confined_path::tests`: 1 passed. Log: `memory-instructions-review-confined-tests.log`.
- `cargo test -p brokk-mj-core project_memory::tests`: 12 passed. Log: `memory-instructions-review-store-tests.log`.
- `cargo test -p brokk-mj-core a_task_summary_is_one_line_of_the_parents_own_instructions`: 1 passed. Log: `memory-instructions-review-summary-test.log`.
- `cargo test -p brokk-mj-controller reviewer_children_use_the_resolved_profile_without_delegation_policy_or_capacity`: 1 passed. Log: `memory-instructions-review-controller-test.log`.
- `cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime -p brokk-mj-controller -p brokk-mj-review -p brokk-mj-daemon --all-targets -- -D warnings`: passed. Log: `memory-instructions-review-clippy.log`.
- `cargo fmt --all` and `git diff --check`: clean.

No live acceptance, default-instance operation or push was performed. Review corrections are submitted for another formal review after commit; diagnostic logs remain untracked.


## Second review corrections

Review of `903cac6..bc66f0f` found four fixable defects and re-raised Grok freshness. The parent explicitly accepted Grok's original-session prelude retention, just as for Codex; the finding was formally overridden before committing changes. No native replacement, re-send or repetitive resume warning was added. New Grok native sessions read the staged block; already used native sessions can retain older instructions even after file replacement.

Pinned OpenCode 2.2.7 source proves that OPENCODE_CONFIG_DIR replaces the global XDG directory, rather than adding another source (`packages/util/src/global.ts:83`, `packages/core/src/config/plugin/instruction.ts:41`). Session configuration now pins that variable alongside XDG_CONFIG_HOME, and worker preparation reapplies the same home helper from the authoritative retained launch home before merging the target login environment. A user's override directory is not used or copied within a Mjolnir session. The native integration fixture sets a conflicting directory, uses the actual home helper, resolves the pinned loader's effective path, and verifies the other profile's file remains untouched.

Automated brief writers now fail through the same delivery decision when no composed-instruction channel exists. The failure names the harness and adapter version, occurs before session/new/load/resume, and reaches the existing brief_failed path. This is an exception to keeping a user's unsupported-adapter session usable: a brief writer without its brief structure cannot provide a meaningful automated result. The lifecycle fixture proves no native open occurs on all five append-capable harness families when that capability is removed. The daemon integration proves the actionable writer error is displayed and reviewers continue without forwarding an unstructured brief.

Native-file parsing now rejects any duplicated start or end delimiter, including a standalone user start marker before the true block; the file remains unchanged on rejection. Reads and atomic replacements both use the existing confined directory helpers. A shared regular-file opener supports byte-preserving reads as well as text reads, rejects links/FIFOs before reading, and keeps replacement bound to the opened directory while preserving file permissions.

Affected checks, outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed. Log: `memory-instructions-review2-worker-tests.log`.
- `cargo test -p brokk-mj-worker-runtime session_instructions_native_files_replace_only_the_owned_section`: 1 passed after switching the writer to the existing confined atomic helper. Includes effective OpenCode path, untouched other-profile bytes, nested-marker rejection and FIFO staged-file rejection. Log: `memory-instructions-review2-native-final-test.log`.
- `cargo test -p brokk-mj-core confined_path::tests`: 1 passed. Log: `memory-instructions-review2-confined-tests.log`.
- `cargo test -p brokk-mj-daemon a_brief_writer_that_ends_without_a_handback_leaves_the_review_to_run_without_a_brief`: 1 passed. Log: `memory-instructions-review2-brief-failure-test.log`.
- `cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime -p brokk-mj-daemon --all-targets -- -D warnings`: passed. Log: `memory-instructions-review2-clippy.log`.
- Final `cargo clippy -p brokk-mj-worker-runtime --all-targets -- -D warnings` after the confined-writer refactor: passed. Log: `memory-instructions-review2-worker-final-clippy.log`.
- `cargo fmt --all` and `git diff --check`: clean.

No still-failing tests, live acceptance, default-instance operations or pushes. Diagnostic logs remain untracked; the corrected implementation is submitted for formal re-review after commit.


## Startup routing ownership correction

The default seed-1171 delegation scenario passed on base 903cac6 but failed on this implementation before the standing-prompt assertions: send_message chose a turn while the newly addressable child had no managed relay handle, then waited for the held startup model response. A relay attachment about 30 ms later could not change that durable route. The parent approved fixing this race here as a separate commit. Diagnostic chronology and comparison: `session-instructions-delegation-routing-evidence.md`.

Ordinary and automated-review registration now obtain a session-manager Starting acknowledgement before the database commits the relation. The production API requires the manager; unowned registration is test-support-only. The session manager owns the route query and distinguishes a launching child from an unmanaged session. Starting queues a durable mailbox row; published incompatible protocols retain turn delivery. There is no added wait, retry or route re-check.

Affected checks, outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-daemon send_to_registered_child_before_attachment_queues_mailbox_without_a_turn`: 1 passed after the final API constraint; real manager, no target/actor, two sends produce one mailbox row and no turn acceptance. Log: `memory-instructions-routing-final-test.log`.
- `cargo test -p brokk-mj-daemon send_message_`: 9 passed, including protocol-33 turn compatibility and startup queuing. Log: `memory-instructions-routing-compatibility-tests.log`.
- `cargo test -p brokk-mj-controller session_adoption_deadline_also_bounds_an_unanswered_manager_request`: 1 passed. Log: `memory-instructions-routing-manager-test.log`.
- `cargo test -p brokk-mj-controller reviewer_children_use_the_resolved_profile_without_delegation_policy_or_capacity`: 1 passed. Log: `memory-instructions-routing-registration-test.log`.
- `cargo clippy -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-daemon -p brokk-mj-worker-runtime --all-targets -- -D warnings`: passed. Log: `memory-instructions-routing-clippy.log`.

Full default delegation seed 1171 will run after the source commits and a normal dev CLI/worker rebuild. The initial test-hooks binary build exposed conflicting test/production identity stamps; it was replaced with the normal dev configuration rather than bypassing binary validation. No live/default instance was used.


## Third review corrections

Automated reviewers now use the existing unsupported-channel failure exception alongside brief writers. The actionable error names the harness and adapter version and occurs before native open; the daemon's role_failed path leaves the review incomplete and never advances its baseline. Ordinary unsupported-adapter sessions still open with their warning. The existing Decision Log entry records the parent ruling.

Durable reviewer_launch.role is authoritative when loading historical reviewer launches that omit subagent_kind. The role golden derives reviewer and brief-writer text from a sanitized real launch record instead of manually injecting the role. The delegation fixture advertises standard append support, captures the native open's systemPrompt, and checks that standing child/report guidance moved there while the assignment and report directory remain in the first prompt. Its startup model gate and mailbox ordering/idempotency assertions are unchanged.

Affected checks outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-worker-runtime session_instructions_system_prompt_contract_replays_pinned_adapters`: 1 passed, including unsupported reviewer rejection with no native open. Log: `memory-instructions-review3-lifecycle-tests.log`.
- `cargo test -p brokk-mj-daemon a_failed_review_closes_without_advancing_coverage`: 1 passed, including visible adapter cause and no RecordReviewed. Log: `memory-instructions-review3-failure-test.log`.
- `cargo test -p brokk-mj-worker-runtime golden_session_instructions_by_durable_role`: 1 passed using the historical launch fixture. The first run caught an incorrectly sanitized seed_image_environment value; correcting that field to boolean false restored the actual launch shape. Logs: `memory-instructions-review3-legacy-role-test.log` (initial fixture failure), `memory-instructions-review3-legacy-role-fixed-test.log` (passing correction).
- `python3 -m py_compile tests/e2e/delegation.py`: passed outside the sandbox.
- Final edited-crate clippy after the routing correction also covers these worker/daemon fixes: `memory-instructions-routing-clippy.log` (passed).

Default delegation seed 1171 is pending the normal dev rebuild after the source commits. No Milestone 3 live harness acceptance was started.
