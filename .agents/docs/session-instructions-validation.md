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

Full default delegation seed 1171 passed after the source commits and a normal dev CLI/worker rebuild; see Final acceptance below. The initial test-hooks binary build exposed conflicting test/production identity stamps; it was replaced with the normal dev configuration rather than bypassing binary validation. No live/default instance was used.


## Third review corrections

Automated reviewers now use the existing unsupported-channel failure exception alongside brief writers. The actionable error names the harness and adapter version and occurs before native open; the daemon's role_failed path leaves the review incomplete and never advances its baseline. Ordinary unsupported-adapter sessions still open with their warning. The existing Decision Log entry records the parent ruling.

Durable reviewer_launch.role is authoritative when loading historical reviewer launches that omit subagent_kind. The role golden derives reviewer and brief-writer text from a sanitized real launch record instead of manually injecting the role. The delegation fixture advertises standard append support, captures the native open's systemPrompt, and checks that standing child/report guidance moved there while the assignment and report directory remain in the first prompt. Its startup model gate and mailbox ordering/idempotency assertions are unchanged.

Affected checks outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-worker-runtime session_instructions_system_prompt_contract_replays_pinned_adapters`: 1 passed, including unsupported reviewer rejection with no native open. Log: `memory-instructions-review3-lifecycle-tests.log`.
- `cargo test -p brokk-mj-daemon a_failed_review_closes_without_advancing_coverage`: 1 passed, including visible adapter cause and no RecordReviewed. Log: `memory-instructions-review3-failure-test.log`.
- `cargo test -p brokk-mj-worker-runtime golden_session_instructions_by_durable_role`: 1 passed using the historical launch fixture. The first run caught an incorrectly sanitized seed_image_environment value; correcting that field to boolean false restored the actual launch shape. Logs: `memory-instructions-review3-legacy-role-test.log` (initial fixture failure), `memory-instructions-review3-legacy-role-fixed-test.log` (passing correction).
- `python3 -m py_compile tests/e2e/delegation.py`: passed outside the sandbox.
- Final edited-crate clippy after the routing correction also covers these worker/daemon fixes: `memory-instructions-routing-clippy.log` (passed).

Default delegation seed 1171 passed after the normal dev rebuild of the source commits; see Final acceptance below. No Milestone 3 live harness acceptance was started.


## Final acceptance after third review and routing correction

- Source checkpoints: `5cdda85fa` (starting registration ownership) and `a0b529777` (third-review corrections).
- `cargo build -p brokk-mjolnir -p brokk-mj-worker --bins`: passed on the normal dev profile, with no test-hooks or identity-validation bypass. Source and HEAD remained unchanged during the build. Log: `memory-instructions-routing-e2e-build.log`.
- `python3 tests/e2e/delegation.py --mj target/debug/mj --worker target/debug/mj-worker --instance instructions-routing-55d6204e`: full default scenario, seed 1171, PASSED. The startup gate and mailbox ordering/deduplication checks remain unchanged. The held-model send_message returned in 0.180 s (pre-fix failure: 60.373 s). Standing role/report text was checked in the native open's append; task and directory remained in the first prompt. Later handoff, recovery, user-message and usage-retention checks also passed. Log: `memory-instructions-routing-delegation-e2e.log`.
- Artifacts: `target/reliability-artifacts/delegation-seed-1171-903074`. Parent `058bb1d323ed793180dc632e49214dd3`, child `9e3131b7dc58b9495c16e574d4384ee8`; runtime `/mnt/optane/tmp/hel-r-903074-27262rd5`.
- The fixture cleaned up its processes; an explicit daemon stop using that runtime's private MJ_CONFIG_DIR/MJ_DATA_DIR and the same named instance confirmed it was already stopped. Log: `memory-instructions-routing-cleanup.log`.

All affected checks now pass. Initial failures and their fixes remain documented above; diagnostic memory-*.log files are intentionally untracked. Relay revision 27, snapshot fields and RuntimeEvent serialization remain unchanged; the new registration/route query is in-process only. The AgentInitialized serializer continues preserving existing protocol-71 event digests. Formal re-review follows the documentation checkpoint. No default-instance operation, push or Milestone 3 live harness acceptance was performed.


## Fourth review compatibility clarification

A partial fourth-review finding proposed turn routing for a Starting child that later publishes a protocol below RELAY_STRUCTURED_MAILBOX_PROTOCOL. That launch is excluded by existing production ownership: Starting entries are installed only for fresh child registrations, and every fresh install uses this daemon's worker_build_id. `WorkerInstall::for_target` in `mj-worker-staging/src/worker_binary/image_worker.rs` checks the image build and selects a host replacement when needed; host selection's `worker_binary_prerequisite_with_verifier` in `binary_source.rs` rejects an explicit stale MJ_WORKER_BINARY instead of using it. Older running/retained sessions do not acquire fresh registration entries, and the passing protocol-33 message test preserves their turn route.

The parent confirmed this invariant; the final review arrived and the finding was formally overridden before any new commit. A short comment at the Starting route names both enforcing launch functions; no runtime assertion, wait or compatibility probe is added. This comment/documentation change requires diff review only; the previously passing affected tests and full seed-1171 acceptance remain applicable. The comment is included with the fourth-review corrections below.


## Fourth review failure-path corrections

The same durable lifecycle transition now owns the failed-launch outcome. `insert_session_with_parent` and `update_lifecycle_fields` settle pending ParentMessages in the session-record transaction when the record becomes StartupCleanup or Error, preserving the stored startup error as the failure reason. Public enqueue holds a writer transaction and uses the same failure classifier, so a producer delayed after selecting the mailbox route cannot insert pending work after that transition. Existing outbox Failed/accepted_at fields represent this outcome; no migration or wire change is needed.

The session manager subscribes to the existing committed-state feed and releases registrations for terminal/missing records, including children that never published a pollable target. Before commit, the manager's registration reply owns a guard; the creator then holds it across the write. Dropping either side releases that guarded entry. An explicit publish acknowledgement transfers ownership to the feed and reads its latest snapshot, handling coalesced failure/removal before handover. Drop and route use the same FIFO channel. Per-failure forget calls were removed.

Initial mechanical checks found private guard-construction fields (fixed with module-local visibility; `memory-instructions-review4-failure-test.log`). The next fixture incorrectly chose a new route after failure while retaining an old Running export, and therefore waited in the existing turn path. Its test-owned child was stopped after the backtrace identified the wait; the fixture now models a producer already paused after its route decision by calling the actual enqueue boundary. Logs: `memory-instructions-review4-failure-fixed-test.log` and `memory-instructions-review4-hang-backtrace.log`. The corrected focused regressions pass:

- `cargo test -p brokk-mj-daemon failed_startup_settles_queued_parent_messages_without_attachment`: 1 passed; atomic failure, late enqueue and parent-facing delivery annotations. Log: `memory-instructions-review4-failure-final-test.log`.
- `cargo test -p brokk-mj-daemon launch_reservation_releases_on_precommit_abort_and_terminal_publication`: 1 passed; pre-commit guard drop, ordinary terminal feed update, and failure/removal coalesced before handover, without a target or actor. Log: `memory-instructions-review4-reservation-test.log`.
- `cargo test -p brokk-mj-daemon send_to_registered_child_before_attachment_queues_mailbox_without_a_turn`: 1 passed with the guard handover. Log: `memory-instructions-review4-routing-test.log`.

Additional affected checks passed on the dev profile outside the sandbox:

- `cargo test -p brokk-mj-daemon a_failure_after_a_sub_agent_registers_moves_it_to_error`: 1 passed (`memory-instructions-review4-registration-failure-test.log`).
- `cargo test -p brokk-mj-daemon outbox_`: 3 passed, including old-protocol failure reporting, stopped-child pruning and mailbox-aware parking (`memory-instructions-review4-outbox-tests.log`).
- `cargo test -p brokk-mj-daemon send_message_`: 9 passed (`memory-instructions-review4-message-tests.log`).
- `cargo test -p brokk-mj-controller session_adoption_deadline_also_bounds_an_unanswered_manager_request`: 1 passed (`memory-instructions-review4-manager-test.log`).
- `cargo test -p brokk-mj-store database::mailbox::tests`: 4 passed (`memory-instructions-review4-store-tests.log`).
- `cargo test -p brokk-mj-store single_session_read_includes_related_rows_and_ignores_unrelated_invalid_records`: 1 passed (`memory-instructions-review4-state-read-test.log`). The initial state_io::tests filter matched zero tests and was corrected (`memory-instructions-review4-state-tests.log`).
- `cargo clippy -p brokk-mj-controller -p brokk-mj-store -p brokk-mj-daemon --all-targets -- -D warnings`: passed (`memory-instructions-review4-clippy.log`).
- Formatting and `git diff --check`: clean.

A normal dev rebuild and full delegation seed 1171 passed after source checkpoint bf7150bcf; see acceptance below. No Milestone 3 live acceptance or default-instance operation was performed.


## Acceptance after fourth-review failure-path corrections

- Source checkpoint: `bf7150bcf` (guard handover, committed-feed cleanup, atomic parent-message failure).
- `cargo build -p brokk-mjolnir -p brokk-mj-worker --bins`: passed, normal dev profile; source and HEAD stayed unchanged during the build. Log: `memory-instructions-review4-e2e-build.log`.
- `python3 tests/e2e/delegation.py --mj target/debug/mj --worker target/debug/mj-worker --instance instructions-review4-55d6204e`: full default seed-1171 scenario PASSED. The original model gate, mailbox ordering/idempotency, standing append, first prompt, handoff/recovery and cleanup checks all passed. Held-model send_message: 0.200 s; daemon handoff: 0.453 s. Log: `memory-instructions-review4-delegation-e2e.log`.
- Artifacts: `target/reliability-artifacts/delegation-seed-1171-3645703`; parent `b6fc4c7691a22fd04b0622ce83a67fa7`, child `18c88dc533795cdd3fd558363afea486`; runtime `/mnt/optane/tmp/hel-r-3645703-7o29z3iz`.
- An explicit stop with that runtime's private config/data and the same named instance confirmed the daemon was already stopped (`memory-instructions-review4-cleanup.log`).

No still-failing checks, default-instance operations, pushes or Milestone 3 live acceptance. Diagnostic memory-*.log files remain untracked. The schema, relay revision/snapshot and RuntimeEvent shapes are unchanged; Failed uses the existing compatible outbox representation. The corrected checkpoint is submitted for formal re-review.


## Fifth review: recoverable Error and original enqueue failure

The parent corrected the terminal-state rule: Error can recover through explicit Resume/Restart, so its pending ParentMessages must survive. No durable initial-launch marker or Error auto-resume is added. Only StartupCleanup/Destroying settle queued messages as Failed; a removed target retains the existing TargetGone/error path. Reservation release still follows exits from Starting as before.

Before implementation edits, `/tmp/session-instructions-error-resume-probe.py` reused the existing delegation fixture with the current real dev CLI/worker. With the isolated daemon stopped it seeded a legacy Error record and pending parent-message row, restarted the daemon to load that state, then explicitly resumed the retained environment. The outbox accepted the message and the native harness prompt contained its text exactly once. Result: PASS in `instructions-error-resume2-55d6204e`; log `memory-instructions-review5-error-resume-probe-fixed.log`; artifacts `target/reliability-artifacts/delegation-seed-1171-516249`; runtime `/mnt/optane/tmp/hel-r-516249-57uqlgjs`. An initial probe only missed the existing [phone].enabled prerequisite for Resume (`memory-instructions-review5-error-resume-probe.log`); enabling it in that private test configuration allowed the actual recovery check. Fixture cleanup stopped its processes.

Enqueue now carries an internal Failed(reason) outcome out of the writer transaction, preserving its durable failure receipt, and the public operation returns that failure to the sender after commit. Existing failure receipts are authoritative on retries; /clear's distinct discard receipt retains its previous behavior. The no-attachment regression pauses a sender after the real manager chooses its mailbox route, commits terminal startup cleanup, then verifies the original call returns the stored error. A control child's pending row survives Error and its return to Running. No implementation behavior was changed before the real recovery probe passed. After the interrupted build, process inspection confirmed that no prior test/build remained; validation restarted and completed outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-daemon failed_startup_settles_queued_parent_messages_without_attachment`: 1 passed; original-call failure after the routing/enqueue race, committed failure receipts and recoverable-Error control (`memory-instructions-review5-terminal-final-test.log`).
- `cargo test -p brokk-mj-daemon outbox_`: 3 passed (`memory-instructions-review5-outbox-tests.log`).
- `cargo test -p brokk-mj-daemon send_message_`: 9 passed (`memory-instructions-review5-message-tests.log`).
- `cargo test -p brokk-mj-store database::mailbox::tests`: 4 passed, including /clear discard and removed-target compatibility (`memory-instructions-review5-store-tests.log`).
- `cargo clippy -p brokk-mj-store -p brokk-mj-daemon --all-targets -- -D warnings`: passed (`memory-instructions-review5-clippy.log`).
- Explicit stop of the private recovery-probe instance confirmed it was already stopped (`memory-instructions-review5-probe-cleanup.log`).
- Formatting and diff checks are clean. No still-failing checks, default-instance operations, pushes or Milestone 3 live acceptance. Logs remain untracked; schema, relay snapshot and RuntimeEvent shapes remain unchanged. Corrections are committed and submitted for re-review.


