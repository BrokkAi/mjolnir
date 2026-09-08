# Release Mjolnir v2.3.0

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Publish the work since v2.2.0, including immediate New, startup workspace corrections, the stopped-session toggle, the visible workspace switcher, browser voice input, and native DSH/Muse session import and relocation. Users should obtain version 2.3.0 from GitHub, crates.io, and npm. The user explicitly authorized committing, pushing master, creating the release tag, and pushing that tag.

## Progress


- [x] (2026-09-08) Read the release instructions and merge origin/master's native import commit into the current master branch without conflicts.
- [x] Select minor version 2.3.0 for the combined additions since v2.2.0.
- [x] (2026-09-08) Synchronize all eight package versions, five dependency constraints, lockfile entries, and the generated license report; supplemental notices match. Prepare the clean release commit for validation.
- [x] (2026-09-08) Run clean-commit formatting, tests, host and musl Clippy/release builds, whole-workspace checks, all eight source packages, licenses, npm tests, documentation, and version checks against candidate `10004d18`.
- [x] Push `10004d18` to origin/master; CI passes reliability, Linux desktop, licenses, and voice checks, but exposes an incorrect raw-output readiness assertion in the macOS PTY test.
- [x] (2026-09-08) Validate the corrected PTY readiness check and push replacement candidate `f286aa71ce971148a05eb0789065fe6b8d5362b3`. Repeat the full clean-commit local checks; CI run `34242392556` passes all seven jobs.
- [x] (2026-09-08) Create and push annotated tag v2.3.0 at `f286aa71`; remote tag object is `2a0d09a3efe624aed208390141541f1ce54918f4`. Release workflow `34245436627` is building the downloads.
- [x] (2026-09-08) Verify the published GitHub Release, all three archives and checksum sidecars, the Linux controller and worker version, all eight Rust crates, and all four npm packages at latest 2.3.0.

## Surprises & Discoveries


Local master had two session correction commits while origin/master had one native import commit. A normal merge preserved both without conflicts. The v2.2.0 GitHub, crates.io, and npm workflows all completed successfully. Publisher configuration is unchanged; the user's explicit instruction not to repeat the trust-list detour overrides RELEASING.md's settings-inspection requirement. No authentication or publisher changes are needed.

The macOS PTY log shows the empty preview is correctly rendered through separate cursor-addressed writes (`N`, then ` session`), retaining unchanged cells from `Loading sessions…`. Waiting for contiguous `No sessions` bytes is invalid for incremental terminal rendering. The termination test now waits for the first-frame `PgUp/PgDn preview` controls, after the live feed has started, and preserves its signal, bounded-exit, and terminal-restoration assertions. Empty-preview content is already covered by the preview's buffer-rendering tests.

## Decision Log


Use 2.3.0 because the release also contains browser voice input and native import features added upstream since 2.2.0. Keep the existing branch and publisher configuration. Use a normal merge, never a rebase or force push. Validate the exact clean release commit before tagging and rely on the existing automated release workflows for all platforms and registries.

## Outcomes & Retrospective


Mjolnir v2.3.0 is published at https://github.com/BrokkAi/mjolnir/releases/tag/v2.3.0. GitHub release workflow `34245436627`, crates.io workflow `34248971085`, and npm workflow `34248983366` all completed successfully. All six GitHub assets are public, their archive digests match the checksum sidecars, and the downloaded Linux controller and portable worker both report 2.3.0. All eight Rust crates expose 2.3.0 without being yanked; all four npm packages expose 2.3.0 as latest. The release notes describe the final feature scope. No release work remains.

The macOS failure was an invalid terminal-stream assertion, not missing preview content. Synchronizing the termination test on first-frame controls preserved the advertised shutdown checks and passed on macOS and Linux. This completion record is committed separately after publication; the immutable release tag continues to identify `f286aa71`.

## Context and Orientation


