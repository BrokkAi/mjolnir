# Make managed ACP harnesses usable from a clean machine

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

A technically capable person who has installed Mjolnir but has never used a coding agent should be able to open the dashboard, choose one of Mjolnir's managed ACP harnesses, authenticate, and reach a usable session without first learning how to install Node.js, npm, npx, a harness CLI, or a package manager toolchain. Existing installations remain authoritative: if Mjolnir finds already-installed and configured agent homes, it continues to offer those rather than replacing them.

The managed path must also respect where an agent runs. A person may be willing to run a harness inside a local Podman, Docker, or Apple container while declining to run it directly on their Mac. Managed setup therefore creates an account profile, not an implicit session, and leaves the session wizard's target choice in control. The onboarding copy recommends an available container when that is the safer default for the selected harness.

After this work, the default local workflow should have one user-facing bootstrap boundary: installing Mjolnir itself. Once `mj` exists, Mjolnir owns harness and Node acquisition, records clear progress and failures, and does not require the user to execute `curl`, Bash vendor installers, or external `tar` during harness setup. Operating-system facilities such as a POSIX shell, libc, DNS, TLS, and network access are assumed. SSH targets still require an SSH server, and container targets still require their container engine because those are target infrastructure rather than harness prerequisites.

## Progress

- [x] (2026-10-25 09:30Z) Audited existing managed harness, Node, login, target, and Git bootstrap paths.
- [x] (2026-10-25 09:30Z) Confirmed that DSH and Muse npx calls use the leased managed Node installation and are not external software prerequisites.
- [x] (2026-10-25 09:30Z) Confirmed that Podman, Docker, Apple container, SSH Podman, and SSH Docker provisioning already install `git` and `gh` through the image package manager when missing, subject to root or passwordless sudo.
- [x] (2026-10-25 09:48Z) Make managed Node cache probing recoverable, detect incompatible glibc artifacts before download, and report failed system probes.
- [x] (2026-10-25 09:48Z) Add managed Node mirror support with checksum verification and tests.
- [x] (2026-10-25 10:12Z) Use Mjolnir's bounded HTTPS client for verified Node, Muse, and OpenCode artifact downloads.
- [ ] Replace Kimi and Grok vendor shell installers with direct pinned artifacts.
- [x] (2026-10-25 10:12Z) Replace the external OpenCode `tar` call with the Rust archive reader already used for Node.
- [ ] Run Claude login and setup-token through the pinned Claude CLI.
- [ ] Add a managed-agent onboarding action that creates a non-own profile home, saves it, and hands off to sign-in.
- [ ] Offer existing installed profiles first and keep manual/advanced profile creation available.
- [ ] Make onboarding explain target choice and recommend an available container for harnesses the user does not want on the host.
- [ ] Add clean-machine integration coverage for managed profile creation and harness installation.
- [ ] Update human documentation and diagnostics.
- [ ] Run affected crate suites, workspace Clippy, and formatting; commit and push validated slices.

## Surprises & Discoveries

- Observation: `npx` is shipped inside the managed Node distribution and is leased with it. DSH and Muse use that npx after `prepare_harness_launch` and `discover_profile_config` call `ensure_node_runtime`.
  Evidence: `mj-worker/src/worker_runtime/harness_launch.rs:53` and `mj-worker/src/worker_runtime/discovery.rs:21` prepare Node before either launch path; `mj-core/src/harness_install.rs:923` verifies bundled `npx`.
- Observation: Container Git is already implicit. All local and SSH container provisioning plans prepend `install_git_plan`, including Apple container.
  Evidence: `mj-controller/src/targets/provision.rs:86`, `:106`, `:133`, and `:176`; the managed-container branch installs `git` and `gh` through apt, dnf, yum, or apk in `mj-controller/src/targets/bootstrap.rs:82`.
- Observation: The lowest-level harness installer still launches external `curl` for Node, Muse, OpenCode, Kimi, Grok, and installer scripts, external Bash for Kimi and Grok, and external `tar` for OpenCode.
  Evidence: `mj-core/src/harness_install.rs:666`, `:987`, `:1011`, and `:1042`.
- Observation: Managed Node repairs currently stop before the repair branch when an installed `node` cannot be executed because `managed_node_is_usable` propagates the probe error.
  Evidence: `mj-core/src/harness_install.rs:769` and `:917`.
- Observation: The pinned Linux Node archives are glibc builds, while the worker itself is also shipped as a static musl executable for custom Linux containers.
  Evidence: `mj-core/assets/node/runtime.json`; `docs/src/content/docs/custom-images.md`.
