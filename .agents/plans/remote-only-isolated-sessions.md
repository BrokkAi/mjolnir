# Require network remotes for isolated sessions

This living ExecPlan follows `.agents/PLANS.md`. Update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture

An isolated session must behave like an ordinary independent Git checkout. Starting from a local directory resolves its configured default network fetch and push destinations, clones the fetch remote's default branch, and creates `mj/<session-id>`. Local unpublished commits and dirty files do not enter that clone. The session never has a Git connection to the host checkout. Closing saves a checkpoint, not a branch in the host repository. Only raw local execution supports repositories without network remotes.

## Progress

- [x] (2026-09-08) Inspected creation, bridge, archive, resume, and wizard paths; user selected remote requirement, default branch only, split fetch/push preservation, and no legacy bridge resume.
- [x] (2026-09-08) Delegated shared resolver, checkpoint preservation, and creation surfaces to independently owned Luna tasks.
- [x] (2026-09-08) Implemented remote-only provisioning and removed host bridge lifecycle/protocol entrypoints.
- [x] (2026-09-08) Integrated creation previews and policy validation across surfaces; retry and cancellation are covered in TUI and browser tests.
- [x] (2026-09-08) Completed full-suite execution, corrected the retry test fixture, passed the affected reruns, format and diff checks, and warning-free clippy.
- [x] (2026-09-08) Integrated origin/master at d324aaa0 on hel4 and staged the reviewed implementation for the authorized commit and push.

## Surprises & Discoveries

The old `local` bundle member unconditionally replaced the target origin with a writable host bridge. Single-directory quick launches synthesize such bundles. Bundle membership is not a publishing policy.

The old checkpoint SessionDelta excludes origin remote-tracking refs. A push to a different repository can move those refs even when the fetch repository cannot supply the published commits. New managed clones record an immutable starting commit and checkpoint against that commit instead.

Raw local primary checkouts already create separate managed session worktrees. The existing dirty-source acknowledgment is also bypassed by quick launch, which explicitly assumes copying current contents. That assumption must be removed from isolated creation.

The final review found that explicit workspace-to-raw moves restore into a linked worktree, whose local Git configuration is shared with the host checkout. Raw branch restoration now skips managed network configuration, and explicit raw moves still check their host source prerequisites. A real Git test verifies preservation of origin, push URLs, push defaults, and absence of managed markers.

Git HTTPS does not consume GH_TOKEN by itself. The default-branch probe now adds a command-scoped GitHub credential helper using the existing environment token or GitHub CLI login. A real `git credential fill` check with a fixture token confirmed host scoping without writing Git config.

## Decision Log

On 2026-09-08 the user chose requiring real remotes over a remote-less clone with publication on close. No hidden local fallback is permitted. Failures do not change destinations or execution targets.

The user chose always using the remote default branch, removing configured git_ref branch/tag overrides, and preserving different Git default fetch and push destinations. Existing obsolete configuration must fail with actionable guidance rather than be ignored.

Legacy host-bridge sessions will not resume; no migration is required. Ordinary resume for new remote-backed sessions preserves their checkpoint state instead of restarting from the default branch.

Target clones normalize selected destinations into origin's fetch URL and explicit push URLs. Remote names on the host are not semantically significant. Git's effective destination choices, including explicit push URL overrides, are preserved.

New managed clones record mj.remoteWorkspace=true and mj.baseCommit=<initial commit>. Archive metadata carries the managed marker and push URLs, and restore reinstates them. This avoids a new session-database schema while preserving network destinations independently of host configuration on resume.

User additionally authorized merge and push on completion. Work remains on the current hel4 branch and pushes to its upstream origin/master after validation and integration.

An isolated resume needs a managed archive with network provenance. Raw archives without that provenance cannot be converted into isolated workspaces by guessing a source or reconstructing host state; they fail before any session or configuration mutation. Explicit network-to-raw moves remain supported with source prerequisite checks and host configuration preserved.

Upstream advanced to d324aaa0 (utility-model selection). The authorized merge was a clean fast-forward on hel4, preserving all implementation edits; final validation includes that commit.

## Outcomes & Retrospective

Implementation and upstream integration are complete. Isolated clones require network remotes, start from the advertised default branch, use independent session branches, and never serve or publish into the host clone. Checkpoint restore uses archived network provenance and preserves committed and dirty session work. Raw local no-remote sessions remain supported; unsupported raw-to-isolated moves fail in the target picker before a live source is interrupted. Explicit isolated-to-raw moves preserve host Git configuration.

Validation evidence:

- Full `cargo test --no-fail-fast -- --quiet` execution passed chat (446), controller (725), core (874), worker library (109), worker CLI (4), CLI (210), and the import, logging, persistence, and worker-exit integration tests. One new TUI retry test had an incorrect hard-coded profile expectation; it now compares with the actual original request, and both isolated-creation tests pass on rerun. Other TUI tests passed (377).
- The five-second PTY deadline was exceeded on the saturated shared host with `TOKIO_WORKER_THREADS=2`. All five PTY tests passed serially with `TOKIO_WORKER_THREADS=4`, without changing production code or relaxing deadlines.
- `cargo test raw -- --quiet` passed all 67 selected tests after the final raw-move compatibility restriction.
- `cargo clippy --all-targets -- -D warnings` passed after the final test correction. `cargo fmt --all -- --check` and `git diff --cached --check` passed.
- `MJ_BROWSER_SPEC=new-session.spec.js npm --prefix tests/e2e/web test` passed 22 JavaScript unit tests and seven browser tests.
- A real Git credential-helper probe with a fixture token verified GitHub authentication and host scoping without network access or host config changes.