## Sixth review: owner-decided finality

The parent replaced the earlier StartupCleanup/Destroying terminal rule after provisioning.rs showed automatic retry through StartupCleanup back to Provisioning. Intermediate states settle nothing. Final failed-startup settlement now occurs only in save_startup_cleanup_outcome, after its archived=1 write in that same transaction. Generic lifecycle/state writers no longer settle messages. Enqueue reads the durable archived failed-startup fact, so a delayed sender after final cleanup gets the stored failure while ordinary Error remains recoverable. Destruction uses existing record removal/TargetGone.

The manager retains the launch registration through unarchived cleanup and retry; committed final archival/removal releases it, and a published worker protocol hands it over to the attached worker. Worker acceptance SQL is unchanged: the failure update and acknowledgement each require accepted_at IS NULL. In-progress Destroying no longer fills the failure marker ahead of a valid acknowledgement; an accepted row also survives final archived cleanup.

One deterministic test per finding: startup_retry_preserves_parent_message_and_launch_registration (new), destroying_preserves_worker_mailbox_acceptance (new), and the extended failed_startup_settles_queued_parent_messages_without_attachment (late send after final archived Error). All affected checks completed outside the sandbox on the dev profile:

- `cargo test -p brokk-mj-daemon startup_retry_preserves_parent_message_and_launch_registration`: 1 passed (`memory-instructions-review6-retry-test.log`).
- `cargo test -p brokk-mj-daemon destroying_preserves_worker_mailbox_acceptance`: 1 passed (`memory-instructions-review6-acceptance-final-test.log`). The interrupted attempt ended during compilation and was restarted only after confirming no process remained.
- `cargo test -p brokk-mj-daemon failed_startup_settles_queued_parent_messages_without_attachment`: 1 passed, now covering the archived final Error and original delayed sender error (`memory-instructions-review6-finality-test.log`).
- `cargo test -p brokk-mj-daemon launch_reservation_releases_on_precommit_abort_and_terminal_publication`: 1 passed (`memory-instructions-review6-reservation-test.log`).
- `cargo test -p brokk-mj-daemon send_to_registered_child_before_attachment_queues_mailbox_without_a_turn`: 1 passed (`memory-instructions-review6-routing-test.log`).
- `cargo test -p brokk-mj-daemon outbox_`: 3 passed (`memory-instructions-review6-outbox-tests.log`).
- `cargo test -p brokk-mj-store database::mailbox::tests`: 4 passed, including late acknowledgement after /clear (`memory-instructions-review6-store-tests.log`).
- `cargo clippy -p brokk-mj-controller -p brokk-mj-store -p brokk-mj-daemon --all-targets -- -D warnings`: passed (`memory-instructions-review6-clippy.log`).

