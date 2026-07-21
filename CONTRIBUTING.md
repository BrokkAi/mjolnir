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
- Keep changes focused on one problem or capability. For a large ACP, Council,
  permission, session-format, terminal-mode, or release change, open an issue
  or discuss the direction on [Discord](https://discord.gg/geYkWUeH) first.
- Do not put credentials, private source code, or unredacted private
  transcripts in issues, tests, logs, or pull requests. Report suspected
  vulnerabilities privately to
  [feedback@brokk.ai](mailto:feedback@brokk.ai).

An issue is useful but not mandatory for a well-scoped pull request. Use
`Fixes #123` or `Closes #123` when a pull request resolves an existing issue.

## Development Setup

Mjolnir is a Rust 2024 workspace. The default member builds the `mj` terminal
client without the optional native speech stack:

```bash
cargo build --release
./target/release/mj --cwd .
```

The `brokk-mj-voice-worker` workspace member provides local Ctrl-R dictation.
On Debian or Ubuntu, install the ALSA development headers before building it:

```bash
sudo apt-get install libasound2-dev
cargo build --release -p brokk-mj-voice-worker
```

The worker is optional for ordinary Mjolnir development. When testing
dictation, put `mj-voice-worker` beside `mj` in the target directory or set
`MJ_VOICE_WORKER` to the worker executable.

## Understand the Runtime Boundaries

Mjolnir is an ACP client that owns terminal presentation, user input,
permissions, session controls, Council orchestration, and persistence around
one or more agent subprocesses. The detailed repository contracts are
maintained in [AGENTS.md](AGENTS.md). The most important contribution
boundaries are:

- Do not write logs to standard error while the TUI owns the terminal. Use
  `--debug-file` or `BROKK_TUI_LOG` for Mjolnir diagnostics and
  `--agent-stderr` or `BROKK_TUI_AGENT_STDERR` for ACP adapter output.
- Inline mode must remain inline. A cursor-position timeout or redraw problem
  must not terminate the session or switch the user into the fullscreen TUI.
- Permission requests must preserve the complete requested content. Long
  commands, descriptions, and option labels must remain reachable while
  wrapping, scrolling, paging, and resizing.
- Terminal ownership and restoration must be deterministic across normal exit,
  cancellation, signals, panics, subprocess failures, and startup errors.
- Keep model selection separate from ACP adapter selection. Council role
  handoffs, cancellation, permissions, token usage, and transcript labels must
  remain attributable to the correct role.
- Headless and remote paths share the Council runtime with the TUI. Preserve
  machine-readable output, non-blocking permission behavior, nested permission
  identity, and shutdown semantics when changing shared code.
- Configuration and session provenance are versioned persisted formats. Make
  migrations, fallback behavior, and worktree ownership explicit rather than
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
- Use `tests/termination_pty.rs` for terminal restoration and signal behavior.
- Use the deterministic fixtures in `tests/e2e/` for ACP process, Council
  handoff, tool, permission, transcript, or cancellation flows that need a
  process boundary.
- Add negative controls for permission, protocol, persistence, cleanup, and
  terminal-lifecycle changes.
- Update [README.md](README.md) when a user-visible command, keyboard action,
  setup flow, ACP adapter, Council behavior, remote feature, configuration
  option, or limitation changes.
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

When changing the voice worker, also run:

```bash
cargo clippy -p brokk-mj-voice-worker --all-targets -- -D warnings
cargo test -p brokk-mj-voice-worker
cargo build --release -p brokk-mj-voice-worker
```

UI changes need proportionate manual validation in every affected surface.
Check inline and fullscreen modes separately; for layout changes, include narrow
and resized terminals. Also exercise headless output or the remote viewer when
shared rendering, Council, permission, or session code affects those paths.
Include a screenshot or terminal recording for visible rendering changes.

CI runs the main checks on Linux, macOS, and Windows, checks the voice worker on
Linux, validates the Android ARM64 target, and independently verifies dependency
licenses and packaged legal files. You do not need to reproduce every runner
locally, but consider terminal capabilities, path syntax, filesystem behavior,
subprocesses, audio dependencies, and platform-specific packaging when changing
portable code.

## Dependency and License Changes

Commit `Cargo.lock` when dependency resolution changes. Mjolnir uses a reviewed,
deny-by-default dependency-license policy and ships generated notices for the
Rust workspace, native voice dependencies, embedded fonts, and the pinned Anvil
binary. Do not broaden an allowed license or add an exception without explaining
and reviewing the obligation it introduces.

After changing dependencies, license policy, bundled assets, the voice worker,
or pinned Anvil material, use Node.js 24 and the tool versions pinned by CI to
refresh and validate the reports:

```bash
cargo install --locked cargo-about --version 0.9.1 --features cli
cargo install --locked cargo-deny --version 0.20.2
cargo fetch --locked

cargo deny --workspace --config licenses/deny.toml --locked check licenses
cargo about generate --workspace --offline --config licenses/about.toml \
  --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
node scripts/generate-supplemental-third-party-notices.mjs
```

Review the generated diff rather than assuming regeneration is sufficient. CI
recreates both notice reports, inventories bundled native material, checks the
crate package contents, and fails when committed output is stale. Keep
`voice-worker/LICENSE` synchronized with the root license. A pinned Anvil update
must also update its runtime and release references together with the matching
`licenses/anvil-vX.Y.Z/` bundle.

## Pull Requests

A useful pull request description lets a reviewer understand the behavioral
change without reconstructing it from the file diff. Recent Mjolnir pull
requests consistently provide:

- A concise description of what changed, why, and the observable effect.
- Key semantic changes rather than a list of edited files.
- Root cause for bug fixes when it is known.
- Before/after evidence and capability or safety boundaries for UI, session,
  ACP, Council, permission, terminal, remote, or voice changes.
- Important touch points for broad or cross-cutting changes.
- Exact test, lint, build, packaging, benchmark, and manual-validation commands
  actually run.

If a relevant check could not be run or failed because of an environment
constraint, say so explicitly and include any narrower validation that did
pass. Do not report a check as passing based only on an expected outcome.

Reviewers will pay particular attention to:

- Terminal ownership, restoration, inline-mode resilience, and complete
  permission content.
- ACP compatibility and correct separation between Mjolnir-owned and
  adapter-owned state.
- Council role attribution, cancellation, and deterministic transcript and
  tool-result behavior.
- Safe permission, worktree, session, configuration, and remote-control
  boundaries.
- Regression tests, negative controls, and manual evidence for affected modes.
- Documentation and repository-contract drift.
- Cross-platform behavior, release packaging, and dependency-license
  obligations.

## Releases

Releases are maintainer-driven. The root and voice-worker `Cargo.toml` files
must carry the same version, with `Cargo.lock` and `SCRIPT_VERSION` in
`install.sh` updated for the release.

A `vX.Y.Z` tag triggers the GitHub release and crates.io workflows. The publish
workflow refuses to publish when the tag differs from either crate version. The
release workflow builds Linux x86-64 and ARM64, Android ARM64, Windows x86-64,
and a universal macOS archive. Desktop archives contain `mj`, the voice worker,
and pinned Anvil; Android omits the voice worker. Every archive includes the
applicable licenses and notices and is published with a SHA-256 sidecar.

To announce a published GitHub Release in Discord, set the
`DISCORD_RELEASE_WEBHOOK_URL` repository Actions secret to the target channel's
webhook URL. The release workflow reuses GitHub's generated release notes,
prevents mentions from being parsed, suppresses automatic link embeds, and
leaves a failed Discord delivery as a warning so it cannot invalidate an
already-published release.

Before tagging, maintainers should confirm that:

1. Both crate manifests, `Cargo.lock`, and `install.sh` match the intended tag.
2. The pinned Anvil runtime version, release workflow, README, and bundled legal
   directory agree when that dependency changed.
3. Formatting, Clippy, release builds, tests, and relevant cross-platform or
   packaging checks pass.
4. Dependency-license policy and generated notice reports are current.
5. User-facing installation, configuration, and release documentation reflects
   the shipped behavior.
6. The release commit is merged and the tagged commit is the exact commit meant
   to be published.
