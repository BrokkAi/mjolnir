# Restore web and Setup shortcuts and release v2.3.1

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Restore F4 as the web viewer shortcut, expose Setup on F7, and make the composer advertise the same commands that its keys execute. Publish the repair as patch version 2.3.1 through the existing GitHub, crates.io, and npm release workflows. The user requested the fix and a new release as soon as possible.

## Progress


- [x] (2026-09-08) Identify the regression: the Setup change reassigned F4 and moved web to F7, but the composer's independent footer still advertises F4 web and has no Setup hint.
- [x] (2026-09-08) Restore F4 web, add F7 Setup, and supply composer footer hints from the action registry. The integration test fails on the old composer missing F7 and passes after the repair.
- [x] (2026-09-08) Validate key dispatch and rendered hints; the full Rust suite, Clippy, portable-worker Clippy, license policy, supplemental notices, npm tests, and docs checks pass. Regenerate the four deterministic terminal captures and commit the repair on master.
- [x] (2026-09-08) Commit the repair as `f334e3be`; synchronize all eight package versions, five internal dependencies, lockfile entries, and license report to 2.3.1 with no unrelated dependency changes.
- [ ] Validate the clean 2.3.1 release commit locally and in CI.
- [ ] Tag and publish v2.3.1, verify downloads and all registry packages, and record the outcome.

## Surprises & Discoveries


`mj-chat/src/hel_chat/active.rs::render_chat_footer` contains a hardcoded function-key string independent of `mj-tui/src/actions.rs`. Existing tests confirm each copy independently, so they accepted contradictory advertised behavior. Setup itself is implemented and routed; its shortcut was missing from the footer seen while typing.

The current disposable container has no build cache or installed license tools. Install the pinned tools and keep validation logs under `target/release-v2.3.1/`. The existing upstream master matches local master at the start. All three v2.3.0 publication workflows succeeded; their configuration remains unchanged. The prior release plan records the user's instruction not to repeat the trusted-publisher settings detour; preserve that decision and use the existing publishers.

## Decision Log


Restore the established F4 web binding and put Setup on F7. This preserves the user's established shortcut and gives Setup a separate advertised key. Supply the composer's global hints from the TUI action registry through its rendering interface so future shortcut edits update both footers. Do not add a crate or duplicate another key list in production code. Use patch version 2.3.1 because this repairs existing behavior. Commit and publish on the current master branch using the established release pipeline.

## Outcomes & Retrospective


The repair is implemented. Its regression test renders a real composer, checks F4 web and F7 setup, and opens the correct dialogs from all four focus locations. Full validation and publication remain.

## Context and Orientation


`mj-tui/src/actions.rs` owns key definitions and command dispatch. `mj-tui/src/render.rs` builds pane footer hints from those definitions. `mj-tui/src/combined.rs` renders the combined dashboard and delegates the prompt area to `mj-chat`. `mj-chat/src/hel_chat.rs::ChatRegions` describes the areas delegated to chat, and `mj-chat/src/hel_chat/active.rs` draws the composer footer. Pass host-owned global hints alongside its footer area. The CLI's `mj-cli/src/dashboard.rs` catches global keys before routing typing to chat; its tests can render a real chat and then activate the advertised keys.

`Cargo.toml` owns the release version. `scripts/release-version.mjs sync`, `cargo update --workspace`, and cargo-about update the internal requirements, lockfile, and license report. `RELEASING.md` defines the release gates. Existing workflows build platform archives, publish the completed GitHub Release, then publish eight Rust crates and four npm packages.

## Plan of Work


First repair the keys and shared footer data, with regression coverage that renders the composer and activates F4 and F7 from each focus. Update onboarding hints, help, README, web and terminal documentation, and deterministic terminal captures. Run focused tests and the required Rust suite and Clippy, then commit the coherent fix.

Next bump the shared version to 2.3.1, synchronize the internal dependencies and lockfile, regenerate the report, and commit the release candidate. Validate that exact clean commit with formatting, tests, Clippy, release builds, license checks, source packaging, npm tests, documentation checks, and the release-version script. Push master, wait for CI on the exact candidate, then create and push an annotated v2.3.1 tag. Monitor the release and registry workflows and verify their public results.

## Concrete Steps


Run from the repository root, with logs under `target/release-v2.3.1/`:

    cargo fmt --check
    cargo test --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo build --locked --release --target-dir target/release-host -j 8
    cargo build --locked --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo clippy --locked --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings
    cargo check --locked --workspace --all-targets
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix docs run check
    npm --prefix docs run build
    node scripts/release-version.mjs check v2.3.1

Use cargo-about 0.9.1 and cargo-deny 0.20.2. Compare a fresh generated license report and supplemental notices with their committed copies. Tests run in the unrestricted container; no sandbox escalation flag is available or needed. This container lacks WebKitGTK and ALSA development packages, and passwordless sudo is unavailable. Run desktop and voice compilation and tests through the exact-commit CI jobs, which install their own dependencies; do not treat an unavailable local all-workspace check as a failed implementation. After a clean committed candidate passes validation and exact-commit CI, use `git tag -a v2.3.1 <candidate> -m 'Mjolnir v2.3.1'` and push that tag to origin.

## Validation and Acceptance


With a conversation open and the composer focused, its visible footer advertises F4 web and F7 setup. F4 opens Web viewer and requests its address; after closing it, F7 opens the editable Setup modal. The same keys work from Sessions, Targets, and Quota. Help and onboarding match. The regression test must fail on the old implementation and pass with the repair. All release gates pass on the tagged commit. GitHub exposes all archives and checksums, a downloaded Linux archive verifies and reports 2.3.1, and every Rust and npm package publishes 2.3.1.

## Idempotence and Recovery


Version synchronization and report generation are repeatable. Never move a published tag or force-push master. Registry workflows skip versions that already exist and can resume partial publication. Preserve credentials and publisher settings. Report actual failures and repair their causes before retrying.

## Artifacts and Notes


The starting commit is `80f6ea0e`. The preceding release is v2.3.0. Keep build logs and downloaded verification artifacts under target; keep this durable plan up to date with outcomes.

## Interfaces and Dependencies


Use the existing `ChatRegions` render boundary to carry global footer hints from the action registry. Preserve composer-specific hints, notices, and history-search behavior. No external dependencies, subprocess behavior, event-loop work, or publisher settings change.

Revision 2026-09-08: record the verified footer mismatch and the repair and release sequence.

Revision 2026-09-08: record the before/after regression evidence and use exact-commit CI for desktop and voice system dependencies unavailable locally.

Revision 2026-09-08: record passing fix validation and regenerated captures before committing the repair.

Revision 2026-09-08: record synchronized release versions and the validated fix checkpoint before committing the release candidate.
