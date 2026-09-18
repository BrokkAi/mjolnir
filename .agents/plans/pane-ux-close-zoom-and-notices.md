# Pane UX: click-to-close, split-open notices, wheel and focus fidelity, zoom, last pane


This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained according to `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture


The terminal dashboard (`mj`) tiles its conversation area into panes, each showing one session's transcript and composer. The tiling landed in `.agents/plans/conversation-split-panes.md`, which this plan builds on. Using the panes exposed five gaps that this plan closes.

After this plan lands, a user can close any conversation pane by clicking a `×` chip on its title row, the way the support panes already offer clickable title chips. Asking to "open in split" a session that is already on screen no longer leaves a blank pane behind; instead the dashboard flashes a short notice and, if the session is in another pane, moves the keyboard there. Rolling the mouse wheel over an unfocused pane scrolls that pane, not the focused one. The focused pane's transcript border is drawn in the accent colour so the focused pane is visible at a glance, not just by its composer. `prefix+z` zooms the focused pane to fill the whole conversation band and toggles back; `prefix+;` returns the keyboard to the previously focused pane.

To see it working: start `mj` against the fake-harness lab (`tests/e2e/prepare-luna-lab.py`) with three sessions, split twice, then try each behaviour listed under `Validation and Acceptance`.

## Progress


- [x] M0: this ExecPlan written and committed (2026-09-18, ac500d28).
- [x] M1: close a pane by id; a `×` chip on every conversation pane's title row. Done 2026-09-18T10:48-05:00; `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all clean.
- [x] M2: split-open notices for a session that is already open. Done 2026-09-18T11:34-05:00; `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all clean.
- [x] M3: wheel routes to the pane under the pointer; focused transcript border. Done 2026-09-18T12:26-05:00; `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all clean.
- [x] M4: zoom on `prefix+z` (`pane_size` moves to `prefix+shift+z`). Done 2026-09-18T13:20-05:00; `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all clean. Docs text for zoom and the moved `pane_size` updated; screenshots still to regenerate.
- [x] M5: last pane on `prefix+;`. Done 2026-09-18T14:05-05:00; `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all clean.
- [x] Docs and screenshot regeneration for the key changes. Done 2026-09-18T14:05-05:00; `terminal-surface.mdx` and `configuration.md` cover the chips, the notices, the wheel, zoom, last pane, and the moved `pane_size`; the four captures under `docs/src/assets/screenshots/` were regenerated and the palette shows **Zoom pane** (`ctrl+b z`) and **Focus last pane** (`ctrl+b ;`).
- [x] End-to-end check against the lab, then push. Run on 2026-09-18T15:05-05:00
  against the fake-harness lab: six of the seven acceptance behaviours passed as
  written; the `⋯ → Open in split right` notice read "Already open in this
  pane" because the keyboard had already moved when the session was named. The
  wording was unified to that one notice (see the Decision Log, 2026-09-18) and
  the behaviour, a focus move plus a notice and no blank pane, is what the plan
  asks for. See `Artifacts and Notes`.

## Surprises & Discoveries


`DashboardState::close_focused_pane` was removed rather than kept as a thin
wrapper over `close_pane`. Only three unit tests called it; `mj-cli` went
straight to `close_pane(pane)`, so a wrapper would have carried its own
explaining to do for no saved churn.

A chat that splits its transcript, for a second opinion or a turn review,
divides the whole transcript rectangle in half and draws the reviewer on the
right. The close chip sits at the outer right edge, so on a split it lands on
the reviewer half's title row and the reserved columns are taken out of the
primary title instead. It costs three cells of a title that is usually short;
M1 leaves it alone rather than teaching `mj-chat` about host chips per half.

M2's "already in this pane" case is narrower than it first reads. `moving` is
true whenever the focused pane shows the session, and a row selection sets
`previous_pane_sessions` for that pane, so `displaced` is `Some` on the ordinary
selection-then-split path. The notice therefore fires only when the session was
already in the focused pane with no earlier conversation to put back — a
`⋯` menu split on the pane you are already in, or a repeated split key.

An unfocused pane's chat needs no extra state for the wheel: `mj-chat`'s
pointer handler scrolls the transcript from its fallback arm, on the host's
promise that the pointer was over the conversation, and every hit-testing arm
above it reads surfaces the pane registered in its own draw. Unfocused panes
are drawn each frame, so those surfaces and `last_viewport_height` already
describe the pane's own rectangle.

