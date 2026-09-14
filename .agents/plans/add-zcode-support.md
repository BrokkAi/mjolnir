# Add native ZCode sessions and sub-agent support

This ExecPlan follows `.agents/PLANS.md` and must be maintained from research through implementation. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


Mjolnir users will be able to select a ZCode profile, discover its GLM models, and use the actual ZCode coding agent through normal Mjolnir chat. Claude and Codex parents will be able to launch ZCode children, inspect their transcripts, send follow-up input, interrupt them, and close them in the existing sub-agent workspace. Each child shares its parent's target and project directory while retaining its own conversation and profile state.

The first delivery includes managed ZCode backend installation and the default container image, as well as local bare and raw SSH targets. The adapter and backend are both pinned and managed by Mjolnir. A child never installs a separate container or changes its parent's target. The authenticated Linux installation is the live acceptance platform.

## Progress


- [x] (2026-09-13) Inspect Mjolnir harness integration, sub-agent ownership, quota selection, profile staging, and database migration requirements.
- [x] (2026-09-13) Locate installed ZCode desktop backend; its headless CLI reports 0.16.5. Locate existing settings and credential files without printing secret values.
- [x] (2026-09-13) Inspect two community ACP adapters and select the current TypeScript adapter as the compatibility-probe candidate.
- [x] (2026-09-13) Write this implementation plan, including target scope, compatibility gates, migration handling, and live tmux acceptance.
- [x] (2026-09-13) Prove authenticated adapter compatibility with GLM-5.3-Flash, model/effort discovery, Coding Plan quota, and approved/denied guardian writes.
- [x] (2026-09-13) Add ZCode harness metadata, pinned managed adapter/backend installation, target environment handling, profile staging, and breaking schema migration 32.
- [x] (2026-09-13) Integrate quota reporting, skills, ordinary sessions, and the existing sub-agent control surfaces.
- [x] (2026-09-13) Complete the isolated tmux campaign with Claude Sonnet and GPT Luna parents, including prompt-border navigation and daemon reattachment.
- [ ] Complete full Rust test/clippy gates, install the committed build, and smoke the configured live profile.

## Surprises & Discoveries


The executable `/usr/bin/zcode` resolves to the Electron desktop application `/opt/ZCode/zcode`. Its version/help attempts failed in the restricted sandbox. The actual headless entry point is `/opt/ZCode/resources/glm/zcode.cjs`, and `node /opt/ZCode/resources/glm/zcode.cjs --version` returns `0.16.5`. Set the backend path explicitly; invoking a desktop launcher discovered on PATH is not a valid headless launch strategy.

The user's configuration is `/home/jonathan/.config/mjolnir/config.toml`; the installed CLI is `/home/jonathan/.cargo/bin/mj`, version 2.6.4. Existing ZCode state includes `.zcode/v2/config.json`, `.zcode/v2/credentials.json`, and `.zcode/cli/db/db.sqlite` under the user's home. At inspection, `.zcode/cli/config.json` did not exist. The desktop config contains GLM-5.3 and GLM-5.3-Flash model metadata. Credential-file presence does not prove that authentication, quota, or headless model requests work.

The candidate `william0wang/zcode-acp` source at commit `ad31b663eab446c2478f573498e5f7e3025d4c20` declares npm package `zcode-acp-server` version 0.37.1, Node >=22, ACP protocol 1, and support for ZCode CLI >=0.16.0. Registry publication and artifact integrity remain to be verified. Its source was inspected under `/tmp/mj-zcode-acp-plan`. The older Rust adapter `jpalmae/zcode-acp`, inspected under `/tmp/mj-zcode-acp-review` at `42fe149d4b501469343c01f23ba3801832306d53`, targets an older backend and offers less relevant recent compatibility evidence.

The npm registry publishes adapter 0.37.1 at `https://registry.npmjs.org/zcode-acp-server/-/zcode-acp-server-0.37.1.tgz` with integrity `sha512-D8Ejw2RzkaxKtpO5ZJR/h41fRdpIjTKxHlPMCiO5hhCOLCe2+1k/U+fGWjBTp+IuKWDL8weEr/6VToyTWiV0DA==`. The host provides Node 24.15.0, satisfying the adapter and backend requirement for Node's built-in SQLite module.

