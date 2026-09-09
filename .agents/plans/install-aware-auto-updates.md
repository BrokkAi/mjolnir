# Install-aware auto-updates for npm, Homebrew, and curl installs

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up
to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` at the
repository root.

## Purpose / Big Picture

Today a Mjolnir user has no in-product way to learn that a new release exists.
Mjolnir 1.x had a startup update check, but the 2.0 rewrite dropped it, and
even the old version only *told* package-manager users to upgrade themselves.
After this change, `mj` checks — at most once a day, only for interactive
runs — whether the channel it was installed from has a newer release. If so,
it **asks first**, and only after an explicit yes does it upgrade:

- A **curl install** (the `install.sh` release installer, files under
  `~/.local/bin` by default) keeps the old 1.x mechanism unchanged: download
  the release archive, verify its SHA-256 sidecar, extract `mj`, replace the
  running executable, and re-exec.
- An **npm install** (`npm install -g @brokkai/mjolnir`) runs
  `npm install -g @brokkai/mjolnir@latest` as a child process after the
  prompt. Mjolnir never writes into `node_modules` itself; npm owns that
  tree.
- A **Homebrew install** (the `BrokkAi/homebrew-tap` formula) runs
  `brew update` and then `brew upgrade mjolnir` after the prompt. The
  `brew update` step matters: the *local* Homebrew formula index is often
  stale right after a release even though the tap on GitHub is fresh, and
  without it `brew upgrade` reports "already up-to-date".
- A **cargo install** (`cargo install brokk-mjolnir`) gets the old one-line
  notice with the command to run. Delegating to cargo is deliberately out of
  scope: a self-driven cargo rebuild is slow and surprising.

You can see it working by starting `mj` from a terminal on a machine whose
installed version is older than the channel's latest release: before the
dashboard appears, mj prints what it found, asks
`Upgrade now? [Y/n]`, and on consent runs the channel's upgrade command with
its live output visible, then restarts into the new version. Answering `n`,
or having no update, changes nothing about startup.

## Progress

- [x] (2026-09-09) Research: locate the 1.x prior art (`src/self_update.rs`
      as of commit `e07c8c5e`), confirm the 2.0 tree has no update code, and
      confirm the Homebrew tap formula and npm launcher both set
      `MJOLNIR_NO_UPDATE_CHECK`, which nothing reads anymore.
- [x] (2026-09-09) Design agreed with the user: delegate to package managers
      for npm/brew, keep the curl mechanism, cargo stays notice-only.
- [x] (2026-09-09) Milestones 1+2 (commit `0c902697`): update-check module in
      `mj-controller/src/hel_controller/update.rs` — detection, per-channel
      version lookups, 24-hour stamp, prompt-and-apply flows for all four
      channels, curl self-replace port, restart retargeting. 23 tests,
      clippy clean. Committed as one checkpoint because an M1-only commit
      fails the `-D warnings` gate on intentionally-not-yet-wired code.
- [x] (2026-09-09) Milestone 3 (commit `9f9eee60`): startup wiring in
      `mj-cli/src/main.rs` for `None`/`Workspaces` invocations.
- [x] (2026-09-09) Milestone 4 (commit `362dd902`): npm launcher sets
      `MJOLNIR_MANAGED_BY_NPM`/`_NPX`; 12 npm tests pass.
- [x] (2026-09-09) Milestone 5 (commit `206d29dd`): install.md "Keeping
      Mjolnir current" section; RELEASING.md Homebrew tap section. The
      storage-network.md part is dropped — see Surprises.
- [x] (2026-09-09) Full validation: `cargo fmt --check` clean; `cargo
      clippy --all-targets -- -D warnings` clean workspace-wide; `cargo
      test` passes with zero failures across every suite (run outside the
      sandbox as the suite requires); `npm test` 12/12.

## Surprises & Discoveries

- Observation: the 1.x fallback for an npm install whose env marker was
  missing classified the binary as a "direct" install, so the old self-update
  path would have replaced files *inside* `node_modules` and corrupted npm's
  package database.
  Evidence: `git show e07c8c5e:src/self_update.rs`,
  `InstallMethod::current()` falls through to `install_method_from_exe`,
  which has no `node_modules` check.
- Observation: both the npm launcher and the Homebrew formula's `mj` wrapper
  set `MJOLNIR_NO_UPDATE_CHECK=true` in the 2.0 tree, but no 2.0 code reads
  that variable — the launchers disable a check that no longer exists.
  Evidence: `npm/launcher/mj.js:61`; the tap formula at
  `https://raw.githubusercontent.com/BrokkAi/homebrew-tap/main/Formula/mjolnir.rb`
  (its wrapper exports the variable); `grep -r MJOLNIR_NO_UPDATE_CHECK mj-cli src mj-tui`
  finds no reader.
