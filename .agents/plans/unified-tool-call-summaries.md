# Render concise, stable tool-call summaries in the TUI and web viewer

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Tool calls currently change presentation as a conversation advances: a new TUI call first shows its full provider title, then older completed calls collapse to the first whitespace-delimited word. The browser does not apply the same grouping at all. After this work, a shell call such as `cd dir && python x.py | cat | wc ; print ok` appears immediately as `cd && python | cat | wc ; print` in both Rich TUI and web feeds. Consecutive successful completed calls still form one row, while pending, running, and failed calls remain separate. TUI Raw mode continues to show original titles and details.

A follow-up enriches direct invocations of common developer CLIs with their semantic command path: `git add`, `cargo test`, `gh pr create`, and `docker volume rm`. Arguments such as paths, package names, and npm script names remain omitted. String shell commands and raw argv use the same invocation summarizer.

## Progress

- [x] (2026-09-09 21:17Z) Inspected the live ACP, materialized transcript, TUI collapse, browser projection, HTTP delta, and DOM update paths.
- [x] (2026-09-09 21:17Z) Settled parser behavior, status/grouping semantics, Raw-mode behavior, and the browser topology-reset protocol.
- [x] (2026-09-09 23:06Z) Added the shared tree-sitter parser, bounded presentation sidecar, live-update handling, and archive round trips.
- [x] (2026-09-09 23:06Z) Shared Rich grouping with browser projection and added presentation-key reset handling.
- [x] (2026-09-09 23:06Z) Updated browser polling/key state and advanced the service-worker shell cache to v12.
- [x] (2026-09-09 23:06Z) Regenerated dependency/license metadata and passed the focused, workspace, clippy, license, JavaScript, and capture validations.
- [x] (2026-09-09 23:18Z) Reviewed the integrated diff and prepared the validated implementation for its required current-branch commit.
- [x] (2026-09-10 00:02Z) Settled the developer-core whitelist, semantic-depth rules, direct-wrapper behavior, raw-argv handling, and no-retrofit compatibility policy.
- [x] (2026-09-10 00:31Z) Added shared subcommand extraction for Bash command nodes and ordinary raw argv.
- [x] (2026-09-10 00:31Z) Proved TUI, browser, grouping, and persisted-summary behavior and recaptured the terminal evidence.
- [ ] Finish validation against the merged v2.6.0 upstream, review, commit, and push the follow-up.

## Surprises & Discoveries

- Observation: Before this change, the TUI protected the newest completed tool from collapse, which directly caused the visible full-title-to-first-word transition.
  Evidence: The removed `protected_tool_index` path excluded the newest completed call until a later entry arrived; the replacement applies its compact summary immediately.
- Observation: The browser updates keyed DOM nodes but does not remove omitted nodes on an ordinary delta.
  Evidence: `applyConversationEntries` in `mj-controller/src/web/viewer.js` only appends or replaces IDs; `reset` clears the feed.
- Observation: ACP `ToolCallUpdateFields` values are patches rather than complete calls.
  Evidence: `apply_session_update_to_entries` currently updates individual title, status, content, and location fields, so a stable summary must retain the source selected by earlier updates.
- Observation: A status update can change group membership without changing the number of transcript entries.
  Evidence: The first focused TUI rerun retained a stale collapse decision until the render cache began fingerprinting member revisions as well as entry count.
- Observation: Cargo's `links` inventory treats both new tree-sitter packages as native-link participants even though `tree-sitter-language` uses the field for ABI uniqueness and wasm metadata.
  Evidence: The supplemental notice generator rejected the unaudited packages until both roles were recorded in `auditedLinksPackages`; the regenerated notice check then passed.
- Observation: The optional all-target workspace check reaches desktop bindings unavailable on this host.
  Evidence: `cargo check --workspace --all-targets` stopped at missing `libsoup-3.0`, Pango, GLib, GDK, and Cairo system packages. The required default workspace test and clippy commands do not select that unavailable desktop host configuration and passed.
- Observation: Before the follow-up, ordinary raw argv lost every element after the executable, while shell-interpreter argv already unwrapped its script for tree-sitter.
  Evidence: The old `command_source` returned `ToolSummarySource::Executable(first)` for `['git', 'add', '.']`; it now retains the vector and shares invocation parsing with Bash command nodes.
