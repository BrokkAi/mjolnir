# Shared path inputs with home expansion

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

People can type `~/.codex4` in an account directory editor and save the actual home path. All terminal path editors and existing web project/repository inputs apply the same semantics. SSH paths use the remote login home, never the controller home.

## Progress

- [x] Inspected setup, wizard, mount, web preflight, and existing text controls.
- [x] Implement shared interpretation and reusable path controls.
- [x] Integrate setup, project, repository, and mount editors and background resolution.
- [x] Validate behavior, full serial Rust suite, strict Clippy, and standalone web tests.
- [ ] Commit feature checkpoint, merge newer upstream commits, validate integration, and push upstream.

## Surprises & Discoveries

Setup edits generic JSON strings; path metadata must distinguish controller, target, and relative destinations. Project validation currently returns unit and must return the resolved path. Mount sources belong to the container engine host.

## Decision Log

Expand on apply, preserving editing text until success. Both terminal and web surfaces are included. Resolve remote home asynchronously. Only bare tilde and leading tilde path component expand; named users are unsupported. Do not expand variables or shell expressions. Container destinations require absolute paths; repository destinations remain safe relative paths. Existing hand-edited configuration is not automatically migrated. User authorized pushing after completion.

## Outcomes & Retrospective

Implementation is complete. The full suite passed with `cargo test -q -- --test-threads=1` (2,950 active tests), strict Clippy passed, and all 25 standalone web unit tests passed. Parallel runs exposed an existing self-update fixture ETXTBSY error; serial execution passed that test as well. The feature is ready for a checkpoint commit. The configured upstream is origin/master, which has four newer commits that must be merged on hel2 before the authorized push.

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
