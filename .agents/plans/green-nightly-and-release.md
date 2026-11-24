# Restore green CI and publish the next Mjolnir release

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This document must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

The scheduled nightly release has not produced a release because Linux, macOS, and Windows CI have remained red. After this work, the default branch will have successful Linux CI, both platform Nightly lanes will pass on the same revision, and `v2.75.0` will be published through the normal release workflow so users can install the next version from every supported channel.

## Progress

- [x] (2026-11-24 10:15Z) Confirmed that the latest scheduled Nightly failed both platform lanes and skipped its release job; the latest published release remains `v2.74.0`.
- [x] (2026-11-24 10:15Z) Confirmed that Linux CI is currently red on `master` at `c48f1a8fe` across nine jobs.
- [x] Diagnose every current Linux failure from the latest CI logs.
- [x] Fix the Linux failures and validate the affected packages and workflow scripts locally.
- [x] Diagnose the current macOS and Windows failures from the scheduled Nightly logs.
- [x] Fix the platform-specific test fixtures and production path handling identified by those lanes.
- [x] Merge the newer `origin/master` head (`3fe5757ec`) into the local branch.
- [ ] Commit and push the Linux fixes.
- [ ] Dispatch a manual Nightly run on the updated default branch and diagnose the current macOS and Windows failures.
- [ ] Fix the platform failures and validate locally where the host permits; use manual Nightly dispatches for macOS and Windows proof.
- [ ] Reach a fully green push CI and a fully green manual Nightly run on the same revision.
- [ ] Run SlopCop against the complete validated change.
- [ ] Prepare `v2.75.0`, tag the prepared commit, push the tag, and verify publication across all release channels.

## Surprises & Discoveries

- Observation: The Nightly workflow is running on schedule; this is not a disabled or missed schedule.
  Evidence: Scheduled run `45335501851` started at 2026-11-24T05:45:10Z and its macOS and Windows jobs both failed.
- Observation: Nightly releases have not merely missed one night. The twenty scheduled runs queried from November 5 through November 24 were all failures or cancellations.
  Evidence: `gh run list --workflow nightly.yml --event schedule --limit 20` returned no successful runs.
- Observation: Current Linux CI at `c48f1a8fe` fails nine jobs, including formatting, unused dependencies, dependency licenses, controller and daemon tests, worker tests, library tests, and the three-client reliability smoke.
  Evidence: CI run `45361331219` lists those failures at https://github.com/BrokkAi/mjolnir/actions/runs/45361331219.
- Observation: Two Linux failures exposed production defects rather than only stale expectations.
  Evidence: sub-agent startup request IDs derived from colon-qualified request keys violated relay command-ID rules, and a failed post-acceptance close left a parked child permanently marked `Closing`. Both now have owning-code fixes and regression tests.
- Observation: The macOS and Windows logs identify reusable platform assumptions, not one shared release failure.
  Evidence: macOS tests assumed local Podman, a PATH-provided sleep, and Linux-only target defaults; Windows tests assumed Unix local-bare paths and POSIX absolute-path parsing for imported OpenCode working directories.
- Observation: The local host has Git 2.43, whose `git stash` refuses SHA-256 repositories.
  Evidence: a minimal `git init --object-format=sha256` repository reproduces `Cannot save the current status`; the same tests pass in project CI with a newer Git.

## Decision Log

- Decision: Treat "fix everything" as making Linux push CI and both platform Nightly lanes green before cutting the requested release.
  Rationale: A release tag runs the complete suite and will not publish while any lane is red. Fixing only one nightly test would leave the release blocked.
  Date/Author: 2026-11-24 / Codex
- Decision: Fix and publish Linux blockers before dispatching platform CI.
  Rationale: A manual Nightly dispatch tests the default branch. Landing a green Linux revision first prevents wasting scarce macOS runner minutes on a revision that still cannot pass the complete release gate.
  Date/Author: 2026-11-24 / Codex
- Decision: Parse imported native working directories as either controller-host or target absolute paths.
  Rationale: A Windows controller can import native history recorded on a POSIX target. Checking only `Path::is_absolute` rejected valid `/work/...` paths before import.
  Date/Author: 2026-11-24 / Codex

## Outcomes & Retrospective

Work is in progress. The initial diagnosis established that the release gate is behaving as designed and that all three platform families need repair.

## Context and Orientation

Mjolnir is a Rust workspace with a CLI named `mj`, a daemon, workers, a controller, storage crates, a TUI, and supporting workflow scripts. Linux CI runs on every push and is defined by `.github/workflows/ci.yml`. The shared macOS and Windows definitions are `.github/workflows/macos.yml` and `.github/workflows/windows.yml`; `.github/workflows/nightly.yml` runs them at 05:41 UTC, maintains one tracking issue per platform, and releases a scheduled revision only when both lanes and the revision's push CI are green. The release workflow is `.github/workflows/release.yml`, and the manual version/tag procedure is documented in `RELEASING.md`.