- Observation: The upstream branch gained the v2.6.0 release commit while this follow-up was in progress.
  Evidence: `origin/master` points at `2da5697f`, one commit beyond this branch's original base, and changes only release plans, workspace versions, the lockfile, and generated license versions.
- Observation: The first full follow-up test run reached one transient `ETXTBSY` failure while spawning a copied controller test executable.
  Evidence: `npm_upgrade_restarts_after_the_running_package_is_removed` passed immediately when rerun alone; no tool-summary assertion failed.

## Decision Log

- Decision: Parse Execute calls with `tree-sitter-bash` and keep the existing compact-token rule for other real ACP calls.
  Rationale: Shell syntax needs structural parsing, while non-shell titles do not represent Bash programs; every real call must still receive its compact label immediately.
  Date/Author: 2026-09-09 / Codex
- Decision: Preserve the original provider title and cache bounded presentation source metadata beside the canonical tool call.
  Rationale: Raw mode needs the original text and patch updates need enough state to recompute without inventing ACP metadata.
  Date/Author: 2026-09-09 / Codex
- Decision: Group only completed successful calls; active and failed calls are visible group boundaries. Preserve the newest interleaved thought before a grouped row.
  Rationale: This is the user-selected behavior and matches the useful part of the existing TUI collapse.
  Date/Author: 2026-09-09 / Codex
- Decision: Add an opaque browser presentation key derived from Rich topology.
  Rationale: A key mismatch can force a full reset when grouping removes or reorders already-delivered rows, including same-relay-ordinal projection repairs, without overloading the event cursor.
  Date/Author: 2026-09-09 / Codex
- Decision: Whitelist the developer-core executable set: git, gh, cargo, rustup, npm, pnpm, yarn, bun, uv, pip, pip3, docker, and podman.
  Rationale: These CLIs have stable command positions and materially different verbs in coding sessions; infrastructure cloud CLIs can be added later with their own option grammars.
  Date/Author: 2026-09-10 / Codex
- Decision: Show any unambiguous literal first verb for a whitelisted executable, with a registered second level for semantic namespaces such as `gh pr`, `docker volume`, `uv tool`, and `rustup target`.
  Rationale: This supports plugin and future verbs without treating arbitrary operands as nested commands.
  Date/Author: 2026-09-10 / Codex
- Decision: Do not unwrap launch wrappers and do not retrofit stored presentation summaries.
  Rationale: Direct invocations stay predictable (`sudo git status` remains `sudo`), and existing saved conversations retain the exact presentation already persisted.
  Date/Author: 2026-09-10 / Codex

## Outcomes & Retrospective

The shared parser now emits `cd && python | cat | wc ; print` for the requested command and uses the same compact summary from the first pending/running frame through completion or failure. Rich TUI and browser projections use one grouping decision, so sequential successful calls form one comma-separated row while active and failed calls remain boundaries. Raw TUI mode still retains provider titles and details.

The browser carries an opaque topology key with its relay cursor. Group formation, growth, dissolution, or thought reordering forces a complete feed replacement; ordinary content changes remain incremental. Older browser/server combinations remain compatible when the key is absent.

The follow-up now enriches direct developer CLI invocations in both string commands and raw argv. It emits summaries such as `git add`, `cargo test`, `gh pr create`, `docker volume rm`, and `uv tool install`; unknown literal verbs remain visible for whitelisted tools, while arbitrary arguments and non-whitelisted commands stay compact. Existing persisted presentations are read unchanged.

Validation passed: focused core/chat/controller/TUI behavior tests; 13 Node viewer tests; the complete `cargo test` workspace suite; `cargo clippy --all-targets -- -D warnings`; `cargo deny` license checks; generated Cargo About and supplemental notices; formatting and diff checks. A 120×42 terminal-cell capture at `/tmp/unified-tool-call-summaries-tui.json` showed `cd && python | cat | wc ; print, cargo` as one completed group and `cargo` immediately for a running call. The only unavailable supplemental check was the optional desktop all-target build described above.

