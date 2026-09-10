# Test mj across diverse AWS Spot distributions

This ExecPlan is maintained according to `.agents/PLANS.md`. It records an operational testing task, not a product feature.

## Purpose / Big Picture

Exercise current committed mj through real tmux terminals on disposable Linux VMs, find reproducible corner cases, and file tickets in BrokkAi/mjolnir. The user authorized AWS Spot provisioning with the keypair in `~/.secrets`, a $25 spending limit, ticket creation, and immediate teardown when each VM is finished. No product fixes are requested. Stop early when additional environments stop adding meaningful coverage or defect classes; four hours is only a per-VM safety ceiling.

## Progress

- [x] (2026-09-10 15:54Z) Inspect repository harnesses, credentials, default VPC, and issue templates.
- [x] (2026-09-10 16:03Z) Build GNU and static-musl binaries; select verified publisher AMIs for six distros.
- [x] (2026-09-10 16:22Z) Gather tmux evidence on Ubuntu, Debian, Fedora, Alpine, NixOS, and Amazon Linux; stop after diminishing returns.
- [x] (2026-09-10 16:20Z) Reduce findings and publish deduplicated GitHub issues #984 and #985.
- [x] (2026-09-10 16:24Z) Verify all cloud resources removed and write the task-owned report for the required documentation commit.

## Surprises & Discoveries

HEAD advanced between planning and execution, from 6be22ae4 to 350854488a66b18108759294c607a6c79b3f7f8e. The repository already provides a real-terminal component harness and an EC2 SSH test helper. The tmux harness intentionally fakes ACP (the protocol used to communicate with coding agents) and Podman, so its passes alone do not prove real container provisioning works.

## Decision Log

Use current committed HEAD at execution start, as requested, and retain its revision in every artifact. Use us-east-2, at most three concurrent non-burstable VMs, one-time Spot requests, no paid AMI products, and unique run tags. Keep AWS credentials on the controller and use disposable SSH credentials. Decisions recorded 2026-09-10 by Codex.

## Outcomes & Retrospective

Completed six distinct distro environments and filed two product defects: release installer success despite unusable binaries (#984), and hidden field labels in compact agent forms (#985). Independent real-tmux probes passed on all six, with one unconfirmed Alpine detach timeout that did not recur in a full retry or six focused shortcut trials. The standard component harness did not fully pass because of stale expectations and the reproduced form defect; full SSH provisioning also remains unverified. These limits are explicitly recorded in `.agents/docs/aws-distro-testing-2026-09-10.md`.

All six VMs are terminated. The final ledger verifies zero remaining run-owned volumes, security groups, or key pairs, and all Spot requests closed or cancelled. Local ephemeral SSH keys were removed and the watchdog exited. Approximately 1.14 VM-hours cost about $0.034 in compute; estimated total including ancillary charges is below $1. No product code or dependency changes were made. The original untracked files remain untouched.

## Context and Orientation

`tests/e2e/tui_components_tmux.py` drives real mj through tmux, using `reliability_lab.py` for isolated configuration, a fake agent, and cleanup. It accepts `--seed` and `--hel`; a matching worker must be at `target/debug/mj-worker`. `tests/e2e/ssh_docker_lab.py` supplies shared subprocess and SSH helpers. Preserve raw artifacts and task-specific orchestration under `target/distro-spot-20260910/`; write the final agent-facing report under `.agents/docs/`.

## Plan of Work

First build the pinned revision with the repository toolchain and record checksums. Resolve official images for Ubuntu, Debian, Fedora, Amazon Linux, Rocky, AlmaLinux, openSUSE, Alpine, and NixOS, prioritizing different libc, init, package manager, and security defaults. Skip unavailable images after a bounded attempt. Do not replace VM coverage with containers.

Next create a tagged security group allowing SSH only from the controller, a temporary key, and small Spot batches. Register resource IDs immediately and run a controller watchdog plus guest expiry. Track costs conservatively, stop new launches at $20, and reserve $5 for teardown. Copy the committed harness and binaries to each VM and run the baseline through tmux. Record compatibility failures before attempting native builds. Probe locale, terminal size, unusual paths, missing dependencies, streaming beyond pipe capacity, cancellation, and cleanup. Supplement fake-backend tests with real SSH and rootless containers where practical.

Finally reduce defects, search existing issues, and file one issue per new root cause with exact reproduction and sanitized evidence. After two distinct environments add no defect class or meaningful coverage, stop if remaining candidates repeat tested assumptions. Retrieve evidence and terminate each VM immediately after use, then verify all tagged resources are gone and commit only task-owned report or harness changes.

## Concrete Steps

From `/home/jonathan/Projects/hel`, run `cargo build --locked -p brokk-mjolnir -p brokk-mj-worker`. Deploy committed source plus verified binaries, and run `python3 tests/e2e/tui_components_tmux.py --seed 91001 --hel target/debug/mj` on each prepared VM. Capture bootstrap, binary startup, harness, and exploratory output. Use `gh issue list` before `gh issue create --repo BrokkAi/mjolnir --body-file ...`.

## Validation and Acceptance

Only completed scenarios count as passes. Separate environmental gaps and harness failures from product defects. Preserve OS, architecture, libc, shell, locale, tmux version, commit, binary hash, captures, and logs. No Rust changes are planned; if that changes, run `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings`. Review and validate any new Python scripts and the documentation diff.

## Idempotence and Recovery

Use exact IDs and unique tags to scope cleanup. Record resources before the next mutation. One-time requests must not relaunch instances; cancel outstanding requests and terminate instances explicitly. Root disks are delete-on-termination. A guest poweroff deadline and independent controller watchdog bound abandoned instances. Never remove remote working files before stopping their owning processes. Verify instances terminated, no pending requests or detached volumes remain, then delete temporary security groups and keys. Preserve partial evidence when interrupted.

## Artifacts and Notes

AWS authentication and default VPC discovery succeeded before provisioning. Existing untracked `mj.sqlite3` and `.agents/plans/restore-tui-workspaces-and-status.md` belong to other work and must remain untouched.

## Interfaces and Dependencies

No public API changes. The operational dependencies are AWS CLI, SSH/SCP, tmux, Python, Git, the pinned Rust toolchain, and distro package managers. Credentials must never appear in logs, VM files, or issues.

Initial execution plan recorded 2026-09-10 to make cloud ownership, evidence requirements, and early stopping explicit.

Execution update 2026-09-10 16:14Z: filed #984 for Debian release glibc incompatibility and #985 for hidden field labels in inline forms. The baseline harness contains stale path-error and composer-label assertions; only disposable copies were adapted. Verified 70,034-byte request/reply flow, extreme terminal resizes, and responsive detachment on Ubuntu, Fedora, and Debian (musl diagnostic build). Real rootless Podman run/exec/remove passed on Ubuntu and Fedora. The synthetic SSH repository failed remote-source validation, so full SSH provisioning is not claimed. The large-reply assertion was corrected to inspect the agent entry tail because conversation responses intentionally omit older lines.

Completion update 2026-09-10: extended #984 with actual release failures on Amazon Linux, Alpine, and NixOS. All six passed the independent core probe using GNU or diagnostic static-musl builds as documented. Stop after the second batch: further related distro variants would mostly repeat the same runtime assumptions and packaging failure. Raw evidence, exploratory drivers, AMI metadata, pricing, and cleanup verification remain in `target/distro-spot-20260910/`; the concise tracked report links the findings and specifies coverage limits. Python syntax, documentation diff, issue publication, and final cloud inventory were verified before the documentation commit.
