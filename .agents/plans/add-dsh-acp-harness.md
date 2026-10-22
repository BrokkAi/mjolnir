# Add dsh-acp (DeepSeek) as a managed ACP harness

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change a person can configure a profile with `kind = "dsh"`, give it a DeepSeek API key, and run Mjolnir sessions on DeepSeek models through BrokkAi's `dsh-acp`, a DeepSeek coding agent that speaks the Agent Client Protocol (ACP) itself. ACP is JSON-RPC over standard input and output: Mjolnir's worker sends prompts, model and mode selections, permission answers and cancellation, and receives streamed messages, thoughts, tool calls and usage. On a bare target the worker installs the exact pinned `dsh-acp` release binary, verified by SHA-256, with no Node.js needed; the reference container image carries the same binary. Sessions stream through the existing TUI, the Plan toggle works, writes and commands ask for approval on raw targets and run freely inside containers, the session survives suspend, checkpoint restore at a different workspace path, and resume, and token usage is priced.

To see it working: put `DEEPSEEK_API_KEY` in the environment Mjolnir starts with (or in `secrets.toml` and reference it from the profile), run `mj setup` or add `[profiles.dsh] kind = "dsh" home = "~/.dsh-acp"`, start a session on `localhost`, and prompt it.

The source project is https://github.com/BrokkAi/dsh-acp. Version 0.2.0 is the first release published to GitHub Releases (six platform archives with `.sha256` files and an embedded `release.json`) and to npm as `@brokkai/dsh-acp` (all six binaries behind a Node launcher).

## Progress

- [x] (2026-10-22) Read dsh-acp 0.2.0 source and probe its real binary over ACP (initialize, session/new, set mode, prompt without a key).
- [x] (2026-10-22) Correct the separate guardian classification of OpenCode and MiMo Code (commit 071618d8f), which this harness's design depends on.
- [x] (2026-10-22) Add `HarnessKind::Dsh` identity, authentication, environment ownership, execution policy, and launch script in `mj-core`.
- [x] (2026-10-22) Pin the release, add `mj-worker/assets/dsh/runtime.json`, and install it on managed targets.
- [x] (2026-10-22) Controller: container launch, staging, migration 92, checkpoint capture and relocation, quota, utility model, import, SessionWiki, TUI choices.
- [x] (2026-10-22) Container image and its pin test; documentation.
- [x] (2026-10-22) Targeted tests, then full validation of touched crates and clippy.
- [x] (2026-10-22) Live check in an isolated instance with a local bare target against a scripted DeepSeek-compatible server.
- [x] (2026-10-22) Commit.

## Surprises & Discoveries

- Observation: Mjolnir already had a harness called DSH, `HarnessKind::Deepseek` with id `deepseek`, which ran DeepSeek's own `@deepseek-ai/dsh`. It was removed in 5463a82ef (issue #1027) in favour of DeepSeek through a Codex profile. The store still tolerates `'deepseek'` in its `harness_kind` CHECK constraints, and listing sessions skips such rows with a warning.
  Evidence: `mj-controller/src/database/baseline.sql` lines 265 and 294; commit 5463a82ef message.

- Observation: dsh-acp has no home directory, no configuration file, no login file and no skills. It reads `DEEPSEEK_API_KEY` from its environment and keeps sessions under `DSH_ACP_DATA_DIR/sessions/<id>.json`; the data directory defaults to `.dsh-acp` relative to its working directory, which would be inside the repository.
  Evidence: `src/main.rs` and `src/config.rs` in dsh-acp v0.2.0.

- Observation: dsh-acp reads no instruction file (no `AGENTS.md`). Mjolnir delivers its session guidance by appending to the staged home's instruction file, so without a bridge that guidance would silently never reach the model. dsh-acp appends `DSH_ACP_SYSTEM_PROMPT` to its built-in system prompt and rebuilds that prompt from its configuration on every `session/load`.
  Evidence: `Config::built_in_system_prompt` and `SessionStore::get` in dsh-acp.

