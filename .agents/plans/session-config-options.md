# Expose every advertised ACP session setting

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Mjolnir can already apply any ACP session configuration option through `mj set-config` and the daemon API, but the terminal and web conversation surfaces expose only model and effort as general-purpose interactive controls. Other harness options, such as approval or collaboration settings, are therefore discoverable only by using a separate CLI and copying a key and value carefully.

After this change, a person in a live session can run `/config` to see every select option the harness advertised, choose one, and choose its value in the terminal. The web composer exposes the same catalogue through a visible Session settings control, and `/config <key> <value>` works on both surfaces. The existing `/model` and `/effort` shortcuts and `mj set-config` remain available. Nobody should need to know a harness-specific ACP key in advance.

## Progress

- [x] (2026-10-28) Trace the existing daemon and client paths and record the design.
- [x] (2026-10-28) Generalize the terminal selector and `/config` slash command.
- [x] (2026-10-28) Publish `/config` to the web client and add its settings dialog.
- [x] (2026-10-28) Add focused regression coverage for arbitrary option selection.
- [x] (2026-10-28) Update user documentation and the skill reference where session commands are listed.
- [x] (2026-10-28) Run affected tests and full validation for touched crates.
- [x] (2026-10-28) Check live-harness availability; no configured Mjolnir profile or provider login exists in this container, so no paid live turn was run.
- [x] (2026-10-28) Commit and push the validated change.

## Surprises & Discoveries

- Observation: The daemon already validates any advertised key and choice in `mj-controller/src/server/validation.rs`, so no new mutation endpoint or storage format is needed.
  Evidence: `ControllerAction::SetConfig` looks up the live option by key and verifies the value against its choices.
- Observation: The terminal picker stores model and effort in separate vectors and its key is a `&'static str`, while the web prompt readout and command palette hard-code the same two names.
  Evidence: `mj-chat/src/chat/config_picker.rs`, `mj-chat/src/chat/autocomplete.rs`, and `mj-controller/src/web/viewer.js`.
- Observation: The web snapshot already publishes the complete ACP option catalogue, including arbitrary option ids and their choices.
  Evidence: `viewer_config_options` in `mj-controller/src/server/config_view.rs` keeps model/effort canonical aliases and then publishes every other selector.
- Observation: Enabling the settings controls in the browser fixture exposed a latent header layout effect: the existing model select becomes visible when the fixture accurately reports `set_config`, adding one model name to the golden's visible text.
  Evidence: The regenerated `tests/e2e/web/golden/viewer_conversation.txt` records that model name and the new Session settings control.
- Observation: A named test instance changes the configured web port inherited by an isolated settings golden, making its expected `127.0.0.1:3765` fail even though the test itself is unrelated.
  Evidence: The same test passes alone and in the full controller suite when given unique `MJ_CONFIG_DIR` and `MJ_DATA_DIR` paths without `MJ_INSTANCE`; the host's default paths are never used.

## Decision Log

- Decision: Keep the daemon/API setter as the single mutation path and add only client-side discovery and selection.
  Rationale: The backend already owns live configuration validation, queueing, acknowledgement, and persistence.
  Date/Author: 2026-10-28 / Codex
- Decision: Use `/config` as the cross-surface command and reserve it from agent-advertised commands.
  Rationale: It matches the existing `mj set-config` terminology and can support both a guided picker and direct `key value` input.
  Date/Author: 2026-10-28 / Codex
- Decision: Apply one setting per user selection rather than batching several settings in the web dialog.
  Rationale: A model change can replace the effort catalogue. Sequential one-setting mutations preserve the harness's ordering and make each rejection attributable.
  Date/Author: 2026-10-28 / Codex
- Decision: Make `/config` open a centered terminal dialog while leaving `/model` and `/effort` on the existing anchored dropdowns.
  Rationale: The complete settings catalogue needs more visual separation than an anchored model picker, while the focused shortcuts remain fastest in their existing form.
  Date/Author: 2026-10-28 / Codex

## Outcomes & Retrospective

The feature is implemented on both interactive clients. `/config` opens a centered terminal dialog, while the browser exposes a Session settings modal and keeps the prompt settings visible. The full `brokk-mj-chat` suite (419 tests), `brokk-mj-core` suite (549 tests), and `brokk-mj-controller` suite (2184 tests) pass with 17 controller tests ignored for their documented external prerequisites. Workspace clippy passes with warnings denied. The complete web suite passes 58 unit tests and 114 deterministic browser tests, with 3 environment-dependent skips.

A real harness is installed in the container, but no configured Mjolnir profile or provider login exists, so the paid real-harness path was not exercised. The shipped viewer assets and daemon projection were exercised with an arbitrary `approval_mode` option, and terminal command and picker transitions were covered directly.

## Context and Orientation

An ACP session configuration option is a selectable setting advertised by the active agent harness. Each option has an id such as `model`, `effort`, `mode`, or `approval_mode`, a human-readable label, a current value, and a list of accepted values. The worker relays this catalogue to the daemon, which publishes it in each session snapshot. The terminal chat state lives in `mj-chat`, the web client is `mj-controller/src/web/viewer.js` with its HTML and CSS beside it, and the phone command projection is `mj-controller/src/server_runtime/snapshot.rs`.

