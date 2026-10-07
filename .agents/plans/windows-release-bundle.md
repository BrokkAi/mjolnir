# Publish a native Windows release bundle with the Linux workers

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. It tracks GitHub issue #1241.


## Purpose / Big Picture

Mjolnir's controller (`mj`, the terminal client and CLI that also launches the background daemon `mj daemon-run`) already builds and starts on native Windows, but nothing ships it there. Every GitHub release carries Linux and macOS archives only, the curl installer `install.sh` is bash-only and refuses Windows, npm has no Windows package, and the startup update check returns early on Windows. A Windows user therefore has to build from source and update by hand.

After this work a GitHub release also carries `brokk-mjolnir-<tag>-x86_64-pc-windows-msvc.zip` and its `.zip.sha256` sidecar. The zip holds `mj.exe`, `mj-desktop.exe` (the native window `mj app` opens), and the two static Linux session workers `mj-worker-x86_64-unknown-linux-musl` and `mj-worker-aarch64-unknown-linux-musl`, which the controller copies into Docker containers and onto SSH hosts. A PowerShell installer, `install.ps1` at the repository root, installs that bundle with one command:

    irm https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.ps1 | iex

and an installed `mj.exe` offers the next release at startup, downloads it, verifies its checksum, replaces itself and its companions even though they are running, and restarts into the new build, exactly as the release-installer path already does on Linux and macOS.

To see it working: on Windows, run the installer, open a new terminal, run `mj --version`; then install an older tag with `MJOLNIR_VERSION` and start `mj` to watch the upgrade prompt replace the running build.


## Progress

- [x] (2026-10-07) Research the release workflow, installers, updater, worker lookup, and npm checks; write this plan.
- [x] (2026-10-07) Milestone 1: build and package the Windows zip in `.github/workflows/release.yml`, attach it to the release, and keep npm publication's asset count correct; extend `scripts/release-workflow.test.mjs`. The package step was run locally against fixtures (with `tar -a` standing in for `zip`) and its sidecar verified with `shasum -c`; the full test runs in the Linux `licenses` CI job.
- [ ] Milestone 2: let the release-installer update path replace running binaries and restart on Windows (`mj-controller/src/controller/update.rs`), and recognize Cargo installs that record `mj.exe`.
- [x] (2026-10-07) Milestone 3: add `install.ps1` and its Windows test `scripts/install-ps1.test.mjs`, run in the Windows CI lane. The test passes on Windows 11 (ARM64, Windows PowerShell 5.1).
- [ ] Milestone 4: document the Windows install and update path.


## Surprises & Discoveries

- Observation: the updater already anticipates Windows assets. `current_platform()` maps Windows to `x86_64-pc-windows-msvc`, `select_mj_asset` looks for `-x86_64-pc-windows-msvc.zip`, and `stage_release_archive` reads zip archives with the `zip` crate. Only three gates block it: the `cfg!(windows)` early return in `check_prompt_and_apply`, the `ensure!(cfg!(unix))` in `install_release_archive`, and `restart_current_process`, which bails on non-Unix.
  Evidence: `mj-controller/src/controller/update.rs`, functions named above.

- Observation: `publish-npm.yml` asserts the release has exactly six assets (`jq -e 'length == 6'`), and `scripts/release-workflow.test.mjs` builds six fixture assets. Adding the zip without changing these would stop npm publication for every later release.

- Observation: Windows refuses to overwrite or delete a running executable but allows renaming it. `mj-client/src/executable.rs` already relies on this. The daemon runs from the installed `mj.exe` itself (`daemon_launch_executable` in `mj-cli/src/daemon.rs`), so during an upgrade `mj.exe` is typically held by the client doing the upgrade and by the old daemon.

- Observation: the installer test must run PowerShell asynchronously. With `spawnSync` the Node process that also serves the fake release cannot answer, so the installer hangs on its first request.

- Observation: the daemon does not keep the installed worker files open. At startup it copies them into `<data dir>/workers/pinned` (`capture_worker_binary_sources` in `mj-controller/src/controller/worker_binary/binary_source.rs`), so the zip's worker files can be replaced while a daemon runs.


## Decision Log

- Decision: ship only `x86_64-pc-windows-msvc`.
  Rationale: the x64 controller runs natively on x64 Windows and under the built-in x64 emulation of Windows 11 on ARM. Its updater, compiled for x86_64, keeps selecting the x64 asset, so an emulated install stays consistent. No CI lane builds `aarch64-pc-windows-msvc`, and a build that first runs at tag time can block the whole release, because the `release` job needs every package job.
  Date/Author: 2026-10-07, Claude.

- Decision: package the zip on Ubuntu with bash, like the other archives, from a `native-x86_64-pc-windows-msvc` artifact built on `windows-latest`.
  Rationale: `scripts/release-workflow.test.mjs` already executes each package step's bash body against fixtures; a bash step keeps the Windows archive under that same test. `zip` and `shasum` are present on Ubuntu runners, and the sidecar format `<hash>  <name>` is what both the Rust updater and the installer read.
  Date/Author: 2026-10-07, Claude.