The TypeScript adapter reads credentials, skills, plugins, and several state paths directly from HOME/.zcode. It does not currently reference `ZCODE_DATA_BASE_DIR`. The ZCode bundle contains that environment variable, but its exact path semantics and encrypted credential portability need a probe. The adapter also initially creates native sessions with `mode: "yolo"`, and spawns the backend as a detached process group with its own watchdog. These are concrete isolation, approval, and teardown concerns to resolve before production integration.

The validated adapter required a deterministic compatibility patch. The patch makes all adapter-owned paths honor `ZCODE_HOME`, selects only `builtin:zai-coding-plan`, applies the requested model after native session creation, establishes `build` before the first prompt, forwards native-tool suppression, and keeps the backend under the adapter watchdog. The managed runtime downloads the official ZCode 3.11.2 AppImage, verifies its architecture-specific SHA-256, and extracts only the headless `resources/glm` runtime; Mjolnir never launches the Electron application.

Ordinary ACP tool permissions were previously surfaced only for Muse. ZCode therefore received an immediate cancelled response even though its backend was correctly in `build` mode. Generalizing that existing structured elicitation path to Muse and ZCode produced a real `allow_once` decision and a real `deny` decision; the accepted file existed with exact content and the denied file remained absent.

The `disallowedTools` implementation was initially present only at sibling `../codex-acp` commit `e72eb6d`; merely sending the metadata to npm 1.11.3 did not suppress native collaboration after resume, and GPT Luna invoked native `sendInput`. After the user authorized publication, the exact change passed the adapter's unit/type/build gates and packed-artifact guardian/yolo live tests, then shipped as `@brokkai/codex-acp` 1.11.4 through its trusted GitHub workflow. Mjolnir pins that published artifact directly. A post-restart GPT Luna transcript contained only `mcp.mj-subagents.*` delegation tools and completed child follow-up.

ZCode stores all native conversations in one live SQLite database and offers no selected-session export in this backend generation. Copying that database or its WAL into a checkpoint would leak unrelated desktop conversations and can be inconsistent. This delivery therefore supports daemon/worker reattachment against the original profile store but intentionally advertises native checkpoint recovery as unavailable for ZCode. Relocatable ZCode checkpoint/import support is deferred until a selected-session export exists.

Mjolnir currently has six closed harness enum values. Its database revision is 31, with CHECK constraints and stored harness enum values. Adding ZCode is breaking for older readers and writers even though existing rows can be preserved. The prior sub-agent tmux campaign succeeded on local bare targets; its container campaign was blocked by host runtime prerequisites. Recheck target capability during implementation instead of assuming that historical limitation persists.

## Decision Log


Decision: use stable harness ID `zcode` and display label `ZCode`, with GLM models advertised by the adapter. Rationale: harness grouping identifies the actual coding agent; it must not merge ZCode into Claude merely because a provider endpoint accepts Anthropic-shaped requests. Date/author: 2026-09-13, Codex.

Decision: use the current TypeScript adapter as the first compatibility candidate and pin a verified artifact after the probe. Rationale: it covers the installed backend generation, model/effort controls, questions, and resume. Do not build a replacement adapter inside Mjolnir before establishing the actual gaps. Date/author: 2026-09-13, Codex.

Decision: keep Claude and Codex as the only Mjolnir delegation parents. ZCode is an ordinary selectable harness and an eligible child. Rationale: this preserves the user's explicit parent scope. ZCode children must not recursively create invisible native children; verify native `Agent`/`Task` suppression through the backend's advertised `--disallowedTools` option before enabling that child role. Date/author: 2026-09-13, Codex.

Decision: include full target support in the first delivery: managed adapter and backend installation on bare targets plus the default container image. Rationale: the user selected parity with other Mjolnir harnesses rather than an installed-target-only increment. Obtain official versioned ZCode artifacts and checksums for each supported platform; do not redistribute or fetch an unversioned desktop bundle. Date/author: 2026-09-13, user and Codex.

Decision: use the existing `builtin:zai-coding-plan` provider and its GLM Coding Plan allowance. Do not select the simultaneously enabled `builtin:zai-start-plan` provider. Rationale: the adapter documents that Start Plan requires browser-solved Aliyun CAPTCHA headers and cannot operate headlessly, while Coding Plan uses an API key and is supported. Date/author: 2026-09-13, user and Codex.