- Observation: the dashboard's background-job pattern (`DashboardIoUpdate`
  variants, `tokio::spawn` + mpsc, notice bar via `set_notice`) is the house
  style for long work near the TUI — but it is not needed here, because the
  entire update flow completes *before* the TUI event loop starts.
  Evidence: `mj-cli/src/dashboard/io.rs:44`; `mj-cli/src/pollers.rs:1031`.
- Observation: the 1.x `docs/src/content/docs/storage-network.md` service
  table no longer exists in the 2.0 docs tree, so the planned network-contact
  disclosure was folded into install.md's "Keeping Mjolnir current" section
  instead.
  Evidence: `ls docs/src/content/docs/` has no storage-network.md.

## Decision Log

- Decision: npm and brew upgrades delegate to the package manager
  (`npm install -g @brokkai/mjolnir@latest`, `brew update` +
  `brew upgrade mjolnir`); curl installs keep the 1.x self-replace mechanism
  byte-for-byte in behavior.
  Rationale: writing into `node_modules` or the Homebrew Cellar by hand
  corrupts the package managers' databases — the invariant the npm launcher
  comment has stated since 1.x. The user confirmed the per-channel commands
  and that "the old mechanism works great".
  Date/Author: 2026-09-09, Ryan (user) + plan author.
- Decision: cargo installs stay notice-only.
  Rationale: user directive; a self-driven `cargo install` rebuild is slow
  and compiles on the user's machine, unlike the other channels' binary
  installs.
  Date/Author: 2026-09-09, Ryan (user).
- Decision: the check and prompt run at startup, before the dashboard's
  render loop starts, reusing the 1.x user experience.
  Rationale: it preserves the curl flow the user called out as working well,
  avoids new in-TUI dialog/restart machinery, and cannot violate the
  repository rule that TUI/web surfaces never block on their event loops
  because no event loop exists yet when the check runs. The check itself is
  throttled so it normally costs nothing.
  Date/Author: 2026-09-09, plan author.
- Decision: throttle the network check to once per 24 hours via a stamp file
  at `data_dir()/update-check.json`.
  Rationale: the 1.x code contacted an API on every interactive start; a
  stamp keeps startup fast and polite while still surfacing releases the
  same day for daily users.
  Date/Author: 2026-09-09, plan author.
- Decision: `npx` runs are notice-only, like cargo.
  Rationale: an `npx` run is ephemeral (a cached copy, not an installation).
  Running `npx -y @brokkai/mjolnir@latest` from inside a live session would
  nest a second mj; the honest action is to tell the user the command.
  Date/Author: 2026-09-09, plan author.
- Decision: native Windows skips the check entirely; the 1.x PowerShell
  replacement machinery is not ported.
  Rationale: Mjolnir 2.0 supports the controller on Linux and macOS only
  (Windows is a CI compile gate; use WSL2, per `docs/src/content/docs/install.md`),
  so a Windows self-replacer would be dead, untestable code.
  Date/Author: 2026-09-09, plan author.