The reviewed implementation is ready for the authorized commit and push on hel4 to origin/master. Git publication receipts are reported with task completion.


## Context and Orientation

`src/hel_remote_git.rs` will resolve network destinations using shared subprocess executors, allowing callers to supply cancellation and deadlines. `mj-controller/src/hel_controller/backend.rs` produces clone specifications; `provisioning.rs` currently starts the bridge and seeds local snapshots. `resume.rs` reconstructs targets and restores archives; it must use saved network provenance rather than reconnect to a host. `src/hel_archive/git.rs` and `src/hel_checkpoint.rs` collect and restore committed/dirty work. The TUI wizard and phone API have supervised preflight paths in `mj-cli/src/dashboard/io.rs` and `mj-cli/src/server.rs`.

## Plan of Work

First implement shared NetworkGitSource resolution and default HEAD probing with behavior tests. Resolve configuration without network I/O; make the default-branch probe an explicit operation. Reject local paths, file URLs, ext/helper transports, missing or ambiguous defaults. Honor Git's branch fetch/push settings, global remote.pushDefault, explicit pushurl values, and URL rewrites. URLs shown to users or persisted in archives must not contain secret credentials.

Next remove host bridge server/proxy commands and supervision. All bundle repositories produce network clone specs. Fresh managed clones start from origin/HEAD and immediately get their own branch, safe push defaults, explicit push destinations, and immutable checkpoint base. Validate policy before target creation. Raw local behavior stays intact; remote bare sessions also require network remotes.

The checkpoint milestone stores managed metadata and computes deltas against the launch base. Restore retains the session's selected branch and dirty state and reinstates network/push configuration. Resume rejects legacy local-bridge provenance before provisioning. Reconstruct network clone inputs from archives for new resumed sessions so host sources are not required.

The creation milestone replaces isolated dirty acknowledgment with a background remote preview showing source, initial branch, destinations, and exclusion of local changes. Apply the same resolver in controller creation and API paths. The UI event loop must perform no filesystem scans or Git/network calls. Independent operations remain concurrent and cancellation stays responsive.

Finally update human-facing configuration/workspace documentation and startup context, review all edits, run required validations, and commit. Integrate upstream on the current branch with a merge if necessary, rerun affected validations, then push HEAD to upstream master.

## Concrete Steps

Run commands from `/home/jonathan/Projects/hel4`. Read source with `rg --threads 1` if the host has insufficient thread capacity. Do not redirect Cargo output storage to /tmp. Coordinate build invocations across agents. Every cargo test command requires elevated permissions because tests open local sockets.

Required final commands are `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. Stage only task-owned files and commit on the current branch. Fetch origin with elevated permissions, inspect divergence, merge origin/master if needed, validate the resulting commit, and push to the upstream branch. Do not merge PR 981's obsolete bridge design.

## Validation and Acceptance

Real Git fixtures must show a host with another checked-out branch, unpublished commits, staged/unstaged/untracked data producing a clean remote-default session without changing the host. Two sessions must push different branches without changing upstream default HEAD. Test non-origin defaults, distinct fetch/push repositories, multiple explicit push URLs, invalid defaults, missing remotes, local URLs, URL rewriting, old git_ref configuration, and mixed bundles. Failures must not fallback locally.

Checkpoint tests must prove restore before and after publication with split remotes, retain dirty files, and allow another checkpoint after restore. Use more than 64 KiB in fixtures that exercise streaming. Legacy bridge sessions fail resume clearly. Raw local no-remote sessions continue to work. Creation preview and input transition tests must demonstrate consistent TUI/API/quick-launch policy and background execution.

## Idempotence and Recovery

Do not reset or overwrite existing session branches during reconnect or resume. Fresh-target initialization may run once and must preserve an existing matching managed marker on retries. Reject incompatible existing checkouts instead of replacing their state. Failed creation follows existing bounded process teardown and cleanup. Failed checkpoints preserve the target. Unsupported legacy archives remain on disk.

## Interfaces and Dependencies

NetworkGitSource has fetch_url:String and push_urls:Vec<String>. Resolver functions accept ProjectRepository or a local Path plus a shared CommandExecutor. default_branch accepts a resolved source and executor, and returns the advertised branch name. Provisioning normalizes these destinations into origin. Archive RepositoryMetadata gains defaulted managed marker and push_urls fields for explicit new-session semantics. The checkpoint capture enum gains a remote-workspace mode that reads and validates the immutable starting commit. No new crates or external dependencies are required.

## Artifacts and Notes

This plan replaces PR 981's host-bridge approach. Branch at implementation start is hel4, upstream origin/master, starting HEAD 14b99cb3679de7b18121ce022169e1052cac1a54. Working tree was clean. Agent ownership is disjoint: resolver source files, archive/checkpoint files, creation UI files; root owns provisioning, lifecycle, protocol removal, integration, and final review.

Revision note: Initial implementation plan records the accepted policy and explicit merge/push authorization.

Revision note: Implementation milestones completed; recorded browser and focused validation evidence. Full Rust compilation exposed stale test imports after bridge removal, which were corrected.

Revision note: Recorded final integration review, safe explicit raw moves, private HTTPS authentication, updated test counts, and upstream fast-forward.

Revision note: A final lifecycle audit found raw-to-isolated moves must be rejected by the shared target-compatibility check, not merely by restore, so a live raw source is never stopped for an unsupported destination. Updated the compatibility behavior test and removed stale host-bridge guidance from targets, security, and troubleshooting documentation.

Revision note: Closed implementation and validation milestones with exact suite results, corrected-test reruns, and the PTY environment constraint.
