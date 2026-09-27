# Run workers on existing Macs over SSH

This ExecPlan follows `.agents/PLANS.md` and must remain current throughout implementation.

## Purpose / Big Picture

An existing Mac, including a manually provisioned EC2 Mac, can run a session for a Linux or Mac controller using the existing SSH machine and bare runtime. Mjolnir uploads a matching macOS worker, starts it remotely, and reconnects after transport or controller interruption. The host remains running when a session closes. AWS host allocation and application toolchains such as Xcode remain operator responsibilities.

## Progress

- [x] (2026-09-26) Inspected deployment, release packaging, SSH workspace behavior, and repository instructions; user approved existing-host SSH scope and offered `jonathans-macbook-air` for testing.
- [x] (2026-09-26) Implemented shared platform detection and Darwin worker selection across preflight, launch, recovery, and upgrade.
- [x] (2026-09-26) Packaged the universal Darwin worker in every archive/npm distribution; all 11 packaging tests and 14 Linux release tests pass.
- [x] (2026-09-26) Added login-shell prerequisites, Darwin capacity sampling, and portable installed-worker digest checks. Actual Mac probe reports 16 GiB and eight logical cores.
- [x] (2026-09-27) Full dev-profile Cargo suite passed with eight test threads; final Clippy and the login-shell regression passed.
- [x] (2026-09-27) Real Mac launch, harness turn, concurrent workers, daemon restart during a turn, suspend/checkpoint and resume passed.
- [ ] Commit the validated implementation, exercise the changed-build idle upgrade, and clean up isolated test resources.
- [x] (2026-09-27) Filed Apple-container-over-SSH follow-up #1169; user excluded Podman on macOS. Apple container requires a separate install.

## Surprises & Discoveries

The release already builds a universal Mac worker, but only distributes it with the Mac application. Remote selection detects architecture alone and requires a portable Linux worker. SSH bare sessions already have the needed detached process and durable relay lifecycle. Their selected repository remains on the remote host, and cross-machine moves are deliberately refused.

## Decision Log

- Decision: Use `kind = "ssh"` machines and `kind = "bare"` targets without a new configuration schema or database migration. Rationale: Mac support is an execution-platform distinction, not a different ownership model. Date: 2026-09-26.
- Decision: Distribute one universal Darwin worker alongside both portable Linux workers. Rationale: existing release builds already combine the two Apple architectures, and Linux controllers need the foreign executable without executing it locally. Date: 2026-09-26.
- Decision: Require working Git and harness prerequisites on existing hosts, with actionable diagnostics; do not install Xcode or Homebrew automatically. Rationale: host provisioning is outside the selected scope. Date: 2026-09-26.

## Outcomes & Retrospective

The supplied Mac built a matching dev worker in 38 seconds. The full dev-profile workspace suite passed with eight test threads after an initial default-parallel run hit two worker relay timeouts. Final Clippy passed. The Linux controller launched two Darwin workers, a harness returned MAC_WORKER_OK after a daemon restart during its turn, and suspend/checkpoint/resume preserved the first session while the second stayed live. The direct CLI checkpoint request exposed a separate Linux debug-daemon stack overflow; automatic and suspend checkpoints completed and verified their archives. Idle replacement across build revisions is the remaining acceptance check. Initial working tree contains unrelated untracked files; leave them untouched.

## Context and Orientation

The controller owns provisioning and selects the binary under `mj-controller/src/controller/worker_binary/`. A target locator describes where commands execute; `mj-controller/src/targets/` turns those commands into local or SSH subprocesses. A worker is a detached process on the target, keeping the harness and durable relay journal alive independently of the controller. The release workflow builds native and portable binaries, and `npm/scripts/package-release.mjs` copies archive contents into npm distributions.

## Plan of Work

First introduce a shared platform detector which combines operating system and normalized architecture. Probe existing SSH bare hosts before selecting or downloading a worker. Keep local native selection and portable Linux container behavior. Extend pinned source snapshots and artifact lookup to Darwin, supporting architecture-specific development binaries and the universal release artifact. Preserve build-stamp checks, content-addressed caches, and existing idle admission for worker upgrades. Test Linux and Darwin selection and unavailable artifacts.

Next separate Mac build output from archive assembly in the release workflow, allowing every archive to depend on the universal worker without a dependency cycle. Include the universal artifact in npm packages and test actual archive assembly and package contents.

Then make SSH prerequisites use a consistent remote shell environment and validate Git by executing it. Supply Darwin host CPU/memory sampling in the existing capacity result shape. Linux container sampling remains Linux-specific. Add behavior tests and include macOS-sensitive paths in the CI trigger list where automatic cfg detection does not cover them.

