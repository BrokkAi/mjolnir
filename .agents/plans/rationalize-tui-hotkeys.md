# Rationalize the terminal surface's hotkeys

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

The rules for this document are in `.agents/PLANS.md`, at the repository root. This document must be maintained in accordance with that file.

## Purpose / Big Picture

Hel is a terminal program (`mj`) that runs several coding agents at once and shows them all on one screen. That screen is called the terminal surface. Today, learning what its keys do means reading the source, because the only documentation on screen is one of four fixed strings printed along the bottom row, and some of what those strings say is wrong: two error messages tell the user to press `Ctrl+X` to cancel something, and `Ctrl+X` is not bound to anything at all.

After this work a person who has never used the program can learn every key from inside it. Pressing `F1` (or `?` while a pane has the keyboard) opens a scrollable list of every command the surface has, with its keys, what it does, and — when it cannot be run right now — why not. The bottom row stops being a fixed string and starts naming only the keys that apply at this moment: `Alt-X cancel launch` appears while the selected agent session is starting up and disappears when it finishes. A small set of chords works from everywhere including the text composer, so the user does not have to move the keyboard to a pane before acting. And the "edit session" dialog, which exists only because the bottom row ran out of room, goes away in favour of a searchable command list.

The proof that it works is direct: run `mj`, read the bottom row, press `F1`, read the list, press `Escape`, and see the screen you were on come back exactly as you left it.

## Progress