- Decision: the module lives in `mj-controller` at
  `mj-controller/src/hel_controller/update.rs`.
  Rationale: that crate already depends on `reqwest` (json + rustls), `sha2`,
  and the shared subprocess helpers (`hel::hel_subprocess`), and `mj-cli`
  already depends on `mj-controller`, so no new dependency edges are created.
  Date/Author: 2026-09-09, plan author.
- Decision: the npm launcher and (out-of-repo) Homebrew wrapper switch from
  setting `MJOLNIR_NO_UPDATE_CHECK=true` to setting
  `MJOLNIR_MANAGED_BY_NPM` / `MJOLNIR_MANAGED_BY_NPX` /
  `MJOLNIR_MANAGED_BY_HOMEBREW`; `MJOLNIR_NO_UPDATE_CHECK` remains a
  user-set opt-out.
  Rationale: the markers are how mj distinguishes "delegate to the package
  manager" from "self-replace"; keeping the old variable as an opt-out
  preserves an escape hatch for air-gapped or pinned setups. The formula
  change lands in the separate tap repository and is documented in
  RELEASING.md so the per-release tap bump carries it.
  Date/Author: 2026-09-09, plan author.
- Decision: exe-path detection additionally recognizes `node_modules`
  installs without the env marker.
  Rationale: closes the 1.x hole where an env-less npm install would have
  been misclassified as "direct" and self-replaced under `node_modules`.
  Date/Author: 2026-09-09, plan author (see Surprises).
- Decision: milestones 1 and 2 landed as a single commit.
  Rationale: the repository gate is `cargo clippy --all-targets -- -D
  warnings`; an M1-only commit carries dead-code warnings for code M2 was
  always going to wire, so the first green checkpoint is the full module.
  Date/Author: 2026-09-09, plan author.
- Decision: Homebrew restarts through the `mj` wrapper on `PATH`, not
  through the running binary's own path.
  Rationale: `brew upgrade` installs the new release into a fresh Cellar
  directory and repoints the wrapper; re-execing this process's own Cellar
  path would relaunch the old version. npm bundles replace files in place,
  so npm keeps the same-exe restart. Encoded in
  `managed_restart_target` and pinned by
  `managed_restart_retargets_homebrew_to_its_wrapper`.
  Date/Author: 2026-09-09, plan author.
- Decision: the prompt names the semver version uniformly (the 1.x prompt
  showed the raw tag for curl installs).
  Rationale: one consistent line across channels; the tag still appears in
  the download banner, where it selects the release.
  Date/Author: 2026-09-09, plan author.

## Outcomes & Retrospective

Shipped: interactive `mj` startups check their own install channel at most
once a day, ask `Upgrade now? [Y/n]` before changing anything, and on
consent upgrade through the package manager (npm: `npm install -g
@brokkai/mjolnir@latest`; Homebrew: `brew update && brew upgrade mjolnir`)
or, for curl installs, through the unchanged 1.x download-verify-replace
flow; npx and cargo installs get the notice line only. The npm launcher
declares `MJOLNIR_MANAGED_BY_NPM`/`_NPX` instead of disabling the check, and
RELEASING.md pins the tap wrapper's `MJOLNIR_MANAGED_BY_HOMEBREW` contract
for the out-of-repo formula bump. Validated by 23 module tests (including
loopback-server end-to-end channel checks), the launcher suite, and the full
workspace gate.

