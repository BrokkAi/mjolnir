# Close the Mjolnir 2.0 transfer issues and restore green master

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

The canonical rules for ExecPlans are in `.agents/PLANS.md`. This document must be maintained in accordance with that file.

## Purpose / Big Picture

Mjolnir 2.0 must ship as Mjolnir everywhere a user, agent, release consumer, registry, browser, or operator can observe, while retaining the internal Rust `hel` crate and module names and any target-side compatibility contracts that are deliberately outside these issues. The release workflows must be able to package the entire workspace, and the deterministic reliability laboratories must follow the current keyboard interface instead of the retired one. When this work is complete, issues #949 through #964 have focused fixes, the current master reliability failures reproduce green locally, release packaging verifies all crates on the supported host target, and the repository's full formatting, test, Clippy, release-build, web, npm, and legal-notice checks pass.

## Progress

- [x] (2026-09-02 08:24Z) Audited master, created issues #949 through #964, inspected the failed GitHub Actions jobs, and read `.agents/PLANS.md` in full.
- [x] (2026-09-02 08:35Z) Repaired the deterministic reliability drivers, exposed and fixed the stale daemon protocol in the configuration-rename hook, and passed all three focused reproductions.
- [ ] Fix the user-visible runtime identity issues in small coherent checkpoints.
- [ ] Fix release, packaging, web-cache, agent-image, RunsOn, workflow, import-E2E, voice-cache, and generated-notice identities.
- [ ] Run every required validation, update this plan with evidence, and commit the final validated checkpoints.

## Surprises & Discoveries

- Observation: The ordinary Rust, coverage, cross-platform, desktop, voice, and license jobs pass at `71bdd80d`, while the master CI failure is isolated to the deterministic three-client reliability smoke.
  Evidence: GitHub Actions run 33587052527 reports only `Three-client reliability smoke` failed; the failure says `tui-1 did not quit within two seconds`.

- Observation: Two scheduled reliability failures are direct test-driver drift after the hotkey refactor. `TerminalClient.quit` still sends Ctrl-Q although the UI moved detach to Alt-Q, and `browser_lab.py` still recognizes the old pane footer and opens the deleted edit-session dialog with `e`.
  Evidence: `tests/e2e/reliability_lab.py::TerminalClient.quit` sends byte `0x11`; `tests/e2e/browser_lab.py` expects `Enter open · n new · s resume · e edit`; `mj-tui/src/actions.rs` declares Alt-Q and exposes Stop only through the F2 palette.

- Observation: `cargo package --locked --workspace` inherits the repository's musl default and verifies the desktop GTK dependency graph for musl.
  Evidence: a local package run reached `brokk-mj-desktop` and failed in `glib-sys` and `pango-sys`; `.cargo/config.toml` sets `build.target = x86_64-unknown-linux-musl`, while `publish.yml` supplies no `--target`.

- Observation: The configuration-rename crash hook never ran because the Python reliability client sent daemon protocol version 4 to a version 5 daemon; its background-thread failure was not inspected until after the harness had already timed out waiting for the hook marker.
  Evidence: the focused scenario reported `incompatible daemon protocol 4; expected 5` after the harness was changed to fail when the request completes before its hook. Reading `protocol_version` from `daemon.json` made the scenario pass without changing its timeout.

- Observation: The command-palette overlay does not render a `Command palette` title at the tested terminal size, but it consistently renders its interaction hint.
  Evidence: the first corrected browser run showed the palette commands and `type to filter · Up/Down · Enter runs · Esc closes`, then timed out waiting for the nonexistent title. Waiting on the interaction hint made the full browser/TUI scenario pass.

## Decision Log

- Decision: Treat the task as issues #949 through #964 plus the already-requested current-master failures, not every historical repository issue.
  Rationale: Those are the issues created from the release audit immediately before the user asked to fix all of them; expanding to unrelated historical issues would be unbounded scope creep.
  Date/Author: 2026-09-02, Codex.

- Decision: Preserve the internal `hel` Rust library, module, and type names and do not rename target-side executable or filesystem contracts unless an issue explicitly requires it.
  Rationale: `Cargo.toml` documents the internal crate boundary, and several small issues explicitly limit themselves to public wording so they can ship independently without a target-state migration.
  Date/Author: 2026-09-02, Codex.