- Observation: Claude's managed package installs `node_modules/.bin/claude`, but interactive login and setup-token still execute `claude` from `PATH`.
  Evidence: `mj-core/src/config/harness.rs:1070`; `mj-cli/src/main.rs:1922`; `mj-cli/src/main.rs:2045`.
- Observation: Adding a profile currently starts with an empty home and `codex` as the kind. A new user must know the harness, choose a home, save, select the row, and then sign in.
  Evidence: `mj-tui/src/setup/schema.rs:41`; `mj-tui/src/setup.rs:1334`; `mj-tui/src/setup.rs:2688`.
- Observation: First-run discovery only creates profiles for installed agents. With none, it tells the person to add a profile manually.
  Evidence: `mj-tui/src/setup.rs:3250`.

## Decision Log

- Decision: Treat installation of `mj` itself as the unavoidable bootstrap boundary.
  Rationale: No executable can install itself before it exists. The guarantee starts after the user obtains Mjolnir through the release installer, npm, Homebrew, or a direct archive.
  Date/Author: 2026-10-25, Codex
- Decision: Do not classify npx as an external prerequisite.
  Rationale: Every managed Node distribution Mjolnir accepts or installs provides node, npm, and npx together; the selected bin directory is carried through the ACP launch environment.
  Date/Author: 2026-10-25, Codex
- Decision: Keep installed-agent discovery ahead of managed onboarding.
  Rationale: Existing configured homes and logins are the least surprising and least disruptive source of profiles.
  Date/Author: 2026-10-25, Codex
- Decision: Managed onboarding creates an isolated Mjolnir-owned profile home rather than the harness's conventional user home.
  Rationale: OpenCode and Muse deliberately avoid using the pinned binary against a user-owned database, and a fresh home lets Mjolnir own migration and login state safely. Existing detected homes remain untouched.
  Date/Author: 2026-10-25, Codex
- Decision: Managed setup stops at a configured, authenticated profile; it does not create a session or choose a target permanently.
  Rationale: Profiles are account identity, while targets belong to individual sessions. The session wizard can recommend a container for the selected harness without silently moving existing workflows onto one.
  Date/Author: 2026-10-25, Codex
- Decision: Preserve plain local Git-free sessions as a separate prerequisite reduction, landed through PR #1351 or equivalent changes.
  Rationale: Git-free local directories solve a different user need from managed harness installation and should remain independently testable.
  Date/Author: 2026-10-25, Codex

## Outcomes & Retrospective

No implementation has landed yet. The work begins with correctness fixes in the managed Node layer because onboarding depends on that layer being repairable and honest about unsupported platforms.

## Context and Orientation

Mjolnir has three relevant layers. The controller, implemented in `mj-controller`, owns configuration, sessions, target provisioning, and the dashboard-facing API. Each session runs through a target-side `mj-worker`, implemented in `mj-worker`. Shared harness metadata and artifact installation live in `mj-core`.

A harness profile is one account for one coding agent. Its `home` is a directory containing that account's configuration and login. Existing profiles are discovered by `mj setup` and first-run startup. A managed profile is not a separate persisted kind: it is an ordinary profile whose home is outside the harness's conventional user-owned location, which makes the existing pinned installer and ACP sign-in safe to use.

An ACP bridge is the process that translates between Mjolnir's Agent Client Protocol connection and a coding agent such as Codex, Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, MiMo Code, or DSH. "Managed" means Mjolnir selects an exact pinned version and installs or caches its bytes itself.

The first-run dashboard is assembled in `mj-cli/src/dashboard.rs`; background discovery is in `mj-cli/src/dashboard/io/spawn.rs`. The setup editor and profile actions are in `mj-tui/src/setup.rs`. `mj login` is the existing terminal handoff that installs a pinned harness, reads its ACP authentication methods, and performs an interactive login in `mj-cli/src/acp_login.rs`.

The harness installer is `mj-core/src/harness_install.rs`. Managed Node metadata is `mj-core/assets/node/runtime.json`. Harness pins and generated bridge scripts are in `mj-core/src/harness_runtime.rs`. Target provisioning helpers are in `mj-controller/src/targets`.

## Plan of Work

