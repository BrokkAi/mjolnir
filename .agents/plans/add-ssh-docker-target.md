# Add Docker over SSH and validate on disposable EC2

This is a living ExecPlan maintained under `.agents/PLANS.md`. Progress, discoveries, decisions, and outcomes must be updated as implementation proceeds.

## Purpose / Big Picture

A user can configure `kind = "ssh-docker"`, launch coding sessions in Docker on a Linux SSH host, use isolated writable attachments, reconnect, checkpoint, recover, and destroy sessions. The controller needs SSH but no local Docker installation. All live Docker validation runs on a fresh EC2 host; Docker must not be installed on any existing user machine.

## Progress

- [x] (2026-09-06) Inspected local Docker and SSH Podman implementations, AWS configuration, and test workflows; user approved implementation and automatic test-host termination on success or failure.
- [x] (2026-09-06) Added shared types, SSH command transport, remote Docker mount smoke, and schema 24 migration; eight focused SSH Docker tests and the migration test pass.
- [x] (2026-09-06) Integrated controller lifecycle, setup, doctor, UI, and documentation; focused tests and all-target compilation pass.
- [x] (2026-09-06) Full `cargo test`, formatting, and Clippy with warnings denied pass; host CLI and portable musl worker built.
- [ ] Perform live acceptance tests (completed: new EC2 instance i-097865482bc2da7d5, Docker 29.8.0 ready; remaining: full lifecycle, failure injection, and orphan adoption).
- [ ] Collect evidence and remove test resources (implementation checkpoint committed as 9b82b0e8; final test-runner/fix commit pending).

## Surprises & Discoveries

Live EC2 doctor exposed a preexisting naming mismatch: resource_name emits mj-* but Docker backing-directory cleanup accepted only hel-*. Both normal close and failed-launch rollback now accept current and legacy prefixes; executable shell behavior tests cover this rather than checking script text alone. The first live smoke proved copy-on-write and removed its container/volume, but reported the backing-directory cleanup failure.

Docker writable attachments are managed Linux OverlayFS volumes. The existing smoke runner creates its lower directory with local filesystem calls, so it cannot simply be wrapped in SSH. Both source creation and post-write verification must run on the remote host. Existing SSH Podman code already stages worker binaries in a content-addressed remote cache, then copies them into containers.

The database is currently schema 23. The constrained session_targets table gained Docker in schema 20 and workspace_storage in schema 22. Its rebuild must preserve every column. The user's working tree has an unrelated AGENTS.md edit; exclude it from commits.

Planning verified default AWS credentials, default VPC vpc-0050fb351b91024e5 in us-east-1, public subnet subnet-06587855564853ad7 in us-east-1a, and Canonical's Ubuntu 24.04 AMD64 public AMI parameter. Recheck cloud facts before launch.

## Decision Log

2026-09-06: Add an explicit ssh-docker kind with existing SSH connection and Docker container settings. Preserve existing serialized kinds and Podman-only workspace storage validation. Docker runs through the remote CLI and host scripts, not Docker contexts or a Docker API client. This keeps filesystem operations beside the daemon.

2026-09-06: Generalize the shared remote-container execution boundary with an engine argument. Runtime templates and locators expose container_engine() to avoid repeated interpretation. Configuration SshDocker has ssh and container fields; durable locator SshDocker has host and container_id; runtime locator has ssh and container_id. Docker carries no Podman workspace-storage field.

2026-09-06: Use a new on-demand t3.large Ubuntu 24.04 EC2 host, 60 GiB encrypted gp3 root disk, standard CPU credits, default AWS profile/us-east-1. Install Docker Engine there only. Collect evidence and terminate after either outcome; do not leave the host for inspection. The instance is an ssh-docker target, not a change to Mjolnir's aws-ec2 product target.

## Outcomes & Retrospective

Core and product integration pass automated validation. Fixed Docker cleanup naming, added executable rollback/close tests, and fixed SSH host checkpoint staging cleanup while preserving unverified source archives. Remote doctor OverlayFS smoke now passes, and two real SSH Docker coding sessions have launched. The disposable EC2 host is running Docker 29.8.0; live acceptance and resource cleanup remain. Resource ledger: target/ssh-docker-e2e/implementation-20260906/ledger.json.

## Context and Orientation

