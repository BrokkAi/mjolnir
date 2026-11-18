# Deliver Mjolnir's standing instructions and project memory through one compaction-proof channel

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir runs coding agents ("harnesses": Claude Code, Codex, Muse, DSH, Antigravity, Kimi Code, OpenCode and Grok Build) and gives each session standing instructions. Examples are how to use project memory (shared Markdown notes with an index file `MEMORY.md`), how to use Mjolnir's own MCP tools (sub-agents, mailboxes, review, history search), and, for sub-agent sessions, the role the session plays (child implementer, reviewer, relation-brief writer, oracle advisor). Today these reach the model through three unrelated channels: a hidden block in the first user message (project memory), the `instructions` field Mjolnir's MCP servers return when the harness connects to them, and a visible prefix on the first prompt of sub-agent sessions.

All three can vanish when the harness compacts its context, that is, when it summarises or drops old conversation to fit the context window. After that the agent forgets the project memory, how Mjolnir's tools are meant to be used, or what role it was given. The text is also scattered and partly duplicated.

After this change there is one composer that builds a session's standing instructions and one delivery owner that puts them where the harness keeps them through compaction. Harnesses that accept an ACP system-prompt append get the text there. The other three get it in their native instruction file, which each of them keeps in context through compaction. To see it working, start sessions in an isolated instance, force a compaction, and ask the agent where its project memory lives and what its role and tool rules are; it answers from the instructions.


## Progress

- [x] (2026-11-18) Parent-approved ownership correction: register new children with the session manager before committing their addressable records; queue a send before attachment. The deterministic regression, nine existing message-routing tests, manager deadline test and controller registration test pass, as does clippy on client/controller/daemon/worker. Correction committed separately as 5cdda85fa. Full default delegation seed 1171 passes after the source checkpoints (isolated instructions-routing-55d6204e).

- [x] (2026-11-18) Third-review fixes validated: unsupported automated reviewers fail visibly without advancing coverage, historical reviewer launches derive roles from reviewer_launch.role, and the delegation fixture negotiates and checks standing append instructions. Lifecycle, failed-review and role-golden regressions pass; final client/controller/daemon/worker clippy passes. Corrections committed as a0b529777. Normal dev CLI/worker rebuild and full default delegation seed 1171 pass; request formal re-review.

- [x] (2026-11-18) Second-review corrections validated: reject nested native markers and special staged files using confined reads/writes, pin OpenCode's replacing config override (including retained launches), and stop unsupported automated brief writers before native open. Instruction, native-file, confined-directory and daemon brief-failure tests pass; edited-crate clippy and final worker clippy pass. Grok freshness finding overridden by parent decision; corrections proceed to formal re-review.

- [x] (2026-11-18) Corrected first-review defects in `621276a78`: read-only task scope, native marker escaping, non-regular indexes, Kimi/Muse fixtures, daemon packet expectation and unsupported-adapter coverage. All affected tests and five-crate clippy passed. Generic hung-filesystem deadline finding overridden per parent ruling; fixes proceed to formal re-review.

- [x] (2026-11-18) Saved superseded implementation as local reference `a77d745ae`; committed this plan first at `aa5c0fa00`. The old worktree is read-only.
- [x] (2026-11-18) Verified per-session native file copies and existing durable role facts before implementing.
- [x] (2026-11-18) Implemented unified composition, standard/vendor append, native owned-section replacement, and removal of startup/clear user-memory installation; targeted tests passed.
- [x] (2026-11-18) Routed MCP/role/rubric guidance through composition and removed it from MCP initialization and assignment prefixes; targeted tests passed.

- [x] (2026-11-18) Reconnaissance, live harness traces and an instruction audit (evidence summarised in Context and Orientation; full reports are listed in Artifacts and Notes).
- [x] (2026-11-18) Design decisions taken with the user (Decision Log).
- [x] (2026-11-18) Milestone 1: composer and delivery owner, with project memory as the first section; remove the first-prompt memory block.
- [x] (2026-11-18) Milestone 2: move the MCP servers' guidance and the standing parts of sub-agent first prompts into the composer.
- [ ] Milestone 3: live acceptance on every harness in an isolated instance.


