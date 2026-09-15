# Replace the type-ahead and opening-session prompt panes with the real composer

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to
date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

While a session is starting, resuming, or being attached to the terminal, the
dashboard today shows two stand-in "prompt" panes that are not the real
composer: a dashboard-owned type-ahead `TextInput` during Starting/Resuming
transitions, and an "Opening session" advice panel while an attach is in
flight. The type-ahead pane answers only plain keys — every Ctrl/Alt readline
binding of the real composer (Ctrl-A/E, Ctrl-K/Y, Alt-B/F, word kills, and so
on) is missing — and both panes make starting a session feel like a different
application.

After this change there is one prompt pane everywhere: the real chat composer
(`mj-chat`'s `ChatState`) rendered in the prompt band during Starting/Resuming
transitions and during an attach, with the full readline key handling. Enter
still does not send while no session is live (it keeps the draft and explains
why), and whatever was typed flows into the real conversation when the session
attaches. The two stand-in panes are removed.

How to see it working: launch a new session (or restart a live one); while the
transition panel shows in the transcript band, the prompt band is the real
composer. Ctrl-A moves to line start, Ctrl-K kills to line end, Ctrl-Y yanks,
Alt-B/F move by words. Enter shows "Sending opens when the session is live;
the draft is kept." When the session opens, the draft is in the real composer.

## Progress

- [x] (2026-09-15) Map the existing type-ahead/opening flow across mj-tui, mj-cli, mj-chat.
- [x] (2026-09-15) Write this ExecPlan.
- [x] (2026-09-15) mj-chat: `ChatState::standby` constructor, `draft()`, `set_draft()`,
      `paste()`, `desired_prompt_height()`, `draw_prompt_band()`, `take_render_changed`
      made public.
- [x] (2026-09-15) mj-chat: composer band of `render_in` extracted into
      `render_composer_band`.
- [x] (2026-09-15) mj-chat: standby semantics — Enter keeps the draft with a notice, no
      autocomplete popup, standby placeholder and hint text.
- [x] (2026-09-15) mj-tui: `standby_prompts: BTreeMap<String, ChatState>` replaces
      `transition_composers`; key and paste routing through
      `handle_standby_prompt_key`.
- [x] (2026-09-15) mj-tui combined.rs: standby prompt drawn during transitions and
      attaches; `render_type_ahead_composer` and the Opening prompt panel deleted;
      `render_empty_conversation` split into transcript hero + prompt advice.
- [x] (2026-09-15) mj-cli: seed/carry-over/ChatOpened rewiring (standby draft preferred
      over the cache when the attach lands).
- [x] (2026-09-15) Tests updated and added; workspace `cargo test` green except two
      pre-existing `brokk-mj-controller` failures proven present on the pristine tree;
      `cargo clippy --all-targets -- -D warnings` clean.
- [ ] Memory note updated; work committed.

## Surprises & Discoveries

- Observation: `ChatState::handle_key` already centralizes every readline
  binding, and `ChatState::new` is a plain data constructor from a snapshot —
  so a bare `ChatState` can exist without a live session if a dedicated
  constructor skips snapshot bootstrap.
  Evidence: `mj-chat/src/chat.rs:668-804`; key dispatch at
  `mj-chat/src/chat.rs:2778-2974`.
- Observation: at `Focus::Prompt` the action registry matches only chords
  (Alt letters, function keys, Ctrl page keys) — plain characters and
  non-chord Ctrl/Alt combos fall through. That makes `spec_for_key(key,
  Focus::Prompt)` the exact boundary between "dashboard chord" and "composer
  key".
  Evidence: `mj-tui/src/actions.rs:797-819`.
- Observation: the registry's only Ctrl bindings are Ctrl-PageUp/PageDown, and
  its Alt chords are Alt-N/W/S/A/X/Z/G/Q — none collide with the readline
  chord set (Ctrl-A/E/K/Y/W/U/J/M, Alt-B/F/D, Alt-Enter), so the standby gets
  the full readline surface without registry changes.
  Evidence: `mj-tui/src/actions.rs` `GLOBAL_CHORDS` and `COMMANDS` keys.
- Observation: two `brokk-mj-controller` tests (`cache_gc_evicts_the_oldest_mirror_to_meet_its_soft_cap`,
  `the_ec2_disk_probe_fails_instead_of_undercounting_an_unreadable_path`) fail in this
  environment on the pristine tree too (verified via `git stash`), so they are
  pre-existing and unrelated.
  Evidence: `/tmp/pristine1.log`, `/tmp/pristine2.log` after stashing all changes.

## Decision Log

- Decision: the standby pane is a real `ChatState` built by a new
  `ChatState::standby` constructor, not a richer `TextInput` clone and not a
  full `ActiveChat`.
  Rationale: `ActiveChat` requires a live `ManagedSessionHandle` and spawns
  feeds, so it cannot exist pre-attach; a `ChatState` gives the exact real
  composer key handling and rendering for free. A `TextInput` again would
  repeat the bindings mismatch being fixed.
  Date/Author: 2026-09-15 / agent.
- Decision: Enter during standby is intercepted inside `submit_input` (keeps
  the draft, posts the shared notice), not by the host.
  Rationale: the no-send rule then holds for every host and routing path, and
  `ChatAction::Prompt` can never escape a session-less chat.
  Date/Author: 2026-09-15 / agent.
- Decision: autocomplete is disabled in standby (`update_autocomplete` no-ops)
  instead of drawing popups from the standby band.
  Rationale: command completion popups render through `render_in`'s overlay
  path, which the standby band does not run; an open-but-invisible popup would
  silently swallow arrow keys.
  Date/Author: 2026-09-15 / agent.
- Decision: retiring (Moving/Stopping/Destroying) and failed transitions keep
  the existing Status panel.
  Rationale: there is no conversation to type toward, and the user's complaint
  covers the two type-toward stand-ins only.
  Date/Author: 2026-09-15 / agent.

## Outcomes & Retrospective

The two stand-in prompt panes are gone. During a Starting/Resuming transition
and while an attach is in flight, the prompt band is the chat's own composer
(a `ChatState::standby` instance) with the full readline key handling; the
conversation band above it still shows the transition panel or the opening
hero. Enter never sends while no session is live — the interception lives in
`submit_input`, keyed off the `standby` flag, so no host can bypass it — and
the draft flows into the real conversation through the existing
`ComposerDraftCache` capture plus a new preference for the standby's newest
text in the `ChatOpened` handler (closing a race where typing during the
attach would have been lost).

Validation: mj-chat 514, mj-tui 451, mj-cli 101+integration tests green;
`cargo clippy --all-targets -- -D warnings` clean; the two failing
`brokk-mj-controller` tests were proven pre-existing on the pristine tree.

Lesson: when the codebase already funnels behavior into one state machine
(here, the composer key dispatch), the cheap route to "make X behave like Y"
is a second constructor of the same state machine plus visibility fixes, not
a parallel implementation. The former type-ahead `TextInput` was exactly such
a parallel implementation.

## Context and Orientation

Three crates matter. `mj-chat` owns the chat UI: `ChatState`
(`mj-chat/src/chat.rs:470`) holds the composer text, kill buffer, history, and
the readline key dispatch in `handle_key` (`chat.rs:2544`); `ActiveChat`
(`mj-chat/src/chat/active.rs:425`) wraps a `ChatState` plus a live session
handle and background feeds; `render_in` (`active.rs:2691`) draws transcript
and composer, with the composer band drawn in the `else` branch at
`active.rs:2864-3080`. `mj-tui` owns the dashboard projection
(`DashboardState`, `mj-tui/src/lib.rs:518`) and the combined render
(`mj-tui/src/combined.rs`). `mj-cli` owns the controller/attachment lifecycle:
`open_chat_session` (`mj-cli/src/dashboard.rs:1636`), the `ChatOpened` handler
(`mj-cli/src/dashboard/io.rs:1735`), and `begin_lifecycle_operation`
(`mj-cli/src/dashboard/actions.rs:1232`).

Today's stand-ins: `DashboardState::transition_composers`
(`BTreeMap<String, TextInput>`) is seeded by `seed_transition_composer` from a
warm chat's draft, edited by `handle_type_ahead_key` (plain keys only), drawn
by `render_type_ahead_composer` (`combined.rs:898`) inside
`render_transition_surface` (`combined.rs:759`), and carried over by
`take_transition_composer_draft` in `open_chat_session`. While an attach is in
flight, `set_opening_session` makes the chat-less branch draw
`render_empty_conversation` (`combined.rs:990`) whose prompt band is the
"Opening session" advice panel.

Key routing facts: `handle_dashboard_key` (`mj-tui/src/lib.rs:1769`) tries the
type-ahead composer for plain keys before list navigation; chords fall through
to the registry (`spec_for_key`, `mj-tui/src/actions.rs:797`), which at
`Focus::Prompt` only answers chords. `handle_paste` (`lib.rs:1540`) routes to
the transition composer. Mouse clicks inside the prompt band already focus the
prompt (`mj-cli/src/dashboard.rs:2764-2771`).

## Plan of Work

mj-chat first, so the other crates can switch to real APIs instead of
transitional shims.

1. `mj-chat/src/chat.rs`: add a `standby: bool` field (false in the existing
   constructors). Add `ChatState::standby(session_id, config, header,
   notices)` building the same field set as `new` with `WorkerPhase::Idle`, an
   empty transcript, the header identity applied, and `standby: true`. Add
   `pub fn draft(&self) -> String` (wrapping `encoded_draft`), `pub fn
   paste(&mut self, text: &str)` (wrapping `input::handle_paste`), and make
   `take_render_changed` public. In `submit_input`, when `standby`, keep the
   input and return `ChatAction::None` after
   `set_notice("Sending opens when the session is live; the draft is kept.")`.
2. `mj-chat/src/chat/autocomplete.rs`: in `update_autocomplete`, clear and
   return when `self.standby`.
3. `mj-chat/src/chat/active.rs`: move the composer-band `else` body of
   `render_in` into `pub(super) fn render_composer_band(frame, prompt_area,
   chat, prompt_focused, note: Option<Line<'static>>)`; `note` is appended as
   a left-aligned bottom-border line (the real render passes `None`). Render
   the standby placeholder instead of the chat placeholders when
   `chat.standby`. Add `ChatState::draw_prompt_band(&mut self, frame, area,
   prompt_focused, note)` that clears the frame's chat surfaces, calls
   `render_composer_band`, and leaves the surfaces registered for the host to
   merge. Add `ChatState::desired_prompt_height(width)` mirroring
   `ActiveChat::desired_prompt_height`, and have the latter delegate.
4. `mj-tui/src/lib.rs`: replace `transition_composers` with
   `standby_prompts: BTreeMap<String, ChatState>`. Methods:
   `standby_prompt_session()` (gate: selected session whose transition is
   Starting/Resuming, or `opening_session` equals the selection),
   `standby_prompt(id)` / `standby_prompt_mut(id)` (lazy-create via
   `ChatState::standby`, header identity built from the session record and
   subagent count), `seed_standby_prompt(id, text)` (creates the entry and
   sets the draft), `take_standby_prompt_draft(id)` (removes, returns draft),
   `drop_standby_prompt(id)`, and clearing on workspace switch. Replace
   `handle_type_ahead_key` with `handle_standby_prompt_key`: gate on
   `Focus::Prompt`, return early when `spec_for_key(key, Focus::Prompt)`
   matches a dashboard chord, otherwise hand the key to
   `ChatState::handle_key`, mapping `CycleFocus` to `cycle_focus` and
   `PasteFromClipboard` to a notice, and propagate `take_render_changed` into
   `mark_render_changed`. Route `handle_paste` to `standby_prompt_mut(..)
   .paste(..)` under the same gate. Delete `type_ahead_session`,
   `transition_composer`, `seed_transition_composer`,
   `take_transition_composer_draft`.
5. `mj-tui/src/combined.rs`: delete `render_type_ahead_composer`,
   `transition_prompt_height`, and the local `prompt_content_width`. In the
   transition branch, keep the transition panel; for non-failed
   Starting/Resuming draw `draw_prompt_band` on the session's standby with the
   Alt-X cancel note; retiring/failed keep the Status panel. In the chat-less
   branch keep the Opening transcript hero but draw the standby prompt instead
   of the Opening advice panel (split `render_empty_conversation` into
   transcript-hero and prompt-advice halves). Feed `desired_prompt` from
   `ChatState::desired_prompt_height` when the standby is on screen, and
   append the standby's `frame_surfaces` to the dashboard's.
6. `mj-cli/src/dashboard/actions.rs`: `begin_lifecycle_operation` seeds
   `seed_standby_prompt`. `mj-cli/src/dashboard.rs`: `open_chat_session` takes
   the standby draft into `ComposerDraftCache` (same place as before).
   `mj-cli/src/dashboard/io.rs`: the `ChatOpened` Ok branch prefers a fresh
   standby draft (`take_standby_prompt_draft` → `with_draft`) over the cache,
   so typing during the attach wins; the terminal branches drop the standby.
7. Tests: rework the mj-tui transition-composer tests to the standby API;
   update the mj-cli Alt-X-cancel-from-composer tests; add tests that the
   standby answers readline chords, that Enter keeps the draft, that paste
   lands, and that a standby draft carried over seeds the opened chat.

## Concrete Steps

All commands run from `/workspace/hel`.

    cargo test -p mj-chat -p mj-tui -p mj-cli
    cargo clippy --all-targets -- -D warnings

Per AGENTS.md these run outside the restricted sandbox (the suite binds
loopback sockets). Build for the host; no cross targets involved.

## Validation and Acceptance

- `cargo test` green across the workspace crates touched; new tests: standby
  readline editing (Ctrl-A/Ctrl-K/Ctrl-Y/Alt-B), Enter-keeps-draft notice,
  paste, draft carry-over into the opened chat.
- `cargo clippy --all-targets -- -D warnings` clean.
- Manual: `cargo run -p mj-cli` in a workspace, launch a session, type with
  readline chords while Starting, press Enter (notice, draft kept), and see
  the draft in the composer when the session opens.

## Idempotence and Recovery

All edits are ordinary source changes; re-running the test commands is safe.
If the standby refactor must be abandoned, reverting the three crates'
commits restores the type-ahead composer, which remains functional until its
removal lands.

## Artifacts and Notes

Reference map from exploration (pre-change line numbers): transition surface
`mj-tui/src/combined.rs:759-892`; type-ahead composer `:898-956`;
`handle_type_ahead_key` `mj-tui/src/lib.rs:1347-1375`; paste routing
`:1540-1557`; key routing `:1793-1797`; carry-over
`mj-cli/src/dashboard.rs:1704-1709`; seeding
`mj-cli/src/dashboard/actions.rs:1246-1260`; launch focus
`mj-cli/src/dashboard/io.rs:2291-2297`; ChatOpened
`mj-cli/src/dashboard/io.rs:1735-1801`.

## Interfaces and Dependencies

New public mj-chat API used by the hosts:

- `ChatState::standby(session_id: &str, config: &Config, header:
  SessionHeaderIdentity, notices: Notices) -> Self`
- `ChatState::draft(&self) -> String`
- `ChatState::paste(&mut self, pasted: &str)`
- `ChatState::draw_prompt_band(&mut self, frame: &mut Frame, area: Rect,
  prompt_focused: bool, note: Option<Line<'static>>)`
- `ChatState::desired_prompt_height(&self, width: u16) -> u16`
- `ChatState::take_render_changed(&mut self) -> bool`

Removed mj-tui API: `transition_composers`, `type_ahead_session`,
`transition_composer`, `seed_transition_composer`,
`take_transition_composer_draft`, `handle_type_ahead_key`,
`render_type_ahead_composer`, `transition_prompt_height`.
