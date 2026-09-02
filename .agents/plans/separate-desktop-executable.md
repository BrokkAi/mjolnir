# Separate the desktop shell from the headless `mj` executable

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

After this change, the `mj` controller and worker executable has no GTK, WebKitGTK, WRY, or native desktop-shell dependency on any target. A separate `mj-desktop` executable owns the native window and its local TLS proxy. Users can still run `mj app`; it locates and runs `mj-desktop` beside `mj`. Users can also launch `mj-desktop` directly. Release archives, the shell installer, and npm platform packages install the two executables together on supported desktop platforms.

The separation is observable in three ways. `cargo tree -p brokk-mjolnir --target x86_64-unknown-linux-gnu` contains no `brokk-mj-desktop`, `wry`, `webkit2gtk`, `gtk`, or `soup3`. `cargo check -p brokk-mj-desktop --target x86_64-unknown-linux-musl` completes without trying to find GTK system libraries and the resulting unsupported helper exits with a clear platform error. A GNU/Linux desktop build produces sibling `mj` and `mj-desktop` executables, and `mj app` starts the helper while the helper obtains its viewer bootstrap from `mj` without placing the signed viewer cookie in command-line arguments or environment variables.

## Progress

- [x] (2026-09-02 09:34Z) Inspected the CLI feature graph, desktop crate target conditions, daemon startup/status path, subprocess helpers, and release/install/npm packaging surfaces.
- [x] (2026-09-02 09:34Z) Chose a separate-executable protocol that preserves direct `mj-desktop` launch and keeps credentials in an anonymous process pipe.
- [x] (2026-09-02 09:43Z) Added a versioned, cookie-redacting desktop bootstrap payload plus shared companion-path and inherited-I/O subprocess helpers with unit tests.
- [x] (2026-09-02 09:47Z) Moved proxy ownership into the `mj-desktop` executable and restricted the desktop library and native dependencies to macOS, Windows, or GNU/Linux.
- [x] (2026-09-02 09:47Z) Replaced the CLI's linked desktop feature with an always-visible launcher and hidden bootstrap command.
- [x] (2026-09-02 09:51Z) Updated Cargo metadata, documentation, CI, release archives, installer, npm packaging, crates.io publishing, and release guidance for the sibling executable.
- [x] (2026-09-02 09:53Z) Updated `Cargo.lock`; regenerated and compared both license reports, which required no checked-in report changes.
- [x] (2026-09-02 10:05Z) Validated formatting, target dependency isolation, musl and GNU checks, focused behavior, the full test suite, Clippy, package assembly, npm tests, YAML/shell/JavaScript syntax, license policy, and release-version consistency.
- [x] (2026-09-02 10:11Z) Committed the validated implementation as `65ff6f6b` and pushed the current `master` branch to `origin/master`.

## Surprises & Discoveries

- Observation: The CLI's existing target-specific optional dependency prevents `mj-desktop` from entering a musl build of `brokk-mjolnir`, but `brokk-mj-desktop` itself enables WRY for every non-Android target and GTK/WebKitGTK for every Linux environment.
  Evidence: `cargo check --locked -p brokk-mjolnir --target x86_64-unknown-linux-musl --all-features` succeeds, while `cargo check --locked -p brokk-mj-desktop --target x86_64-unknown-linux-musl` fails in `gio-sys`, `glib-sys`, `gdk-sys`, and related `pkg-config` probes.

- Observation: A literal `cargo check --workspace` on musl also includes the independently packaged voice worker and fails first on ALSA. The supported headless contract must therefore name `brokk-mjolnir` rather than imply that every workspace package is native-library-free.
  Evidence: `cargo check --locked --workspace --target x86_64-unknown-linux-musl` fails in `alsa-sys` from `brokk-mj-voice-worker` before reaching the desktop package.

- Observation: The existing desktop bootstrap is coupled to private CLI daemon types only to start the daemon and read its viewer status. Moving the whole daemon client into the core would be a much larger and riskier refactor than a narrow child-process bootstrap protocol.
  Evidence: `mj-cli/src/desktop.rs` calls `daemon::connect_or_start()` and matches `WebViewerStatus`; `mj-cli/src/daemon.rs` is several thousand lines and also owns all session-control actions.

- Observation: `cargo package --workspace` verification inherited the repository's musl target and failed on the independently packaged ALSA voice worker. Extracted packages also resolve same-version dependencies from crates.io, so a newly added core API cannot be verified from a dependent extracted package until core is published.
  Evidence: the first package verification failed in `alsa-sys`; a GNU-target retry reached `brokk-mj-desktop` but selected registry `brokk-mj-core 2.0.0`, whose checksum and missing `hel_desktop` module proved it was not the workspace source.

