# Run a Linux daemon from a native Windows SSH client (#1514)

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`. Update Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective at every stopping point. All paths below are relative to the repository root unless stated otherwise.

## Purpose / Big Picture


A person can run the daemon (the process owning sessions, settings, database and workers) on a separate Linux computer and use native `mj.exe` on Windows to control it over SSH. Running `mj --instance remote-dev sessions` on Windows will start or reach the Linux daemon without creating a Windows daemon or store. The dashboard will complete Linux paths and discover Linux harness installations. HTTP commands and the browser use the Linux computer's advertised viewer URL directly. One PowerShell command will install matching development builds on both computers; release users install the same release on both.

This is private-network operation with working SSH authentication and host trust already configured. NAT traversal, exposing the control socket publicly, and publishing a remote container's viewer are outside this work. The native desktop shell (`mj app`, crate `mj-desktop`) is removed rather than adapted; the browser is the graphical client.

## Progress


- [x] (2026-11-12) Read the complete plan standard, both research reports, container and local HTTP routing plans, installation scripts, and build identity implementation; verify principal references in current source.
- [x] (2026-11-12) Record settled design, including the user's correction that EC2 CLI commands are forwarded.
- [ ] Implement and validate milestone 1: endpoint, connector, supervised tunnel and command forwarding.
- [x] (2026-11-12, foundation worker) Add supervised subprocess ownership with concurrent stderr capture, exit observation and cancellation cleanup; add SSH placement fields and placement-only loader. Remaining foundation work: connector, endpoint and tunnel; no remote acceptance claim yet.
- [ ] Implement and validate milestone 2: direct HTTP credentials and browser opening.
- [ ] Implement and validate milestone 3: dashboard paths, discovery and preflight.
- [ ] Implement and validate milestone 4: dashboard setup, probes, settings and cache work.
- [ ] Implement and validate milestone 5: explicit remote paths, attachment uploads, client-side import upload and daemon-side voice transcription.
- [ ] Remove the native desktop shell (`mj-desktop`, `mj app`, `mj-cli/src/desktop.rs`, release packaging); independent of the other milestones.
- [ ] Implement and validate milestone 6: matching-build PowerShell go script.
- [ ] Implement milestone 7: user documentation, final validation, platform dispatch and real Windows acceptance.

## Surprises & Discoveries


`mj-client/src/build_identity.rs::BuildIdentity::same_source` compares release version and a present Git revision, ignoring executable timestamps. It does not hash dirty source. Two differently edited trees at one HEAD can pass. This does not change the chosen matching rule; the development script must build clean, immutable checkouts and disclose that the rule alone cannot detect dirty builds.

`mj-cli/src/api_client.rs::ApiClient::connect_to` currently requires `local_address`, calls `route_viewer_to_local_socket`, and reads `api_token_path()` from client disk. No API-token acquisition action was found in `mj-client/src/daemon.rs`. Remote operation needs an authenticated control action; copying daemon cookie signing keys is unnecessary and prohibited by this design.

The container plan says repository preflight moved, but only remote-repair preflight moved: `mj-cli/src/dashboard/actions.rs` still calls `preflight_new_session` locally. The existing HTTP handler `mj-controller/src/server/handlers.rs::preflight_new` is the implementation to share. This is a scope clarification, not a contradiction of the user decisions.

The suggested disposable sshd exists as `LocalSshd` in `mj-core/src/targets/ssh.rs`, not as a localhost fixture under `tests/e2e/`. `tests/e2e/ssh_docker_acceptance.py` uses a cloud lab ledger; do not provision EC2 to test this feature. Reuse the local sshd fixture's key/configuration/lifetime pattern in the new driver. The Windows installer is root `install.ps1`, not `scripts/install.ps1`.

No existing checked-in control-daemon exchange golden was located by searching daemon modules and golden filenames. Existing HTTP goldens include `mj-controller/tests/golden/phone-new-session-preflight.txt` and `viewer-settings-exchange.txt`. Milestone 3 establishes one real control exchange golden and extends it in later milestones, while extending those existing HTTP cases for shared behavior. Do not invent a nonexistent baseline test.

Foundation discovery (2026-11-12): existing Windows subprocess helpers create process groups but have no job-object ownership. The new supervised helper adds a kill-on-close job; real Windows validation remains necessary. Placement-only deserialization ignores daemon-owned tables, including structurally invalid profiles, without credential resolution.

## Decision Log


Decision: Add `[daemon] runtime = "ssh"`, `host`, optional `instance`, and an SSH argument list `ssh_args` using existing `SshTarget` conventions. The remote instance defaults to the client's selected instance, including default. Only this placement table is client-owned; other settings come from daemon settings APIs. Remote host/container placement is separate and never recursively follows another SSH placement. Rationale: paths, profiles and credentials must have one machine owner. Date/Author: 2026-11-12, user design; argument-list field spelling specified by this plan.

Decision: The remote program is `[daemon] remote_command`, default `mj`, run through the remote user's login shell so a PATH set in shell profiles (for example `~/.cargo/bin`, where `scripts/install.sh` installs) applies; a plain non-interactive SSH command often lacks it. Reuse the login-shell wrapping that SSH targets already use if one exists (`mj-core/src/targets/ssh.rs::remote_command_line` and the macOS PATH helper are the starting points) rather than adding a second rule. When the remote reports the command missing (exit status 127), the client says so and names `remote_command` and the remote install command. Rationale: the first real run would otherwise fail with "mj: command not found" on a box where `mj` works interactively. Date/Author: 2026-11-12, parent review.

Decision: Remote `mj daemon endpoint --json` owns startup through existing `connect_or_start`. Generalize container CLI forwarding into one placement dispatcher, including daemon status/stop/restart, setup, login, doctor and EC2. Import is not forwarded; see the import decision below. Rationale: only the remote launcher may use its lock, store guard, process IDs or container engine. EC2 configuration and keys belong there too. Date/Author: 2026-11-12, user design and subsequent EC2 correction.

Decision: Require `BuildIdentity::same_source`, with both builds named on mismatch and instructions to run the go script or install the same release. Do not push binaries automatically. Rationale: different OS executable timestamps are irrelevant, but source compatibility matters during heavy development. Date/Author: 2026-11-12, user.

Decision: Each client process owns one supervised `ssh -N -o ExitOnForwardFailure=yes -o BatchMode=yes -L 127.0.0.1:<local>:127.0.0.1:<remote-port> <host>` child, without dependence on ControlMaster. Select the local port by binding loopback port zero, reading the assigned port and releasing it immediately before spawning SSH. A confirmed local bind collision retires that attempt and selects a new port; no fixed retry count. Other refusals are reported. Readiness is authenticated Ping through that tunnel or child exit with captured stderr. Rationale: preserves `DaemonClient`'s numeric local socket and works with Windows OpenSSH. Date/Author: 2026-11-12, user; port selection resolved here.

Decision: One placement-aware connector owns discovery, initial connection, request reconnect and attachment reconnect. Refresh the remote endpoint and replace the tunnel after handoff; a lost tunnel says nothing about daemon liveness. Replay only explicitly unaccepted requests. Disable remote executable relaunch; retain drafts and show the build repair instruction. Rationale: prevents duplicate mutations and cross-OS executable assumptions. Date/Author: 2026-11-12, user.

Decision: HTTP and browser connections use the advertised viewer URL directly, with API credentials obtained over authenticated control. SSH placement ignores `local_address`; host/container local routing retains #1440 behavior. `mj app` opens the default browser remotely. Loopback advertisements produce an actionable notice identifying remote `[phone]` settings. Remote container viewer publishing is deferred. Rationale: SSH transports control only; the browser must reach the server independently. Date/Author: 2026-11-12, user.

Decision: Move every dashboard machine operation listed in milestones 3 and 4 to daemon actions for all placements, reusing existing interpretation and jobs. Upload image bytes before sending attachment references. Rationale: fixes the same shared-filesystem assumptions in container placement and advances #1043. Date/Author: 2026-11-12, user.

Decision (user, 2026-11-12, supersedes the earlier deferrals): Remove the native desktop shell: the `mj-desktop` crate, `mj app`, `mj-cli/src/desktop.rs` and its cookie bootstrap, and its release packaging. The browser pointed at the advertised viewer URL replaces it. Voice records audio on the client and sends it to the daemon, which runs transcription (the voice worker) and returns text; the client no longer reads daemon-profile credential files. Import reads native harness transcripts on the client machine (the common case: the person's own Claude Code or Codex history) and uploads them to the daemon, which registers the session; this is one mechanism for every placement, and it also fixes container placement, where `mj import` currently scans the container's empty homes. The web viewer has no import. An imported session's project path is a daemon-host path the person chooses when it differs from the transcript's recorded path. Rationale: the dashboard and web viewer are viewers; the daemon coordinates workers; input devices and the person's own history live on the client.

Decision: SSH launches require explicit remote project paths wherever cwd was the default. Login uses an interactive SSH terminal; document callback limitations without inventing a harness compatibility matrix. Rationale: client cwd and daemon credential files cannot be used across machines. Date/Author: 2026-11-12, user.

Decision: Default development commit is the remote source checkout's HEAD, resolved once to a full SHA; optional `-Commit` overrides it. Both machines build in one reused, dedicated, clean, detached build worktree each, moved to that SHA (parent review: per-SHA worktrees would make every build cold), without switching the user's current branch. Rationale: the box is the development source of truth, origin/master could omit local committed work, and edits or commits during install split binary stamps. The script's explicitly requested detached build checkouts are build inputs, not development branches. Date/Author: 2026-11-12, plan author within the user's script choice.

## Outcomes & Retrospective


Planning is complete; no product code, installation, daemon operation or commit is part of this assignment. Implementation and measured acceptance remain unchecked. The main implementation risk is incomplete removal of client filesystem assumptions; use the inventory embedded below and the real separated-directory test rather than declaring success after tunneling Ping.

## Context and Orientation


`mj-cli/src/main.rs` dispatches `mj` commands; `mj-cli/src/daemon.rs::connect_or_start` coordinates local startup and handoff, and `daemon/container.rs::exec_in_container` currently forwards selected commands. `mj-core/src/config/daemon.rs::{DaemonConfig,DaemonRuntime}` defines placement. `mj-client/src/daemon.rs` defines `DaemonAction`, JSON framing, `DaemonClient`, `connect_existing`, and `request_with_reconnect`. A control token authenticates those framed TCP requests; it is distinct from HTTP's API token. A tunnel forwards a client loopback TCP port to the remote loopback port without exposing the daemon listener to the network.

`mj-core/src/targets/ssh.rs::{ssh_command,ssh_command_owned,remote_command_line}` owns SSH argument quoting and configured options; commands go through shared subprocess helpers, not manual pipe management. The existing SSH session lease is designed for multiplexed remote commands. A forwarding child must deliberately avoid requiring that Unix-only machinery; do not just attach `ssh_session` metadata and assume it is portable. Preserve operator SSH configuration and known-host policy; never add accept-new or silently degrade sharing.

`mj-cli/src/dashboard/actions.rs` initiates user work and `dashboard/io/spawn.rs` runs background effects. Their config-only Controller still executes machine work locally. `mj-controller/src/daemon.rs` and its submodules dispatch control requests; HTTP counterparts live in `mj-controller/src/server/handlers.rs`. `mj-client/src/settings/` holds the shared settings model. `mj-chat/src/chat/{remote,attachments}.rs` currently installs pasted images in `AttachmentStore::controller` on the client. Clipboard, terminal rendering, notifications, explicit input-file reads and output-file writes remain client work.

The checked-in container placement plan `.agents/plans/container-native-daemon.md` and completed `.agents/plans/local-daemon-api-routing.md` describe existing behavior to preserve. This plan includes their necessary constraints: a remote launcher may itself own a container, its data stays in its volume, and only local clients route HTTP to `local_address` while preserving TLS hostname checks.

## Plan of Work


### Milestone 1: a usable remote control CLI


Add SSH placement fields in `mj-core/src/config/daemon.rs`. Split client placement loading from daemon settings loading so startup does not discover profiles, credentials or sessions from Windows config. The client table must survive settings updates without replacing the remote daemon's own `[daemon]` table. Use neutral preconnection display defaults in `mj-cli/src/splash.rs`; authoritative spinner and settings arrive after connection. Refuse a remote endpoint command encountering another SSH placement with the exact remote config path and instruction to choose host/docker/podman there; never recurse.

Add the machine command in `mj-cli/src/main.rs` and its implementation in `mj-cli/src/daemon.rs`. It calls existing local `connect_or_start`, then prints exactly one JSON object with `protocol_version`, `build_version`, `control_address`, `token`, and `instance_id`. Only stdout carries JSON; launch notices go to stderr. For a remote container, return the box's published loopback control endpoint, not the container's private address. Do not alter frozen management replies to add these fields. Check build/protocol compatibility before ordinary commands.

Introduce `mj-client/src/daemon/connector.rs` with `DaemonConnector`, `ConnectionEndpoint` and `ConnectionLease`; the lease owns the transport supervisor and endpoint generation. Existing clients retain a shared lease rather than spawning a tunnel per request. Put lifecycle startup policy in the CLI as today, but pass the connector through every `DaemonClient` connection, including `connect_existing`, `request_with_reconnect`, and `mj-cli/src/daemon.rs::maintain_attachment`. There must be no dependency from mj-client back to mj-cli. A reconnect asks the same connector to refresh; a serialized refresh prevents simultaneous attachment and request recovery from making independent tunnels. Close/reap the old child before replacing it. Remote tokens and endpoints remain process-private, never masquerading as local `daemon.json` with a remote PID.

The SSH supervisor uses shared subprocess infrastructure with concurrent stderr draining, cancellation and child-process-group teardown. If infrastructure lacks a supervised long-lived child API, extend it there rather than implementing pipes at this call site. Endpoint execution uses existing quoting. The tunnel is a direct OpenSSH child with configured arguments and no required ControlMaster, on Unix and Windows. Select port zero as recorded above; distinguish OpenSSH's local-listen refusal from authentication, host-key, DNS, remote forwarding and command errors. Only a confirmed collision selects another local port. Ping attempts wait with backoff or a child/state event between attempts; caller cancellation terminates work. Do not invent a tunnel startup timeout or retry cap. Stderr, not speculative PID probes, explains SSH failure. Concurrent processes may each own their own tunnel.

Create one `run_where_daemon_lives(args, interactive)` dispatcher replacing `exec_in_container` call-site branching in `mj-cli/src/main.rs`. Host executes locally, container delegates to existing engine execution, SSH invokes quoted `mj --instance <remote> ...`. Preserve exit status, stdout/stderr, stdin and terminal ownership. Forward setup, login, doctor, EC2 and daemon status/stop/restart before touching local stores or EC2 state. Login and terminal setup use `ssh -t`; JSON commands do not allocate a terminal. A remote host may then dispatch inside its container. Command acknowledgement loss is an error, never permission to run the mutation again.

Exclude SSH placement from `upgraded_daemon_executable` in `mj-cli/src/daemon.rs` and relaunch handling in `dashboard/upgrade.rs`. On incompatible handoff show both build identities, preserve in-memory and persisted drafts, keep quit responsive, and instruct running `scripts/dev-remote.ps1` or installing the same release. Audit `main.rs::refuse_unknown_workspace`, logging and stopped-workspace listing so SSH never consults local SQLite or daemon PIDs. Remote settings/workspace reads go through the connector.

Validate with the real SSH driver described below and focused commands `cargo test -p brokk-mj-client daemon` and `cargo test -p brokk-mjolnir daemon`. A usable checkpoint is `mj --instance ssh-e2e sessions`, plus forwarded status, stop and restart, with no client daemon/store created. Commit the validated coherent change on the current branch when implementing, per repository rules.

### Milestone 2: direct HTTP and browser access


Add `DaemonAction::ApiCredential` returning the existing API bearer token through authenticated control, handled by the daemon using the same token owner as HTTP authentication. Never return the cookie signing key. Use this for all placements in `mj-cli/src/api_client.rs::connect_to`; remove the client token-file read. Branch routing by the connector's placement: SSH uses `viewer_url` unchanged, ignores `local_address`, preserves certificate fingerprint and TLS hostname verification, and does not install a loopback DNS resolver. Retain redirect/proxy security policy deliberately; do not weaken certificate checking to make a remote URL work.

The desktop shell is removed in its own change (see the Decision Log), so there is no `mj app` to adapt. When the viewer is reachable, `mj daemon status` prints its URL. A loopback advertisement returns: “The viewer is unreachable from this computer. On <host>, enable [phone] tailscale_detect with working Tailscale, or set [phone] bind to a reachable address and supply tls_cert and tls_key.” The notice includes the exact remote config path and `ssh <host> mj --instance <remote> setup` as an available next step. An unreachable viewer must not prevent control CLI or dashboard use.

Run `cargo test -p brokk-mjolnir api_client` and the milestone-1 driver with a reachable non-loopback advertised URL and a loopback-only case. Expect authenticated HTTP requests to reach the remote listener even when local_address points somewhere unusable on the client. Wrong certificates must still fail. Record a browser-open observation, then commit this usable HTTP checkpoint.

### Milestone 3: dashboard paths and project creation use the daemon


Define typed control actions and replies in `mj-client/src/daemon.rs` for CompletePath, CheckProjectDirectories, DiscoverProjects, ResolveContainerPath, ResolveSetupPath, ValidateMountSource, ValidateSessionMounts and PreflightNewSession. Reuse current Controller path interpretation, project-picker discovery and `server/handlers.rs::preflight_new`'s shared logic; move shared interpretation to one owner rather than copying handlers into the control server. Rewire corresponding `dashboard/actions.rs` branches, including both ValidateProjectDirectory and final new-session preflight. Paths entered or displayed here always mean daemon/selected-target paths, including Windows clients typing Linux paths.

Implement handlers in daemon request dispatch with supervised background work. Results carry request identities so stale completion cannot overwrite a newer input. Preserve cancellation for cancellable existing effects, surface progress immediately and deliver contextual failures; never block event/render loops or discard spawned-task errors. Long daemon-owned jobs must either finish bounded with their request or resume from durable state; disposable read/probe work can cancel during handoff without holding admission for minutes. Mutating actions participate in existing labeled admission and drain ownership.

Establish `mj-controller/tests/golden/daemon-api-exchange.txt` through a colocated daemon test named `daemon_api_exchange`, exercising real authenticated control requests and rendering whole replies with normalized temporary roots. Extend existing `phone-new-session-preflight.txt` for shared HTTP behavior instead of manufacturing small input/output unit examples. Include remote-only paths absent on the client and superseded completion cancellation as actual boundaries. Run `cargo test -p brokk-mj-controller daemon_api_exchange` and `cargo test -p brokk-mj-controller preflight`, then the SSH driver dashboard-path scenario. Expect Linux completions and valid remote repository creation without any Windows stat. Commit this usable dashboard checkpoint.

### Milestone 4: dashboard environment work uses the same owner


Add daemon actions for settings profile/runtime discovery, saved-profile login status, startup availability, checkpoint archive sizes, spinner preference persistence, TestTarget, and mbx build-cache preview/install/re-preview. Replace local operations in `mj-cli/src/dashboard/io/spawn.rs` and cache/target branches in `dashboard/actions.rs`. Reuse current discovery and cache helpers with the daemon executor and daemon credentials, including secret-reference resolution, PATH probes and target SSH keys. Return sizes as metadata; clients must not stat checkpoint paths. Save spinner through the settings owner rather than local `Config::update`. Settings Login must invoke the placement dispatcher with selected profile and correct instance, suspending/restoring terminal ownership as currently required.

Preserve progress/cancellation for cache installation and target probes, report background failure, and keep independent jobs concurrent. A serialized connector refresh is not a gate for unrelated work. Extend the same daemon API exchange golden with these user-visible responses, including failures; extend `viewer-settings-exchange.txt` where shared settings behavior changes. Run `cargo test -p brokk-mj-controller daemon_api_exchange`, `cargo test -p brokk-mj-controller settings`, and `cargo test -p brokk-mjolnir dashboard`. Prove that a profile and archive present only on the server appear correctly and that a failed target test reports the server's error. Commit this checkpoint.

### Milestone 5: explicit paths, attachments, import and voice


Change `mj-cli/src/go.rs`, `simple.rs`, dashboard startup cwd inference in `dashboard.rs`, and workspace inference in `main.rs` so SSH placement never canonicalizes client cwd as a project. Add an explicit `--path <remote-path>` to simple/go entry points where absent and validate it through milestone 3. Missing path says “SSH daemon launches require an explicit remote path; use mj go --path /path/on/<host>” (adapt the command for simple launch/workspace selection). Normal dashboard opening without automatic cwd session creation remains available. Preferences and recipes that interpret project paths must use daemon settings/actions; client-only UI hints and explicit local file inputs stay local.

Add authenticated attachment upload using existing attachment-store installation logic, then update `mj-chat/src/chat/remote.rs` and `attachments.rs` to read/optimize image bytes locally and await daemon installation before submitting the prompt reference. Use existing protocol framing/chunking conventions rather than raising frame limits. Return the installed opaque reference; cleanup/cancel uses the store's owner. Retry only explicitly unaccepted installation requests or use an existing content-addressed/idempotent install contract. References from history remain opaque. Verify with more than 64 KiB of image payload across the real connection and compare stored bytes/reference behavior.

Import from the client: the CLI `mj import` and the dashboard import dialog keep scanning native transcripts on the client, then upload the prepared archive to the daemon through an authenticated, chunked control upload (the same transfer convention as attachments), and the daemon installs it under its own sessions directory and registers it. The client never sends a client path for the daemon to open. Checkout edits recorded against a client checkout are carried in the archive; the session's project path is a daemon-host path chosen in the dialog (or `--path` on the CLI), defaulting to the recorded path. Voice: the client records and encodes audio, sends it to a daemon transcription action that runs the voice worker with the daemon's credentials, and receives text; remove `mj-client/src/auth.rs` reads of daemon-profile files from the voice path. Document that remote login browser OAuth using remote localhost callbacks may fail from Windows; paste/setup-token/API-key methods avoid that callback requirement. Do not claim all pinned harnesses support a specific device flow.

Extend `daemon_api_exchange` and real SSH driver for upload and no-cwd behavior. Run `cargo test -p brokk-mj-chat attachment`, `cargo test -p brokk-mj-controller daemon_api_exchange`, and `cargo test -p brokk-mjolnir dashboard`. Check cancellation, draft retention and disconnected-submit behavior through real event transitions, not only manual rendering. Commit this checkpoint.

### Milestone 6: one Windows development go script


Create `scripts/dev-remote.ps1` with required SSH destination parameter exposed as `-Host` (use a different internal variable: PowerShell's `$Host` is reserved), optional `-Commit`, `-RemoteCheckout`, `-WindowsCheckout`, and `-Instance`. Resolve the source checkout's HEAD once by SSH, or resolve the supplied commit there. Require that commit to be available to the Windows checkout by fetching the configured repository; for unpublished remote commits, fetch through the same SSH host's Git repository URL. If policy prevents that fetch, report the precise Git command needed to publish/make that commit readable; do not silently substitute origin/master.

Each machine has one dedicated build worktree (created on first run with `git worktree add --detach`, default `<checkout>-build`), reused on every run and moved to the selected SHA with `git checkout --detach <sha>`. Reuse keeps that worktree's Cargo `target/` warm; a fresh worktree per SHA would make every Windows build a cold build of the whole workspace. Never reset, clean or checkout the user's active source tree. Refuse a dirty build worktree with its path and a concrete instruction to preserve the edits and rerun. Hold a build-script lock for each dedicated checkout through installation, check HEAD and cleanliness before and after, and never edit/commit/merge there during the build. Source changes must invalidate the run visibly. This avoids the documented split stamp defect in sequential native and portable worker builds. Do not override Cargo targets or mbx configuration; preserve normal build storage.

On Linux execute the commit's `scripts/install.sh` in that worktree; it installs native mj/worker plus the matching host-architecture musl worker and voice helper under CARGO_INSTALL_ROOT, CARGO_HOME or ~/.cargo/bin. Capture its actual installed executable path rather than relying on an unrelated mj earlier on PATH. On Windows require native Rust/MSVC tooling as in `.github/workflows/release.yml::build-windows` and run `cargo build --release --locked -p brokk-mjolnir --bin mj --target x86_64-pc-windows-msvc`. This is an install build, not dev-profile validation. Install mj.exe and whatever client-side voice recording helper milestone 5 leaves on the client; the desktop shell no longer exists. Install using root `install.ps1`'s destination precedence (`MJOLNIR_INSTALL_DIR` or LOCALAPPDATA/Programs/Mjolnir/bin), staged replacement/aside handling for running executables, and PATH handling. Extract shared installer logic if needed rather than maintaining subtly different replacement rules.

Finally use the installed Windows binary and selected SSH configuration to run `mj --instance <instance> daemon status`. Enhance SSH status output in milestone 1 to name both client and daemon identities and confirm same_source. A run succeeds only when these match. Partial failure is recoverable by rerunning at the same SHA; never claim success after only one installation. Run PowerShell parser validation and execute the script twice on the actual Windows box, expecting the same commit installed and matching status both times. Record both transcripts, then commit the script checkpoint.

### Milestone 7: publish instructions and complete acceptance


Add `docs/src/content/docs/daemon-ssh.md` beside `daemon-container.md`, with client-only `[daemon]` example, remote settings ownership, same-release installation on both hosts, explicit Linux paths, SSH config/host trust and login instructions, direct viewer prerequisites and actionable limitations. Include go-script prerequisites and invocation. Explain that SSH control works without viewer exposure, and remote-container viewer publication is outside scope. Link the page using existing navigation conventions.

Run final validation once as below, commit the documentation and final fixes, and dispatch nightly on master after the platform-sensitive implementation is pushed. Follow repository review requirements for implementation commits. This planning assignment itself must not commit or push anything.

## Concrete Steps


All implementation commands run from the repository root. Before implementation, inspect issue #1514's assignee and label; the parent/top-level owner claims it and adds agent-in-progress, coordinating #1043 overlap. A child does not take ownership from another person. Check `git status --short` before staging only owned paths. Keep each milestone on the current branch; do not create branches, rebase or open a PR. Commit/push validated implementation checkpoints as repository instructions require unless the parent narrows that authority.

The new end-to-end driver has this explicit interface:

    cargo build -p brokk-mjolnir --bin mj
    python3 tests/e2e/ssh_remote_daemon.py --mj target/debug/mj --instance ssh-e2e --scenario all

Implement `--scenario control`, `http`, `dashboard`, `attachments`, and `all` so each milestone can run its own scenario. Reuse isolation/cleanup patterns from `tests/e2e/reliability_lab.py` and the LocalSshd fixture described above. Launch a real foreground sshd on localhost with temporary host/client keys and pinned known_hosts, forwarding allowed, password login off; no accept-new and no dependency on personal sshd configuration. Remote command execution uses a test-only PATH wrapper for the real absolute mj binary and distinct server MJ_CONFIG_DIR/MJ_DATA_DIR, plus `--instance ssh-e2e` on every invocation. Client config/data roots differ from server roots. Fail clearly when sshd prerequisites are missing; do not count skipped SSH cases as successful acceptance. Terminate process groups before deleting fixture files. No live default instance may be touched.

Example success evidence, with volatile ports/revisions redacted only in stored test goldens:

    control: endpoint JSON parsed; authenticated Ping through real SSH
    control: sessions listed; client daemon/store absent
    reconnect: remote restart recovered; draft retained; accepted mutation executed once
    transport: tunnel killed; remote daemon and worker remained alive
    http: advertised URL used directly; wrong certificate refused
    dashboard: remote-only paths and profiles visible
    attachments: uploaded image reference resolves on daemon

## Validation and Acceptance


Every cargo test runs outside the restricted sandbox with elevated permissions, dev profile, and isolated test data directories. Do not redirect Cargo target output or alter mbx. Focused commands are specified per milestone; after a failure fix and rerun only failing tests. Use the same package selection where possible to avoid feature-graph recompilation. Add only golden, real end-to-end, independently changing interface, or tricky-condition tests; no default-value, input-check or small feature-example unit tests.

The SSH driver must test remote handoff while attached, including changed endpoint/token, an active worker surviving restart, and draft preservation on an incompatible build. Kill only the tunnel and prove the original daemon PID/worker remains running, then reconnect without a second daemon. Force the selected local forwarding port to be occupied and observe a new port attempt; authentication and host-key refusals instead report their stderr and stop. Exercise an acknowledgement lost after mutation acceptance and prove no replay. Use explicit barriers around admission/acceptance for deterministic races rather than sleeps. Cancellation must reap the SSH child, drain stderr concurrently (include more than 64 KiB), and leave responsive quit. These tests protect ownership and partial-failure boundaries, not implementation details.

After the final implementation round, run the single full touched-crate validation, adding any newly touched crate to this command:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-chat
    cargo clippy --all-targets -- -D warnings
    python3 tests/e2e/ssh_remote_daemon.py --mj target/debug/mj --instance ssh-e2e-final --scenario all

Expect zero failed tests and no Clippy warnings. Do not promise a test count before running. Store exact commands, totals and logs, including baseline failures with reasons, in the implementation evidence directory. Review golden changes as full exchanges; never approve regenerated output blindly. New control actions advance the protocol revision at integration without changing frozen upgrade management formats. No database migration is expected; any discovered need requires a fresh classified migration and isolated upgrade coverage.

After pushing platform-sensitive code:

    gh workflow run nightly.yml --ref master

Inspect the dispatched Windows and macOS results; log the run URL and failures. Windows OpenSSH behavior requires the real Windows manual check even if Linux tests pass. On the user's Windows computer run `pwsh -File scripts/dev-remote.ps1 -Host <box> -RemoteCheckout <box-checkout> -Instance ssh-real`, configure that instance's `[daemon]` SSH placement, run status, open the dashboard, select a remote-only repository, paste an image, open `mj app`, and exercise usage/API commands against the advertised URL. Restart the remote daemon while attached and confirm the draft survives; stop the forwarding child and confirm remote work survives. Repeat the go command and verify both builds still match. Keep every smoke invocation in ssh-real, never default/live store. If real Windows access is unavailable, record that acceptance as outstanding rather than substituting WSL for native Windows.

## Idempotence and Recovery


Endpoint discovery can be repeated because the remote startup lock is authoritative. Reconnection replaces process-owned transport only; it never treats failed SSH as authority to destroy a daemon or delete metadata. Explicit stop/restart goes to the remote launcher and retains its admission/drain rules. No accepted mutation is automatically retried after ambiguous delivery. Handoff must wait only for daemon-owned bounded work, not workers, questions or reviews.

The development script uses immutable build inputs and staged binary installation; failure leaves clear per-machine status. Rerun the same SHA to finish. Never delete a checkout beneath a running installer, downgrade a newer daemon, or reset user data to repair compatibility. Stop test subprocess groups first, stop only the named isolated daemon, then remove test directories. Redact tokens and credentials in evidence; endpoint JSON itself contains a bearer secret and must not be pasted into public logs.

## Artifacts and Notes


Implementation evidence belongs under `.agents/docs/ssh-remote-daemon/`, with test logs and the native Windows transcript; human documentation belongs under `docs/src/content/docs/`. The two starting read-only reports are `.mj/agents/c18326cb1d1fd38e334b628a7e1ac864/map.md` and `.mj/agents/4c2da4cc520f34282a28981ebfc34946/audit.md`, against a7e57692e. This plan embeds the required decisions and remaining inventory so those reports are not prerequisites for implementation.

A client configuration example, separate from the remote's own host/container config, is:

    [daemon]
    runtime = "ssh"
    host = "linux-box"
    instance = "ssh-real"
    ssh_args = []
    remote_command = "mj"

The server's reachable viewer alternative is `[phone] tailscale_detect = true` with operational Tailscale, or `[phone] bind = "<reachable-IP>:<port>"` with both `tls_cert` and `tls_key` paths on the server. No automatic insecure bind is introduced.

## Interfaces and Dependencies


Use existing SSH, subprocess, attachment-store, settings and preflight code; no new workspace crate or mandatory third-party framework is required. In `mj-client/src/daemon/connector.rs` define an async connector contract taking a connection intent (existing-only or ensure-running) and cancellation token, returning `ConnectionLease` with the authenticated local `SocketAddr`, remote identity, instance identity and shared supervisor lifetime. Existing-only remote discovery must not accidentally start a daemon; provide the endpoint command an existing-only mode for that intent, while normal endpoint defaults to connect_or_start. Refresh is serialized within the connector and consumers never independently read metadata for the same fact. A remote lease carries no usable local PID or local daemon executable.

`mj-cli/src/daemon.rs` supplies local startup behavior and placement dispatch. `mj-client/src/daemon.rs::DaemonClient` retains the connector/lease through request and attachment lifetimes; framing still operates on local TCP. The endpoint JSON is a new machine-readable CLI interface, not a modification to frozen daemon management shapes. Typed actions for machine jobs use existing request IDs/progress/cancel mechanisms, and all consumers use daemon answers rather than recomputing filesystem facts. Any new job persistence must meet the existing daemon handoff durability rules before landing.

Revision note (2026-11-12, parent review): added `[daemon] remote_command` with login-shell PATH, and changed the go script to one reused build worktree per machine so builds stay warm. Milestones 3–5 do not depend on SSH placement (they fix every placement) and may be implemented in parallel with milestone 1 in separate detached worktrees; new `DaemonAction` protocol revisions are assigned at merge time in landing order.

Revision note (2026-11-12): Initial plan written from verified research and settled user decisions; EC2 forwarding incorporated from the parent's correction. Explicitly recorded dirty-build matching limits, actual Windows installer location, and absent control exchange golden to avoid relying on unverified report suggestions.
