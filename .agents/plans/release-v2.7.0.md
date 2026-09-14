# Publish Mjolnir v2.7.0

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md` and the release requirements in `RELEASING.md`.

## Purpose / Big Picture

Publish the accumulated session API, managed subagent, recovery, configuration, and runtime improvements since v2.6.4. Completion means a public GitHub release with verified archives, all twelve Rust packages and four npm packages published at 2.7.0, and the Homebrew formula updated to the same version.

## Progress

- [x] (2026-09-14) Confirmed `master` is clean, equals `origin/master`, and has no release after v2.6.4.
- [x] (2026-09-14) Selected v2.7.0 and prepared the release plan, public notes, and workspace version bump.
- [x] (2026-09-14) Synchronized dependency constraints, twelve lockfile entries, generated licenses, and package assets; release-version, asset, and whitespace checks pass.
- [ ] Run all local release validations and verify exact-commit CI and publisher authorization.
- [ ] Commit and push the validated release commit, then create and push the annotated v2.7.0 tag.
- [ ] Verify GitHub assets, crates.io, npm, and Homebrew publication; record final evidence.

## Surprises & Discoveries

- Observation: The crates.io workflow now publishes twelve packages rather than the nine described in the prior release plan because review, transcript, and checkpoint were split into independently published crates after v2.6.4.
  Evidence: `.github/workflows/publish.yml` lists twelve crates in dependency order.
- Observation: The first macOS Clippy run found that `libc::TIOCSCTTY` is a 32-bit request while `libc::ioctl` accepts `c_ulong`; after fixing that compile error, post-exit slave-side termios reads returned `EIO` on macOS.
  Evidence: Casting the request to `libc::c_ulong` restores compilation, and inspecting the shared PTY settings through the still-open master makes all eight `termination_pty` behavior tests pass.
- Observation: SQLite's `SQLITE_OPEN_NOFOLLOW` rejects a database path with any symlinked ancestor on macOS, including the ordinary `/var` alias used by temporary directories.
  Evidence: Canonicalizing the trusted harness home before appending `goals_1.sqlite` preserves final-component symlink rejection and makes the focused checkpoint tests plus the full workspace suite pass.
- Observation: Exact-commit CI exposed an unguarded Unix socket client in the subagent MCP path and a reliability fixture pinned to Codex ACP 1.11.1 after production advanced to 1.11.3.
  Evidence: The non-Unix build now returns an explicit unsupported-platform error, and the fixture seeds the current managed install under the profile's `XDG_CACHE_HOME`; host Clippy and the full workspace test suite pass.
- Observation: Successive Windows Clippy runs exposed Unix-only review capture declarations, a login-environment binding consumed only inside a Unix cfg block, and integration-fixture cleanup that directly used Unix signals.
  Evidence: Review capture and login setup now respect their platform boundaries, while fixture cleanup uses process-group signals on Unix and the existing cross-platform process API elsewhere.

## Decision Log

- Decision: Release as v2.7.0 rather than v2.6.5.
  Rationale: The changes since v2.6.4 add substantial user-visible capabilities, including a session API, managed subagents, durable subagent events, goal recovery, and new session configuration controls; a minor version communicates that scope while preserving semantic compatibility.
  Date/Author: 2026-09-14 / Codex

## Outcomes & Retrospective

Release preparation is in progress. No tag or registry publication has occurred yet.

## Context and Orientation

`Cargo.toml` owns the workspace version and same-release internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` embed workspace package versions. `scripts/release-version.mjs` synchronizes and checks versions, while `scripts/sync-package-assets.mjs` keeps canonical notices and documentation in published packages. `.github/workflows/release.yml` builds and publishes GitHub assets, then dispatches Rust publication; `.github/workflows/publish.yml` and `.github/workflows/publish-npm.yml` publish registry packages. `BrokkAi/homebrew-tap` is updated manually after release assets exist.

## Plan of Work