The exact finality writer is save_startup_cleanup_outcome: its archive branch writes archived=1, then settles unaccepted ParentMessages before committing. Generic lifecycle writers no longer settle. No schema, relay snapshot or RuntimeEvent shape changed. Formatting/diff checks are clean; diagnostic logs remain untracked. No still-failing checks or default-instance operations. No review findings from this round are overridden.


## Seventh review: recovering child-create mailbox ownership

recover_creates previously restarted a durable child create with an empty manager registration map. It now awaits restore_starting for the child before starting the recovered lifecycle. Startup recovery completes this admission before serving sends. The existing manager owner handles an idempotent Restore event and follows its existing committed-state/handover rules; no persisted field, wire shape or alternate route predicate was added.

The deterministic regression persists a provisioning child's create intent and first queued message, stops its manager, restores a new manager twice, then sends a new request before any worker attaches. Both messages remain mailbox-queued and neither has a turn acceptance. The first run's fixture reset did not create the durable intent (`memory-instructions-review7-recovery-test.log`); the fixture now seeds it through the actual cleanup/retry owner transaction. WSL restarts interrupted subsequent compilation; each resume checked for surviving processes before rebuilding. Source checkpoint 5604ba37b preceded long validation and is amended into the final commit.

Checks outside the sandbox on the dev profile, with inherited MJ_CONFIG_DIR and MJ_DATA_DIR cleared; the tests then install their own isolated configuration/data directories and named instance:

