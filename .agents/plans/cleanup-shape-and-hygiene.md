# Reshape mj for size, dispatch and hygiene without changing scope

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` at the repository root and must be maintained in accordance with that file.

## Purpose / Big Picture

mj is a Rust workspace of about 325,000 lines, half of it tests. It works, but its shape slows every change: 31 source files are over 3,000 lines, five coding-agent harnesses (Claude, Codex, Kimi, Grok, Muse) are told apart by 206 hand-written `match` arms spread over 44 files, and the same five target types exist twice in `mj-core` with no conversion between them, so the controller builds the second form by hand in about 160 places. The test suite repeats fixtures and has a handful of tests that fail under load. Eight dependencies are declared but unused, and versions are declared per crate rather than once.

After this plan, every user-facing feature behaves as before. A contributor can find the code for one responsibility in a file of ordinary size, add a harness fact in one place, convert a stored target into an execution plan with one call, and trust that `cargo test` passes on a loaded machine. The release build is unchanged in behavior; the difference shows in `wc -l`, in `cargo machete` reporting nothing, in `cargo tree -d` losing six duplicate crates, and in the environment-variable documentation matching the code.

## Progress

- [x] (2026-09-16) Stage 1.1: harness facts live on `impl HarnessKind` (`mj-core/src/config/harness.rs`, credential JSON methods in `credentials.rs`). Commit ad0f9460.
- [x] (2026-09-16) Stage 1.2: `TryFrom<StoredTarget>` and Podman/SSH `From` conversions in `mj-core/src/targets/convert.rs`; `backend_locator` reduced to a lookup plus the conversion. Commit 34082761.
- [x] (2026-09-16) Stage 1.3: `[workspace.dependencies]` has 47 entries, lock byte-identical; reqwest 0.13; eight unused dependencies removed, `cargo machete` clean. Commits 9333674e, 3b4a3c48, ae2b80eb.
- [ ] Stage 1.3 remaining: sha2 0.11 (23 call sites across seven crates plus consolidation of three hex helpers). Moved to stage 3, after the stage-2 merges.
- [x] (2026-09-16) Stage 1.4: config, state and credentials tests moved to sibling files; `config.rs` split into `config/{harness,targets,ui,loading}.rs`, `relay/snapshot.rs` into `snapshot/{apply,budget,digest}.rs`; `failed_login_never_returns_an_ambient_environment` fixed. Commits 0040e426, 26c5f500, 6fb557d4. Merged as ffbe2f25; full suite 3,409 passed, 0 failed.
- [ ] Stage 3 additions found in stage 1: `local_sockets` tests race on the process working directory (`a_short_path_binds_without_switching_directory` vs `a_long_path_binds...`); `capture_deadline_includes_descendants_holding_output_pipes` needs a decision about `BoundedProcessExecutor` error wording.
- [x] (2026-09-16) Stage 2A: 24 test modules moved, 12 files split into directory modules (every production file under 2,000 lines), harness facts used, `RefusingExecutor`/`IsolatedTest`/`install_fake_command` shared, #1036 and the npm-upgrade ETXTBSY fixed at the source (symlinked fixture dispatcher, hard-linked binary), relay-proxy stderr tail made incremental, `doctor.rs` error-text checks 11 to 1 (typed `PodmanProbeFailure`, one `classify_ssh_stderr`). `MJ_UTILITY_LIVE_*` were test-only; nothing removed. Merged as 3a1703f3.
- [x] (2026-09-16) Stage 2B: 5 test modules moved, `relay.rs`/`acp.rs`/`unix.rs`/`checkpoint.rs`/`projection.rs` split (largest production file now 1,250 lines), git test runner shared in mj-worker and mj-checkpoint, long-root socket test fixed at the source (daemon-exit race, now checkpoint-only daemon), `MJ_CHECKPOINT_BENCH_PHASES` removed from production (`ARCHIVE`/`HARNESS_HOME` were test-only), `SESSION_SETUP_GUIDANCE` exported so producer and test share one string. No stage-1 API applied: nothing matched with an identical value.
- [x] (2026-09-16) Stage 2C: 15 test modules moved, 10 files split (every production file under 2,000 lines), `credential_file_name()` used in mj-client, nine fixture copies collapsed into `test_support` modules, one `rerun_in_isolated_child` helper in mj-cli. `MJ_CHAT_CAPTURE_*` and `MJ_GO_CAPTURE_PATH` are test-only screenshot hooks; nothing removed. `import_label` kept because two of five strings differ from `display_name()`.
- [x] (2026-09-16) Stage 3, part 1: `.gitignore` pattern fixed, pinned `cargo machete` CI job, kept variables documented in `configuration.md`, `.agents/docs/internal-environment-variables.md` written. Commit 5e62bebb.
- [ ] Stage 3, part 2 (in progress): sha2 0.11 with one hex helper, dead `MJ_DISCOVER_LOGIN_PATH` constant removed, five more `HarnessKind` fact methods (`mcp_config_file`, `supports_delegation_tools`, `native_session_dirs`, managed entrypoint, `marks_own_turn_end`), `local_sockets` cwd race serialized.
- [ ] Stage 3, part 3: measurements re-taken, retrospective written.

## Surprises & Discoveries

- Observation: The "about 160 hand-built plan-form target constructions" baseline was a measurement artifact. The grep counted `match` patterns such as `targets::TargetLocator::LocalBare { .. } =>`. The real duplication was two stored-to-plan conversion functions in mj-controller, `backend_locator` (`controller/backend.rs`) and a drifted copy `recovery_backend_locator` (`controller/recovery_scan.rs`).
  Evidence: stage 1b report; after the change one shared `TryFrom` remains in `mj-core/src/targets/convert.rs` plus the deliberately kept drifted copy.
- Observation: `recovery_backend_locator` differs from `backend_locator` in three ways that look like defects: podman `workspace_storage` is replaced by `Default::default()`, a borrowed target's `worker_id` becomes `None`, and a missing AWS address becomes the literal destination `ssh_user@unavailable.invalid` instead of an error. Behavior was kept as-is; it needs its own decision.
  Evidence: `mj-controller/src/controller/recovery_scan.rs` lines 807, 824, 840, 865-868 before stage 1b.
- Observation: The stored target form cannot convert to the plan form on its own. `SshBare` stores only a host and `AwsEc2` only an instance id and address, so the conversion needs the target template and the session id. The interface is `TryFrom<StoredTarget<'_>>` with a named `TargetConversionError`, not `From<&state::TargetLocator>`. Likewise `config::TargetTemplate` to `targets::TargetTemplate` needs per-session resource allocation and stays in mj-controller.
  Evidence: stage 1b report.
