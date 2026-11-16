# Faster SSH-host launches: send profile content once per host and batch launch commands

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root. Tracking issue: #1541.


## Purpose / Big Picture

A Mjolnir session on an SSH host (an EC2 machine registered as an `ssh-podman` target, or any `ssh-bare` target) should start in well under a minute even when several sessions start on the same host at once. Today it does not. On 2026-11-15 four Claude sessions started on one EC2 host within 30 seconds, and session `2b0ac47bdab7045c3eb55f39c82acbfa` took 211 seconds. Two costs dominated, both visible in the daemon log `~/.local/share/mjolnir/logs/mj-daemon-20261115T051028.347Z-3559993.log`.

First, every session uploads its whole harness profile (the set of files a harness such as Claude Code reads from its configuration directory) as one compressed archive. For Claude that archive is about 10.7 MB, and about 10.0 MB of it is the `plugins/` directory, which is the same for every session that uses the same profile. Four concurrent uploads took 62 to 87 seconds each and filled the controller's outbound link.

Second, after the upload the launch runs about 69 separate remote commands, one after another. Each normally takes about 0.2 seconds. While the link was full, each took 2.5 to 5 seconds, which cost about 130 seconds.

After this change, the controller sends each distinct profile file to a host once, and later sessions on that host reuse the stored copy. The launch also issues far fewer remote commands, because commands that do not depend on each other's results run as one remote script. To see it working, start four sessions on one EC2 host at the same time. The daemon log should show that only the first session sends plugin content. It should also show fewer `session command stage finished` records per launch, and a much shorter `provision session` phase.


## Progress

- [x] (2026-11-15 06:10Z) Diagnosed the incident from the daemon log and mapped both areas with read-only child investigations. Findings: `.mj/agents/98f62e22782823df6b3d6d3e298d2000/findings.md` (profile archive) and `.mj/agents/5eb1525f3de8aadc518093a8507a917f/findings.md` (round trips). These are local, uncommitted report directories; this plan repeats every fact it relies on.
- [x] (2026-11-15) Milestone 1: host profile content store implemented and validated; real-shell regressions, full controller suite (2,651 passed, 27 ignored), workspace clippy and formatting pass.
- [x] (2026-11-15 07:19Z) Milestone 2: shared step-script helper and all six groups implemented and committed in the batching worktree. Focused checks pass; final full suites pass (controller 2,650; core 651), and workspace clippy passes with warnings denied. Parent owns review of the integrated milestones.
- [x] (2026-11-16) Milestone 3: live burst check on an m7i.xlarge in us-east-1a from the isolated instance `launch-perf-1541`, old build `2fa098787` against new build `511c007f6` on the same host. Results are in Outcomes & Retrospective. Host terminated afterwards.


## Surprises & Discoveries

- Observation: a byte-only store key loses executable modes when two paths have identical bytes but different modes. A filesystem probe with equal-content 0700 and 0600 files produced one shared inode; changing either link's mode changed both paths and the store. The parent approved separate `<sha256>` and `<sha256>.x` keys.
  Evidence: `profile_archive_tests::streamed_profile_roundtrips_through_system_tar_without_per_file_commands` includes equal-content executable and non-executable paths and checks their installed modes and store keys.
- Observation (Milestone 2 validation): the network Git transport fixture originally allowed only a `git` executable. It now supplies its URL rewrites through Git's per-command environment configuration so both direct Git invocations and Git inside a batch use the same transport mapping. The initial focused run passed 14 cases and rejected the batched case before executing it.
- Observation (Milestone 2 validation): mbx tracks the Git revision read by build scripts. Committing while Cargo was compiling caused `build-script inputs changed during compilation`; the interrupted check was restarted after all implementation commits, with the revision held fixed.

- Observation (Milestone 2): Cargo package names are `brokk-mj-core` and `brokk-mj-controller`; the short directory names in the original commands are not package selectors. Validation commands below use the package names.
- Observation (Milestone 2): overriding `settings.directory` before asking the resolution owner changes its cache key, so merely replacing `inspect_host` with `inspect` still repeats the launch inspection. The caller must first look up the original settings.
- Observation (Milestone 2): worker-store private-upload discard is best-effort today. A batch must report its failure without turning successful publication into an error; profile staging discard, in contrast, stays success-only.