- Decision: Make release packaging pass an explicit GNU host target rather than changing the repository-wide musl default.
  Rationale: Mjolnir deliberately defaults normal development to the static worker target. Only package verification needs the desktop-capable host target.
  Date/Author: 2026-09-02, Codex.

- Decision: Use the command palette to stop a session in the browser/TUI reliability laboratory.
  Rationale: Stop has no direct key after the edit dialog was retired; the F2 palette is now the advertised and registry-backed interface, so exercising it is a behavior test rather than reaching around the UI.
  Date/Author: 2026-09-02, Codex.

- Decision: Derive reliability daemon request protocol versions from daemon metadata and fail immediately when a background request completes before its expected crash hook.
  Rationale: The metadata is the daemon's current protocol contract. A hardcoded copy drifted silently, and surfacing the request outcome preserves the causal error instead of reporting a misleading hook timeout.
  Date/Author: 2026-09-02, Codex.

## Outcomes & Retrospective

Work is in progress. No issue is complete until its behavior is validated and committed.

## Context and Orientation

The repository is a Rust workspace. The root package in `Cargo.toml` publishes `brokk-mj-core` but intentionally exposes the Rust library name `hel`; `mj-cli/` builds the public `mj` executable, `mj-tui/` owns the dashboard state machine and command registry, `voice-worker/` builds the local speech sidecar, and `src/` contains the shared runtime implementation. Product-facing files include `README.md`, `install.sh`, `licenses/`, `containers/`, `src/web/`, `scripts/`, and `.github/workflows/`.

The reliability laboratory in `tests/e2e/reliability_lab.py` starts isolated real daemons and terminal clients under pseudo-terminals. `tests/e2e/browser_lab.py` combines a real TUI with Playwright. `tests/e2e/test_hook_chaos.py` activates compile-time crash hooks through `MJ_TEST_HOOK`; the hook writes a marker and blocks until the test kills its owning process. These tests are allowed to use processes, loopback sockets, and Unix sockets only outside the restricted sandbox.

Issues #949 through #964 divide the transfer cleanup into small surfaces: CLI wording, logging, import E2E, crates packaging, EC2 instructions, target diagnostics, bridge bootstrap errors, transcript truncation markers, voice cache, worker diagnostics, agent image metadata, checkpoint/resume diagnostics, web cache identity, supplemental notices, workflow presentation, and the RunsOn helper. The current master failures are separate reliability-driver and crash-hook work and must not be hidden by weakening timeouts or removing coverage.

## Plan of Work

First update the reliability clients to speak the current command contract. Encode Alt-Q as the terminal escape sequence followed by `q`. In `browser_lab.py`, identify the Sessions pane from the current contextual footer, open F2, select `Stop session`, and retain the existing confirmation and retry behavior. Reproduce the three-client and browser scenarios outside the sandbox. Reproduce the one failing configuration-rename crash hook by itself, inspect its retained artifacts, and fix the cause rather than increasing its timeout.

Next fix runtime identity in narrow file groups matching #949, #950, and #953 through #960. Change only strings users or agents observe: CLI import and doctor output; log names and diagnostics; ACP bridge bootstrap errors; disposable EC2 guidance; target, worker, checkpoint, and resume error context; and transcript truncation markers. Add or update colocated behavior tests. Move voice models to the Mjolnir cache namespace and use a Mjolnir downloader user agent without reading Hel state as fallback.

Then fix distribution and operator surfaces matching #951, #952, and #959 through #964. Repair all four ignored import runners and the Rust binary fallback. Pass the native GNU target to workspace package verification. Change OCI metadata and workflow descriptions, give the PWA a fresh Mjolnir cache namespace, update the RunsOn helper's resource/config identities, and rename stale workflow presentation and cache/temp keys without changing the jobs themselves. Update the supplemental-notice generator and regenerate its checked-in output.

Commit after each coherent, validated milestone. Reference the relevant issues in commit trailers so pushing the commits later can close the issues automatically. Do not push from this plan because the repository instructions require explicit push authorization.

## Concrete Steps

All commands run from `/home/ryan/code/mjolnir`.