First synchronize versioned metadata and generated notices for 2.7.0, then commit the preparation as a coherent checkpoint. Run formatting, Clippy, release builds, full tests, license checks, packaging checks, npm tests, web tests, script tests, and documentation checks. Verify the clean release commit with `node scripts/release-version.mjs check v2.7.0`, confirm exact-commit GitHub CI, and verify the configured trusted publishers from registry evidence before tagging.

After every gate passes, push the release commit, create an annotated `v2.7.0` tag at that exact commit, and push the tag. Monitor the GitHub Release and registry workflows until complete. Download the assets, verify checksums and archive contents, set the prepared release notes, update the Homebrew formula through its repository script, and verify all public channels before marking this plan complete.

## Concrete Steps

Run from `/Users/ryansvihla/code/mjolnir`. Keep logs and downloaded evidence under `target/release-v2.7.0-checks/` and `target/release-v2.7.0-assets/`:

    node scripts/release-version.mjs sync
    cargo update --workspace
    cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
    node scripts/sync-package-assets.mjs sync
    node scripts/release-version.mjs check v2.7.0
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo build --release
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix tests/e2e/web run test:unit
    node --test scripts/run.test.mjs
    npm --prefix docs run check
    npm --prefix docs run build

When the release commit and its CI are green, run `git tag -a v2.7.0 <validated-SHA> -m 'Mjolnir v2.7.0'` and `git push origin v2.7.0`. Set public notes with `gh release edit v2.7.0 --notes-file .agents/docs/release-v2.7.0-notes.md --repo BrokkAi/mjolnir`.

## Validation and Acceptance

The tag must resolve to a clean commit for which every local command above and every required CI job succeeds. The GitHub Release must contain three platform archives and their SHA-256 sidecars; every checksum must match and every archive must contain `mj`, `mj-desktop`, `mj-voice-worker`, and both portable workers, with the macOS archive also containing the native universal worker. All twelve Rust crates must expose non-yanked 2.7.0, all four npm packages must expose 2.7.0 under `latest`, and the remote Homebrew formula must select 2.7.0 with matching archive checksums.

## Idempotence and Recovery

Version synchronization, generation, validation, packaging, and asset downloads are repeatable. Never move or force-push a published tag. The registry workflows skip already published versions, so a failed partial publication can be rerun safely. Stop before tagging if publisher authorization, exact-commit CI, or any required validation is not verified.

## Artifacts and Notes

Public notes are stored in `.agents/docs/release-v2.7.0-notes.md`. Publisher evidence and validation logs belong under `target/release-v2.7.0-checks/`; downloaded release assets belong under `target/release-v2.7.0-assets/`. These target artifacts are local evidence and are not committed.

## Interfaces and Dependencies

Use the repository-pinned Rust 1.96.0 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node 24, Git, GitHub CLI, crates.io, npm, and the `BrokkAi/homebrew-tap` repository. Preserve the current workflow identities, package names, environments, and dependency order.

Revision 2026-09-14: initialized the release plan and selected v2.7.0 from the scope accumulated since v2.6.4.

Revision 2026-09-14: recorded and fixed the macOS PTY portability failures discovered by release validation.

Revision 2026-09-14: incorporated the upstream daemon startup error-reporting fix into the release candidate and public notes.

Revision 2026-09-14: completed the local validation matrix, fixed macOS Codex goal checkpoint paths and workspace license exceptions, and produced all twelve package archives.

Revision 2026-09-14: resumed after incorporating the global `--instance` / `-i` isolation flag from commit `1cbe5778`; restarted exact-candidate validation before tagging.

Revision 2026-09-14: fixed the Windows subagent MCP compile failure and synchronized the deterministic reliability harness with the current managed Codex ACP pin.

Revision 2026-09-14: gated review capture and login setup correctly, then made integration-fixture daemon cleanup compile and terminate processes cross-platform after exact-commit Windows CI exercised the test targets.