- Observation: Moving dependencies between workspace members did not change the workspace-wide license inventory.
  Evidence: freshly generated `THIRD_PARTY_LICENSES.html` and `SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt` both diffed cleanly against the checked-in reports, and `cargo deny ... check licenses` reported `licenses ok`.

## Decision Log

- Decision: Promote the existing `brokk-mj-desktop` package to produce an `mj-desktop` binary instead of creating another workspace crate.
  Rationale: The existing package is already the compilation, publication, and ownership boundary for native WebView code. A new crate would only reorganize code and would violate the repository guidance against unnecessary workspace crates.
  Date/Author: 2026-09-02 / Codex

- Decision: Keep `mj app` as the stable user-facing command and also support running `mj-desktop` directly.
  Rationale: Existing users retain their command, desktop launchers can target the dedicated executable, and headless builds expose a useful error when the helper is absent instead of hiding the command at compile time.
  Date/Author: 2026-09-02 / Codex

- Decision: Have `mj-desktop` invoke a hidden `mj desktop-bootstrap` command and read a JSON payload from stdout; never pass the signed viewer cookie in argv or the environment.
  Rationale: `mj` remains the sole owner of daemon compatibility/startup logic and cookie minting. The pipe is inherited only by the child and the existing shared subprocess helper drains output while handling stdin safely. This avoids duplicating daemon wire formats or exposing a credential in process listings.
  Date/Author: 2026-09-02 / Codex

- Decision: Move certificate generation, the loopback TLS proxy, and WebView lifecycle coordination into `mj-desktop`.
  Rationale: Those facilities exist only to support the desktop window. Keeping any of their dependencies in `brokk-mjolnir` would weaken the desired headless dependency boundary.
  Date/Author: 2026-09-02 / Codex

- Decision: Preserve the existing release archive names and add `mj-desktop` as a sibling companion rather than inventing a new distribution channel in this change.
  Rationale: The current GNU/Linux and macOS archives already carried the desktop-linked `mj`; adding a separate executable preserves supported platforms while allowing the main executable to run without loading desktop libraries. The installer and npm packager can copy one additional companion without changing their public package names.
  Date/Author: 2026-09-02 / Codex

- Decision: Version the JSON launch document at protocol version 1 and reject unknown fields or incompatible versions while redacting the cookie from `Debug` and parse errors.
  Rationale: The two crates can be installed independently from crates.io. An explicit version turns mismatched installations into a useful compatibility error rather than an opaque viewer failure, without exposing the credential.
  Date/Author: 2026-09-02 / Codex

- Decision: Assemble source packages with `cargo package --workspace --no-verify`, validate the live workspace explicitly on GNU/Linux, and make both package and publish jobs override the repository's musl target with GNU/Linux.
  Rationale: Package extraction cannot use unpublished same-release path dependencies, while `cargo publish` can verify each dependent after the ordered loop has published its prerequisites. GNU targeting also lets the already-installed ALSA and WebKitGTK libraries validate their intended packages.
  Date/Author: 2026-09-02 / Codex

## Outcomes & Retrospective

The implementation now produces two process boundaries with the intended runtime behavior. `mj` always exposes `app`, supervises `mj-desktop` from a blocking-work task, and owns daemon startup plus cookie minting. `mj-desktop` can run directly, obtains its versioned launch document through captured stdout from a hidden controller command, and owns certificate generation, proxying, and the native WebView. The cookie never enters argv, environment variables, logs, or debug formatting.

Both musl and GNU dependency-tree probes show no desktop package or native GUI crates beneath `brokk-mjolnir`. A release GNU build's `mj` has no WebKitGTK/GTK/Soup/GIO dynamic entries, while its sibling `mj-desktop` has exactly those expected GUI dependencies. Direct musl checks of both packages succeed without GTK `pkg-config` probes. The complete default test suite and the GNU desktop tests pass; both Clippy surfaces are warning-free.

Release archives, direct installer, npm packager, CI matrices, and crates.io workflows all understand the companion executable. Linux desktop use still depends on the system WebKitGTK runtime, but a headless user can run the sibling `mj` without loading or installing those libraries when using the static musl artifact.

## Context and Orientation

The workspace root `Cargo.toml` defines `brokk-mj-core` and lists `mj-cli`, `mj-tui`, `mj-desktop`, and `voice-worker` as members. The root `.cargo/config.toml` makes `x86_64-unknown-linux-musl` the default build target because the static `mj` binary is also uploaded into Linux session targets. “musl” is the C library used for that static Linux build. “GNU/Linux” here means a Linux target using glibc, represented by a Rust target whose `target_env` is `gnu`.

