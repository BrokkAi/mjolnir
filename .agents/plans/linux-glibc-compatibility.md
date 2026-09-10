# Restore Linux CLI compatibility

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Issue #984 records GNU binaries that cannot start on Debian 12 and Amazon Linux 2023 because they require glibc 2.39. Build the CLI against glibc 2.28, matching Bifrost, and reject unusable installer candidates before replacing files.

## Progress

- [x] Inspect Bifrost and release/installer code; confirm master and origin/master.
- [x] Implement isolated zigbuild CLI builds and ELF checks.
- [x] Stage and smoke-test installer candidates; 14 behavioral tests pass, including corrupt-archive preservation.
- [x] Add CI runtime checks and documentation; actionlint and shell checks pass.
- [x] Cross-build ARM64: maximum GLIBC 2.28; validate tmux harness on Debian with diagnostic CLI.
- [x] Validate both ELF binaries and x86-64 tmux runtime on all three distributions.
- [ ] Commit changes and push authorized upstream; observe native ARM CI.

## Surprises & Discoveries

The release workflow uses plain cargo for the CLI; only workers use musl. Installer checksum caching occurs before extraction and never checks startup. Local Python lacks ensurepip, so tool installation needs another available Python environment. Agent planning files require elevated filesystem access on this host. The host mbx Cargo wrapper injected a native GCC override into AWS-LC on x86-64: its object reported GCC 15 and referenced __isoc23_sscanf/strtol. Running with PATH prefixed by $HOME/.cargo/bin uses unwrapped Cargo without relocating build storage. The first ARM64 build already passed the glibc 2.28 contract. NFS TEST_STATEID stayed at 25 across the slow tool installation.

## Decision Log

Use cargo-zigbuild 0.23.3 and Zig 0.15.2 with GNU target suffix .2.28, matching Bifrost; retain Rust 1.96.0. Separate target/release-cli output prevents native helper builds from replacing compatible artifacts. Preserve archive/npm names. Desktop/audio dependencies and Alpine/NixOS are outside this fix. User authorized commit and push on the current branch. Reuse the one fetched archive checksum for verification and cache recording so a second network fetch cannot give inconsistent results.

## Outcomes & Retrospective

Both release builds pass the ELF contract with maximum GLIBC 2.28. The x86-64 CLI reports mj 2.6.1, renders the dashboard in tmux, and exits with status zero on Rocky Linux 8 (glibc 2.28), Debian 12 (2.36), and Amazon Linux 2023 (2.34). All 14 installer/ELF regression tests and 13 npm tests pass; actionlint, Bash parsing and diff checks pass. Native ARM runtime checks run in CI. No release tag is authorized or needed; existing published archives remain unchanged until a release.

## Context and Orientation

`.github/workflows/release.yml` packages CLI, desktop, voice helper and static session workers. `.github/workflows/ci.yml` runs regular validation. `install.sh` downloads archives and writes the user's installation. ELF is the Linux executable format; its loader, shared libraries and GLIBC symbol versions determine whether a binary starts.

## Plan of Work

First add shared toolchain/build/ELF scripts and use them in both release jobs and an architecture CI matrix. Verify with readelf and run in Rocky Linux 8 (glibc 2.28), Debian 12 and Amazon Linux 2023 containers. Next locate all installer companions and run staged mj --version before installing; persist checksum only on success. Invalid cached executables trigger a fresh download. Add Node tests and document the CLI floor in README.md and RELEASING.md.

## Concrete Steps

From the repository root run `node --test scripts/linux-release.test.mjs`, `npm test --prefix npm`, and `bash -n install.sh`. Install pinned tools with `bash scripts/setup-linux-cli-toolchain.sh`, export its displayed Zig path, then run `bash scripts/build-linux-cli.sh x86_64-unknown-linux-gnu` and the ARM equivalent after installing that Rust target. Run `bash scripts/test-linux-cli-runtime.sh TARGET` on a matching architecture with Docker or Podman. Review `git diff --check`, stage only changed paths, commit on master and push origin/master. Observe CI.

## Validation and Acceptance

Both architectures must pass ELF verification with maximum GLIBC no greater than 2.28. Runtime checks start mj --version on glibc 2.28 and previously failing distributions. Installer tests prove failed startup or missing companions preserve installed files and checksum, valid cached binaries skip downloads, and invalid cached binaries are repaired. Verify tmux startup and clean exit on Debian and Amazon Linux. No Rust source/dependency edits are planned; shell/Node checks and real release builds provide relevant local validation.

## Idempotence and Recovery

Build artifacts remain under target. Installer staging uses existing temporary-directory cleanup. Preflight failure does not modify an installation. Remove runtime containers after testing. Preserve unrelated files and never create a branch or tag.

## Artifacts and Notes

Local evidence is in ignored target/linux-cli-x64.log, target/linux-cli-arm64.log and target/linux-cli-runtime.log. Run CONTAINER_RUNTIME=podman bash scripts/test-linux-cli-runtime.sh x86_64-unknown-linux-gnu locally. The unwrapped rebuild corrected AWS-LC object references to __isoc99_sscanf and strtol, and both architectures list only libm, libpthread, libc, libdl and their standard GNU loader as dynamic dependencies. Earlier AWS evidence is `.agents/docs/aws-distro-testing-2026-09-10.md`; those machines have been terminated.

## Interfaces and Dependencies

Public asset/npm names stay GNU. Shared build scripts accept an unsuffixed GNU Rust target triple and build brokk-mjolnir into target/release-cli. ELF verification accepts binary path and target triple. No public Rust API changes.

Plan recorded 2026-09-10; initial implementation and installer test outcomes incorporated.

Updated during validation: recorded ARM64 success, 14 passing regressions, and the local compiler-wrapper interference; x86-64 validation continues with unwrapped Cargo.

Updated after local validation: both ELF checks and all three x86-64 runtime cases passed. Delivery and native ARM CI observation remain.