- [x] (2026-09-01 00:00Z) M0. Wrote this ExecPlan into `.agents/plans/rationalize-tui-hotkeys.md` from the approved design.
- [x] (2026-09-01 00:00Z) M1. Added `mj-tui/src/actions.rs` (the command registry), `mj-tui/src/help.rs` (the `F1` overlay and `Mode::Help`), rewrote `handle_dashboard_key` against the registry, made the footer contextual, fixed the two `Ctrl+X` notices, and updated `README.md`.
- [x] (2026-09-01 00:00Z) M1 tests. Seven named tests added (four in `mj-tui/src/render.rs`, three in `mj-tui/src/help.rs`) plus three registry consistency tests in `mj-tui/src/actions.rs`.
- [x] (2026-09-01 00:00Z) M1 validation. `cargo test -p brokk-mj-tui` 253 passed, `cargo test -p brokk-mjolnir` green, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied, live check in tmux captured under `Artifacts and Notes`.
- [x] (2026-09-01 00:00Z) M2. Added `global_chord()` and `DashboardState::global_chord_allowed()` to `mj-tui/src/actions.rs`, replaced `workspace_picker_event` in `mj-cli/src/dashboard.rs` with `global_chord_event` at the top of the batching loop, moved the F-keys (`F3` workspaces, `F4` web viewer, `F2` left unbound for the palette), turned `Ctrl-G`/`Ctrl-Q`/`Ctrl-R`/`Ctrl-T`/`Ctrl-X` into `Alt-G`/`Alt-Q`/`Alt-R`/`Alt-T`/`Alt-X`, added `Alt-N` and `Alt-A` from anywhere, extracted `ActiveChat::detach()`, and updated `README.md`.
- [x] (2026-09-01 00:00Z) M2 tests. Twelve named tests added (eight in `mj-cli/src/dashboard.rs`, three in `mj-tui/src/lib.rs`, three in the root crate) plus one regression test for the empty-grid crash the move uncovered; the footer- and key-asserting tests in `render.rs`, `resume.rs`, `active.rs`, `history.rs`, `lib.rs`, and `termination_pty.rs` were updated to the new map.
- [x] (2026-09-01 00:00Z) M2 validation. `cargo test -p brokk-mj-tui` 256 passed, `cargo test -p brokk-mjolnir` green (including the PTY detach test, now driven by `Alt-Q`), `cargo test --lib hel_chat` 249 passed, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied, live check in tmux captured under `Artifacts and Notes`.
- [x] (2026-09-01 00:00Z) M3. Added `mj-tui/src/palette.rs` (`Mode::Palette`, `begin_palette`, `handle_palette_key`, `palette_entries`, `render_palette`), added `CommandId::Palette` on `F2` as a global chord, deleted `SessionEditDialog` with its `begin_session_edit`/`handle_session_edit_key`/`render_session_edit` and `Mode::SessionEdit`, unbound `e`, and updated `README.md`.
- [x] (2026-09-01 00:00Z) M3 tests. Nine named tests added in `mj-tui/src/palette.rs`; the M1 help test now asserts the palette entry and its key; the key-driven fixtures in `mj-tui/src/dialogs.rs` and the three tests in `mj-tui/src/lib.rs` and `mj-tui/src/render.rs` that pressed `e` now go through the palette.
- [x] (2026-09-01 00:00Z) M3 validation. `cargo test -p brokk-mj-tui` 265 passed, `cargo test -p brokk-mjolnir` green, `cargo test --lib hel_chat` 249 passed, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied, live check in tmux captured under `Artifacts and Notes`.
- [x] (2026-09-01 00:00Z) M3.5. Made resume a global chord (`Alt-S`, `Scope::Global`), deleted the plain-letter aliases `n`, `a`, and `s` so plain letters are pane-local only, gave `CommandSpec` a `footer_group` and `footer_rank`, rebuilt `combined_footer_text` as three groups separated by ` │ `, shortened the footer words (`read`, `palette`), rewrote the chat's four footer strings in the same shape, fixed the resume dialog's unreachable tab arrows, and updated `README.md`.
- [x] (2026-09-01 00:00Z) M3.5 tests. `sessions_pane_n_and_a_still_work_as_aliases` became `plain_n_a_and_s_no_longer_act`; three tests added (`footer_groups_pane_alt_and_function_keys_in_that_order` and `footer_drops_pane_hints_before_alt_hints_and_never_function_keys` in `mj-tui/src/render.rs`, `arrow_keys_switch_tabs_even_while_the_search_field_has_focus` in `mj-tui/src/resume.rs`) plus `alt_s_opens_the_resume_dialog_from_the_composer` beside the M2 chord tests in `mj-cli/src/dashboard.rs`; the footer- and key-asserting tests in `render.rs`, `resume.rs`, `active.rs`, `help.rs`, `lib.rs`, and `wizards/tests.rs` moved to the new map.
- [x] (2026-09-01 00:00Z) M3.5 validation. `cargo test -p brokk-mj-tui` 268 passed, `cargo test -p brokk-mjolnir` green, `cargo test --lib hel_chat` 249 passed, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied, live check in tmux captured under `Artifacts and Notes`.
- [x] (2026-09-01 00:00Z) M3.6. Replaced `CommandId::RefreshCapacity` and `CommandId::RefreshQuotas` with one global `CommandId::Refresh` on `F5`, collapsed `DashboardAction::RefreshQuotas`/`RefreshCapacity` into `DashboardAction::RefreshAll`, removed the plain `r` bindings, reworded detach (`Alt-Q detach`, "Detach from this terminal"), moved history search back to `Ctrl-R`, gave the chat footer the dashboard's width rule, and updated `README.md`.
- [x] (2026-09-01 00:00Z) M3.6 tests. `plain_r_no_longer_refreshes` added in `mj-tui/src/lib.rs` and `f5_refreshes_targets_and_quotas_from_the_composer` beside the M2 chord tests in `mj-cli/src/dashboard.rs`; `pane_actions_follow_the_focused_pane` now proves the panes' `Enter` rather than their `r`; `alt_r_opens_history_search_and_alt_r_inside_it_cycles_scope` became `ctrl_r_opens_history_search_and_alt_r_inside_it_cycles_scope`; the footer- and key-asserting tests in `render.rs`, `active.rs`, `history.rs`, and `hel_chat.rs` moved to the new map.
- [x] (2026-09-01 00:00Z) M3.6 validation. `cargo test -p brokk-mj-tui` 269 passed, `cargo test -p brokk-mjolnir` green, `cargo test --lib hel_chat` 249 passed, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt` applied, live check in tmux captured under `Artifacts and Notes`.
- [ ] M4 (optional, to be proposed separately after M3). One shared vocabulary of built-in slash commands for the terminal surface and the web viewer.

## Surprises & Discoveries

- Observation: the footer is drawn from two places, not one. `mj-tui/src/render.rs:61` draws it for the plain dashboard and `mj-tui/src/combined.rs:452` draws it for the combined surface, and the second one skips it entirely when the chat has already drawn its own footer. Both had to be given the row's width for the width-aware footer to work.
  Evidence: `grep -rn "render_footer" mj-tui/src` returns exactly those two call sites plus the definition.

- Observation: several of the keys the old `handle_dashboard_key` matched were already unreachable, because `handle_key_at` answers them first. `Ctrl-Q`, `Ctrl-C`, and `F2` are all caught in `handle_key_at` (`mj-tui/src/lib.rs`, around the "Workspaces moved off Ctrl-W" comment) before the mode is dispatched at all, so the arms for them further down never ran. They are in the registry anyway, because the footer and the help overlay have to name them.
  Evidence: `handle_key_at` returns `DashboardAction::QuitDetach` for `Ctrl-Q` and `DashboardAction::OpenWorkspacePicker` for `F2` before it reaches `match self.mode.clone()`.

- Observation: `SessionOperationKind::label()` returns capitalized words ("Launch", "Stopping"), which read badly inside a footer sentence. The footer lowercases them so the hint reads `x cancel launch` rather than `x cancel Launch`.
  Evidence: `mj-tui/src/lib.rs`, `impl SessionOperationKind { pub const fn label(self) }`.

- Observation: `ChatAction::QuitDetach` has a second producer, the `/detach`
  slash command (`src/hel_chat.rs`, `LocalCommand::Detach`), so it and
  `ChatEventOutcome::QuitDetach` had to stay. Only `CyclePaneLayout` and
  `OpenWebDialog` were unproduced once the composer's escape hatches went, and
  only those two were deleted.
  Evidence: `grep -rn "ChatAction::QuitDetach" src/` returns the `/detach` arm
  as well as the key handler.

- Observation: the dead wizard `F2` arms were dead for the reason the design
  guessed, but `CompleteMountSource` is very much alive. `Tab` on the mount
  source field calls the same completion helper two lines above each `F2` arm,
  so the arms could go while the action stayed.
  Evidence: `mj-tui/src/wizards/dashboard.rs`, the `KeyCode::Tab if
  wizard.mounts.focus == MountFocus::Source` arm.

- Observation: turning the pane dial on a workspace with no sessions at all
  crashed the program, and always had. `render.rs` built the "more sessions
  than fit" marker with `then_some(viewport_end - 1)`, whose argument is
  evaluated before the guard is read, so an empty grid underflowed. Moving the
  dial off `Ctrl-G` is what made a first-run user likely to press it. Fixed
  with `then(|| …)` and covered by `the_minimized_grid_draws_with_no_sessions`.
  Evidence: `attempt to subtract with overflow` at `mj-tui/src/render.rs:705`
  from a live run against an empty scratch workspace.

- Observation: `spec_for_key` refused every chord while the composer had
  focus, because its Prompt guard tested for CONTROL — the modifier the old
  bindings used. With the bindings on Alt the guard had to test for ALT
  instead, or a second `Alt-G` after the first (which lands the keyboard in
  Prompt) did nothing.
  Evidence: `alt_g_cycles_the_pane_layout_and_ctrl_g_no_longer_does` failed on
  its second press before the guard was changed.

- Observation: two notices still told the user to press keys that no longer
  exist. `reject_selected_operation` and `open_selected_session` both said
  "press x to cancel it" — the wording M1 corrected, which M2 then invalidated
  by moving cancel to `Alt-X` — and the controller's first-run notice said
  "Press Ctrl+N to start your first session", a key that has never been bound
  in 2.x. Both are fixed here.
  Evidence: `grep -rn "to cancel it" mj-tui/src` and the live first-run capture
  under `Artifacts and Notes`, which printed the `Ctrl+N` advice on screen.

- Observation: `reject_selected_operation` had exactly one caller,
  `begin_session_edit`, so deleting the edit dialog left it dead. The gate it
  performed now lives in the registry as `session_idle`, which is what greys
  Rename, Container settings, and Stop in the palette and in the help overlay.
  Evidence: `cargo build -p brokk-mj-tui` reported it as never used once the
  dialog was gone.

- Observation: `truncate_text` (`mj-tui/src/widgets.rs`) collapses runs of
  spaces, so using it on a padded row destroys the padding. The palette's key
  column came out as `› r Refresh capacity` rather than aligned. The palette
  clips with its own `clip` instead.
  Evidence: the first live capture of the palette, before the fix.

- Observation: the resume dialog's footer has always advertised `←/→ tabs`,
  and from the search field those arrows have never switched tabs. The
  `_ if typing => { dialog.search.handle_key(key) }` catch-all in
  `handle_resume_dialog_key` sat above the `KeyCode::Left`/`KeyCode::Right`
  arms, so while the search field had focus — which is where `/` and any typed
  query leave it — the arrows moved the text cursor instead. The tab-switch
  arms moved above the catch-all; an in-field cursor is not worth a tab the
  footer promises and the surface does not give. Covered by
  `arrow_keys_switch_tabs_even_while_the_search_field_has_focus`.
  Evidence: reported from a live run, then reproduced in the source order of
  `mj-tui/src/resume.rs`, and confirmed fixed in the M3.5 live check.

- Observation: the chat's footer is not width-aware, so at 100 columns its row
  is clipped rather than shortened. Grouping moved `Alt-G panes` to the right
  of the composer's own keys, which makes the clipping easier to notice, but
  the row was already longer than 100 columns before this milestone. Giving
  the chat footer the dashboard's dropping rule is a candidate for M4.
  Evidence: `render_chat_footer` draws a `Paragraph` with no width argument;
  the two chat tests that asserted `Alt-G panes` at 100 columns now assert a
  hint from the first group instead.

- Observation: the chat's footer really did overflow once `F5 refresh` was
  added: at 200 columns the row lost `F1 help` entirely, which is the hint the
  M3.5 note said must never be dropped. The chat footer now has the
  dashboard's rule, as a small `fit_footer` in `src/hel_chat/active.rs` — whole
  hints go from the composer's group first, then from the chords, never from
  the function keys. It is a copy rather than a share, because `mj-tui` depends
  on the root crate and not the other way round, so `combined_footer_text`'s
  loop is unreachable from there; M4's shared vocabulary is where the two meet.
  Evidence: `chat_footer_advertises_alt_keys_and_f1` failed at its 200-column
  `TestBackend` with the row clipped after `F5 refresh `.

## Decision Log

- Decision: the registry lives in `mj-tui` as a crate-private module (`mj-tui/src/actions.rs`), not in a new workspace crate.
  Rationale: whether a command applies is a question about `DashboardState`, which lives in `mj-tui`. A new crate would need the state type moved or a trait invented for no gain, and the repository guidelines say not to create a workspace crate purely to reorganize code.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: M1 leaves every existing key binding alone. `F2` still opens the workspace picker, `F3` still opens the web viewer, `Ctrl-G` still turns the pane dial, `x` still cancels, `e` still opens the session edit dialog. Only `F1` and `?` are new.
  Rationale: M1 is the plumbing change — one table instead of scattered `match` arms — and it is much easier to be sure the plumbing is right when the observable behaviour is unchanged apart from the two additions. The rebinding lands in M2, where it is the only thing changing.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: `CommandId` has no `Palette` entry in M1, even though the design's enum listed one.
  Rationale: the help overlay lists every entry in `COMMANDS`, and one of M1's tests asserts exactly that. An entry for a palette that does not exist yet would put a line in the help screen advertising a command that does nothing. `Palette` joins the enum in M3, together with the thing it opens.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: `global_chord()` is not added in M1.
  Rationale: nothing calls it until the pre-filter in M2 exists, and `cargo clippy --all-targets -- -D warnings` (which this repository requires) rejects an uncalled private function. It arrives in M2 alongside its caller.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: the digit keys `1` through `9`, which fold a project by its number, stay as a hand-written arm rather than becoming a registry command.
  Rationale: `dispatch_command(id)` takes no argument, so a registry command cannot carry "which digit". Giving it one for a single caller would complicate every other command. The `ToggleProject` entry covers `Space` and says in its description that `1` to `9` fold by number, so the help screen still tells the truth.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: `Scope::Setup` is excluded from key matching in `spec_for_key`.
  Rationale: its key is `e`, which the Sessions, Targets, and Quota panes also use. The surface has always resolved this by checking for an empty configuration first, and that check stays where it was, in `handle_dashboard_key`, calling `dispatch_command(CommandId::OpenConfig)`. Putting the ambiguity in `spec_for_key` would mean giving it the dashboard, which its signature deliberately does not take.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: the help overlay lists commands that are unavailable, greyed, rather than hiding them.
  Rationale: a reference that hides what does not apply leaves the reader unable to tell "this key does not exist" from "this key does not work here". Greying with the reason answers both.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: the footer's `Enter` hints for the Targets and Quota panes lost the `/e` they used to carry ("Enter/e actions" became "Enter actions").
  Rationale: the footer prints one key per command — the first in its list — so the format stays uniform as commands are added. Both keys are still bound and both are listed in the help overlay. `e` is removed entirely in M3 anyway.
  Date/Author: 2026-09-01, implementation of M1.

- Decision: `global_chord()` matches against the registry's own `keys` lists
  rather than a second hand-written key table. `GLOBAL_CHORDS` names only the
  ids that answer from everywhere, and `KeyHint::is_chord()` picks out the
  entry — a function key or an Alt letter — that carries the chord.
  Rationale: a separate table would be exactly the drift M1 set out to remove.
  This way the footer label, the help overlay line, and the chord are one fact.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: `CommandId`, `global_chord`, `dispatch_command`, and
  `global_chord_allowed` became `pub` and are re-exported from `mj-tui`.
  Rationale: the pre-filter lives in `mj-cli`, a separate crate, so the
  crate-private forms M1 used were unreachable there. The rest of the registry
  — `CommandSpec`, `Availability`, `Scope`, `spec_for_key`, `available` — stays
  crate-private.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: `dispatch_command(CommandId::Help)` toggles rather than only
  opening.
  Rationale: the pre-filter catches `F1` before the help overlay's own key
  handler can see it, so without a toggle here `F1` would open the reference
  and then have no way to close it. `Esc` and `?` still close it through
  `handle_help_key`.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: `Alt-R` and `Alt-T` sit in the composer's Alt block, below the
  open-reverse-search check, not beside `Alt-V` above it.
  Rationale: an open reverse-i-search takes every key first, which is what
  lets `Alt-R` keep its older job of cycling the search's scope
  (`src/hel_chat/history.rs`). Putting the opener above that check would have
  made the chord open a search it was already inside.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: `Ctrl-R` inside an open reverse-i-search still steps to the
  previous match, even though `Ctrl-R` no longer opens one.
  Rationale: that binding is readline's, not the surface's, and the search
  prompt is a readline context. `Alt-R` is the only key that changed hands.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: `Alt-X` inside the target-actions dialog is handled by the dialog,
  not by the pre-filter, and `global_chord_allowed` returns false for
  `CancelOperation` whenever any modal is open.
  Rationale: one rule ("cancel belongs to whatever is in front of you") covers
  both cases, so no dialog has to be named in the pre-filter.
  Date/Author: 2026-09-01, implementation of M2.

- Decision: the palette lists the Sessions pane's group when the composer has
  the keyboard, rather than no pane group at all.
  Rationale: the composer is not a pane, but every Sessions-pane command
  answers from it as a chord (`Alt-N`, `Alt-A`). Listing nothing would hide
  commands that do work from where the user is standing.
  Date/Author: 2026-09-01, implementation of M3.

- Decision: there is no separate "selected session" notion for the composer.
  Rationale: the conversation on screen already follows
  `selected_session_id`, and `open_selected_session` sets it before handing the
  keyboard to the prompt. `selected_session()` is therefore already "the open
  chat's session" whenever the composer has focus, and inventing a second
  notion would give the surface two answers to one question.
  Date/Author: 2026-09-01, implementation of M3.

- Decision: `CommandId::Palette` carries the footer word `commands`, so the
  footer ends `… · F2 commands · F1 help`.
  Rationale: the palette is now the only way to reach rename, container
  settings, and stop. The footer lost `e edit` in this milestone, and a
  discovery path that is not named anywhere is not a discovery path.
  Date/Author: 2026-09-01, implementation of M3.

- Decision: Enter on a `Blocked` entry sets the reason as a notice and leaves
  the palette open, while `Hidden` entries are not listed at all.
  Rationale: "not now" and "not ever, here" are different answers. A blocked
  command is worth showing with its reason and worth letting the user pick
  something else without pressing `F2` again; a hidden one would only be noise
  (container settings for a session that is not on a container).
  Date/Author: 2026-09-01, implementation of M3.

- Decision: the ranking rule is copied into `mj-tui/src/palette.rs` as a
  private `rank`, not shared with `matching_indices` in
  `src/hel_chat/autocomplete.rs`.
  Rationale: the two live in different crates with no common home for it yet.
  M4 proposes that home, and moving it there is one of M4's steps.
  Date/Author: 2026-09-01, implementation of M3.

- Decision: resume moved to `Alt-S` in `Scope::Global`, and the plain-letter
  aliases `n`, `a`, and `s` were removed rather than kept. The rule is now that
  a plain letter is pane-local only; everything reachable from anywhere is a
  chord and has exactly one spelling.
  Rationale: M2 left two spellings for three commands, which is the drift this
  plan exists to remove: the footer named one, the help overlay named both, and
  the palette named both. One spelling means the footer, the reference, and the
  palette agree without a rule about which alias to print. Resume had no chord
  at all, so it could not be reached from the composer; `Alt-S` is allowed
  under the same condition as `Alt-N` — no modal open, or over the help
  overlay. The resume dialog's own `a` and `s` are modal-local and untouched.
  Date/Author: 2026-09-01, implementation of M3.5.

- Decision: the footer is three groups separated by ` │ ` — the focused pane's
  commands, the `Alt` chords in a fixed order, then the function keys — and a
  command's group and order come from `footer_group` and `footer_rank` on its
  spec rather than from where it sits in `COMMANDS`.
  Rationale: a row whose contents reshuffle as the pane changes is read from
  the start every time. With three groups the reader looks in one place for a
  given kind of key. Registry order cannot express this, because `Tab` and
  `Alt-G` share `Scope::Pane` and belong in different groups, so the placement
  had to be stated per command. Narrow terminals drop the pane group first,
  then the chords, and never the function keys, because those are the way to
  the palette and the key reference — the only things that can name what the
  narrow row had to leave out.
  Date/Author: 2026-09-01, implementation of M3.5.

- Decision: refreshing is one global key, `F5`, rather than a plain `r` on
  each of the Targets and Quota panes. `CommandId::Refresh` replaces
  `RefreshCapacity` and `RefreshQuotas`, and it refreshes both.
  Rationale: the two panes each refreshed only themselves, so a user who
  wanted current figures had to focus each pane in turn, and neither key was
  reachable from the composer. One key from anywhere is fewer keys and more
  reach. It also empties the plain-letter set down to what the Sessions list
  needs — `Enter`, `Space`, `1`–`9` — plus `?`, `e`, and the list navigation,
  which is as close to "plain letters are pane-local" as the surface gets.
  `global_chord_allowed(Refresh)` is unconditionally true: asking the daemon
  for fresh capacity and quota figures cannot disturb a dialog on screen.
  Date/Author: 2026-09-01, implementation of M3.6.

- Decision: `DashboardAction::RefreshQuotas` and `DashboardAction::RefreshCapacity`
  were collapsed into one `DashboardAction::RefreshAll` rather than kept
  beside it.
  Rationale: the registry was their only producer, so keeping them would have
  left two variants no key can reach — exactly the unowned code this plan
  exists to remove. `RefreshAll`'s arm in `mj-cli/src/dashboard/actions.rs` is
  the two old bodies run in order, and the notice it sets was reworded from
  "Refreshing profile quotas…" to "Refreshing targets and quotas…" so the row
  says what actually happened.
  Date/Author: 2026-09-01, implementation of M3.6.

- Decision: `Alt-Q` is described as detaching this terminal client, not as
  quitting. The footer word is `detach`, the label "Detach from this terminal",
  and the description says the daemon and its sessions keep running.
  Rationale: nothing stops when the terminal client goes away, and a key
  labelled "quit" invites the reader to believe their agents stopped with it.
  The same wording now runs through the footer, the help overlay, the palette,
  the chat's own footer row, and the README.
  Date/Author: 2026-09-01, implementation of M3.6.

- Decision: prompt-history search returns to `Ctrl-R`, reversing the M2 move to
  `Alt-R`. Inside an open search, `Alt-R` still cycles the scope and `Ctrl-R`
  still steps to the previous match.
  Rationale: readline keys are exempt from the Ctrl-to-Alt move, and `Ctrl-R`
  is the universal reverse-search key: a shell user reaches for it without
  reading anything. M2's own decision log already granted the exemption to
  `Ctrl-R` *inside* the search ("that binding is readline's, not the
  surface's"); the same argument applies to the key that opens it. `Alt-T`
  keeps the rendering toggle, which is not a readline key.
  Date/Author: 2026-09-01, implementation of M3.6.

## Outcomes & Retrospective

M1 is complete and shipped green. What exists now that did not before: one table (`mj-tui/src/actions.rs`) that key handling, the footer, and the help overlay all read; an `F1` key reference that opens over anything and restores it; and a footer that names only the keys that apply. The user-visible gain from M1 alone is real but modest — the mislabelled `Ctrl+X` advice is gone, `x cancel launch` only appears when it can be used, and `F1` finally answers.

The larger gain is what M1 makes cheap. M2 rebinds a dozen keys; because every binding is now one line in one table, that milestone touches the table and the pre-filter rather than a dozen scattered `match` arms and four hand-written footer strings. M3's command palette is essentially a renderer over `available()`.

M2 is complete and shipped green. Every key in the design's table has moved,
and the chords answer from the composer as well as from the panes: the pane
letters `n`, `a`, and `s` still work where they always did, and everything else
is reachable without leaving the prompt. The one-release notices carry the two
chords with the strongest muscle memory, `Ctrl-G` and `Ctrl-Q`.

M2 cost far less than it would have before M1, which was the point: the
rebinding itself was fifteen lines of the registry table, and the pre-filter is
twelve lines in the controller. The work that remained was in the tests and the
documentation that name the keys — which is a fair measure of how well the M1
tests were pinning the surface down.

The lesson worth carrying: the old code was not wrong so much as unowned. The footer strings and the `match` arms were each individually correct when written and drifted apart afterwards, because nothing forced them to agree. The test `every_footer_hint_dispatches_the_command_it_names` is the thing that stops that happening again — it presses every key the footer advertises and asserts the surface lands where the registry says it should.

M3 is complete and shipped green. The session edit dialog is gone, `e` is
unbound, and everything that dialog offered is in the palette on `F2`, which
also reaches every other command the surface has. The palette is what the
design said it would be — a renderer over `available()` plus a query — and it
cost about three hundred lines, most of them drawing.

The two bugs M3 turned up were both of the kind this plan exists to remove: a
notice naming a key that moved (`press x to cancel it`, correct in M1, wrong
from M2 onward) and a notice naming a key that never existed in 2.x
(`Press Ctrl+N to start your first session`). Neither is reachable from the
registry, because both are free text in a `set_notice` call. That is the
remaining seam: the registry now owns the keys the footer, the help overlay,
and the palette name, but not the keys prose names. Deriving those is a
candidate for M4 alongside the shared command vocabulary.

## Context and Orientation

The repository is a Rust workspace. Three parts matter here.

`mj-tui` is a library crate holding the state and input handling for the terminal surface. Its central type is `DashboardState` in `mj-tui/src/lib.rs`. That type holds the configuration, the live sessions, which pane has the keyboard (`Focus`), and which dialog if any is open (`Mode`). It performs no input and output of its own: `handle_key` turns a key press into a `DashboardAction`, a plain data value the controller then acts on. This separation is why the whole surface can be unit-tested without a terminal.

`mj-cli` is the binary crate. `mj-cli/src/dashboard.rs` runs the event loop that reads terminal events, hands them to `DashboardState` or to the chat, and performs whatever `DashboardAction` comes back.

The root crate (`src/`) holds the conversation view, called the chat or the composer. `src/hel_chat.rs` and `src/hel_chat/active.rs` own its keys and its own footer row. The composer is a separate keyboard focus from the panes; when it has the keyboard it consumes almost every letter as text, which is why the panes can use plain letters and the composer cannot.

Some terms, defined once:

A **pane** is one of the three list regions of the surface: Sessions (the running agents), Targets (the machines and containers they run on), and Quota (each agent profile's remaining allowance). `Focus` names which of these, or the composer (`Focus::Prompt`), currently owns the keyboard.

A **mode** is what dialog is open. `Mode::Dashboard` means none. `DashboardState::modal_open()` is simply "the mode is not `Dashboard`", and the controller uses it to decide whether a key belongs to a dialog or to the surface underneath.

An **operation** is a long-running thing happening to one session — starting it, resuming it, stopping it, importing it. While one is in flight the session cannot be renamed or opened, and the operation can be cancelled. `DashboardState::session_operation_kind(session_id)` reports whether one is running and which.

The **footer** is the bottom row of the screen. It shows a notice when one is set, and otherwise the hotkey hints. Before this work the hints were one of four `&'static str` values chosen by `Focus`, in `combined_footer_text` in `mj-tui/src/render.rs`.

