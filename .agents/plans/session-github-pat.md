# Configure a dedicated GitHub credential for every session


This ExecPlan follows `.agents/PLANS.md` and remains a living record of issue #1432.

## Purpose / Big Picture


Users can set `[github] session_token_path = "~/.secrets/gh_agents_pat"` to give every managed session a dedicated GitHub token instead of distributing their host GitHub CLI login. The daemon (the controller process) reads the file on its own host; workers (the processes running sessions locally or remotely) receive only the token through the existing private credential transport. Local bare, SSH, EC2, local containers, remote containers, and sub-agents use the same selection. Token rotation reaches existing workers without restarting their turns.

## Progress


- [x] (2026-10-30) Inspect credential selection, worker wrappers, provisioning, resume, and periodic reconciliation; claim #1432.
- [x] (2026-10-30) Implement configuration, authoritative credential selection, startup installation, and reconciliation on every target.
- [x] (2026-10-30) Extend configuration and credential integration regressions; focused tests, complete core/controller suites, workspace clippy, and touched-crate formatting pass.
- [x] (2026-10-30) Commit and push the feature, install matching controller/workers, and configure the user’s PAT path; initial startup/restart/rotation smoke passes.
- [x] (2026-10-30) Install the validated override-removal follow-up and run the expanded smoke test; default daemon handoff and configured path confirmed.

## Surprises & Discoveries


Local bare sessions already use the worker-owned `gh` wrapper, but periodic GitHub credential sync explicitly excludes them. The wrapper reads `github-token` on every invocation. Containers initially receive credentials in their creation environment. Restored workers can start queued prompts before the controller reconnects, so the current token must be installed before worker startup, rather than waiting for periodic reconciliation.

## Decision Log


- Decision: Add a file path rather than token text to configuration, with precedence over GitHub App session tokens and the host login.
  Rationale: A file supports rotation without putting credentials in serialized configuration; an explicit setting must never silently fall back to a broader credential.
  Date/Author: 2026-10-30 / Codex.
- Decision: Reconcile GitHub credentials for every target with an override and publish the GitHub configuration with each credential target, replacing the App-configured flag.
  Rationale: The controller owns credential selection, and a target's configuration snapshot also makes reload invalidate obsolete background work. Every staged target participates in normal GitHub sync, including local bare. This also restores the host/App source after an override is removed, rather than leaving a stale PAT in the local worker.
  Date/Author: 2026-10-30 / Codex.

## Outcomes & Retrospective


The feature is committed as `2c54f66cd`, with the local override-removal fix in `42e8f7d4f`. After a clean upstream merge, matching controller and native/musl workers were installed from `1eb015722`. The local daemon runs `2.63.1+1eb01572` and the host’s `~/.config/mjolnir/config.toml` contains `github.session_token_path = "~/.secrets/gh_agents_pat"`. Its configuration diagnostic is ready. Ordinary sync delivered the configured PAT to this running session; verification compared bytes without printing them. The named `pat-1432-smoke` instance passed real worker startup, restart with changed token contents, and periodic rotation, under a private tmux server with fake harness credentials and dummy PATs.

A final inspection caught an override-removal defect: excluding an unconfigured local target from sync left its old PAT in place. Local targets now follow the same normal sync rule as other targets. The affected 44 tests, complete 2,204-test controller suite, workspace clippy, formatting, and diff checks pass for this follow-up. Core’s 553-test suite passed in the initial full validation; its code is unchanged in the follow-up. The expanded real-worker smoke also passed startup, restart, periodic rotation, and removing the override. The test ran in `pat-1432-smoke` under tmux using an empty ambient home, isolated config/data/profile/cache directories, a deterministic harness, and dummy credentials. No default-instance test sessions were created. Existing doctor findings are a missing Mac worker and review residue; the PAT configuration introduced no new diagnostic failure.

## Context and Orientation


`mj-core/src/config/github.rs` defines the serialized GitHub settings. `mj-controller/src/controller/github_app.rs` selects session credentials, while `controller/backend.rs` retains the host login lookup. `mj-core/src/credentials.rs` defines published credential targets and the existing token-file validation. `mj-controller/src/pollers/worker_targets.rs` selects live workers; `worker_client/credential_sync.rs` reconciles their private token files once a minute. `controller/provisioning.rs` and `controller/resume.rs` own startup and must install the selected token before a worker starts. `mj-worker/src/worker_runtime/unix/git_env.rs` already installs the invocation-time wrapper and Git HTTPS helper; no new relay message or worker format is necessary.

