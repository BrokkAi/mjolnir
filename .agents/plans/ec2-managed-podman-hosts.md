# Provision reusable EC2 Podman hosts from mj

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The user wants a persistent c7i.16xlarge EC2 VM with Podman, configured as an mj target, and a verified session on it. Encode that same workflow as `mj ec2 create`, `list`, and `destroy` so future hosts need no manual AWS, SSH, or TOML work. A persistent host runs multiple independent rootless containers; the existing `aws-ec2` launch-template target instead owns one VM per bare session and remains a separate feature.

## Progress

- [x] (2026-11-09) Inspected configuration, AWS account, available zones, image, and CLI/configuration/subprocess interfaces.
- [x] (2026-11-09) Provisioned the requested C7i; user then requested shutdown and selected C8id.16xlarge with local NVMe, ZFS/lz4, and mold. Replacement is running and original is stopped.
- [x] (2026-11-09) Registered ec2-c8id16, disabled discovered bare access, removed old configuration, and verified real session cfd87ded54b8f20fed978447b3089998 replied EC2_ZFS_PODMAN_OK with ZFS/mbx evidence.
- [x] (2026-11-09) Implemented create/list/destroy, ZFS cache bootstrap, pinned mbx setup, mold in the agent image, documentation, and AWS boundary/partial-failure integration tests.
- [x] (2026-11-09) Initial EC2 checkpoint passed the full CLI/controller suites and clippy, was reviewed clean, and was pushed as 348ba323a. Image publication and cross-platform CI were started.

## Surprises & Discoveries

AWS default credentials have no configured region. The existing account is 842609633142, with default public subnets in us-east-1. c7i.16xlarge supplies 64 vCPUs and 128 GiB RAM and is offered in zones a, b, c, d, and f. Canonical's SSM parameter resolves Ubuntu 24.04 to an official 099720109477 image. The config editor already provides `Config::update` with an interprocess lock, atomic replacement, and comment-preserving edits.

## Decision Log

Use a normal SSH machine with a Podman runtime for persistent VMs. Reuse the AWS CLI credential chain, avoiding a new SDK dependency and authentication mechanism. Require instance type and availability zone on create; infer region from a standard zone or accept an explicit region. Expose storage and network overrides needed for non-default VPCs. Use AWS ownership tags as cloud inventory, unique operation identifiers for resources, and a dedicated SSH key and known-hosts file under the owning mj config directory. Preserve a partially provisioned VM when an accepted launch or later bootstrap fails, reporting its identity and destroy command so accepted resources remain discoverable. Preserve unfinished pre-launch resources under the same local record and cloud operation tags, so destroy also recovers a refused or interrupted launch. Record the creating AWS account and refuse destruction under a different identity. Destroy only explicitly mj-managed instances and their matching resources; do not remove configuration until AWS termination succeeds.

## Outcomes & Retrospective

The C8id replacement is running as ec2-c8id16, with ZFS/lz4 mbx storage and mold verified by a real mj container session. Persistent EC2 create/list/destroy commands are published. Profile transfer now uses one compressed archive; the same measured profile packed/uploaded in about 13 seconds instead of 162 seconds. The VM-per-session launch/UI/probe path and legacy Move recovery were removed; the user explicitly confirmed that no legacy EC2 Moves exist and declined compatibility complexity. Passive historical formats and cleanup remain.

Future EC2 creates automatically place Podman writable layers and the existing per-container /tmp volumes on local NVMe at /mnt/nvme/podman when supported. The user explicitly chose future VMs only, so the live host keeps its original Podman store. This final setup was validated with AWS boundary fixtures and compared with Morannon's actual configuration; the busy live host's separate-store smoke was cancelled before it ran a container. Full affected-crate suites and all 35 browser tests passed, with one loaded core timing-test failure passing its isolated rerun; workspace clippy and shell checks passed. Review was clean, the independently completed #1482 change was merged, and source was pushed as 9745824b5. No mj installation or release was requested or performed.

## Context and Orientation

`mj-cli/src/main.rs` declares the clap command tree, logging command names, and dispatch. Add `mj-cli/src/ec2.rs` for command arguments and the provisioning workflow. This is a one-shot CLI operation and must not block any dashboard or daemon event loop; run blocking cloud/SSH work with supervised `spawn_blocking`. `mj-core/src/config.rs` owns `Config::update`; `mj-core/src/config/machines.rs` defines SSH machines and stored targets; `mj-core/src/config/targets.rs` defines resolved SSH Podman targets. `mj-core/src/subprocess.rs` provides `run_with_input` and output draining; all child process capture uses it. Historical EC2 locators and cleanup remain readable; new sessions use persistent SSH/Podman hosts.