A wheel over an unfocused pane holding no conversation now falls through to
`DashboardState::handle_event_result` rather than scrolling the focused pane.
That handler answers the wheel only inside the support panes, so a wheel over
the conversation band does nothing, which is the intended result.

Moving `pane_size`'s default off `prefix+z` needs no `CONFIG_VERSION` bump and
no migration. `KeysConfig::resolve` walks the user's own fields before the
defaults, and a default that collides with a user binding is dropped rather
than reported: a user who wrote `pane_size = "prefix+z"` keeps it and simply
gets no default zoom key. `CONFIG_VERSION` stays 12. `prefix+shift+z` round
trips through `format_key_combo` unchanged, as `prefix+shift+n` and
`prefix+shift+r` already do.

The footer's key row prints the support-pane size key, so
`footer_groups_pane_keys_then_prefix_chords_in_rank_order` in
`mj-tui/src/render/tests.rs` pinned `z size` and now expects `shift+z size`.
Zoom itself has no footer word, so it does not join that row.

Only two places asked the layout for the panes to draw: the pane-width
measurement before the band allocation and the pane loop itself. Both now call
`conversation_panes`, and because `conversation_pane_areas` — what
`chat_region_contains` and `pane_bands` read — is built from the loop's result,
hit-testing follows the zoom without a separate change. `focus_pane_toward`
still reads the full layout on purpose.

`prev_focus` cannot dangle. `TileLayout::close_focused` clears it outright and
`close_pane` clears it when the pane it names is the one being closed, so
`previous_focus()` never points at a pane that has gone. `focus_last_pane_command`
still checks membership in `pane_ids()` before moving, because the guard costs
one walk of a tree with a handful of leaves and the alternative is a silent
`focus_pane` that does nothing while the command reports a move.

`format_key_combo` prints `;` for the semicolon key, not the `semicolon` alias
it accepts on the way in, so the default is written `"prefix+;"` — the same
shape as `split_horizontal = "prefix+-"`, where `minus` is likewise only an
input alias.

`cargo build --workspace` fails in this checkout because `mj-desktop` needs
GTK development packages that are not installed. The workspace's
`default-members` exclude `mj-desktop`, so the three validation commands in
this plan are unaffected: they build and test the default members only.

## Decision Log


- Decision: include all four extras (notices, wheel and border fidelity, zoom, last pane) alongside the `×` chip, and follow herdr's hotkeys where herdr has them.
  Rationale: user decision in the planning session on 2026-09-18. herdr is the tiling terminal whose keybinding conventions this dashboard already follows for pane commands.
  Date/Author: 2026-09-18, Jonathan Ellis with Fable.
- Decision: for a session already open in another pane, "open in split" flashes a notice and moves the focus.
  Rationale: user decision. Silently moving the keyboard was the confusing part; the move itself is what the user usually wanted.
  Date/Author: 2026-09-18, Jonathan Ellis with Fable.
- Decision: the split notice has one wording, "Already open in this pane", on both the focus-move arm and the nothing-to-put-back arm of `open_session_in_split`.
  Rationale: the end-to-end run showed the "another pane" arm is unreachable from the terminal. Every way to name a session (row click, the `⋯` chip, arrow keys) opens it first through `open_chat_session`, which already moves the keyboard to the pane showing it, so the split command always runs with the keyboard in the session's pane. Putting the "keyboard moved" notice inside `open_chat_session` instead would flash it on every arrow-key pass over a session shown in another pane, which is noise. The wording that is true on the real path is kept; the arm stays as a defensive case for a caller that has not opened the session.
  Date/Author: 2026-09-18, Fable.
- Decision: `pane_size` moves from `prefix+z` to `prefix+shift+z` so herdr's `prefix+z` can mean zoom.
  Rationale: herdr binds zoom to `prefix+z` by default. Keeping parity with herdr matters more than keeping the current sizing key.
  Date/Author: 2026-09-18, Fable.
- Decision: last pane defaults to `prefix+;`.
  Rationale: herdr has `last_pane` but leaves it unbound; tmux uses `prefix+;`, which is the widest-known convention.
  Date/Author: 2026-09-18, Fable.