Finally build with the existing mbx Cargo setup, test on the supplied Mac, and document exact results below. Build storage must not be redirected. Live application tests use `--instance macos-ssh-test` and isolated configuration/data. Remote test repositories and worker builds live in a dedicated directory under the remote user's home, never in an existing project.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, run focused controller tests as implementation proceeds, then `cargo test` and `cargo clippy --all-targets -- -D warnings` in the dev profile outside the restricted sandbox. Run `node --test scripts/release-workflow.test.mjs npm/test/package-release.test.mjs` and applicable formatting/CI checks. Use the existing Cargo wrapper without target-directory overrides.

Probe `jonathans-macbook-air` using noninteractive SSH with bounded connection time. Once reachable, inspect available Rust, Git, Node and npm, build a matching worker from the same source revision, and configure an isolated controller with an SSH bare target pointing at a disposable repository. Exercise start, turn delivery, concurrent sessions, reconnect, controller replacement, worker upgrade at idle, close and resume. Stop test processes before removing their working files.

## Validation and Acceptance

A Linux controller must select and upload a Darwin executable to the Mac, complete a worker handshake, and run a harness turn. Reconnecting and replacing the controller must preserve accepted work. Closing one session must preserve the host and other sessions. Missing tools and missing/mismatched workers must fail with useful messages. Automated selection tests cover both Apple architectures and both Linux architectures. Release tests must prove each archive and npm package contains the universal Darwin worker with executable permissions. Record any unavailable hardware or external prerequisite explicitly rather than claiming that unperformed checks passed.

## Idempotence and Recovery

Worker sources remain immutable and keyed by content hash. The existing session owner continues to serialize worker replacement and cleanup. No database migration is expected. All remote test resources are task-specific; keep their names in validation notes and remove only those resources after terminating their processes. Do not change branches or push. Stage only this task's files and commit validated changes on the current branch.

## Artifacts and Notes

Initial inspection: current branch `master`; unrelated untracked paths `.agents/plans/fix-subagent-tool-results.md`, `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3`.

## Interfaces and Dependencies

Add one shared target-platform value in the existing controller target module, with Linux/Darwin OS and normalized `x86_64`/`aarch64` architecture. Worker preflight accepts the command executor so an existing SSH host is detected before artifact resolution. Existing public machine/runtime configuration and worker protocol remain unchanged. Native Mac artifacts are produced by the macOS CI runner; the Linux controller only reads and uploads their bytes.

Revision note (2026-09-26): Initial executable plan recorded after implementation authorization.


Validation update (2026-09-26): SSH access is ready. The user explicitly authorized source transfers between their two hosts after automatic review initially rejected a broader archive. The isolated Mac build is `/Users/jonathan/Projects/mjolnir-macos-ssh-test`, the test repository is `/Users/jonathan/Projects/mjolnir-macos-ssh-fixture`, and Linux config/data/artifacts are under `/mnt/optane/macos-ssh-test`. The instance name is `macos-ssh-test`, workspace `mac-test`, and its API binds to loopback port 37809. No live instance data is used.

Discovery and decision (2026-09-26): The first real CLI launch exposed an existing shared API/web bug: bare project paths were sent to controller-local quick-bundle registration, which rejects `/Users/...` on Linux. Correct both surfaces to use the existing raw-project context identity after validation, leaving remote path validation to the daemon's supervised session admission. This applies equally to local and remote bare directories and avoids introducing a Mac-only exception. Add API and web regression tests proving no local bundle lookup occurs.

Discovery (2026-09-27): macOS accounts commonly use zsh, where `status` is read-only. SSH prerequisite scripts now load the account login environment, then execute their POSIX syntax in `/bin/sh`. The regression test supplies a shell with a read-only status variable.

Validation evidence (2026-09-27): `/mnt/optane/macos-ssh-cargo-test-complete.log`, `/mnt/optane/macos-ssh-clippy-final.log`, and `/mnt/optane/macos-ssh-login-tests.log` contain passing automated results. Real fixture is `/Users/jonathan/Projects/mjolnir-macos-ssh-fixture-network`, cloned from octocat/Hello-World; the initial local-only Git fixture was rejected as expected for lacking a network remote. Sessions A (`087162628169f8b48de8fee1fedc9afe`) and B (`8dc2b27429db1e96cd51e52c04e79a88`) are isolated under instance macos-ssh-test.

Scope update (2026-09-27): User requested containers on SSH Macs if small, otherwise an issue. Existing Docker SSH backend can talk to a Linux Docker daemon on the Mac, but the supplied host has no running daemon. Apple container is explicitly local-only in config resolution and target lifecycle types, and Podman preflight requires local rootless `podman unshare`, which excludes the Mac remote client. Assess these separately rather than changing the bare-worker platform boundary.

Follow-up (2026-09-27): https://github.com/BrokkAi/mjolnir/issues/1169 tracks Apple containers over SSH. Its public body describes requested behavior and installation prerequisites only.