## Plan of Work

First provision and test the requested real host using the installed mj. Record nonsecret resource identities under `.agents/docs/`. Add a self-contained CLI module that discovers a compatible official Ubuntu image and subnet, provisions tagged key/security-group/instance resources, waits for SSH and cloud-init, verifies rootless Podman, and atomically adds configuration. List queries tagged instances. Destroy verifies ownership, terminates the instance, waits for network teardown, deletes matching owned security groups and key pairs, and removes only the configuration that still refers to that instance. Surface partial failures and permit destroy retries. Add command help and user-facing CLI reference updates in established documentation locations only.

## Concrete Steps

Work in the current checkout and branch. Use elevated AWS/SSH operations and installed `mj doctor --json` before and after configuration edits. For build/test use existing Cargo and mbx storage without overrides. Start any new binary with `--instance ec2-host-test` and isolated configuration/data directories. Use a private tmux socket for a real remote-container session smoke. Run `cargo fmt --all`, focused `cargo test -p brokk-mjolnir ec2` outside the sandbox, then the full touched-crate suite and `cargo clippy --all-targets -- -D warnings`. Commit only changed task files, request the configured review, and push HEAD to origin/master, merging concurrent upstream movement if needed.

## Validation and Acceptance

The cloud host must report `podman info` as rootless and run a container. A session on its configured target must start a real worker/harness and return evidence from inside the container. `mj ec2 create c7i.16xlarge --zone us-east-1b --name example` should print instance, address, target, and next steps. `mj ec2 list --region us-east-1` should include managed hosts with state/type/zone/address/target. `mj ec2 destroy example --region us-east-1` must remove its VM and owned auxiliary resources and matching configuration while preserving unrelated entries. Tests exercise the AWS/SSH command contracts and failure ordering, rather than duplicating local computations.

## Idempotence and Recovery

Use a RunInstances client token so an uncertain launch can be identified without creating another VM. Cloud tags identify ownership, operation, and mj target, enabling discovery after a process exits. Never destroy unrelated resources. Preserve an accepted VM on bootstrap/configuration failure and print the retry/cleanup command. Destroy may be repeated after partial cleanup; absent owned resources count as already removed. Termination deliberately destroys all containers and work on that selected host, so help and command output must state that scope.

## Artifacts and Notes

Provisioning evidence and session results will be recorded in `.agents/docs/ec2-managed-host-2026-11-09.md`. Do not commit SSH private keys or credentials. Live doctor already reports pre-existing fixable prerequisites; compare before/after findings instead of claiming a globally clean system.

## Interfaces and Dependencies

Use clap Args/Subcommand, anyhow, serde_json, the existing reqwest client only if needed for public-IP discovery, `mj_core::config::{Config, Machine, TargetTemplate, SshConnection, ContainerTemplate}`, and shared subprocess helpers. No new crate or database migration is necessary. Use AWS CLI JSON request/response values rather than shell interpolation. SSH provisioning must wait for cloud-init and report its log if bootstrap fails.

Revision 2026-11-09: initial plan after account and repository discovery.

Revision 2026-11-09: incorporated the user's C8id/local-NVMe/ZFS/mold and Podman-only choices, completed provisioning evidence, and diagnosed the separate recursive-scp profile-sync delay. The initial full CLI suite passed 243 tests and found one missing --json help description; that description was corrected. Workspace clippy passed its initial run. Final focused and integration validation follows the last changes.

Revision 2026-11-09: final CLI validation passed (all tests); workspace clippy passed. Added wrong-account refusal coverage and corrected the initial help-description failure. Review and remote publication are the remaining steps.

Revision 2026-11-09: review found address lifetime, SSH-readiness, and terminated-name-reuse defects. Future creates reserve a tagged Elastic IP and publish configuration only after SSH, cloud-init, and requested mbx installation are ready. The controller's existing EC2 SSH wait is shared rather than duplicated. The active manually provisioned host keeps its address to preserve live connections. Revalidate the CLI and controller crates and workspace clippy after these fixes.

Revision 2026-11-09: the review regressions and help-surface check passed, and workspace clippy passed after the fixes. The expanded final CLI/controller suites are still running; request the second review in parallel, and do not push before both required validation and review are complete.

## Expanded scope: profile transport and legacy EC2 removal