- Observation: reqwest 0.13 renamed its TLS features. `rustls` now means aws-lc-rs; `rustls-no-provider` keeps ring, which works only because every binary already calls `install_rustls_crypto_provider()` first. `.query()` needs the new `query` feature. No Rust source changed.
  Evidence: stage 1b commit "Upgrade reqwest to 0.13".
- Observation: sha2 0.11 is not a version bump. digest 0.11 returns an array without `LowerHex` or `io::Write`, breaking 23 call sites across seven crates, six of them in files another agent owned. Stopped; it should follow stage 1.4 and consolidate the three existing hex helpers (`mj-controller/src/server/api.rs`, `mj-client/src/session.rs`, `mj-core/src/state.rs`).
  Evidence: stage 1b report, 25 `cargo check` errors.
- Observation: flate2 1.1.9 (locked) and 1.1.10 differ in default features (`runtime_detection` added). The workspace table pins the feature list explicitly, so a future bump needs a decision rather than changing mj-worker's build silently.
  Evidence: stage 1b report.
- Observation: `MJ_E2E_SSH_HOST` is read only inside a `#[cfg(test)]` module (`mj-core/src/targets/ssh.rs`), so the plan's claim that it is on a production path was wrong. Nothing to remove. The other environment-variable claims should be re-checked the same way before acting.
  Evidence: stage 1a report.
- Observation: A second login-shell test, `capture_deadline_includes_descendants_holding_output_pipes`, failed once under load with "assertion failed: ...contains(\"did not answer\")". The test is about the deadline, so raising it would destroy its purpose. Possible cause: `BoundedProcessExecutor::execute` (`mj-core/src/targets.rs`) rewrites the error as "did not answer" only when the inner call returns `Err`. Not confirmed.
  Evidence: stage 1a report.
