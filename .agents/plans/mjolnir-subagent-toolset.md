# Build Mjolnir-managed sub-agents

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current as implementation proceeds. Maintain it in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change, a Claude or Codex session can delegate focused work through Mjolnir-owned MCP tools. Each delegated agent has an independent conversation and profile but runs in the parent's existing target and working directory, so the agents see the same files without Mjolnir provisioning a second container or checkout. The parent can choose among configured child profiles and models, observe completion, send follow-up instructions, wait, interrupt, and close children.

The terminal and web interfaces show a `Sub-agents` entry in the prompt's lower border. Opening it enters a virtual workspace containing the parent session's children. While the view is open, the parent's display name appears first in the Workspaces list with an `X`; activating the `X` returns to the normal parent workspace and leaves children running. Closing or stopping a parent with active children shows a warning and, once confirmed, stops the children before target cleanup.

## Progress

- [x] (2026-09-13) Grounded the design in configuration, session records, durable relay, reviewer profile staging, native HTTP API, background-task controls, and both workspace surfaces.
- [x] (2026-09-13) Verified the pinned Claude adapter accepts `disallowedTools`, the adjacent Codex adapter implements `_meta.codex.options.disallowedTools`, and ACP exposes session forking; selected a portable parent-written handoff instead of native forking.
- [x] (2026-09-13) Added configuration v9, durable `subagent_sessions` ownership in schema revision 31, and child lifecycle that borrows the parent's exact target without owning its cleanup.
- [x] (2026-09-13) Added the target-side JSON-lines MCP bridge, relay protocol v12 queue, parent-only orchestration, profile/model discovery, source excerpts, waits, follow-ups, interruption, close, and completion delivery.
- [x] (2026-09-13) Added TUI and web prompt-border controls, hidden ordinary child rows, virtual-family workspaces, parent-name headers, dialog-style close controls, ordinary child chat interaction, and family-stop warnings.
- [x] (2026-09-13) Completed automated tests and the local-bare tmux/provider campaign. The disposable-container repetition is host-blocked: rootless Podman has neither a writable run root nor `crun`, and Docker is not installed.

## Surprises & Discoveries

- Observation: Mjolnir already has prompt-free persistent profile/model/effort discovery, so sub-agent settings and MCP discovery do not need another provider probe.
  Evidence: `mj-controller/src/controller/profile_config.rs` and `GET /api/v1/profiles/{id}/config` feed `mj models` and review settings.
- Observation: the reviewer implementation already stages another profile inside a primary session's target with a private harness home.
  Evidence: `mj-controller/src/controller/reviewer.rs::stage_reviewer_profile_controlled` derives the live backend and worker root from the parent and supports both ACP-delivered and harness-profile-delivered MCP servers.
- Observation: ordinary sessions currently derive their worker root from their session ID and target locator, while a session record assumes ownership of its target.
  Evidence: `Controller::worker_placement` calls `targets::worker_root(&backend, session_id)`; `SessionRecord` has no parent or target-owner field.
- Observation: the working tree already contained unrelated untracked files before implementation.
  Evidence: `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3` were present and must remain unstaged.
- Observation: child workers perform the ordinary readiness sync even though only parent workers own a sub-agent endpoint. The Unix relay originally rejected that sync, causing every child to fail before ACP startup.
  Evidence: the first two tmux matrices produced `this session has no Mjolnir sub-agent tools`; child workers now answer an empty request list while still refusing completion writes without an endpoint.
- Observation: an idle materialized projection exists before a new child's first prompt is submitted, and failed follow-up configuration may leave it idle. Treating idle alone as completion made `wait_agents` return early and mislabeled setup failures.
  Evidence: the campaign returned `completed` with `output: null` before any child prompt event. Status now combines lifecycle, asynchronous start status, and materialized execution, and a regression covers failed startup.
- Observation: the authenticated Muse child home advertises only `muse-spark-1.3-contributor`, while source-home discovery also advertises the non-contributor variant. Muse reports an empty current model during source-home discovery and rejects setting its first advertised default with `invalid_target`.
  Evidence: both final parent runs used the child catalogue's selector and returned `MUSE_OK parent=initial shared=unmodified`. The worker now recognizes Muse's empty-current/first-choice shape as an implicit default.
- Observation: the original isolated data-root path exceeded the Unix socket path limit.
  Evidence: the live campaign kept configuration and artifacts under `target/subagent-live-20260913-1` but used `/tmp/mjsa-data` for its disposable database and worker roots.

## Decision Log

- Decision: only Claude and Codex sessions receive the Mjolnir sub-agent MCP tools in v1; any enabled eligible profile may serve as a child, but children cannot spawn descendants.
  Rationale: these are the parent harnesses trained for delegation and both accept native-tool suppression. Parent-only delegation bounds the ownership model.
  Date/Author: 2026-09-13, user and Codex.