The user explicitly expanded scope to address the measured 162-second profile upload and remove the old VM-per-session path. Replace both remote-container and SSH-bare recursive profile scp operations with one gzip tar archive streamed through the existing CommandExecutor stdin facility. Build the archive in an owner-private temporary file, preserve the staged allowlist and recursive-copy contents, unpack with restrictive permissions, and report producer/transport/extraction errors without falling back to file-by-file copy. Validate the Rust archive writer against system tar with incompressible content exceeding 64 KiB, Unicode paths, nested files, and symlinks, then measure the same actual session-profile transfer on EC2 without disturbing running sessions.

Remove session-owned EC2 creation, resource selection, launch-template setup, and user-facing documentation from new-session paths. Keep only passive persisted-format readers and existing-instance cleanup/checkpoint handling needed to preserve historical work and compatible upgrades. New/restarted/resumed work selects an ordinary persistent SSH/Podman target; it must never launch a per-session VM. Remove the user's obsolete aws-runson launch-template machine/target after verifying no active session uses it. Trace all creation entry points, including Move, Resume, resource probes, target discovery and API validation, and make one authoritative target-availability rule exclude retired AWS targets. Expand the applicable isolated configuration/store/API compatibility regressions instead of adding happy-path unit examples. Run focused checks during implementation and the full suites of every touched crate plus workspace clippy once after the final round.

Progress for expanded scope:

- [x] (2026-11-09) Implemented archive streaming for SSH-bare and remote containers. Rust writer/system tar and real node transport passed with an archive over 8 MiB; the same 587 plugin files packed in 1.083 seconds and uploaded/extracted in 12.185 seconds versus 162.345 seconds for recursive scp.
- [x] Remove legacy VM-per-session admission and provisioning surfaces; retain passive historical readers/cleanup and omit legacy Move recovery per the user.
- [x] Complete expanded validation, review, commit, and push (source published at 9745824b5).

Revision 2026-11-09: removed session-owned EC2 launch builders, instance-size controls, discovery/probes, and the launch-template script's config writer. Config availability now excludes the retired kind in new/resume/move admission and target lists. Historical config/locator/runtime JSON remains readable and existing Move destinations can only be discovered, adopted, or cleaned up; no stored launch request is executed. #1482 is assigned to another session and outside this work. The mold agent image published successfully (run 42750912583).

Revision 2026-11-09: the focused Rust regressions passed (14 controller, two core, two CLI); all 35 affected browser tests passed after updating the single removed EC2 scenario. The retained RunsOn script test passed and no longer writes mj configuration. Final full suites for core/client/controller/TUI/CLI and workspace clippy are in progress; review runs in parallel.

Revision 2026-11-09, user decision after review: no legacy EC2 machines or Moves exist; do not add upgrade/recovery complexity for them. Declined both compatibility findings per that explicit instruction. Remove old Move destination boot/adoption/revalidation code and its obsolete tests; preserve only passive formats and exact-resource cleanup. The first final suite passed all 2,523 controller tests and client tests; core had one existing harness-install ETXTBSY failure, to rerun alone. Workspace clippy and all 35 browser tests passed.

Revision 2026-11-09, additional user scope: EC2 NVMe must hold Podman's writable store at /mnt/nvme/podman as on Morannon, including each container's existing dedicated /tmp volume. Auto-select NVMe/ZFS from the AWS instance-type response, configure rootless OverlayFS before first Podman use, and order linger services after pool initialization. The active host has running sessions on EBS, so migration timing is pending the user's choice; prepare and test future provisioning independently. Morannon's actual storage.conf confirms graphroot=/mnt/nvme/podman and fuse-overlayfs.

Revision 2026-11-09: the user chose **Configure future VMs only** for NVMe Podman storage. Do not change or migrate the current host's default storage or stop/recreate its sessions. Existing /tmp volume handling already meets the per-container requirement; changing graphroot moves those volumes to NVMe automatically. The final reduced retirement implementation passed full core/client/controller/TUI/CLI suites (2,507 controller; 632 core; 581 TUI; 244 CLI unit tests plus integrations). The last NVMe bootstrap change is passing clippy; the final suite is in progress. An isolated storage smoke on the busy current host stalled in Podman info and was cancelled, without changing its default storage.

Revision 2026-11-09: final NVMe revision passed client/controller/TUI/CLI suites and all integration targets; the core suite had one timing-sensitive retry test failure (empty attempt counter under load), which passed its isolated rerun. The preceding complete run passed all core tests too. All 35 affected browser tests, the RunsOn script regression, bash syntax/format checks, and workspace clippy passed. No current EC2 storage migration is authorized or required; future provisioning uses NVMe automatically. Commit the reduced legacy path and NVMe policy, then review/push.
