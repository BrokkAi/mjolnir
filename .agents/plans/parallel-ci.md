# Parallel CI with refreshed build caches

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Shorten the wait for CI without removing validation. Release compilation, portable-worker validation, desktop validation, and CLI lint/tests should run on separate runners. Cache compiled dependencies and outputs in all build jobs so subsequent commits can reuse work.

## Progress

- [x] (2026-09-16) Inspect the workflow and job timings.
- [x] (2026-09-16) Split independent jobs and add scoped caches.
- [x] (2026-09-16) YAML parsing, actionlint, command-coverage review, and 19 workflow-related tests passed (tests in the existing Linux development container).
- [x] (2026-09-16) Complete implementation checkpoint for commit; remote runs remain pending push authorization.

## Surprises & Discoveries

Compatibility builds have no cache and took seven to eight minutes compiling. The primary cache takes one to two minutes to restore and is immutable until Cargo.lock or Rust changes. License-tool installation took 94 seconds. These observations came from CI run 35114039820; its primary matrix failed at formatting, so it cannot establish complete baseline matrix timings.

The macOS system Bash lacks mapfile, which the existing Linux ELF verifier needs. Its tests passed in the existing Linux development image with the repository mounted read-only and networking disabled; no verifier change was necessary.

## Decision Log

Keep Clippy and dev-profile tests together, but separate release builds and portable-worker builds. Separate desktop validation from CLI validation on macOS and Windows. Keep every existing command and platform restriction. A small local composite action centralizes cache policy. Cache namespaces include lane, OS, architecture, configuration, dependencies, and commit; fallback restores stay in the same lane and configuration. Cache the pinned cargo-deny executable separately. Cancel superseded runs on the same event/ref without affecting release workflows.

## Context and Orientation

`.github/workflows/ci.yml` defines all checks. Its `mjolnir` matrix runs lint, release builds, and dev-profile tests sequentially. Other jobs cover packaging/licenses, Linux desktop, voice, reliability, and old-Linux compatibility. `scripts/build-linux-cli.sh` uses `target/release-cli`; portable workers use `target/worker`. Reliability needs both controller and worker binaries before it starts; leave those builds together to avoid artifact handoffs.

## Plan of Work

First add `.github/actions/cache-rust/action.yml` to restore registry/git sources and caller-selected build paths with fresh per-commit keys. Update the workflow to use it after installing Rust. Move formatting to a standalone job, CLI release compilation to a separate OS matrix, portable compilation to its own Linux job, and desktop macOS/Windows validation to a separate matrix. Preserve the Linux desktop job. Add missing caches to the other jobs and a pinned executable cache before cargo-deny installation.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Edit using apply_patch. Run `git diff --check`, parse YAML, and run actionlint on the workflow. Compare each old run command to its destination to ensure no check disappears. Run `node --test scripts/release-workflow.test.mjs scripts/linux-release.test.mjs` for existing workflow-related behavior checks. No Rust or Cargo dependencies change, so a new Rust suite run is not needed for this workflow-only change.

## Validation and Acceptance

Locally, YAML and actionlint must pass and existing script tests must pass. On GitHub after a separately authorized push, independent jobs must start without needs dependencies, previous platform checks must pass, and a subsequent successful commit should restore a prior build cache and save its refreshed cache. Actual speedup needs two remote runs, including a warm cache; local validation cannot prove it.

## Idempotence and Recovery

Cache misses run normal builds. Repeated runs are safe; exact cache hits need no overwrite. Revert the workflow and composite action together to restore the old layout. No release or repository-setting changes are authorized here.

## Artifacts and Notes

New check names may require owners to update branch protection if individual old matrix checks are required. Do not mutate those settings here.

## Interfaces and Dependencies

The composite action takes required lane and build-paths inputs and uses actions/cache@v5. Existing Rust 1.96.0 and other tool versions remain unchanged. The cache namespace hashes configuration, Cargo manifests, and lockfile, with a commit suffix for refresh.

## Outcomes & Retrospective

Independent CLI release, portable worker, native desktop, and formatting jobs are implemented. All build jobs have scoped caches; cargo-deny has an exact-version executable cache. CONTRIBUTING.md describes the layout. Existing validation commands and platform restrictions are preserved. Local workflow validation and all 19 script tests pass. Speedup and cache reuse still need remote cold/warm runs after an authorized push. No runtime behavior changes.

Initial plan written on 2026-09-16 to record scope and measurement limits.

Updated on 2026-09-16 after implementation and local validation, including the Linux-only test-environment requirement.