- Observation: Every environment-variable removal the plan named turned out to be a test-only read once checked against `#[cfg(test)]` boundaries: `MJ_E2E_SSH_HOST`, `MJ_UTILITY_LIVE_*`, `MJ_CHAT_CAPTURE_*`, `MJ_GO_CAPTURE_PATH`, `MJ_CHECKPOINT_BENCH_ARCHIVE` and `_HARNESS_HOME`. Only `MJ_CHECKPOINT_BENCH_PHASES` gated production code. The baseline counted variables per file, not per code region, and test modules under 500 lines stayed inline where the count could not see them. `MJ_DISCOVER_LOGIN_PATH` is the opposite case: a constant only ever passed to `environment.remove`, set nowhere.
  Evidence: stage 2A, 2B, 2C reports; `grep -rn MJ_DISCOVER_LOGIN_PATH` shows only removes.
- Observation: Splitting a module is not a pure text move. Private items need `pub(super)`, former `pub(super)` items need `pub(in <parent>)` to keep the same reach, explicit `super::` paths point one level deeper, `include_str!` paths gain a `../`, and a new module name can shadow a glob re-export (`relay::protocol`, `unix::proxy`) with only a warning. The compiler catches the first and last; the others need a reviewer.
  Evidence: stage 2A, 2B, 2C reports.
- Observation: No duplicate tests exist. Agents hashed normalized test bodies across 1,205 controller tests and every client and worker test and found no two alike. The fixture duplication was real (git runners, refusing executors, key/mouse/draw helpers, self-spawn boilerplate); whole-test duplication was not.
  Evidence: stage 2A, 2B, 2C reports.
- Observation: "Write to a temp name and rename" does not cure ETXTBSY. A rename keeps the inode, and a forked child's inherited write descriptor refers to the inode. The cures are a hard link (when the child needs `current_exe()` to resolve inside the fixture) or a symlink to a checked-in dispatcher that this process never opened for writing. The dispatcher may use only shell builtins because tests replace `PATH`.
  Evidence: stage 2A commit "Stop tests exec'ing files they have just written"; `.agents/plans/instant-list-profiles.md` analysis.
- Observation: The long-root socket test failed from a daemon-exit race, not an accept race: the daemon returned `Err` (missing ACP bridge) before the status request was served, so the listener dropped and the kernel reset the connection.
  Evidence: stage 2B report; sibling tests `expect_err` on the same path.
- Observation: `failed_login_never_returns_an_ambient_environment` failed once more after its deadline went to 600 seconds, so the deadline was not the cause. The assertion now prints the error text; six looped runs did not reproduce it. Cause unknown.
  Evidence: merged-tree run after stage 2B, `assertion failed: error.contains("42")`.
- Observation: The session scratchpad is shared between parallel agents; one agent's helper scripts and a validation log were overwritten by another's. Parallel agents should use per-agent subdirectories.
  Evidence: stage 2A report.
- Observation: `cargo test` without `--no-fail-fast` stops at the first failing crate, so a late crate's regression can hide behind an early crate's flake. Validation now uses `--no-fail-fast`.
  Evidence: stage 2B report.
- Observation: Agent worktrees created with `isolation: "worktree"` branched from the hel3 worktree's line (`1a5be3c6`), not from master. Merging them as-is would have carried the user's in-progress hel3 commits into master; the first cherry-pick attempt conflicted on moved test text that contained hel3 edits. Both branches were rebased onto master before landing.
  Evidence: `git merge-base master <branch>` was 7fd533eb while `git log master..<branch>` showed 13 hel3 commits.

## Decision Log

- Decision: This pass changes shape, not scope. Allowed changes are log and error text, a daemon protocol version bump, compatible schema migrations, and moving code. Not allowed: removing or changing a user-visible feature, raising the 2.8.0 compatibility floor, or removing shims that read stored content.
  Rationale: The user chose "small internal changes" as the behavior budget and "no scope cuts" as the standing rule.
  Date/Author: 2026-09-16, user.
- Decision: Items deferred by the previous cleanup stay deferred: the drafts-store merge, dictation through the daemon, import transcripts as observations, the self-updater via install.sh, and issues #1043, #1044, #1032, #1047.
  Rationale: The user chose not to fold related work into this pass.
  Date/Author: 2026-09-16, user.
- Decision: File splits happen in two steps: inline test modules over 500 lines move to sibling `tests.rs` files first, then production files still over 2,000 lines are split by responsibility.
  Rationale: Moving tests is a verbatim move with near-zero risk and halves many files by itself. Responsibility splits are then smaller and easier to review.
  Date/Author: 2026-09-16, user.