- Observation: the four sessions each copying the worker binary into their containers was not repeated host uploads. Each container has its own worker store at `/var/lib/hel/workers/worker-binaries`, and all four reused the host's transport cache. The two `stage replacement Mjolnir worker` records per session are an SSH `true` followed by `podman cp`, and the `true` does nothing.
  Evidence: `mj-controller/src/controller/worker_binary/store.rs:7-24` (store and gate identity include the container), `upgrade.rs:683-693` (the `true` is chosen when `remote_source` is set).
- Observation: Claude writes into its own `plugins/` directory (marketplace metadata, directory cache). A single shared, writable plugin directory would therefore share mutable state between sessions, and a read-only mount would break Claude's writes.
  Evidence: strings in the pinned Claude Code 2.1.323 binary, saved by the profile mapper in `claude-pinned-plugin-evidence.txt`.
- Observation: the per-profile plugin trees differ between profiles (`~/.claude`, `~/.claude2`, … each have a distinct digest), but they hold mostly the same marketplace files at different versions. Storing content per file, not per tree, lets different profiles and later marketplace updates reuse most stored files.
- Observation: two of the launch's Git records report `succeeded=false` but are expected probes: a missing fresh-checkout marker (status 1) and an absent push refspec (status 5). `daemon/support.rs:134` treats only status 0 as success in the log.


## Decision Log

- Decision: encode the user executable bit in the content key: `<sha256>` is stored at 0600 and `<sha256>.x` at 0700. Both the digest query and archive link targets use that key. Restrict extracted private files before creating hardlinks, and discard any previous staging directory before extraction, so neither an initial upload nor a repeat runs chmod through a stored inode.
  Rationale: byte identity alone cannot preserve differing executable modes. This keeps the approved symlink-then-hardlink mechanism and private final copies. The existing upload's umask and permission restriction remove group and other permissions.
  Date/Author: 2026-11-15, parent ruling implemented by Milestone 1 worker.
- Decision: encode each step failure as an index, status and JSON-escaped purpose in stderr; reuse the existing command error checkers to decode it. Do not extend serialized `CommandSpec`.
  Rationale: a batch retains the original purpose in failures across SSH, container execution and command plans, without a persisted contract change. Every argv element uses `targets::posix_quote`.
  Date/Author: 2026-11-15, batching implementer.
- Decision: support best-effort cleanup as a step property. The helper returns warning records even when an earlier step fails, and worker publication logs them with `tracing::warn`. Keep the separate executor cleanup when transfer fails before the batch can complete.
  Rationale: preserve the existing warning-only discard policy and cancellation cleanup instead of hiding errors or failing a successful publication. Parent explicitly confirmed this policy.
  Date/Author: 2026-11-15, parent and batching implementer.
- Decision: configuration preparation first asks `inspect` with the original machine settings, then projects onto the recorded directory through that same owner only for a different or disabled retained cache. Keep `publish_at`'s compare-and-replace reread.
  Rationale: the ordinary launch reuses its existing resolution key, while retained containers preserve their recorded mount and policy handling. Do not normalize the owner's keys or introduce another cache. Parent explicitly confirmed this caller projection.
  Date/Author: 2026-11-15, parent and batching implementer.

- Decision: change only how profile bytes reach the host; each session still receives a private, writable copy of its whole profile.
  Rationale: Claude writes into `plugins/`, and Resume, Move and profile refresh all delete and reinstall the profile directory (`controller/resume.rs:811-818`, `worker_binary/install.rs:1043-1093`). Sharing the final tree would touch all of those flows. Sending bytes once per host removes the measured bottleneck and keeps the session's files exactly as they are today. The worker binary store's host transport cache (`store.rs:582-656`) already works this way.
  Date/Author: 2026-11-15, parent session.