- Decision: zoom is not persisted. A restart comes back unzoomed.
  Rationale: it needs no schema change and matches herdr, where zoom is a transient view state, not part of the layout.
  Date/Author: 2026-09-18, Fable.
- Decision: while zoomed, the other panes are not drawn at all, their chats stay warm and pumped, and their unread badges keep accruing because they are not in `drawn_sessions`.
  Rationale: herdr's behaviour. Drawing hidden panes nowhere would be wasted work, and not acknowledging their read receipts is the honest state: the user did not see them.
  Date/Author: 2026-09-18, Fable.

## Outcomes & Retrospective


All five milestones landed, and the surface now does what the Purpose asked
for. A `×` chip on every conversation pane's title row closes that pane from
the mouse, wherever the keyboard is. Asking to split out a session that is
already on screen no longer leaves a blank pane: it either moves the keyboard
to the pane that has it or says the pane you are in already shows it, with a
notice either way. The wheel scrolls the transcript it is over rather than the
focused one. The focused pane's transcript border is drawn in the accent
colour, but only when there is a second pane to tell it apart from, so the
single-pane surface is unchanged. `prefix+z` zooms the focused pane to fill the
band and back, with a `Z` chip beside the `×`; `prefix+;` returns to the
previous pane; `pane_size` moved to `prefix+shift+z` to make room.

What the work taught, beyond the entries above: the pane list is read in one
place for drawing and in one place for measuring, and every hitbox is derived
from the drawing pass, so zoom needed no separate hit-testing change. The
conversation band's state divides cleanly into what is stored (the tile tree
and which session each pane holds) and what is not (the zoom), and keeping the
zoom out of `export_conversation_layout` was enough to make a restart come back
unzoomed with no schema work. The key table's resolution order meant moving a
default binding needed no config migration, which is worth remembering the next
time a default key moves.

Two things are deliberately left: the close chip lands on a split chat's
reviewer half rather than teaching `mj-chat` about host chips per half, and the
end-to-end check against the fake-harness lab is still to run.

## Context and Orientation


The dashboard is a Rust workspace. The crates that matter here are `mj-tui` (the dashboard's model, key handling, and rendering), `mj-chat` (one conversation's transcript and composer, rendered inside a pane), `mj-cli` (the controller: it owns the terminal, the running chats, and the connection to the daemon), and `mj-core` (shared config including the key table).

A *pane* is one rectangle in the conversation band. Panes are arranged by `TileLayout` in `mj-tui/src/tile_layout.rs`, a binary split tree. Each pane has a `PaneId`. `TileLayout` tracks the focused pane (`focus`) and the pane focused before it (`prev_focus`, at `tile_layout.rs:92`). `TileLayout::close_pane(id)` (`tile_layout.rs:245`) already removes an arbitrary pane; it returns `false` for the last pane, which the caller empties instead of removing.

`DashboardState` (`mj-tui/src/lib.rs`) holds the layout plus a map from pane to session id. Pane commands live in `mj-tui/src/dashboard_conversation.rs`: `close_focused_pane` (line 110), `split_focused_pane`, `focus_pane_toward`, `restore_conversation_layout`, `reset_conversation_layout`. Commands are described by `CommandId` entries in `mj-tui/src/actions.rs`; each has a `Scope`, an `available` predicate (the pane commands use `conversation_pane_ready`), a title, and an optional `KeyAction`. Key names come from the `key_actions!` table in `mj-core/src/config/keys.rs` (`PaneSize / pane_size = "prefix+z"` is at line 383). `mj-tui/src/keybinds.rs::command_for_action` maps a `KeyAction` to its `CommandId`. The tests `every_key_action_maps_to_exactly_one_command` and the `(CommandId, "ctrl+b x")` table near `actions.rs:1571` pin the mapping and the default spellings; `format_key_combo_round_trips_every_default_binding` pins that each default spelling round-trips through the formatter.

A command that changes the panes returns `DashboardAction::ConversationPanesChanged { focus_moved }` (`mj-tui/src/lib.rs`, near line 190, next to `ClosePane`) so the controller can re-sync which chats are attached and save the layout.

