# Expose fetch to every session

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Every newly launched session should be able to call `mj-agents.fetch` with `session_id`, `path`, and optional `ls`, regardless of its delegation policy. Content lands in the calling session's target temporary directory using the same layout as `mj fetch`; directories remain zip files. Listings return structured entries. Source access retains the CLI's workspace, report-directory, and temporary-directory boundaries.

## Progress

- [x] (2026-10-28) Trace CLI fetch, worker tool registration, durable delegation requests, and transfer limits.
- [x] (2026-10-28) Add shared fetched-file storage and the target-side receive command.
- [x] (2026-10-28) Add fetch requests, dispatch, and tool registration for every session role.
- [x] (2026-10-28) Extend MCP and transfer integration coverage; validate touched crates, the full isolated delegation scenario, and workspace clippy. Correct one outdated Claude permission expectation and rerun only the failing test.
- [x] (2026-10-28) Review, commit on master, merge concurrent upstream changes without rebasing, and push HEAD to origin/master as 9379c006a.
- [x] (2026-10-28) Final merged-tree clippy and formatting passed; agent-dev image workflow 40688502116 published amd64, arm64, and the combined manifest successfully.

## Surprises & Discoveries

The relay limits a frame to 8 MiB, while fetch permits 16 MiB files. Returning encoded file bytes through the delegation reply would break the existing limit. Existing `worker_command` subprocess transfers already support file data without using that relay.

Top-level sessions already receive the MessageOnly role. Managed children without handback have no server, and Kimi normally reads servers from its profile but only reviewer launches currently configure it.

## Decision Log

- Decision: Transfer content to a target-side `store-fetch` command through the shared subprocess helper and return only saved-file metadata through MCP. Rationale: retain CLI file limits without changing transport limits or retaining content in durable delegation replies. Date: 2026-10-28.
- Decision: Retain current role names and add FetchOnly for managed children that cannot hand back. Rationale: universal fetching must not grant messaging or delegation to children. Preserve older top-level launches without a controller grant. Date: 2026-10-28.
- Decision: Share fetched-file storage between CLI and worker in `mj-checkpoint`. Rationale: one interpretation owns the destination layout and validates the session directory component. Date: 2026-10-28.

## Outcomes & Retrospective

Implementation is complete. All 33 focused MCP tests, the 16 MiB worker transfer/truncation regression, the complete isolated delegation scenario, workspace clippy, formatting, and Python syntax checks pass. The full touched-crate suites found only an outdated Claude permission assertion; it now includes fetch and its focused rerun passed. Worker clippy passed again after that test-only correction. The implementation is committed as a7e6f034a; upstream merged cleanly as 9f2686e2b. The source is pushed to origin/master as 9379c006a. Final merged-tree workspace clippy and formatting passed. Image publication workflow 40688502116 succeeded for amd64, arm64, and the combined manifest. No implementation work remains.

## Context and Orientation

`mj-cli/src/api_commands.rs::fetch` uses the HTTP file route and saves bytes locally. `mj-checkpoint/src/archive/fetch.rs` defines fetched content and listings. `mj-worker/src/subagent_mcp.rs` lists tools and submits requests to the worker's durable queue; the daemon dispatches them in `mj-controller/src/server_runtime/api.rs`. `mj-core/src/subagent.rs` owns request wire formats and allowed tools. `mj-core/src/worker_launch.rs` resolves each launch's role. Claude and Kimi read staged profile registrations; other harnesses receive the server through ACP (the harness protocol).

## Plan of Work

First move destination calculation and writing into `FetchedPath::save`, returning serializable metadata. Reuse it from the CLI and a worker `store-fetch` command that consumes length-checked stdin. Extend the existing worker subprocess helper with optional stdin. Then add the Fetch action and daemon dispatch: listings decode to structured JSON; content is read from the source target and streamed to store-fetch on the caller target. Add fetch to each role's definitions and allow rules, add FetchOnly for handbackless managed children, and configure Kimi's profile on normal launches as well.

## Milestones

The first milestone provides identical destination storage for CLI and worker. The second exposes a working MCP fetch through all role registrations. The final milestone extends existing MCP contracts, tests a transfer larger than 64 KiB and the relay frame boundary, and validates the affected crates.

## Concrete Steps

Work from the repository root. Use normal Cargo storage (the existing mbx-managed target link); never redirect it. Run focused tests while editing, then `cargo fmt --all -- --check`, `cargo test -p brokk-mj-checkpoint -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mjolnir`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. All runtime tests retain isolated data directories or use `--instance session-fetch-tool`.

## Validation and Acceptance

Existing MCP contract tests must list fetch for Parent, MessageOnly, Child, and FetchOnly while keeping spawn and handback restricted. A real worker subprocess must save binary content larger than the relay's 8 MiB frame limit intact, zip directories unchanged, return listings, and reject incomplete input without publishing a partial destination. Daemon dispatch coverage must demonstrate the source path checks remain in force and the copy lands on the caller's target. Required checks must pass before committing and pushing.

## Idempotence and Recovery

Fetch overwrites only its deterministic temporary copy, using atomic publication. A repeated content fetch is safe. Delegation requests retain their existing durable identities and supervised execution; subprocess work uses existing bounded cancellation. No migration, live daemon restart, installation, or release is needed.

## Artifacts and Notes

The complete delegation scenario passed in instance fetch-tool-20261028, including 16 MiB binary copies, zipped directories, listings, and source-boundary refusals from both none and native policies. The source checkpoint is a7e6f034a. The full suites passed except the outdated Claude expectation, corrected and verified by cargo test -p brokk-mj-worker the_claude_profile_allows_its_own_sub_agent_tools. Workspace clippy and the final worker clippy check both passed. Runtime evidence was disposable under /tmp/mj-fetch-e2e-artifacts.

## Interfaces and Dependencies

Use existing `FetchedPath`, `SessionListing`, `SubagentToolAction`, `worker_command`, subprocess stdin handling, and `mj_core::config::atomic_write`. Add no crate dependencies. The tool accepts `{session_id: string, path: string, ls?: boolean}` and returns a listing or `{path, kind, source, bytes}`.

Initial plan written after tracing the source and transport limits.

Progress update: completed the implementation and extended existing role/approval contracts and the isolated delegation scenario. Fetch execution is replayable across daemon handoff and does not hold upgrade admission while transferring.

Validation update: the CLI package is named brokk-mjolnir. The first full-test command rejected the incorrect package selector before running tests; the corrected command is running.

Validation update: the complete isolated delegation scenario passed. The only full-suite failure was an existing Claude child allow-list expectation omitting fetch; the corrected test passed on its focused rerun. Temporary copies are cleaned up by the scenario. A concurrent upstream Restart refactor and CLI startup fix were merged without conflicts before publication.

Publication update: pushed 9379c006a after merging the independently validated upstream Restart refactor and CLI startup fix. The first push was rejected because master advanced; fetching and merging resolved it without conflicts. The agent-dev image workflow is https://github.com/BrokkAi/mjolnir/actions/runs/40688502116.

Completion update: final merged-tree cargo clippy --all-targets -- -D warnings and cargo fmt --all -- --check passed. The image publication workflow succeeded, as did CI portable-worker compilation and the three-client reliability smoke. Source changes were pushed to master as 9379c006a; this final plan update records the completed result.