The **accelerator** is Command on macOS and Control everywhere else. `dashboard_accelerator(modifiers)` in `mj-tui/src/lib.rs` is the one place that knows this, and every "Ctrl-something" binding goes through it.

## Plan of Work

The work is five milestones after this plan itself. Each ships with the test suite green and is committed on its own.

### M0 — this document

Write `.agents/plans/rationalize-tui-hotkeys.md` following `.agents/PLANS.md`, carrying the approved design's decisions, architecture, milestones, and risks in a form a reader with no prior context can follow. Keep it updated as each milestone lands. Nothing else changes.

### M1 — the registry, the footer, and the help overlay

This milestone changes no existing key binding. It replaces the machinery behind them and adds `F1` and `?`.

Create `mj-tui/src/actions.rs`. It defines `CommandId` (one value per thing the surface can do), `Scope` (where a command belongs: `Global`, `Pane`, `Sessions`, `Session`, `Targets`, `Quota`, `Setup`), `Availability` (`Ready`, `Hidden`, or `Blocked` carrying the sentence explaining why), `KeyHint` (a key code, its modifiers, and the text used to name it on screen), and `CommandSpec`, which pairs a `CommandId` with a label, a description, a scope, its keys, a function producing the footer word, and a function reporting availability. `COMMANDS` is the static table of every command. `spec(id)` looks one up. `spec_for_key(key, focus)` answers "what does this key press mean here". `available(dashboard, scope_filter)` lists what can be run right now. `DashboardState::dispatch_command(id)` runs one, by calling the same entry points the key handler used to call directly.

