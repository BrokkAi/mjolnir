# Isolate container Git without adding a workflow


This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises &
Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture


Container sessions must never push changes into the host checkout. Local history
and dirty files seed each session independently. Normal Git pushes publish an
automatically created `mj/<full-session-id>` branch to the real network upstream,
such as GitHub. No import UI or new user configuration is introduced.

## Progress


- [x] (2026-09-08) Restarted from clean cd6262f7 in the user-requested detached
  worktree `.mjolnir/worktrees/git-session-isolation`.
- [x] (2026-09-08) Denied receive-pack at the host and worker; added policy/readiness checks and tested bounded legacy broker retirement.
- [x] (2026-09-08) Separated mj-source from network origin and added automatic branches; 29 provisioning tests pass.
- [x] (2026-09-08) Added source baseline and archive branch marker; new bridge and checkpoint regression tests pass.
- [x] (2026-09-08) Added real Git push/checkpoint tests and updated documentation.
- [x] (2026-09-08) Verified broker preparation on live reconnect, orphan adoption,
  and quiet worker upgrades; existing bare-project behavior remains outside the
  managed workspace policy.
- [x] (2026-09-08) Completed formatting, elevated full tests and all-target Clippy;
  prepared this isolated worktree for its implementation commit.
- [x] (2026-09-08) Committed the implementation as 61930b8c in the detached worktree.
- [x] (2026-09-08) User authorized pushing to master. Merged upstream 14b99cb3
  into this worktree without conflicts.
- [x] (2026-09-08) Validated the combined result: 2,759 tests passed, 16 ignored,
  no failures; formatting and all-target Clippy passed.
- [x] (2026-09-08) User changed delivery to a pull request before any master push.
  Prepared the integration commit for a PR branch targeting master.

## Surprises & Discoveries


The original bridge passes `receive.denyCurrentBranch=updateInstead`. A push can
change a clean host checkout; detached HEAD during a rebase also leaves master
unprotected. Brokers are supervised, so killing one process alone can restart
the same writable service. Checkpoints exclude all origin refs, which must be
decoupled from the new publishing destination.

Source archives can carry another session's branch-policy setting, so setup only
treats a marker matching its own full session ID as an initialized workspace.
Legacy bare orphan sessions may lack a bundle altogether; the existing bare
target classification must exclude them before resolving managed repositories.

The complete test run produced 2,756 passes, 16 ignored tests, and one unrelated
failure in `render::tests::minimized_sessions_truncate_only_the_top_line`.
An untouched archive of base cd6262f7 reproduced the identical narrow-layout
failure. Master independently advanced to 14b99cb3 with a fix for that test during
the initial validation. The subsequent authorized integration includes that fix
and passes the entire suite.

## Decision Log


- Decision: Use a read-only `mj-source` remote for host history and origin for
  the real upstream. Rationale: user chose automatic ordinary Git workflows,
  without import steps. Date: 2026-09-08.
- Decision: Deny receive-pack on the host, not only in target configuration.
  Rationale: explicit pushes and old workers must not bypass isolation.
- Decision: Preserve a branch-policy marker in archive metadata. Rationale:
  restores must distinguish old inherited branches from intentional user changes.
- Decision: Work and commit only in this isolated worktree. Rationale: the user
  explicitly rejected implementation in master and requested a fresh start.
- Decision: Code only, no deployment, running-session changes, history repair,
  push, or integration into master. Rationale: explicit user-selected scope.
- Decision: Integrate and push to master after the implementation commit.
  Rationale: the user subsequently requested "push it to master". Resolve and
  validate integration in the existing worktree, then fast-forward master and
  push its configured upstream. No release or running-session migration is requested.
- Decision: Publish a PR branch targeting master instead of pushing master.
  Rationale: the user's latest instruction is "ok make a pr". This supersedes
  the direct master push; the implementation and validation remain in this worktree.

## Outcomes & Retrospective


The read-only source bridge, upstream publishing configuration, automatic session
branches, source-based checkpoints, and backward-compatible migration are
implemented in the isolated worktree. Real Git tests prove distinct ordinary
pushes and preservation of committed, staged, unstaged, and untracked work.
Formatting and all-target Clippy pass. After integrating current master, the full
test run passes with 2,759 passed tests, no failures, and 16 ignored tests.