src/hel_config.rs contains user TOML target definitions; src/hel_state.rs contains durable locators; src/hel_database.rs and src/hel_database/schema.rs map those locators to SQLite. src/hel_targets.rs builds CommandSpec values (executable, arguments, input, and operation metadata) and CommandPlan sequences. src/hel_targets/ssh.rs quotes commands for remote execution. ExecutionBoundary describes where a command runs: host, container, SSH host, or remote container.

mj-controller/src/hel_controller contains backend conversion, provisioning, worker_binary installation/control, checkpoint, reviewer, git_cache, and recovery_scan paths. mj-controller/src/hel_setup.rs and hel_doctor.rs implement initial setup and diagnostics; hel_server.rs and web/viewer.js expose target choices to the web surface. mj-tui and mj-cli consume target types for labels, grouping, mount completion, and actions.

OverlayFS presents an unchanged lower directory together with session-owned upper/work directories containing writes. Docker uses labeled named volumes to mount this view. Cleanup must remove the owning container, then volumes, then backing directories, and must report failures instead of deleting files beneath a surviving mount.

## Milestones

### 1. Public identity and runtime transport

Add SshDocker to all three target representations, config validation and helpers, plus schema 24 migration and persistence round-trip tests. Add ImageHost::SshDocker, remote Docker preflight, engine-aware remote command construction, and SSH Docker planning for provision, exec, reconnect, metrics, removal, and absence confirmation. Generalize Docker overlay run/cleanup scripts for an optional SSH host. Keep secret input off arguments and preserve command metadata when wrapping commands. The milestone is accepted when core tests prove every remote Docker operation is SSH-wrapped, transport quotes literal arguments correctly, and old target serialization remains compatible.

### 2. Product integration

Extend worker installation and remote caching, architecture discovery, archive/profile transfer, checkpoint/resume, reviewer staging, recovery/adoption, image refresh, capacity, clone cache, and mount validation. SSH Docker filesystem operations must use the remote host. Add setup's Docker SSH choice and doctor runtime/image/OverlayFS smoke checks through one shared smoke runner. Extend TUI/web choices and README plus docs/DOCKER.md. Existing operations remain supervised background work and UI cancellation/error paths remain responsive. Validate failure handling and all new enum branches with colocated behavior tests.

### 3. Automated validation

Run the default workspace tests outside the sandbox, Cargo formatting checks, and Clippy with warnings denied. Build the CLI for the host and mj-worker explicitly for x86_64-unknown-linux-musl. Test database migration, SSH quoting and engine choice, bad connections/daemon failure, read-only and writable mounts, ownership conflicts, repeated cleanup, worker streaming above 64 KiB, restore, and recovery. Do not install or start Docker on an existing host to satisfy a test. Add an opt-in EC2 validation runner under tests/e2e; record AWS resource IDs incrementally and provide repeatable cleanup.

### 4. Live EC2 acceptance

Create the host only when implementation is ready. Generate a unique run ID and temporary SSH key; import its public key into EC2, create a dedicated SSH security group restricted to the controller's current public IPv4 /32, then launch one tagged instance in the verified default VPC/public subnet. Resolve /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id at launch. Require metadata v2, use no IAM instance profile or Docker TCP endpoint, and set root disk DeleteOnTermination plus instance-initiated shutdown behavior terminate. User data installs a four-hour shutdown timer before Docker setup and installs Docker Engine from Docker's signed Ubuntu apt repository. Enable Docker and add ubuntu to the docker group. Record AMI, Docker version, image digest, instance ID, key name, security group, disk ID, and connection details in private local artifacts.

Use a separate known-hosts file and isolated MJ_CONFIG_DIR, MJ_DATA_DIR, daemon, and fixture repositories. Preserve logs under target/ssh-docker-e2e/<run-id>; keep local runtime sockets in a short local temporary path if required. Stage credentials only via the usual profile allowlist; use the existing codex3 profile for one small real coding task. The target uses ghcr.io/brokkai/mjolnir/agent-dev:latest with its resolved digest recorded.

Run mj doctor --json --smoke and verify remote overlay writes do not alter the lower directory. Launch two sessions and verify independent transcript streaming and resource reporting. Restart the isolated controller and reconnect. Exercise recovery scan/adoption with separate isolated state. Checkpoint a session, close it, restore into a fresh Docker container, and verify workspace contents plus continued interaction. Inject launch failure and temporarily stop Docker on this new test host; confirm useful errors and responsive UI, then restart Docker and recover. Destroy sessions and verify exact session containers, volumes, backing directories, and clone snapshots are removed without touching the other live session.