The availability functions reuse gates that already exist rather than inventing new ones: `session_operation_kind` (`mj-tui/src/ingest.rs`), the check inside `reject_selected_operation` (`mj-tui/src/lib.rs`), `selected_container_session` and `config_is_empty` (both `mj-tui/src/lib.rs`), and the guard at the top of `begin_new` in `mj-tui/src/wizards/dashboard.rs`.

Rewrite `handle_dashboard_key` in `mj-tui/src/lib.rs` to look every command up in the registry, and delete `handle_sessions_key` entirely, its work now being done by the table. What stays as hand-written arms is only the input that is not a named command: `Ctrl-C` as quit's second key, `Shift-Tab` as the reverse of `Tab`, `Escape` doing nothing on a pane, list navigation (`Up`/`Down`/`j`/`k`/`Ctrl-P`/`Ctrl-N`/`Home`/`End`), the `e`-when-the-configuration-is-empty path, and the digit keys.

Create `mj-tui/src/help.rs`. It adds `Mode::Help(HelpOverlay { scroll, return_to })`, where `return_to` is a boxed `Mode`. `begin_help` moves the current mode into `return_to`; closing puts it back directly rather than through `cancel_modal`, which would discard a half-filled wizard. `help_lines(dashboard)` builds the list from `COMMANDS` grouped by scope, greying anything not `Ready`, and appends a static section for the composer's own keys. `render_help` draws it as a centered modal like every other dialog.

