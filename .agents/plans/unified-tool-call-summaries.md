# Render concise, stable tool-call summaries in the TUI and web viewer

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Tool calls currently change presentation as a conversation advances: a new TUI call first shows its full provider title, then older completed calls collapse to the first whitespace-delimited word. The browser does not apply the same grouping at all. After this work, a shell call such as `cd dir && python x.py | cat | wc ; print ok` appears immediately as `cd && python | cat | wc ; print` in both Rich TUI and web feeds. Consecutive successful completed calls still form one row, while pending, running, and failed calls remain separate. TUI Raw mode continues to show original titles and details.

A follow-up enriches direct invocations of common developer CLIs with their semantic command path: `git add`, `cargo test`, `gh pr create`, and `docker volume rm`. Arguments such as paths, package names, and npm script names remain omitted. String shell commands and raw argv use the same invocation summarizer.

A corrective follow-up makes compound Bash summaries structural rather than punctuation-driven. It removes orphan separators from loops and conditionals, represents newline-separated commands, recognizes the command launched through `nice`, and lets a TUI reader expand one completed call in place without losing the compact grouping around it. Completed tool presentation is subdued like thinking in both Rich surfaces so active and failed work remains visually prominent.

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
- [x] (2026-09-10 00:52Z) Merged current upstream, passed final validation on v2.6.0, reviewed the result, and prepared it for push.
- [x] (2026-09-10 02:05Z) Recovered the exact malformed commands from the live materialized session and identified punctuation flattening plus missing newline separators as distinct parser defects.
- [x] (2026-09-10 02:05Z) Settled transient TUI expansion identity, group-splitting behavior, selection-safe click routing, and completed-tool visual treatment.
- [x] (2026-09-10 03:10Z) Corrected compound-command normalization and heredoc-following separation, added `nice` command extraction, and versioned cached summaries so affected saved sessions repair on load.
- [x] (2026-09-10 03:10Z) Added TUI member hitboxes and per-call expand/collapse behavior, then applied completed-tool desaturation to TUI and web.
- [x] (2026-09-10 03:38Z) Ran focused and full validation, captured and reviewed the TUI, fast-forwarded current upstream, and committed and pushed the current branch.

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
- Observation: Anonymous semicolons in the Bash concrete syntax tree serve both executable list separation and compound-keyword punctuation.
  Evidence: The live `for ...; do ...; done; python3 ...` call summarized as `cd && ; set ; ; ; ; rm ; mkdir ; ; nice ; echo ; ; python3`; the walker emitted punctuation before `do` and `done` even though those keywords produce no command summary.
- Observation: A newline after a heredoc terminator separates commands but is not an anonymous operator node collected by the original walker.
  Evidence: Live calls ending `PYEOF\ngrep ... | head` summarized as `python3 grep | head`, joining two independent invocations with a space.
- Observation: Before the follow-up, ordinary raw argv lost every element after the executable, while shell-interpreter argv already unwrapped its script for tree-sitter.
  Evidence: The old `command_source` returned `ToolSummarySource::Executable(first)` for `['git', 'add', '.']`; it now retains the vector and shares invocation parsing with Bash command nodes.
- Observation: The upstream branch gained the v2.6.0 release and a Kimi detached-shell fix while this follow-up was in progress.
  Evidence: `origin/master` advanced through `2da5697f` and `492a4bdf`; both merged cleanly before final validation.
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
- Decision: Derive separators between adjacent summarized command ranges, preferring explicit logical, pipeline, or background operators and using one semicolon for structural or newline-only boundaries.
  Rationale: A command pair is the semantic unit the compact text connects. This prevents grammar punctuation from appearing without commands and prevents newline-separated commands from being concatenated.
  Date/Author: 2026-09-10 / Codex
- Decision: Treat `nice` as a developer-tool summary entry whose first literal non-option operand is the launched command.
  Rationale: Long-running validation calls commonly use `nice -n 10`; showing `nice cargo` or `nice python3` distinguishes their actual work while preserving the established one-extra-word rule.
  Date/Author: 2026-09-10 / Codex
- Decision: Keep expanded-call state local to the TUI and key it by the durable entry's `start_seq`.
  Rationale: Expansion is a reader preference rather than transcript data. Excluding the expanded call from a completed streak naturally splits the compact group on both sides, and stable sequence identity distinguishes repeated commands.
  Date/Author: 2026-09-10 / Codex
- Decision: Toggle expansion only after selection classifies a gesture as a click, using hitboxes rebuilt from the rendered frame.
  Rationale: This keeps drag selection intact and ensures wrapped or scrolled summary segments target what the reader actually clicked.
  Date/Author: 2026-09-10 / Codex
