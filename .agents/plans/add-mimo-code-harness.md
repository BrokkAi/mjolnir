# Add MiMo Code as a managed ACP harness

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change, a person can add a MiMo Code profile, have Mjolnir detect the installed `mimo` CLI, install a pinned MiMo Code runtime on managed bare targets, and run a full ACP session with model selection, permissions, tools, cancellation, checkpoint restore, and the reference container image. A person can verify the result by running `mj setup`, seeing a `MiMo Code` profile, completing `mj login`, and starting a session whose transcript streams through the existing TUI.

MiMo Code currently reports its ACP agent identity as OpenCode because its ACP implementation descends from OpenCode. Mjolnir must never infer the harness from that field. The configured `HarnessKind` remains authoritative.

The separate architectural request to preserve unknown harness sessions across daemon versions is tracked as issue #1325. This plan adds MiMo Code using the current persisted-enum schema and therefore includes the required breaking migration.

## Progress

- [x] Add MiMo Code identity, discovery metadata, authentication, and ACP policy mappings.
- [x] Pin and install the official `@mimo-ai/cli` package for managed workers and the reference image.
- [x] Add DB, checkpoint, import, SessionWiki, quota, and plan-mode handling.
- [x] Update TUI choices, documentation, tests, and container pin checks.
- [x] Run targeted tests, then full validation on touched crates.
- [x] Review the diff and commit the completed harness support on the current branch.

## Surprises & Discoveries

- Observation: MiMo Code implements ACP v1 inside its own CLI; there is no separate `mimo-acp` bridge package.
  Evidence: `packages/cli/src/cli/cmd/acp.ts` registers `mimo acp`, and `packages/cli/src/acp/agent.ts` advertises protocol version 1 plus load/list/resume/fork session capabilities.

- Observation: MiMo Code's ACP `initialize` response identifies the agent as `OpenCode`.
  Evidence: `packages/cli/src/acp/agent.ts` sets `agentInfo.name` to `"OpenCode"` and describes an `opencode` login method.

- Observation: MiMo Code supports an official `MIMOCODE_HOME` root that contains sibling `config`, `data`, `state`, and `cache` directories.
  Evidence: `packages/shared/src/global.ts` resolves all four paths beneath `MIMOCODE_HOME` when it is non-empty and absolute.

- Observation: MiMo Code's own ACP README is stale.
  Evidence: the README says streaming and tool reporting are absent, while `packages/cli/src/acp/agent.ts` emits `agent_message_chunk`, `agent_thought_chunk`, and tool-call updates.

- Observation: MiMo Code creates a private HTTP shim inside its ACP process, so binding a loopback port must be available even though the protocol itself is stdio.
  Evidence: the ACP process failed with `Failed to start server on port 0` inside the restricted sandbox and returned a valid initialize response with protocol version 1 outside it.

- Observation: MiMo Code ignores ACP `additionalDirectories` in `session/new`.
  Evidence: its `Agent.newSession` uses `params.cwd` and MCP servers but does not consume additional workspace roots. Mjolnir therefore rejects multi-repository MiMo bundles instead of silently omitting repositories.

- Observation: the npm package contains a Node launcher and a platform-specific native MiMo payload. The launcher remains executable without the postinstall helper.
  Evidence: after `npm ci`, `node_modules/.bin/mimo` resolved to `@mimo-ai/cli/bin/mimo`, and `mimo --version` printed `0.1.15`.

## Decision Log

- Decision: Use `HarnessKind::Mimo` with the stable identifier `mimo`.
  Rationale: MiMo Code is the product name and `mimo` is the official executable; the repository already uses short harness identifiers such as `muse` and `opencode`.
  Date/Author: 2026-10-21 / Codex

- Decision: Represent a profile home as a `MIMOCODE_HOME` root rather than reusing OpenCode's XDG-config-directory convention.
  Rationale: `MIMOCODE_HOME` is MiMo Code's official complete isolation boundary. It keeps config, credentials, database, state, and cache under one session-owned directory and avoids leaking into a user's XDG data tree.
  Date/Author: 2026-10-21 / Codex

- Decision: Use `.mimocode` beneath the user's home as Mjolnir's default profile root when `MIMOCODE_HOME` is not set.
  Rationale: Mjolnir requires one root for a profile, while a stock MiMo Code installation without `MIMOCODE_HOME` splits its config and data across XDG directories. Detection still finds an installed `mimo` binary and offers an unauthenticated profile; `mj login` then populates the isolated root.
  Date/Author: 2026-10-21 / Codex

- Decision: Pin `@mimo-ai/cli@0.1.15` and launch `mimo acp`.
  Rationale: This is the official npm distribution and ACP entry point at the time of implementation. It includes platform-specific native payloads.
  Date/Author: 2026-10-21 / Codex

- Decision: Set `MIMOCODE_DISABLE_AUTOUPDATE=1` for managed and container launches.
  Rationale: MiMo Code otherwise applies patch updates automatically, which could replace the pinned runtime during a live session.
  Date/Author: 2026-10-21 / Codex