- Decision: `[subagents]` stores enabled state, additional eligible profile IDs, and a per-parent concurrency limit of six by default. A parent's own enabled profile is eligible independently of the list.
  Rationale: checkboxes remain understandable, existing profile discovery supplies model choices, and the parent works without initial setup.
  Date/Author: 2026-09-13, user and Codex.
- Decision: suppress native delegation with `disallowedTools` and inject Mjolnir's tools through MCP. Codex receives `_meta.codex.options.disallowedTools`; Claude receives `_meta.claudeCode.options.disallowedTools`.
  Rationale: one authoritative orchestrator prevents invisible native children and keeps every child in Mjolnir's workspace.
  Date/Author: 2026-09-13, user.
- Decision: use fresh child conversations with a parent-written handoff and optional one-based inclusive `(file, start, end)` excerpts, capped at 256 KiB combined. Do not use native ACP forking in v1.
  Rationale: native forking cannot preserve context across harnesses and would create two different semantic paths. Explicit excerpts avoid wasting model output on copied code.
  Date/Author: 2026-09-13, user and Codex.
- Decision: a child is a normal durable Mjolnir session whose target locator and working directory are borrowed from its parent, while target ownership remains exclusively with the parent.
  Rationale: normal sessions already provide transcripts, prompts, elicitations, reconnect recovery, model configuration, and both chat surfaces. Separate worker roots give children independent relays and harness homes without separate target provisioning.
  Date/Author: 2026-09-13, Codex.
- Decision: Grok Build is the live-test workhorse. Also test DeepSeek Flash, Muse Spark, Claude Sonnet, and GPT Luna as children of both Claude and Codex parents. Enable Muse only in isolated test configuration.
  Rationale: Grok has the largest available free-token budget while the full matrix proves cross-profile and same-provider behavior.
  Date/Author: 2026-09-13, user.
- Decision: store parent/child ownership in a normalized relation table instead of adding nullable ownership fields to every `SessionRecord`.
  Rationale: ordinary session records remain stable while foreign keys, unique request identity, and the relation payload provide one durable source of family ownership. Cleanup selects borrowed behavior through the relation and target locator.
  Date/Author: 2026-09-13, Codex.
- Decision: schema revision 31 is a breaking reader/writer floor even though it adds a table.
  Rationale: older writers do not understand family ownership and could stop or clean up a child as if it owned the parent's target. The minimum compatible revision therefore advances with the schema version.
  Date/Author: 2026-09-13, Codex.

## Outcomes & Retrospective

Implementation is complete. Claude and Codex parents receive Mjolnir's MCP tools and unconditional native-delegation suppression; all other harnesses and every child omit the MCP endpoint. Eligible profiles are configured by checkbox with the parent profile implicitly eligible and a default concurrency cap of six. Children retain ordinary durable sessions and conversations while borrowing the parent's target and filesystem.

The tmux campaign used server `mj-subagents-20260913-1`, artifacts under `target/subagent-live-20260913-1`, database/worker roots under `/tmp/mjsa-data`, and project `/tmp/mj-subagent-live-20260913-project`. Both parent harnesses ran Grok Build, DeepSeek Flash, Muse Spark, Claude Sonnet, and GPT Luna. Observed replies included `FOLLOWUP_OK` from parent-to-Grok interaction and each provider's marker plus `parent=initial` / `shared=unmodified`. A daemon restart preserved existing parent and child workers, and the TUI capture showed the named parent virtual workspace with its `X` close control. Muse was enabled only in the isolated copied configuration.

Automated validation passed: formatting and `git diff --check`; Clippy on all default targets; the controller library (1181 passed, 5 ignored), worker library (401 passed, 8 ignored), full TUI suite, web unit tests, and the Playwright virtual-workspace behavior. One full default `cargo test` run hit an unrelated concurrent managed-Grok-cache lease test; that exact test passed in isolation and the complete worker library then passed. `cargo test --workspace` additionally tries the intentionally non-default desktop crate and could not build because this host lacks GTK/WebKit pkg-config libraries.

The disposable-container repetition could not run on this host. `podman info` fails because `/run/user/1000` is unavailable and the configured `crun` runtime is absent even with an isolated `XDG_RUNTIME_DIR`; Docker is not installed. No host configuration was changed to work around that environmental limitation.

## Context and Orientation

`mj-core/src/config.rs` owns the versioned configuration and profile definitions. `mj-core/src/state.rs` owns durable session records. `mj-controller/src/database.rs` and its `database/` modules project session and API state into SQLite. `mj-controller/src/controller/` creates, resumes, stops, and deletes sessions and stages profile homes on targets. `mj-controller/src/server/api.rs` and `mj-controller/src/server_runtime/api.rs` expose the authenticated Web API and implement it against the running daemon.