Change `combined_footer_text` in `mj-tui/src/render.rs` to take the dashboard and the row's width and return a `String` built from `available()`. Each ready command with a footer word contributes `"{first key label} {word}"`; the segments join with a middle dot and the row always ends with `F1 help`. When the text is too wide, whole segments are dropped from the right — never truncated part-way, because half a hint names a key that does not exist.

Fix the two notices in `mj-tui/src/lib.rs` that say "press Ctrl+X to cancel it" to say "press x to cancel it", which is the key that is actually bound.

Update the "The terminal surface" section of `README.md` to mention `F1` and `?`, and to say the footer shows only the keys that apply now.

### M2 — global chords

Generalize `workspace_picker_event` in `mj-cli/src/dashboard.rs` into a `global_chord_event` that runs at the top of the event-batching loop, before the event is routed to a pane or to the chat, so a chord answers even while the composer has the keyboard. `DashboardState::global_chord_allowed(id)` decides which chords survive an open dialog: help always, quit always, the rest only when no dialog is open.

Then move the keys. `F2` becomes the command palette, `F3` the workspace picker, `F4` the web viewer. `Ctrl-G`, `Ctrl-Q`, `Ctrl-R`, `Ctrl-T`, and `Ctrl-X` become `Alt-G`, `Alt-Q`, `Alt-R`, `Alt-T`, and `Alt-X`, freeing those Control letters for the composer's readline editing. `Alt-N` and `Alt-A` create a session and mark everything read from anywhere. `Ctrl-C` still quits outside a text field. The old Control chords are dropped rather than aliased, with a one-release notice telling the user where each went; keeping both sets would defeat the point.

`Alt-Q` caught by the pre-filter must still run the bookkeeping the chat does when it detaches — saving the draft and the read cursor, cancelling dictation — so that body moves out of the `ChatAction::QuitDetach` arm into `ActiveChat::detach()` and the pre-filter calls it.

### M3 — the command palette

Create `mj-tui/src/palette.rs` with `Mode::Palette`, a searchable list of every available command, opened by `F2`. It lists the selected session's commands first under a heading naming that session, then the focused pane's, then the ones that apply anywhere. Unavailable commands are greyed with their reason. Selecting one returns to the dashboard and calls `dispatch_command`, so rename, container settings, and stop all work through the same path the keyboard uses.