*Surface controls* are the clickable chips the dashboard draws on the support panes. `mj-tui/src/surface_controls.rs` defines `SurfaceControl` (the variant `Session(usize)` is at line 18), a form of controls with `ControlKind`s, `handle_surface_mouse` (line 113) that turns a click on a control into a `DashboardAction`, and `render_session_row_actions` (line 215) that draws the chips. `component_handles_mouse` in `mj-tui/src/component_events.rs` (near line 50) is asked first whether a control claims a pointer event; only if not does the controller's `dispatch_event` (`mj-cli/src/dashboard.rs:1629`) hand it to a chat.

Rendering of the conversation band is `render_combined` in `mj-tui/src/combined.rs`. It asks the layout for `PaneInfo { id, rect, is_focused }` per pane (near line 625) and loops over the panes (lines roughly 776 to 955), drawing each one's chat, or a transition surface, or an empty pane, with the focused pane drawn last so its overlays cover the others. The empty pane draws a right-aligned opening spinner near lines 1033 to 1043. The per-pane title-row size controls for the support panes are placed by `pane_size_control_areas` in `mj-tui/src/render/sessions.rs:80`, which puts them at the pane's right edge.

Inside a pane, `mj-chat` draws the transcript through `render_transcript` (`mj-chat/src/chat/transcript/render.rs`). Its border is `theme::panel(false)` (line 11), never focused. `ChatRegions` (`mj-chat/src/chat.rs:160`) is the `Copy` struct that tells `mj-chat` where the transcript, prompt, footer, and overlay go.

On the controller side, `DashboardContext` in `mj-cli/src/dashboard.rs` owns the chats. `open_session_in_split` (line 934) takes the session out of the focused pane, splits, and refills the old pane from `previous_pane_sessions`; `close_focused_pane` (line 1001) drops the focused pane's chat and bookkeeping. `visible_chat` in `mj-cli/src/dashboard/session_state.rs:166` returns the focused pane's chat; `chat_shows_in_pane` nearby knows how to honour an attach that is still in flight (`opening_chat_sessions`). A *notice* is a one-line message in the footer set with `self.dashboard.set_notice(...)`; "Not enough room to split" is the existing pane notice. `DashboardContext` cannot be built in unit tests because it takes the terminal and talks to the daemon, so controller-side behaviour is verified end to end.

The user-facing docs live at `docs/src/content/docs/terminal-surface.mdx` ("Conversation panes" section, `prefix+z` mentioned at lines 128 and 206) and `docs/src/content/docs/configuration.md` (`[keys]` reference, lines 177 to 178). Palette and help screenshots are regenerated by an ignored test in `mj-tui`.

## Plan of Work


Each milestone is one commit. Implementation of each milestone is delegated to a faster model; Fable reviews before committing.

### M1: close a pane by id, and a `×` chip on every pane title


`DashboardAction::ClosePane` becomes `ClosePane { pane: PaneId }`. The palette and keyboard command in `actions.rs` returns it with the focused pane's id. `DashboardState::close_focused_pane` generalizes to `close_pane(pane) -> Option<String>` on top of `TileLayout::close_pane(id)`; when `pane` is not focused, focus and its history are untouched. The last pane is emptied, not removed, as today. After a close, the Sessions highlight follows the focused pane's session, as today.

`DashboardContext::close_focused_pane` generalizes to `close_pane(pane)`: remove that pane's entries from `opening_chat_sessions`, `previous_pane_sessions`, and `attachments` (all keyed by `PaneId`), detach and drop its chat, run `sync_opening_session`, save the layout.

Add `SurfaceControl::ClosePane(PaneId)` beside `Session(usize)`. A new `render_pane_close_control`, modelled on `render_session_row_actions`, draws a three-cell ` × ` chip at `Rect::new(transcript.right() - 4, transcript.y, 3, 1)`, the same right-edge geometry as `pane_size_control_areas`. Muted style at rest, `theme::selection(true)` when armed. Register it in the surface form with `ControlKind::Button` so `component_handles_mouse` claims the click before `dispatch_event` sends it to the chat. `handle_surface_mouse` maps `Activate(ClosePane(pane))` to `DashboardAction::ClosePane { pane }`.

Draw the chip in the pane loop of `render_combined` right after each pane's own draw, so the focused pane, drawn last, still covers earlier chips with any full-frame overlay. Skip the focused pane's chip while its chat has a component modal open (`ActiveChat::component_modal_open`). Show the chip on every pane when there is more than one; on a lone pane only when it holds a session, because closing it empties it, the same as `prefix+x`.