Each ordinary session has a target-side `mj-worker` process with a private root. `mj-core/src/relay/` defines the durable controller/worker protocol; `mj-worker/src/relay.rs` owns its state machine, and `mj-client/src/session.rs` exposes a controller-side session handle. The worker opens a harness through ACP in `mj-worker/src/acp.rs` and can inject MCP servers in ACP requests or staged Claude/Kimi profile files.

`mj-chat/src/chat.rs` and `mj-chat/src/chat/active.rs` implement the shared session chat and its prompt-border background-task control. `mj-tui/src/workspaces.rs` renders the terminal workspace list. `mj-controller/src/web/viewer.js`, `viewer.html`, and `viewer.css` implement the web surface.

A virtual workspace is transient navigation state representing one parent and its children; it is not a `WorkspaceRecord` and never participates in workspace deletion or movement. A borrowed target is a child's reference to the exact live target locator and working directory owned by its parent. Cleanup removes the child's worker root and staged profile but cannot remove the shared container, instance, checkout, or project directory.

## Plan of Work

First add `SubagentConfig` to the versioned configuration with `enabled = true`, `max_concurrent = 6`, and a set of additional eligible profile IDs. Validate IDs against configured profiles. Add settings editors to both surfaces using existing checkbox/list controls and asynchronous profile discovery. Advance the config version and preserve older defaults.

Add a normalized durable parent/child relation. The parent must exist, be active, use Claude or Codex, and have a live target. Child creation copies the parent's workspace ID, bundle/project identity, target locator, working-directory identity, mounts, and execution policy, but does not provision or own them. It stages the selected profile into a child-private worker root on that same target and uses the ordinary session manager thereafter. Stop/close recursively stops active children first; cleanup consults ownership before removing target resources. Parent moves are refused while children are active. Because old binaries cannot preserve this ownership contract, advance the SQLite migration revision and minimum compatible revision in one transaction and test with isolated stores.

Create shared request/response types for agent identities, status, profile choices, source ranges, spawn requests, messages, waits, interruption, and closure. Extend the authenticated API beneath `/api/v1/sessions/{parent}/subagents` and publish family changes through the existing durable event stream. Every route verifies parent ownership. Persist family metadata and delivery identities so reconnects and daemon restarts cannot duplicate results.

Add an `mj subagent-mcp` worker mode. It serves a small JSON-lines MCP server and connects to a Unix socket inside the parent worker root. The worker owns the other end and forwards typed requests over the durable relay to the controller. Calls receive bounded responses without serializing all operations behind a wait. A wait is registered and completed later; it does not block worker relay or other MCP requests. No bearer token or controller network endpoint enters the target.

The controller orchestration service consumes those requests, applies authorization and concurrency admission atomically, creates borrowed-target sessions, installs the hidden context, starts the child prompt, observes child state, and answers or emits completion. `spawn_agent` accepts task name, instructions, optional profile/model/effort, optional relative working directory, parent-written handoff, and file ranges. Missing selectors inherit the parent's current selections when the profile matches; otherwise they use the selected profile defaults. File excerpts are read through the shared target subprocess helpers, reject traversal/symlink escape, invalid ranges, non-text data, or aggregate context above 256 KiB, and are persisted before submission.

Expose `list_profiles`, `spawn_agent`, `list_agents`, `send_input`, `wait_agents`, `interrupt_agent`, and `close_agent`. Count preparing, queued, running, and input-blocked children against the configured maximum; completed idle children release capacity and reacquire it for follow-up turns. Deliver child completion to the parent once, steering an active turn when supported or queuing a continuation otherwise. Explicit user cancellation suspends automatic continuation until the user resumes.

Build the injected server definition beside the existing project-memory and reviewer MCP definitions. Only parent sessions receive it. Children and all non-Claude/Codex sessions omit it. Add `disallowedTools` to new, resume, and load metadata for both supported parents and children so native tools remain suppressed after recovery. Test the exact metadata against pinned Claude 0.73.0 and Codex ACP behavior.

Add a distinct Sub-agents prompt-border control beside background tasks. It shows active count and attention state and opens after completion too. In the TUI, opening sets virtual-family navigation state, places the parent display name first in the Workspaces list with an `X` hitbox, and fills the normal sessions/chat panes with children. In web, prepend the equivalent selected tab with an accessible close button. Closing restores the parent workspace/session, draft, selection, and scroll positions without stopping children. Child chat supports ordinary prompting, queues, elicitations, interruption, and close. Parent stop/close displays active child count and requires confirmation before family shutdown.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Keep this document current and commit each coherent validated milestone on `master`, staging explicit paths only. Never stage the unrelated pre-existing files.

For each Rust milestone, run focused tests outside the restricted sandbox, then:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

For web changes, also run:

    npm --prefix tests/e2e/web test:unit
    npm --prefix tests/e2e/web test