## Surprises & Discoveries

- Removing pre-harness memory I/O exposed a send_message race: the child was addressable before the session manager owned it, so the route permanently chose a turn about 30 ms before relay attachment. Default delegation seed 1171 passes on base 903cac6 and fails on the implementation before this fix. The parent approved correcting registration ownership here; the fixture's startup gate and ordering checks remain unchanged. Evidence: `.agents/docs/session-instructions-delegation-routing-evidence.md`.

- First review found that read-only oracle/brief-writer behavior was prose, not enforced: `controller/reviewer.rs` inherits the parent target's execution policy and `subagents.rs` borrows its workspace. Their first prompts now retain one shared task-scope sentence, independently of standing-role delivery.
- `ProjectMemoryStore::startup_index` previously bypassed the existing `ConfinedDir` reader. It now uses that reader, which refuses links and non-regular descriptors and opens Unix files nonblocking before checking their type. FIFO indexes therefore become an unavailable warning rather than waiting for a writer.
- Quoted native section delimiters in an index previously corrupted the next replacement. The native writer now entity-escapes only its own delimiter strings inside the managed payload; user bytes outside the section remain exact.

- Moving first-prompt prefixes also affects persisted child task summaries. Their reader now strips the shared directory-only note while retaining historical prefix support; the existing summary regression and isolated controller registration test pass.
- Native instruction replacement is mutating I/O, unlike the reused memory/login reader. A worker-owned async write lease moves into the off-runtime thread and survives cancellation until atomic replacement finishes, preventing an older canceled open from overtaking a replacement.

- Verified: all three native instruction files are session-owned copies. `stage_profile` includes `agent_instructions_file()` and `copy_profile_entry_within` follows source symlinks into copied bytes. `target_profile_home` is session-specific; `install_local_profile_stage` removes legacy profile symlinks and copies the stage. `shared_profile_login` links only the credential file. The worker resolves OpenCode's XDG parent using `home_from_environment`, and Kimi/Grok's direct homes from the explicit launch environment.
- Verified: `WorkerLaunchConfig.subagent_kind` and `reviewer_launch.role` already carry durable child/oracle/reviewer/brief-writer facts at every launch. No new persisted role field is needed.

- Observation: an earlier attempt (detached worktree `~/Projects/mjolnir-wt/memory-compaction`, commits `ef6c07081` and `cd79f2d22`, never pushed) re-sent the memory block as a user message after detecting each compaction for Kimi, OpenCode and Grok. It needed a durable relay slot, a relay snapshot revision bump, per-harness compaction recognisers and a background refresher. A later check showed all three harnesses keep their native instruction file through compaction, which makes all of that unnecessary. Reuse that worktree's system-prompt plumbing, recorded ACP fixtures and tests, and none of its re-send machinery.
  Evidence: Kimi 2.6.3 copies `AGENTS.md` into its system prompt when a session binds and supplies it separately from history on every provider request. OpenCode 2.2.7 renders its instruction baseline into a system part on every request (`session/model-request.ts:114–141`, `session/instruction-state.ts:159–168`) and advances that baseline when a compaction ends (`session/projector.ts:1118–1122`). Grok Build 1.0.89 re-inserts the initial native instruction snapshot after compaction: a marker that existed only in the staged `AGENTS.md` survived two real automatic compactions (27693→18824 and 40208→15415 tokens).
- Observation: the ACP system-prompt field is behind the `unstable_session_system_prompt` cargo feature of `agent-client-protocol-schema` 1.15.1. Enabling it adds an optional `systemPrompt` capability to `AgentCapabilities`, which changes the serialized `AgentInitialized` relay event and therefore the event digests older daemons compute (relay protocol 71). The earlier worktree solved this with a serializer on that event that clears the new field; reuse it.
- Observation: Codex accepts a different system prompt on resume but keeps answering from the original one. Muse, DSH and Antigravity take the new value on resume; Antigravity reverts to the stored original if the field is omitted, so the text must be sent on every open. Kimi and Grok snapshot the native file when the session opens; rewriting the file mid-session does not change what they see. OpenCode picks up file changes when the next compaction ends.


