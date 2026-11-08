# Withhold GitHub credentials from sessions and children

This living ExecPlan follows `.agents/PLANS.md`. Jonathan authorized implementation and pushing, explicitly without installation. Do not run a new build against the default instance, migrate the live store, install binaries, restart the live daemon, or delegate implementation.

## Purpose / Big Picture

Evaluation sessions must be able to work without receiving the person's GitHub authentication. Add `mj new --no-github-credentials`, an equivalent child spawn option, and `[github] session_credentials = false` as an instance default for newly created sessions. Resolve the choice once into each session record; children inherit a parent's opt-out and cannot turn it back on. Existing sessions retain their accepted choice across restart, resume and Move. Keep GitHub event subscriptions independent.

This controls credentials Mjolnir supplies and the normal `gh` and Git configuration of the session. Public GitHub data remains available anonymously. A bare target shares its Unix user's readable files and is not a filesystem security sandbox; record this limit honestly. Controller-owned repository preflight may authenticate before a session starts without transferring that credential to the agent.

## Progress

- [x] Confirm no current opt-out and trace provisioning, bootstrap, refresh, gh wrapper, Git helper, and child inheritance.
- [x] Obtain Jonathan's direct authorization: implement and push only. Claim #1474.
- [x] Add the durable creation policy, CLI/API/MCP options and instance default.
- [x] Enforce the policy at credential producers and worker admission, including fresh container environment and refreshed tokens.
- [x] Isolate gh and Git authentication configuration for opted-out sessions and preserve useful Git identity settings.
- [x] Add isolated security-boundary, persistence and inheritance regressions; run focused checks while iterating and final required validation once, rerunning only failing historical fixtures.
- [x] Prepare validated commits and delivery evidence for configured push review, origin/master, and the report to Jonathan and session 1ea309c3a68c04ca5e3e64aeda6ee07a; exclude installation. GitHub issue #1474 and the session report record delivery.

## Surprises & Discoveries

`--no-github-events` only excludes watcher targets. Containers receive a token in their launch environment; workers save it before their clean login re-exec. Periodic credential synchronization later updates the worker's `github-token` file. Every session gets a gh wrapper and a Git configuration that includes the original host configuration before installing GitHub-specific helpers. A missing token file alone does not isolate gh's host login store or other Git authentication settings. A real `gh` regression also exposed an inherited parent's wrapper restoring its token after the child's wrapper cleared it. Wrapper dispatch now excludes Mjolnir launcher directories before invoking the target executable; the regression explicitly creates an authenticated parent wrapper.

`mj-core/src/remote_git.rs` installs a helper only for controller-owned source preflight. It is distinct from the persistent worker helper in `mj-worker/src/worker_runtime/unix/git_env.rs`. Worker configuration is regenerated on recovery, so the accepted privacy choice must live in the durable session record, not be recomputed from today's global setting.

## Decision Log

Use a persisted `SessionRecord.no_github_credentials` boolean as the accepted session fact. The instance setting defaults to sharing credentials for compatibility; new sessions opt out when either their request disables credentials or the instance default does. A child opts out when its parent does or its own request does. Carry the accepted fact in `WorkerLaunchConfig`, and reject credential installation at the worker as well as withholding controller production. Credential refresh and recovery must consult the same stored choice.

The new database revision is breaking: an older daemon cannot honor the accepted credential prohibition and could resume or refresh a protected session with authentication. Advance the compatibility floor atomically with the new column; test only isolated stores. The current live store remains untouched because installation was explicitly excluded. Allocate the final migration number against current master before the last merge.

## Outcomes & Retrospective

The stored policy, CLI/API/MCP options, instance default, worker admission, startup, recovery and refresh gates are implemented. Real Git and gh cannot recover the dummy host, repository or parent-wrapper credentials, repeated setup removes stale worker tokens, and worker admission rejects token installation. All ten worker environment integration tests pass, including fresh and restarted credential-free container workers. Full touched-crate suites found eight obsolete fixture expectations; their targeted reruns replace broad repetition. Source delivery is commit, configured push review, and origin/master; no installation or live migration is authorized.