The implementation was committed as 61930b8c in this detached worktree. The user
then authorized integration against upstream 14b99cb3, which includes the
independent TUI fix. Integration validation is complete. The user subsequently
requested a pull request, so the validated result will be published on a PR branch
targeting master. Master has not been changed or pushed by this task.
No real session was stopped or migrated. Running binaries are not protected
merely by source edits; an updated controller must enforce the new policy.

## Context and Orientation


`src/hel_git_proxy.rs` serves Git fetch/push commands over a framed stream between
a host broker and target worker. `mj-controller/src/hel_controller/provisioning.rs`
starts and supervises brokers, fetches history, seeds files and starts sessions.
`resume.rs` restores archives before the agent resumes. `src/hel_checkpoint.rs`
selects deltas, and `src/hel_archive/git.rs` collects/restores history and files.
Existing managed bare-worktree behavior is outside this container change.

## Plan of Work


### Milestone 1: Make the host source read-only


First restrict Git services to upload-pack and add a broker policy version. Old
specifications may be inspected for migration, but cannot run in new brokers;
old executables must reject new specifications. Use bounded supervised retirement
for replacements, and do not reuse a broker with incompatible policy.

The host broker is the controller process that carries Git traffic to the local
repository. In `src/hel_git_proxy.rs`, `git_service` must reject receive-pack,
Git's push service, before any Git process starts. Keep upload-pack, Git's fetch
service, available. `run_broker` validates the specification version, and the
worker proxy performs the same early push rejection. The real protocol tests
exercise more than 64 KiB of traffic and verify source refs and files stay intact.

### Milestone 2: Separate publishing and preserve session work


Generalize checkpoint baselines to a named remote, with absent values meaning
origin. Local repositories use mj-source. Preserve a branch-policy marker in
archive metadata with a default for old archives. Seed/restoration precedes
branch setup; create the managed branch once and preserve subsequent user changes.
Reject migration during an active Git operation rather than rewriting it.

In `src/hel_checkpoint.rs`, `CheckpointRepositorySpec.baseline_remote` chooses
which remote supplies the history excluded from a checkpoint. Set it to mj-source
for local bundles in `mj-controller/src/hel_controller/checkpoint.rs`.
`GitHistoryMode::SessionDeltaFromRemote` in `src/hel_archive/git.rs` excludes
only that source's remote-tracking refs, so publishing a commit does not remove
it from the checkpoint. `RepositoryMetadata.session_branch` preserves the
initial branch marker through archive collection and restoration.

Separate source configuration and network upstream discovery. Preserve supported
fetch/push URL semantics, reuse GitHub normalization and auth, strip credentials,
and reject local/file/helper transports as publishing destinations. Missing
upstreams leave local work usable without a writable host fallback. Override
inherited unsafe push defaults in managed checkouts.

Create `mj-controller/src/hel_controller/provisioning/git.rs` to own these Git
operations. `WorkspaceGit::connect_source` configures mj-source and network
origin. `WorkspaceGit::initialize_branch` creates `mj/<full-session-id>`
without tracking a source branch and sets current-branch push defaults. Repeated
setup preserves a user-selected branch when the session marker already matches.
The two-session test publishes to a temporary upstream and restores a checkpoint
after publication while checking that host and upstream main remain unchanged.

### Milestone 3: Enforce the policy on existing connections


In `mj-controller/src/hel_controller/provisioning.rs`, prepare the versioned
broker before local source access. Its readiness file acknowledges the read-only
policy. Retire an old broker using the existing bounded process-group teardown
before replacing its specification. A stale supervisor cannot restart the old
writable service with the new specification.

Carry a pure broker specification in `RelaySessionTarget`, populated by
`mj-cli/src/pollers.rs`. In `mj-controller/src/hel_session_manager.rs`, prepare
it with `spawn_blocking` before connecting, keeping process and filesystem work
off UI loops. Apply the same gate during orphan adoption in
`mj-controller/src/hel_controller/recovery_scan.rs`. Configure repository
remotes and branches after archive restoration in `resume.rs` and while a worker
is stopped during replacement in `worker_restart.rs`. Raise checkpoint
capability versions to 2 so old workers are upgraded, while new readers accept
version 1 specifications.

## Concrete Steps