- Decision: leave `mj-voice-worker.exe` and the macOS worker out of the Windows bundle.
  Rationale: the issue asks for the Linux workers as the only worker payload. The voice worker has never been built or tested for Windows in CI; `mj` reports dictation as unavailable when the helper is missing (it looks for a sibling `mj-voice-worker`), which is honest. It can be added once a CI lane builds it.
  Date/Author: 2026-10-07, Claude.

- Decision: replace running files on Windows by renaming each installed file aside inside the install directory, then renaming the staged file into place, and deleting the aside copies once nothing runs them.
  Rationale: rename is the only operation Windows permits on a running image, and keeping both renames in one directory keeps them on one volume. The aside names start with `.mj-replaced-` so a later update can sweep them; a file still mapped by a running process (the old daemon until it is replaced) refuses deletion and is retried on the next sweep. If the second rename fails, the aside file is renamed back so the install is never left without the file.
  Date/Author: 2026-10-07, Claude.

- Decision: restart on Windows by running the new `mj.exe` as a child with the same arguments and exiting with its status, the same pattern `continue_under_upgraded_build` in `mj-cli/src/daemon.rs` already uses where `exec` does not exist.
  Rationale: Windows has no `exec`; a child that inherits the console is the closest equivalent and is already proven in this codebase.
  Date/Author: 2026-10-07, Claude.

- Decision: the PowerShell installer requires the checksum sidecar and installs to `%LOCALAPPDATA%\Programs\Mjolnir\bin` by default, adding that directory to the user `Path` unless `MJOLNIR_NO_MODIFY_PATH` is set.
  Rationale: the Rust updater already refuses a release without a sidecar; the installer should not be weaker. A per-user directory needs no elevation and matches where per-user Windows programs install. Its parent directory is not a Cargo root, so the updater detects it as a release-installer ("Direct") install.
  Date/Author: 2026-10-07, Claude.


## Outcomes & Retrospective

Not started.


## Context and Orientation

A release starts when a tag `vX.Y.Z` is pushed. `.github/workflows/release.yml` verifies the version, runs the CI workflow on the tag, and in parallel builds binaries: `build-worker-x64` and `build-worker-arm64` build the static Linux worker (`mj-worker`, built from crate `brokk-mj-worker` for `*-unknown-linux-musl`) and upload artifacts named `mj-worker-<triple>` that each contain one file `mj-worker`; `build-linux`, `build-linux-arm64`, and `build-macos` build `mj`, `mj-desktop`, and the voice worker. Package jobs (`package-x86_64-unknown-linux-gnu`, `package-aarch64-unknown-linux-gnu`, `package-macos`) download those artifacts plus `release-notices` (generated third-party license files) and run a bash step named `Package archive` that creates one top-level directory `brokk-mjolnir-<tag>-<target>/`, copies the binaries and the workers (renamed to `mj-worker-<triple>`), README, LICENSE and `licenses/`, archives it, and writes `<archive>.sha256` with `shasum -a 256`. The `release` job needs CI and every package job, downloads all `archive-*` artifacts, creates a draft release with the listed `files:` globs, publishes it, and dispatches crates.io publication. `.github/workflows/publish-npm.yml` then downloads the release assets for npm and checks their count.

`scripts/release-workflow.test.mjs` (run by the `licenses` job in `.github/workflows/ci.yml` with `node --test`) extracts each `Package archive` bash body from `release.yml` and runs it in a fixture directory, asserting the archive's contents and checksum, and simulates the npm asset download.

The controller locates Linux workers for a container or SSH target by looking for `mj-worker-<triple>` beside its own executable (`select_sibling_worker` in `mj-controller/src/controller/worker_binary/binary_source.rs`), so the zip must place them next to `mj.exe` without an `.exe` suffix. The worker must come from the same commit as the controller (`verify_worker_build` in `mj-core/src/worker_build.rs`); building everything from the tag guarantees that. `mj-desktop.exe` is found the same way, as a sibling with the platform executable suffix (`mj-controller/src/desktop.rs`).

The startup update check lives in `mj-controller/src/controller/update.rs`. `InstallMethod::detect` decides whether `mj` came from npm, npx, Homebrew, Cargo, or a release archive ("Direct"). For Direct installs `check_prompt_and_apply` fetches the latest GitHub release, asks `Upgrade now? [Y/n]`, downloads the archive and its sidecar, verifies SHA-256, calls `install_release_archive` (which stages every binary in a temporary directory beside the executable, then renames each into place, the controller last), and restarts. `mj-cli/src/main.rs` calls it before starting the daemon or the dashboard, only for interactive `mj` and `mj workspaces`.


## Plan of Work