## Context and Orientation

`mj-cli/src/api_commands.rs` defines `mj new` and creates the API payload. `mj-controller/src/server/api/types.rs`, `mj-client/src/daemon.rs`, `server/actions.rs`, `server_runtime/actions.rs`, `daemon/create.rs`, and `controller.rs` carry creation requests into durable `mj-core/src/state.rs::SessionRecord`. `mj-controller/src/database/{schema,state_io}.rs` owns migration and SQL serialization. Every new session field must also be assigned in existing constructors and exhaustive destructures.

Child creation spans `mj-worker/src/subagent_mcp.rs`, `mj-core/src/subagent.rs::SubagentToolAction::Spawn`, daemon dispatch in `mj-controller/src/server_runtime/api.rs`, API spawn handling and `controller/subagents.rs::RegisterSubagentRequest`. Oracle and reviewer registration use the same final registration path and must inherit the parent's prohibition even without a separate option.

Credential producers are `controller/provisioning.rs`, `controller/github_app.rs`, `pollers/worker_targets.rs`, `worker_client/credential_sync.rs`, and recovery's configured token install in `session_manager/recovery.rs`. Worker launch configuration is built in `controller/worker_binary/launch.rs`. The worker's earliest container-token copy is in `mj-worker/src/main.rs`, before login re-exec. `worker_runtime/unix.rs` stages Git setup on a blocking background task. `worker_runtime/unix/requests.rs` admits credential synchronization requests. `worker_runtime/unix/git_env.rs` owns the session gh wrapper and Git config and is the single location for normal tool authentication interpretation.

## Plan of Work

First add the creation/default policy and durable storage, preserving current behavior for existing records and absent flags. Extend child spawn interfaces with an optional boolean whose false value cannot override the parent. Advance the configuration format if required so older writers cannot silently erase a new instance privacy default. Add the database revision and compatibility classification together; never rewrite shipped migrations.

Then carry the resolved policy into provisioning, refresh selection, recovery and worker launch. Suppress token environment seeding and configured-file token installation for opted-out sessions. A worker must independently refuse incoming token installs, remove any stale token copy at startup, and strip credential variables after target/login environment composition. Continue syncing harness logins, skills and non-GitHub credentials normally.

Finally, create a session-private gh configuration for opted-out workers and ensure the wrapper never reads a token file in that mode. Generate isolated Git authentication settings without inheriting host credential helpers or authorization headers. Preserve author identity where available. Keep normal authenticated-session behavior unchanged and keep command execution on existing supervised subprocess helpers. Avoid claiming isolation from files an unsandboxed bare process can explicitly read.

## Concrete Steps

Work in the current branch of `/home/jonathan/Projects/mjolnir/.mj/clones/3a22110f314bb64233ef78431ee412ad`. Use `rg` for related constructors and interpretation helpers. Build using plain Cargo and the existing mbx layout; never override Cargo target directories. Run every Cargo test outside the restricted sandbox. Keep package selection stable across focused and final commands.

Use a named instance such as `github-credentials-1474` for every real worker, daemon or CLI acceptance invocation and isolated `MJ_CONFIG_DIR`/`MJ_DATA_DIR` for migration tests. After focused regressions pass, run full touched-crate suites and `cargo clippy --all-targets -- -D warnings` once after the final code round. Run formatting and applicable JSON/Python checks. Stage only changed files, commit current branch, request configured push review, and push `HEAD:master`. Merge upstream on a rejected push; do not rebase or create a branch. Do not install.

## Validation and Acceptance

Use real Git and gh behavior with isolated fake credentials: an opted-out worker must expose no GH_TOKEN/GITHUB_TOKEN, have no worker `github-token`, use a private empty gh auth store, and supply no GitHub credential helper or authorization header. Verify real credential lookup cannot recover dummy tokens placed in an ambient gh store or Git configuration. Check that refresh cannot install a token later and that a restarted worker keeps the prohibition. Preserve existing authenticated controls by extending existing integration coverage.