- `cargo test -p brokk-mj-daemon recovered_child_create_queues_new_messages_before_attachment`: 1 passed (`memory-instructions-review7-recovery-final-test.log`).
- `cargo test -p brokk-mj-daemon a_failure_after_a_sub_agent_registers_moves_it_to_error`: 1 passed (`memory-instructions-review7-registration-test.log`).
- `cargo test -p brokk-mj-daemon launch_reservation_releases_on_precommit_abort_and_terminal_publication`: 1 passed (`memory-instructions-review7-reservation-test.log`).
- `cargo test -p brokk-mj-daemon send_to_registered_child_before_attachment_queues_mailbox_without_a_turn`: 1 passed (`memory-instructions-review7-routing-test.log`).
- `cargo test -p brokk-mj-daemon outbox_`: 3 passed (`memory-instructions-review7-outbox-tests.log`).
- `cargo clippy -p brokk-mj-controller -p brokk-mj-daemon --all-targets -- -D warnings`: passed (`memory-instructions-review7-clippy.log`).

No still-failing checks. Formatting/diff checks are clean. No default instance, live harness acceptance or push was used. Diagnostic logs remain untracked; the final candidate is submitted for re-review.


## Eighth review: legacy profiles, host login and memory data

Parent rulings: no legacy profile migration in this lane. For Kimi/OpenCode/Grok, a symlinked legacy staged home now returns the same unsupported-channel outcome as unavailable append capability: ordinary sessions continue with a Warning naming the shared home and “Suspend this session, then Resume it” to restage privately; automated reviewer/brief-writer roles fail visibly. The I/O thread detects the link before any managed-section write and retains the write lease through the operation. Shared files and the profile link remain unchanged.

