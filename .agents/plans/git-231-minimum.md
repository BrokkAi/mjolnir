# Require Git 2.31 in doctor and retire Git 2.25 workarounds

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture

`mj doctor` will diagnose Git older than 2.31 on the controller and configured
SSH bare targets before unsupported commands break workspace operations. With
that supported minimum, repository discovery, checkpoint capture and workspace
moves can ask Git for absolute paths directly. Managed-clone fetches can preserve
FETCH_HEAD and exclude the symbolic origin/HEAD without copying and deleting it.

## Progress

- [x] (2026-10-28) Inspect doctor, Git helpers and existing compatibility tests.
- [x] (2026-10-28) Verify Git 2.31 supports absolute path output, negative
  refspecs and --no-write-fetch-head, but not worktree list -z.
- [x] (2026-10-28) Implement doctor reporting and extend the existing golden
  scenario and checked-in output.
- [x] (2026-10-28) Remove obsolete path and fetch workarounds; retain branch-list parsing
  required by Git 2.31 and update its external-tool test fixtures.
- [x] (2026-10-28) Run the touched crate suites: checkpoint 166 tests, core
  551 tests, worker 718 tests, and controller 2,186 tests. The controller's
  obsolete four-command assertion was removed and that test passed on rerun.
- [x] (2026-10-28) Pass workspace Clippy with warnings denied and formatting.
- [x] (2026-10-28) Verify controller doc tests and rerun the full doctor test
  group after enforcing strict host-key checks on the SSH Git probe.

## Surprises & Discoveries

Git 2.31 still lacks `git worktree list -z`. Both production branch-occupancy
checks must retain their newline-delimited porcelain parsing. Git ref names
cannot contain newlines, so that parsing remains sufficient for these checks.
The upstream v2.31.0 Documentation/git-worktree.txt lists only --porcelain.

## Decision Log

Keep general path validation and normalization: relative core.hooksPath values
and cross-platform protocol paths still need them. Replace rev-parse calls that
manually reconstruct absolute paths with --path-format=absolute and remove
Git 2.25 emulation from the tests. Preserve regression scenarios using real Git
or fixtures restricted only by the supported 2.31 surface.

Doctor uses its existing bounded executor and reports missing, failed or
unparseable version probes as fixable. No session-admission behavior changes.
Its SSH version probe uses the original target's probe lease and prepends
StrictHostKeyChecking=yes so a configured accept-new policy cannot make a
diagnostic run trust a host key.

## Outcomes & Retrospective

Doctor reports Git older than 2.31 as fixable, including on configured SSH
bare targets. Its existing golden covers the version boundary, platform
suffixes, missing or malformed Git, and remote output. Obsolete relative-path
reconstruction and fetch workarounds are removed; branch-list parsing still
supports Git 2.31. The original 2185/2186 controller pass became a complete
pass after removing its obsolete assertion about the extra path-resolution
command and rerunning the affected test. Core and worker executable integration
tests and their doc tests passed as well. Regression scenarios verify source
FETCH_HEAD is unchanged and copying cached refs does not create clone FETCH_HEAD.
Publication follows the repository's normal commit and push instructions below.

## Context and Orientation

`mj-controller/src/doctor.rs` supplies human and JSON doctor checks and also the
setup summary. Its tests and existing golden live in the sibling doctor/tests.rs
and mj-controller/tests/golden/doctor-external-runtime-checks.txt. Git path calls
are spread across mj-core repository discovery, mj-checkpoint archive capture,
mj-controller managed worktrees and mj-worker move capture. Existing Git225
test adapters emulate unsupported flags; these must stop rejecting features
available in the new minimum.

## Plan of Work

Add local and SSH bare Git checks using git --version, accepting ordinary Git,
Apple Git and Git for Windows version suffixes. Render observed and required
versions and provide upgrade guidance. Extend the existing doctor golden rather
than adding parser unit tests. Replace relative rev-parse path reconstruction
with absolute Git output. Restore --no-write-fetch-head and a negative refspec
excluding refs/remotes/origin/HEAD in cached-origin copying. Simplify its
preparatory ref inspection. Run existing linked-worktree, ignored/staged file,
operation-state and cached-origin preservation regressions.

## Concrete Steps

From the repository root, use normal mbx-backed Cargo commands. Run targeted
tests during edits. After the final edits, run cargo fmt --all -- --check,
cargo test -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-controller
-p brokk-mj-worker, and cargo clippy --all-targets -- -D warnings. All tests
retain their existing isolated data directories. Any manual CLI invocation
uses --instance git-231-doctor. Stage only changed files, commit on the current
branch and push HEAD:master; merge origin/master if upstream moved.

## Validation and Acceptance

Doctor's full rendered golden must show old Git as fixable and 2.31, Apple Git
and Windows Git as ready, and include diagnostic output for unavailable Git.
Existing repository, checkpoint and worker suites must pass after the cleanup.
Fetch regressions must verify cached refs and objects survive and FETCH_HEAD
is preserved. No unsupported worktree -z flag may be introduced.

## Idempotence and Recovery

Changes are source and test fixtures only. Test repositories and stores are
temporary and isolated. Do not upgrade installed Git, touch live sessions or
redirect Cargo output. Failed validation can be retried on the failing test.

## Artifacts and Notes

Upstream Git v2.31.0 documentation was inspected directly for rev-parse,
fetch-options and worktree option support. Command logs belong in /tmp.

## Interfaces and Dependencies

Reuse DoctorCheck, CommandExecutor, CommandSpec and ssh_validation_command.
Use the existing subprocess/executor boundaries; add no crate dependency.

Plan created after the user expanded the doctor change to remove obsolete Git
2.25 compatibility code.

Implementation update: absolute Git path queries replaced relative-directory
reconstruction, Git 2.25 adapters were narrowed to the remaining Git 2.31
worktree-list limitation, and cached-ref copying now uses a negative refspec.

Validation update: all touched library suites passed, with one existing
command-count fixture adjusted after the removed Git query. Master advanced
during validation; integrate those concurrent changes using a merge.

Final review update: the SSH probe now follows the connectivity probe's strict
host-key policy. The complete doctor group and final workspace Clippy passed
after this change; controller doc tests and formatting also passed.