- Decision: Per-harness dispatch is consolidated by extending `impl HarnessKind` with data methods, not by introducing a trait or a descriptor table.
  Rationale: `HarnessKind` already carries 15 such methods, so this is the existing pattern. Behavior-heavy code (usage pollers, ACP quirks, importers) stays where it is.
  Date/Author: 2026-09-16, user.
- Decision: The two target type families stay separate types with one conversion between them. Stored serde encodings are unchanged.
  Rationale: Merging the types would require proving that the second serde encoding is never persisted, or a migration. A conversion removes the 160 hand-built constructions without touching stored bytes.
  Date/Author: 2026-09-16, user.
- Decision: Stage 2 runs as three Opus agents in separate git worktrees, one branch each, merged into `master` by the orchestrator after the full test suite passes on the merged tree. This plan is the explicit authorization for those branches.
  Rationale: The user chose parallel agents per crate, and worktrees keep the crate partition physical.
  Date/Author: 2026-09-16, user.
- Decision: A move commit contains only moves and re-exports; an edit commit contains only edits.
  Rationale: Git's rename detection makes a pure move reviewable at a glance. Mixing edits into a move forces the reviewer to read every relocated line.
  Date/Author: 2026-09-16, agent.
- Decision: Tests were not moved out of crates into `tests/` directories. Every self-spawning test drives crate-private code, and making that surface `pub` for an out-of-crate test would widen visibility, the opposite of the split work. They share one spawn helper per crate instead.
  Rationale: stage 2A and 2C findings.
  Date/Author: 2026-09-16, agents, accepted by orchestrator.
- Decision: The worker-binary test table that lists each harness's instructions file keeps its literals rather than calling `agent_instructions_file()`.
  Rationale: Substituting the method would make the test compare the implementation to itself; the table is the only independent statement of the mapping.
  Date/Author: 2026-09-16, agent A.
- Decision: `recovery_backend_locator` keeps its drifted behavior and is not routed through the shared conversion.
  Rationale: Two of its three differences look like defects (dropped podman workspace storage, dropped `worker_id`); proving that needs the recovery path's history and belongs in its own issue, not a refactor.
  Date/Author: 2026-09-16, agent 1b, accepted by orchestrator.
- Decision: No CI size or complexity ratchets are added. `.agents/plans/` history is left alone.
  Rationale: The user declined both.
  Date/Author: 2026-09-16, user.
- Decision: Environment variables for screenshot capture (`MJ_CHAT_CAPTURE_*`, `MJ_GO_CAPTURE_PATH`), checkpoint benchmarking (`MJ_CHECKPOINT_BENCH_*`) and live-test profile selection (`MJ_UTILITY_LIVE_*`, `MJ_E2E_SSH_HOST`) leave production code paths. Dev tuning knobs and user-facing variables stay and get documented. Parent-to-child handoff variables stay as internal.
  Rationale: The user chose these three classes as niche.
  Date/Author: 2026-09-16, user.

## Outcomes & Retrospective

(to be written at completion)

## Context and Orientation

The repository is a Cargo workspace. Run every Cargo command from the repository root. The crates:

`mj-core` holds shared types: configuration (`mj-core/src/config.rs`, which defines `HarnessKind`, `TargetTemplate`, `ContainerTemplate`, `PodmanWorkspaceStorage`), durable session state (`mj-core/src/state.rs`, which defines `TargetLocator` and `PodmanWorkspaceLocator` as stored in the database with a kebab-case serde `kind` tag and `PathBuf` paths), and execution plans (`mj-core/src/targets.rs`, which defines a second `TargetTemplate`, `ContainerTemplate`, `PodmanWorkspaceStorage`, `TargetLocator` and `PodmanWorkspaceLocator` with a snake_case tag and `String` paths, used to build the argv vectors that run on a target). A *target* is where a worker runs: the local machine, a local podman or docker container, an Apple container, an EC2 instance, or an ssh host with or without a container. A *harness* is the coding agent CLI a worker drives.

`mj-controller` is the daemon: SQLite store (`database.rs`), web server (`server.rs`, `server/api.rs`, `server_runtime.rs`), worker supervision (`controller/*.rs`, `session_manager.rs`, `worker_client.rs`), quota pollers (`*_usage.rs`, `utility_llm.rs`), diagnostics (`doctor.rs`). `mj-worker` runs one harness on a target and keeps the relay journal (`relay.rs`, `worker_runtime/unix.rs`, `acp.rs`). `mj-checkpoint`, `mj-transcript` and `mj-review` are worker-side libraries. `mj-tui`, `mj-chat`, `mj-cli` and `mj-client` are the terminal client. `mj-desktop` and `voice-worker` are workspace members but not default members; `mj-desktop` needs GTK development libraries to build.