With the palette in place, delete the session edit dialog (`SessionEditDialog` and its `begin_session_edit`, `handle_session_edit_key`, and `render_session_edit` in `mj-tui/src/dialogs.rs`, and `Mode::SessionEdit`) and unbind `e`. The dialog only ever existed because the footer had no room for three more hints.

### M3.5 — one spelling per command, and a footer in three groups

Two things the earlier milestones left half-done.

First, the plain-letter aliases. `Alt-N` and `Alt-A` arrived in M2 beside the
letters `n` and `a` they duplicated, and resume kept only the letter `s` and so
could not be reached from the composer at all. Resume becomes `Alt-S` in
`Scope::Global`, the three plain letters go, and the rule becomes: a plain
letter is pane-local only. What is left on the panes is `Enter` (open a
session, or a target's or profile's actions), `Space` and `1`–`9` (fold a
project), `r` (refresh, removed in M3.6), and `?` (help). The resume dialog's own `a` and `s`
are modal-local and stay exactly as they are.

Second, the footer. It becomes three groups separated by ` │ `: the focused
pane's commands, the `Alt` chords in a fixed order (`Alt-N`, `Alt-S`, `Alt-A`,
`Alt-G`, `Alt-X` while something is in flight, `Alt-Q`), then the function keys
(`F2`, `F3`, `F4`, `F1`). `CommandSpec` gains `footer_group` and `footer_rank`
so the placement is stated rather than inherited from the table's order, and
the footer words shorten (`read`, `palette`). Width squeezes drop the pane
group first, then the chords, and never the function keys. The chat's own
footer strings take the same shape.

### M3.6 — one refresh key, and the detach wording

Two user decisions taken after M3.5 shipped.

First, refreshing. The plain `r` on the Targets pane and the plain `r` on the
Quota pane become one global function key, `F5`, that refreshes both from
anywhere including the composer. `CommandId::RefreshCapacity` and
`CommandId::RefreshQuotas` become one `CommandId::Refresh` in `Scope::Global`
with `FooterGroup::Function`, placed before `F1`, so the function group reads
`F2 palette · F3 workspaces · F4 web · F5 refresh · F1 help`. It joins
`GLOBAL_CHORDS` and is allowed over an open dialog, because asking for fresh
figures disturbs nothing. `DashboardAction::RefreshQuotas` and
`DashboardAction::RefreshCapacity` collapse into `RefreshAll`, whose arm in
`mj-cli/src/dashboard/actions.rs` runs both requests. `Enter` on Targets and
Quota is untouched.

Second, the wording. `Ctrl-C` and `Alt-Q` detach this terminal client; the
daemon and its workers keep running. The footer word for `CommandId::QuitDetach`
becomes `detach`, its label "Detach from this terminal", and its description
says so.

Third, prompt-history search returns to `Ctrl-R`, reversing M2 for that one
key: readline keys are exempt from the Ctrl-to-Alt move. `Alt-R` inside an
open search still cycles the scope, and `Alt-T` keeps the rendering toggle.

### M4 — shared command vocabulary (optional, propose separately)

The 1.x shared slash-command registry was deleted in the 2.0 cutover, and the terminal surface's list (`builtin_command_choices` in `src/hel_chat/autocomplete.rs`) and the web viewer's list (`phone_commands` in `mj-cli/src/server.rs`) have already drifted apart. Deriving both from one list, with one gate for the fast and plan modes, would stop that. Propose it on its own after M3.

## Concrete Steps

Work from the repository root, `/home/jonathan/Projects/hel`.

Build and test:

    cargo test -p brokk-mj-tui
    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt

Run every `cargo` command outside the sandbox with elevated permissions. The suite opens loopback TCP and Unix sockets; a sandboxed run fails with `EPERM` or hangs and tells you nothing.

`.cargo/config.toml` defaults the build target to `x86_64-unknown-linux-musl`, so the built controller doubles as the container worker. On a host that is not x86-64 Linux, pass your own triple, for example `cargo build --target aarch64-apple-darwin`.

Expected output after M1:

    cargo test -p brokk-mj-tui
    ...
    test result: ok. 253 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

For the live check, build and drive the program under tmux rather than attaching to it, so the check is scripted and repeatable:

    cargo build
    tmux new-session -d -s hk -x 200 -y 50 './target/x86_64-unknown-linux-musl/debug/mj'
    tmux capture-pane -t hk -p
    tmux send-keys -t hk F1
    tmux capture-pane -t hk -p
    tmux send-keys -t hk Escape
    tmux kill-session -t hk

Do not press Enter and do not type into any prompt during that check if a daemon with real sessions is running: the keys reach a live agent.

## Validation and Acceptance

Acceptance for M1 is what a person sees, not what the code contains.

Start the program with no session starting or stopping. The bottom row reads, from the Sessions pane, `Enter open · Alt-N new · s resume · Alt-A mark read · e edit · Tab pane · Alt-G panes · F3 workspaces · F4 web · Alt-Q quit · F1 help` (M1 shipped this with the old key names; M2 moved them), and it contains no word "cancel". Start a session. While it is provisioning, the row gains `Alt-X cancel launch`. When provisioning ends, that hint disappears again. This is `footer_lists_cancel_only_while_an_operation_is_in_flight`, which fails against the old fixed strings and passes after.

Press `F1`. A bordered list appears over the screen, headed "Keys", grouped into Sessions pane, Selected session, Targets pane, Quota pane, First-run setup, Panes, Anywhere, and Composer. Every command in the registry appears with its keys; the ones that cannot run where you are standing are dim and say why. `Up` and `Down` scroll it. Press `Escape` and the screen you were on returns. Press `n` to open the new-session wizard, then `F1`, then `Escape`: the wizard is still there with its selections intact, because help restores the mode it opened over instead of cancelling it. That is `help_overlay_returns_to_the_wizard_it_opened_over`.

From any pane, `?` opens the same overlay and `?` closes it: `question_mark_opens_help_from_a_pane`.

Narrow the terminal. Hints vanish from the right one whole hint at a time, and `F1 help` is always the last thing standing: `footer_drops_whole_hints_when_the_width_runs_out`.

Every key the footer names does what the footer says it does. `every_footer_hint_dispatches_the_command_it_names` proves this mechanically: for each hint in the footer, at each pane, it presses the key on one copy of the surface and calls `dispatch_command` on another, then asserts the two agree on the returned `DashboardAction`, on the resulting mode, and on where the keyboard ended up.

The remaining named tests are `footer_ends_with_f1_help_at_every_focus` and `help_overlay_lists_every_registry_command_with_its_primary_key`. Three further tests in `mj-tui/src/actions.rs` guard the table itself: every `CommandId` has exactly one entry, no two commands claim the same key in the same pane, and cancel is available only while an operation runs.

## Idempotence and Recovery

Every step is a source edit, so re-running the plan is re-reading the files; nothing is destructive and nothing is migrated. The commands under `Concrete Steps` can be run any number of times.

The one thing to be careful about is the live check. `tmux kill-session -t hk` ends it; if a check is interrupted, run that command before starting another, or the next `tmux new-session -d -s hk` fails because the name is taken. The program under check attaches to a running daemon and its real sessions, so keep to the keys listed and do not send `Enter`.