For the follow-up, 18 focused core parser tests and 106 focused chat/TUI/browser transcript tests pass, as does clippy with warnings denied. The 120×42 terminal-cell capture at `/tmp/tool-subcommand-summaries-tui.json` shows `cd && python | cat | wc ; print, cargo test` in one completed group and `cargo clippy` immediately for a running call. Final workspace validation will be repeated after merging the upstream release commit.

## Context and Orientation

`src/hel_transcript.rs` defines the shared `ChatEntry` rendered by clients and applies live ACP updates. `src/hel_projection.rs` stores canonical transcript items and reconstructs them from legacy entries. `mj-chat/src/hel_chat/transcript.rs` projects canonical items, computes TUI collapse states, and creates browser transcript payloads. `mj-client/src/web.rs` defines that wire payload. `mj-controller/src/hel_server.rs` serves cached projections using a relay-event cursor, while `mj-controller/src/web/viewer.js` polls and updates DOM nodes by entry ID.

A Rich projection is the decluttered transcript used by the normal TUI and browser. Raw mode is the TUI diagnostic view that retains provider titles, tool details, and entries omitted as duplicates. A presentation topology is the ordered set of rows produced after Rich grouping and omission; content may change without changing this topology.

## Plan of Work

Add workspace-compatible `tree-sitter` and `tree-sitter-bash` dependencies to the core crate. Implement one shared helper near `ChatEntry` that selects a command source and parses it outside render loops. For Execute calls, prefer a string `raw_input.command`; for an argv array, unwrap scripts passed through `sh`, `bash`, `dash`, or `zsh` command flags, otherwise retain the complete string vector for direct invocation parsing. If raw input is unavailable, strip known lifecycle wrappers from the provider title and parse that. Bound source length before parsing. Walk Bash list, pipeline, subshell, and command nodes so anonymous operator tokens remain visible; summarize each direct command with the same invocation helper used by raw argv. For whitelisted executables, include the first literal verb after recognized global options and a second verb for registered namespaces. Skip arguments, assignments, redirections, dynamic expressions, and substitutions. Ambiguous leading options fall back to the executable. If parsing yields no executable, use the normalized first meaningful title token. Apply that compact-token behavior directly to non-Execute calls.

Extend canonical tool transcript data with optional serde-defaulted presentation metadata holding the summary, its bounded selected source, source kind, and tool kind. Extend `ChatEntry` with an optional serde-defaulted summary. Populate and update these fields in both live ACP and materialized projections, preserving them through legacy reconstruction and canonical serialization. Title-only updates must not erase a raw-command-derived summary; raw input or kind updates recompute it. Keep `ChatEntry.text` unchanged for Raw mode. The client-generated active-terminal card has no real tool-call identity and keeps its current full text. Failed fallback tools must render the compact label plus their failure details in Rich and web while retaining the original full entry in Raw.

Refactor Rich collapse into a shared presentation projection used by TUI line rendering and `TranscriptSnapshot::browser_transcript`. Remove newest-tool protection. A lone real tool renders with its cached summary. Two or more sequential completed-success tools, with thoughts allowed between them, produce the newest thought followed by one synthetic tool entry whose comma-separated components are per-call summaries. The synthetic entry carries the maximum member update cursor and revision. Pending, running, and failed tools terminate a run. Existing restart and raw-only omission rules remain active.

Add `presentation_key` to `BrowserTranscript` and an optional matching query field to the conversation endpoint. Compute a stable SHA-256 digest over only structural Rich decisions: ordered grouped member identities and roles, plus omitted/restart identities. Exclude normal rows and content revisions so ordinary appends and content updates stay incremental. The browser stores the last key and sends it with later `after_seq` polls. A mismatch returns the full current projection with `reset: true`; a match uses existing cursor filtering. Missing keys retain backward compatibility. Update JavaScript state/reset handling and bump `mj-controller/src/web/service-worker.js` from shell cache v11 to v12.