Exercise session creation through its actual API/CLI surface and child/oracle registration through the existing state transition. Save and reopen the isolated database and prove the flag survives, existing records remain shared by default, and an older schema revision refuses the protected store. Confirm the global default resolves only new session choices. Reject a child's attempt to regain credentials by keeping inherited prohibition authoritative. These are persistence, independent-interface and security-boundary tests; do not add trivial default-value or list-count tests.

## Idempotence and Recovery

The session's stored decision is immutable. Restart and Move rebuild launch policy from that decision. Repeated credential synchronization must leave an opted-out worker unauthenticated. Cleanup removes only session-owned files, never the user's gh login store or Git configuration. A failed preparation reports the error through the existing supervised launch path.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1474. The comparison lead's findings described authenticated writes to evaluation issues/PRs despite task instructions. Source-level authentication withholding addresses that capability; anonymous public reads and host filesystem access remain separate concerns.

## Interfaces and Dependencies

Add `no_github_credentials` to accepted session creation and child spawn data and `WorkerLaunchConfig`; use serde defaults and omit false where legacy launch compatibility permits. Add `GithubConfig.session_credentials` with an explicit default of true. Add no new crate or dependency. Reuse existing Git config rendering, credential validation/writing, environment setup and subprocess helpers instead of creating parallel interpretations.

Revision note (2026-11-08): initial plan records authorized scope, credential paths, policy ownership, isolated migration requirements and the explicit prohibition on installation.

Revision note (2026-11-08): implemented schema revision 119 (breaking) and config version 22; added regressions for migration, creation defaults, child inheritance, real Git/gh credential lookup, configured-PAT startup, container environment and worker bootstrap. Validation is in progress.

Revision note (2026-11-08): retained the accepted policy in ownership markers so orphan adoption cannot silently reenable credential sharing; the normal launch field and database remain the execution policy sources. Initial API, migration, default/inheritance, MCP and configured-PAT tests passed. The real gh regression caught inherited wrapper authentication, fixed before final validation. Workspace clippy passed before that final wrapper/adoption change and will be repeated.

Revision note (2026-11-08): full core/controller/worker and CLI/client/chat/checkpoint/TUI validation ran. Historical schema rewinds now remove revision 119’s column, and the MCP contract includes the new optional property. Core, worker environment, client, CLI parsing, TUI, chat and checkpoint checks pass; four controller fixture and one MCP fixture rerun pass. All three isolated daemon-upgrade fixture reruns pass. Source delivery is the remaining step. Workspace clippy is clean after these fixture updates.

Revision note (2026-11-08, push review): two independent reviewers found late login-environment composition restoring token variables, and Git URL specificity allowing repository-path auth to outrank host resets. Both are being fixed: accepted launch policy populates the supervisor’s existing final exclusions, and the Git wrapper queries the command-selected repository for every matching authentication scope before executing Git. Hard-won regressions exercise a persisted supervisor spec with all four token names and path-specific repository helpers/headers through `git -C`. Workspace clippy, all five focused worker regressions, the provisioning regression, and all ten real-worker environment integration tests pass. No findings are declined.

Revision note (2026-11-08, SSH alias review): the next review identified user SSH aliases resolving to GitHub. Replaced hostname-pattern Includes with a session Git SSH wrapper that asks OpenSSH `-G` for the effective hostname, then disables GitHub authentication and multiplexed socket reuse at command precedence. A real OpenSSH regression checks both a GitHub alias and an internal alias whose authentication remains enabled. The focused Git/gh/SSH regression, formatting and workspace clippy pass. No findings are declined; installation remains excluded.

Revision note (2026-11-08, private-checkout review): preserved controller-owned authentication for private repositories independently of the worker policy. Host cache preparation receives the controller token; the shared checkout builder streams it only into GitHub HTTPS fetch/clone processes. Container creation and worker startup receive none. Reused the existing transient AWS checkout helper, scoped its credential override to GitHub hosts, and retained stdin across SSH container exec. A real Git regression checks the bootstrap-style URL helper cannot outrank the checkout credential and that no config/token is persisted. The 146 active checkout/cache/target tests, five final focused regressions, formatting and workspace clippy pass. All implementation work is complete; delivery follows configured review and push. No new build has been installed.