For reliability work, build the test-hook binary and run focused real-process scenarios outside the sandbox:

    cargo build --locked -p brokk-mjolnir --features test-hooks
    MJ_CHAOS_ISOLATED=1 tests/e2e/run-reliability.sh --scenario multi-client-happy-path --seed 1 ./target/x86_64-unknown-linux-musl/debug/mj
    MJ_CHAOS_ISOLATED=1 tests/e2e/run-test-hook-chaos.sh ./target/x86_64-unknown-linux-musl/debug/mj --hook config_replacement_before_reference_migration --seed 700003

Run the browser scenario when its Playwright dependencies are present:

    tests/e2e/run-browser-reliability.sh --seed 700001 ./target/x86_64-unknown-linux-musl/debug/mj

For focused code, use package/module tests as changes land, including:

    cargo test --locked -p brokk-mjolnir logging
    cargo test --locked --lib hel_worker::snapshot
    cargo test --locked --lib hel_controller::worker_binary
    cargo test --locked --lib hel_targets
    cargo test --locked -p brokk-mj-voice-worker
    node --test tests/e2e/web/viewer.unit.test.mjs
    npm test --prefix npm

For packaging, run with the same explicit target the workflow gains:

    cargo package --locked --workspace --target x86_64-unknown-linux-gnu

If ignored local build/documentation artifacts make Cargo report a dirty tree, add `--allow-dirty` only for local verification; the clean GitHub runner must use the command without it.

At the end run:

    cargo fmt --all -- --check
    cargo test --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo build --release --locked
    node scripts/release-version.mjs check v2.0.0
    node scripts/generate-supplemental-third-party-notices.mjs /tmp/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt
    diff -u licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt /tmp/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt
    git diff --check
    git status --short --branch

## Validation and Acceptance

The master fix is accepted when the deterministic three-client scenario exits both TUIs inside two seconds using Alt-Q, the browser scenario reaches Sessions through the current footer and stops through the F2 command palette, and every crash hook including configuration rename completes without a leaked process. No timeout may simply be increased to turn these green.

Each identity issue is accepted when its focused test observes Mjolnir in new user-visible output and rejects obsolete Hel wording in that surface. Internal Rust paths such as `src/hel_*`, types such as `HelConfig`, and the `[lib] name = "hel"` declaration remain. Release packaging is accepted when every workspace crate packages and verifies for `x86_64-unknown-linux-gnu` while ordinary builds still default to musl. The supplemental notice generator must reproduce the checked-in file exactly.

The entire task is accepted only after the complete required Rust suite, warnings-denied Clippy, optimized build, JavaScript and npm tests, release-version check, and package verification pass. The working tree must contain only the intended commits and this updated ExecPlan.

## Idempotence and Recovery

All tests and generators are repeatable. Reliability tests create isolated temporary roots and preserve failure artifacts under `target/reliability-artifacts`; rerunning with the recorded seed is safe. Package artifacts remain under `target/package` and are ignored. Notice generation to `/tmp` is non-destructive until an intentional `apply_patch` updates the checked-in report. If a checkpoint fails, inspect and commit only files changed for that checkpoint; never reset unrelated user work.

## Artifacts and Notes

Initial master failure evidence:

    three-client smoke: reliability: failed: tui-1 did not quit within two seconds
    scheduled soak: reliability: failed: tui-1 did not quit within two seconds
    browser convergence: the keyboard never reached the Sessions pane
    crash hook: timed out waiting for test hook config_replacement_before_reference_migration

Initial release-package evidence:

    Packaging brokk-mj-desktop v2.0.0
    error: failed to run custom build command for glib-sys v0.18.1
    warning: pkg-config has not been configured to support cross-compilation

## Interfaces and Dependencies

No public Rust API is planned. `mj-tui/src/actions.rs::COMMANDS` remains the source of truth for dashboard commands, and the reliability test must drive that interface through terminal bytes. `src/hel_test_hooks.rs::reach_test_hook` remains the crash-boundary mechanism. `.cargo/config.toml` retains the musl default; `.github/workflows/publish.yml` alone supplies the GNU package-verification target. Generated supplemental notices remain owned by `scripts/generate-supplemental-third-party-notices.mjs`.

Revision note (2026-09-02 08:24Z): Created the plan from the current issue set, master CI evidence, release audit, and repository planning standard.

Revision note (2026-09-02 08:35Z): Recorded the repaired hotkey, browser-palette, and daemon-protocol reliability contracts and their passing focused scenarios.
