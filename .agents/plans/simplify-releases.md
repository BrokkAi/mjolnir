# Release known-good commits with parallel builds

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Maintainers can tag a known-good commit without repairing master or repeating passing validation. Independent compilation runs concurrently; archive assembly alone waits for workers. The committed version must still match the tag.

## Progress

- [x] (2026-09-16) Inspect workflows and identify redundant compilation and policy.
- [x] (2026-09-16) Separate Linux compilation from packaging; split macOS compilation into an architecture matrix.
- [x] (2026-09-16) Simplify runbook and agent release policy.
- [x] (2026-09-16) Remove registry recompilation and parallelize independent npm work.
- [x] (2026-09-16) Validate and review; commit this completed checkpoint.

## Surprises & Discoveries

All platform builds waited for both workers although compilation does not consume workers. Registry publication checked the workspace, then rebuilt extracted packages. The release workflow already did not rerun CI; the runbook imposed local repetition.

## Decision Log

- Decision: Trust the maintainer's selection of a known-good commit without adding a CI approval gate.
  Rationale: The user explicitly requests a simpler tag-based release. Existing passing checks count; current master does not matter.
  Date/Author: 2026-09-16, Codex.
- Decision: Keep version checks, archive verification, draft assembly, and registry dependency order.
  Rationale: These concern newly produced artifacts or publication constraints, rather than repeated source validation.
  Date/Author: 2026-09-16, Codex.

## Outcomes & Retrospective

Completed the simpler runbook and aligned agent policy, six independent compilation jobs (including the two macOS matrix instances), archive assembly joins, registry compilation removal, and parallel npm platform publication. No release or push was performed. Actual hosted timing requires a future release. Version preparation remains necessary when the selected commit does not already have the intended version.

## Context and Orientation

`RELEASING.md` is the maintainer runbook; `AGENTS.md` governs agents. `.github/workflows/release.yml` compiles binaries and publishes archives. `.github/workflows/publish.yml` publishes Rust source packages and `.github/workflows/publish-npm.yml` publishes npm binary packages. A job's `needs` lists prerequisite jobs. Artifact downloads remove executable permissions, so archive assembly must restore them.

## Plan of Work

Milestone one replaces the preflight checklist with selecting a known-good commit and pushing a matching tag, with concise version preparation and recovery instructions. Align the agent policy so it does not reintroduce the checklist.

Milestone two separates Linux compilation from archive assembly and makes macOS compilation an architecture matrix. All compilation depends only on version verification. Packaging downloads native binaries and workers; macOS combines architectures using `lipo`. Preserve names, checksums, licenses, and executable modes. Only download archive artifacts in the final release job.

Milestone three removes the registry workspace check and extracted-package rebuilds using `--no-verify`. Keep source package checks and publication order. Parallelize npm platform uploads and availability waits, collecting every subprocess exit status before publishing the root wrapper.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Edit the files above. Run `node --test scripts/linux-release.test.mjs npm/test/*.test.mjs`, lint the changed workflows with actionlint, and run `git diff --check`. Review job dependencies and artifact paths. Stage only changed files and commit on the current branch.

## Validation and Acceptance

Runbook review must show that older known-good commits are eligible and passing checks need not be repeated. Workflow lint and existing installer/npm tests must pass. Both workers, both Linux platforms, and both macOS architectures must compile independently, with packaging waiting on its inputs. Artifact names and binary paths remain compatible with the installer. No Rust source or dependency changes are planned, so Cargo validation is unnecessary. Hosted releases are not exercised locally.

## Idempotence and Recovery

Registry version checks preserve resumable publication. Parallel npm failures must prevent wrapper publication, and all child statuses must be collected. Retry failed workflow jobs on the same commit; never move a published tag. The GitHub release stays a draft until all archives have uploaded.

## Artifacts and Notes

Before: workers precede platform compilation; macOS architectures compile sequentially. After: independent compilation precedes archive-only joins. Validation: actionlint v1.7.7 passed for all four changed workflows (shellcheck was unavailable). All 18 workflow/npm tests and six installer tests passed. `git diff --check` passed. The broader Linux ELF suite had six failures because this Mac has Bash 3 without `mapfile`; the unchanged Linux verifier requires newer Bash. New archive and publication tests are wired into CI.

## Interfaces and Dependencies

Keep checkout v6, upload-artifact v7, download-artifact v8, pinned Rust, and existing Linux build scripts. Preserve external archive/package names. Cargo `--no-verify` packages source without recompiling it.

Revision: Initial plan records inspection and the user's direction to simplify the runbook.

Revision: Completed implementation and recorded validation, including the existing Linux-only test limitation. The new tests execute actual workflow shell with fixture binaries and registry tools, proving concurrent upload ordering and failure propagation.