Tests are colocated in `#[cfg(test)] mod tests` blocks. Some files keep tests in a sibling file already (`#[cfg(test)] mod tests;` with `tests.rs` next to the module). Three crates have a `test_support.rs` with shared fixtures: `mj-controller/src/controller/test_support.rs`, `mj-tui/src/test_support.rs`, `mj-chat/src/chat/test_support.rs`. Twelve files re-run the test binary as a child process (`Command::new(std::env::current_exe())` with `--exact`) to test process behavior. `mj_core::test_hooks` is correctly gated behind the `test-hooks` feature and is not part of this plan.

Baseline measurements on master 7fd533eb (2026-09-16): 31 files over 3,000 lines; 184 inline test modules and 31 dedicated test files; `HarnessKind::Claude` referenced 206 times in 44 non-test files; about 160 hand-built `targets::TargetLocator::X { .. }` constructions in mj-controller (`controller/worker_binary.rs` 90, `controller/checkpoint.rs` 22, `controller/resume.rs` 13, `controller/provisioning.rs` 13, `controller/reviewer.rs` 12, `controller.rs` 9); 8 cargo-machete hits (mj-worker: regex, zstd; mj-controller: image, serde_yaml; mj-chat: sha2; mj-cli: axum-server, getrandom, url); 10 crates present in two or three versions; 31 non-test `MJ_*` environment variables read in code, 12 documented; about 157,000 test lines against 169,000 production lines.

The largest files: `mj-controller/src/server.rs` 7,600; `mj-worker/src/relay.rs` 7,072; `mj-tui/src/render.rs` 6,578; `mj-chat/src/chat.rs` 6,331; `mj-chat/src/chat/active.rs` 6,133; `mj-controller/src/controller/worker_binary.rs` 5,904; `mj-controller/src/daemon.rs` 5,796; `mj-controller/src/session_manager.rs` 5,498; `mj-controller/src/database.rs` 5,168; `mj-checkpoint/src/checkpoint.rs` 5,072; `mj-controller/src/server_runtime.rs` 5,051; `mj-tui/src/lib.rs` 4,858; `mj-controller/src/controller/checkpoint.rs` 4,794; `mj-transcript/src/projection.rs` 4,579; `mj-cli/src/dashboard.rs` 4,378; `mj-controller/src/server/api.rs` 4,194; `mj-tui/src/dialogs.rs` 4,084; `mj-core/src/config.rs` 3,843.

## Plan of Work

Stage 1 changes `mj-core` and the workspace manifest, serially, one commit per item, because every other crate depends on them.

First, harness facts. `impl HarnessKind` at `mj-core/src/config.rs:403` already has methods such as `home_env`, `display_name`, `default_home_leaf` and `launch_flag_for`. Every `match` on `HarnessKind` elsewhere that returns *data* about a harness moves onto this impl: credential file names and the JSON keys that hold token expiry (`mj-core/src/credentials.rs` around lines 216 to 270), login commands (`credentials.rs` around line 534), binary names, event-name aliases, home leaves, skills paths (`mj-core/src/skills.rs`). Matches that perform different *behavior* per harness (usage pollers in `mj-controller/src/*_usage.rs`, ACP quirks in `mj-worker/src/acp.rs`, import parsers) stay. The review test is: after this pass, every remaining `match` on `HarnessKind` outside mj-core does something different per harness, not merely names something different.

Second, one conversion between the two target type families. Add `From` (or `TryFrom` where the plan form needs information the stored form lacks, with an explicit error) in `mj-core` from the stored types to the plan types, and replace every hand-built construction in mj-controller. Both serde encodings stay exactly as they are; this commit changes no stored bytes. Path text is turned into `String` in one place at the conversion boundary.

