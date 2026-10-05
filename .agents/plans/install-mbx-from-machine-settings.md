# Install mbx from machine settings

This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Users will be able to install the pinned Linux mbx binary on a local or SSH container host from Settings › Setup › Machines. If the host has no mbx, the action says “Install mbx”; if its mbx is too old, it says “Upgrade mbx”. The action verifies the release checksum on the controller, installs atomically on the host, runs `mbx setup --yes`, adds an idempotent PATH block to `~/.profile`, and verifies the final version. The action stays off the terminal UI event loop and asks before it changes the host.

## Progress

- [x] (2026-10-04) Read the repository instructions and both assigned investigation notes; mapped the cache preview and local/SSH transfer APIs.
- [x] Add the controller installation module, host scripts, and fake-executor tests.
- [x] Wire an eligible action, confirmation, supervised background work, and result refresh through TUI and CLI.
- [x] Run formatting, controller/TUI/CLI tests, and Clippy; fix findings.
- [x] Commit the completed implementation on the current branch.

## Surprises & Discoveries

- The current cache preview only stores an mbx version. Its private probe can return `mbx` as a PATH spelling and does not check `~/.local/bin`; the new install module will use an absolute resolved path and check PATH, `~/.local/bin`, then `~/.cargo/bin`, matching the parallel probe contract.
- The machine page expands the implied local machine's cache settings before rendering, so its fresh preview can drive the install action even when the saved config omits the default cache block.
- SSH SCP uploads are already supervised by `CommandExecutor`; an upload can target a home-relative temporary sibling, then publish with a quoted host shell command.
- An existing `.profile` may lack a final newline; the append script now starts its marked block on a new line and the temporary-HOME test covers this edge case.

## Decision Log

- Decision: Keep the installer’s absolute-path probe local to `controller/mbx/install.rs` until it can consume the parallel probe contract.
  Rationale: This worktree still has the old private probe, and the install rules require additional candidates and resolved paths. Keeping the contract-specific code in the new module avoids editing session cache/provisioning/doctor behavior while allowing the parallel change to be reconciled during merge.
  Date/Author: 2026-10-04 / Codex.
- Decision: Determine whether the TUI should offer installation through a controller helper based on the same preview result.
  Rationale: TUI must not duplicate `MBX_VERSION` comparison or infer Linux support from user-facing reason strings; the controller owns that interpretation.
  Date/Author: 2026-10-04 / Codex.

## Outcomes & Retrospective

Implemented the machine-settings Install mbx / Upgrade mbx action. The controller checks Linux and architecture, downloads the pinned musl archive into its data cache with checksum verification, selects only an absent or Mjolnir-managed destination, publishes through a temporary sibling, runs setup, appends the marked PATH block idempotently, and verifies the final path/version. The TUI confirms the host mutation, displays a busy state, and shows the result or error with a fresh preview. Tests use fake command executors or temporary HOME directories only; no real host install was run.

Validation: `cargo fmt --all -- --check` passed; `cargo test -p brokk-mj-controller` passed (2,221 passed, 10 ignored); `cargo test -p brokk-mj-tui -p brokk-mjolnir` passed (TUI: 961 passed, 2 ignored; CLI library: 280 passed, plus its isolated integration suites); the final profile-block test passed after its newline edge-case fix; workspace `cargo clippy --all-targets -- -D warnings` passed, and controller all-target Clippy passed again after the last controller edit.

Remaining integration note: this worktree's cache preview still has the old version-only probe. The installer uses its own resolved absolute-path probe per the agreed contract. Reconcile it with the parallel branch's updated preview probe when merging; `MBX_VERSION` remains in `mbx.rs`.

## Context and Orientation