Decision: use isolated stores for all implementation tests. Plan a new breaking migration at the next unused revision (32 if 31 is still current), including its compatibility floor in the same transaction. Rationale: AGENTS.md explicitly says a feature request does not authorize an incompatible upgrade of the live store. Prepare the complete validated upgrade and backup procedure before seeking that final authorization. Date/author: 2026-09-13, Codex.

Decision: do not archive ZCode's shared native SQLite database. Rationale: a full database snapshot would expose unrelated conversations and a live file copy is not transactionally safe. Same-store restart/resume is supported; selected-session checkpoint recovery and native import remain explicitly unavailable. Date/author: 2026-09-13, Codex.

Decision: publish Codex ACP `disallowedTools` as `@brokkai/codex-acp` 1.11.4 and pin that registry version directly, replacing the temporary generated patch over 1.11.3. Rationale: the user explicitly authorized publication, and a normal immutable package pin keeps bare installs, ambient fallback, and the container image aligned without commit-specific patch machinery. Date/author: 2026-09-13, user and Codex.

## Outcomes & Retrospective


ZCode now runs as a native Mjolnir harness through `zcode-acp-server` 0.37.1 and the headless ZCode 3.11.2/CLI 0.16.5 backend. The isolated store at `/tmp/mj-zcode-live-20260913` proved direct GLM-5.3-Flash inference (`c57132ed7973002429f9407dbcef54e3`), live Coding Plan quota, approved and denied guardian writes (`e4ab5a8c0906a78b1fcb26b831cf584b`), Claude Sonnet parenting (`3091f770ec5a7a7b0479f6b181346dea`), and GPT Luna parenting plus post-restart follow-up (`21852a46a85efd48f3e8f3673835313b`). Parent and child database rows had the same `localhost` target and exact project worktree.

The tmux interface showed `Sub-agents (1)`, opened a virtual workspace whose Workspaces header was `codex-zcode-parent  X`, displayed and accepted interaction with the ZCode child, and returned to the parent workspace through the X. Host Podman remains unavailable because `/run/user/1000` does not exist, so the disposable-container runtime smoke could not run; container parity is covered by pinned assets and build-time checks. Native selected-session checkpoint/import remains the explicit limitation described above.

## Context and Orientation


An ACP adapter translates Mjolnir's Agent Client Protocol requests into the coding agent's native protocol. Mjolnir's controller owns sessions and targets; the target-side worker owns the adapter process and relays its updates. A profile is one harness kind plus its source settings, credentials, and environment. Profile staging copies selected inputs into a worker-private home. A checkpoint is the saved project and selected native conversation state used for recovery.

`mj-core/src/config.rs` defines `HarnessKind`, home mapping, names, policy capabilities, and configuration. `mj-core/src/harness_runtime.rs` holds exact managed adapter pins. `mj-worker/src/worker_runtime/harness.rs` owns installation, validation, leases, and cleanup; `mj-controller/src/controller/worker_binary.rs` builds launches and stages allowlisted profile files. `mj-core/src/credentials.rs`, `mj-controller/src/setup.rs`, and `mj-controller/src/doctor.rs` handle authentication/discovery/diagnostics. `mj-core/src/skills.rs` decides native skill locations.

`mj-worker/src/acp.rs` and `mj-core/src/acp/surface.rs` translate ACP selectors, permissions, and events. `mj-controller/src/quota.rs` refreshes profile quota in background tasks. `mj-client/src/quota.rs` holds shared display data. `mj-controller/src/server_runtime/api.rs` handles delegation tools: it first filters eligible profiles, groups them by harness, orders each group by remaining quota descending, then returns one concrete profile per harness. Known quota sorts ahead of unknown quota; ties use profile ID. For multiple quota windows, selection uses the minimum remaining percentage. Preserve this rule for ZCode.

`mj-controller/src/controller/subagents.rs` implements child creation on the parent's borrowed target. `mj-worker/src/checkpoint.rs`, `mj-controller/src/controller/checkpoint.rs`, and the controller resume/move modules implement durability. `mj-controller/src/database.rs` and `database/schema.rs` implement schema migration. The existing terminal and web session surfaces should render ZCode through these shared contracts; no new sub-agent workspace UI is needed.

## Plan of Work


### Milestone 1: prove the native adapter boundary