Third, dependencies. Every version shared by two or more crates moves into `[workspace.dependencies]` in the root `Cargo.toml`, and crates use `workspace = true`. reqwest goes from 0.12 to 0.13 (declared in mj-controller, mj-cli, mj-desktop) and sha2 from 0.10 to 0.11 (seven crates) to match `brokk-anvil-client`; the reqwest, sha2, digest, block-buffer, crypto-common and cpufeatures duplicates should then disappear from `cargo tree -d`. The rand, thiserror, strum and itertools duplicates come from `oauth2`, `ratatui`, `reedline` and `agent-client-protocol-schema` and are left alone. The eight cargo-machete hits are removed one at a time, each confirmed by `cargo check --all-targets`; machete cannot see macro-only or feature-only uses. After each dependency change run `cargo check -p brokk-mj-voice-worker` as well, because it is not a default member.

Fourth, mj-core file shape. Inline test modules over 500 lines move verbatim to sibling `tests.rs` files declared with `#[cfg(test)] mod tests;`. Then `config.rs` and any other mj-core file still over 2,000 lines is split by responsibility into a directory module, with `pub use` re-exports so that no path used by another crate changes. The test `mj_core::login_environment::tests::failed_login_never_returns_an_ambient_environment` runs a login shell under a 2-second timeout and fails under build load; it is fixed at the source.

Stage 2 runs three agents in parallel, each in its own git worktree and branch, each confined to its crates and forbidden from touching mj-core, CI workflows or another agent's crates. Each agent does four things in order, as separate commits: move inline test modules over 500 lines to sibling files verbatim; split production files still over 2,000 lines into directory modules by responsibility, with moves and re-exports only; replace remaining `HarnessKind` data matches and hand-built plan-form target types with the stage-1 APIs; and clean the test suite by sharing fixtures through the crate's `test_support.rs`, deleting tests only where another test covers the same behavior (naming both in the commit message), and fixing flaky tests at the source.

Agent A owns mj-controller. Ten of the twelve self-spawning test files are here (`controller.rs`, `controller/worktree.rs`, `controller/lifecycle.rs`, `controller/checkpoint.rs`, `controller/provisioning.rs`, `session_manager.rs`, `controller/recovery_scan.rs`, `controller/resume.rs`, `controller/worker_binary.rs`, `controller/move_session/tests.rs`); they move to `mj-controller/tests/` sharing one spawn helper, or stay with a shared helper where crate-private access is needed, and the agent says which. Flaky tests to fix: issue #1036 (`codex_usage` tests, `Text file busy`), `controller::update::tests::npm_upgrade_restarts_after_the_running_package_is_removed` (a binary replaced while running; write to a temporary name and rename), `worker_client::tests::a_relay_proxy_that_fails_for_another_reason_is_not_retried`, and issue #974 (a logging integration test leaks detached daemons). The `MJ_UTILITY_LIVE_{CODEX,DEEPSEEK,GROK,KIMI,MUSE}_PROFILE` reads leave `utility_llm.rs` production paths and move into the live test that uses them. The eleven error-text `.contains(` checks in `doctor.rs` become typed results where the producer is in this repository; text from external tools may stay string-matched through one helper per tool.

Agent B owns mj-worker, mj-checkpoint, mj-transcript and mj-review. mj-worker is 72 percent tests; `worker_runtime/relay_tests.rs` and `acp/tests.rs` are the places to look for per-harness copies of one assertion. The test `worker_runtime::relay_tests::a_worker_binds_its_sockets_under_a_root_longer_than_sun_path` resets its connection under load and is fixed at the source. `MJ_CHECKPOINT_BENCH_{ARCHIVE,HARNESS_HOME,PHASES}` leave `mj-checkpoint/src/checkpoint.rs` and `archive.rs`; if the benchmark is still wanted it becomes a test-only or example-only entry point. `MJ_E2E_SSH_HOST` in `mj-core/src/targets/ssh.rs` is handled in stage 1.4 alongside the other mj-core work.

Agent C owns mj-tui, mj-chat, mj-cli and mj-client. `MJ_CHAT_CAPTURE_{PATH,ROWS,COLUMNS}` leave `mj-chat/src/chat/active.rs` and `MJ_GO_CAPTURE_PATH` leaves `mj-tui/src/go.rs`, after checking `docs/` and `scripts/` for a consumer. The two remaining self-spawning test files, `mj-cli/src/import.rs` and `mj-cli/src/dashboard/io.rs`, follow Agent A's rule.

The orchestrator reviews each branch (moves separate from edits, deleted-test list in commit messages, no mj-core edits), merges it into `master`, and runs clippy plus the full test suite on the merged tree before merging the next.