- Observation: each session record persists `config.permission_mode`, `config.mode` and `cwd`, and `session/load` refuses a request whose `cwd` differs from the stored one ("session cwd mismatch").
  Evidence: `load_session` in `src/agent.rs`; a probe created `sessions/dsh-<uuid>.json` at `session/new`, before any prompt.

- Observation: the real 0.2.0 binary answers initialize with protocol 1, `loadSession`, session `resume`, `fork`, `list`, `close`, `systemPrompt`, HTTP MCP, and the `_session/steering` extension. `session/new` offers config options `model` (`deepseek-flash`, `deepseek-v4-pro`), `mode` (`agent`, `plan`), `permission_mode` (`ask`, `allow`, `deny`) and `network`. A prompt without a key fails with "Authentication required".
  Evidence: probe transcript, 2026-10-22.

- Observation: dsh-acp 0.2.0 reports session-cumulative token usage in its `PromptResponse`, which ACP v1 defines as "token usage for this turn", and does not declare `_meta["mjolnir.dev/usage-scope"]`. Mjolnir therefore records DSH usage as `unspecified` and keeps it out of totals and cost, which is correct: counting it as per-turn would double-count. The fix belongs in dsh-acp (per-turn usage plus the `turn` declaration muse-acp makes); Mjolnir then needs no change.
  Evidence: `final_usage` starts from `initial_record.usage` in `src/agent.rs`; a live second turn of two requests (2,460 tokens) reported 3,690, the session total; `mj usage` showed "2 reported an unknown scope".

- Observation: migration 90 skipped a table only when `'mimo'` was the final CHECK member, so re-running it after `'dsh'` was appended failed with "unexpected sessions harness constraint" in every test that rolls a current store back. Migration 74 had the same defect fixed when MiMo was added; the skip test now recognizes the value anywhere, which changes no store's resulting schema.
  Evidence: 20 controller database tests failed before the fix and pass after it.

- Observation: unrelated to this harness, `mj resume` answered "unknown workspace" for a session created with `mj new --workspace-id default` until the daemon restarted: the viewer snapshot's workspace list, which the API validates against, did not include a workspace created after the daemon started.
  Evidence: the same resume succeeded immediately after `mj daemon restart`.

- Observation: dsh-acp runs commands with no OS sandbox; its file tools are confined to the workspace roots and writes, commands and MCP calls ask through ACP unless allowed. The same is true of OpenCode and MiMo Code, which Mjolnir had listed as guardian harnesses; that was fixed separately in 071618d8f.

## Decision Log

- Decision: Name the variant `HarnessKind::Dsh`, identifier `dsh`, display name `DSH`, CLI `dsh-acp`.
  Rationale: `deepseek` must not be reused, because rows the removed harness wrote would be read back as dsh-acp sessions with an incompatible native format. `DSH` is what Mjolnir's UI already called DeepSeek's harness, and dsh-acp calls itself "DeepSeek Harness".
  Date/Author: 2026-10-22 / Claude

- Decision: The profile home is the `DSH_ACP_DATA_DIR` root, conventionally `~/.dsh-acp`. Staging copies only `AGENTS.md` (and the inert `skills/` tree every harness carries); native state is `sessions/<id>.json`.
  Rationale: the data directory is the only per-profile directory dsh-acp has. Pointing it at the staged home keeps sessions out of the repository and inside the checkpointed native tree.
  Date/Author: 2026-10-22 / Claude

- Decision: Authenticate with `AuthScheme::ApiKey { env_key: "DEEPSEEK_API_KEY" }`, inherited from the environment Mjolnir starts with like a Codex `env_key`, and refuse to start a profile without it (`ensure_ready`). Setup reports a dsh home authenticated when the key resolves. `credential_file_name` names `auth.json`, a file no dsh-acp release writes, because the kind-level function is total and every reader tolerates a missing file.
  Rationale: there is no login to run or sync. Making the marker optional would touch 43 call sites for no behavioural gain.
  Date/Author: 2026-10-22 / Claude