## Decision Log

- Decision: the session manager acknowledges a Starting registration before the daemon commits a new child relation. The parent-message route asks that owner once; Starting queues a mailbox message and a published protocol retains its compatibility decision. The unowned controller registration wrapper is available only to tests/test-support; production callers must supply the manager. Do not delay, retry or reconsider a route after a timeout.
  Rationale: an addressable child launched by this daemon cannot be classified as unmanaged. This removes the race exposed by the changed launch timing at its ownership boundary.
  Date/Author: 2026-11-18, parent agent, explicitly approved as a separate commit after base seed 1171 passed.

- Decision: Grok native sessions keep the instructions they started with when reopened, matching the accepted Codex freshness limitation. Do not add re-send machinery, replace native sessions, or warn on every resume.
  Rationale: parent accepts retention of the original snapshot; an every-resume warning provides no actionable remedy. Record this limitation in Outcomes.
  Date/Author: 2026-11-18, parent ruling after second review.
- Decision: automated brief writers and reviewers are exceptions to warn-and-open. The same delivery function rejects startup before native open when it has no supported composed-instruction channel; its error names the harness and adapter version. A brief writer reaches the existing `brief_failed` path so review continues without a brief; a reviewer reaches `role_failed`, making the review visibly incomplete without advancing the baseline.
  Rationale: warn-and-open protects a user's session. An automated writer without its brief structure can only return an unstructured report, which is worse than no brief. An automated reviewer without its rubric produces no reliable verdict.
  Date/Author: 2026-11-18, parent ruling after second review, extended to reviewers during third review.
- Decision: pin `OPENCODE_CONFIG_DIR` to the session-owned staged OpenCode home alongside the XDG pin, including retained worker launch configurations. Do not use or copy the user's override directory within Mjolnir sessions.
  Rationale: pinned OpenCode 2.2.7 `util/global.ts:83` replaces Global.Service.config with OPENCODE_CONFIG_DIR, and `config/plugin/instruction.ts:41` reads its AGENTS.md instead of the default XDG global file. The staged profile is authoritative, with the same ownership reasoning as XDG_CONFIG_HOME.
  Date/Author: 2026-11-18, parent ruling after source verification.

- Decision: retain the single `READ_ONLY_TASK_SCOPE` sentence in every oracle assignment and brief-writer packet, while all other standing role text stays composed.
  Rationale: launch inspection confirmed that read-only behavior is prose-only; an older adapter without append support must still receive the assignment's safety scope. This is task data, not a user-message fallback for standing guidance.
  Date/Author: 2026-11-18, parent ruling after first review.
- Decision: `MEMORY.md` must be a regular file. Reject non-regular indexes through the existing confined reader, warn and compose unavailable guidance. Do not introduce a read deadline for regular files on hung filesystems; require cancellation-safe session stop/close and runtime shutdown instead.
  Rationale: parent explicitly chose special-file rejection only. A hung filesystem is its own failure; speculative deadlines contradict the repository rule. The generic-stall portion of the review finding was formally overridden before committing fixes.
  Date/Author: 2026-11-18, parent ruling after first review.

- Decision: Kimi, OpenCode and Grok always use their verified session-owned native file. The other five harnesses use an advertised append shape; if neither append is advertised, open without the composed text (except automated brief writers and reviewers, as decided above) and emit a visible warning naming the harness and adapter version and explaining that updating the adapter restores delivery. Never write their native file or send a user-message fallback. An advertised append that is rejected still fails visibly.
  Rationale: legacy shared Codex homes remain supported by `subagent_acp_mcp`; a capability-only file fallback could mutate shared files. Missing capabilities in older adapters must not prevent otherwise working sessions from opening.
  Date/Author: 2026-11-18, parent agent after implementation safety check.