Stage 3 finishes serially: `.gitignore` names `.mjolnir/worktrees/` while `mj-controller/src/controller/worktree.rs:216` writes `.mj/worktrees/`, so the pattern is corrected (nothing under `.mj/` or `.claude/` is deleted; those are live session and agent worktrees). A `cargo machete` step joins `.github/workflows/ci.yml`. The kept environment variables are documented: user-facing (`MJ_CONFIG_DIR`, `MJ_DATA_DIR`, `MJ_INSTANCE`, `MJ_WORKER_BINARY`, `MJ_WORKER_DIR`, `MJ_CONTROLLER_BINARY`, `MJ_BIFROST_BIN`, `MJ_SSH_MAX_CONCURRENT`) and dev tuning knobs (`MJ_DEV_RESTART_STALE_DAEMON`, `MJ_SSH_CONTROL_MASTER`, `MJ_TURN_STALL_TIMEOUT_MS`, `MJ_DISCOVER_LOGIN_PATH`, `MJ_GITHUB_CLI_BIN`) in whichever of `docs/src/content/docs/configuration.md` or `troubleshooting.md` already lists environment variables; parent-to-child handoffs (`MJ_ORIGINAL_BASH_ENV`, `MJ_ORIGINAL_GIT_CONFIG_GLOBAL`, `MJ_CONTROLLER_LOCK_EXPECTED`, `MJ_CONTROLLER_LOCK_PROBE`, `MJ_WORKER_BINARY_OVERRIDE_CHILD`) in a short note under `.agents/docs/`. Then the baseline measurements are re-taken and the retrospective written.

## Concrete Steps

All commands run from the repository root. Validation before every commit:

    cargo clippy --all-targets -- -D warnings
    cargo test > /path/to/log 2>&1; echo "exit $?"

Run `cargo test` outside the restricted sandbox on the dev profile. Never pipe it through `head` or `tail`; that hides the exit status. After a dependency change also run `cargo check -p brokk-mj-voice-worker`.

Measurements, before and after:

    find mj-* -name '*.rs' -not -path '*/target/*' | xargs wc -l | awk '$1>3000 && $2!="total"' | wc -l
    grep -rlE '^#\[cfg\(test\)\]' --include='*.rs' mj-* | grep -vE 'tests?\.rs$' | wc -l
    grep -rlE 'HarnessKind::Claude\b' --include='*.rs' mj-* | grep -vE '/tests?\.rs|_tests\.rs|/tests/' | wc -l
    grep -rhoE 'targets::TargetLocator::[A-Z][A-Za-z]+ \{' --include='*.rs' mj-controller/src | wc -l
    cargo machete
    cargo tree -d -e normal --depth 0

## Validation and Acceptance

Acceptance is behavioral: every test that passed before the plan passes after it, on the merged tree, without retries. `cargo clippy --all-targets -- -D warnings` is clean. `cargo machete` reports no unused dependency. `cargo tree -d` no longer lists reqwest, sha2, digest, block-buffer, crypto-common or cpufeatures twice. No `match` on `HarnessKind` outside mj-core returns only data. No hand-built `targets::TargetLocator::X { .. }` remains in mj-controller outside the conversion itself and tests. The named flaky tests pass ten times in a row under a parallel build (`cargo test <name> -- --test-threads=8` while another build runs). The documented environment-variable list matches the non-test `MJ_*` names read in code.

## Idempotence and Recovery

Every step is a normal commit on a branch; a failed step is reverted with `git revert` or by discarding the branch. Splits are additive moves, so a half-done split still compiles or is reverted as a unit. Dependency removals are one commit each so a false machete hit is a one-commit revert. No step touches stored data, the database schema, or the network.

## Artifacts and Notes

(to be filled with measurement output and merge evidence as stages complete)

## Interfaces and Dependencies

At the end of stage 1, `mj-core` exposes, at minimum, conversions of the form

    impl From<&mj_core::state::TargetLocator> for mj_core::targets::TargetLocator
    impl From<&mj_core::config::TargetTemplate> for mj_core::targets::TargetTemplate

(or `TryFrom` with a named error where information is missing), plus `HarnessKind` methods for every harness fact previously matched elsewhere, with names that describe the fact (for example `credential_file_name`, `login_command`, `credential_expiry_ms`). The root `Cargo.toml` `[workspace.dependencies]` table declares every version shared by two or more crates.