configure_home_environment remains suitable for host/profile commands and preserves OPENCODE_CONFIG_DIR. configure_staged_home_environment applies the existing private config pin for staged launch configuration, retained worker preparation and native-file delivery tests. Host login therefore retains its configured provider/plugin directory.

Project memory remains in the approved system append. A single escape_prompt_data helper encodes ampersands and angle brackets in file contents, so closing/opening memory tags and any other tag text stay data. MEMORY_GUIDANCE now states that the index is data written by agents, to verify against current instructions/code rather than obey. All five role goldens include a hostile closing memory-index tag plus an imperative; the output contains escaped tag data inside the sole real index block.

Source and goldens were saved in local WIP checkpoints before long builds, then amended into the final candidate. Checks ran outside the sandbox on the dev profile, clearing inherited MJ_CONFIG_DIR/MJ_DATA_DIR:

- `MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-worker-runtime golden_session_instructions_by_durable_role`: 1 passed, regenerated five goldens (`memory-instructions-review8-golden-update.log`).
- `cargo test -p brokk-mj-worker-runtime legacy_native_profile_links_warn_without_writing_shared_home`: 1 passed across three harnesses, including automated-role failures (`memory-instructions-review8-legacy-test.log`).
- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed, including non-update golden validation, native-file ownership and pinned-adapter lifecycle (`memory-instructions-review8-worker-tests.log`).
- `cargo test -p brokk-mj-core only_claude_on_macos_keeps_its_own_home_for_profile_commands`: 1 passed, preserving the explicit host config override (`memory-instructions-review8-host-test.log`).
- `cargo test -p brokk-mj-core every_harness_is_pointed_at_its_staged_home`: 1 passed (`memory-instructions-review8-staged-test.log`).
- `cargo test -p brokk-mj-core project_memory::tests`: 12 passed (`memory-instructions-review8-memory-tests.log`).
- `cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime -p brokk-mj-worker-staging --all-targets -- -D warnings`: passed (`memory-instructions-review8-clippy.log`).