First, correct the managed Node state machine. In `mj-core/src/harness_install.rs`, make the cache health check distinguish healthy, absent, and unusable. Any executable or probe failure in an existing cache must enter the repair path instead of escaping before it, while an install held by a live lease must still refuse repair. Add interpreter metadata to the Linux Node entries and validate it before downloading a glibc archive. Preserve a detailed reason for the final failure. Change the system-runtime probe to return a small internal outcome that distinguishes missing, too old, and probe failure; log probe failures while still selecting the managed fallback. Add a checksum-verified `NODEJS_ORG_MIRROR` override. Add focused tests for corrupt cache repair, inactive installs, missing dynamic loaders, and mirror URL selection.

Second, make artifact acquisition independent of external command-line clients. Add a bounded async HTTP download helper in `mj-core` using the workspace `reqwest` dependency. It accepts only HTTPS URLs, follows a bounded redirect chain that remains HTTPS, applies connect and total deadlines, enforces a byte ceiling, writes through the existing atomic staging directory, and computes the existing SHA-256 check. Replace `download_verified_async` and `download_installer`. Replace OpenCode's external `tar -xzf` with the Rust `tar` and `flate2` readers already used for Node. Add direct artifact metadata for Kimi Code and Grok Build so their vendor shell installers are no longer executed. Use the existing progress callbacks and staged directory conventions.

Third, make authentication independent of ambient CLIs. Extend the managed harness result with the pinned CLI entrypoint where a bridge differs from the CLI. Expose it without weakening the install lease. In `mj-cli/src/main.rs`, use that path for Claude `auth login` and `setup-token`, falling back to `PATH` only for profiles explicitly using a user-owned installation. Extend the focused login tests to use scripts at the pinned paths and remove `PATH` Claude executables.

Fourth, add managed onboarding. Add a setup action available when no enabled profile exists or when the user explicitly chooses managed setup. The action opens a harness chooser listing the managed harnesses and a plain-language authentication summary. Choosing one computes a collision-free profile ID and an absolute Mjolnir-owned home under Mjolnir's configuration directory, creates an ordinary profile value, validates it, saves it through the existing supervised setup-save path, and then invokes the existing `SetupLogin` action. DSH is included because it needs only the documented environment key and needs no interactive login. Existing profiles suppress the automatic prompt but do not remove the manual managed-setup action.

The chooser must explain target implications without selecting a target for the user. If a local container target is available, its row says that container sessions keep the harness off the host. If no container is available, local use remains selectable and the existing raw-target warning remains authoritative. The session wizard, not profile setup, records the eventual target.

Fifth, update first-run messaging and documentation. Welcome output should say that managed setup is available in Settings instead of instructing a novice to install an agent CLI. The quickstart should present two ordered paths: use existing installations, or use managed setup. Profile and runtime docs should describe the actual cold-start downloads and no longer claim that users need `curl`, Bash, npx, or tar. Keep the explicit container Git behavior documented, including the root/passwordless-sudo condition.

Sixth, verify the whole path in isolated environments. Run focused tests after each slice, then full suites for touched crates, then workspace Clippy and formatting once after the last code round. Add an integration test that starts with an isolated config containing no profiles, selects managed setup, saves a generated profile, and proves the generated home is absolute and outside the conventional user home. Add a clean-host harness installation test whose PATH excludes curl, tar, Node, npm, npx, and harness commands. Use recorded or fake ACP servers for deterministic tests and a separate isolated instance for any live daemon test.

## Concrete Steps

All commands run from the repository root:

    /workspace/eebe1ad0bf4a45be37238d78e8bde189/mjolnir

Start by inspecting the current tree:

    git status --short --branch

After the managed Node repair changes:

    cargo test -p brokk-mj-core managed_node
    cargo test -p brokk-mj-core system_node
    cargo test -p brokk-mj-controller node_preflight

After replacing artifact clients:

    cargo test -p brokk-mj-core harness_install
    cargo test -p brokk-mj-worker harness_launch
    cargo test -p brokk-mj-controller bridge_fallback

After pinned Claude login:

    cargo test -p brokk-mjolnir acp_login
    cargo test -p brokk-mjolnir login

After managed onboarding:

    cargo test -p brokk-mj-tui setup
    cargo test -p brokk-mjolnir dashboard

Before each completed commit, run formatting:

    cargo fmt --all -- --check

After the final code round, run the required full validation outside the restricted sandbox:

    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-worker
    cargo test -p brokk-mj-controller
    cargo test -p brokk-mjolnir
    cargo test -p brokk-mj-tui
    cargo clippy --all-targets -- -D warnings

