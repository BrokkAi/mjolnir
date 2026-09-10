# Shared path inputs with home expansion

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

People can type `~/.codex4` in an account directory editor and save the actual home path. All terminal path editors and existing web project/repository inputs apply the same semantics. SSH paths use the remote login home, never the controller home.

## Progress

- [x] Inspected setup, wizard, mount, web preflight, and existing text controls.
- [x] Implement shared interpretation and reusable path controls.
- [x] Integrate setup, project, repository, and mount editors and background resolution.
- [x] Validate behavior, full serial Rust suite, strict Clippy, and standalone web tests.
- [x] Commit feature checkpoint and integrate newer upstream commits on hel2.
- [x] Validate the integrated code and prepare the final merge for the authorized upstream push.

## Surprises & Discoveries

Setup edits generic JSON strings; path metadata must distinguish controller, target, and relative destinations. Project validation currently returns unit and must return the resolved path. Mount sources belong to the container engine host.

## Decision Log

Expand on apply, preserving editing text until success. Both terminal and web surfaces are included. Resolve remote home asynchronously. Only bare tilde and leading tilde path component expand; named users are unsupported. Do not expand variables or shell expressions. Container destinations require absolute paths; repository destinations remain safe relative paths. Existing hand-edited configuration is not automatically migrated. User authorized pushing after completion.

## Outcomes & Retrospective

The shared path widget is used across terminal path editors and existing web project/repository inputs. Home expansion happens on apply, uses the owning host, and is preserved through validation, launch, and persistence. Draft changes cancel pending path jobs; stale replies cannot overwrite newer input. Existing completion, destination restrictions, and upstream Git-repair behavior are retained.

The complete suite passed serially after the primary upstream integration (2,962 active tests), and all 26 standalone web unit tests passed. The final upstream macOS worker-preparation merge passed the CLI/daemon package suite, all three script tests, shell syntax validation, and strict Clippy across all targets. Parallel test execution exposed an existing self-update fixture ETXTBSY error; that test passed serially. A later core test waited for a local filesystem journal write and then passed. No host mounts or build storage were changed.

Implementation and integration are complete. The final publication command after committing this record is `git push origin HEAD:master`, preserving the existing hel2 branch and its configured upstream.

## Context and Orientation

`mj-chat/src/hel_text_input.rs` provides text editing and `mj-chat/src/components` provides reusable rendered controls. `mj-tui/src/setup.rs` edits configuration through a schema. Wizards and container dialogs edit projects and mounts. `mj-cli/src/dashboard` supervises UI background work. `mj-controller/src/hel_server.rs` accepts web requests; controller methods run their validation. `src/hel_targets.rs` provides shared subprocess commands and path validators.

## Plan of Work

First add a pure Path/PathBuf expansion helper in the core crate and PathInput/PathField wrappers reusing TextInput/TextField. Then attach explicit path context to setup fields and use path state in wizard and container editors. Resolve local homes without filesystem scans; resolve remote homes through supervised background commands. Carry resolved paths back through project and mount validation results, reject stale results, and use resolved values in persistence/history. Reuse the controller resolver for web preflight and action admission; show resolved project paths in the web review. Keep repository source classification separate from pure path input. Add behavior tests at each boundary.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. Run `cargo fmt --all -- --check`, `cargo test` with elevated permissions, and `cargo clippy --all-targets -- -D warnings`. Run the existing web test harness where applicable. Stage only changed files, commit on the current branch, and push its upstream after checks pass.

## Validation and Acceptance

Tests must cover tilde, nested suffix, spaces, Unicode, literal embedded tilde, missing home, named-user error, unchanged relative/absolute paths, and non-execution of shell text. Applying an account home must store the expanded path. Remote home failure preserves draft and surfaces an error. Cancellation and stale replies must not replace newer input. Mount destinations and repository destinations retain their restrictions. Web preflight and direct submission must resolve consistently.

## Idempotence and Recovery

No configuration migration or user-data mutation is needed. Changes are additive source edits; retries use the existing supervised job lifecycle. Failed apply preserves the draft. Git operations remain on the existing branch.

## Artifacts and Notes

The original failure was an account home `~/.codex4` passed literally as CODEX_HOME. Correct input applies `/home/jonathan/.codex4` before quota use.

## Interfaces and Dependencies

Use existing crates only. Shared interpretation accepts `&Path` and an explicit optional home and returns `Result<PathBuf>`. PathInput owns TextInput and reuses its editing behavior. PathField delegates rendering to TextField. Remote resolution uses CommandExecutor and fixed commands, with no user input in shell code. Project and mount background results carry resolved PathBuf values.

Revision: completed widget integration, remote cancellation, completion preservation, and account/mount/web regression coverage. Recorded the unrelated transient subprocess test failure before repeating validation.

Revision: recorded successful validation and the required upstream integration. No host mounts or build storage were changed.

Revision: feature checkpoint eb9a1406 is committed. Merging origin/master preserves agent-profile enablement and the Git-remote repair preflight loop; conflicts combine the new resolved project_directory response with upstream remote_repairs. Integration validation remains pending.

Validation after merging 6be22ae4: all 2,962 active Rust tests passed with one test thread, strict Clippy passed, and all 26 standalone web tests passed. The longer core run waited for an ext4 journal write; NFS TEST_STATEID stayed at 19. Master advanced again during validation, so the next step is to inspect and integrate its latest commits before publication.

Final revision: completed both upstream integrations and recorded passing validation for the final tree.