Formatting/diff checks are clean. No still-failing checks, default-instance operations, schema/wire changes, pushes or Milestone 3 acceptance. Logs remain untracked. No findings from this round were overridden; the final candidate is submitted for re-review.


## Ninth review: negotiated MCP guidance

The ACP open owner computes MCP availability once from the initialized adapter capabilities and passes it to the instruction composer and existing post-open warning. A Muse host without the HTTP grant now receives memory data and role text without history or agent-tool guidance. Worker/oracle roles use their existing last-message report wording when handback is unavailable. Failed-host diagnostics retain their original precedence.

The existing Muse ACP fixture now offers both history and agent sockets and captures the actual system append. It checks both tool groups are described when granted and omitted when withheld. An initial fixture edit hit the native-probe helper instead of the fake-adapter helper; that compile error was corrected before validation. No production behavior was changed to accommodate the fixture.

Saved a local source checkpoint before compilation. Checks ran outside the sandbox on the dev profile, clearing inherited MJ_CONFIG_DIR/MJ_DATA_DIR; existing test isolation remains in place:

- `cargo test -p brokk-mj-worker-runtime muse_sessions_receive_mcp_servers_and_say_so_when_the_host_withholds_them`: 1 passed (`memory-instructions-review9-muse-test.log`).
- `cargo test -p brokk-mj-worker-runtime a_muse_host_that_could_not_start_reports_its_own_diagnostic`: 1 passed (`memory-instructions-review9-diagnostic-test.log`).
- `cargo test -p brokk-mj-worker-runtime session_instructions`: 4 passed (`memory-instructions-review9-worker-tests.log`).
- `cargo clippy -p brokk-mj-worker-runtime --all-targets -- -D warnings`: passed (`memory-instructions-review9-clippy.log`).

Formatting/diff checks are clean. No still-failing checks, default-instance operations, schema/wire changes, pushes or Milestone 3 acceptance. Logs remain untracked. No findings from this round were overridden; the final candidate is submitted for re-review.


## Tenth review: Claude registration outcome and external memory data

configure_claude_subagent_mcp returns whether it installed the role servers. Worker preparation carries that actual outcome through the existing profile_registration field instead of unconditionally reporting true. The composer uses it for Claude agent/review guidance and handback availability. Shared legacy MessageOnly profiles still open without mutating the user's profile; registered private profiles keep their guidance. No launch, relay or journal serialization field was added.

The formatter now applies escape_prompt_data to the memory directory, repository identifiers, repository paths and unavailable-index reason as well as index contents. The delimiter-injection regression feeds tag and ampersand text through every data position, for both available and unavailable indexes, and verifies the only real block delimiters belong to the composer.

The shared-profile integration regression verifies a skipped registration reports false and preserves the profile bytes. The recorded ACP lifecycle fixture now distinguishes registered and unregistered Claude profiles and checks both send_message and request_review guidance. Its first run incorrectly expected an empty append object when no standing text remained; delivery correctly omits that parameter. The fixture now checks omission.

Source was checkpointed before each long build. Checks ran outside the sandbox on the dev profile with inherited MJ_CONFIG_DIR/MJ_DATA_DIR cleared; existing isolated test directories remain in place:

- `cargo test -p brokk-mj-core memory_block_delimiters_cannot_be_injected_by_paths_or_read_errors`: 1 passed (`memory-instructions-review10-escaping-test.log`).
- `cargo test -p brokk-mj-worker-runtime the_claude_registration_never_edits_a_profile_that_may_be_the_persons_own`: 1 passed (`memory-instructions-review10-registration-test.log`).
- `cargo test -p brokk-mj-worker-runtime session_instructions`: golden, native-file and shutdown regressions passed; lifecycle fixture failed with the empty-append expectation described above (`memory-instructions-review10-worker-tests.log`).
- `cargo test -p brokk-mj-worker-runtime session_instructions_system_prompt_contract_replays_pinned_adapters`: 1 passed after correcting the fixture (`memory-instructions-review10-lifecycle-final-test.log`).
- `cargo clippy -p brokk-mj-core -p brokk-mj-worker-runtime --all-targets -- -D warnings`: passed (`memory-instructions-review10-clippy.log`).

Formatting/diff checks are clean. No still-failing checks, default-instance operations, schema/wire changes, pushes or Milestone 3 acceptance. Logs remain untracked. No findings were overridden; the final candidate is submitted for re-review.
