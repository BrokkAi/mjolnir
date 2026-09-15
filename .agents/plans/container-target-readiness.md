# Check container availability before accepting a target

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

All target choices must show runtime availability and block Next when the service is down. F5 retries after the service starts. Background checks keep the terminal responsive.

## Progress

- [x] Traced picker configuration, host capacity, and shared runtime preflight.
- [x] Implement background readiness checks and picker/input guards for every target.
- [x] Supply implicit local candidates and expose Settings/profile/machine entry points.
- [x] Add missing Zcode profile choice to Settings; confirm Git refs are intentionally unsupported and keep them out.
- [x] Test failure, recovery, stale completion, Settings saves, and real-terminal explicit creation.
- [ ] Complete clean release-commit validations, CI, and publication for v2.9.0.

## Surprises & Discoveries

The picker reads configuration and sizing only. Host capacity cannot distinguish stopped Docker from healthy Apple on the same machine. Controller test_target already performs the necessary read-only preflight.

## Decision Log

Apply this to every target kind. Supply standard local candidates in Config::load without requiring setup or writing the file; saved names override defaults. Keep Config::load_from as the exact persisted view for transactions. Use the existing prerequisite dispatch hook and supervised executor. Keep launch-time preflight because availability can change after selection. Reject old results using generation numbers and configuration snapshots.

## Outcomes & Retrospective

UI Settings and live target availability are implemented and behavior-tested. Release publication remains. Apple was started successfully and passed its disposable run/exec/remove smoke test. Docker is stopped. The user expanded scope to remove every CLI-setup-only configuration dependency and requested a release when complete.

## Context and Orientation

`mj-core/src/config.rs` supplies implicit local candidates at runtime; `mj-tui/src/setup.rs` implements the Settings editor; `mj-tui/src/wizards.rs` renders choices; `mj-tui/src/wizards/dashboard.rs` handles input and prerequisite dispatch. `mj-cli/src/dashboard/actions.rs` starts background jobs and `mj-cli/src/dashboard/io.rs` applies their results. `mj-controller/src/controller/backend.rs::preflight_target` checks runtime availability.

## Plan of Work

Add per-target readiness state to DashboardState. Dispatch checks through the existing prerequisite hook while the target picker is open. Show pending/failure reasons, disable Next in rendering and input, and refresh on F5 or a new wizard. Run independent checks as supervised background tasks with deadlines. Apply only results matching their current generation and configuration.

## Concrete Steps

Work in `/Users/ryansvihla/code/mjolnir`. Edit TUI state and wizard rendering/input, then CLI action/result dispatch. Add behavior tests. Run elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, rustfmt for touched files, and `git diff --check`. Commit only changed files.

## Validation and Acceptance

Pending and failed Docker checks must prevent advancement without creating a session. F5 followed by a successful result must allow Next. A stale result must not overwrite a newer check. Cancelling the wizard or selecting another target stays immediate. Apple uses the same behavior. Existing tests must continue passing.

## Idempotence and Recovery

Read-only checks can be repeated. No database migration or automatic service startup is added. Existing launch preflight remains the final validation.

## Artifacts and Notes

Docker reports its configured Colima socket missing. Apple reported an unregistered apiserver before the service was started.

## Interfaces and Dependencies

Add request/result variants to DashboardAction and DashboardIoUpdate. Reuse spawn_cancellable_io, CancellableProcessExecutor, and Controller::test_target. No new crate or dependency.

Plan created following the report that a stopped Docker service remains selectable.


## Expanded scope and release

The UI is the primary configuration surface. F7 opens Settings and the command palette directly opens Agent Profiles and Machines and Runtimes; the target action menu opens a selected target's Settings. All serialized configuration fields must be editable, including the Zcode profile type previously missing from defaults. Git refs were investigated and deliberately excluded because validation rejects them. The optional CLI setup command remains supported. The web surface is a session viewer; this change targets the full terminal/desktop control surface used in the report.

After feature validation, follow RELEASING.md: bump the workspace version and internal dependencies, Cargo.lock, license report and package asset copies; validate the clean release commit before tagging. Verify trusted publisher authorization, push the current branch and tag, monitor publication, and update the Homebrew tap. Release authorization was explicitly provided by the user.

Revision: expanded following the user's requirement that all configuration be accessible through UI options and every target be checked live.


Release preparation evidence: all twelve crates report GitHub trusted publication from BrokkAi/mjolnir run 34933738432 at the v2.8.0 commit. All four npm packages have GitHub trusted-publisher identities and SLSA provenance naming BrokkAi/mjolnir/.github/workflows/publish-npm.yml. Both workflows are byte-for-byte unchanged from v2.8.0, including crates-io and npm-publish environments. Evidence is saved in target/release-v2.9.0-checks/publisher-verification.json. The next release is v2.9.0 because v2.8.0 already exists remotely.

Validation discoveries: installer tests needed fake Cargo to emit every artifact from grouped builds; corrected and committed separately. Linux ELF tests pass under Linux in a disposable Apple container (macOS Bash 3.2 cannot run the Linux verifier). Full runtime defaults must be preserved after config edits, while unchanged implicit defaults must stay out of persisted files; Config::update now handles this centrally.


Feature validation: all unit suites passed; the final PTY test initially exposed a changed first-run selection caused by automatic candidates. New sessions now preserve localhost as the default when there is no recent session, and the real-terminal regression passes. Local bare readiness is immediate because it uses the running host and has no external runtime to probe. TUI suite: 453 passed, 2 ignored. Strict clippy and formatting passed; documentation checks/build and npm/web tests passed; Linux ELF tests passed in a disposable Linux container. Documentation screenshots were regenerated and Settings was visually inspected.


Release integration: upstream added clickable model/effort controls, a pinned New bundle action, contextual worktree options, and portable disk-measurement assertions during release preparation. Merge preserves those changes and target-readiness gating. The added upstream wizard tests use the same successful availability fake as existing wizard behavior tests. Release build, strict Clippy, license policy/report comparison, and workspace crate packaging passed before integration; validate the combined commit before tagging.