- Decision: Install managed runtimes from the GitHub release archives, pinned per platform in `mj-worker/assets/dsh/runtime.json` with SHA-256, entrypoint `dsh-acp` at the install root, and run `dsh-acp --version` after extraction. The reference image installs from the same file with `scripts/install-dsh-acp.py`. The container fallback for custom images is `npx -y @brokkai/dsh-acp@0.2.0`.
  Rationale: one file owns the pin and the checksums for both paths; the native archive is about 5.5 MB against 85 MB for the npm package and needs no Node.js. The version run turns a musl or too-old-glibc target into a clear install failure instead of a confusing bridge exit.
  Date/Author: 2026-10-22 / Claude

- Decision: Launch through one shell prelude, `dsh_acp_script()`, used by the managed launch and the container fallback. It fails when `DSH_ACP_DATA_DIR` is unset, sets `DSH_ACP_SYSTEM_PROMPT` to the staged `$DSH_ACP_DATA_DIR/AGENTS.md` (empty when there is none), and execs the harness.
  Rationale: this is dsh-acp's own channel for added instructions, it is rebuilt on every launch so a resumed session gets the current guidance, and always setting it means a target's login shell cannot leak a different value in.
  Date/Author: 2026-10-22 / Claude

- Decision: Realize execution policy with the ACP `permission_mode` option on every open: `allow` when unconstrained, `ask` under configured approvals. Reject `DSH_ACP_PERMISSION_MODE` and `DSH_ACP_SYSTEM_PROMPT` in a profile's `environment`.
  Rationale: the record persists its permission mode, so a launch variable would not reach a resumed session, and a session moved from a container to a raw target would keep `allow`. Mjolnir owns both values, so a profile setting would only be silently overridden; refusing it at load says who owns them.
  Date/Author: 2026-10-22 / Claude

- Decision: DSH does not support guardian approvals, so the wizards show the raw-target DANGER warning.
  Rationale: it has no sandbox; `ask` prompts are approvals on host execution, not a guardian.
  Date/Author: 2026-10-22 / Claude

- Decision: On checkpoint restore at a new workspace path, rewrite the record's top-level `cwd` to the target primary checkout; capture only `sessions/<id>.json`, never the `.<id>.<uuid>.tmp` files of an interrupted atomic write.
  Rationale: `session/load` refuses a different `cwd`, so without the rewrite a moved session cannot resume. `additional_directories` is replaced by each load request and needs no rewrite.
  Date/Author: 2026-10-22 / Claude

- Decision: Migration 92 admits `'dsh'` in both harness-kind constraints and raises the compatibility floor (breaking), exactly as migration 90 did for MiMo Code.
  Rationale: an older daemon cannot represent the value and a full state save could drop the session.
  Date/Author: 2026-10-22 / Claude

- Decision: Quota reads "API" (usage-priced) without an error. A DSH profile serves utility inference in the DeepSeek family only when its base URL is DeepSeek's own host.
  Rationale: DeepSeek bills by usage. dsh-acp posts to `<base>/chat/completions` while the shared OpenAI client appends `/v1`; the two agree only on `api.deepseek.com`, the same rule the DeepSeek-on-Codex path uses.
  Date/Author: 2026-10-22 / Claude

- Decision: A missing DSH home is reported by `mj doctor` with "create it" rather than login advice, and the profile docs say to create it.
  Rationale: with no login, nothing ever creates the directory, and Move refuses a profile whose home is missing.
  Date/Author: 2026-10-22 / Claude

## Outcomes & Retrospective

The harness is implemented and was exercised live in an isolated instance (`--instance dsh-live`) with a local bare target and a scripted DeepSeek-compatible server reached through `DEEPSEEK_BASE_URL`. Model discovery installed the pinned binary from the verified GitHub archive into the managed cache and listed `deepseek-flash` and `deepseek-v4-pro`. A session streamed a reply; the upstream request carried the profile key as a bearer token and a system prompt holding the profile's staged `AGENTS.md`, and Mjolnir's `mj-agents` and `mj-memory` MCP servers appeared as dsh-acp tools. A command under configured approvals stopped at "DSH requests permission: Run touch dsh-proof.txt" and ran in the session clone only after approval. The native record lived in the staged home, never in the repository. After setting `permission_mode` to `allow` by hand, suspending, restarting the daemon and resuming, the same native session came back from the checkpoint with all of its messages and the history reached the model, and `permission_mode` was `ask` again.