Give the title room: add `pub title_controls: u16` to `ChatRegions` (it stays `Copy`) and have `render_transcript` truncate the title to `area.width - 2 - title_controls`. `mj-tui` passes 4 when it will draw the chip. The empty pane's opening spinner shifts left by the same reserve.

Tests in `mj-tui/src/tests.rs`, the `surface_controls.rs` tests, and the `mj-chat` transcript tests: two panes draw two chips on their title rows; clicking the unfocused pane's chip yields `ClosePane { pane }` for that pane and leaves focus and highlight where they were; `close_pane` on an unfocused pane removes it without moving focus; a long transcript title truncates short of the reserve.

### M2: split-open notices for an already-open session


In `open_session_in_split`: when the session is in another pane, keep the focus move and then `set_notice("Already open in this pane")` (one wording for both arms; see the Decision Log for why the focus-move arm is not reachable from the terminal). When the session is in the focused pane and there is nothing to put back (`moving && displaced.is_none()`), `set_notice("Already open in this pane")` and return before splitting. The existing displaced path, where a row selection just pulled the session in and the previous session goes back, is unchanged. This mirrors the "Not enough room to split" notice and is verified end to end. Update the "Conversation panes" section of `terminal-surface.mdx`.

### M3: pointer and focus fidelity


Wheel: in `dispatch_event`, when `over_pane` is `Some(pane)`, that pane is not focused, and the event is `ScrollUp | ScrollDown`, route it to that pane's chat instead of `visible_chat()`. Add `pane_chat_mut(pane)` beside `visible_chat` in `session_state.rs`; it looks up `dashboard.pane_session(pane)` in `chats`, honouring an in-flight attach the way `chat_shows_in_pane` does. Clicks keep focusing first.

Focused border: add `pub pane_focused: bool` to `ChatRegions`; `render_transcript` uses `theme::panel(pane_focused)`. `mj-tui` sets it to `is_focused && pane_count() > 1`, so the single-pane surface draws exactly as before and the existing render tests hold. Apply the same rule to the `mj-tui`-drawn transcript surfaces for the focused pane: `render_empty_transcript`, `render_transition_surface`, and the launch standby.

Test: with two panes the focused pane's transcript border uses the focused style and the other does not; with one pane the border is unfocused.

### M4: zoom (`prefix+z`)


Keys: `PaneSize / pane_size` moves to `prefix+shift+z`; a new row `Zoom / zoom = "prefix+z"`. New `CommandId::ZoomPane` (`Scope::Pane`, `available: conversation_pane_ready`, title "Zoom pane") and `KeyAction::Zoom` in `command_for_action`. Extend `every_key_action_maps_to_exactly_one_command` and the default-spelling table.

State: `DashboardState.conversation_zoomed: bool`, not persisted, cleared by `restore_conversation_layout` and `reset_conversation_layout`. New `DashboardState::conversation_panes(area) -> Vec<PaneInfo>`: when zoomed and `pane_count() > 1`, a single `PaneInfo { id: focused, rect: area, is_focused: true }`; otherwise `conversation_layout.panes(area)`. `render_combined` uses it where it currently asks the layout. `focus_pane_toward` keeps using the full layout, so directional focus works against the hidden layout and stays zoomed, as in herdr.

Rules: the command toggles zoom on the focused pane; with one pane it sets the notice "Only one pane; nothing to zoom"; `split_focused_pane` and `close_pane` clear the zoom first; focus moves keep it; opening a session into the focused pane keeps it.

While zoomed the other panes are not drawn. Their chats stay warm and pumped; they are not in `drawn_sessions`, so their read receipts are not acknowledged and their Sessions rows keep unread badges. The zoomed pane's title row shows a ` Z ` chip left of `×`, registered as `SurfaceControl::Command(CommandId::ZoomPane)` so a click unzooms through `run_available_command`. The command returns `ConversationPanesChanged { focus_moved: false }` so the controller re-syncs; nothing is saved because zoom is not part of the stored layout.

Tests: zoom draws one pane filling the band; `FocusPaneRight` while zoomed moves focus and stays zoomed; split and close clear it; one pane refuses with the notice; clicking ` Z ` unzooms.