- Decision: Set `MIMOCODE_CONFIG_DIR` to the staged `config/` directory.
  Rationale: MiMo Code otherwise also consults `<HOME>/.mimocode`; giving its
  explicit config-directory override the staged path makes the session profile
  authoritative while preserving the upstream layered config model.
  Date/Author: 2026-10-21 / Codex

- Decision: Map MiMo Code's normal mode to `build` and its planning mode to `plan`.
  Rationale: Its built-in primary agents are named `build` and `plan`; treating the normal mode as `default` would make Mjolnir's Plan toggle fail to restore execution mode.
  Date/Author: 2026-10-21 / Codex

- Decision: Renumber the MiMo harness migration from revision 87 to revision
  90 when integrating the latest `origin/master`.
  Rationale: revisions 87 through 89 shipped concurrently upstream. MiMo's
  reconstructed harness-kind tables must include their schema, and its ledger
  entry must follow those migrations rather than collide with them.
  Date/Author: 2026-10-21 / Codex

## Outcomes & Retrospective

Implementation is complete and merged with the latest upstream OpenCode 2,
container-image, checkpoint, and migration work. The harness is discoverable, installable, launchable
through ACP, represented in persisted sessions, checkpointed with its SQLite
database and WAL, exposed in TUI and documentation, and covered by focused
regressions. The pinned package's real `mimo acp` process returned a valid ACP
initialize response outside the restricted sandbox.

Follow-up (2026-10-22): the first real session failed every prompt with
`ACP prompt returned no session updates`. The staged profile had no
`data/auth.json`, because the conventional `~/.mimocode` home is the official
installer's executable directory, not a `MIMOCODE_HOME` root. A stock MiMo Code
install keeps its configuration in `~/.config/mimocode` and its login in
`~/.local/share/mimocode/auth.json`, so Mjolnir never found or staged the login;
MiMo Code then logged `Invalid API Key` to its own log and ended the turn with
no ACP update. Staging now resolves the stock XDG trees for a profile at the
conventional home, layers the profile's own `config/` over the stock
configuration so synced skills still win, and prefers a login that Mjolnir
created in the profile root once one exists. `mj login` for that profile leaves
`MIMOCODE_HOME` unset so it signs in to the stock install. A configured
`MIMOCODE_HOME` root keeps its existing `config/` and `data/` layout.

Validation passed for the full core, checkpoint, worker, client, controller, and TUI
suites; controller validation skipped one unrelated host-tool digest test
because this host lacks `shasum`. TUI validation passed apart from one stale
expected diagnostic, which was corrected and rerun successfully. After the
merge, the controller suite had one timing-sensitive cancellation test fail
once and pass on immediate focused rerun. The CLI suite passed except the
pre-existing PTY auto-theme test, which failed identically on
rerun while the fixture's restricted `PATH` could not find `awk` or `dirname`.
Clippy passed with `-D warnings` for every touched crate. The reference
container image was not built locally.

## Context and Orientation

`mj-core/src/config/harness.rs` defines the closed `HarnessKind` enum and the behavior metadata shared by controller, worker, checkpointing, skills, credentials, and UI. `mj-controller/src/setup.rs` iterates `HarnessKind::ALL` to discover default homes, environment overrides, and installed CLIs. The worker resolves a managed runtime in `mj-worker/src/worker_runtime/harness.rs` and launches the harness through the ACP supervisor.

ACP is the Agent Client Protocol: JSON-RPC over standard input and output that lets Mjolnir send prompts, approvals, model selections, and cancellation, and receive streamed messages and tool calls. MiMo Code provides that server directly through `mimo acp`.

MiMo Code stores its native session and related data in `data/mimocode.db` plus a SQLite WAL file beneath `MIMOCODE_HOME`. The `state` and `cache` siblings are not conversation state. Checkpoint code must capture the database and WAL while omitting the rebuildable shared-memory file.

The controller persists a session's harness kind in the `sessions` and `hidden_native_sessions` SQLite tables. The CHECK constraints currently admit every shipped harness. Adding a kind is classified as breaking because an older daemon does not understand the new value; this implementation follows the migration pattern used for OpenCode and advances the compatibility floor.

## Plan of Work

First, extend `HarnessKind` and its metadata. Add `Mimo`, its display and stable identifiers, binary name, `MIMOCODE_HOME` environment root, `.mimocode` default home, `data/auth.json` credential marker, `mimo auth login` command, `AGENTS.md`, `config/skills` synchronization, `data` native state root, standard guardian support, and `mimo acp` arguments. Reject an explicit `MIMOCODE_HOME` in a profile's environment because the profile's `home` is that root. Use MiMo Code's unconstrained startup environment instead of staging a permission file.

Second, add the runtime pin and installation. Define `MIMO_CLI_VERSION = "0.1.15"` and a pin whose entrypoint is `node_modules/.bin/mimo`. Add package and lock assets under `mj-worker/assets/harnesses/mimo/`. Install them with the existing `install_npm` path, require Node 22 or newer, set the launch environment to disable auto-update, and validate the executable after relocation. Unconstrained execution uses `MIMOCODE_DANGEROUSLY_SKIP_PERMISSIONS=1`, which avoids the unreliable precedence of a staged JSON permission string. Add the same npm package to `containers/Containerfile.agent-dev` and verify `mimo --version`.