## Plan of Work


Add the optional controller-side path and validate it using existing path interpretation. Resolve its bounded, single-line contents with the shared GitHub-token validator on background work. Errors name the setting and path without token contents. When the setting is absent, retain App and host credential selection. Publish the configuration in credential targets and reconcile tokens for local sessions too. Install the token atomically with sensitive stdin on the target before provisioning or resume starts its worker. Ensure branch export does not replace a configured PAT with an App token. Update the shipped configuration reference and the human GitHub credential guide.

## Concrete Steps


Work from the repository root on the current branch. Use `cargo fmt --all -- --check`. Run every Cargo test outside the sandbox. During implementation run focused tests in `brokk-mj-core` and `brokk-mj-controller`; after the last change run the full suites of touched crates and `cargo clippy --all-targets -- -D warnings`. Do not redirect Cargo output or change the build cache layout. Live invocations of new binaries must use `--instance pat-1432` and isolated config, data, and profile homes. If installing, commit before starting `scripts/install.sh`, never while installation is building.

## Validation and Acceptance


Configuration round trips with the path and without token contents. A configured missing, empty, malformed, or oversized file fails without consulting host authentication. Rotating a file changes the next selected credential. Existing worker-protocol coverage verifies replacement and removal. Target-publication coverage proves local sessions now participate and configuration changes invalidate old targets. Startup transport tests run the real atomic installation command on a private temporary worker root, including paths containing spaces, and verify file contents and private permissions without emitting secrets. Isolated live sessions demonstrate the restricted token fingerprint from the first command, and a changed file propagates without restarting the session.

## Idempotence and Recovery


The setting is optional and contains no secret. Removing it restores the previous App/host source on the next sync. Atomic target replacement prevents partial token reads; lifecycle ownership prevents startup writes racing background reconciliation. All live tests use a named instance and disposable state. Never print, commit, or checkpoint the user's PAT. Preserve unrelated configuration and concurrent work. If installation would replace a newer installed build, leave the tested implementation committed and explain that limitation instead of downgrading.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1432. The user's requested source file is `~/.secrets/gh_agents_pat`; its bytes are not part of this plan.

## Interfaces and Dependencies


Use `GithubConfig.session_token_path: Option<PathBuf>`, existing `path_input` helpers, `credentials::validate_github_token`, `CommandSpec::with_sensitive_stdin`, the target command wrappers, and existing relay token installation/removal. No new dependency, crate, database migration, or protocol revision is required.

Plan created 2026-10-30 after tracing startup and credential ownership.

## Milestones


First complete configuration and authoritative token selection, then prove it with configuration and precedence/rotation regressions. Next cover every worker-start path and ordinary periodic synchronization without changing container creation. Finally run full validation, commit and push, then install and set the user’s path after verifying the new build is current.

Plan revised 2026-10-30 to incorporate the user’s approved scope; container environment delivery and ordinary periodic sync are retained.

Implementation note 2026-10-30: refresh the override in the shared worker-start helper and automatic recovery, covering restart, resume, upgrades, and parked-child wake. App-only export renewal skips PAT overrides. The host configuration currently has no GitHub App; the user explicitly authorized setting the PAT path after installation.

Validation update 2026-10-30: the complete core and controller suites pass without failures. Formatting and diff checks pass for touched crates. The documentation sync script passes; the docs npm checker is unavailable because dependencies are not installed. An isolated native-worker/TUI smoke driver is prepared at `/mnt/optane/tmp/mj-pat-1432/smoke.py` for startup, restart, and ordinary rotation.

Follow-up 2026-10-30: normal sync must continue after removing the optional path, otherwise local workers retain the old PAT. Fixed the target-selection rule and marked its regression as hard-won. The source file, container environment delivery, worker protocol, and database schema are unchanged.

Completion evidence 2026-10-30: installation completed from `1eb015722`; the final named-instance result is `/mnt/optane/tmp/mj-pat-1432/artifacts/session-pat-seed-1432-2954362/pat-result.json`, with all four checks true. Default daemon PID 3014865 reports `2.63.1+1eb01572`; its PAT setting is confirmed without printing the token. The incoming upstream checkpoint and native gh-approval changes merged without conflicts; their recorded full validation was reused, and the final live smoke exercises the composed worker. No implementation work remains. This final plan-only update records deployment evidence and does not require reinstalling matching binaries.
