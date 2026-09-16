# Contributing to Mjolnir

Thanks for helping improve Mjolnir. Contributions from people using AI tools
are welcome; everyone remains responsible for the accuracy, safety, licensing,
and relevance of what they submit. Please follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before You Start

- Search existing issues and pull requests before opening a new one.
- Use the TUI, session, or remote bug form for incorrect behavior while Mjolnir
  is running. Use the other-bug form for installation, development setup,
  packaging, updating, or documentation problems. Blank issues remain
  available when neither form fits.
- Keep changes focused on one problem or capability. For a large target,
  session-format, execution-policy, checkpoint, worker-protocol, or release
  change, open an issue or discuss the direction on
  [Discord](https://discord.gg/geYkWUeH) first.
- Do not put credentials, private source code, or unredacted private
  transcripts in issues, tests, logs, or pull requests. Report suspected
  vulnerabilities privately to
  [feedback@brokk.ai](mailto:feedback@brokk.ai).

An issue is useful but not mandatory for a well-scoped pull request. Use
`Fixes #123` or `Closes #123` when a pull request resolves an existing issue.

## Development Setup

Mjolnir is a Rust 2024 workspace. The default members build the headless `mj`
controller and its terminal dashboard, without the optional native desktop and
speech stacks:

```bash
cargo build --release
./target/release/mj
```

Use `scripts/run.sh` when exercising sessions: on Linux it builds the native
controller plus the dedicated musl worker, while macOS builds both binaries
natively for `local-bare` development.

`scripts/install.sh` installs the same binaries from the checkout into Cargo's
install root, so managed targets find the portable worker beside `mj`. Both
scripts build through `scripts/lib/build.sh` and default to the release profile,
so they share Cargo artifacts and neither forces the other to recompile. The
default is release because the worker these scripts build is uploaded to remote
and container targets, where an unoptimized binary costs transfer size and
session speed. A profile flag applies to every binary a script builds, so pass
`--profile dev` to both, or to neither, when trading that for link speed.

Bare `mj` opens the workspace dashboard; a first run opens the setup dialog
instead. The `brokk-mj-voice-worker` workspace member provides local Alt+V
dictation.
On Debian or Ubuntu, install the ALSA development headers before building it:

```bash
sudo apt-get install libasound2-dev
cargo build --release -p brokk-mj-voice-worker
```

The worker is optional for ordinary Mjolnir development. When testing
dictation, put `mj-voice-worker` beside `mj` in the target directory or set
`MJ_VOICE_WORKER` to the worker executable.

## Crate Layout

The control plane starts with a foundation and then splits into independent
branches. A crate may use the crates below it. It must never use a crate above
it or introduce a dependency between sibling branches.

- `brokk-mj-core` (repository root, library `hel`) is the foundation. It holds
  configuration, persisted state, the database, the relay protocol, and the
  shared transcript, diff, and target types.
- `brokk-mj-worker` (`mj-worker/`, library `mj_worker`) is the target side. It
  runs inside a container or on an SSH host and supervises the agent process
  there.
- `brokk-mj-client` (`mj-client/`, library `mj_client`) holds client-facing
  contracts and shared presentation helpers. It uses the foundation.
- `brokk-mj-controller` (`mj-controller/`, library `mj_controller`) is the
  daemon side. It provisions targets, manages sessions, and serves the web
  surface. It uses the foundation and client branches.
- `brokk-mj-chat` (`mj-chat/`, library `mj_chat`) holds the conversation view
  state that the terminal and web surfaces render. It uses the foundation and
  client branches, in parallel with the controller.

The controller must not depend on the worker, and the worker must not depend on
the controller. The controller and chat branches must not depend on each other;
anything they both need belongs in the foundation or client crate. The worker
and controller still talk over the relay protocol. Keeping these branches
apart lets Cargo compile them at the same time, and it stops an edit in one
from rebuilding the other.

`brokk-mj-tui` (`mj-tui/`) builds on the client and chat branches;
`brokk-mjolnir` (`mj-cli/`, which builds the `mj` binary) joins the controller,
chat, and TUI branches; and `brokk-mj-desktop` (`mj-desktop/`) uses the
controller branch. Put new code in the lowest crate that can hold it.

## Understand the Runtime Boundaries

Mjolnir is a session control plane: a persistent per-user daemon owns the
session store, and the terminal dashboard, the web viewer, and the one-shot
CLI commands are all clients of it. The detailed repository contracts are
maintained in [AGENTS.md](AGENTS.md). The most important contribution
boundaries are:

- Do not write logs to standard error while a surface owns the terminal.
  Controller-facing processes log through the non-blocking rotating file
  logger (`mj-cli/src/logging.rs`) under the Mjolnir data directory, which
  retains ten files; never block or corrupt a surface that owns the terminal.
- Detaching a client must never tear down sessions. The daemon and detached
  workers keep running when the dashboard or viewer exits, and quitting a
  surface must stay responsive while its cleanup stays bounded.
- Permission requests must preserve the complete requested content. Long
  commands, descriptions, and option labels must remain reachable while
  wrapping, scrolling, paging, and resizing.
- Terminal ownership and restoration must be deterministic across normal exit,
  cancellation, signals, panics, subprocess failures, and startup errors.
- Keep relay-protocol compatibility and the separation between
  controller-owned and worker-owned state. Cancellation, permissions, and
  transcript behavior must remain deterministic across that boundary.
- The terminal dashboard and the web viewer render the same `mj-chat`
  conversation state. Preserve machine-readable daemon output and shutdown
  semantics when changing shared code.
- Configuration and session provenance are versioned persisted formats. Make
  migrations, fallback behavior, and workspace ownership explicit rather than
  silently reinterpreting stored state.
- Do not add lint suppressions to make CI pass. Fix the underlying problem; if
  an external constraint genuinely requires an exception, document the
  invariant that makes it safe.

## Tests and Documentation

Add the smallest regression test that would have caught the problem:

- Put focused unit tests beside the implementation in its module-level
  `#[cfg(test)]` block.
- For state-machine changes, test the event transition or input handler
  directly instead of relying only on a manual TUI check.
- Use the integration tests in `mj-cli/tests/` — `termination_pty.rs` for
  terminal restoration and signal behavior, plus the import, logging,
  store-divergence, and worker-proxy tests beside it.
- Use the deterministic shell/expect harness in `tests/e2e/` for flows that
  need a process boundary — for example
  `tests/e2e/run-reliability.sh --scenario multi-client-happy-path --seed N <mj binary>`
  for multi-client daemon/dashboard reliability, `session_restart_chaos.sh`,
  and the Playwright web checks under `tests/e2e/web/`.
- Add negative controls for permission, protocol, persistence, cleanup, and
  terminal-lifecycle changes.
- Update the embedded guides in `mj-controller/docs/`, `docs/SSH.md`,
  `docs/AWS.md`, and the pages in `docs/src/content/docs/` when a user-visible
  command, keyboard action, setup flow, harness, target kind, configuration
  option, or limitation changes; `docs/scripts/sync-podman.mjs` copies the
  Podman and Docker guides into the site during `npm run build` in `docs/`.
  Update [README.md](README.md) when the front-door positioning,
  installation, compatibility, or primary quick start changes.
- Update [AGENTS.md](AGENTS.md) when an implementation invariant or contributor
  checklist changes.

During development, run targeted tests by name or module. Before submitting,
run the same core checks as CI:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo test
```

Cargo never deletes stale build output, and each distinct feature or flag set
keeps its own incremental cache, so `target/` grows without bound (one checkout
reached 144 GiB). Run `scripts/sweep-target.sh` now and then, or before a long
validation run; it removes artifacts no build has used in 7 days and needs
`cargo install cargo-sweep --locked`.

When changing the voice worker, also run:

```bash
cargo clippy -p brokk-mj-voice-worker --all-targets -- -D warnings
cargo test -p brokk-mj-voice-worker
cargo build --release -p brokk-mj-voice-worker
```

UI changes need proportionate manual validation in every affected surface.
Check the terminal dashboard and the web viewer separately; for layout changes,
include narrow and resized terminals. Also exercise the viewer when shared
rendering, session, review, or permission code affects those paths.
Include a screenshot or terminal recording for visible rendering changes.

CI runs workspace lint/tests, CLI release builds, portable Linux musl worker
checks, and desktop checks in independent jobs. Linux and macOS execute the
workspace tests; Windows compiles them. Separate jobs check formatting, older
GNU/Linux compatibility, a deterministic multi-client reliability scenario,
the Linux voice worker, dependency licenses, and packaged legal files.
Build caches are isolated by job, OS, architecture, and configuration, with
per-commit keys that restore compatible prior builds and refresh after success.
Superseded CI runs for the same event and ref are cancelled. You do not need to reproduce
every runner locally, but consider terminal capabilities, path syntax,
filesystem behavior, subprocesses, audio dependencies, and platform-specific
packaging when changing portable code.

## Dependency and License Changes

Commit `Cargo.lock` when dependency resolution changes. Mjolnir uses a reviewed,
deny-by-default dependency-license policy and ships generated notices for the
Rust workspace, native voice dependencies, and embedded fonts. Do not broaden
an allowed license or add an exception without explaining
and reviewing the obligation it introduces.

CI checks the dependency-license policy using pinned cargo-deny. Generated
third-party reports are release artifacts, not repository files: the tag
workflow creates them with cargo-about and the audited native/font inventory.
Do not regenerate or commit reports when bumping a version.

To check a dependency or license-policy change locally:

```bash
cargo install --locked cargo-deny --version 0.20.2
cargo fetch --locked
cargo deny --workspace --config licenses/deny.toml --locked check licenses
```

Keep the audited license inputs under `licenses/` current when changing bundled
native material or fonts. Cargo includes the root LICENSE via workspace
metadata; there are no per-crate copies to synchronize. CI checks actual crate
contents, and release packaging fails if notice generation fails.

## Pull Requests

A useful pull request description lets a reviewer understand the behavioral
change without reconstructing it from the file diff. Recent Mjolnir pull
requests consistently provide:

- A concise description of what changed, why, and the observable effect.
- Key semantic changes rather than a list of edited files.
- Root cause for bug fixes when it is known.
- Before/after evidence and capability or safety boundaries for UI, session,
  target, execution-policy, checkpoint, terminal, remote, or voice changes.
- Important touch points for broad or cross-cutting changes.
- Exact test, lint, build, packaging, benchmark, and manual-validation commands
  actually run.

If a relevant check could not be run or failed because of an environment
constraint, say so explicitly and include any narrower validation that did
pass. Do not report a check as passing based only on an expected outcome.

Reviewers will pay particular attention to:

- Terminal ownership, restoration, dashboard and viewer resilience, and
  complete permission content.
- Relay-protocol compatibility and correct separation between
  controller-owned and worker-owned state.
- Cancellation, deterministic transcript and tool-result behavior, and
  checkpoint and recovery correctness.
- Safe permission, worktree, session, configuration, and remote-control
  boundaries.
- Regression tests, negative controls, and manual evidence for affected modes.
- Documentation and repository-contract drift.
- Cross-platform behavior, release packaging, and dependency-license
  obligations.

## Releases

Releases are maintainer-driven. Do not bump crate versions in a pull request.
The tagging runbook lives in [RELEASING.md](RELEASING.md).