Validation: workspace clippy with `-D warnings` (excluding `mj-desktop`, whose GTK build dependency is unavailable in this container) and the full suites of `mj-core`, `mj-checkpoint`, `mj-worker`, `mj-tui`, `mj-controller` and the `mj` CLI. Not exercised live: a container or EC2 target (no container engine here), the `allow` policy on an unconstrained target, and the TUI's Plan toggle, which the API does not expose; the toggle's mapping is covered by `plan_exit_selects_each_harness_own_normal_mode` and dsh-acp accepted `mode` `plan` and `agent` live. The reference image was not built; its installer script ran successfully on its own.

Remaining limitation: DSH token usage is recorded but not totalled or priced until dsh-acp reports per-turn usage and declares its scope (see Surprises).

## Context and Orientation

`mj-core/src/config/harness.rs` defines the closed `HarnessKind` enum and every per-harness fact: identifier, display name, home variable, conventional home, credential file, instruction file, skills directories, execution enforcement (how a target's policy becomes harness controls), guardian support, native session directories and bridge arguments. `HarnessProfile` in the same file decides authentication (`auth_scheme`), readiness (`ensure_ready`) and environment inheritance (`inherit_provider_key`). Adding a variant makes every exhaustive `match` fail to compile, which lists most sites to update; a few sites compare with `==` or `matches!` and must be found by searching for `HarnessKind::Mimo`.

`mj-core/src/harness_runtime.rs` holds the exact pinned versions and `pin()` (install identity, display version, entrypoint), shared by controller and worker. `mj-worker/src/worker_runtime/harness.rs` installs a pinned runtime into `~/.cache/mjolnir/harnesses/<id>/<install_id>` on bare targets ("managed") and builds the launch command. Container targets are "ambient": `bridge_launch` in `mj-controller/src/controller/worker_binary/harness.rs` returns a shell script that uses the image's executable. `stage_profile` in the same file copies a profile's allowlisted files into the per-session staged home the harness runs from.

`mj-checkpoint` captures a harness's native files (`native_scan.rs`) and restores them (`restore.rs`), rewriting workspace paths where a harness stores them. `mj-controller/src/database/schema.rs` migrates the SQLite store. `mj-controller/src/quota.rs`, `utility_llm.rs`, `import/native.rs` and `sessionwiki/harness_adapters.rs` each match on the harness. `mj-tui/src/setup/schema.rs` lists the profile kinds the settings editor offers. `containers/Containerfile.agent-dev` builds the reference image; `bridge_fallback_pins_match_the_agent_dev_containerfile` in `mj-controller/src/controller/worker_binary/tests.rs` keeps it in step with the pins.

## Plan of Work

Core identity: add `Dsh` to `HarnessKind` and `ALL`, with `DSH_ACP_DATA_DIR`, `.dsh-acp`, `auth.json` (documented as never written), `dsh-acp`, `AGENTS.md`, `skills`, no harness-owned skills, id `dsh`, display `DSH`, install advice naming the npm package and the release archives, native session directory `sessions`, empty bridge arguments, no guardian, no MCP config file. Add `HarnessKind::api_key_variable` (`DEEPSEEK_API_KEY` for DSH) and route `auth_scheme`, `inherit_provider_key`, `ensure_ready` and setup's authentication check through it. Add the reserved-variable check to `HarnessProfile::validate`. Execution enforcement: `ask` and `allow` through `acp_setting` on `permission_mode`. Plan control: DSH's normal mode is `agent`, set through the `mode` config option. Missing-session detection stays `false`.

Runtime: `DSH_ACP_VERSION = "0.2.0"`, pin `dsh-acp-0.2.0` with entrypoint `dsh-acp`, `dsh_acp_script()`. Worker: `install_dsh` downloads `https://github.com/BrokkAi/dsh-acp/releases/download/v<version>/dsh-acp-v<version>-<target>.tar.gz`, verifies the SHA-256 from `runtime.json`, extracts `dsh-acp-v<version>-<target>/dsh-acp`, moves it to the install root, and runs `--version`; launch is `sh -c '<script>' mj-dsh <install>/dsh-acp`.

Controller: `bridge_launch` for DSH resolves `dsh-acp` on `PATH` or `npx -y @brokkai/dsh-acp@<version>`, then runs the script. Staging needs no extra files. Migration 92. Checkpoint: capture `sessions/*.json` without dot-files; restore rewrites `cwd`. Quota: `API`. Utility: DeepSeek family when the base URL host is `api.deepseek.com`. Import and SessionWiki: not supported yet, with clear errors like MiMo's. TUI: add `dsh` to the profile kinds.

Image: `COPY mj-worker/assets/dsh/runtime.json` and `scripts/install-dsh-acp.py`, install into `/usr/local/bin`, check `dsh-acp --version`. Extend the pin test. Docs: profiles, overview, targets, security, containers, custom images, quickstart, the `mj` skill's configuration reference, and the Harness pins section of `AGENTS.md`.

## Concrete Steps

Work from the repository root. Run every Cargo test outside the restricted sandbox. Focused checks while working:

    cargo test -p brokk-mj-core config
    cargo test -p brokk-mj-checkpoint dsh
    cargo test -p brokk-mj-controller dsh
    cargo test -p brokk-mj-controller bridge_fallback_pins_match_the_agent_dev_containerfile
    cargo test -p brokk-mj-controller recent_revisions_upgrade_directly_and_preserve_user_data
    cargo test -p brokk-mj-worker dsh

Final validation:

    cargo test -p <every touched crate>
    cargo clippy --all-targets -- -D warnings

Live check (inside a Mjolnir session the agent environment overrides `--instance`, so unset it):

    env -u MJ_CONFIG_DIR -u MJ_DATA_DIR -u MJ_SESSION_ID target/debug/mj --instance dsh-live daemon status

with an instance config holding a `kind = "dsh"` profile whose environment points `DEEPSEEK_BASE_URL` at a local scripted server and sets a dummy key, and `[targets.localhost] kind = "bare"`.

## Validation and Acceptance

Acceptance is: a store at revision 91 upgrades to 92 keeping its sessions, and a fresh store admits `dsh`; a profile without `DEEPSEEK_API_KEY` is refused with a message that says where to set it, and one that sets `DSH_ACP_PERMISSION_MODE` or `DSH_ACP_SYSTEM_PROMPT` fails to load; a checkpoint keeps `sessions/<id>.json` and drops temporary files, and restore at a new path rewrites `cwd`; the container pin test fails if the image and `runtime.json` disagree; and, live, a managed local bare session installs the verified binary, receives Mjolnir's staged instructions in its system prompt, streams a reply from the scripted server, asks for permission before a command under configured approvals, toggles Plan mode, and resumes after suspend.

## Idempotence and Recovery

The managed install uses the existing content-addressed cache, staging directory, manifest receipt and lease; an interrupted install is removed on retry. The migration rebuilds the two tables in one transaction with foreign keys checked, and skips a table that already admits `'dsh'`. Live checks use an isolated instance only.

## Interfaces and Dependencies

    pub enum HarnessKind { ..., Mimo, Dsh }
    pub const fn api_key_variable(self) -> Option<&'static str>
    pub const DSH_ACP_VERSION: &str = "0.2.0";
    pub fn dsh_acp_script() -> String

`mj-worker/assets/dsh/runtime.json`:

    {"version": "0.2.0", "platforms": {"linux-x86_64": {"target": "x86_64-unknown-linux-gnu", "sha256": "..."}, ...}}