Collect logs before cleanup, including failures. Always terminate the test instance, wait for termination, verify root-disk removal, and delete the imported key and dedicated security group. Cleanup is keyed to recorded IDs and ownership tags and can run again after interruption. If cleanup is incomplete, explicitly report the exact remaining IDs; never claim completion with leaked resources.

### 5. Delivery

Review actual integrated changes and evidence, update this plan, and commit coherent validated checkpoints on the current branch. Stage explicit file paths and exclude AGENTS.md. Do not branch, push, rebase, or open a PR for this task.

## Concrete Steps

Work from /home/jonathan/Projects/hel3. Run Cargo tests with elevated permissions because they exercise sockets. Use normal build storage; never redirect Cargo output to /tmp.

    cargo test -p brokk-mj-core hel_targets
    cargo test -p brokk-mj-core hel_database
    cargo test -p brokk-mj-controller
    cargo test
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo build -p brokk-mjolnir --bin mj
    cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl

Expected result: all commands exit zero, with no new ignored tests or relaxed coverage rules. Run relevant web unit tests if web files change. Update this section with exact opt-in live runner commands and artifact locations after implementation.

## Validation and Acceptance

Success requires accepted ssh-docker TOML and durable state, remote-only Docker commands, unchanged attachment sources after writes, verified checkpoint/restore and recovery, useful errors under transport/runtime failure, passing automated checks, successful live session interaction, and confirmed AWS cleanup. Empty/unavailable cloud test results do not count as live validation. Existing local Docker, SSH Podman, and other target behavior must remain covered by the default suite.

## Idempotence and Recovery

Database changes are transactional and preserve old records. Resource operations verify exact ownership. AWS cleanup records exist before the next creation step; a repeated cleanup tolerates confirmed absence but does not hide permission/network errors. Instance shutdown terminates the host as a backstop, but the local runner still verifies termination and removes ancillary resources. Existing machines and ordinary Mjolnir sessions are outside this test environment.

## Interfaces and Dependencies

No new crate or API client. Add public SshDocker variants described in the decision log, ImageHost::SshDocker, verify_ssh_docker(&SshTarget, &impl CommandExecutor), and container_engine() helpers on runtime TargetTemplate/TargetLocator. Replace the Podman-specific execution boundary with SshContainer { engine, ssh, container_id }. Use shared subprocess helpers for Rust process execution and existing SSH/scp helpers for transfer; report failures with the remote destination and operation.

## Artifacts and Notes

Primary agent owns core types, persistence, runtime transport, integration, EC2 lifecycle, and final review. Delegate controller lifecycle and UI/diagnostics to Luna agents with disjoint file ownership after publishing interfaces. Each reports unexpected design issues and does not delegate further. Raw evidence is local build output, not committed credentials. This plan and a concise agent-facing validation note may be committed.

Plan revision 2026-09-06: Recorded the accepted design, verified environment, shared interfaces, ownership, and mandatory EC2 cleanup before implementation.

Plan revision 2026-09-06: Recorded passing core tests and integrated all-target compilation. Shared Docker cleanup now preserves clone snapshots on container/volume removal failure and propagates backing-directory removal errors.

Plan revision 2026-09-06: Recorded passing full tests, formatting, Clippy, binary builds, and the newly provisioned test host. Live acceptance and mandatory cleanup are still in progress.

Plan revision 2026-09-06: Recorded the naming mismatch found by live validation and the required regression tests before repeating the smoke test.

Plan revision 2026-09-06: Recorded successful corrected smoke, real session launches, checkpoint staging fix, and passing focused regression tests. Preparing a validated implementation checkpoint; cloud cleanup remains mandatory.

Plan revision 2026-09-06: The main live lifecycle run passed in acceptance-2: two coding tasks, unchanged attachment sources, checkpoint/close/restore, restart/reconnect, and normal close. Extra failure tests found early provisioning discard was only in memory, leaving a database ghost; fixing and regression-testing persistence before repetition. The extra checkpoint fixture must send an initial Codex turn because Codex does not create a native rollout for an untouched conversation. Its leftover session was identified through recovery scan and removed using exact-ID recovery destroy.

Plan revision 2026-09-06: Early provisioning discard now persists before returning its original error. The isolated reload regression and all 24 provisioning tests pass; repeated live failed-image launch removed both its cloud resources and controller row. Host CLI rebuilt and Clippy passed. Final extended recovery run is in progress.