If a milestone has to be abandoned part-way, the surface still works: the registry is additive, and `handle_dashboard_key` reads it only after its own hand-written arms have had their say.

## Artifacts and Notes

The footer, captured from a running `mj` at 200 columns with the Sessions pane focused, nothing in flight, and no session yet created. `Enter open`, `e edit`, and `x cancel` are correctly absent, because there is no session to open, edit, or cancel:

    n new · s resume · a mark read · Tab pane · Ctrl-G panes · F2 workspaces · F3 web · Ctrl-Q quit · F1 help

The help overlay, captured after `tmux send-keys -t hk F1` in that same run, trimmed to the first two groups. The dim `(not available here)` suffix is what the greyed entries carry:

    ┌ Keys · Up/Down scrolls · Esc or F1 closes ───────────────────────────────────┐
    │Sessions pane                                                                 │
    │  Enter         Open session  Show the selected session's conversation …      │
    │                (not available here)                                          │
    │  n             New session  Start the wizard that picks a profile, a b…      │
    │  s             Resume a session  Open the picker for every session tha…      │
    │  a             Mark all read  Clear the unread marker on every session…      │
    │  x             Cancel operation  Stop the launch, resume, or stop the …      │
    │                (not available here)                                          │
    │  Space         Fold project  Space folds the selected session's projec…      │
    │                                                                              │
    │Selected session                                                              │
    │  e             Session commands  Open the selected session's rename, c…      │
    │  —             Rename session  Give the selected session your own title.     │
    │  —             Container settings  Edit CPU, memory, and mounts for th…      │
    │  —             Stop session  Shut the selected session down, after a c…      │
    └──────────────────────────────────────────────────────────────────────────────┘

`Escape` put the dashboard back with its footer unchanged.

After M2, the same footer captured the same way. Every key in it moved except
`s`, `Tab`, and `F1`:

    Alt-N new · s resume · Alt-A mark read · Tab pane · Alt-G panes · F3 workspaces · F4 web · Alt-Q quit · F1 help

With the support panes collapsed, from the composer:

    Tab pane · Alt-G panes · F3 workspaces · F4 web · Alt-Q quit · F1 help

The help overlay after M2, trimmed to the groups that changed. The chord and
its plain-letter alias share one line, and cancel has moved into `Anywhere`:

      Alt-N / n     New session  Start the wizard that picks a profile, a bundle, and a target.
      s             Resume a session  Open the picker for every session that is not live.
      Alt-A / a     Mark all read  Clear the unread marker on every session at once.
    …
    Panes
      Tab           Next pane  Move the keyboard down the layout; Shift-Tab reverses it.
      Alt-G         Pane layout  Turn the two-position dial: panes open, or collapsed for the conversation.

    Anywhere
      Alt-X         Cancel operation  Stop the launch, resume, or stop the selected session is in the middle of.
      F3            Workspaces  Switch to another workspace.
      F4            Web viewer  Show the address and code for the browser and phone viewer.
      Alt-Q         Detach  Leave the terminal surface; the sessions keep running.
      F1 / ?        Help  List every key this surface answers.

The M2 live check, driven the same way against an isolated `MJ_CONFIG_DIR` and
`MJ_DATA_DIR`: `Alt-N` opened the new-session wizard from the dashboard,
`Escape` closed it, `F3` opened the workspace picker, `Alt-G` collapsed the
support panes to their two summary rows, `F1` opened the reference over the
collapsed layout and `F1` closed it again, `Ctrl-G` printed
`Ctrl-G moved to Alt-G` in the notice bar and turned nothing, and `Alt-Q`
detached with the usual "Active sessions will continue working" message and
exit status 0.

The M3 live check, run the same way against an isolated `MJ_CONFIG_DIR` and
`MJ_DATA_DIR`. The footer with the Sessions pane focused and no session
created, `F2 commands` now sitting between quit and help:

    Alt-N new · s resume · Alt-A mark read · Tab pane · Alt-G panes · F3 workspaces · F4 web · Alt-Q quit · F2 commands · F1 help

The palette after `F2` from that pane. The query line is at the top, the
focused pane's group leads because there is no session to head a group of its
own, and the hint row closes it:

    ┌ Commands ────────────────────────────────────────────────┐
    │> ▏                                                       │
    │  Sessions pane                                           │
    │›   Alt-N / n   New session                               │
    │    s           Resume a session                          │
    │    Alt-A / a   Mark all read                             │
    │  Panes                                                   │
    │    Tab         Next pane                                 │
    │    Alt-G       Pane layout                               │
    │  Anywhere                                                │
    │    F3          Workspaces                                │
    │    F4          Web viewer                                │
    │    Alt-Q       Detach                                    │
    │    F1 / ?      Help                                      │
    │type to filter · Up/Down · Enter runs · Esc closes        │
    └──────────────────────────────────────────────────────────┘

After typing `help`, the list is the one match and its group heading:

    │> help▏                                                   │
    │  Anywhere                                                │
    │›   F1 / ?      Help                                      │

`Escape` put the dashboard back with its footer unchanged, and `Alt-Q`
detached.

The M3.5 live check, run the same way against an isolated `MJ_CONFIG_DIR` and
`MJ_DATA_DIR`. The footer at 200 columns, with the Sessions pane focused, no
session created, and nothing in flight:

    Tab pane │ Alt-N new · Alt-S resume · Alt-A read · Alt-G panes · Alt-Q quit │ F2 palette · F3 workspaces · F4 web · F1 help

`Enter open` is absent because there is no session to open; with one selected
the row starts `Enter open · Tab pane │ …`. At 100 columns the pane group goes
first and then the last chord, and the function keys stay whole:

    Alt-N new · Alt-S resume · Alt-A read · Alt-G panes │ F2 palette · F3 workspaces · F4 web · F1 help

`Alt-S` opened the resume dialog on the Mjolnir tab. Inside it, `/` moved to
the search field, typing `x` filtered the list, and `→` switched to the Import
tab from that field — the arrows the dialog's own footer has always advertised
and which used to move the text cursor instead. `Escape` put the dashboard back
with its footer unchanged, and `Alt-Q` detached.

The M3.6 live check, run the same way against an isolated `MJ_CONFIG_DIR` and
`MJ_DATA_DIR`. The footer at 200 columns from the composer, with no session in
the fresh workspace:

    Tab pane │ Alt-N new · Alt-S resume · Alt-A read · Alt-G panes · Alt-Q detach │ F2 palette · F3 workspaces · F4 web · F5 refresh · F1 help

`F5` from the composer put `Targets and quotas refreshed.` in the notice bar,
reset the Quota pane's title from `Quota (refreshed 32s ago)` to
`Quota (refreshed 1s ago)`, and moved the Targets row for `local` from
`30% CPU · 28% RAM` to `52% CPU · 30% RAM`, so both halves really ran. `F1`
listed the new entries:

      F5            Refresh targets and quotas  Re-probe every target's capacity and ask every profile for its quota again.
      Alt-Q         Detach from this terminal  Leave this terminal client; the daemon and its sessions keep running.
      Ctrl-R                    search prompt history