The terminal's `ChatState` already tracks the ACP surface and routes `ChatAction::SetConfig` through `mj-chat/src/chat/active/dispatch.rs`. The web sends the same operation as `{"action":"set-config","session_id":...,"key":...,"value":...}` to `/api/actions`. The daemon validates it before sending a relay command to the worker.

## Plan of Work

First, extend `mj-chat/src/chat/config_picker.rs` so one picker can show either the advertised option catalogue or the values of one option. Keep the existing snapshotted choice list so a live refresh cannot reorder and apply an unintended value. Add an accessor on `mj-core/src/acp/surface.rs` for the full advertised options and use `session_config_choices` for every key.

Second, add `LocalCommand::Config`, command-palette entry, dynamic key/value completion, and direct parsing in `mj-chat/src/chat/autocomplete.rs` and `mj-chat/src/chat/input_state.rs`. Bare `/config` opens the option selector, `/config <key>` opens that option's value selector, and `/config <key> <value>` emits `ChatAction::SetConfig` after checking the current advertised catalogue. Include `config` in the local-command reservation list.

Third, add `/config` to `phone_commands` in `mj-controller/src/server_runtime/snapshot.rs`. In `mj-controller/src/web/viewer.js`, make the prompt settings line show all current options plus a Session settings button. The button opens a modal with one select per advertised option; changing a select posts one `set-config` action, waits for refresh, and re-renders from the latest session snapshot. Add generic `/config` command handling and completion over option ids and values. Add the minimum CSS needed for the dialog and clickable settings entry.

Fourth, add focused tests. In `mj-chat/src/chat/config_picker.rs`, test the option-catalogue to value-catalogue transition and direct arbitrary setting selection, including stale-catalogue refusal. In the web tests, use a fixture with a non-model option, exercise both the visible settings dialog and `/config`, and assert the exact posted action without allowing the browser to invent accepted values.

Finally, update `docs/src/content/docs/sessions.md`, `docs/src/content/docs/cli-reference.md` if wording needs to distinguish terminal and CLI commands, and `mj-core/assets/skills/mj/SKILL.md` so agents point users to `/config` for interactive changes and `mj set-config` for scripted changes. Run formatting and targeted tests after each round, then the required Cargo checks for touched Rust crates. Exercise the real binary in an isolated `--instance` with an agent catalogue if a suitable harness is available.

## Concrete Steps

Run all commands from `/workspace/33ae52f5cf7b8723db169f8dce7b347f/mjolnir`.

Start with:

    cargo test -p brokk-mj-core acp
    cargo test -p brokk-mj-chat config
    cargo test -p brokk-mj-controller phone_snapshot

Run browser checks from `tests/e2e/web` with the repository's documented Playwright setup, for example:

    npx playwright test plan-mode.spec.js --project deterministic --grep "session settings expose"

The new focused cases must fail before their implementation and pass afterward. After the final edits, run:

    cargo test -p brokk-mj-chat
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

The exact controller test invocation may be narrowed first to the affected test names, per the repository testing rules, and the touched-crate suites run once after the last change.

## Validation and Acceptance

With a live session whose harness advertises at least one option other than model and effort, running `/config` in the terminal must display every advertised option. Selecting the arbitrary option must replace the list with its accepted values; selecting a value must emit a setting change, clear the picker, and eventually show the harness's confirmed value in the prompt title or status.

Running `/config <arbitrary-key> <accepted-value>` must apply it. An unknown key or an unadvertised value must be refused locally with a message naming `/config`, without sending a prompt or mutation. `/model` and `/effort` must continue opening their direct value selectors.

In the web session, the prompt settings area must show a Session settings control. Opening it must list every advertised option and its current choice. Changing a non-model select must post exactly one `set-config` action with that key and value, disable controls while waiting, and expose a rejection in the dialog. `/config <key> <value>` must submit through the same action, and naked `/config` must open the dialog. The page must remain usable at 320 CSS pixels without horizontal scrolling.

## Idempotence and Recovery

The source edits and tests are repeatable. No database migration or durable data rewrite is involved. A failed client-side setting leaves the active harness configuration unchanged unless the daemon accepted the command; in that case the next snapshot is authoritative. Cancel a focused test daemon with the matching isolated instance name. Never point test commands at the default instance.

## Artifacts and Notes

Key files for orientation:

    mj-chat/src/chat/config_picker.rs
    mj-chat/src/chat/autocomplete.rs
    mj-chat/src/chat/input_state.rs
    mj-core/src/acp/surface.rs
    mj-controller/src/server_runtime/snapshot.rs
    mj-controller/src/web/viewer.js

## Interfaces and Dependencies

`mj_core::acp::surface::AcpSessionSurface` must expose its advertised configuration options by reference:

    pub fn config_options(&self) -> &[SessionConfigOption]

Existing `mj_core::acp::session_config_choices`, `find_session_config_option`, and `surface::config_current_value` remain the shared interpretation of ACP option ids and aliases. No new daemon API type or database field is required. `ViewerConfigOption` remains the web wire type and `ChatAction::SetConfig { key: String, value: String }` remains the terminal action.

---

Revision note: Created 2026-10-28 after tracing the existing `set-config` path and identifying the hard-coded model/effort controls in both interactive clients.