"Green" means the workflow completed with the GitHub Actions conclusion `success`; a skipped release job is not green because one of its required jobs failed. "Nightly" is the scheduled GitHub Actions workflow, not a locally running daemon. "Release commit" is the isolated commit created by `cargo release minor --workspace --execute --no-confirm`. The tag points exactly at that release commit.

## Plan of Work

First, collect the latest failed Linux job logs and map each failure to the smallest ownership boundary: generated formatting, duplicate or unused dependencies, license policy, controller lifecycle state, daemon recovery state, worker environment, or reliability timing. Reproduce each distinct failure on the local Linux host in a named Mjolnir test instance where the existing tests require one. Make root-cause fixes in the owning modules, preserving existing behavior and adding only tests allowed by the repository testing guidelines.

Second, validate the affected packages with focused tests and Clippy checks, run the workflow-script checks when workflow scripts change, then commit only the changed files and push directly to `origin/master`. Wait for Linux push CI on that commit and address any remaining failures before dispatching platform work.

Third, manually dispatch `.github/workflows/nightly.yml` on `master`. Read the raw macOS and Windows job logs, classify each failed test as a real platform defect or a test-only determinism problem, and patch the owning production or test-support code. Repeat focused Linux validation and a manual Nightly dispatch after each logically coherent batch. Platform-specific behavior must be tested in the actual platform lane rather than inferred from Linux.

Fourth, once push CI and both Nightly lanes are green on the same revision, run SlopCop over the complete diff. Prepare the next minor release through the documented `cargo release minor --workspace --execute --no-confirm` path, tag that prepared commit, push the tag, and monitor `release.yml` through GitHub, crates.io, npm, OCI, and Discord status. Publication is complete only when the GitHub release is no longer a draft and each package channel reports the new version.

## Concrete Steps

All commands run from `/workspace/12369d525a50601d24026badd08b437a/mjolnir`.

Inspect the latest push CI and raw failed-job logs:

    gh run view 45361331219 --json status,conclusion,event,headSha,jobs
    gh api repos/BrokkAi/mjolnir/actions/jobs/<job-id>/logs > /tmp/<job-name>.log

Reproduce a failing package test with focused output:

    cargo test -p <package> <test-filter>

Run the required behavior checks for an affected Rust package:

    cargo clippy -p <package> --all-targets -- -D warnings

After committing and pushing, require the push CI for `HEAD` to report:

    status completed
    conclusion success

Dispatch platform proof:

    gh workflow run nightly.yml --ref master

The run must show both the macOS and Windows reusable-workflow jobs with conclusion `success`.

Prepare and publish the requested release only after that proof:

    cargo install --locked cargo-release --version 1.1.5
    cargo release minor --workspace --execute --no-confirm
    git tag v2.75.0 <prepared-commit>
    git push origin v2.75.0

## Validation and Acceptance

Acceptance requires observable GitHub evidence, not only local compilation. The Linux push workflow for the release source must complete successfully. A manual Nightly dispatch on the same revision must have successful macOS and Windows jobs. SlopCop must approve the complete change before the final release preparation is pushed. The release workflow must publish GitHub release `v2.75.0` and complete crates.io, npm, daemon-image, and announcement jobs; registry jobs may skip an already-published version during recovery, but every channel must ultimately expose the release.

## Idempotence and Recovery

CI diagnosis and read-only API calls are safe to repeat. Focused test runs use isolated instances or their existing `MJ_CONFIG_DIR` and `MJ_DATA_DIR` fixtures. If a push is rejected, merge `origin/master` into the current branch and push the merge commit as required by repository policy rather than rebasing. If a release run fails before publication, fix on top of the release commit, delete only the unpublished `v2.75.0` tag, and tag the fixed commit with the same version. If publication is partial, rerun the failed release jobs; the workflow skips already-published package versions.

## Artifacts and Notes

The latest scheduled Nightly:

    run: 45335501851
    commit: 3cee15c4c984ea1e1cfac2870ddfc6be723d4a27
    macOS: failure
    Windows: failure
    Release master: skipped

The latest Linux push CI before implementation:

    run: 45361331219
    commit: c48f1a8fe5f12404930dddf46b8be2991493dc03
    failed jobs: 9

## Interfaces and Dependencies

No new public API is planned at the start of this work. Existing module boundaries and command behavior must remain intact unless a failing test demonstrates that their current contract is wrong. GitHub Actions remains the source of truth for platform behavior. `cargo-release` version `1.1.5` prepares the workspace version, internal dependency constraints, and `Cargo.lock` together. The configured GitHub token remains the publication credential; no new release authorization audit is needed because the user explicitly requested a release.

Revision note (2026-11-24): Initial plan created after confirming the failed scheduled Nightly and current Linux CI.