Milestone 1 changes `.github/workflows/release.yml`. Add a job `build-windows` (`needs: verify-version`, `runs-on: windows-latest`) that installs Rust 1.96.0 with the `x86_64-pc-windows-msvc` target, runs `cargo build --release --locked -p brokk-mjolnir -p brokk-mj-desktop --target x86_64-pc-windows-msvc`, copies `mj.exe` and `mj-desktop.exe` from `target/x86_64-pc-windows-msvc/release/` into `native/`, and uploads `native-x86_64-pc-windows-msvc`. Add `package-x86_64-pc-windows-msvc` on `ubuntu-latest`, needing `build-windows`, both worker jobs, and `release-notices`, whose `Package archive` bash step builds `brokk-mjolnir-${GITHUB_REF_NAME}-x86_64-pc-windows-msvc/` with `mj.exe`, `mj-desktop.exe`, the two Linux workers, README, LICENSE, and the same `licenses/` content, then runs `zip -q -r -X "$name.zip" "$name"` and `shasum -a 256 "$name.zip" > "$name.zip.sha256"`, uploading `archive-x86_64-pc-windows-msvc`. The `release` job gains that package job in `needs` and the globs `artifacts/brokk-mjolnir-*.zip` and `artifacts/brokk-mjolnir-*.zip.sha256`. In `publish-npm.yml` the asset-count assertion becomes 8. In `scripts/release-workflow.test.mjs` the archive test loop gains the Windows target (asserting the zip's listing and checksum instead of tar modes) and the npm fixture gains the two Windows assets.

Milestone 2 changes `mj-controller/src/controller/update.rs`. Remove the `cfg!(windows)` early return. Remove the Unix-only `ensure!` in `install_release_archive` and route each final rename through one function that on Unix is `std::fs::rename` and on Windows renames an existing target aside to a unique `.mj-replaced-<name>-<random>` file in the same directory, renames the staged file into place, restores the aside file if that fails, and then tries to delete the aside file. Before staging, sweep `.mj-replaced-*` leftovers from earlier updates, ignoring files Windows still reports as in use. Give `restart_current_process` a Windows branch that runs the target with the original arguments through `mj_core::subprocess::run_interactive` and exits with its code. Change Cargo detection to look for the binary name with `std::env::consts::EXE_SUFFIX`, because Cargo records `mj.exe` on Windows; without that a Cargo-installed `mj.exe` would be treated as a release install and overwritten by the release bundle. Add a Windows test that installs a zip over an executable that is running and checks the replacement, plus the existing failure cases on a zip.

Milestone 3 adds `install.ps1`: Windows PowerShell 5.1 compatible, strict mode, `$ErrorActionPreference = 'Stop'`. It reads `MJOLNIR_VERSION` (tag, default latest), `MJOLNIR_INSTALL_DIR`, `MJOLNIR_GITHUB_OWNER` (default `BrokkAi`), `MJOLNIR_GITHUB_API` (default `https://api.github.com`, so tests and mirrors can serve release metadata), `GITHUB_TOKEN`, and `MJOLNIR_NO_MODIFY_PATH`. It finds the `-x86_64-pc-windows-msvc.zip` asset and its sidecar, downloads both into a temporary directory, verifies the hash, extracts, runs the candidate `mj.exe --version`, and installs every file with the same rename-aside scheme as the updater, controller last, then updates the user `Path`. `scripts/install-ps1.test.mjs` serves a fake release from a loopback HTTP server, builds the zip with `Compress-Archive` using a copy of `node.exe` as `mj.exe` (it answers `--version`), and checks a fresh install, an upgrade while the installed `mj.exe` is running, and that a checksum mismatch leaves the installed files unchanged. The Windows CI lane runs it with `node --test`.

Milestone 4 updates `docs/src/content/docs/install.md` (a Windows section, the update bullet, removing the "no native Windows controller" sentences while stating that a Windows controller runs Docker Desktop and SSH targets, not local bare sessions), `README.md`, `docs/src/content/docs/overview.md`, and `CONTRIBUTING.md` where they say Windows is unsupported.


## Concrete Steps

All commands run from the repository root, `C:\Users\ryansvihla\code\mjolnir` on the development machine.

    node --test scripts/release-workflow.test.mjs        (on Linux or macOS; needs bash, zip, shasum)
    cargo test -p brokk-mj-controller controller::update
    cargo clippy --all-targets -- -D warnings
    node --test scripts/install-ps1.test.mjs             (on Windows)


## Validation and Acceptance

The release workflow test passes for all four targets, and the Windows case shows a zip whose single top-level directory holds `mj.exe`, `mj-desktop.exe`, both Linux workers, and the notices, with a sidecar that `shasum -c` accepts. The updater tests pass on Windows, including replacement of a running executable. The installer test passes on Windows. The first tagged release after this change lists eight assets, npm publication succeeds, and on a Windows machine `install.ps1` installs a working `mj` that starts its daemon.


## Idempotence and Recovery

Re-running the installer reinstalls the selected release; files are replaced by rename, so an interrupted run leaves either the old or the new file under each name, plus at most a `.mj-replaced-*` leftover that the next run or update removes. A failed release build changes nothing published: the release job only runs when every package job succeeds.


## Artifacts and Notes

None yet.


## Interfaces and Dependencies

No new crates. `install_release_archive(current_exe: &Path, archive_name: &str, archive_bytes: &[u8]) -> Result<PathBuf>` keeps its signature. New private helper in `update.rs`:

    fn replace_installed_file(staged: &Path, target: &Path) -> Result<()>