Build an isolated protocol probe under `tests/e2e/` using the real adapter and Mjolnir's ACP client, following the Muse fixture pattern. Verify npm publication for the inspected version, resolve its exact tarball/integrity, and inspect installation scripts before execution. Launch the adapter's explicit stdio entry point (`node <installation>/dist/index.js` or its verified `server` subcommand), force its runtime to Node, and leave its optional remote hub disabled. Set `ZCODE_BIN` to the verified backend bundle path and use a Node version with the required SQLite support. Never infer compatibility from a successful `initialize` alone.

Before using credentials, run two synthetic profiles to prove complete separation of config, credential reads, model registry, skills, native SQLite databases, and adapter history indexes. Define `HarnessProfile.home` as the actual `.zcode` directory. Add one shared path resolver and apply the backend's verified data-root mapping. Do not redirect HOME for the whole agent environment, because child shells must retain their normal filesystem and Git behavior. If adapter paths ignore the data-root override, fix those paths in the adapter source and exercise the fix with tests. Prepare any upstream fix as a reviewable patch and record the resulting artifact pin; remote PR creation/publication requires separate explicit instruction.

Stage only the existing active ZCode provider and necessary auth material for the initial live probe. Preserve native provider identity and endpoint, particularly the distinction between a desktop ZCode allowance and a generic GLM Coding Plan/API balance. Do not synthesize a standalone API configuration from assumptions or copy unrelated providers' secrets. Test whether encrypted credentials survive staging; retain actionable login errors if they do not. Verify an authenticated, minimal GLM-5.3-Flash reply, actual shell/file execution in a disposable project, advertised model/effort choices, cancellation, and resume.

Prove configured approvals with a denied file write and an approved file write. The adapter's initial yolo setting must not allow work before the controller establishes the requested policy, including lazy native-session creation on first prompt. If necessary, correct the adapter's initial mode handling. Prove native child tools can be denied without disabling ordinary tools. Kill the ACP adapter deliberately and assert that its detached backend, watchdog, and tools stop within a bound. Promotion requires all these contracts; record and repair source defects before proceeding.

### Milestone 2: add a durable selectable harness


Add `HarnessKind::Zcode` to core metadata and all exhaustive dispatch sites. Add source-home discovery, explicit backend path resolution on the target, managed adapter pin/package lock, and cache validation using the existing atomic installation and lease machinery. Validate the backend version and required adjacent bundle assets; do not copy only `zcode.cjs` if the backend references sibling packages. Persist a backend compatibility fingerprint with profile discovery so desktop upgrades invalidate cached capabilities. Missing runtime prerequisites must fail in preflight before a child is reported running. Unsupported target arrangements must fail before provisioning or source-session teardown.

Implement profile staging with explicit allowlists for necessary v2 provider configuration, credential material, CLI settings, project instruction files, and native skills. Exclude desktop task history, log files, certificates, unrelated provider credentials, and native database copies from initial profile staging. Keep credential refresh updates field-scoped so syncing login cannot overwrite worker model choices or unrelated settings. Login must target the profile's native CLI entry point and report a browser/device challenge through the existing workflow. Do not claim guardian support until milestone 1 proves it; the final supported policy must preserve parent target approvals on local bare sessions.

Add the next schema migration, explicitly classified breaking beside its definition. Widen every affected harness constraint, preserve rows, indexes, foreign keys, projections, workspace state, and existing parent/child relations, then advance the revision and minimum compatible revision atomically. Do not modify earlier migrations. Account for serialized harness values in configuration, archives, and worker wire contracts; older clients must reject unsupported state with useful diagnostics. Add tests for upgrading a populated revision-31 fixture and for rejection by an older reader/writer.

### Milestone 3: integrate quota and native conversation recovery


Add a controller-side ZCode quota reader using the shared credential/provider resolver established in milestone 1. The inspected adapter uses `/api/monitor/usage/quota/limit` for standard GLM plans, with response windows carrying used/remaining counts, percentages, and reset times. This is prior art, not evidence that the user's desktop allowance uses the same quota. Probe the actual selected provider, map only verified inference limits to `ProfileQuota`, and keep auxiliary MCP/search quotas distinct. Report unavailable or authentication failure honestly. Do not fabricate 100% for an unknown subscription. Test per-harness selection with two ZCode profiles, including unequal quota, ties, unknown reports, and disabled/ineligible candidates.