`mj-controller/src/controller/mbx.rs` owns the shared cache preview and remains the source of `MBX_VERSION`; `mj-controller/src/controller/cache_host.rs` represents the local or SSH machine and builds supervised host commands. The new sibling `mj-controller/src/controller/mbx/install.rs` owns downloading the pinned release, host path selection, atomic publication, setup, and profile activation. `mj-tui/src/setup.rs` owns machine settings state and button visibility. `mj-tui/src/lib.rs` describes actions, while `mj-cli/src/dashboard/actions.rs` runs I/O off the UI loop and `mj-cli/src/dashboard/io.rs` returns results to the setup dialog.

The release archive contains a static Linux musl binary for either `x86_64` or `aarch64`; `install.rs` extracts and verifies its pinned SHA-256 before transfer. A host path is “managed” for replacement only when its resolved regular file is below `$HOME/.local/bin` or `$HOME/.cargo/bin`. Other locations, including mise-managed versions, are left alone and receive a package-manager upgrade instruction.

## Plan of Work

Move the release digests, URL construction, checksum verification, archive extraction, and cached downloader from `mbx.rs` into the new install module, retaining `MBX_VERSION` in `mbx.rs` and redirecting the existing container binary loader to the moved downloader. Add a controller function that checks Linux and host architecture, probes mbx, selects an allowed destination, transfers to a temporary sibling, chmods and renames atomically, runs setup, appends the marked profile block only when missing, and verifies both path and version. Use the shared command executor for every local/SSH command and the shared SCP upload helper for SSH. Unit tests use a fake executor and a supplied temporary binary; no test runs setup on the real host.

Have the controller interpret preview eligibility so the UI uses the same pinned-version and Linux-support rules. Add one page action and an explicit confirmation naming the machine and effects. Once confirmed, the CLI starts cancellable supervised work, immediately reflects a busy label, then returns the result and a fresh cache preview. The setup dialog displays success or the full failure and updates whether the action is install, upgrade, or hidden.

## Concrete Steps

From the assigned worktree, inspect and edit the files listed above, then run `cargo fmt --all -- --check`, controller/TUI/CLI crate tests, and `cargo clippy --all-targets -- -D warnings`. Cargo test invocations must use the repository-required elevated execution and isolated test setup. Profile block behavior may be exercised with `sh` only against a temporary HOME. Never invoke `mbx setup` on the machine or edit the real user profile.

Expected successful controller behavior is a result naming the absolute installed path, version `1.22.0`, whether `~/.profile` changed, and that new login shells pick up the PATH update. A mise-path result must fail before upload, setup, or profile editing and tell the user to upgrade through mise.

## Validation and Acceptance

Controller tests cover host platform and architecture commands, x86-64/ARM64 release selection, absent/install and managed-path replacement, refusal of external paths, local copy and SSH SCP command shapes, setup stderr propagation, exact-version verification, and marker idempotence/quoting. TUI tests cover button labels, hiding for compatible/unsupported hosts, confirmation wording, busy state, result/error, and refreshed preview. CLI wiring compiles and the crate tests pass. All changed Rust is rustfmt-clean, and workspace Clippy passes with warnings denied.

## Idempotence and Recovery

The profile script checks the marker before appending and never rewrites unrelated profile text. Binary publication uses a unique temporary sibling and atomic rename, preserving a currently running old inode. If setup or profile editing fails after publication, the command reports the failure; retrying the same action is safe and reruns idempotent setup/profile logic. Refused external installations perform no mutation.

## Artifacts and Notes

No host installation is an acceptable test artifact. Tests may use temporary local files and recorded `CommandSpec`s; the only direct shell check is against a temporary HOME.

## Interfaces and Dependencies

The controller will expose a machine install operation taking `&Machine` and `&impl CommandExecutor`, returning a result containing the absolute program path, verified version, whether the profile block changed, and whether this was an install or upgrade. A separate controller helper will decide whether a preview warrants an Install or Upgrade action. The TUI action will carry its setup generation, preview key, machine identifier, and machine value so stale results can be discarded. No new crate or persisted configuration field is needed.