What remains: the tap formula change itself (separate repository, rides the
next release's bump); `mj app`/desktop binaries never check (out of scope);
a real end-to-end prompt against a published newer release is only
observable after the next version ships — until then the loopback tests
stand in for the network half.

Lesson learned: the 1.x code's biggest risk was never the happy path — it
was misclassifying an install (env-less npm would have self-replaced inside
`node_modules`). The path-forensics backstop and the restart-retarget test
exist because classification, not downloading, is where the old design was
one env var away from corrupting a package database.

## Context and Orientation

This repository is a Cargo workspace of cooperating Rust crates plus a few
packaging trees. The ones that matter here:

- `src/` is the `hel` library crate (`brokk-mj-core`), the shared core. Two
  helpers live here: `hel::hel_config::data_dir()` (defined in
  `src/hel_config.rs` around line 1674) returns the per-user data directory
  (for example `~/.local/share/mjolnir`), and
  `hel::hel_config::atomic_write` writes a file via a temp file plus rename.
  `src/hel_subprocess.rs` holds the shared child-process helpers every call
  site must use instead of hand-rolled `std::process` plumbing — notably
  `run_inherited(&mut Command) -> Result<ExitStatus>` (child shares the
  terminal, used for interactive installs) and `run_capturing_stdout`.
- `mj-controller/` (`brokk-mj-controller`, library name `mj_controller`)
  holds daemon-side controller logic. Its `src/hel_controller.rs` declares
  submodules (`mod provisioning;`, `mod worker_binary;`, …) that live in
  `src/hel_controller/`. It already depends on `reqwest 0.12` (features
  `json`, `rustls-tls`, `blocking`, `stream`) and `sha2`.
- `mj-cli/` (`brokk-mjolnir`) is the `mj` binary users invoke. Its
  `src/main.rs` builds one Tokio runtime in `fn run()` and dispatches to
  `run_command`, which by default (no subcommand) or with the `workspaces`
  subcommand starts the interactive full-screen dashboard (the TUI). The
  TUI itself is state-only code in `mj-tui/` and is irrelevant to this plan,
  because everything here happens before it starts.
- `npm/` packages `@brokkai/mjolnir` for npm. `npm/launcher/mj.js` is the
  `bin` shim: it resolves the platform bundle under `node_modules` and
  spawns the native `mj` binary from it, passing an environment it
  constructs. `npm/test/launcher.test.mjs` asserts on that environment.
- `install.sh` is the curl installer; it places binaries in
  `~/.local/bin` (or `$MJOLNIR_INSTALL_DIR`).
- The Homebrew tap is the separate GitHub repository `BrokkAi/homebrew-tap`.
  Its formula `Formula/mjolnir.rb` downloads the release archives, installs
  into the Cellar, and exposes a *wrapper script* named `mj` that sets
  environment variables before exec-ing the real binary. This repository
  cannot change that file directly; RELEASING.md will document the required
  wrapper contract so the per-release tap bump carries it.

An "install method" is how the running `mj` binary got onto the machine. The
word "channel" means the distribution source for that method: the npm
registry, the Homebrew tap's formula file on GitHub, or GitHub Releases
(curl). "Re-exec" means replacing the current process image with a fresh
exec of the (new) binary so the user continues in the upgraded version
without retyping their command.

Mjolnir 1.x had `src/self_update.rs`, last seen in commit `e07c8c5e`
("Make update notices install-aware"), removed by the 2.0 rewrite. This plan
revives its detection, version lookups, and curl flow nearly verbatim, and
adds the new delegate-to-package-manager flows. You can read the old file
with `git show e07c8c5e:src/self_update.rs`.

The environment-variable contract, all lowercase `MJOLNIR_` prefixed:

- `MJOLNIR_MANAGED_BY_NPM`, `MJOLNIR_MANAGED_BY_NPX`,
  `MJOLNIR_MANAGED_BY_HOMEBREW` — set to any value by the respective
  launcher/wrapper to declare who owns upgrades. Presence is what matters.
- `MJOLNIR_NO_UPDATE_CHECK` — set by the *user* to disable all of this.

## Plan of Work

### Milestone 1 — the update-check module

Create `mj-controller/src/hel_controller/update.rs` and declare
`pub mod update;` in `mj-controller/src/hel_controller.rs` next to the
existing `mod` declarations. Add `semver = "1"` to
`mj-controller/Cargo.toml` `[dependencies]` (it is already in `Cargo.lock`
transitively; this makes it a direct dependency).

The module contains, in this order:

1. Source URLs as constants, exactly as 1.x had them:
   `https://api.github.com/repos/BrokkAi/mjolnir/releases/latest`,
   `https://registry.npmjs.org/@brokkai%2Fmjolnir/latest`, and
   `https://raw.githubusercontent.com/BrokkAi/homebrew-tap/main/Formula/mjolnir.rb`.
   Group them in a small `struct UpdateSources` with a `Default` impl so
   tests can point every fetch at a local loopback server.
2. `enum InstallMethod { Npm, Npx, Homebrew, Cargo, Direct }` with
   `fn detect(env: &dyn Fn(&str) -> Option<OsString>, exe: Option<&Path>) ->
   Self`. Detection order: the `MJOLNIR_MANAGED_BY_NPX`,
   `MJOLNIR_MANAGED_BY_NPM`, then `MJOLNIR_MANAGED_BY_HOMEBREW` markers;
   then exe-path heuristics — any `Cellar`/`mjolnir` adjacent component pair
   means Homebrew (port 1.x `is_homebrew_executable`), the path containing
   `node_modules` and `@brokkai` means Npm (new, closes the 1.x hole);
   a path whose canonical parent is a `bin` directory recorded in a sibling
   `.crates2.json`/`.crates.toml` means Cargo (port 1.x
   `cargo_install_root` / `cargo_install_recorded` / `cargo_source_matches`
   verbatim); anything else is Direct. In production `env` reads
   `std::env::var_os` and `exe` is `std::env::current_exe().ok().as_deref()`.
   Keep `fn update_command(&self) -> Option<String>` (the human-readable
   upgrade command used in notices: npm →
   `npm install -g @brokkai/mjolnir@latest`, npx →
   `npx -y @brokkai/mjolnir@latest`, brew → `brew upgrade mjolnir`, cargo →
   `cargo install --locked brokk-mjolnir`, Direct → `None`) and
   `fn channel_name(&self) -> &'static str`.
3. Version lookups, all through one private
   `async fn fetch_text(sources: &UpdateSources, url: &str) -> Result<String>`
   that builds a `reqwest::Client` with a 5-second timeout and a
   `mj/<version>` user agent, rejects non-success statuses with the URL and
   status in the error. Then:
   - `fetch_latest_npm_version` GETs the npm URL and deserializes
     `{"version": "..."}`.
   - `fetch_latest_homebrew_version` GETs the formula URL and finds the
     first trimmed line of the form `version "X.Y.Z"` (port 1.x
     `parse_homebrew_formula_version`).
   - `fetch_latest_release` GETs the GitHub URL and deserializes
     `tag_name` plus `assets: [{name, browser_download_url}]`.
   - `update_info_from_release` ports 1.x asset selection: parse the tag
     with semver, return `None` when not newer than the current version,
     pick the `mj` archive for the current platform (macOS prefers
     `-universal-apple-darwin.tar.gz`; Linux matches
     `-<rust_target>.tar.gz`), and require a `<archive>.sha256` sidecar
     asset. Port 1.x `Platform`/`current_platform` wholesale.
4. `struct UpdateInfo { version, tag, asset, checksum_asset }` and
   `enum AvailableUpdate { Managed { version, method }, Direct(UpdateInfo) }`,
   plus `async fn latest_update(sources, method, current_version) ->
   Result<Option<AvailableUpdate>>`: Direct consults the release endpoint;
   every other method consults its channel endpoint and compares semver.
5. A 24-hour stamp: `struct UpdateCheckStamp { last_check_ms: u64 }`,
   read/written at `hel::hel_config::data_dir().join("update-check.json")`
   with `hel::hel_config::atomic_write`; `fn check_is_due(now_ms) -> bool`
   returns false when the stamp is younger than 24 hours. A missing or
   corrupt stamp file counts as due. The stamp is written *before* the
   network fetch so a hung fetch cannot cause a retry storm, and only when
   the check actually runs (never for notice-only-only paths or skips).
6. Unit tests (`#[cfg(test)] mod tests` at the bottom of the file), in the
   repository's descriptive style: a detection table over (markers, exe
   path) → method; parser tests with npm/formula/release fixtures copied
   from the real endpoints' shapes; asset-selection tests for the universal
   macOS archive and the sha256-sidecar requirement; stamp-age tests.