Run all edits and checks in
`/home/ryan/code/mjolnir/.mjolnir/worktrees/git-session-isolation`. Run focused
tests while implementing, then `cargo fmt --all -- --check`, elevated
`cargo test`, and `cargo clippy --all-targets -- -D warnings`. Do not redirect
build storage into /tmp. Review the diff and stage only task-owned paths. Commit
in this detached worktree and report the commit; do not move master.

The final full command was `cargo test --no-fail-fast -- --quiet`, run outside
the restricted sandbox. Its results were:

    chat:       444 passed, 1 ignored
    controller: 731 passed, 1 ignored
    core:       873 passed, 6 ignored
    TUI:        375 passed, 1 failed, 2 ignored
    worker:     109 passed, 2 ignored; binary and proxy tests: 5 passed
    CLI:        210 passed; integration tests: 9 passed, 4 ignored
    Clippy:     cargo clippy --all-targets -- -D warnings passed

To reproduce the baseline TUI failure without editing a checkout, unpack
`git archive cd6262f7` into `target/baseline-git-isolation` and run, from the
worktree directory:

    CARGO_TARGET_DIR=/home/ryan/code/mjolnir/.mjolnir/worktrees/git-session-isolation/target cargo test --manifest-path target/baseline-git-isolation/Cargo.toml -p brokk-mj-tui --lib render::tests::minimized_sessions_truncate_only_the_top_line -- --exact --nocapture

The narrow session summary is truncated to `› · ACP pretty na ⋯ ` on both
the original base and this worktree. No unrelated TUI code is part of this fix.

## Validation and Acceptance


Real temporary repositories must prove clone/fetch success and rejection of
normal, explicit master, forced, deletion and tag pushes. Compare host refs,
index and files with clean, dirty, detached and rebasing source states. Rejected
exchanges must not break later fetches. Use more than 64 KiB through bounded pipes.
Two sessions must publish different branches to a separate upstream without
changing host refs or upstream main. Checkpoints must restore unpublished and
published commits, dirty files, managed/user-selected branches and legacy archives.
Missing refs repair from the chosen baseline. Broker policy tests cover legacy
rejection, replacement and stale supervisor behavior.

## Idempotence and Recovery


Repeated setup must not reset history or dirty files. Failed broker replacement
must close write access or fail connection, never reuse unsafe service. Existing
real sessions and both host refs and files are outside implementation mutations.
Tests use fixture repositories. Keep the worktree and committed result available.

## Artifacts and Notes


The original source uses `receive.denyCurrentBranch=updateInstead`; the original
target origin is `ext::<worker>/hel worker git-proxy ... %S`. Source fetching
retains the latter transport with no receive capability.

## Interfaces and Dependencies


Add an internal broker policy version, optional checkpoint baseline remote, and
optional archive branch-policy marker. Preserve old default semantics when fields
are absent. Reuse existing command/executor and subprocess helpers; add no crate
or dependency. Update local-repository documentation with the observable behavior.

Initial fresh-worktree plan recorded 2026-09-08.

Revision 2026-09-08: live-session reconnection also needs a policy gate, not only
new-session provisioning. Relay targets now carry a pure broker specification;
connection tasks prepare it in supervised blocking work before contacting the
worker. Quiet worker upgrades reconfigure source/origin and branch policy while
the worker is stopped. This prevents reused legacy sessions from retaining a
writable broker or trying to checkpoint against a missing mj-source remote.
Checkpoint protocol capability versions are raised to 2, while new readers still
accept version 1 specifications. The initial core run exposed three stale test
expectations and one unrelated executable-busy test race; expectations were
updated and the full required suite will validate the result.

Revision 2026-09-08: recorded completed validation, the independent baseline test
failure, and final compatibility details. New Git test process execution uses
the shared subprocess helper. Remote path text is normalized only when constructing
target command arguments, preserving native Path and PathBuf handling internally.

Revision 2026-09-08: the user explicitly authorized pushing the finished fix to
master. Keep integration work in the existing detached worktree, validate the
merged source, then publish by a normal fast-forward push. This supersedes the
earlier restriction against integration and pushing.

Revision 2026-09-08: recorded successful integration validation and the user's
subsequent change to PR delivery. Publish a dedicated branch and open its pull
request against master; do not push master directly.