`mj-cli/Cargo.toml` now has an empty default feature set and no dependency on `brokk-mj-desktop`, rcgen, or desktop-only proxy crates. `mj-cli/src/desktop.rs` starts or connects to the daemon, mints an authenticated web-viewer cookie for the hidden `desktop-bootstrap` command, and launches a sibling executable for `mj app`. `mj-cli/src/main.rs` compiles `app` on every target, so unsupported or incomplete installations receive an actionable helper error rather than an unknown subcommand.

`mj-desktop/Cargo.toml` now describes both the `mj_desktop` library and the `mj-desktop` executable. `mj-desktop/src/lib.rs` owns the native Tao window and WRY WebView, certificate pinning, cookie installation, navigation policy, and platform-specific WebView hooks. `mj-desktop/src/main.rs` owns the desktop bootstrap child, certificate generation, and loopback proxy. WRY uses WebKitGTK on Linux, so native Linux desktop compilation needs GTK and WebKitGTK headers and the shipped executable needs their shared libraries. A crate-level supported-platform condition and target-specific dependency tables exclude all native GUI code on musl.

`mj-cli/src/daemon.rs` owns the persistent controller daemon and a private client protocol. `connect_or_start()` starts the current `mj` executable with the hidden `daemon-run` command when necessary. Its status reply includes `WebViewerStatus`, which supplies the viewer URL. `src/hel_server.rs` owns the cookie signing key and `mint_desktop_session_cookie()`. `src/hel_subprocess.rs` is the required shared location for child-process pipe handling.

The new narrow protocol will live in `src/hel_desktop.rs` and be exported by `src/lib.rs`. A `DesktopLaunch` value contains the viewer URL and signed session cookie needed by the desktop process. It is serialized as JSON only across a pipe. It is not a network API and contains no certificate because `mj-desktop` generates and owns its ephemeral certificate and proxy.

Release archives are built in `.github/workflows/release.yml`; `install.sh` installs sibling companion executables; `npm/scripts/package-release.mjs` stages archive contents into platform npm packages. `.github/workflows/ci.yml` checks musl core builds and a separate GNU/Linux desktop job. `.github/workflows/publish.yml` verifies source packages before crates.io publishing. `README.md`, `mj-desktop/README.md`, and `RELEASING.md` describe installation and release composition.

## Plan of Work

First add `src/hel_desktop.rs`. Define the serialized `DesktopLaunch` structure, JSON read/write helpers that add useful context without ever formatting the cookie into an error, and a path resolver that finds a named executable beside the current executable while honoring a narrowly named `MJ_DESKTOP_BINARY` or `MJ_CONTROLLER_BINARY` override at each call site. Add unit tests for JSON round trips, malformed input, and platform-safe sibling filename construction. Add a shared inherited-I/O process runner to `src/hel_subprocess.rs` if the `mj app` launcher needs it; keep captured bootstrap output on `run_with_input`, which already handles pipe backpressure safely.

Next change `mj-desktop/Cargo.toml` to declare an `mj-desktop` binary at `src/main.rs`, depend on `brokk-mj-core`, and move all proxy and native-window dependencies into the desktop package. Restrict Tao, WRY, WebKitGTK, GIO, and Soup to macOS, Windows, or GNU/Linux as appropriate. In `mj-desktop/src/lib.rs`, replace every broad non-Android native `cfg` and broad Linux `cfg` with the exact supported-platform expression, leaving platform-independent origin and TLS policy code testable elsewhere. Add `mj-desktop/src/main.rs`: locate sibling `mj`, execute hidden `desktop-bootstrap`, deserialize `DesktopLaunch`, generate the ephemeral certificate, start the proxy on a Tokio worker runtime, and call `mj_desktop::run` on the main thread. On unsupported targets, compile a small main function that exits with a clear message and no native GUI dependency.

Then simplify `mj-cli/Cargo.toml`: remove the `desktop-app` feature, remove the `mj-desktop`, Axum, Reqwest, and rcgen optional dependencies, and keep the normal default feature set empty. Rewrite `mj-cli/src/desktop.rs` as two small responsibilities. `desktop_bootstrap()` connects to or starts the daemon, validates viewer readiness, mints the cookie, and writes `DesktopLaunch` JSON. `run_desktop_app()` locates and supervises the sibling `mj-desktop` executable without blocking the Tokio event loop. In `mj-cli/src/main.rs`, compile the module and visible `App` variant on every target, add a hidden `DesktopBootstrap` variant, and route both unconditionally. Add parsing and dependency-boundary behavior tests.