### Milestone 2 — prompt and apply

Extend the same module with the apply flows:

1. `enum StartupUpdateOutcome { Skipped, UpToDate, Notified, Declined }`
   (the process never returns from a successful upgrade — it re-execs).
2. `pub async fn check_prompt_and_apply() -> StartupUpdateOutcome`: the
   public entry point. Guards, in order: return `Skipped` on Windows
   (`cfg!(windows)`), debug builds (`cfg!(debug_assertions)`), non-terminal
   stdin or stdout (`std::io::IsTerminal`), or
   `MJOLNIR_NO_UPDATE_CHECK` present. Then detect the method, read the
   stamp (not due → `Skipped`), fetch, and dispatch. Every fallible step is
   reported as a one-line `mj: …` message on stderr and yields `Skipped` —
   a broken update check must never block someone from using mj.
3. Managed channels: for `Homebrew`, prompt; on consent run
   `brew update` then `brew upgrade mjolnir` via
   `hel::hel_subprocess::run_inherited` so live output streams to the
   terminal; on success print `mj: upgraded to X.Y.Z; restarting` and
   `restart_current_process()`. For `Npm`, the same with
   `npm install -g @brokkai/mjolnir@latest` (locate `npm` on `PATH`; error
   clearly if absent). For `Npx` and `Cargo`, print the 1.x notice line —
   `mj X.Y.Z is available through <channel>; current version is <current>.
   Run: <command>` — and return `Notified`.
