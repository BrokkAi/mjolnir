```md
# Share immutable worker binaries between session roots


Maintain this living ExecPlan according to `.agents/PLANS.md`.

## Purpose / Big Picture


Workers currently duplicate the same executable in every session. Install each target build once, verify its SHA-256, and hard-link session executables to that immutable file. Restart swaps retain their existing rename semantics and receive a 60-second timeout. Failed hard links use private copies. No startup pre-staging remains in scope.

## Progress


- [x] (2026-10-25) Measured startup candidates and obtained decision to replace pre-staging with shared storage.
- [x] (2026-10-25) Found direct installation writes; parent authorized converting them all.
- [x] (2026-10-25) Audited checkpoint native capture/restore, Move reset, and container control-state transfer.
- [x] (2026-10-25) Implement shared target store and route initial installs, recovery, upgrades and remote harness preparation through it.
- [x] (2026-10-25) Add real-filesystem concurrency, same-inode promotion, initial-install isolation, EXDEV and no-link fallbacks, collection and tar archive regressions.
- [x] (2026-10-25) Run affected suites, final full controller/core suites with --no-fail-fast and eight test threads, formatting, and workspace Clippy; all final checks pass.

- [x] (2026-10-25) Fix review findings: retry retired copy sources, register and sweep killed store pins, and defer capacity checks until actual copies.
- [x] (2026-10-25) Re-run targeted worker-binary tests; fix the new tests' shell argument index and measured filesystem path, and update one harness command-sequence fixture.
- [x] (2026-10-25) Review follow-up targeted run: 101 passed, 3 ignored; final full controller suite: 2121 passed, 17 ignored. Formatting and whitespace checks pass.
- [x] (2026-10-25) Review follow-up workspace Clippy (--all-targets, -D warnings) passes.

## Surprises & Discoveries


Initial local, SSH, and container installs wrote directly to hel, unlike upgrades. These must disappear before sharing inodes. Container recreation uses tar over the control tree; extraction must unlink existing paths first to avoid writing through links. Ordinary checkpoints carry selected native state and repositories, not the installed hel; restore installs the executable separately. Move explicitly unlinks hel in the retained root before reinstalling. A second hard-link edge case is that mv rejects two names for an already shared inode; the common promotion function consumes the redundant private link instead. GNU tar --unlink-first rejects the root dot header; stripping the leading dot for content extraction and then restoring only root metadata preserves bytes and directory permissions.

Review found that the process-local store gate cannot protect an unpinned fallback source from a foreign collector. A failed copy must distinguish a disappearing source from other I/O failures. SIGKILL skips shell EXIT traps, so private store links need the same explicit live ownership and stale sweeping as prepared binaries. Registration and sweep execution also need serialization: protecting only the snapshot calculation leaves a window where a sweep can delete a newly registered file.

## Decision Log


The parent approved fixing all in-place writers rather than stopping again for convertible sites (2026-10-25). Store location is a worker-binaries directory beside session roots, derived from existing target worker-root helpers; it shares the roots' filesystem where possible. Container targets store inside the container, SSH bare targets on the SSH host. Image-owned workers retain their existing immutable image link. Store writes use private unique files and verified publication; no binary bytes are ever written through a session or published store path.

On 2026-10-25, retain the existing LiveStaging registry for store pins and serialize each session's registration/sweep through a separate cancellable gate. Keep ordinary EXIT cleanup, and let the existing stale sweep reclaim leftovers after SIGKILL; do not introduce an independent pin sweeper. Check capacity immediately before uploads and private copies, preserving cached links and collection on full targets. Foreign upload/transport directory cleanup remains age-only, as requested.

## Outcomes & Retrospective


Implemented lazy per-target immutable stores, shared initial/recovery/upgrade installation, safe hard-link promotion, and collection including image-worker transitions and abandoned SSH transport pins. Session recovery allows 60 seconds and deadline errors distinguish timeout from cancellation. No startup pre-staging, protocol changes, migrations, commits, or pushes were added.

Original validation passes: controller 2,118 tests with 17 existing ignores; core 546 tests with 3 existing ignores; 3 core doctests pass and one remains ignored. Formatting, diff whitespace checks, and workspace Clippy with warnings denied pass. An earlier uncapped suite encountered unrelated five-second MBX profile and ten-second shell-initialization timeouts; both pass alone on changed code and an unmodified archive of 9317fe87c. A final eight-thread full run passed both cases. The baseline checkout was removed after its tests completed.

Review follow-up validation passes 101 targeted tests and 2121 controller tests; all four intermediate failures were fixture/setup errors, corrected and rerun. No production failures remain in the final controller suite. Workspace Clippy with warnings denied, formatting, and whitespace checks also pass. No review follow-up work remains.

Live Docker/SSH targets were not exercised; existing Docker integration tests remain ignored without the required image and portable worker. The existing Docker image-worker regression now also checks collection on a transition to an image-owned worker. Existing per-session copies become shared only through subsequent installations or replacements; no bulk rewrite of live roots occurs.

## Context and Orientation


`mj-controller/src/controller/worker_binary/install.rs` installs initial session payloads. `upgrade.rs` stages and promotes worker upgrades and recoveries. `mj-core/src/targets.rs` dispatches commands to local, SSH, and container targets and supervises cancellation. `mj-controller/src/targets/container_upgrade.rs` copies stopped control state through tar. A hard link is another directory entry naming the same immutable file; removing a session link never removes the store entry.

## Plan of Work


Add one runtime installer in the worker_binary module. It resolves the target store path, checks the digest, transfers a unique private file on a miss, verifies and publishes it, links a private session staging path, then renames that path into place. Retain target-specific upload and container owner normalization through existing command helpers. Serialize store operations and garbage collection per target using an independent store owner, preserving callers' existing worker lifecycle ownership. Garbage collection removes only sufficiently old entries with no session hard links. Replace initial binary upload branches with the common installer and keep profile/configuration handling intact. Change container extraction to unlink before writing. Distinguish executor deadlines from cancellation without changing cancellation control flow.

## Milestones


The first milestone is complete: `store.rs` implements immutable verified target storage and the initial/upgrade/recovery callers all use `install_worker_binary`. SSH container transfer caches also use the common installer to pin a verified host entry during engine copy. Existing image-worker symlinks retain their safe rename semantics.

The second milestone is complete in source: real filesystem tests exercise concurrent installs, repeated same-inode installation and promotion, replacement of one shared session, deletion, collection, corruption repair, and unavailable hard links. A real tar roundtrip tests container-control-state capture and restore. The archive audit found ordinary checkpoints do not contain installed executables.

The final milestone is validation: complete the full changed-crate suites with `--no-fail-fast`, fix any failures these changes cause, and run workspace Clippy with warnings denied. Logs are in the report directory. Record final results in Progress and Outcomes before handing back without committing.

## Concrete Steps


From the repository root use `cargo fmt --all`, `cargo test -p brokk-mj-controller -p brokk-mj-core --no-fail-fast -- --test-threads=8`, and `cargo clippy --all-targets -- -D warnings`. Cargo tests run elevated and use their existing isolated data directories. Do not redirect Cargo targets or change mbx configuration. Logs belong in `.mj/agents/0ca82db8b9479753262a963875ff5906/`.

## Validation and Acceptance


Real filesystem tests must show two sessions share an inode, concurrent same-build installers converge, replacing one session leaves the other and the old store bytes unchanged, cross-filesystem links copy safely, session deletion preserves the store, and collection removes only unreferenced old entries. Real tar archive/extraction must preserve content without overwriting another hard link. Existing target-command and controller tests must pass after command-shape changes. Timeout failures name the elapsed limit and purpose while genuine cancellation remains cancellation.

## Idempotence and Recovery


Every executable write has a private destination and verified rename/publication. Failed uploads clean their private files. Retry rechecks the store and never rewrites an installed inode. Abandoned private names can be swept without affecting published entries. No live daemon or store migration is performed.

## Artifacts and Notes


Selection findings and the initial hard-link write reproduction are in the report directory. Preserve further audit and validation output there.

## Interfaces and Dependencies


Use existing TargetLocator, locator_command, CommandExecutor, target_path, worker_executable_digest, target_storage::ensure_room_for, and subprocess helpers. The common installer receives executor, locator, session identity, source WorkerInstall, and destination name; callers retain their current lifecycle and admission rules. No new crate or persisted protocol/schema contract is required.

Plan update (2026-10-25): recorded the implemented target stores, preserved SSH transport caching, additional same-inode mv failure, and current validation progress. Independent store ownership does not remove callers' lifecycle permits; no new background operation or admission blocker exists.

Validation update (2026-10-25): nine focused affected tests pass; final Clippy and formatting checks pass with source edits frozen. The first full suite exposed only the four affected fixtures/archive/deadline cases and all were fixed; no unrelated baseline failure was observed.

Completion update (2026-10-25): final full suites and lint/format checks pass. All observed affected failures were fixed, and unrelated timeout cases were checked separately on pristine source before retaining their original code. Detailed evidence is in the report directory's validation.md and final-audit.md.

Review follow-up (2026-10-25): independent review found a foreign-collector race in fallback copies, SIGKILL-leaked store pins outside the existing staging sweep, and premature full-copy capacity checks. Fallback cp now retries missing-source failures (75) while preserving other errors and the final digest verification. Session pins use canonical hel.store-* names registered through LiveStaging. A per-session cancellable staging gate serializes registration and sweep execution, preventing stale snapshots from deleting newly live files. The store and staging owners are separate; neither adds worker lifecycle ownership. Uploads check capacity only at the store upload path; failed hard links request capacity at the private session pin before copying. Initial installation, upgrade staging, immediate replacement, and managed-harness worker preparation no longer reserve duplicate worker bytes in advance. Harness download checks remain. Foreign .upload/.transport age-only collection is unchanged.

Review acceptance: reproduce source retirement immediately before cp opens it, verify unrelated copy failures remain errors, SIGKILL an actual shell after creating a pin and prove a later sweep removes it, preserve registered live pins across installs, and isolate a full-capacity board to show cached links/collection work while uploads and private copies refuse. Re-run the worker-binary tests, full controller suite (--no-fail-fast), workspace Clippy with warnings denied, and formatting before handing back without committing.
```