Use ordinary ACP model and effort discovery, streaming, questions, usage, and cancellation. Exercise image input only when advertised and report unsupported capabilities explicitly. Parent delegation uses the existing MCP endpoint and child lifecycle without a special GLM path. Native delegation suppression for children must survive new/load/resume, while parent tools continue to be injected only into Claude and Codex.

Inspect ZCode's SQLite persistence and native session identity before implementing checkpoint capture. Save a consistent snapshot using supported export facilities or SQLite backup with selected-session filtering, never a raw copy of a live database/WAL or all desktop conversations. Test restore with a nonce remembered only in the native conversation and prove no unrelated session or credential leaks into the archive. Preserve ordinary daemon reconnect and worker restart. If native relocation is unsupported, reject moves to a different project directory before stopping the source and document that limitation. External desktop-session import and ZCode-as-parent delegation are outside this first delivery.

### Milestone 4: validate and prepare the user's installation


Run the automated checks below, then launch the built Mjolnir in an isolated tmux campaign. Use the user's existing account only after the authenticated adapter probe succeeds, and use its actual advertised GLM-5.3-Flash selector. Launch from Claude Sonnet and GPT Luna parents, verifying each parent's installed suppression/injected MCP path. Grok Build remains the workhorse for repeated general orchestration checks; ZCode is used where testing its own behavior is essential.

Once isolated tests pass, prepare installation of the built controller and worker, a minimal additive ZCode profile entry, and an eligibility update preserving the user's existing selections. Show the exact affected configuration paths, tested artifacts, current/next schema revisions, backup method, daemon-stop requirements, and rollback constraints. The repository forbids an incompatible live-store upgrade on feature authorization alone, so request that upgrade authorization only at this concrete deployment boundary. With approval, take a consistent backup, install both executables, migrate through the normal workflow, add the profile, and repeat the minimal real sub-agent smoke on the user's install. Never restart unrelated active sessions without accounting for their continuation.

## Concrete Steps


Work from `/home/jonathan/Projects/hel`, use `apply_patch` for edits, and keep agent-owned artifacts in `.agents/`. Read current `AGENTS.md` before implementation. Preserve untracked `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3`. Commit each coherent validated change directly on the current branch; do not push or publish an adapter release without explicit authorization.

Initial read-only runtime checks are:

    node --version
    node /opt/ZCode/resources/glm/zcode.cjs --version
    node /opt/ZCode/resources/glm/zcode.cjs --help

Expected backend version at planning time is `0.16.5`. Reconfirm at execution. Record new tests and exact reproduction commands as milestones progress. The protocol fixture should become a colocated ignored live test such as `zcode_live_session_round_trip`, enabled by explicit adapter/backend/private-profile environment paths. This test name is planned, not an existing command.

Required Rust validation, with every cargo test outside the restricted sandbox:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo build -p brokk-mj-worker --bin mj-worker
    git diff --check

Run adapter tests/type checks for any adapter patches and relevant web tests if product web code changes. Update license inputs and generated notices using the repository's existing license workflow when introducing packaged dependencies. Do not redirect Cargo build storage into `/tmp`.

Create a unique tmux server with an isolated config root, short isolated data root (to avoid Unix socket path limits), and a disposable Git project. Record actual generated paths in this plan. Example server commands, replacing the seed with the recorded unique value:

    tmux -L mj-zcode-<seed> -f /dev/null new-session -d -s campaign -n tui -x 160 -y 48
    tmux -L mj-zcode-<seed> new-window -d -t campaign -n daemon
    tmux -L mj-zcode-<seed> new-window -d -t campaign -n evidence