4. The prompt is 1.x `prompt_for_update` verbatim: print
   `mj <tag-or-version> is available; current version is <current>. Upgrade
   now? [Y/n] `, flush, read a line; empty, `y`, `Y`, `yes` accept; anything
   else declines (`Declined`).
5. Direct: port 1.x `download_apply_and_restart` and its helpers —
   `download_bytes` (120-second timeout), `verify_checksum` (sha256 the
   archive against the sidecar's first whitespace field),
   `extract_mj_binary`/`extract_optional_voice_worker` over tar.gz and zip
   (port `extract_named_binary*`; keep the "sidecars are optional, `mj` is
   mandatory" comment and behavior), `install_voice_worker`/
   `install_sibling_binary`, `replace_current_exe` (Unix temp-file + chmod
   755 + rename; macOS `xattr -d com.apple.quarantine` strip), and
   `restart_current_process` (Unix `exec` of the new binary with the
   original argv). Skip 1.x's Windows replacement script entirely; the
   Windows guard in `check_prompt_and_apply` makes it unreachable.
6. Tests: extend the loopback-server tests to drive `latest_update` for all
   channels end-to-end; test the prompt parser as a pure function
   (`fn prompt_answer_is_yes(&str) -> bool`); test the notice text
   formatting; test that the brew/npm command builders produce the exact
   `Command` (program plus args) via a small pure
   `fn upgrade_command_for(method) -> Command`-style helper so no test
   spawns a real package manager.

### Milestone 3 — startup wiring

In `mj-cli/src/main.rs`, inside `fn run(cli: Cli)` after the Tokio runtime
is built and before `runtime.block_on(run_command(...))`: when
`cli.command` is `None` or `Some(Command::Workspaces)`, call
`runtime.block_on(mj_controller::hel_controller::update::check_prompt_and_apply())`.
If it returns `Skipped`, `UpToDate`, `Notified`, or `Declined`, continue
into `run_command` unchanged (a notice has already been printed). A
successful upgrade never returns (the process re-execs), so no extra
branching is needed. All other subcommands (daemon, import, doctor, login,
app, …) never consult the updater.

### Milestone 4 — npm launcher markers

In `npm/launcher/mj.js`, replace the current env construction: delete
`MJOLNIR_NO_UPDATE_CHECK` from the inherited environment, then set
`MJOLNIR_MANAGED_BY_NPX: "true"` when `process.env.npm_command === "exec"`
(the marker of an `npx` run) else `MJOLNIR_MANAGED_BY_NPM: "true"`. Keep
the existing comment explaining that npm owns upgrades. Update
`npm/test/launcher.test.mjs` to assert the new markers (one case per
`npm_command` value) and remove the old assertion.

### Milestone 5 — documentation

- `docs/src/content/docs/install.md`: add a short "Keeping Mjolnir current"
  section after the installer sections: mj checks its install channel once
  a day on interactive startup, asks before changing anything, upgrades
  curl installs itself, runs `npm install -g @brokkai/mjolnir@latest` for
  npm, runs `brew update && brew upgrade mjolnir` for Homebrew, and only
  prints the command for npx and cargo; set `MJOLNIR_NO_UPDATE_CHECK=1` to
  opt out.
- `docs/src/content/docs/storage-network.md`: extend the service-contact
  table — npm registry row gains "npm/npx update checks"; GitHub row gains
  "update checks"; add a row for the Homebrew tap raw-formula fetch
  (github.com) for Homebrew update checks.
- `RELEASING.md`: add a "Homebrew tap" section recording that each release
  bumps the tap formula and that the formula's `mj` wrapper must export
  `MJOLNIR_MANAGED_BY_HOMEBREW=1` (and must not export
  `MJOLNIR_NO_UPDATE_CHECK`), because mj uses that marker to choose
  `brew upgrade` over self-replacement. Note the tap repository is updated
  outside this repository's release workflow.

## Concrete Steps

All commands run from the worktree root
(`/home/ryan/code/mjolnir/.claude/worktrees/fuzzy-puzzling-tome`). The
repository requires Rust tests to run outside the restricted sandbox with
elevated permissions, because the suite uses loopback TCP.

Milestone 1:

    $EDITOR mj-controller/src/hel_controller/update.rs
    $EDITOR mj-controller/src/hel_controller.rs   # add: pub mod update;
    $EDITOR mj-controller/Cargo.toml              # add: semver = "1"
    cargo test -p brokk-mj-controller update::

Expect the new module's tests to compile and pass; nothing else moves yet.

Milestone 2:

    $EDITOR mj-controller/src/hel_controller/update.rs
    cargo test -p brokk-mj-controller update::

Expect prompt, notice, and command-builder tests to pass alongside
Milestone 1's.

Milestone 3:

    $EDITOR mj-cli/src/main.rs
    cargo build -p brokk-mjolnir
    MJOLNIR_NO_UPDATE_CHECK=1 ./target/debug/mj   # starts normally, no updater output
    ./target/debug/mj                             # debug build: updater skips silently

Debug builds skip the updater by design (`cfg!(debug_assertions)`), so the
second command is the expected no-op, not a failure.

Milestone 4:

    $EDITOR npm/launcher/mj.js npm/test/launcher.test.mjs
    npm test

Expect the launcher tests to pass with the new marker assertions.

Milestone 5: edit the three documents; no build required, but re-read the
diff.

Full gate, in this order:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test            # outside the sandbox, elevated permissions
    npm test

## Validation and Acceptance

Automated, per milestone: the commands above, with the update-module tests
failing before each milestone's code exists and passing after (for
Milestone 1 that is literally true — the module does not exist yet).

Behavioral acceptance, phrased so a person can verify:

1. With `MJOLNIR_NO_UPDATE_CHECK=1` set, a release build of `mj` starts
   with no updater output and no network call (verify with any process
   network monitor, or by pointing the sources at an unreachable host —
   the stamp guard plus the env guard mean it never dials).
2. On a machine with an npm-installed mj older than the registry's
   `latest`, a release build prints the prompt before the dashboard;
   answering `n` starts the dashboard unchanged; answering `y` streams
   `npm install -g @brokkai/mjolnir@latest` output, then re-execs into the
   new version (`mj --version` inside the restarted process reports the
   new one).
3. Same scenario with a Homebrew install: consent runs `brew update` then
   `brew upgrade mjolnir` visibly, then re-execs.
4. Same scenario with a curl install: consent downloads the release
   archive, verifies its sha256 sidecar, replaces the binary, and re-execs
   (the 1.x behavior, unchanged).
5. A second `mj` start within 24 hours of a check performs no fetch.
6. With the network unplugged, `mj` starts normally after a brief,
   non-fatal `mj: …` stderr line (once per 24 hours at most).

Points 2–4 require a published release newer than the local install; until
the next release exists they are validated by the loopback-server tests,
which replay the exact endpoint payloads (npm JSON, formula text, release
JSON with asset lists) and assert the fetched decision and the exact child
command that would run.

## Idempotence and Recovery

Every step is additive and re-runnable. If a milestone fails midway, the
workspace still builds (each milestone is committed only after its tests
pass), and re-running the step's edit/test pair is safe. The stamp file is
written with `atomic_write`, so a crash cannot leave a half-written stamp;
deleting `update-check.json` under `data_dir()` simply makes the next start
check again. An interrupted upgrade is recoverable by design: `npm install
-g` and `brew upgrade` are both re-runnable package-manager operations, and
the curl path writes to a temp file and renames only after the checksum
verifies, so a failed download leaves the installed binary untouched. The
installer's own recovery path ("re-run install.sh") also remains valid for
curl installs.

## Artifacts and Notes

Prior art, for reference while porting (all reachable from this worktree):

    git show e07c8c5e:src/self_update.rs        # the full 1.x module
    git show e07c8c5e -- npm/launcher/mj.js     # the launcher marker diff
    git log --oneline -3 --all -- src/self_update.rs

Current-tree anchors cited by this plan:

    mj-cli/src/main.rs                 fn run(): runtime + dispatch, wiring point
    mj-controller/src/hel_controller.rs  submodule declarations
    mj-controller/src/hel_subprocess.rs  run_inherited (line ~234), run_capturing_stdout
    src/hel_config.rs                  data_dir (~1674), atomic_write
    npm/launcher/mj.js                 env construction (~lines 53-63)
    npm/test/launcher.test.mjs         env assertions (~line 64)

The 1.x notice line, kept verbatim for npx/cargo:

    mj 2.5.0 is available through Homebrew; current version is 2.4.0. Run: brew upgrade mjolnir

## Interfaces and Dependencies

New dependency: `semver = "1"` in `mj-controller/Cargo.toml`
(`[dependencies]`). Everything else already exists in the workspace
(`reqwest`, `sha2`, `serde_json`, `flate2`, `tar`, `zip`, `toml`, `dirs`,
`anyhow`).

Public interface after completion — in `mj_controller::hel_controller::update`:

    pub enum InstallMethod { Npm, Npx, Homebrew, Cargo, Direct }
    pub enum StartupUpdateOutcome { Skipped, UpToDate, Notified, Declined }
    pub async fn check_prompt_and_apply() -> StartupUpdateOutcome

Everything else in the module is private. `InstallMethod::detect` takes
injected environment/exe accessors so tests never mutate process state; the
URL constants are grouped in a private `UpdateSources` struct with
`Default` so loopback tests can substitute endpoints. The npm launcher
contract is: the child environment contains exactly one of
`MJOLNIR_MANAGED_BY_NPM` / `MJOLNIR_MANAGED_BY_NPX`, and the Homebrew
wrapper contract (documented in RELEASING.md, applied in the tap repo) is
`MJOLNIR_MANAGED_BY_HOMEBREW=1`.

## Revision Notes

- 2026-09-09, post-implementation: Progress, Surprises, Decision Log, and
  Outcomes updated to record the implementation. Milestones 1+2 landed as
  one commit for the `-D warnings` gate; storage-network.md turned out not
  to exist in the 2.0 docs, so its disclosure moved into install.md; the
  Homebrew restart targets the wrapper on `PATH` (not the running Cellar
  binary), and the prompt names the semver version uniformly across
  channels.