Update build and distribution surfaces. The GNU/Linux and macOS release jobs must build both `brokk-mjolnir` and `brokk-mj-desktop`, copy `mj-desktop` into each archive, make it executable, and create both universal macOS binaries. The crates.io package verification job must explicitly build the GNU/Linux desktop package rather than relying on a feature that is inert under the repository's default musl target. CI must continue testing the headless packages on musl and explicitly compile/test the desktop package on GNU/Linux, macOS, and Windows. The installer companion list, npm staging script, npm smoke checks, and npm tests must require `mj-desktop`. Documentation must explain that source and crates.io installs need both packages for `mj app`, that plain `mj` remains usable without desktop shared libraries, and that Linux desktop use still requires WebKitGTK at runtime.

Finally regenerate `Cargo.lock` through Cargo metadata/build commands and regenerate license reports only if their checked content changes. Run the repository's required formatting, tests, Clippy, packaging, and release checks. Commit only the files changed for this plan, then push `master` to `origin/master` as explicitly requested.

## Concrete Steps

Work from `/home/ryan/code/mjolnir`.

Implement the core protocol and executable split with `apply_patch`, then format:

    cargo fmt --all

Prove the main package has no desktop graph on both Linux environments:

    cargo tree -p brokk-mjolnir --target x86_64-unknown-linux-musl -e normal
    cargo tree -p brokk-mjolnir --target x86_64-unknown-linux-gnu -e normal

Neither output may contain `brokk-mj-desktop`, `wry`, `webkit2gtk`, `gtk`, or `soup3`.

Compile the unsupported direct desktop package and the normal headless package for musl:

    cargo check --locked -p brokk-mj-desktop --target x86_64-unknown-linux-musl
    cargo check --locked -p brokk-mjolnir --target x86_64-unknown-linux-musl

Compile the native GNU/Linux pair where WebKitGTK development libraries are available:

    cargo check --locked -p brokk-mjolnir -p brokk-mj-desktop --target x86_64-unknown-linux-gnu

Run focused and full tests outside the restricted sandbox because repository tests use loopback and Unix sockets:

    cargo test --locked -p brokk-mjolnir --bin mj
    cargo test --locked -p brokk-mj-desktop
    cargo test --locked