### M5: last pane (`prefix+;`)


`TileLayout::previous_focus() -> Option<PaneId>` exposes `prev_focus`. New `CommandId::FocusLastPane` ("Focus last pane", `Scope::Pane`, `conversation_pane_ready`), `KeyAction::LastPane / last_pane = "prefix+;"`. Write the spelling that `format_key_combo` prints so the round-trip test passes (`semicolon` is an accepted alias near `keys.rs:158`). `DashboardState::focus_last_pane_command()`: when a previous pane exists and differs from the focused one, `focus_pane` it and return `ConversationPanesChanged { focus_moved: true }`; otherwise set the notice "No previous pane".

Test: after focusing A then B the command returns to A; with no history it notices.

### Docs and screenshots


Update `terminal-surface.mdx` (the "Conversation panes" table and both `prefix+z` mentions) and `configuration.md` (`[keys]` reference) for the new keys, the moved `pane_size`, the `×` and `Z` chips, the notices, and the wheel behaviour. Regenerate the palette and help captures with the ignored screenshot test. The `key_actions!` table gains rows only, so `CONFIG_VERSION` stays; the split-panes ExecPlan records that precedent.

## Concrete Steps


All commands run from the repository root, `/home/jonathan/Projects/hel3`, outside the restricted sandbox, on the dev profile.

Per milestone:

    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Screenshots after the key changes:

    cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture

## Validation and Acceptance


Per milestone the three commands above pass. End to end against the fake-harness lab with three sessions: clicking `×` on an unfocused pane closes it and the keyboard stays where it was; selecting a session already shown and pressing `prefix+v` shows the notice and leaves no blank pane; `⋯ → Open in split right` on a session in another pane moves the keyboard there and shows the notice; the wheel over an unfocused pane scrolls that transcript; the focused pane's transcript border is accented; `prefix+z` fills the band with one pane and `prefix+l` moves within the zoom; `prefix+;` returns to the previous pane; a restart restores the layout unzoomed.

## Idempotence and Recovery


Every step is an ordinary source edit; re-running the checks is safe. No migrations or destructive operations. If a milestone's review fails, fix in place before its commit; each commit is independently buildable.

## Artifacts and Notes


### End-to-end run against the fake-harness lab, 2026-09-18

Build `target/debug/mj` at `686dd831`; lab from
`python3 tests/e2e/prepare-luna-lab.py --seed 7 --hel ./target/debug/mj
--fake-delay-ms 500`; isolated `MJ_CONFIG_DIR`/`MJ_DATA_DIR` under
`/tmp/hel-r-3432302-_zidk7ap`; `tmux -L pane-ux-7`, one 220x50 window. Three
sessions on profile `fake` and target `localhost` (raw localhost):
A `fe9285ad…` (later titled `P2 alpha000…`), B `3e23dff2…` (later `Q1
beta000…`), C `5360d284…`. Mouse events were injected as SGR sequences through
`tmux send-keys -H`; writing them to `#{pane_tty}` does not work, because that
is the slave side and never reaches the program's stdin.

1. Close chip on an unfocused pane — pass. Two panes, both title rows carry a
   chip (`×` at columns 145 and 218):

       ╭ localhost/project  Idle  fake  P2 alpha000 alpha001 alpha002 alpha…─ × ╮╭ localhost/project  Idle  fake  5360d284d64f1c81bcdae3c98ee58569 ─── × ╮

   A left-click on the unfocused left pane's `×` (col 145, row 1) left one pane
   holding the focused session, and the Sessions highlight did not move:

       ╭ localhost/project  Idle  fake  5360d284d64f1c81bcdae3c98ee58569 ─
       │› ○ 5360d284d64f1c81bcdae3c98ee58569