- Decision: Version cached summaries and rederive metadata written by older parser rules from the stored complete call.
  Rationale: The reported live conversation already contains persisted malformed labels. Versioning repairs those rows after upgrade while continuing to reuse current cached summaries; this supersedes the earlier no-retrofit choice for parser bug fixes.
  Date/Author: 2026-09-10 / Codex

## Outcomes & Retrospective

The shared parser now emits `cd && python | cat | wc ; print` for the requested command and uses the same compact summary from the first pending/running frame through completion or failure. Rich TUI and browser projections use one grouping decision, so sequential successful calls form one comma-separated row while active and failed calls remain boundaries. Raw TUI mode still retains provider titles and details.

The browser carries an opaque topology key with its relay cursor. Group formation, growth, dissolution, or thought reordering forces a complete feed replacement; ordinary content changes remain incremental. Older browser/server combinations remain compatible when the key is absent.

The follow-up now enriches direct developer CLI invocations in both string commands and raw argv. It emits summaries such as `git add`, `cargo test`, `gh pr create`, `docker volume rm`, and `uv tool install`; unknown literal verbs remain visible for whitelisted tools, while arbitrary arguments and non-whitelisted commands stay compact. Current persisted presentations are read unchanged.

The corrective pass now joins only adjacent parsed commands, so structural punctuation around `for`, `if`, groups, and `case` cannot become orphan separators. Newline-only boundaries such as a command after a heredoc render as one semicolon, and `nice` exposes its launched command after recognized adjustment options. Presentation metadata carries a parser version, so complete calls saved under the old rules are repaired from their stored ACP source when either Rich surface loads them.

Completed TUI tools can be clicked by their exact compact member text. Opening one call renders its full provider title and details and splits completed groups on both sides; clicking its expanded rows closes it and allows the group to reform. Pending and completed tools use the thinking palette in the TUI, and the web viewer applies the same subdued treatment to non-active tool rows. Running and failed calls retain their status emphasis.

Validation passed: focused core/chat/controller/TUI behavior tests; 13 Node viewer tests; the complete `cargo test` workspace suite; `cargo clippy --all-targets -- -D warnings`; `cargo deny` license checks; generated Cargo About and supplemental notices; formatting and diff checks. A 120×42 terminal-cell capture at `/tmp/unified-tool-call-summaries-tui.json` showed `cd && python | cat | wc ; print, cargo` as one completed group and `cargo` immediately for a running call. The only unavailable supplemental check was the optional desktop all-target build described above.

For the follow-up, 18 focused core parser tests and 106 focused chat/TUI/browser transcript tests pass. After merging current upstream, the complete workspace suite and clippy with warnings denied passed on v2.6.0. The Cargo About output exactly matches the committed license report, `cargo deny` passed with its pre-existing unmatched-exception warning, supplemental notices regenerated without a diff, and all 13 browser viewer tests passed. The 120×42 terminal-cell capture at `/tmp/tool-subcommand-summaries-tui.json` shows `cd && python | cat | wc ; print, cargo test` in one completed group and `cargo clippy` immediately for a running call.

For the corrective pass, 23 core parser tests, 112 chat transcript tests, all 25 browser unit tests, the complete workspace suite, and clippy with warnings denied passed. After the final upstream fast-forward, the changed controller suite passed serially with 758 tests and the client suite passed all 7 tests; the ordinary parallel controller run's sole `ETXTBSY` executable-copy failure passed immediately alone and in the serial suite. The 120×42 capture at `/tmp/tool-call-expansion-tui.json` shows the expanded full call split from its compact neighbor, both completed rows muted, and the running row emphasized.

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

The corrective expansion evidence was generated with:

    MJ_CHAT_CAPTURE_PATH=/tmp/tool-call-expansion-tui.json \
      MJ_CHAT_CAPTURE_COLUMNS=120 MJ_CHAT_CAPTURE_ROWS=42 \
      cargo test -p brokk-mj-chat capture_chat_preview -- --ignored --nocapture

Review `git diff`, stage only files changed for this feature, commit on the current branch, and push the current branch to its upstream as requested.

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

Revision note (2026-09-10): Added the developer-core CLI subcommand follow-up, raw-argv parity, compatibility policy, final v2.6.0 validation, and push preparation.

Revision note (2026-09-10): Added the corrective parser, persisted-summary repair, `nice` extraction, TUI expansion, and completed-tool styling follow-up after reproducing the malformed live transcript.