Use an explicit isolated instance such as `--instance managed-onboarding-test` for every manual daemon or TUI run. Stop that daemon when the manual run is complete. Do not point any test at the default store.

## Validation and Acceptance

The managed Node repair acceptance begins by placing executable `bin/node`, `bin/npm`, and `bin/npx` files under the selected cache directory whose interpreter does not exist. Starting a managed Node session must replace that directory from a verified archive and then launch successfully. If another process holds the install lease, the launch must fail without deleting the live installation and must name the live-lease condition.

On a target without a compatible system Node and without the pinned glibc loader, Mjolnir must fail before downloading the archive and explain which runtime contract the target does not satisfy. With `NODEJS_ORG_MIRROR` set to a local test server, Mjolnir must download from that server and still reject a payload whose pinned checksum does not match.

The artifact acceptance removes `curl`, `tar`, Node, npm, npx, and every harness executable from the target PATH. Each managed harness setup must still install its pinned bytes and reach a scripted ACP `initialize` response. Kimi and Grok must not execute a vendor installer script. OpenCode extraction must not invoke `tar`.

The login acceptance uses a managed Claude install whose `node_modules/.bin/claude` is a test script and whose PATH contains no `claude`. `mj login` and `mj login --setup-token` must execute the pinned script and report credential changes correctly.

The onboarding acceptance starts with an isolated configuration containing no profiles. The welcome or Settings flow must offer managed setup, list the supported harnesses, create a collision-free profile in a Mjolnir-owned home, save it before login, and invoke the existing sign-in handoff. Re-running the flow must not overwrite the first profile or choose the same ID. Existing installed profiles must still be detected and offered first.

The target guidance acceptance checks that managed setup does not silently choose local execution for a container-capable harness. The subsequent session wizard must still require an explicit target or use its existing remembered/default behavior, and it must offer available local containers according to its readiness rules.

## Idempotence and Recovery

Every artifact install remains staged under the existing Mjolnir cache, validated before rename, and protected by the existing install and lease files. A failed HTTP request, checksum check, extraction, or executable probe removes only staging and leaves a previously valid installation untouched. Retrying repeats the download or selects an existing complete cache.

Managed onboarding never deletes or edits an existing profile. A generated profile ID is selected from unused IDs, and saving goes through the same locked configuration update as other clients. If sign-in fails after save, the saved profile remains available for retry, and the failure is shown in the setup dialog. Cancelling before save leaves no configuration change.

## Artifacts and Notes

The currently confirmed container Git behavior is:

    target kind            bootstrap
    local Podman           install git and gh when missing
    local Docker           install git and gh when missing
    local Apple container  install git and gh when missing
    SSH Podman             install git and gh when missing
    SSH Docker             install git and gh when missing

The install command needs root or passwordless sudo inside the container. The default agent image already includes both Git and GitHub CLI, so the bootstrap normally finds them present.

The npx correction is:

    ensure_node_runtime -> PATH=<managed-node>/bin:...
    DSH launch          -> npx -y @brokkai/dsh-acp@0.3.0
    Muse adapter        -> npx or pinned bridge, using the same managed PATH

No user-installed npx is involved.

## Interfaces and Dependencies

In `mj-core/src/harness_install.rs`, keep the public managed-harness resolver but add internal result types that describe why a cached runtime is unusable. The health check must not use an error as its ordinary repair signal.

In `mj-core`, add one bounded HTTPS download function used by every managed artifact. It must return the downloaded byte count or a contextual error and must not expose secrets in diagnostics. The exact function name may follow local conventions, but there must be one implementation of URL validation, redirect policy, timeout, byte limit, and checksum verification.

Add `reqwest.workspace = true` to `mj-core/Cargo.toml`. Reuse `tar`, `flate2`, `sha2`, `tempfile`, and existing atomic publication helpers. Do not add a new workspace crate.

In `mj-core/src/config/harness.rs`, add a pure helper that returns the absolute managed home for a harness profile ID. It must account for Muse and OpenCode's nested XDG home layout and DSH's data-directory layout. It must not depend on whether a login currently exists.

In `mj-tui/src/setup.rs`, add a managed-setup control and a small chooser state. Profile creation must produce a serialized profile draft, then use the existing save and `SetupLogin` actions rather than performing filesystem, network, or login work on the render/event loop.

In `mj-cli/src/dashboard/actions.rs`, route the chosen managed profile through the existing supervised save path and then the existing terminal login handoff. No TUI event handler may block on artifact download or authentication.