- Decision: derive the worker's instruction role from `reviewer_launch.role` when present, falling back to existing `subagent_kind`; retain `ContextCleared.memory` and its historical reader while new workers always produce `None`; serialize `AgentInitialized` without the feature-gated capability to preserve protocol-71 digests.
  Rationale: these existing launch facts already survive resume/move, and historical wire shapes must remain readable and hash-identical. Legacy reviewer launch files can omit subagent_kind while retaining reviewer_launch.role, so the latter is authoritative in the single derivation function (parent ruling during third review). No persisted field or relay revision changes are needed.
  Date/Author: 2026-11-18, implementation following the settled compatibility constraints.
- Decision: Claude lifecycle fixture is source-derived from the pinned 0.108.0 bridge rather than a live recording. The prepared probe remains at `/tmp/claude-session-instructions-probe.py` for the parent.
  Rationale: automatic approval review rejected the credential-backed probe under the original no-Milestone-3 constraint. Parent explicitly directed source-derived coverage and reserved live Claude checking for Milestone 3.
  Date/Author: 2026-11-18, parent agent.

- Decision: Kimi, OpenCode and Grok use their session-owned native file. For the other five, select once per session open from what the harness advertises: the standard ACP `sessionCapabilities.systemPrompt` (Claude, Codex, Muse, DSH); otherwise Antigravity's vendor capability `agentCapabilities._meta.antigravity.systemPrompt = {append: true}`, which takes a string at `_meta.antigravity.systemPrompt.append` on the open request. An older adapter advertising neither receives the visible warning described above and opens without the composed instructions.
  Rationale: each channel was verified live to keep its content through compaction. Each harness has exactly one path.
  Date/Author: 2026-11-18, user ("Persistent where offered"; "Staged file; check memory too") and parent agent.
- Decision: no compaction detection, no re-sending, no relay snapshot change.
  Rationale: every channel above survives compaction by itself; see Surprises.
  Date/Author: 2026-11-18, parent agent, following the user's conditional choice.
- Decision: what moves. The MCP servers' `instructions` guidance moves into the composer and is removed from the MCP servers (they keep tool names and descriptions). The standing parts of sub-agent first prompts (child authority and report contract, reviewer and brief-writer role, oracle role, the review rubric) move too. Text that only makes sense once stays in the first prompt: the assignment itself, the oracle's question, and the review packet (tree IDs, diff commands, changed files, briefs, declined findings).
  Rationale: user choice: "MCP: move it into the optimized delivery. first-prompt prefixes: probably also move but make sure we don't move something that genuinely applies only on first."
  Date/Author: 2026-11-18, user.
