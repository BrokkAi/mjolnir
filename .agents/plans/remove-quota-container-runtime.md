# Run quota harnesses on the host

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Quota probes must never start containers or cause the release agent image to be downloaded. Codex, Grok and Claude should use a host CLI when installed and otherwise prepare the same pinned installation as bare workers. Older quota runtime config values must still load but have no effect.

## Progress

- [x] (2026-11-19) Traced quota runtime, callers, config and managed harness preparation.
- [x] (2026-11-19) Removed container runtime module, startup sweep and image refresher contribution; replaced launch resolution with shared host helper.
- [x] (2026-11-19) Completed compatibility and fixture updates; doctor has no quota-container-specific strings; updated user documentation and shared web settings golden.
- [x] (2026-11-19) Ran affected crate checks, all-target clippy and quota/config/continuation/settings filters through mbx; all tests passed (one existing live-service test ignored).
- [x] (2026-11-19) Completed local commit preparation; the session records the commit and required review verdict in its final report.

## Surprises & Discoveries

The image refresher resolves quota execution with host_has_harness=false, pulling the release image even when every actual quota probe uses a host CLI. Codex separately checks an existing managed cache and cannot install a missing harness. Claude already calls harness_install::locate and holds its installation lease through renewal.

## Decision Log

Use one shared resolver in mj-accounts/src/quota_harness.rs for all three CLIs. It searches the profile PATH over the daemon environment, then calls mj-core/src/harness_install.rs::locate with the built-in fallback launch. Retain installation and Node leases for the process lifetime. Inside a daemon container the existing Image policy is preserved. Remove the legacy enum and discard the entire runtime key with the existing retired-setting reader (renamed discard_legacy_setting for shared use); never serialize it, and remove the editable selector. Drop the old sweep: prior containers were already removed on normal completion/cancellation and swept by older startup; there is no evidence requiring permanent cleanup machinery.

## Outcomes & Retrospective

The container runtime, labels, sweep, request configuration and image refresh registration are deleted. CLI probes use the shared host resolver and the bare-worker installer. Legacy runtime values are discarded on load, omitted on save and removed from the shared settings UI. User documentation and the web settings golden now describe host execution. No database migration or protocol revision is needed. No default instance or live credentials were used.

Checks compile all six edited Rust packages. All-target clippy is clean after fixing the retired-field visibility lint and a redundant image-plan closure. Accounts quota tests (46 passed, one existing live-service test ignored), Codex transcript tests (9 passed), Claude renewal tests (25 passed), Grok process/managed lease tests (2 passed), config tests (143 passed) and daemon quota tests (25 passed) have passed. Daemon continuation tests (20 passed) and shared settings tests (10 passed) also passed. Final host/managed lookup regressions passed after switching to the shared cross-platform program-name helper and moving filesystem lookup off the runtime thread. Logs are in the report directory; the final report records the completed validation.

## Context and Orientation

mj-accounts owns quota subprocesses and reports. mj-daemon starts the pollers and image refresher. mj-core owns configuration and pinned harness installation; mj-client owns the settings schema used by terminal and web clients. mj-controller re-exports quota APIs and mj-import uses quota requests for utility selection. No store or relay format changes are needed.

## Plan of Work

Delete quota_runtime.rs and its re-exports, remove QuotaRuntimeConfig from requests and callers, and route CLI launches through quota_harness::locate. Remove obsolete container/cache-only resolver tests and retain recorded protocol and credential concurrency tests. Mark the historical container plan reverted, update visible strings, and retain legacy config parsing without emitting the setting.

## Concrete Steps

From the repository root, inspect changes with git diff and search for retired symbols. Run mbx check and mbx clippy --all-targets -- -D warnings for brokk-mj-accounts, brokk-mj-core, brokk-mj-client, brokk-mj-controller, brokk-mj-daemon and brokk-mj-import (and any additional edited crate). Run mbx test on quota and config filters outside the sandbox. Store text logs in .mj/agents/45ecb543c8154439942e02488be0c963. No live instance is required; tests retain their isolated directories.

## Validation and Acceptance

The affected crates compile and have no clippy warnings. Existing quota, renewal and config tests pass. No quota container runtime, label, sweep or image registration remains. Older auto/host/podman/docker values deserialize, are ignored and disappear when serialized. The settings surface describes host execution and offers no runtime selector.

## Idempotence and Recovery

This change edits source only. Stage only changed files, commit on the current master branch and do not push. No default daemon, credentials, images or store are modified.

## Artifacts and Notes

Long command output goes in the report directory named above. The historical .agents/plans/quota-checks-in-containers.md remains as history with a reversion note.

## Interfaces and Dependencies

quota_harness::locate returns the host CLI path, launch environment and optional harness_install::Located lease owner. QuotaRefreshRequest no longer contains runtime configuration. QuotaConfig::runtime is a deserialize-only discarded value for client/config compatibility.

Revision 2026-11-19: completed implementation and targeted validation; added real installer-receipt/lease coverage and recorded compatibility and cleanup decisions.