`Escape` put the dashboard back and `Alt-Q` detached.

The live check was run against an isolated configuration and data directory (`MJ_CONFIG_DIR` and `MJ_DATA_DIR`, read by `env_override_os` in `src/hel_config.rs`) rather than the real one, because the real workspace was already attached to another process and attaching a second time would have taken it away from its owner.

The shape of the registry entry, from `mj-tui/src/actions.rs`:

    CommandSpec {
        id: CommandId::CancelOperation,
        label: "Cancel operation",
        description: "Stop the launch, resume, or stop the selected session is in the middle of.",
        scope: Scope::Sessions,
        keys: &[KeyHint::plain(KeyCode::Char('x'), "x")],
        footer: cancel_footer,
        available: operation_in_flight,
    },

## Interfaces and Dependencies

No new dependency is added. The registry uses `crossterm`'s `KeyCode`, `KeyEvent`, and `KeyModifiers`, which `mj-tui` already depends on, and the help overlay uses `ratatui`'s `Paragraph`, `Block`, and `Clear`, which every other dialog in the crate already uses. The overlay is centered with the crate's existing `crate::widgets::centered_modal`, which also registers the popup's body with the mouse-selection engine, so help behaves like every other dialog under the pointer.

In `mj-tui/src/actions.rs`, define:

    pub(crate) enum CommandId { OpenSession, NewSession, ResumeDialog, SessionCommands,
        RenameSession, ContainerSettings, StopSession, MarkAllRead, CancelOperation,
        ToggleProject, RefreshCapacity, TargetActions, RefreshQuotas, EditProfile,
        OpenConfig, CycleFocus, CyclePaneLayout, Workspaces, WebViewer, QuitDetach, Help }

    pub(crate) enum Scope { Global, Pane, Sessions, Session, Targets, Quota, Setup }

    pub(crate) enum Availability { Ready, Hidden, Blocked(&'static str) }

    pub(crate) struct KeyHint {
        pub(crate) code: KeyCode,
        pub(crate) modifiers: KeyModifiers,
        pub(crate) label: &'static str,
    }

    pub(crate) struct CommandSpec {
        pub(crate) id: CommandId,
        pub(crate) label: &'static str,
        pub(crate) description: &'static str,
        pub(crate) scope: Scope,
        pub(crate) keys: &'static [KeyHint],
        pub(crate) footer: fn(&DashboardState) -> Option<String>,
        pub(crate) available: fn(&DashboardState) -> Availability,
    }

    pub(crate) static COMMANDS: &[CommandSpec];
    pub(crate) const SCOPE_ORDER: [Scope; 7];
    pub(crate) fn spec(id: CommandId) -> &'static CommandSpec;
    pub(crate) fn spec_for_key(key: KeyEvent, focus: Focus) -> Option<CommandId>;
    pub(crate) fn available(dashboard: &DashboardState, scope_filter: Option<Scope>) -> Vec<CommandId>;

    impl DashboardState {
        pub(crate) fn dispatch_command(&mut self, id: CommandId) -> DashboardAction;
    }

A `KeyHint` whose `modifiers` contain `CONTROL` means the accelerator, resolved through `dashboard_accelerator` when the key is matched, so the same table is correct on macOS and elsewhere. A `KeyHint` with no modifiers on a character means the plain letter, which only reaches a pane. `available` with `scope_filter: None` returns everything that applies where the keyboard currently is, which is what the footer wants; `Some(scope)` returns one group, which is what the palette will want in M3.

In `mj-tui/src/help.rs`, define:

    pub(crate) struct HelpOverlay { pub(crate) scroll: usize, pub(crate) return_to: Box<Mode> }

    impl DashboardState {
        pub(crate) fn begin_help(&mut self);
        pub(crate) fn close_help(&mut self);
        pub(crate) fn handle_help_key(&mut self, key: KeyEvent, overlay: HelpOverlay) -> DashboardAction;
    }

    pub(crate) fn help_lines(dashboard: &DashboardState) -> Vec<Line<'static>>;
    pub(crate) fn render_help(frame: &mut Frame, area: Rect, dashboard: &DashboardState,
        overlay: &HelpOverlay, surfaces: &mut FrameSurfaces);

In `mj-tui/src/render.rs`, change the signature to:

    pub(crate) fn combined_footer_text(dashboard: &DashboardState, width: u16) -> String;

`DashboardAction` in `mj-tui/src/lib.rs` gains no variants in any milestone of this plan. Every command dispatches to an entry point that already exists, which is what keeps the change reviewable: the registry decides *when* something runs, never *what* it does.

## Risks

`Alt` chords need "Option as Meta" turned on in macOS terminals, and a short `escape-time` in tmux, or the terminal reports them as `Escape` followed by the letter. The surface already carries this exposure through `Alt-V` for dictation, so this is a widening rather than a new risk, and the plain pane letters and the command palette both remain as fallbacks. The README should say so. Turning on `KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS` next to the existing `DISAMBIGUATE_ESCAPE_CODES` in `mj-cli/src/main.rs` may help, but only after checking it leaves Control behaviour intact.

`F1` is swallowed by some terminals and multiplexers before the program sees it. `?` from a pane and the palette's own Help entry are the fallbacks, which is why `F1` is not the only way in.

Muscle memory for `Ctrl-G` and `Ctrl-Q` breaks in M2, by design. The one-release notice, the footer, and the help overlay are what carry users across.

Dropping the composer's arm from the footer is safe: whenever the composer has the keyboard, the chat draws its own footer instead, and `mj-tui/src/combined.rs` skips the dashboard footer entirely in that case.

## Revision notes

- 2026-09-01: Updated through the end of M3.6. The single global refresh key,
  the collapsed refresh action, the detach wording, and the return of history
  search to `Ctrl-R` are in the `Decision Log`; the chat footer's overflow and
  its new width rule are in `Surprises & Discoveries`.
- 2026-09-01: Updated through the end of M3.5. The single-spelling rule and
  the three-group footer are in the `Decision Log`; the resume dialog's
  unreachable tab arrows and the chat footer's missing width rule are in
  `Surprises & Discoveries`.
- 2026-09-01: Updated through the end of M3. The palette's group order at the
  composer, the treatment of `Blocked` versus `Hidden` entries, and the copied
  ranking rule are in the `Decision Log`; the two notices naming keys that do
  not exist, the now-dead `reject_selected_operation`, and the padding
  `truncate_text` eats are in `Surprises & Discoveries`.
- 2026-09-01: Updated through the end of M2. The composer's `Alt-R`/`Alt-T`
  placement, the `Help` toggle, and the crate-visibility change are recorded in
  the `Decision Log` because each is a place where the obvious reading of the
  design produces a surface that does not work.
- 2026-09-01: Created from the approved design and updated through the end of M1. The design's `CommandId::Palette` and `global_chord()` were deferred to their own milestones, and the reasoning is recorded in the `Decision Log` rather than left implicit, because a later reader following this plan from scratch would otherwise add both and find the help overlay advertising a command that does nothing and clippy rejecting an uncalled function.