- Decision: what stays. The content Mjolnir appends to the staged `AGENTS.md`/`CLAUDE.md` today (disposable environment, container storage and attachments, the delegation policy) stays there, unchanged. Mailbox deliveries, shell output, resume rosters, handoffs, reminders and continuations stay per-event user content. Native mechanisms (Claude's own memory, goals, permission modes) stay native.
  Rationale: user: "AGENTS.md: leave it there, slightly different purpose." Per-event content is data, not standing instruction.
  Date/Author: 2026-11-18, user and parent agent.
- Decision: Claude gets the composed instructions through its system-prompt append, except the project-memory section, because Claude Code has its own native memory that already survives compaction.
  Rationale: the pinned Claude bridge 0.108.0 advertises and forwards `systemPrompt` (append maps to the Claude Code preset append).
  Date/Author: 2026-11-18, parent agent.
- Decision: an unreadable `MEMORY.md` never blocks opening a session. The memory section then says the index is unavailable and why, and the worker emits a visible warning naming the path and the error. A harness that rejects the system prompt fails the open visibly, with no retry without it.
  Rationale: never leave the user stuck, while failing visibly (AGENTS.md).
  Date/Author: 2026-11-18, parent agent.


## Outcomes & Retrospective

The third-review fixes and the parent-approved startup routing ownership correction are committed and validated. The deterministic send-before-attachment regression, all nine existing message-routing tests, manager and reviewer registration checks, unsupported-reviewer lifecycle, failed-review baseline protection and historical-role goldens pass. Client/controller/daemon/worker all-target clippy passes. A normal dev CLI/worker rebuild and the unchanged full delegation scenario (seed 1171, isolated instructions-routing-55d6204e) pass; the held-model send returns in 0.180 s, versus the pre-fix 60.373 s failure. Its test daemon is stopped. Review corrections proceed to formal re-review; no push or Milestone 3 live acceptance was performed.

Milestones 1 and 2 are implemented. Accepted freshness limits: Codex can keep its original append on reopen; Grok keeps the model-visible rule prelude captured by the original native session despite a rewritten file, so an already-used pre-upgrade native session is not backfilled with the new standing block. These channels retain the original snapshot and do not promise refreshed instructions for reused native sessions. No per-resume warning or re-send is added. First-review corrections and their passing targeted checks are recorded in `.agents/docs/session-instructions-validation.md`; the generic hung-filesystem deadline demand was declined according to the parent's explicit special-file-only ruling. Recorded/source-derived ACP lifecycle coverage, native-file replacement, five role goldens, unavailable-index and shutdown regressions, MCP contracts, isolated controller registration, task-summary compatibility and relay digest checks pass. The four edited crates pass clippy; the worker-only final follow-up also passed and is recorded in `.agents/docs/session-instructions-validation.md`. The default instance was untouched. Milestone 3 remains parent-owned and unstarted; Claude live marker acceptance and Kimi quota-limited acceptance remain pending there.


## Context and Orientation

Current implementation: `mj-worker/src/acp/instructions.rs` owns composition and delivery before `serve_session` opens a native session. It derives roles from existing durable launch facts, composes memory via `compose_startup_prompt_context`, and installs append text or a marked native-file section. `subagent_mcp` exposes guidance to that composer; the shared MCP transport no longer has an initialization-instructions field. `mj-review::lanes` exports the standing rubric/brief text and renders only task-specific packets. The paragraphs below describe the starting code and explain what was replaced.

Terms. ACP (Agent Client Protocol) is the JSON-RPC protocol Mjolnir uses to drive every harness over stdio: `initialize` (the agent replies with its capabilities), then `session/new`, `session/load` (reopen with replay) or `session/resume` (reopen without replay), then `session/prompt` per user turn, with `session/update` notifications streaming back. The worker (`mj-worker`) is the per-session process that runs the harness; its ACP code is in `mj-worker/src/acp/`. The controller (`mj-controller`) prepares and launches workers. The "staged profile" is the per-session copy of the harness's home/config directory that the controller prepares at each worker launch; the harness's native instruction file in it is named by `HarnessKind::agent_instructions_file()` (`mj-core/src/config/harness.rs`, around line 776: `CLAUDE.md` for Claude, `AGENTS.md` for the others). The controller writes Mjolnir's environment and delegation text into it through `append_staged_instructions` (`mj-worker-staging/src/worker_binary/install.rs:125`), called from `mj-controller/src/controller/worker_launch.rs:267–286`.

Project memory before implementation. `mj_core::project_memory::startup_prompt_context` (`mj-core/src/project_memory.rs`, around line 1080) builds the block: a wrapper `<mj-project-memory>`, the guidance constant `MEMORY_GUIDANCE`, the memory directory path, root lines for multi-repository bundles, and the `MEMORY.md` index (capped by existing code at 200 lines / 25 KiB). `mj-worker/src/worker_runtime/unix.rs` (around lines 2053–2090) installs it as hidden context before the harness starts, for non-Claude sessions that are not resumed; `/clear` rebuilds it in `mj-worker/src/acp/session.rs` (around 724–746) and carries it in `RuntimeEvent::ContextCleared { memory }`, applied in `mj-core/src/relay/snapshot/apply.rs` around 1278. The hidden context goes out with the first prompt (`mj-worker/src/relay/commands.rs` around 1262; `mj-worker/src/worker_runtime/unix/dispatch.rs` around 1537; `mj-worker/src/acp.rs:125–177`).

MCP guidance before implementation. `mj-worker/src/mcp_stdio.rs:129–135` returns `instructions` on MCP initialize. The texts: parent delegation and mailbox policy `SERVER_INSTRUCTIONS` and the Codex discovery suffix (`mj-worker/src/subagent_mcp.rs:16–37`), child instructions (`:39–44`, built from `HANDBACK_REPORT_RULES` in `mj-core/src/subagent.rs:1098–1120`), reviewer and brief-writer variants (`subagent_mcp.rs:442–489`), message-only and fetch-only variants (`:458–475`), review procedure (`:47–52`, `:1123–1144`), and history-search guidance (`mj-worker/src/memory_mcp/history.rs:12`, selected in `mj-worker/src/memory_mcp.rs:27`).

Sub-agent first prompts before implementation. `mj-controller/src/controller/subagents.rs`: oracle role constant at line 15 and prompt at 437–453; reviewer prefix at 432–435; child prefix at 456–459. Review lanes build their prompt from `mj-review/templates/review_rubric.md`, `templates/brief_writer.md` and the packet in `mj-review/src/lanes.rs:154–194`. The reviewer first prefix asks for one output format and the reviewer MCP text for another; whichever the review consumer actually parses is correct, and the composed text must say only that.

ACP session opening. `mj-worker/src/acp/session.rs:160–360` sends `initialize` and opens the session through `new_session_request`, `load_session_request`, `resume_session_request` in `mj-worker/src/acp/launch.rs` (around 735–772). Resume is used when the agent advertises `sessionCapabilities.resume` and the session has no native sub-agents; otherwise load. A cleared session is opened through the same path.

Earlier worktree to reuse from, read-only: `~/Projects/mjolnir-wt/memory-compaction` at `cd79f2d22` (plus a local commit of its last uncommitted work if the parent made one). Reuse: the cargo feature, the content-agnostic system-prompt request plumbing for both the standard field and the Antigravity vendor field, the `AgentInitialized` serializer and its digest test, the off-runtime file reader shared with the Claude login reader (`file_read_off_runtime`), the split of memory composition from index I/O (contents or unavailable), and the recorded ACP lifecycle fixtures under `mj-worker/src/acp/testdata/project-memory/` with their tests. Do not reuse: `pending_project_memory`, relay revision 28, compaction recognisers, the refresher, restored-open plumbing, or the publication-phase change.


## Plan of Work

Milestone 1: composer, delivery owner, memory.

Enable `unstable_session_system_prompt` on the workspace `agent-client-protocol` dependency and port the `AgentInitialized` serializer with its digest test.

Create the composer in the worker, next to session opening, as one function that takes the facts of this open and returns the ordered text: project memory first (omitted for Claude and when no project memory is configured), then, in Milestone 2, the tool and role sections. Build the memory section with the off-runtime reader; on a read failure, emit `RuntimeEvent::Warning` naming the path and the error and compose the section with an "index unavailable: <reason>" line instead of the index. Compose on every open (new, cleared, load, resume) so a reopened session gets the current index.

Create the delivery owner as one function, called after `initialize` and before the open request: standard system prompt if advertised, else the Antigravity vendor field if advertised, else the native instruction file. For the first two, pass the text to the request builders (port the earlier plumbing; Antigravity's string goes inside the existing `_meta` map, merged, not replacing it). For the native file, write the text into the session's staged instruction file inside a section Mjolnir owns, delimited by marker lines, replacing any previous Mjolnir section and leaving every other byte of the file unchanged, off the async runtime and before the open request. Before writing, confirm from the code that the worker can locate that staged file and that it is a per-session copy, never the user's own profile file or a file shared between sessions (check `shared_profile_login` and the staging copy in `mj-worker-staging/src/worker_binary/harness.rs:165–220`); if it is not per-session, stop and report.

Remove the old memory path: the pre-start installation in `unix.rs`, the `/clear` memory rebuild and the memory use of `ContextCleared { memory }` (keep the field on the wire as an always-`None` value if removing it would change the relay journal format; check `mj-core/src/relay/snapshot.rs` around 1452 and the journal readers). Existing sessions whose snapshot already holds a memory block in `pending_prompt_context` keep delivering it once through that generic slot, which stays for its other users; no migration.

Milestone 2: tool and role guidance.

Move each MCP guidance text into the composer as a section included only when this session actually offers that server and role (parent, child, reviewer, brief writer, message-only, fetch-only, review tools offered, history tools offered), keeping the Codex discovery suffix for Codex. Remove those `instructions` from the MCP initialize replies; keep tool descriptions, including the handback tool's description, so the tool remains self-explanatory. Move the standing parts of the child, reviewer and oracle first prompts and the review rubric and brief template into role sections; leave the assignment, question and packet in the prompt. The role must reach the worker as durable session facts available at every launch (new, resume, move), not as text inside a stored prompt; find where the worker learns that it is a child, reviewer or oracle and extend that if needed. Reconcile the reviewer output format with what the review consumer parses.

Milestone 3: live acceptance (Validation).


## Concrete Steps

Work in `~/Projects/mjolnir-wt/session-instructions` (detached at origin/master). Build and test only there, on the dev profile, outside the sandbox, running only the tests for what changed in each round:

    cd ~/Projects/mjolnir-wt/session-instructions
    cargo check -p brokk-mj-worker-runtime -p brokk-mj-core
    cargo test -p <crate> <filter>
    cargo clippy -p <edited crates> --all-targets -- -D warnings

(Use the actual package names from each crate's `Cargo.toml`.)


## Validation and Acceptance

Tests follow AGENTS.md "Which tests to add". Recorded ACP integration: for each standard-system-prompt harness and Antigravity, the open request built by the real builders carries the composed text on new, load and resume, and the recorded agent accepts it (reuse the earlier fixtures). Native-file integration: for Kimi, OpenCode and Grok, opening twice replaces Mjolnir's section without touching user text in the file, and a file with no Mjolnir section gains exactly one. Tricky conditions: an unreadable or non-UTF-8 `MEMORY.md` opens the session with a warning and the unavailable line; a stalled read does not hold runtime shutdown (port the earlier FIFO test). A golden test of the composed text per role (parent, child, reviewer, oracle) with a fixed memory directory keeps the wording reviewable.

Live acceptance in an isolated instance (never the default one): for every harness, start a parent session and one child, force a compaction (automatic where cheap, otherwise `/compact`), then ask where the project memory is, what `MEMORY.md` lists, and how to hand back. Answers must come from the composed instructions. Kimi's login is at its weekly quota until it resets; record that check as pending until then. Stop the instance's daemon afterwards.


## Idempotence and Recovery

Rewriting the native file's Mjolnir section is idempotent by construction. No persisted format changes besides the cargo feature's effect on `AgentInitialized`, which the serializer neutralises. If a pinned harness rejects the system prompt in acceptance, record it in Surprises and stop; do not add a fallback.


## Artifacts and Notes

Evidence reports (in the parent session's clone, `~/Projects/mjolnir/.mj/clones/c5dbf86d7cc3978aa0b6129a8c544ba8/.mj/agents/`): `4e014247b7bb2af73e2f243a0b39c689/findings.md` (harness compaction behaviour), `b1e50d77dd7373f226d281a1a65a3f51/findings.md` and its `traces/` (live traces), `a1b6d71037f25c6d906d907187b377ac/instruction-map.md` (instruction audit), `612df0f8daa5f49428d1b1093d7225f0/` (native-file reload evidence).


## Interfaces and Dependencies

In `mj-worker/src/acp/`, one composer function returning the ordered text for an open, and one delivery function returning which channel this open uses; the three request builders take an optional system-prompt text and an optional Antigravity append. The native-file writer lives next to the delivery function and reuses `append_staged_instructions`'s file-naming helper rather than hard-coding file names.
