# Remove saved projects through the daemon

This ExecPlan follows `.agents/PLANS.md` and remains a record of the implementation and validation.

## Purpose / Big Picture

A user must be able to remove a saved project whose local repository directory no longer exists. The project picker must show removal progress and the actual refusal or failure inside its dialog. Removing a saved project must run in the daemon process, which owns the database writer, rather than partly succeed in the terminal client and then fail when updating the project catalog.

## Progress

- [x] Investigated picker input, missing-directory status, configuration validation, and authoritative runtime updates.
- [x] Reproduced removal in isolated instance `remove-missing-path-probe`: configuration deletion succeeded, but catalog update failed with `database writer is not available for operation hide_catalog_project`.
- [x] Added dialog-local removal feedback and focused picker regressions; focused tests passed.
- [x] Route removal through a daemon request and preserve daemon-owned configuration publication; protocol 65 includes a frozen management transcript after merging upstream's independent protocol-64 change.
- [x] Pass the isolated missing-directory RPC/runtime-feed regression, including preservation of saved configuration on a catalog-write failure, and the frozen management protocol regression.
- [x] Pass the full dev-profile suites of the four affected crates, workspace Clippy with warnings denied, formatting, and diff checks; build the fixed terminal binary.
- [x] Repeat the isolated terminal reproduction with the fixed binary: the project disappeared from the picker, saved configuration, and daemon runtime view without a removal error.
- [x] Finish review and prepare the validated changes for a commit on the current branch.

## Surprises & Discoveries

The missing-directory message is existing project status, not a removal refusal. The picker emits the correct removal action even with a missing local path. The terminal client then calls a controller helper locally; its configuration write succeeds before its database catalog write fails because the client has no database writer. Separately, the picker only rendered operation failures in the global footer, which a short dialog viewport can obscure.

Completed terminal mutations deliberately wait for the daemon's configuration feed. Installing an independent configuration snapshot from the completion would break the single-owner model and could overwrite newer state; that approach was rejected before product edits.

## Decision Log

- Decision: Keep the session-use refusal and remove no repository files. A stopped session may still need its saved project when resuming. Missing directories are not a valid removal precondition. Author: parent agent, 2026-10-27.
- Decision: Route removal through the daemon's existing configuration mutation coordinator and keep the client completion dependent on the authoritative runtime feed. The daemon owns the database writer and serialization. Author: parent agent, 2026-10-27.
- Decision: Show operation errors inside the project dialog before repository details, clear them on selection change or retry, and derive progress from the existing in-flight state. Author: parent agent, 2026-10-27.
- Decision: Hide the catalog record inside the locked configuration edit, after validation but before saving the file. A catalog failure then leaves the saved project available for retry. A later configuration-save failure can leave the catalog hidden, but the project remains in the saved configuration and can be selected or removed again. The database writer never acquires the configuration lock, so this ordering has no reverse lock dependency and matches existing project creation. Author: parent agent, 2026-10-27.
- Decision: Advance the combined protocol to 65 when merging upstream, which independently used 64 to remove terminal-owned session-title writes. Keep upstream's protocol-63 and protocol-64 management fixtures and add 65. Author: parent agent, 2026-10-27.

## Outcomes & Retrospective

Implementation and validation are complete. Focused picker tests, an isolated real client/server RPC against the database writer, the runtime-feed removal check, and management protocol compatibility all pass. Full dev-profile suites passed for `brokk-mj-client`, `brokk-mj-controller`, `brokk-mjolnir`, and `brokk-mj-tui`; workspace `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. The installed `cargo` command is already the mbx shim; the unsupported spelling `mbx cargo` was corrected before tests ran, without changing build storage. Independent review accepted daemon ownership, publication under the configuration mutex, admission through response delivery, and the failure ordering.

The final terminal reproduction used a tmux session in isolated instance `remove-missing-postfix`, with a saved project pointing at `/definitely/not/here/missingpath`. Removing it cleared the picker row and saved configuration, and an authenticated protocol-64 runtime snapshot confirmed the daemon no longer advertised it. No removal or database-writer error appeared. The isolated daemon and terminal were stopped and probe files removed. That manual case had no catalog row for the absent source; the automated RPC regression separately seeds and checks catalog hiding. The missing directory is therefore covered through both the terminal action and the daemon persistence boundary.

The configuration file and catalog database do not share a transaction. If a configuration-file save fails after the catalog is hidden, the project remains in the saved configuration and can be removed again; the failure is visible. The change deliberately orders the writes to preserve that retry path.

## Context and Orientation

`mj-tui/src/wizards/dashboard/new_session.rs` starts removal and folds completion into the picker. `mj-tui/src/wizards/render.rs` draws the picker, including project status and the new removal response. `mj-cli/src/dashboard/io/spawn.rs` starts the supervised background operation; `mj-cli/src/dashboard/io.rs` waits for the daemon feed to confirm the project is gone. `mj-client/src/daemon.rs` defines the client request protocol and connection methods. `mj-controller/src/daemon/actions.rs` dispatches requests, and `mj-controller/src/daemon/state.rs` coordinates configuration mutations. `mj-controller/src/controller.rs::remove_bundle` checks fresh session references, removes the configuration entry, and hides its catalog record.

## Plan of Work

First, retain the completed dialog feedback and focused tests. Add a remove-project request following the existing create-project request pattern, with a client method returning ordinary completion. Dispatch it in the daemon and serialize persistence with its configuration mutation coordinator. Run blocking filesystem and database work in the existing background helpers. Publish the resulting configuration through the daemon owner while respecting serialization, rather than installing snapshots in the terminal client. Use a distinct admission label for this operation. Follow the existing protocol-version and frozen management transcript conventions if the wire change requires a version increase.

Then change the terminal background removal job to call this daemon method. Preserve asynchronous input handling, session-use guards, authoritative feed confirmation, and visible failures. Add an isolated daemon integration regression with a saved local source whose directory is absent, and verify the saved configuration, catalog visibility, and runtime view after removal.

## Validation and Acceptance

From the repository root, use the installed `mbx` wrapper without changing Cargo targets or build configuration. Run focused regressions during implementation, then the full dev-profile suites of every touched crate once after the final changes, workspace `cargo clippy --all-targets -- -D warnings`, formatting, and `git diff --check`. Existing package names are `brokk-mj-client`, `brokk-mj-controller`, `brokk-mjolnir`, and `brokk-mj-tui`. All real daemon and CLI runs must use an isolated named instance and temporary configuration/data directories.

Acceptance requires that removing an unused saved project with an absent directory completes without the database-writer error, disappears from the picker and saved configuration, and stays hidden in the catalog. A session-use refusal and an asynchronous removal failure must appear within the picker dialog, including at a short viewport. Keep the existing guard that protects projects referenced by sessions.

## Idempotence and Recovery

Use only isolated probe data for reproduction; never modify live user configuration or session records. A successful removal leaves no local repository files affected. Preserve visible errors if persistence fails. Stage only this task's changed files and commit directly on the existing branch; do not push.