`Cargo.toml` owns the shared version for eight workspace crates. `scripts/release-version.mjs sync` copies it into five internal dependency constraints. `Cargo.lock` and the generated `licenses/THIRD_PARTY_LICENSES.html` must also reflect the new version. `RELEASING.md` describes validation and publication. `.github/workflows/ci.yml` checks the exact master commit across Linux, macOS, and Windows. `release.yml` builds archives for Linux x64, Linux ARM64, and universal macOS, uploads checksums, then publishes a GitHub Release. The existing registry workflows publish eight Rust packages and four npm packages after that release succeeds.

## Plan of Work


First synchronize all release versions and regenerate the license report, keeping unrelated dependency versions fixed. Commit these files and this plan on master so all validations run against a clean commit. Run the repository's release checks and inspect any failures before pushing. Push the candidate to origin/master and monitor CI; fix any actual failures on master and validate a replacement candidate if needed. Once validation passes, create and push an annotated v2.3.0 tag at the exact candidate. Monitor publication, verify the Linux archive checksum and reported binary version, and confirm all registry versions.

## Concrete Steps


Run from `/home/ryan/code/mjolnir`, with test and build commands elevated and all logs under `target/release-v2.3.0-*`:

    node scripts/release-version.mjs sync
    cargo update --workspace
    cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
    cargo fmt --check
    cargo test --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo build --locked --release
    cargo build --locked --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo clippy --locked --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings
    cargo check --locked --workspace --all-targets
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    node scripts/release-version.mjs check v2.3.0

Generate a second license report under target and compare it with the committed report. Generate supplemental notices under target and compare with `licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt`. Check and build docs using `npm --prefix docs run check` and `npm --prefix docs run build`. After validations, use `git push origin master`, inspect CI for `git rev-parse HEAD`, then `git tag -a v2.3.0 <validated-commit> -m 'Mjolnir v2.3.0'` and `git push origin v2.3.0`.

## Validation and Acceptance


All required local checks and exact-commit CI pass. The annotated tag resolves to the clean validated commit. GitHub lists three platform archives and three checksum files, and the checksum-verified Linux binary reports `mj 2.3.0`. All eight Rust crates and all four npm packages expose 2.3.0. Publication failures must be reported and repaired instead of claiming success.

## Idempotence and Recovery


Version synchronization and report generation are repeatable. Never move a published tag or overwrite upstream commits. Registry workflows skip versions already published, so rerunning a failed workflow resumes partial publication. Keep credentials out of files and logs. Prepare release notes under target and use `gh release edit --notes-file` if the generated notes need a readable summary.

## Artifacts and Notes


The session fixes are commits `39171a81` and `4028975e`; native import is upstream commit `fab6694e`. The release commit is `f286aa71ce971148a05eb0789065fe6b8d5362b3`; the public annotated tag is v2.3.0. Tagging followed successful completion of every validation step; Windows CI cache housekeeping finished afterward and the overall CI run completed successfully. Logs and downloaded artifacts stay under target. The final response records the immutable release commit, tag, URL, and publication outcome.

The Linux archive SHA-256 is `32f08c5930ea91b9f6782c031825f4bc5f8fb29b01781e43f39eb20df7fcc0c1`. Verification records are `target/release-v2.3.0-published.json`, `target/release-v2.3.0-npm-verification.json`, and `target/release-v2.3.0-crates-verification.json`. The Linux archive's checksum passed and its digest matches the public GitHub asset byte-for-byte.

## Interfaces and Dependencies


Use the repository's Rust 1.96 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node 24, and existing GitHub release workflows. Do not change registry authorization, publication triggers, or package identities.

Revision 2026-09-08: start the resumed release with the merged feature scope and explicit commit/push/tag authorization.

Revision 2026-09-08: record synchronized versions and notice checks before creating the clean release candidate.

Revision 2026-09-08: record successful local validation, the public candidate, and the evidence-backed macOS PTY test repair.

Revision 2026-09-08: record passing validation of the corrected candidate and the public release tag while artifact publication runs.

Revision 2026-09-08: record successful GitHub, crates.io, and npm publication and the verified public artifacts and package versions.