- Decision: store content per file, keyed by its SHA-256 digest, not per directory tree.
  Rationale: tree digests differ between profiles and change on every marketplace refresh; per-file digests are shared across both.
  Date/Author: 2026-11-15, parent session.
- Decision: only harness-declared shared content trees go into the host store (for Claude, `plugins/` and `skills/`). Credentials, settings, native configuration, instructions, hooks and project memory stay in the per-session archive, exactly as today.
  Rationale: the store outlives the session. Secrets must not stay on the host after the session's own copy is gone. The shared trees are about 99% of the bytes, so nothing is lost by excluding the small private files. Mark the shared entries in the existing allowlist table in `mj-controller/src/controller/worker_binary/harness.rs:161-205`. Do not create a second list.
  Date/Author: 2026-11-15, parent session.
- Decision: the store does not mount, symlink or hardlink into a session's live profile. Hardlinks are used only inside the host staging directory, which the existing install steps copy from and then delete.
  Rationale: the session never shares an inode with the store, so a write by the harness cannot change stored content. Garbage collection can then decide by age alone.
  Date/Author: 2026-11-15, parent session.
- Decision: serialize the "which digests are missing, then upload them" decision per host with the existing daemon-local store gate (`store_gate` in `store.rs:33`), keyed by the host store path and an `SshBare` locator for the host (as `transfer_binary` does). Across daemons, publication converges through atomic rename.
  Rationale: the incident was concurrent sessions sending the same bytes. Without one decision point per host, each session would see the same digests missing and upload them in parallel.
  Date/Author: 2026-11-15, parent session.
- Decision: batch only commands whose inputs are known before the batch starts. Commands whose output feeds a local decision keep that decision local. They are batched only where the decision can be returned as a structured result without moving the state machine into shell.
  Rationale: the round-trip mapper showed that worker linking, mbx inspection and Git default-branch selection are state machines with distinct failure policies. Concatenating them into one `set -e` script would lose those distinctions.
  Date/Author: 2026-11-15, parent session.
- Decision: the late mbx configuration inspection reuses the existing resolution owner (`RESOLUTIONS` in `mj-controller/src/controller/mbx.rs:38`, `:102-156`) instead of calling `inspect_host` directly (`:392`). Do not add another cache.
  Rationale: one owner per fact (AGENTS.md concurrency rule). The duplicate native-version read and host inspection come from bypassing that owner.
  Date/Author: 2026-11-15, parent session.