Update focused Rust and JavaScript behavior tests. Regenerate `Cargo.lock`, `licenses/THIRD_PARTY_LICENSES.html`, and any supplemental notice output required by `CONTRIBUTING.md`.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`.

First implement and run focused parser/transcript tests:

    cargo test -p brokk-mj-core hel_transcript::tests
    cargo test -p brokk-mj-chat transcript

Then run browser/server tests, using the repository's existing Node test invocation discovered from the test harness and focused Rust test filters. Run all Cargo test commands outside the restricted sandbox because the repository tests require loopback sockets.

After implementation, format and validate the workspace:

    cargo fmt --all
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
    node scripts/generate-supplemental-third-party-notices.mjs

All commands above completed successfully. The final TUI evidence was generated with:

    MJ_CHAT_CAPTURE_PATH=/tmp/unified-tool-call-summaries-tui.json \
      MJ_CHAT_CAPTURE_COLUMNS=120 MJ_CHAT_CAPTURE_ROWS=42 \
      cargo test -p brokk-mj-chat capture_chat_preview -- --ignored --nocapture

Review `git diff`, stage only files changed for this feature, and commit on the current branch. Do not push.

## Validation and Acceptance

The exact shell source `cd dir && python x.py | cat | wc ; print ok` must summarize as `cd && python | cat | wc ; print`. Tests must cover pipelines and lists, quotes, assignments, redirections, subshells, command substitutions, shell `-c` argv, ordinary argv, provider wrappers, malformed input, non-Execute titles, and bounded fallback behavior.

The follow-up must additionally prove `git add && git commit`, `cargo test`, `gh pr create`, `docker volume rm`, and `uv tool install`; raw argv and string commands must agree. Tests must cover recognized leading options, Cargo toolchain selectors, unfamiliar literal verbs, ambiguous options, quoted and dynamic verbs, non-whitelisted executables, direct-only wrappers, and preservation of an already stored one-word presentation.

Live and materialized tests must prove that the same compact label is present while pending, running, completed, and failed, without a title transition. They must prove partial updates and legacy round trips preserve the summary, failed details remain visible, and Raw retains original text. Collapse tests must cover one call, two completed calls, interleaved thoughts, group growth, and active/failed boundaries.

Browser tests must prove initial parity with Rich output, maximum-member revisions on synthetic rows, full reset on presentation-key mismatch when a group forms, grows, or dissolves, and incremental updates when only content changes. JavaScript tests must prove key storage and query behavior, reset replacement, and compatibility with a response that omits the key.

All workspace tests and clippy with warnings denied must pass. Dependency license checks and generated notices must be clean. A manual TUI and browser run should show matching compact labels and grouped rows for the same transcript.

## Idempotence and Recovery

All code-generation and validation commands are safe to repeat. If dependency metadata changes partially, rerun Cargo metadata generation and the prescribed license commands rather than editing generated files manually. Never discard unrelated working-tree changes; stage only files changed for this feature.

## Artifacts and Notes

Expected core example:

    input:  cd dir && python x.py | cat | wc ; print ok
    output: cd && python | cat | wc ; print

Within one tool call, shell operators remain visible. A comma separates summaries from distinct grouped tool calls. Command names remain as written after quote normalization; no special inference is made through wrappers such as `sudo`.

Recognized follow-up examples:

    git add src && git commit -m msg    -> git add && git commit
    ['git', '-C', 'repo', 'status']    -> git status
    gh pr create --draft               -> gh pr create
    npm run build                      -> npm run

## Interfaces and Dependencies

The core crate uses `tree-sitter` 0.25 with `tree-sitter-bash` 0.25.1. `TranscriptBody::Tool` gains optional presentation metadata with serde defaults. `ChatEntry` gains `tool_summary: Option<String>`. `BrowserTranscript` gains an opaque string `presentation_key`. The conversation GET query gains an optional string with the same name. The wire changes are additive and older persisted transcript bodies and cached browser fixtures must continue to deserialize.

Revision note (2026-09-09): Created the initial implementation-ready plan after repository exploration and two independent flow/parser reviews. It records the user-selected status, grouping, thought, and Raw-mode behavior and the topology-key solution for browser deltas.

Revision note (2026-09-09): Recorded the implemented parser, cross-surface grouping, browser reset protocol, validation results, terminal capture, and the optional desktop dependency limitation before final review and commit.