Third, integrate ACP and persistence. Map MiMo's `build`/`plan` modes in `mj-core/src/acp/surface.rs`. Use the generic ACP permission, MCP, streaming, model discovery, cancellation, and resume behavior. Add MiMo to credential freshness parsing, native import labels and unsupported locator, SessionWiki tool-name mapping, quota's unavailable branch, and checkpoint capture/restore validation. Add a new breaking database migration after upstream revision 89 that admits `mimo` in both harness-kind constraints and raises the compatibility floor.

Fourth, expose the harness. Add `mimo` to the TUI profile-kind choices. Stage the full `config` directory and `data/auth.json` into a session profile. Update the runtime, profile, quota, container, and custom-image documentation. Extend the container pin test and the harness mapping tests.

Finally, test the observable path. Use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for database and integration checks. Prove that setup discovers a profile from a `MIMOCODE_HOME` with `data/auth.json`, that a CLI-only installation is offered before login, that the database migration preserves existing sessions, that Plan mode returns to `build`, and that checkpoint export/restore preserves the MiMo SQLite database and WAL. Then run the full tests and clippy for every touched crate and commit the validated change.

## Concrete Steps

Work from the repository root:

    /home/ryansvihla/code/mjolnir/.mj/clones/d0ae4f98f59ec9daa8a0e493193a5131

Generate and inspect the locked npm asset:

    npm install --package-lock-only --ignore-scripts --prefix mj-worker/assets/harnesses/mimo @mimo-ai/cli@0.1.15

Run focused tests as each slice lands, for example:

    cargo test -p brokk-mj-core harness_mapping_and_permission_modes_are_fixed
    cargo test -p brokk-mj-controller bridge_fallback_pins_match_the_agent_dev_containerfile
    cargo test -p brokk-mj-checkpoint mimo
    cargo test -p brokk-mj-controller recent_revisions_upgrade_directly_and_preserve_user_data

Run the final validation outside the restricted sandbox because the suites use loopback sockets:

    cargo test
    cargo clippy --all-targets -- -D warnings

## Validation and Acceptance

Acceptance requires all of the following:

- With a temporary home containing `.mimocode/data/auth.json` and a fake `mimo --version`, setup reports MiMo Code as authenticated.
- With only a fake `mimo --version`, setup reports a MiMo Code profile and marks it unauthenticated.
- A managed local-bare launch resolves the pinned `node_modules/.bin/mimo`, passes `acp`, and sets `MIMOCODE_DISABLE_AUTOUPDATE=1`.
- A recorded or real ACP probe completes `initialize`, `session/new`, model selection, and a prompt without a prompt during discovery.
- The Plan toggle selects `plan` and restores `build` for MiMo.
- A checkpoint captures and restores `data/mimocode.db` and its `-wal` file while omitting `-shm`.
- A store at revision 89 upgrades to revision 90 without losing existing sessions, and the fresh-store harness constraint admits `mimo`.
- The reference image installs exactly `@mimo-ai/cli@0.1.15` and exposes `mimo acp`.
- Workspace tests and clippy pass, and the final commit includes only task-related changes.

## Idempotence and Recovery

The npm asset generation is repeatable. Managed installation uses the existing content-addressed cache, staging rename, manifest receipt, and lease; an interrupted install is removed on retry. The database migration is additive to the enum constraint but wraps table rebuilds and foreign-key checks in a transaction. Tests use temporary isolated stores, and no validation may point at the default instance.

If a test database migration fails halfway, discard only that test temporary directory and rerun from a revision-89 fixture. Do not rewrite an already-applied migration; any correction after shipping must use a later revision.

## Artifacts and Notes

The architectural issue for preserving unknown harness sessions is:

    https://github.com/BrokkAi/mjolnir/issues/1325

The official MiMo Code ACP command is:

    mimo acp

## Interfaces and Dependencies

The existing interfaces remain authoritative:

    pub enum HarnessKind {
        // existing variants
        Mimo,
    }

    pub const fn pin(kind: HarnessKind) -> HarnessPin
    pub const fn npm_bridge(kind: HarnessKind) -> Option<NpmBridge>

No `npm_bridge` entry is needed because `mimo` is the harness executable itself, not an adapter that launches a separate CLI. Add `HarnessKind::Mimo => ["acp"]` to `bridge_args`.

The managed npm asset must depend on exactly:

    "@mimo-ai/cli": "0.1.15"

The worker entrypoint is:

    node_modules/.bin/mimo

The profile home is a `MIMOCODE_HOME` root. Its authentication file is:

    data/auth.json

Its native checkpoint files are:

    data/mimocode.db
    data/mimocode.db-wal

The unconstrained launch environment is:

    MIMOCODE_DANGEROUSLY_SKIP_PERMISSIONS=1