- Decision: the install script copies stored content into the staging directory instead of hard-linking it. This supersedes the earlier hard-link wording.
  Rationale: `podman cp` sends a tar stream, and tar preserves hard links. Two profile files with identical bytes, both linked to one store file, would therefore arrive in the container as one shared inode. An in-place write by the harness (Node's `writeFileSync` writes into the existing file) would then change both. Copying costs about 22 MB of local disk traffic on the host per launch. It also means no staging or session file ever shares an inode with the store, so the ordering rule "restrict permissions before linking" is no longer needed.
  Date/Author: 2026-11-15, parent session (review of 50d715dcb).
- Decision: the install script decides completeness from the placeholder links that remain after copying, not from `find`'s exit status. Each copy goes through a temporary file that is renamed over the link, so any failure leaves the link in place. A remaining link whose store file is gone is reported as missing (status 75, one repeat). Any other remaining link fails the upload.
  Rationale: code review of 09769fc85 found that a copy failure such as a full disk could otherwise depend on whether `find -exec … +` reports the failed invocation. Checking the result directly does not depend on how a given `find` implementation behaves.
  Date/Author: 2026-11-15, parent session.

- Decision: remove the per-file process starts that caused the empty-store regression. Digests are verified with one `sha256sum` or `shasum` pass over all uploaded content files, compared by `awk`, with a count check that catches files a partial pass skipped. New content is published with batched `cp`, `chmod` and `mv` calls through `find -exec … +`, and `rmdir` of the upload directory proves every file moved. Placeholders are now relative symlinks (one `..` per path component) into `.mj-store`, a link the script creates in a private work directory beside the extracted tree. The archive also carries `.mj-keys`, the list of shared keys, so the missing-content check uses shell builtins only. One `cp -RL` then copies the whole tree into the destination with links dereferenced, under umask 077. This supersedes the per-link copy loop.
  Rationale: on the m7i.xlarge test host, per-file process starts cost 1.3 s for digests and 2.2 s for copies over 725 files. They made the first launch on an empty store slower than the old single archive, and were most of the transfer time in every later launch. Checked on Linux by the three profile archive tests, and on macOS by running the script against a hand-built archive: first upload, repeat upload, missing content.
  Date/Author: 2026-11-16, parent session.

## Outcomes & Retrospective

Milestone 1 is complete. Only harness-declared shared trees enter the host content store; each launch still installs a private profile. The real-shell integration tests cover more than 8 MiB of content, spaces, newline and leading-dash filenames, empty directories, distinct executable modes for identical bytes, collection of stale and abandoned content, an upload without shared content on the second launch, independent installed inodes, and a missing-content repeat. Full validation passed with `cargo test -p brokk-mj-controller` (2,651 passed, 27 ignored), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. The first round-trip run exposed a fixture omission: the helper now applies the real SSH installer's final chmod after copying, and the targeted rerun and full suite pass. Milestones 2 and 3 remain separate work; no live EC2 or macOS check was run in this lane.
Milestone 2 validation completed on implementation revision `0200e9d33`. `cargo test -p brokk-mj-core -p brokk-mj-controller` passed: controller 2,650 passed and 27 ignored; core 651 passed and 4 ignored; doctests passed with one pre-existing ignored core doctest. `cargo clippy --all-targets -- -D warnings` passed in the dev profile. `cargo fmt --all -- --check` and `git diff --check` passed. Focused checks covered the real-shell helper partial failure, worker install/store paths, inherited Git settings (5), network Git initialization (15, including the corrected transport fixture), and mbx (75). The initial shim and network fixture expectations were updated to the new batch shape; no checked-in golden files changed. One additional test-fixture commit follows the six group commits. The parent requested one integrated review after both milestones, so this worktree did not request its own review.

The batching worktree did not run live EC2, Docker's ignored live cases, or native macOS checks; Milestone 3 remains with the parent. Text evidence is under `/home/jonathan/Projects/mjolnir/.mj/clones/a83a99501dd0ab62165f069bce1a74a8/.mj/agents/ea3f2d728024eb4d7ae9fb5abd44d51d`, with `full-tests.log`, `clippy.log`, the focused logs and initial failure logs. The final plan-only commit contains these results and no source changes.

Milestone 2 implementation estimate for the incident sequence is 69 commands to about 36 when the existing mbx resolution is available: profile install saves 6, worker replacement saves 6 including deletion of the useless SSH `true`, worker publication saves 2, inherited Git settings save 2, workspace Git configuration saves 6, and mbx inspection reuse plus shim/config batching save 11. This is a code-path estimate, not a timing measurement; the profile-store digest query from Milestone 1 can add a command back. SCP remains separate when a replacement has no remote source. No output-dependent local decision moved into shell. Milestone 3 still measures the combined implementation on a real isolated host.


Milestone 3 (2026-11-16). Method: one isolated instance with a Claude profile holding a copy of the real `plugins/` and `skills/` and no credentials, the public `octocat/Hello-World` bundle, and an m7i.xlarge EC2 host created with `mj ec2 create`. For each build: one warm-up session, then four sessions started at once, with no prompt. Times are the `provision session` launch phase from the instance's daemon log. Each build was run once.

    Old build 2fa098787            New build 511c007f6
    warm-up   104.4 s, 75 cmds     warm-up   111.4 s, 54 cmds
              upload 12.5 s                  query 0.3 s + send 20.4 s (11,523,317 B, 725 stored files)
    burst     56.6-67.6 s each     burst     24.9-36.9 s each
              62 cmds each                   43 cmds each
              upload 10.2-32.0 s             query 0.7-0.8 s + send 2.9-3.7 s (about 73 KB, 0 stored files)

The burst sessions sent no shared content and started in roughly half the time. Remote commands per launch fell from 62 to 43. The warm-up count includes one-time host work.

One regression: the first launch on a host with an empty store took about 8 s longer (20.4 s against 12.5 s for the transfer). On the host, per-file process starts measured 1.3 s for the per-file digest check over 725 files, against 0.02 s for one `sha256sum` call over all of them, and 2.2 s for the per-file copy loop. That accounts for about 3.5 s; the rest is unexplained with one sample of each, and network variation is possible. The same copy loop is most of the roughly 3 s `send profile content` time in burst sessions. A follow-up could verify digests in one call and reduce per-file process starts in materialization.

Regression fix measured (2026-11-16) on a second m7i.xlarge in us-east-1, build `3e2ad1314`, with the same method:

    warm-up   94.9 s, 54 cmds, query 0.3 s + send 14.4 s (11,573,528 B, 725 stored files)
    burst     16.8-23.0 s each, 43 cmds each, query 0.2-0.3 s + send 0.4-0.5 s (about 110 KB, 0 stored files)

Burst sessions now spend under a second on the profile transfer, against 3.6-4.5 s before the fix and 10-32 s with the old build. The first launch on an empty store spent 14.7 s on the transfer, against 20.7 s before the fix and 12.5 s with the old build. The remaining 2 s gap is within what one run per build on different hosts can show. Burst archives grew from about 73 KB to about 110 KB, because each placeholder link now carries a relative path.

## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`) creates sessions. A session runs a worker process (crate `mj-worker`) next to a harness (the coding agent CLI, such as Claude Code). A target is where the worker runs. An `ssh-podman` target is a remote Linux host reached over SSH, where each session gets its own Podman container. EC2 machines created with `mj ec2 create` are registered as `ssh-podman` targets. An `ssh-bare` target runs the worker directly on a remote host, without a container. The controller is the machine running the daemon.

Every remote command is a `CommandSpec` (in `mj-core/src/targets.rs`) with a `purpose` text and a provisioning stage. The executor runs it over a shared SSH connection; each host has one SSH master connection (`ControlMaster=yes`), and commands open channels on it (`mj-core/src/targets/ssh.rs:2083-2087`, `:2339-2365`). The daemon logs one `session command stage finished … purpose=… duration_ms=…` record per command (`mj-controller/src/daemon/support.rs:118-161`). Launch phases are timed by `mj-controller/src/launch_timing.rs` and logged as `session launch phase started/ended`.

Profile staging: `Controller::stage_worker_files_for` (`mj-controller/src/controller/worker_binary/launch.rs:212`) builds a private local directory. `stage_profile` (`worker_binary/harness.rs:207-270`) copies the harness's allowlisted files from the profile home, following symlinks so the stage holds only regular files and directories. Session-specific overlays are then added (`launch.rs:305-381`). `upload_profile_archive` (`worker_binary/install.rs:1148-1185`) packs the stage as one gzip tar and extracts it into a host directory with `umask 077` and `chmod -R go-rwx`. Two callers use it. `install_worker_over_ssh` (`install.rs:1099-1144`, ssh-bare) extracts into `<profile_home>.incoming`, then runs `cp -R` into the profile home. The remote-container branch (`install.rs:932-1038`) extracts into `~/.cache/mjolnir/uploads/<session>/profile` on the host, then runs seven separate commands. These create the container directories, `podman cp` the launch file, the ownership file and the profile into the container, match ownership, restrict permissions, and remove the host staging directory.

The worker binary store (`worker_binary/store.rs`) is the precedent for per-host content. It keys immutable files by SHA-256 and publishes them by writing a unique temporary file and renaming it. It serializes decisions with `store_gate` (a daemon-local, cancellable gate). `transfer_binary` (`:582-656`) installs the binary into a host-level store once, then copies it into each container.

Remote command groups identified in the incident window (file:line from the round-trip mapper):

1. Remote-container profile install, seven commands with no output-dependent decisions: `worker_binary/install.rs:970-1033`.
2. Worker replacement plan, seven commands including a useless SSH `true`: `worker_binary/upgrade.rs:680-713`.
3. Container worker store verify, publish and private-upload cleanup after a copy: `worker_binary/store.rs:475-571`.
4. Three inherited global Git settings (`pull.rebase`, `user.email`, `user.name`) written one per command: `controller/provisioning.rs:1914-1927`.
5. Network Git initialization: a marker read, default-branch selection, branch resolution and switch, then six `config --replace-all` writes and one `--unset-all`: `controller/network_git.rs:225-298`.
6. mbx (the Rust build cache tool) setup: a repeated native-version probe and a full host inspection that bypasses the existing resolution cache (`controller/mbx.rs:392`, `:769`), plus `install the shared-cache mbx shims` and `link the shared machine mbx configuration`, which can run as one script.

No existing helper runs several steps in one remote script and reports which step failed. `CommandPlan::execute` (`mj-core/src/targets.rs:1898`) still issues separate commands.


## Plan of Work

### Milestone 1: host profile content store

At the end of this milestone, a second session that starts on a host with the same profile sends only its small private files. The daemon log for it shows a `send profile content` record (or the chosen purpose) with a small byte count.

Mark shared content trees in the allowlist table in `mj-controller/src/controller/worker_binary/harness.rs`: `plugins/` and `skills/` for Claude, Kimi and Grok; `skills/` and `rules/` for Codex and Muse; `agents/` for Kimi; `skills/` for every other harness. The Mjolnir-managed skill under `skills/` is version-dependent but not secret, so it may be stored. Expose this to the upload, so it knows which top-level paths of the stage are shared.

Replace the internals of `upload_profile_archive` while keeping its contract: afterwards, the destination host directory holds exactly the stage's files and directories, with the same executable bits, owned by the login user and readable only by it. Both callers stay unchanged apart from passing the shared-path set.

The new flow has two SSH commands.

The first command is a digest query. Under the per-host store gate, the controller sends the list of SHA-256 digests of the shared files, one content key per line (a lowercase SHA-256 digest, with `.x` appended for user-executable files), on stdin. A remote `sh` script creates the store directory `~/.cache/mjolnir/profile-content` (mode 0700) if needed. It updates the modification time of each digest file that exists and prints each digest that does not. Updating the time is what keeps garbage collection from removing a file between the two commands.

The second command is the upload. The controller streams one gzip tar containing three kinds of entries: the private files at their own paths; each missing shared file as `.mj-content/<key>`; and each shared file whose digest is already stored as a symlink at its own path, whose target text is `mj-profile-content/<key>`. The remote script discards any earlier staging directory and extracts the tar into the destination. It moves each `.mj-content/<key>` into the store: it first verifies the digest with `sha256sum`, or `shasum -a 256` on macOS, then writes a unique temporary name in the store and renames it. If the store already has the file, it discards the new copy. The script then replaces every symlink whose target starts with `mj-profile-content/` with a hard link to the store file named by the target's last path component. It does this with `find … -type l -exec sh -c '…' sh {} +`, so file names pass as arguments and are never parsed from text. It applies `go-rwx` to extracted directories and regular files before linking any stored inodes, then removes `.mj-content` after linking. Missing files that the stage held only as `.mj-content` entries must also appear at their real paths. Give those a symlink entry too, so one mechanism places every shared file.

Because staging follows symlinks today, the stage holds no symlinks of its own, so every symlink in the extracted tree is one this flow created. Keep the target-prefix filter anyway.

Garbage collection runs inside the digest query, at most once per hour per store; record the last run in a stamp file. It deletes store files whose modification time is more than one day old. It uses the same one-day and one-hour values as the worker store's collection (`store.rs:197-280`); cite that source in a comment. No session file shares an inode with a store file after its staging directory is removed, so age is the only criterion needed.

If a store file disappears between the two commands, for example because a different daemon's collection ran, the link step fails. The upload command then exits with a distinct status and prints the missing digests. The controller repeats the upload once, with those digests sent as content. A second miss is reported as an error. This retry is bounded because its input changes: the missing files are sent.

Local targets keep their current copy path; they have no network transfer.

### Milestone 2: step scripts and batching

At the end of this milestone, the incident launch issues about 40 remote commands instead of 69. Every batched failure still names the step that failed, in the same words as today's purpose text.

Add one shared helper next to `CommandPlan` in `mj-core/src/targets.rs`. It builds one `sh -c` script from an ordered list of steps. Each step has an argv, a purpose, and the exit statuses it accepts (default: 0 only). It runs the steps in order. When a step's status is not accepted, it prints a marker line to stderr naming the step index and status, then exits with that status. The Rust side decodes the marker into an error that names the failing step's purpose and includes its stderr, in the same format `execute_checked` produces today. Steps marked as cleanup run whether or not earlier steps failed, through an `EXIT` trap. Quote every argument with the existing shell-quoting helper; search for it before writing one. The command's logged purpose is the batch's purpose. Cancellation behaves as it does for any single command: the process group is stopped, and cleanup steps run where the trap can still run.

Use the helper for the following groups, in this order, each as its own commit:

1. The seven remote-container profile install commands in `worker_binary/install.rs:970-1033`. Host staging removal is the final ordinary step, so it runs only after earlier steps succeed, and its failure still fails installation. It deliberately does not use the always-run cleanup trap; failure cleanup remains unchanged.
2. The worker replacement plan in `worker_binary/upgrade.rs:680-713`. Delete the SSH `true` chosen when `remote_source` is set.
3. Container worker store verify, publish and private-upload discard (`store.rs:475-571`). Keep the store gates and the final session link exactly where they are.
4. The three inherited global Git settings in `controller/provisioning.rs:1914-1927`.
5. The six `config --replace-all` writes and the `--unset-all remote.origin.push` (accepted statuses 0 and 5) in `controller/network_git.rs:284-298`. Keep the marker read, default-branch selection and branch resolution as they are.
6. mbx: make `prepare_session_configuration` obtain host facts from the existing resolution owner (`mbx.rs:102-156`) instead of `inspect_host`. Keep the recorded directory projection used for retained configuration (`:388`, `:403`), and keep the compare-and-replace reread in `publish_at` (`:357-364`). Run `install the shared-cache mbx shims` and `link the shared machine mbx configuration` as one step script.

Do not move any local decision into shell in this milestone.

### Milestone 3: live check

From an isolated instance, create a small EC2 host with `mj ec2 create`. Start one session, then start four sessions with the same Claude profile at the same time. Record the `provision session` and `prepare worker files` phase times and the number of command records per launch, from that instance's daemon log. Then destroy the host.


## Concrete Steps

Work from the repository root. Implementers build and test only in their own detached worktree (`git worktree add --detach ~/Projects/mjolnir-wt/<name> HEAD`) and commit on its detached HEAD. The parent cherry-picks the commits onto master.

Focused tests during the work:

    cargo test -p brokk-mj-controller worker_binary
    cargo test -p brokk-mj-core targets

Full validation once, after the last round: `cargo clippy --all-targets -- -D warnings` for the workspace, plus `cargo test -p brokk-mj-core -p brokk-mj-controller` for the full suites of the touched crates. Run `cargo test` outside the restricted sandbox and keep the source and Git revision fixed until the commands finish.

Live check (Milestone 3), from an isolated instance only:

    mj --instance launch-perf ec2 create <small instance type>
    mj --instance launch-perf new … (five sessions; four at once)
    grep 'session launch phase ended' ~/.local/share/mjolnir/instances/launch-perf/logs/mj-daemon-*.log

Adjust the log path to the instance's data directory.


## Validation and Acceptance

Milestone 1 acceptance: an integration test runs the real remote scripts with `sh` against a temporary host directory standing in for the SSH host. These tests already exist in the style of `mbx_shim_tests` in `install.rs`. The test stages a tree whose shared part is larger than 64 KB and has file names with spaces, a newline and a leading dash, plus empty directories and an executable file. It uploads it twice, into two destinations, and checks three things. First, both destinations are byte-for-byte equal to the stage, including modes after `go-rwx`. Second, the second upload's tar holds no shared content. Third, no destination file shares an inode with a store file after the staging directory is removed. A second test deletes one store file between the digest query and the upload, and checks that the single repeat succeeds. Run the scripts with both GNU and BSD-compatible flags in mind; macOS ssh-bare hosts run BSD tools.

Milestone 2 acceptance: for each batched group, an existing test of that code path keeps passing. A test of the step-script helper runs a real `sh` and shows three things: a failing middle step is reported with its own purpose and stderr; an accepted nonzero status (5) continues; and a cleanup step runs after a failure. That helper test is a tricky-condition test (partial failure); do not add happy-path unit tests.

Milestone 3 acceptance: in the four-session burst, only the first session's upload carries plugin content. The other three report uploads of under 1 MB. Each launch's `provision session` time is recorded in this plan. Command records per launch drop from about 69 to about 40 in the incident's window.


## Idempotence and Recovery

The digest query and the upload can be repeated safely. Store publication is a rename of a verified temporary file, so a crash leaves at most an orphaned temporary file. Collection removes temporary files older than one day along with old store files. Deleting `~/.cache/mjolnir/profile-content` on a host is always safe: the next launch sends everything again. EC2 hosts created for the live check must be destroyed afterwards with `mj --instance launch-perf ec2 destroy`.


## Artifacts and Notes

Incident timing (daemon log, session 2b0ac47b):

    plan checkout and provision target   8098 ms
    prepare worker files               169044 ms
      upload staged harness profile archive  61926 ms
      ~45 sequential commands at 2.5-5 s each after 05:39:50
    initialize workspace                27840 ms
    provision session                  211108 ms

Claude profile source measurement (fast gzip): whole snapshot 780 files, 22,085,092 bytes, 10,663,949 compressed. Of that, `plugins/` is 759 files, 19,329,159 bytes, 10,009,850 compressed, and `skills/` is 600,357 bytes compressed.


## Interfaces and Dependencies

No new crates. Use the existing `tar` and `flate2` dependencies for the stream and `sha2` (already used by the worker store) for digests. Run commands through the existing executor (`execute_with_stdin` for the streamed upload); do not hand-roll pipes. The step-script helper lives in `mj-core/src/targets.rs` beside `CommandPlan` and returns errors through the same `anyhow` context style as `execute_checked`. No database migration and no relay protocol change are expected; if one becomes necessary, stop and record why in the Decision Log.

Plan update (2026-11-15, Milestone 1 worker): implemented the host content store and recorded the parent-approved executable-mode key refinement; focused integration tests, the full controller suite, workspace clippy and formatting all pass.
No new crates. Use the existing `tar` and `flate2` dependencies for the stream and `sha2` (already used by the worker store) for digests. Run commands through the existing executor (`execute_with_stdin` for the streamed upload); do not hand-roll pipes. The step-script helper lives in `mj-core/src/targets.rs` beside `CommandPlan` and returns errors through the same `anyhow` context style as `execute_checked`. `CommandStep::new(purpose, argv)` declares an operation; `accepted_statuses` declares accepted exit codes, `cleanup` runs it in the exit trap, and `best_effort_cleanup` reports failure without replacing the primary result. `step_script` returns the argv for the existing transport factories. `step_script_error` and `step_script_warnings` decode indexed status/purpose markers; purposes use JSON escaping and argv words use `posix_quote`. The core and controller error checkers decode failures, and worker publication logs all cleanup warnings before checking the result. No database migration and no relay protocol change are expected; if one becomes necessary, stop and record why in the Decision Log.

Plan update (2026-11-15, Milestone 2): recorded batching implementation, preserved failure policies, the original-settings resolution lookup and corrected Cargo package selectors. Validation completed: the focused checks, both full crate suites and workspace clippy pass. The source revision stayed fixed during final validation; the final commit touches only this plan.

Plan update (2026-11-16, parent): recorded the Milestone 3 live measurement and the cold-store regression.

Plan update (2026-11-16, parent): replaced per-file process starts in the install script after the measured empty-store regression.

Plan update (2026-11-16, parent): recorded the measurement of the regression fix; work complete.