Run all required static validation:

    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo clippy --locked -p brokk-mj-desktop --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
    node --test npm/test/*.test.mjs
    node scripts/release-version.mjs check
    cargo package --locked --allow-dirty --list -p brokk-mj-desktop
    CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu cargo package --locked --workspace --allow-dirty --no-verify --offline

Inspect changes and commit only the files named by this implementation. Then push without changing branches:

    git status --short
    git diff --check
    git add <explicit changed paths>
    git commit -m "Split the desktop shell from mj"
    git push origin master

## Validation and Acceptance

The change is accepted when all of the following observable behavior holds.

Running `mj --help` from a musl or no-desktop build lists `app`. Running `mj app` without an installed helper fails quickly and names the expected `mj-desktop` path plus the installation remedy. It must not report `app` as an unknown command.

Running `mj-desktop --version` on a supported packaged platform prints the workspace version. Running `mj-desktop` beside `mj` starts the daemon if necessary and opens the same authenticated viewer as the former linked `mj app`. Running `mj app` produces the same result by launching that sibling. No process command line or environment contains the signed viewer cookie; it travels only in the captured JSON bootstrap output.

`cargo tree -p brokk-mjolnir` on GNU/Linux and musl contains none of the desktop package or native GUI crates. `cargo check -p brokk-mj-desktop --target x86_64-unknown-linux-musl` no longer invokes a GTK-family `pkg-config` probe. A supported GNU/Linux desktop build still compiles against WebKitGTK.

Each Linux and macOS release archive contains executable `mj`, `mj-desktop`, `mj-voice-worker`, and both static session workers. The installer and npm platform packages preserve all five companion roles, and npm smoke validation asserts that `mj-desktop` is executable.

All required Cargo tests and Clippy commands pass, npm packaging tests pass, release version validation passes, and `git diff --check` reports no whitespace errors.

## Idempotence and Recovery

All Cargo, Node test, metadata, formatting, and package-list commands are safe to repeat. The implementation changes no user data or daemon storage format. A failed `mj-desktop` launch leaves the persistent daemon running, matching existing `mj app` behavior, and the ephemeral proxy dies with the desktop process.

If a build fails midway, retain the working tree and resume from the failing command after updating this plan's `Progress` and `Surprises & Discoveries`. Do not delete `target`; Cargo can reuse valid artifacts. If release packaging validation exposes another inventory consumer, update that consumer and record it here rather than removing the new companion from archives.

The final push is retriable with `git push origin master` after confirming the local commit and remote tracking state. Do not rebase, reset, create a branch, or force-push.

## Artifacts and Notes

The post-change dependency evidence is:

    $ cargo tree -p brokk-mjolnir --target x86_64-unknown-linux-gnu -e normal | rg 'brokk-mj-desktop|wry|webkit|gtk|soup'
    <no output>

    $ cargo tree -p brokk-mjolnir --target x86_64-unknown-linux-musl -e normal | rg 'brokk-mj-desktop|wry|webkit|gtk|soup'
    <no output>

    $ cargo tree -p brokk-mj-desktop --target x86_64-unknown-linux-gnu -e normal | rg 'brokk-mj-desktop|wry|webkit|gtk|soup|tao|rcgen'
    brokk-mj-desktop ...
    rcgen ...
    soup3 ...
    tao ... gtk ...
    webkit2gtk ...
    wry ...

Both musl checks and the GNU pair completed:

    Finished `dev` profile ... brokk-mjolnir ... x86_64-unknown-linux-musl
    Finished `dev` profile ... brokk-mj-desktop ... x86_64-unknown-linux-musl
    Finished `dev` profile ... brokk-mjolnir, brokk-mj-desktop ... x86_64-unknown-linux-gnu

The release binaries put GUI shared libraries only on the desktop process:

    $ ldd target/x86_64-unknown-linux-gnu/release/mj | rg -i 'webkit|gtk|soup|gio|gdk'
    <no output>

    $ ldd target/x86_64-unknown-linux-gnu/release/mj-desktop | rg -i 'webkit|gtk|soup|gio|gdk'
    libwebkit2gtk-4.1.so.0 ...
    libgtk-3.so.0 ...
    libgdk-3.so.0 ...
    libsoup-3.0.so.0 ...
    libgio-2.0.so.0 ...

Focused validation reported 108 passing CLI tests and six passing desktop tests. The full default suite passed, including 1,638 core tests, 270 TUI tests with one timing test ignored, 108 CLI tests, and the logging, store-divergence, and PTY integration tests. Both Clippy commands completed without warnings.

## Interfaces and Dependencies

In `src/hel_desktop.rs`, define a public serializable launch payload with private-sensitive formatting behavior:

    pub struct DesktopLaunch {
        protocol_version: u32,
        pub viewer_url: String,
        pub bootstrap_cookie_value: String,
    }

`protocol_version` is constructed as `DESKTOP_LAUNCH_PROTOCOL_VERSION`, currently 1. Serialization and deserialization use JSON byte slices. Errors may name the field, malformed JSON, or an incompatible version but must never include `bootstrap_cookie_value`.

In `mj-cli/src/desktop.rs`, retain:

    pub(crate) async fn run_desktop_app() -> anyhow::Result<()>;
    pub(crate) async fn desktop_bootstrap() -> anyhow::Result<()>;

The first launches `mj-desktop` in `tokio::task::spawn_blocking`. The second is the only CLI path that calls `daemon::connect_or_start`, obtains `WebViewerStatus`, loads the cookie key, and calls `mint_desktop_session_cookie`.

In `mj-desktop/src/main.rs`, the supported-platform path obtains `DesktopLaunch` by running sibling `mj desktop-bootstrap`, then starts the loopback proxy and calls the existing library interface:

    pub fn run(
        options: DesktopShellOptions,
        on_ready: impl FnOnce(DesktopShellRemote),
    ) -> anyhow::Result<DesktopShellExit>;

The desktop process owns `axum`, `reqwest`, `rcgen`, WRY, Tao, and GTK/WebKit dependencies. `brokk-mjolnir` owns none of them solely for desktop launch. Shared `axum` or `reqwest` dependencies already needed by the daemon core are not themselves a failure; the acceptance check is that `brokk-mj-desktop`, WRY, Tao, GTK, WebKitGTK, Soup, GIO, and rcgen do not enter `brokk-mjolnir` through the desktop path.

Plan revision note (2026-09-02 09:34Z): Created the plan after the user expanded the approved headless feature correction into a complete separate-executable boundary, including launch protocol and all build, release, installer, and npm consumers.

Plan revision note (2026-09-02 10:05Z): Recorded the completed implementation and validation evidence, added the versioned credential protocol decision, and corrected crates.io package/publish targeting after package verification exposed musl and unpublished-sibling resolution failures.

Plan revision note (2026-09-02 10:11Z): Recorded the implementation commit and confirmed it reached `origin/master`; this plan-only completion checkpoint follows it so the living plan reflects the repository's final state.
