# Try EC2 capacity preferences and report configured storage

This ExecPlan is maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

Issues #1523 and #1522 concern the standalone `mj ec2` CLI. Users should be able to request several instance types and locations once, have explicit AWS capacity refusals cleaned up automatically, and see both the selected placement and all failed attempts. The existing `zfs_cache` JSON field must describe the cache configured on the host, including automatic local NVMe selection.

## Progress

- [x] (2026-11-12) Read and claim both issues; inspect provisioning, inventory, cleanup, and existing subprocess fixtures.
- [x] (2026-11-12) Correct storage outcome reporting to use the same Podman local-NVMe decision as bootstrap and machine cache configuration.
- [x] (2026-11-12) Add ordered instance type, zone, and region selection, refusal classification, cleanup, and durable history.
- [x] (2026-11-12) Exercise capacity exhaustion, region skipping, cleanup failures, ambiguous launch failures, and inventory across regions: all nine EC2 subprocess fixtures pass, including JSON output assertions and legacy-record cleanup.
- [x] (2026-11-12) Run the final CLI crate suite: 297 tests passed, four existing tests ignored; formatting and diff checks pass.
- [ ] Finish workspace Clippy, automated review, and push to origin/master; verify both issues close.

## Surprises & Discoveries

The existing inventory already queries every region in saved provisioning records. Each create owns a per-target filesystem lock and a unique operation ID used as an AWS client token and resource ownership tag. Cleanup reacquires that lock, so automatic attempt cleanup must share its locked implementation rather than call the top-level destroy command while holding the create lock.

Nitro hosts previously reused the hardware's NVMe capability for machine build-cache settings although their bootstrap does not configure the ZFS cache. The shared decision now includes the Podman runtime, so reported/configured storage agrees for both runtimes. The normal Cargo wrapper is mbx; initial test compilation is using it without build-layout overrides.

## Decision Log

Use explicit ordered type lists rather than attribute-based selection. Preserve the positional type syntax, add `--type`, and accept comma-separated or repeated location flags. Region order comes first, then type order, then zone order, so an older preferred NVMe type is tried across a region before a later type. Explicit zones constrain their region; listed regions with no explicit zones discover available ordinary zones in stable name order. A single explicit subnet constrains placement to its own zone and region.

Retry only explicit AWS rejections before an instance is accepted. Capacity and unsupported-placement refusals advance the candidate; vCPU quota and region opt-in refusals skip the region. Unknown errors, failed cleanup, and failures after an accepted launch retain the recovery record and stop. Give every attempt a distinct operation/client token so changed launch arguments never reuse an idempotency token. Keep failed-attempt history in the winning record and retain a resource-free history record on exhaustion; resource-free history must not block retrying the target.

Query AWS instance-type offerings before provisioning so a type absent from a zone is skipped without creating cloud resources. Use defaulted record states Preparing, Active, and CleanedUp: legacy records default to Active, preserving their ownership; history-only records can be destroyed locally even when their region is disabled. Inventory reports per-region errors alongside successful regions and treats an explicit opt-in refusal as a displayed unavailable region rather than hiding the winner.

## Outcomes & Retrospective

The implementation accepts both positional comma-separated types and `--type`, repeated/comma-separated zones and regions, and region-only requests with zone discovery. Capacity refusals clean up before advancing, regional quota/opt-in refusals skip the region, and all outcomes are retained in create/list output. Failed cleanups and uncertain or already-accepted launches retain recovery information and stop. `zfs_cache` now follows the actual Podman cache configuration. All 297 active CLI tests pass (four existing tests ignored), including nine EC2 fixtures; workspace Clippy and automated review are pending. No paid AWS instance was launched; fixtures emulate AWS/SSH/Podman with isolated configuration and data directories.

## Context and Orientation

`mj-cli/src/ec2.rs` contains Clap argument parsing, `Cloud::call` (AWS CLI subprocess execution), `create` (provisioning and registration), `Record` (keys and recovery metadata), `list` (tagged cloud inventory plus local records), and `destroy` (termination before resource/key/config removal). Its Unix-only tests run in child processes with an isolated PATH holding executable Python/shell AWS, SSH, and Podman fixtures. Configuration is published only after cloud-init and runtime verification. Optional image preloading runs after registration and releases the operation lock first.

## Plan of Work

First correct `zfs_cache` to use the bootstrap outcome and verify the returned JSON against the NVMe fixture. Then move candidate selection outside the single-placement provisioning routine, retain one lock for a target through provisioning, and extract the locked cleanup routine for use by both destroy and rejected attempts. Parse the AWS CLI's standard error code into a typed error carrying service and operation, so only known refusals from the appropriate operations permit another launch. Extend records with backward-compatible defaulted attempt history and a resource-free status. Include the history in JSON and text create/list output, and discover inventory regions from both current placement and history.

Extend the existing subprocess fixtures to vary zones/regions/types and to reject scripted launch combinations. Assert launch order, distinct client tokens, cleanup before the next launch, recorded region skips, successful cache reporting, discoverability, and recovery on cleanup/transport errors. Existing records without the new fields remain readable and retain their recovery behavior.

## Concrete Steps

From the repository root, run focused checks outside the restricted sandbox using the existing mbx Cargo wrapper, without changing build storage:

    cargo test -p brokk-mjolnir ec2::tests::

After the final implementation round, run:

    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Commit only changed files on the existing master branch. Request the required automated review before pushing `HEAD:master`; if upstream moved, fetch and merge it without rebasing. Dispatch `nightly.yml` if the change affects platform behavior.

## Validation and Acceptance

An isolated fixture create with `--type r8id.8xlarge,m8id.8xlarge --region us-east-1,us-east-2` must advance past capacity errors, skip a region on an explicit regional quota refusal, and register exactly one host. JSON must show the winning type/region/zone, the configured cache, and every actual failed attempt with its reason. `ec2 list` must show this history and cover all used regions. All-capacity-failed creates must leave no cloud keys/groups or active local operation blocking a new request. Unknown/ambiguous failures or cleanup failures must retain recovery information and never issue another launch. Existing single-type/single-zone, Nitro, image preload, account ownership, and termination-order fixtures must pass.

## Idempotence and Recovery

Each changed launch gets a fresh AWS client token and record directory. Clean up only resources tagged for that operation, and remove local keys last. Stop if cleanup fails and print the existing `mj ec2 destroy <operation> --region <region>` recovery command. Never automatically retry an uncertain launch acknowledgement. History-only records own no live resources and can be removed through destroy or superseded by a later create without touching unrelated configuration.

## Artifacts and Notes

AWS documents `InsufficientInstanceCapacity`, `Unsupported`, `VcpuLimitExceeded`, and `OptInRequired` in https://docs.aws.amazon.com/ec2/latest/devguide/errors-overview.html. Zone discovery uses `describe-availability-zones` filtered by available state and ordinary availability-zone type; returned order is not guaranteed, so discovered zones must be sorted.

## Interfaces and Dependencies

Use existing anyhow, Clap, serde/serde_json, shared subprocess helpers, and configuration locking; no new crate, daemon API, store migration, or dependency is needed. New record fields use serde defaults so existing keys and operations retain their meaning. Keep refusal classification local to the AWS subprocess boundary and candidate orchestration local to the standalone EC2 command.

Initial plan written 2026-11-12 after inspecting the two issues and the existing EC2 lifecycle.

Updated 2026-11-12 after implementation: documented offering checks, lifecycle states, Nitro storage consistency, and isolated failure fixtures.

Updated 2026-11-12 after validation: recorded nine passing EC2 fixtures, JSON/legacy-record assertions, and the complete CLI suite result.