2. Split notices — mixed. "Already open in this pane" fires as designed: with
   panes `C | A` and A focused, clicking A's Sessions row and pressing
   `prefix+v` left the two panes unchanged and printed

       Already open in this pane

   The `⋯ → Open in split right` case does not print the notice the plan
   predicts. With panes `B | A`, keyboard in the left (B) pane, clicking A's
   `⋯` chip moved the accent border to the right (A) pane *before* the menu
   opened — `DashboardState::handle_surface_mouse` sets `selected_session_id`,
   and the selection is opened through `DashboardContext::open_chat_session`,
   which silently focuses the pane that already holds the session
   (`mj-cli/src/dashboard/drafts.rs:93`). Activating "Open in split right" then
   ran with the session already in the focused pane and printed

       Already open in this pane

   The user-visible outcome the Purpose asks for still holds — the keyboard
   moved to the pane that has the session, a notice appeared, and no blank pane
   was created — but the wording is the other branch's. The
   `"Already open in another pane; the keyboard moved there"` arm
   (`mj-cli/src/dashboard.rs:943`) was not reachable from the dashboard in this
   run: every route that names a session for the split (row click, `⋯` chip,
   arrow keys in Sessions) opens it first, and `focus_conversation_pane` clears
   the selection, so the focused pane and the session's pane are never
   different when the command runs.

3. Wheel over an unfocused pane — pass. Panes `B | A`, keyboard in B. Five
   `\e[<64;180;20M` wheel-up events over the right (A) pane scrolled A only:

       right, before:  │ ● Agent · 11:39
       right, after:   │ │ alpha098 alpha099 alpha100 alpha101 alpha102 …
       left,  before:  │ │ beta176 beta177 beta178 beta179 beta180 …
       left,  after:   │ │ beta176 beta177 beta178 beta179 beta180 …

4. Focused transcript border — pass. SGR at the two transcript top-left
   corners, keyboard in the left pane:

       left  corner: ['38;2;52;70;94', '1', '38;2;99;216;229']   (accent)
       right corner: ['0', '38;2;52;70;94', '48;2;17;29;45']     (resting)

5. Zoom — pass. `prefix+z` filled the band with one pane and drew the chip left
   of `×` (columns 215 and 218):

       ╭ localhost/project  Idle  fake  Q1 beta000 beta001 … ─── Z  × ╮

   `prefix+l` while zoomed showed the hidden neighbour, still zoomed:

       ╭ localhost/project  Idle  fake  P2 alpha000 alpha001 … ─── Z  × ╮

   Clicking ` Z ` (col 215, row 1) restored the two panes. On a single pane
   `prefix+z` printed `Only one pane; nothing to zoom`.

6. Last pane — pass. `prefix+;` moved the accent border back to the pane
   focused before; on a single pane it printed `No previous pane`. Note for
   whoever repeats this: `tmux send-keys` eats a bare `;`, so send it as
   `send-keys -H 3b`.

7. Restart — pass. `prefix+q` from a zoomed two-pane layout, then a fresh `mj`
   with the same lab environment, came back with two panes and no `Z` chip:

       ╭ localhost/project  Idle  fake  Q1 beta000 bet…── × ╮╭ localhost/project  Idle  fake  P2 alpha000 alpha001 alpha002 alph…─ × ╮

Lab torn down afterwards: tmux server killed and every process whose environment
named this lab's config and data directories terminated.

## Interfaces and Dependencies


In `mj-tui/src/lib.rs`:

    DashboardAction::ClosePane { pane: PaneId }

In `mj-tui/src/dashboard_conversation.rs`:

    pub fn close_pane(&mut self, pane: PaneId) -> Option<String>
    pub fn conversation_panes(&self, area: Rect) -> Vec<PaneInfo>
    pub fn zoom_pane_command(&mut self) -> Option<DashboardAction>
    pub fn focus_last_pane_command(&mut self) -> Option<DashboardAction>

In `mj-tui/src/tile_layout.rs`:

    pub fn previous_focus(&self) -> Option<PaneId>

In `mj-tui/src/surface_controls.rs`:

    SurfaceControl::ClosePane(PaneId)

In `mj-chat/src/chat.rs`, `ChatRegions` gains:

    pub title_controls: u16,
    pub pane_focused: bool,

In `mj-cli/src/dashboard/session_state.rs`:

    pub(crate) fn pane_chat_mut(&mut self, pane: PaneId) -> Option<&mut ActiveChat>

In `mj-cli/src/dashboard.rs`:

    pub(crate) fn close_pane(&mut self, pane: PaneId)

In `mj-core/src/config/keys.rs`, `key_actions!` gains `Zoom / zoom = "prefix+z"` and `LastPane / last_pane = "prefix+;"`, and `PaneSize / pane_size` becomes `"prefix+shift+z"`.