The final live campaign uses a unique tmux server and isolated roots. Record the exact generated paths in this document rather than reusing the user's normal database:

    tmux -L mj-subagents-<seed> -f /dev/null new-session -d -s campaign -n tui -x 160 -y 48
    tmux -L mj-subagents-<seed> new-window -d -t campaign -n daemon
    tmux -L mj-subagents-<seed> new-window -d -t campaign -n evidence

Launch the built `mj` from panes after sourcing an isolated environment file. Capture panes with `tmux capture-pane -p -e -S -2000` and store captures, bounded logs, discovered selector IDs, transcripts, and process trees beneath a feature-specific artifact directory under `target/`.

## Validation and Acceptance

Configuration tests prove old configuration loads with sub-agents enabled, a limit of six, and no additional profiles; invalid or disabled eligible IDs are rejected; the current parent profile remains eligible. UI tests prove checkbox edits preserve unrelated settings and model discovery never blocks rendering.

Lifecycle tests create a parent and several child records on each target category through hand-written command fakes. They prove equal target locators and working directories, distinct worker roots and harness homes, no target provisioning for children, no target cleanup by children, parent-first family ownership after replay, atomic concurrency admission, refusal of grandchildren and moves, and child-first stop before parent cleanup. Migration tests classify and prove the breaking revision with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.

MCP and API tests prove parent scoping, selector discovery, spawn defaults and validation, concurrent calls, bounded waits, queued and interrupting input, input-required responses, durable completion deduplication, restart recovery, and error propagation. Stream more than 64 KiB through the socket and excerpt path. Source tests cover one-based inclusive ranges, changed files after capture, binary input, symlink/path escape, aggregate limit, and multiple repositories.

TUI tests prove keyboard and mouse activation, active/attention counts, virtual workspace child rows, parent name updates, the dialog-style `X`, preserved drafts and scroll positions, direct child interaction, and active-child stop confirmation. Web unit and Playwright tests prove the same behavior and accessible controls.

Run a live tmux campaign with actual Claude and Codex parents. From each parent, spawn Grok Build repeatedly for parallel execution, completion, messaging, interrupt, close, detach/reattach, and restart recovery. From each parent also spawn one DeepSeek Flash, Muse Spark, Claude Sonnet, and GPT Luna child, using profile discovery's exact advertised selectors and never substituting a model. Enable Muse only in the isolated configuration. Include a task with source ranges and verify the child received the requested lines without the parent echoing them.

Repeat the live campaign on a disposable container target. Have parent and children read and modify a shared sentinel repository, prove they report the same container identity and observe each other's changes, close children, and verify the parent remains operational. Open the virtual workspace, close its `X` while children run, verify they continue, then reopen it and recover their exact states. Finally stop the parent, accept the active-child warning, and prove process groups stop before worker files and the owned target are removed.

## Idempotence and Recovery

Schema and configuration migrations are deterministic and additive at the storage level. Failed child preparation records a visible terminal state and removes only child-private files. Repeating an MCP spawn with the same request identity returns the same child. Completion delivery identities survive restart. Family shutdown is joinable and safe to repeat. Target cleanup always resolves the owner first; a missing or inconsistent owner relationship fails visibly rather than deleting shared resources.

Live tests use isolated state, disposable projects, and disposable targets. They never change normal user configuration. Tmux cleanup first stops Mjolnir and all recorded process groups, then removes only the exact generated lab directory.

## Artifacts and Notes

The adjacent `../codex-acp` development documentation states that create, resume, load, and fork accept `_meta.codex.options.disallowedTools`, and listing any supported collaboration tool removes Codex's native collaboration family. The pinned Claude adapter merges `_meta.claudeCode.options.disallowedTools` into the Claude Agent SDK options.

The current live configuration contains Claude, Codex, DeepSeek, Grok, Kimi, and a disabled Muse profile. Live validation must copy and edit that configuration into isolated roots rather than enabling Muse globally.

## Interfaces and Dependencies

Add core configuration and family types near their consumers; do not create a new crate. Extend `SessionRecord`, relay protocol, API event payloads, and `SubagentBackend` compatibly where possible. Use `Path` and `PathBuf` until protocol/render boundaries. Reuse `SessionHandle`, `Controller::worker_placement`, profile staging, `ProfileConfig`, target command helpers, prompt context installation, session config commands, and existing elicitation representations.

Keep all target scans, source reads, profile copies, process launches, and database work in supervised background tasks. Never discard task errors. Use the shared subprocess helpers and concurrently drain output while sending input. Unit tests remain in module-level `#[cfg(test)]` blocks; browser behavior belongs in `tests/e2e/web`.

Revision 2026-09-13: Created the living implementation plan from the approved conversational design, including the parent-name virtual workspace tab, dialog-style close control, required model matrix, and tmux live validation.