Launch built executables with explicit `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. Keep redacted captures and test evidence beneath `target/zcode-live-<seed>/`; do not commit secrets or raw credential files. Use `tmux capture-pane -p -e -S -2000` to observe the terminal alongside API transcripts and process-tree evidence. Persist environment files privately with permissions restricted to the user, and avoid printing tokens in commands or logs.

## Validation and Acceptance


Synthetic tests must prove profile separation, selected credential/provider handling, discovery without an inference prompt, actual policy selection before first work, correct questions and permission denial, model/effort persistence, quota parsing/selection, and old-database migration. Drive protocol streams and tool output with more than 64 KiB and verify exact content. Test adapter/backend EOF, crash, cancellation, and process cleanup with bounded waits. Fixture tests should fail on the corresponding old behavior rather than restating enum lists.

The live campaign succeeds when both a Claude parent and a Codex parent call Mjolnir's `list_profiles`, see one eligible ZCode entry, spawn a GLM-5.3-Flash child, and receive its terminal result. Have each child read a parent-created random sentinel and write its own marker in the shared project; verify equal target and directory identities plus different worker/profile roots. Send follow-up input through the parent and directly through child chat. Test a controlled long-running child with interrupt and close, and prove the parent still reads the shared files afterward. Run two ZCode children concurrently to test native database and profile independence.

Open the prompt-border Sub-agents entry with both keyboard and mouse. Confirm the parent's session name appears at the top of the Workspaces list with the existing dialog-style X, observe ZCode output and attention state, answer a question or approval, close the virtual workspace while work continues, and reopen it. Verify UI responsiveness throughout. Restart the isolated daemon, reattach, and confirm the existing child and native conversation survive; perform checkpoint/restore with nonce recall. Closing the family must terminate all owning processes before removing private files and must preserve the parent project's user-owned data.

Live local bare and disposable-container behavior are required. Add a raw SSH smoke when an authorized target is available. Managed installation and container packaging must use verified official artifacts; any unavailable architecture remains incomplete work rather than being silently omitted. Record all executed models, versions, session IDs, target kinds, failures, and coverage limits. The user's normal-install success is only claimed after the final deployment smoke, not from isolated tests alone.

## Idempotence and Recovery


Use private disposable profile and database roots for probes. Never log credentials or overwrite the user's desktop provider selection. Installation stages into a temporary directory and publishes only after validation; use existing leases and process supervision. Failed launches remain visible and clean up only their private state. Stop backend process groups, then watchdogs/adapter as appropriate, before deleting test directories. Retain redacted evidence.

Migration tests are transactional and use copied fixtures. Before an authorized live upgrade, create a consistent database backup and retain the exact prior config and binaries. An older binary cannot read a store after ZCode values are admitted; rollback restores the coordinated pre-upgrade snapshot after stopping new writers and loses post-backup work unless separately exported. Do not silently downgrade or delete new records to make an old binary start.

## Artifacts and Notes


The inspected TypeScript adapter source identifies `zcode-acp-server` 0.37.1 at commit `ad31b663eab446c2478f573498e5f7e3025d4c20`. Relevant files are `src/backend/resolve.ts`, `backend/credentials.ts`, `backend/client.ts`, `utils.ts`, `handlers/session.ts`, `config/options.ts`, and `quota/client.ts`. Source inspection established direct HOME-based reads, initial yolo creation, forwarded client MCP servers, and detached backend ownership. None establishes live Mjolnir compatibility. Pin the artifact actually validated and record any corrective adapter changes here.

## Interfaces and Dependencies


Add `HarnessKind::Zcode` in the existing core crate. Centralize ZCode profile-root interpretation in one helper used by configuration, login, staging, discovery, quota, and recovery. Put ZCode authentication/provider parsing in a focused module shared by those consumers. Add `mj-controller/src/zcode_usage.rs` for quota querying if substantive provider-specific logic is needed; reuse `ProfileQuota` and `QuotaWindow`. Use the existing Node/npm managed installer for the ACP package with a checked-in lock under `mj-worker/assets/harnesses/zcode/` and shared pin metadata in `mj-core/src/harness_runtime.rs`.

Keep all target filesystem work, provider HTTP requests, installation, migrations, and process operations off terminal/web event loops in supervised tasks. Reuse shared subprocess helpers and drain output concurrently with input. Do not create a new workspace crate for this integration. Any new native checkpoint format must be versioned and tested independently from live credentials.

Revision 2026-09-13: initial researched plan. Chose a compatibility-first integration of the recent TypeScript adapter, made raw-target/backend prerequisites explicit, retained Claude/Codex-only parenting, and separated isolated acceptance from the breaking live-store deployment boundary.

Revision 2026-09-13 during implementation: record the user's decisions for full target support and the existing GLM Coding Plan allowance, plus the verified npm adapter metadata and local Node/backend versions.

Revision 2026-09-13 after live acceptance: record the pinned ZCode compatibility patch, guardian elicitation fix, published Codex ACP 1.11.4 consumption, concrete tmux/session evidence, daemon reattachment, Podman host blocker, and the deliberate selected-session checkpoint limitation.
