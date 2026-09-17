# Reduce the weight of the TUI code while staying on ratatui

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

The terminal user interface (TUI) of `mj` lives in three crates: `mj-tui` (dashboard, wizards, dialogs), `mj-chat` (conversation view and the reusable form controls), and the event loop in `mj-cli/src/dashboard.rs`. Together they hold about 58,000 lines of non-test Rust plus about 30,000 lines of tests. A survey found that the size is mostly breadth of features, not duplication, but it also found six places where the same idea is written several times or where machinery exists to solve a problem the framework already solves.

After this plan the user sees the same TUI, with the same keys, mouse gestures, dialogs, and idle behaviour. What changes is invisible to them and visible to every future contributor: one repaint rule instead of several hundred manual "mark me dirty" calls, one place to add a new modal instead of seven, one implementation of readline cursor motion instead of two, one body for the New-session and Move-session wizards instead of fourteen twin functions, one truncation helper instead of six, and background jobs that report a panic instead of leaving the screen waiting forever.

The expected reduction is roughly 1,800 to 2,300 non-test lines and a comparable amount of test scaffolding that only asserted on repaint flags. Each milestone is an independent commit that leaves `cargo test` and `cargo clippy --all-targets -- -D warnings` green.

## Progress

- [x] (2026-09-16 22:49Z) Milestone 1: one repaint rule (delete the manual dirty-flag protocol; collapse `Outcome` to a consumed flag; drop the `rat-event` dependency).
- [x] (2026-09-16 23:18Z) Milestone 2: one erased view of the active modal (`ModalSurface` trait replacing seven `match &self.mode` copies).
- [x] (2026-09-16 23:43Z) Milestone 3: background job helpers report panics and share one send path.
- [x] (2026-09-17 00:14Z) Milestone 4: one implementation of readline cursor motion (composer reuses `text_input.rs` helpers; multiline motions move into `TextInput`).
- [x] (2026-09-17 02:05Z) Milestone 5: one body for the New and Move wizard twins (`WizardDraft` trait), in twelve commits. The steps ran in the order 0, 1, 2, 3, 4, 6, 7, 8, 9, 5, 11, 10; see the Decision Log for why.
  - [x] Step 0: `mj-tui/src/wizards/draft.rs` with `DraftChange`, `WizardDraft`, both impls, and `DashboardState::keep`.
  - [x] Step 1: `adjust_wizard_resources`.
  - [x] Step 2: `prepare_wizard_target` and `apply_wizard_aws_options`.
  - [x] Step 3: `complete_wizard_mount_source`, `validate_wizard_mount`, `apply_wizard_mount_completions`.
  - [x] Step 4: `activate_wizard_mount`, generic `begin_mount_editor` and `edit_selected_mount`.
  - [x] Step 6: `activate_wizard_review`.
  - [x] Step 7: `activate_wizard_control`.
  - [x] Step 8: `apply_wizard_field_edit` and `apply_mount_field_edit`, with the stated alignment.
  - [x] Step 9: `apply_wizard_interaction`.
  - [x] Step 5: `handle_wizard_shortcut`.
  - [x] Step 11: `declare_wizard_controls`; `can_advance_target` replaced by `target_advance_enabled`.
  - [x] Step 10: `handle_wizard_event`; `component_events.rs` calls it for both modes.
- [ ] Milestone 6: small single-purpose cleanups (ActiveChat `Deref`, one truncation helper, one text-prompt dialog, one row viewport type).

## Surprises & Discoveries

- Observation: the "spawn helper" refactor originally estimated at 250 to 350 lines had already been done. `spawn_io`, `spawn_critical_io`, `spawn_async_job`, `spawn_cancellable_io` exist in `mj-cli/src/dashboard/io.rs:355-710` and 19 of the 34 `spawn_*` functions are already one call to them. What remains is two hand-rolled spawns, 17 copies of the "send or log a closed channel" block, and a real gap: the blocking helpers do not report a panic in the job.
  Evidence: `mj-cli/src/dashboard/io.rs:355-374` (`spawn_io`), `:939-976` and `:1144-1181` (the two hand-rolled ones).
- Observation: the chat composer is not a plain string. Every edit goes through `attachments::replace_range`, which keeps `[image N]` markers atomic. A direct swap of the composer field for `TextInput` would corrupt marker ranges. The value of Milestone 4 is therefore in sharing the motion and kill algorithms, not the storage.
  Evidence: `mj-chat/src/chat/input.rs:11-35` (`replace_input_range`), `mj-chat/src/chat/attachments.rs:359-465`.
- Observation: `#[derive(Default)]` on `ReviewWizardView` does not compile because it holds `&'a MountWizard`, and references to arbitrary types have no `Default`. That item is dropped (see Decision Log).
  Evidence: `mj-tui/src/wizards.rs:1554`.
- Observation: substituting "consumed" for "changed" in the batch loop's pre-dispatch draw is lossy without extra work. The dashboard's plain mouse handling (`mj-tui/src/lib.rs:1557-1690`) marked a repaint on a pane-focus click, a row click, a project-heading click, a pane-size control click and a pane wheel, but never called `record_event_handled`, so those events would have reported `consumed == false` and left a stale frame under the next pointer event in the batch. Fixed by recording consumption there; see the Decision Log.
- Observation: the redraw rule as first written had a hole. `tokio::select!` picks one ready arm at random, and `drain_feeds` then applies every message queued behind it. If a clock tick won the select while a background message was also ready, the timer arm set `redraw = false` and the drain applied a visible update that never reached the screen until some later wakeup. The old protocol did not have this hole because `draw()` re-read `take_render_changed()` after the drain. `Feed` now reports whether the drain took a message (`take_delivered`, `mj-controller/src/pollers.rs:205-216`) and the loop ORs that into `redraw` (`mj-cli/src/dashboard.rs:653`).
  Evidence: `mj-cli/src/dashboard.rs` select arms at `:575-650` versus `drain_feeds` at `:1947`; every `drain_*` reads `Feed::next_ready`, which the winning arm's `accept` only latches.
- Observation: `EditOutcome` is kept, not collapsed to `bool`. One caller branches on `EditOutcome::Changed` for a reason that is not repaint: `ChatState::handle_history_search_key` re-runs `refresh_history_search()` only when the key changed the query text, so a cursor move must not restart the search.
  Evidence: `mj-chat/src/chat/history.rs:266-276`.
- Observation: the acceptance grep for `Outcome::` has to be a word-boundary grep. `WaitOutcome::`, `RemotePreflightOutcome::`, `SetupOutcome::`, `WorkerRecordPersistenceOutcome::`, `ReviewDiscoveryOutcome::` and `EditOutcome::` are unrelated enums in the same three crates. `grep -rEn '\bOutcome::' mj-tui/src mj-chat/src mj-cli/src` reports zero; a plain `grep Outcome::` reports the unrelated ones.
- Observation: `mark_render_changed` and its relatives are called from far more places than first counted. The dashboard side has 204 `mark_render_changed()` calls plus 49 `mark_render_changed_cells` calls; the chat side has 152 `mark_visible_changed()` calls plus an elicitation-dialog flag (`take_changed`/`mark_changed`) that is polled from five places. Three trackers are layered: the manual flags, a revision counter diffed around every event, and a visual-state diff inside `Form::handle_at`.
  Evidence: `grep -rc "mark_visible_changed()" mj-chat/src`; `mj-tui/src/lib.rs:1489`; `mj-chat/src/components/scope.rs:826-842`.

- Observation: the Milestone 2 text says "twelve payloads whose whole erased behaviour is one `RefCell<Dialog<K>>`" and then lists eleven. Eleven plus the five hand-written impls is sixteen, which is exactly the number of `Mode` variants other than `Dashboard`, so the list was right and the count was a typo.
  Evidence: `mj-tui/src/lib.rs` `enum Mode` has seventeen variants; `mode_surfaces!` in `mj-tui/src/modal_surface.rs` names sixteen.
- Observation: Milestone 2 does not reduce the line count. It removes 304 lines from the five files it touches and adds a 393-line module, a net gain of about 90 lines, against a predicted saving of 180 to 200. The prediction counted the deleted `match` arms but not what replaces them: fourteen `impl DialogModal` blocks cost two accessor methods each even behind a macro, and the two `Mode` accessors are still one arm per variant. The milestone's actual benefit is the one that was always the main one, that a new modal is declared in one list instead of seven matches.
  Evidence: `git diff --shortstat` for the milestone commit.
- Observation: `SetupDialog::prepare_dialog_state` reads `self.editor`, which is private to `mj-tui/src/setup.rs`, so its `ModalSurface` impl cannot live in `mj-tui/src/modal_surface.rs`. It is written in `setup.rs` instead, and the six inherent methods it absorbed are gone. Every other payload exposes its form as `pub(crate)`, so the rest of the impls are in the new module.
  Evidence: `mj-tui/src/setup.rs` `struct SetupDialog` fields `editor`, `path`, `draft`.
- Observation: `ContainerEditFocus` was re-exported from `mj-tui/src/dialogs.rs` only under `#[cfg(test)]`, so naming `Dialog<ContainerEditFocus>` in another module did not compile. It is now an unconditional `pub(crate) use`.
  Evidence: `mj-tui/src/dialogs.rs:3-4`.
- Observation: Milestone 3 does not remove 55 lines either; it adds about 22 non-test
  lines, plus 70 lines of panic tests. `report` and `blocking_result` cost 30 lines,
  and rustfmt renders `report("label", &updates, DashboardIoUpdate::X { … })` over
  five to seven lines where the `if let Err(error) = updates.send(…) { tracing::debug!(…) }`
  block it replaces took six to eight. The saving is real only where the update value
  is short. The milestone's benefit is the closed bug class and the single send path,
  not the line count.
  Evidence: `git diff --shortstat` for the milestone commit reports 265 insertions and
  173 deletions across four files.
- Observation: three send sites in `mj-cli/src` legitimately do not go through `report`,
  because their send result is control flow rather than a report. `mj-cli/src/dashboard/io.rs:1501`
  sends a remote-repair request and uses `.context("dashboard closed during remote repair
  preparation")?` to abandon the creation recipe; `mj-cli/src/dashboard/actions.rs:1384` and
  `:1392` send inside a web-viewer polling task whose `?` ends the poll when the dashboard
  is gone.
- Observation: the panic message survives the trip. Tokio 1.52's `JoinError: Display`
  writes `task <id> panicked with message "boom"`, so `"{operation} task failed: {error}"`
  contains both the operation and the payload, and the tests can assert on both without
  reaching for `into_panic`.
  Evidence: `~/.cargo/registry/src/*/tokio-1.52.3/src/runtime/task/error.rs:135-152`.
- Observation: `spawn_io`, `spawn_critical_io` and `spawn_cancellable_io_with_token` each
  already had a closure parameter named `report`, so the new free function needed the
  call sites' parameter renamed to `to_update`. The public signatures are unchanged;
  every caller passes it positionally.
- Observation: the composer's Ctrl-K chaining and `TextInput::kill(range, append)` already
  produce the same kill buffer. The composer takes the old buffer aside, kills, then splices
  the old text back in front; `TextInput::kill` appends the new text behind the old one.
  Both give "earlier kill, then later kill" in Emacs order, so `kill_to_line_end(chained)`
  in `TextInput` is one call to the existing `kill(range, chained)`.
  Evidence: `mj-chat/src/chat/input.rs` `kill_to_line_end` versus `mj-chat/src/text_input.rs` `kill`;
  `sequential_control_k_accumulates_one_yankable_block` passes unchanged against both.
- Observation: for a single-line `TextInput` the new line motions and line kills are
  *already* the old whole-value ones. `insert_filtered` drops every control character
  unless the field is multiline, so a single-line value can never contain `\n`, which makes
  `line_start` always zero and `line_end` always the value length. The `self.multiline`
  gate in `handle_key` is therefore belt and braces rather than a behaviour fork; it is kept
  because it states the intent and survives a future change to the filter.
  Evidence: `mj-chat/src/text_input.rs` `insert_filtered`; the new test
  `single_line_fields_keep_their_whole_value_motions`.
- Observation: clippy's `wrong_self_convention` rejects `fn to_line_start(&mut self)`,
  because a `to_*` method on a non-`Copy` type is expected to take `self` by reference
  only. The three private key-routing helpers are named `handle_line_start`,
  `handle_line_end` and `handle_vertical` instead, which also reads better beside
  `handle_key`.
  Evidence: `error: methods with the following characteristics: (\`to_*\` and \`self\` type is
  not \`Copy\`) usually take \`self\` by reference` from `cargo clippy --all-targets`.
- Observation: `mj-chat` holds no other user of the composer's grapheme helpers.
  `previous_grapheme_boundary` was `pub(super)` but had no caller outside
  `mj-chat/src/chat/input.rs`, so it and `next_grapheme_boundary` are deleted outright
  rather than re-exported.
  Evidence: `grep -rn "previous_grapheme_boundary" mj-chat/src mj-tui/src mj-cli/src`
  reported only `chat/input.rs`.
- Observation: Milestone 4 costs about 87 non-test lines rather than saving about 80.
  `mj-chat/src/chat/input.rs` went from 328 non-test lines to 246 and
  `mj-chat/src/text_input.rs` from 553 to 722. Only seven of the added lines are the
  moved algorithms; the rest are the five new public `TextInput` methods, the five private
  routing helpers in `handle_key`, and the doc comments the four now-public free functions
  need. This is the third milestone in a row where the estimate assumed that replacing a
  duplicated block with a call always shortens the file.
  Evidence: `git diff --shortstat HEAD~2 HEAD -- mj-chat` reports 348 insertions and
  215 deletions; the per-file non-test counts above.
- Observation: two full-suite runs failed one test each in a crate this milestone does not
  touch, and both passed on their own immediately afterwards:
  `brokk-mj-core local_sockets::tests::a_short_path_binds_without_switching_directory`
  (the run that produced it was sandboxed, which the repository guidelines say invalidates
  socket tests) and `brokk-mj-worker acp::tests::bridge_exit_during_initialize_returns_an_actionable_error`
  (its test binary took 198 seconds under full-suite load). Neither crate depends on
  `mj-chat`. The accepted run is the elevated `cargo test --no-fail-fast` that reports every
  target green.

- Observation: the plan's step order does not compile. `handle_wizard_shortcut` (step 5)
  calls the control-activation body (step 7) and the interaction body (step 9), and
  `handle_wizard_event` (step 10) calls the control declaration (step 11). Running the steps
  in the written order would have needed two throwaway trait hooks in step 5 and one in
  step 10, each deleted a commit or two later. The steps were run 0, 1, 2, 3, 4, 6, 7, 8, 9,
  5, 11, 10 instead, which is the same twelve commits with no scaffolding.
- Observation: `ResumeWizard::can_advance_target` has a second caller the milestone text did
  not mention, the resume picker's footer at `mj-tui/src/wizards.rs` (`next_enabled:
  wizard.step != WizardStep::Target || wizard.can_advance_target(dashboard)`). Deleting it
  outright would have left that site without a rule, so the rule is now one free function,
  `target_advance_enabled`, shared by the footer and the target step's declaration.
- Observation: the two field-edit twins had drifted on four side effects, not the two the
  milestone counted. Besides the non-key edit differences it names, creation cleared the
  completion candidates only for a typed key in the mount *source* (not for a paste, and
  never for the destination), while resume cleared them for every changing edit to either
  field. Every one of the four is invisible, because each cleared value belongs to a control
  on a step the edited field does not appear on; they are listed in the step 8 commit
  message.
- Observation: the accessor `sizing_error` from the milestone's trait sketch has no caller.
  The two `advance_*` bodies, which are the only readers of that field, stay unmerged and
  read it directly, so the accessor was removed rather than carried with an `allow`.
- Observation: two tests named in the milestone do not exist under those names.
  `a_source_the_host_forces_read_only_cannot_be_unchecked` (step 3) has no match in
  `mj-tui/src/wizards/tests.rs`; the nearest is
  `a_new_attachment_starts_read_only_and_the_combobox_picks_its_access`. Every other named
  test exists and passes, and each step also ran the whole `brokk-mj-tui` suite.
- Observation: three full-suite runs during this milestone each failed one test in a crate
  that does not depend on `mj-tui`, and each of those tests passed on its own straight
  afterwards: `brokk-mj-chat clipboard::tests::downsizes_oversized_synthetic_image_in_powershell`,
  `brokk-mj-core local_sockets::tests::a_short_path_binds_without_switching_directory` (the
  same one Milestone 4 saw) and `brokk-mj-worker subagent_mcp::tests::a_slow_tool_call_does_not_block_a_later_one`.
  All three are load-sensitive. The accepted runs are the elevated `cargo test --no-fail-fast`
  after step 0 and after step 5, which reported every target green.
- Observation: Milestone 5 saves about 155 lines, not the 300 to 330 predicted.
  `mj-tui/src/wizards/dashboard.rs` went from 2,840 lines to 1,979 and `mj-tui/src/wizards.rs`
  from 2,677 to 2,667, against a new 716-line `mj-tui/src/wizards/draft.rs`. Of those 716,
  about 200 are the two accessor blocks (thirteen one-line methods each, written out twice
  because a trait cannot derive them) and about 180 are the New-only bodies that moved out of
  `dashboard.rs` rather than disappearing. This is the fourth milestone in a row where the
  estimate counted the deleted copy and not its replacement. The benefit is the one the plan
  always named: a change to the wizards' shared behaviour is now written once.

## Decision Log

- Decision: Redraw once per event-loop wakeup and stop tracking "did anything visible change" by hand.
  Rationale: ratatui's `Terminal::draw` already diffs the previous and current cell buffers and writes only changed cells, so an unconditional frame build costs CPU time only, never terminal output. The loop already batches ready input before drawing. The existing protocol had grown three redundant trackers and several guards that patch missed flags (`mj-cli/src/dashboard.rs:1383-1389`, `mj-tui/src/component_events.rs:401-403`, `mj-tui/src/lib.rs:1608-1610`). The only legitimate gate is a timer wakeup that changed nothing visible, and those already have their own signature checks (`clock_changed`, `animation_changed`), which stay.
  Date/Author: 2026-09-16 / Claude Fable 5.1 with Jonathan Ellis.
- Decision: Collapse `rat_event::Outcome` (`Continue` / `Unchanged` / `Changed`) to a single `consumed: bool` on `EventResult` and remove the `rat-event` dependency from `mj-chat`.
  Rationale: `Changed` existed only to drive repaint. Once repaint is unconditional, the only fact an event handler must report is whether it consumed the event, which decides whether the event is offered to the next handler. Keeping a three-state enum where one state is dead invites new code to branch on it.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: Keep `rat-focus`. It is used in one file (`mj-chat/src/components/scope.rs`) and replacing it saves no lines. It can be revisited separately.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: Implement `ModalSurface` on each `Mode` payload type, not on `Dialog<K>`.
  Rationale: `SetupDialog` must choose between two `RefCell<Dialog<_>>` with different control types, `HelpOverlay` owns a `Form<()>` and hit-tests a stored rectangle rather than the form, and `text_input_focused` needs the concrete control id. Only the payload knows all three. A helper trait `DialogModal` with a blanket impl covers the twelve payloads whose whole erased behaviour is one `RefCell<Dialog<K>>`.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: For Milestone 4, share the readline algorithms and add multiline motions to `TextInput`, but keep the composer's own `PromptPayload` storage. Do not introduce a generic `TextInput<B: EditBuffer>`.
  Rationale: the generic buffer design would let the composer be a `TextInput` outright, but it adds a trait, a type parameter on every existing `TextInput` user, and an escape hatch for in-place image replacement, for a net saving of about 130 lines. The plain-function version saves less (about 80 lines) but removes the duplicated algorithms, which is the actual bug class ("fix word movement in one editor, forget the other"). If a second consumer of marker-atomic editing ever appears, revisit the generic design; its sketch is recorded in Artifacts and Notes.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: Unify the wizard twins with a trait (`WizardDraft`) of accessors and hooks, not with a shared sub-struct or a `Wizard<Kind>` generic.
  Rationale: about 500 direct field accesses across `wizards/dashboard.rs`, `wizards.rs`, and `wizards/tests.rs` would change under either alternative. The trait is additive: no field access, struct literal, pattern, or render function changes, and each twin pair converts in its own commit.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: Report a panic in a blocking background job by awaiting the `spawn_blocking` join handle inside an outer task, the way `spawn_async_job` already does, rather than with `catch_unwind`.
  Rationale: it is the pattern the file already uses for async jobs, it needs no `AssertUnwindSafe`, and it keeps every helper's observable contract identical (one `DashboardIoUpdate` is always sent).
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: Drop the `ReviewWizardView: Default` item. A `base()` constructor would be line-neutral.
  Rationale: see Surprises & Discoveries.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: `drain_feeds` returns whether it applied a background message, and the loop ORs that into `redraw`; `Feed` gained a `delivered` flag to report it.
  Rationale: closes the hole described in Surprises & Discoveries. The alternative, leaving the timer arms free to suppress a drained feed update, would have made a visible update wait for an unrelated wakeup. This is not a per-mutation dirty flag: it reports that a message arrived, not that something visible changed, and it is cleared by the drain that reads it.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: The dashboard's plain mouse paths now call `record_event_handled()`, so a click that moves pane focus, selects a row, collapses a project, resizes a pane, or switches a workspace tab reports itself consumed.
  Rationale: the batch loop's new `previous_consumed` gate replaces a gate that was on "did anything visible change", and those paths marked a repaint but never recorded consumption. Without this, a second pointer event in the same input batch would be hit-tested against the frame from before the first one. Recording is also the honest answer: those handlers do take responsibility for the event.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `DashboardContext::maybe_open_startup_session` returns whether the pick ran, and the clock arm ORs that into `redraw`.
  Rationale: the startup pick runs inside the clock arm and opens a conversation. `StartupSession::ready` answers true at most once, so the report costs nothing and keeps that one frame from waiting a second.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: Keep `EditOutcome` as a three-state enum and give `apply_field_edit`, `TextField::apply`, and `PathField::apply` that return type in place of `rat_event::Outcome`.
  Rationale: the plan said to collapse it only if every remaining reader of `Changed` used it for repaint. One does not; see Surprises & Discoveries. The mapping is one-to-one with the old outcomes (`Unhandled`/`Handled`/`Changed` for `Continue`/`Unchanged`/`Changed`), so no call site changed meaning.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: The loop keeps its draw at the top of the iteration rather than moving it after `drain_feeds`, with `redraw` reset to true immediately after each draw.
  Rationale: the draw has to happen before the loop blocks on `select!`, or the first frame would wait for the first event, and `continue`/`break` inside the select arms would skip a trailing draw. Drawing at the top of iteration N+1 is still "after the drain of iteration N", which is what the plan's rule asks for.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: The two new loop tests assert at the `DashboardState` plus `TestBackend` level rather than driving `run_dashboard_for_workspace`.
  Rationale: `DashboardContext::open` enters raw terminal mode, loads the controller, and spawns fourteen pollers, so the loop is not reachable from a unit test. `an_unchanged_clock_tick_does_not_redraw` asserts the exact condition the clock arm evaluates (`clock_changed()` is false on a settled surface) and that the frame it declines is byte-identical to the one on screen. `a_feed_update_redraws_without_a_dirty_mark` applies a quota report the way `drain_feeds` does and asserts the next unconditional frame differs, with nothing having marked anything.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `DialogModal` carries an overridable `fn text_input_focused(&self) -> bool` instead of the planned `fn text_controls(&self) -> &'static [Self::Control]`.
  Rationale: `text_controls` can only express "focus is on one of these controls". Three payloads whose behaviour is otherwise exactly one `RefCell<Dialog<K>>` do not fit that shape: `ContainerEditor` answers `field().is_some()`, and the two wizards answer a per-step rule. Under the written plan those three would each need a hand-written `ModalSurface` repeating the five form methods. One overridable predicate covers all fourteen with one mechanism and no repetition; the eleven simple cases are still a single line each.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: The two `Mode` accessors are generated by a `mode_surfaces!` macro from one list of variant names.
  Rationale: written out they are two identical sixteen-arm matches, so the "add a variant here and nowhere else" property lived in two places that could drift. Both generated matches are still exhaustive, so a new variant still fails to compile.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `component_handles_mouse` keeps its dashboard case as an early `if matches!(self.mode, Mode::Dashboard)` return rather than a `match` with a `_` arm.
  Rationale: the acceptance criterion asks that `component_events.rs` hold no `match &self.mode` outside `handle_component_event`, and a two-arm match would have left one. The behaviour is identical.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `NewWizard::text_input_focused` and `ResumeWizard::text_input_focused` moved into their `DialogModal` impls rather than being delegated to.
  Rationale: a trait method and an inherent method with the same name on the same type resolve in favour of the inherent one, so delegation compiles and is correct, but deleting the inherent method later would turn it into silent infinite recursion. Their only caller was the `match` in `text_input_focused` that this milestone deletes. Milestone 5's note that the two methods stay unmerged still holds; they are just in `modal_surface.rs` now.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `TextInput::move_to_line_start`, `move_to_line_end` and `move_vertical` return
  `()` while `kill_to_line_start` and `kill_to_line_end` return `bool`, rather than all five
  returning `()` as the Interfaces section wrote them.
  Rationale: `handle_key` must still answer `EditOutcome::Changed` versus `Handled` for
  Ctrl-U and Ctrl-K, and `EditOutcome::Changed` is not a repaint signal any more (Milestone 1
  kept the enum because `handle_history_search_key` re-runs its search only on `Changed`).
  A kill that removed nothing must not claim a change. The motions have nothing to report.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `handle_key` routes through three private helpers (`handle_line_start`,
  `handle_line_end`, `handle_vertical`) and two more for the kills (`kill_line_backward`,
  `kill_line_forward`) instead of adding a `if self.multiline` guard to each of the eight
  key arms.
  Rationale: Ctrl-A and Home want the same decision with a different `cross_boundary`, as do
  Ctrl-E and End, and Ctrl-P/Up and Ctrl-N/Down; writing the guard inline would have repeated
  it eight times inside a `match` that is already long.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: the composer's word-motion methods `ChatState::previous_word_start` and
  `next_word_end` are deleted rather than kept as one-line wrappers, and their five callers
  in `mj-chat/src/chat.rs` call `text_input::previous_word_start(&self.input, self.input_cursor)`
  directly.
  Rationale: the acceptance criterion is that `chat/input.rs` defines no word, grapheme or
  line boundary function. A wrapper would satisfy the letter and not the point, which is that
  there is one name for each algorithm.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: the milestone is two commits, `text_input.rs` first and `chat/input.rs` second,
  and the three moved tests travel with the first commit.
  Rationale: the first commit is purely additive to `TextInput` plus the test move, so it can
  be read on its own; the second is purely subtractive from the composer. Moving the tests
  in the first commit keeps the new `TextInput` behaviour covered from the moment it exists.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: Order the milestones 1, 2, 3, 4, 5, 6. Milestone 1 goes first because it deletes hundreds of `mark_render_changed` calls that Milestones 2 and 5 would otherwise have to carry through their rewrites. Milestone 3 is independent of everything and can be done at any time.
  Date/Author: 2026-09-16 / Claude Fable 5.1.
- Decision: `report` is generic over the message type (`fn report<T>(operation: &str,
  updates: &UnboundedSender<T>, update: T)`) rather than taking `UnboundedSender<DashboardIoUpdate>`
  as the milestone text wrote it.
  Rationale: three of the send sites it replaces post `LifecycleUpdate` on the lifecycle
  channel (`spawn_lifecycle_operation` and the two in `spawn_dashboard_create_session`).
  A second near-identical function for that channel would defeat the point. `SendError<T>`
  implements `Display` for every `T`, so the debug log is unchanged.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: the restructured blocking helpers report first and drop the critical-operation
  guard second, keeping today's order, rather than the `drop(guard); report(…)` order the
  milestone text sketched.
  Rationale: the guard is what holds quit open. Dropping it before the send opens a window
  in which another thread sees no blockers and quits while the answer is still in flight.
  Reporting first costs nothing and matches `spawn_async_job`.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `report` is `pub(crate)` and `mj-cli/src/pollers.rs` uses it too, though the
  milestone named only `io.rs`, `actions.rs` and `dashboard.rs`.
  Rationale: `spawn_worker_record_persistence` and `spawn_worker_diagnosis` held the same
  two "send or log a closed channel" blocks, and the acceptance grep is over `mj-cli/src`.
  Leaving two copies behind would have kept the pattern alive in the one file a future
  contributor is most likely to copy from.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: a third panic test, `a_panicking_cancellable_job_reports_its_failure_and_releases_quit`,
  covers `spawn_cancellable_io`.
  Rationale: it is the third helper the milestone restructures and the one whose blocking
  closure is wrapped rather than passed straight to `spawn_blocking`, so it is the one
  most likely to be broken by a later edit. The test is eighteen lines.
  Date/Author: 2026-09-16 / Claude Opus 5.
- Decision: `mj-cli/src/dashboard.rs:1106` (`let _ = updates.send(TranscriptTailSeed …)`)
  also goes through `report`, making four converted sites outside `io.rs` rather than the
  three the milestone counted.
  Rationale: it was the one send in the dashboard that discarded its error outright, which
  the repository guidelines forbid. It now logs like every other one.
  Date/Author: 2026-09-16 / Claude Opus 5.

- Decision: run Milestone 5's twelve steps in the order 0, 1, 2, 3, 4, 6, 7, 8, 9, 5, 11, 10
  rather than 0 through 11.
  Rationale: three steps call bodies that later steps unify (see Surprises & Discoveries).
  The written order would have needed three throwaway trait hooks. The reordering keeps every
  commit a self-contained unification with nothing to undo.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `WizardDraft::sizing_mut` hands out `&mut BTreeMap<String, Vec<SessionResourceAllocation>>`
  rather than the shared reference the milestone sketched.
  Rationale: `apply_aws_resource_options` caches a newly resolved size list for a target the
  draft is not currently on, which is a write. A shared reference would have forced a second
  accessor for that one site. Every reader takes a reborrow, so nothing else changed.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `apply_extra_interaction` returns `()` and `apply_extra_field_edit` returns
  `Result<(), FieldEdit>`, rather than the `bool` the milestone gave the first one.
  Rationale: both outcomes of `apply_extra_interaction` end in the same thing, putting the
  draft back, so a handled flag would have been computed and dropped at its only call site.
  The field-edit hook keeps a `Result` because it really does hand the edit on.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `DraftChange::AttachmentOpened` is the review's attachment list being activated
  (editing an entry that already exists) and `DraftChange::AttachmentEditorOpened` is the
  review's Add button (a fresh entry).
  Rationale: the milestone names both variants but does not say which is which, and the two
  wizards disagree about exactly one of them: creation invalidates its preflight when an
  existing attachment is opened and not when a new one is started, while a move invalidates
  on both. This reading is the one that preserves today's behaviour, and it matches the
  milestone's list of the changes creation reacts to.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `ResumeWizard::can_advance_target` is replaced by a free function
  `target_advance_enabled(dashboard, wizard)` instead of simply being deleted.
  Rationale: it had a second caller in the resume picker's footer; see Surprises & Discoveries.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: `prepare_mount_editor` and `prepare_selected_mount_editor` no longer take the
  wizard's step by mutable reference. The selected-entry one answers whether it loaded
  anything and the caller sets the step.
  Rationale: a generic `begin_mount_editor<W>` cannot hand out `&mut wizard.step` and
  `&mut wizard.mounts` at once through accessors. Returning the fact instead of writing
  through an out-parameter is also the clearer signature. Behaviour is unchanged: the only
  other caller reaches the function having just found the entry's index, so the list is never
  empty there.
  Date/Author: 2026-09-17 / Claude Opus 5.
- Decision: the draft-trait hooks call `DashboardState` methods that are now `pub(super)`
  (`invalidate_new_remote_preflight`, `preflight_create_session_action`,
  `preflight_resume_session_action`, `advance_new_wizard`, `advance_resume_wizard`,
  `validate_new_project`, `activate_new_bundle_control`, `request_move_preparation_for_review`)
  plus the two free functions `invalidate_move_preparation` and `declare_wizard_buttons`.
  Rationale: the impls live in `mj-tui/src/wizards/draft.rs` and the bodies they delegate to
  live in `mj-tui/src/wizards/dashboard.rs`, which is a sibling module, so file-private was
  not enough. `pub(super)` keeps them inside `crate::wizards`. The alternative, moving the
  impls into `dashboard.rs`, would have put the trait's definition and its implementations in
  different files for no gain.
  Date/Author: 2026-09-17 / Claude Opus 5.

## Outcomes & Retrospective

Milestone 1 (2026-09-16). The dashboard now draws once per event-loop wakeup.
Every manual repaint signal is gone: `mark_render_changed`, `take_render_changed`,
`render_change_revision`, `mark_render_changed_cells`, `record_form_outcome_cells`
and `record_visible_event_change` on the dashboard side; `mark_visible_changed`,
`visible_revision`, `take_render_changed` on the chat side; the elicitation
dialog's third flag; the `FormVisualState` diff inside `Form::handle_at`; and
`DashboardContext::dirty` with its 31 assignment sites. `EventResult` now carries
`consumed: bool` instead of `rat_event::Outcome`, and `rat-event` is no longer a
dependency of `mj-chat`. About 800 non-test lines went with them, together with
the "previous value" snapshots that only existed to compare against.

Two things did not go as the plan assumed, both recorded above: the timer arms
could swallow a background update that the drain applied in the same iteration,
which needed `Feed::take_delivered`; and `EditOutcome` has a caller that branches
on `Changed` for a non-repaint reason, so it stays a three-state enum.

What is left for a later milestone: the timer signatures in
`mj-tui/src/render_changes.rs` (`clock_changed`, `animation_changed`,
`acknowledge_render`) are still the right gate and stay. `visible_state_signature`,
`capacity_display_signature`, `materialized_display_signature` and
`move_recovery_signature` are gone; `session_is_visible`,
`session_row_is_visible_at` and `support_projection_visible` survive because the
clock and animation signatures still consult them.

Milestone 2 (2026-09-16). One trait now answers what the dashboard asks of
whichever modal is open. `mj-tui/src/modal_surface.rs` defines `ModalSurface`
(confirmation, pointer hit test, pointer release, geometry reset, text focus,
dialog preparation, layer detail) and the helper trait `DialogModal`, which
gives a blanket implementation to the fourteen payloads that answer everything
through the one `RefCell<Dialog<K>>` they own. `HelpOverlay` and `SetupDialog`
write `ModalSurface` out by hand, the latter in `mj-tui/src/setup.rs` because it
reads private fields. `Mode::surface` and `Mode::surface_mut`, generated from one
list of variant names, are the only per-variant lists left outside
`handle_component_event` and `render_modal`.

Seven `match &self.mode` copies in `mj-tui/src/component_events.rs` and the one
in `text_input_focused` are gone, together with six inherent `SetupDialog`
methods and the two wizards' `text_input_focused`. Behaviour is unchanged; the
three places where the old code was inconsistent rather than uniform
(`ContainerEditor` treating no focus as its first field, Setup ignoring the
review editor's text fields, Help hit-testing its stored rectangle) are kept as
they were, each with a comment saying so.

This milestone costs about 90 lines rather than saving 190; see Surprises &
Discoveries for why the estimate was wrong and why the milestone is still worth
having.

Milestone 3 (2026-09-16). A background job that panics now says so. The three
blocking helpers in `mj-cli/src/dashboard/io.rs` (`spawn_io`, `spawn_critical_io`,
`spawn_cancellable_io_with_token`) run their `spawn_blocking` job from an outer
`tokio::spawn` and await its join handle, so a panic becomes
`<operation> task failed: task N panicked with message "boom"` on the I/O
channel instead of a dialog that waits forever. `blocking_result` does that
mapping in one place and `report` does every send, which removed twenty-two
copies of the "send or log a closed channel" block across `io.rs`, `actions.rs`,
`dashboard.rs` and `pollers.rs`.

Two hand-rolled spawns are gone: `spawn_project_source_resolution` is now one
call to `spawn_cancellable_io`, and `spawn_config_rename` is one call to
`spawn_critical_async`. The rename therefore gains the fifteen-second
acknowledgement timeout (`SAVE_ACK_TIMEOUT`) that every other daemon save has:
if the daemon goes quiet mid-rename, the dashboard now says the save was not
confirmed and releases quit instead of holding a blocker until the daemon
answers. That is the milestone's one user-visible change.

Three sends stay outside `report` because their send result is control flow, not
a report; they are named in Surprises & Discoveries.

Milestone 4 (2026-09-17). There is now one implementation of each readline
boundary algorithm. `mj-chat/src/text_input.rs` exports `previous_word_start`,
`next_word_end`, `line_start` and `line_end` as free functions over `(text,
cursor)` beside the `previous_grapheme` and `next_grapheme` it already had, and
`TextInput` gained `move_to_line_start`, `move_to_line_end`, `move_vertical`,
`kill_to_line_start`, `kill_to_line_end` and the `preferred_column` that
`move_vertical` aims for and every other cursor change clears. `handle_key`
routes Ctrl-A, Ctrl-E, Home, End, Ctrl-U, Ctrl-K, Up and Down to them when the
field is multiline, so any future multiline `TextField` gets composer-grade
editing for free. No production code constructs `TextInput::multiline()` today;
the only callers are three tests in `mj-chat/src/components/controls.rs`.

`mj-chat/src/chat/input.rs` lost its copies of all seven algorithms and calls
the shared ones. What stayed is exactly the part that is not shared: every
composer edit still goes through `attachments::replace_range` and
`attachments::snap_cursor`, so an `[image N]` marker is still inserted, moved
over and deleted as one unit. The file is 150 lines shorter.

Tests moved with the behaviour. The three readline tests that covered line
motion and chained kills through `ChatState` now run against
`TextInput::multiline()`, joined by two new ones for the preferred column and
for the unchanged single-line routing; the duplicate
`readline_word_edits_and_grapheme_cursor_are_atomic` is gone, its Alt-B
assertion folded into `readline_edits_at_unicode_grapheme_boundaries`. One
composer test remains for the part only the composer does:
`control_k_and_control_y_round_trip_a_line_holding_an_image_marker`.

Like Milestones 2 and 3, it does not shrink the code. `chat/input.rs` loses 82
non-test lines and `text_input.rs` gains 169, a net cost of about 87 non-test
lines against a predicted saving of about 80, plus 45 net test lines. The plan's
estimate counted the seven deleted algorithms and not what replaces them: five
new public methods on `TextInput`, five private routing helpers, and four free
functions that now carry doc comments because they are public. Most of that
addition is capability that did not exist before -- any multiline `TextField`
now has line motion, vertical motion with a preferred column, and line kills --
rather than relocated code. The milestone's benefit is the closed bug class,
which is that word or line movement can no longer be fixed in one editor and
missed in the other.

Milestone 5 (2026-09-17). The New-session and Move/Resume wizards have one
body each. `mj-tui/src/wizards/draft.rs` defines `DraftChange` and the
`WizardDraft` trait: thirteen accessors, `into_mode`, and thirteen hooks for
the places the two wizards genuinely differ. Fourteen twin function pairs in
`mj-tui/src/wizards/dashboard.rs` became fourteen generic bodies
(`declare_wizard_controls`, `handle_wizard_event`, `apply_wizard_interaction`,
`apply_wizard_field_edit`, `activate_wizard_control`, `activate_wizard_review`,
`activate_wizard_mount`, `handle_wizard_shortcut`, `complete_wizard_mount_source`,
`validate_wizard_mount`, `apply_wizard_mount_completions`,
`apply_wizard_aws_options`, `prepare_wizard_target`, `adjust_wizard_resources`),
and `begin_mount_editor` and `edit_selected_mount` in `mj-tui/src/wizards.rs`
lost their resume copies. `ResumeWizard::can_advance_target` is gone, replaced
by one `target_advance_enabled` that the target step and the resume picker's
footer share.

What stays unmerged is what the milestone said would: the two `preflight_*`
submit actions, the two `advance_*` step machines, and the two
`text_input_focused` methods that Milestone 2 moved into
`mj-tui/src/modal_surface.rs`.

One step changes behaviour on purpose. Step 8 aligned four side effects the
two field-edit copies had drifted on, all of them invisible because each
cleared value belongs to a control on a different step; they are listed in
that commit's message and in Surprises & Discoveries.

Like the three milestones before it, it saves less than predicted: about 155
lines against 300 to 330. Two hand-written accessor blocks and the New-only
bodies that moved rather than vanished account for most of the difference.

## Context and Orientation

The TUI is drawn with the `ratatui` crate (version 0.30) and reads terminal input with the `crossterm` crate (version 0.29). Three crates touch these libraries and nothing else in the workspace does:

`mj-tui` holds the dashboard state (`DashboardState` in `mj-tui/src/lib.rs`), an enum `Mode` (`mj-tui/src/lib.rs:483-506`) naming which modal dialog is open, the concrete dialogs (`mj-tui/src/dialogs.rs`, `mj-tui/src/setup.rs`, `mj-tui/src/workspaces.rs`, `mj-tui/src/resume.rs`, `mj-tui/src/palette.rs`, `mj-tui/src/help.rs`), the New-session and Move-session wizards (`mj-tui/src/wizards.rs` with their event handling in `mj-tui/src/wizards/dashboard.rs` and tests in `mj-tui/src/wizards/tests.rs`), and the pane rendering (`mj-tui/src/render.rs`, `mj-tui/src/combined.rs`).

`mj-chat` holds the conversation view (`ChatState` in `mj-chat/src/chat.rs`, its async orchestration `ActiveChat` in `mj-chat/src/chat/active.rs`, the transcript in `mj-chat/src/chat/transcript.rs`), the composer editing code (`mj-chat/src/chat/input.rs`), a general single-line and multiline text editor `TextInput` (`mj-chat/src/text_input.rs`), and the shared form framework under `mj-chat/src/components/`. In that framework a `Form<K>` (`mj-chat/src/components/scope.rs:310`) tracks focus, hit rectangles, and pointer capture for a set of controls identified by an enum `K`; a `Dialog<K>` (`mj-chat/src/components/dialog.rs:39`) wraps a `Form` with cancel and submit roles and an unsaved-changes confirmation. Every dialog and wizard in `mj-tui` owns one `RefCell<Dialog<K>>` (the `RefCell` lets rendering, which takes `&self`, register geometry).

`mj-cli/src/dashboard.rs` owns the terminal and the event loop. `run_dashboard_for_workspace` (`:412`) runs a `tokio::select!` with about twenty arms: terminal input, the warm chat's own feeds, fourteen background feeds, and three timers. After the winning arm it calls `drain_feeds` and then draws. Terminal input is batched: after the first ready event the loop keeps pulling ready events with a zero timeout so key repeats and pastes coalesce into one frame.

Terms used below. "Repaint" means calling `Terminal::draw`, which rebuilds the whole frame into a cell buffer and writes only the cells that differ from the previous frame. "Consumed" means an event handler took responsibility for an event so it must not be offered to the next handler. "Erased" means accessed through a trait object (`&dyn Trait`) so callers do not need to know the concrete type. "Twin functions" means two functions that differ only by the type they operate on.

Every `cargo test` run in this repository must happen outside the restricted sandbox with elevated permissions, because the suite uses loopback sockets. Run all checks on the dev profile. The required checks after any Rust change are:

    cd /home/jonathan/Projects/hel4
    cargo test
    cargo clippy --all-targets -- -D warnings

Commit each milestone (and each internal step of Milestone 5) on the current branch with only the files you changed staged. Do not run `git add -A`.

## Plan of Work

### Milestone 1: one repaint rule

Scope. Remove every manual "something visible changed" signal and redraw once per event-loop wakeup, except on timer ticks that changed nothing on screen. Collapse the three-state event outcome to a consumed flag. At the end, the dashboard behaves identically, and an idle dashboard still does not repaint every second.

What exists today, so you can find it. On the dashboard side, `DashboardState` has fields `render_changed: Cell<bool>` and `render_change_revision: Cell<u64>` (`mj-tui/src/lib.rs:678-679`) and `last_event_outcome: Cell<Outcome>` (`:680`); functions `mark_render_changed` (`:839`), `take_render_changed` (`:846`), `render_change_revision` (`:850`), `record_event_handled` (`:865`), and the free functions `mark_render_changed_cells` and `record_form_outcome_cells` (`:2532-2545`) that exist because a modal's payload is moved out of `self.mode` while it is handled. `handle_event_result` (`:1470-1504`) compares the revision before and after an event and reports `Outcome::Changed` when it moved. `mj-tui/src/ingest.rs` compares `visible_state_signature()` (four sites at `:543`, `:593`, `:732`, `:812`) and `capacity_display_signature` (`:971`) before and after applying data, only to decide whether to mark. `mj-tui/src/render_changes.rs` holds those signatures plus `clock_changed`, `animation_changed`, and `acknowledge_render`, which gate the timer arms. On the chat side, `ChatState` has `visible_revision: u64` and `render_changed: bool` (`mj-chat/src/chat.rs:671-672`) with `mark_visible_changed` (`:1298`), `take_render_changed` (`:1288`), and `visible_revision` (`:1284`); `ActiveChat::pump` (`mj-chat/src/chat/active.rs:1022`) and `ActiveChat::handle_event_result` (`:1575`) diff the revision to produce `Outcome::Changed`; the elicitation dialog keeps a third flag with `take_changed` and `mark_changed` (`mj-chat/src/chat/elicitation.rs:455-459`). Inside the form framework, `Form::handle_at` (`mj-chat/src/components/scope.rs:760-787`) snapshots `visual_state()` before and after and lets `report_result` (`:826-842`) overrule the handler's outcome. In the loop, `DashboardContext` has `dirty: bool` (`mj-cli/src/dashboard.rs:259`) with 31 assignment sites, and `draw` (`:1387-1401`) returns early unless it is set.

The new rule. The loop draws once per iteration, after `drain_feeds`. A per-iteration local `redraw` starts `true`. Exactly two select arms may set it to `false`: the clock tick sets `redraw = dashboard.clock_changed() || visible chat clock_changed() || notices.generation() != drawn_notice_generation`, and the animation tick sets `redraw = dashboard.animation_changed() || visible chat animation_changed()`. The notice-generation comparison stays because notices are written by background tasks through a shared slot without waking the loop; it is the one signal that is genuinely external. `draw()` loses its early return and its `take_render_changed` calls, keeps the `drawn_notice_generation` bookkeeping, and still calls `acknowledge_render` on both the dashboard and the visible chat after drawing.

Inside the input batch loop (`mj-cli/src/dashboard.rs:494-565`) the frame drawn before hit-testing a mouse event must still be current. Keep a local `previous_consumed: bool`. Before dispatching any event that is not the first in its batch, draw first if `previous_consumed` is true and either the event is a mouse event or `dashboard.modal_open()` is true. This replaces the current `if matches!(event, Event::Mouse(_)) { context.draw()?; }` at `:498-500` and the `changed && (geometry_event || modal)` clauses at the end of `dispatch_event` (`:2905-2926`). `dispatch_event` returns `(consumed, batch_continues)` where `batch_continues` is true when no `DashboardAction` or `ChatEventOutcome` was produced, exactly as today minus the `changed` term.

Edits, in order.

1. In `mj-chat/src/components/scope.rs`, change `EventResult<A>` (`:67`) to `pub struct EventResult<A> { pub consumed: bool, pub action: Option<A> }`. Keep the constructors `ignored()` (consumed false), `handled()` (consumed true, no action), and rename `changed(action)` to `with_action(action)` (consumed true). Delete the `ConsumedEvent` impl (`:139`), `FormVisualState`, `visual_state`, and `report_result` (`:810-842`); `handle_at` returns `handle_inner`'s result directly. At `:1696-1699` map `EditOutcome::Unhandled` to not consumed and both other variants to consumed. Then check every remaining reader of `EditOutcome::Changed`: if the only thing it did was mark a repaint, collapse `EditOutcome` to `bool` too; if any caller branches on `Changed` for a non-repaint reason, keep the enum and record the caller in Surprises & Discoveries. Replace the 19 `.is_consumed()` call sites with `.consumed`. Remove `rat_event` from `mj-chat/src/components/mod.rs:19`, from the five `use rat_event::…` lines in `chat.rs`, `elicitation.rs`, `second_opinion.rs`, `turn_review.rs`, `config_picker.rs`, from `mj-tui/src/wizards.rs:28`, and from `mj-chat/Cargo.toml`. Run `cargo check --workspace` and fix every `Outcome::` reference (159 including tests): `Outcome::Changed` and `Outcome::Unchanged` both mean consumed; `Outcome::Continue` means not consumed.

2. In `mj-chat`, delete `visible_revision`, `render_changed`, `mark_visible_changed`, `take_render_changed`, `visible_revision()` from `ChatState`, and the `render_changed = false` line in `acknowledge_render`. Delete all 152 `mark_visible_changed()` calls. Where a call was the whole body of an `if`, delete the `if` unless its condition has a side effect, in which case keep the condition as a statement. Delete `ElicitationDialog::take_changed` and `mark_changed` and their five polling sites (`chat/input.rs:63`, `chat.rs:2675`, `:3191`, `:3223`, `:3257`). Change `ActiveChat::pump` to return `()`, and `ActiveChat::handle_event_result` to compute `consumed` from `state.event_consumed(&event, &action)` only. Delete `ActiveChat::take_render_changed`.

3. In `mj-tui`, delete the two `Cell` fields, `mark_render_changed`, `take_render_changed`, `render_change_revision`, `mark_render_changed_cells`, and `record_form_outcome_cells`. Replace each `record_form_outcome_cells(&self.last_event_outcome, …, &result)` with `self.last_event_outcome.set(result.consumed)` after changing `last_event_outcome` to `Cell<bool>` and `record_event_handled` to set it true. Simplify `handle_event_result` to `consumed = self.last_event_outcome.get() || action.is_some()`. Delete the 204 `mark_render_changed()` calls with the same "keep side-effecting conditions" rule. In `ingest.rs` delete the signature comparisons; then delete `visible_state_signature`, `VisibleStateSignature`, `capacity_display_signature`, `materialized_display_signature`, and any of `session_is_visible`, `session_row_is_visible_at`, `support_projection_visible` that have no remaining caller (check with grep before deleting each). Keep `clock_changed`, `animation_changed`, `acknowledge_render`, and the clock and animation signatures. Delete the standby forwarding at `lib.rs:1426` and `:1608-1610` and the modal-close marking at `component_events.rs:401-403`.

4. In `mj-cli/src/dashboard.rs`, delete the `dirty` field and all 31 sites, restructure the loop as described under "the new rule", and rewrite the batch loop and `dispatch_event`. Delete `drawn_size` if its only reader was the resize-dirty line at `:502`.

5. Tests. Delete tests whose only assertion is a repaint flag (for example the block at `mj-tui/src/lib.rs:2636-2700` and `mj-chat/src/chat/active.rs:3726`). Convert assertions on `Outcome::Changed` or `Unchanged` to `assert!(result.consumed)` and on `Outcome::Continue` to `assert!(!result.consumed)`. Tests for `clock_changed` and `animation_changed` in `render_changes.rs` stay. Add one test in `mj-cli/src/dashboard.rs` near the existing loop tests, named `an_unchanged_clock_tick_does_not_redraw`, that builds a `DashboardContext` with a `TestBackend`, runs one clock tick with no visible clock text change, and asserts the backend buffer's frame count did not advance; a second, `a_feed_update_redraws_without_a_dirty_mark`, applies a quota update and asserts one new frame.

Acceptance. All three crates' tests pass. Manually: run `mj`, leave the dashboard idle for one minute and observe in `top` that the process stays near zero CPU; then type in the composer and open the F2 palette and see immediate repaints; open a dialog, press Tab several times quickly, and see focus move correctly. `mj-cli/tests/termination_pty.rs` still passes.

### Milestone 2: one erased view of the active modal

Scope. Replace the seven hand-written `match &self.mode` dispatches with a trait object accessor. Net about 180 to 200 lines removed and, more importantly, one place to update when a `Mode` variant is added.

Create `mj-tui/src/modal_surface.rs` declared from `lib.rs` with `mod modal_surface; pub(crate) use modal_surface::ModalSurface;`. Define:

    pub(crate) trait ModalSurface {
        fn confirmation_open(&self) -> bool;
        fn render_confirmation(&self, frame: &mut Frame<'_>, area: Rect, surfaces: &mut FrameSurfaces);
        fn handles_mouse(&self, column: u16, row: u16) -> bool;
        /// Releases a captured pointer gesture; true if one was captured.
        fn cancel_pointer(&mut self) -> bool;
        fn reset_geometry(&mut self);
        fn text_input_focused(&self) -> bool;
        fn prepare_dialog_state(&mut self) {}
        fn layer_detail(&self) -> String { String::new() }
    }

    pub(crate) trait DialogModal {
        type Control: Copy + Eq;
        fn dialog(&self) -> &RefCell<Dialog<Self::Control>>;
        fn dialog_mut(&mut self) -> &mut RefCell<Dialog<Self::Control>>;
        fn text_controls(&self) -> &'static [Self::Control] { &[] }
        fn prepare(&mut self) {}
        fn layer_detail(&self) -> String { String::new() }
    }

    impl<T: DialogModal> ModalSurface for T { /* borrow() for &self methods, get_mut() for &mut self methods */ }

`render_confirmation` takes `&self` and borrows mutably inside, matching what `render_modal` (`mj-tui/src/render.rs:157`) can do while holding `&dashboard.mode`. Implement `DialogModal` for `RenameEditor`, `ConfigIdEditor`, `RepositoryOriginDialog` (text control `DialogControl::Field`; `prepare` receives the three inline arms from `component_events.rs:26-46`), `TargetActionsDialog`, `WebDialog`, `ImportProgress`, `ImportBundleConfirmation`, `ConfirmDialog` (defaults), `CommandPalette` (`PaletteControl::Query`), `WorkspaceManager` (`WorkspaceControl::Name`, `prepare` delegates to its existing `prepare_dialog_state`, `layer_detail` is the current view), `ResumeDialog` (`ResumeFocus::Search`). Implement `ModalSurface` by hand for `ContainerEditor` (its `text_input_focused` must stay `self.field().is_some()` to keep today's behaviour where no focus counts as the first field), `NewWizard` and `ResumeWizard` (delegate `text_input_focused` to their existing methods at `wizards.rs:327` and `:395`, `prepare_dialog_state` to `wizards.rs:294` and `:349`, `layer_detail` to the step), `SetupDialog` (move the bodies of `setup.rs:366-386` and `:907-936` into the impl; `text_input_focused` stays `is_focused(SetupControl::Field)` and ignores the review editor exactly as `lib.rs:1590` does today, with a comment noting that), and `HelpOverlay` (`confirmation_open` false, `render_confirmation` no-op, `handles_mouse` uses the stored `area` as `component_events.rs:180-187` does, `reset_geometry` also clears `area`).

Add to `Mode` two accessors `surface(&self) -> Option<&dyn ModalSurface>` and `surface_mut(&mut self) -> Option<&mut dyn ModalSurface>` (return `None` for `Mode::Dashboard`), and thin wrappers `DashboardState::active_modal` and `active_modal_mut`. These two matches are the only exhaustive lists left, which is the point: adding a variant fails to compile here.

Then rewrite, in `component_events.rs`: `dialog_confirmation_open` to `self.active_modal().is_some_and(|m| m.confirmation_open())`; `render_dialog_confirmation` to one `if let`; `component_handles_mouse` to keep its early return and the `Mode::Dashboard` arm and otherwise delegate; `cancel_component_pointer` and `reset_component_geometry` to keep the `surface_form` handling and otherwise delegate; `prepare_dialog_state` and `dialog_layer_key` to delegate; `component_modal_open` to `!matches!(self.mode, Mode::Dashboard | Mode::Help(_))` with a comment that Help is a pointer surface but not an event-routing modal. Rewrite `text_input_focused` in `lib.rs:1566-1597` to delegate. Leave `handle_component_event` and `render_modal` as they are: each arm does mode-specific work over a generic `Interaction<K>` that cannot be erased. Delete the `SetupDialog` inherent methods that became trait methods once nothing else calls them (`layer_key` is called only from `component_events.rs:13`).

Add one test in `component_events.rs` named `help_over_a_dialog_reports_no_confirmation_and_no_text_focus`: open `Mode::Help` over an `EditContainer` state and assert `dialog_confirmation_open()` is false, `text_input_focused()` is false, and `component_handles_mouse` is true for a point inside the overlay's area. The existing tests at `mj-tui/src/surface_controls.rs:535`, `mj-tui/src/dialogs/container.rs:672`, `mj-tui/src/review_settings.rs:1287`, `mj-tui/src/setup.rs:2491`, `mj-tui/src/wizards/tests.rs:1193`, `:2751`, `:2758`, `:608`, `:645` exercise the rewritten paths unchanged.

### Milestone 3: background job helpers report panics

Scope. In `mj-cli/src/dashboard/io.rs`, make every blocking spawn helper report a panic in its job, put the "send or log a closed channel" logic in one function, and convert the two remaining hand-rolled spawns. About 55 lines removed and one bug class closed.

Today `spawn_io` (`:355-374`), `spawn_critical_io` (`:378-401`), and `spawn_cancellable_io_with_token` (`:684-710`) run `tokio::task::spawn_blocking(work)` and send the mapped result from inside the blocking closure. If `work` panics, no update is sent and the UI waits forever (for example a wizard left with `set_submission_pending(true)` at `mj-tui/src/wizards/dashboard.rs:1340`). `spawn_async_job` (`:406-434`) already handles this by awaiting the inner join handle and mapping a `JoinError` to `"{label} task failed: …"`.

Add two private functions. `report(operation: &str, updates: &UnboundedSender<DashboardIoUpdate>, update: DashboardIoUpdate)` sends and, on a closed channel, logs at debug level with the operation name. `fn blocking_result<T>(operation: &str, joined: Result<anyhow::Result<T>, JoinError>) -> Result<T, String>` maps a join error (a panic) to `"{operation} task failed: {error}"` and a job error to `format!("{error:#}")`, logging both at warn level. Restructure the three blocking helpers as `tokio::spawn(async move { let joined = tokio::task::spawn_blocking(work).await; let result = blocking_result(operation, joined); drop(guard); report(operation, &updates, report_fn(result)); })`, keeping each helper's public signature and its `JoinHandle<()>` return so `spawn_detached_session_state_persist` can still be awaited on quit (`mj-cli/src/dashboard.rs:2824`). Replace the 17 inline send-or-log blocks in `io.rs` (`:370`, `:396`, `:429`, `:704`, `:741-748`, `:766-773`, `:968-973`, `:1019-1025`, `:1056-1060`, `:1177-1179`, `:1431-1435`, `:1548-1555`, `:1562-1566`, `:1574-1580`), two in `mj-cli/src/dashboard/actions.rs`, and one in `mj-cli/src/dashboard.rs` with `report`.

Convert `spawn_project_source_resolution` (`:939-976`) to `spawn_cancellable_io`; its only observable change is that a failure now also gets the helper's warn log. Convert `spawn_config_rename` (`:1144-1181`) to `spawn_critical_async` with `SAVE_ACK_TIMEOUT`; this is a behaviour change (the rename gains the same 15 second acknowledgement timeout every other daemon save has) and the commit message must say so. Leave `spawn_review_settings_discovery`, `spawn_lifecycle_operation`, `spawn_materialized_session_projection`, `spawn_checkpoint_archive_size_refresh`, and `spawn_dashboard_create_session` as they are; they stream, use another channel, or are infallible.

Add a test `a_panicking_blocking_job_still_reports_its_failure`: call `spawn_io("write clipboard", tx, || -> anyhow::Result<()> { panic!("boom") }, DashboardIoUpdate::ClipboardWritten)`, await the handle, and assert the receiver yields `ClipboardWritten(Err(message))` with `message` containing `"boom"`. Add the same for `spawn_critical_io` and additionally assert the tracker has no remaining blockers. The existing tests `asynchronous_saves_leave_the_blocking_pool_available_for_connection_metadata` (`:2686`) and `unresponsive_save_reports_uncertain_durability_and_releases_quit` (`:2754`) must pass unchanged.

### Milestone 4: one implementation of readline cursor motion

Scope. The composer's editing code in `mj-chat/src/chat/input.rs` re-implements word motion, grapheme boundaries, and line boundaries that `mj-chat/src/text_input.rs` already has as private methods, and `TextInput` lacks the multiline motions the composer has. Share the algorithms; give `TextInput` the multiline motions so any multiline `TextField` gets them; keep the composer's marker-aware storage.

Step 1, `text_input.rs`. Turn `previous_word_start`, `next_word_end` (`:385-430`) and `word_class` (`:544-547`) into `pub fn previous_word_start(text: &str, cursor: usize) -> usize` and `pub fn next_word_end(text: &str, cursor: usize) -> usize` free functions (the methods become one-line wrappers). Add `pub fn line_start(text: &str, cursor: usize) -> usize` and `pub fn line_end(text: &str, cursor: usize) -> usize`, ported from `chat/input.rs:116-126`. Add to `TextInput` a field `preferred_column: Option<usize>` and methods `move_to_line_start(&mut self, cross_boundary: bool)`, `move_to_line_end(&mut self, cross_boundary: bool)`, `move_vertical(&mut self, direction: isize)`, `kill_to_line_start(&mut self)`, and `kill_to_line_end(&mut self, chained: bool)`, ported from `chat/input.rs:128-213` and `:288-327` but operating on the private `value`. Every other cursor change clears `preferred_column`. In `handle_key`, when `self.multiline` is true, map Ctrl-A and Ctrl-E to the cross-boundary line motions, Home and End to the same-line motions, Ctrl-U and Ctrl-K to the line kills, and Up and Down to `move_vertical` when no history is attached. No production code constructs `TextInput::multiline()` today (only tests in `controls.rs`), so existing fields are unaffected. Move the three tests `readline_line_movement_kill_and_yank_match_codex`, `sequential_control_k_accumulates_one_yankable_block`, and `any_key_between_control_k_presses_restarts_the_kill_buffer` from `chat/input.rs:458-506` into `text_input.rs` as tests on `TextInput::multiline()`, and add `vertical_motion_keeps_the_preferred_column_across_a_short_line`.

Step 2, `chat/input.rs`. Delete `line_start`, `line_end` (`:116-126`), `previous_word_start`, `next_word_end` (`:215-260`), `previous_grapheme_boundary`, `next_grapheme_boundary`, `word_class` (`:341-358`), and call the `text_input` functions instead. Keep `move_vertical`, `move_to_line_*`, the kill and yank functions, and `handle_paste` in the composer, because they must go through `attachments::replace_range` and `attachments::snap_cursor` to keep image markers atomic; rewrite them to compute offsets with the shared functions and only do the splice locally. Delete the test `readline_word_edits_and_grapheme_cursor_are_atomic` (`:509`) as a duplicate of `readline_edits_at_unicode_grapheme_boundaries` in `text_input.rs`, after adding its Alt-B assertion there. Keep one composer-level smoke test that Ctrl-K then Ctrl-Y through `ChatState::handle_key` round-trips text with an image marker.

Acceptance. `cargo test -p mj-chat -p mj-tui` passes. Manually: in the composer, type two lines, use Ctrl-A, Ctrl-E, Up, Down, Alt-B, Alt-F, Ctrl-K twice then Ctrl-Y and see readline behaviour unchanged; paste an image, place the cursor after its marker, press Backspace and see the whole marker go.

### Milestone 5: one body for the New and Move wizard twins

Scope. `mj-tui/src/wizards/dashboard.rs` (2,939 lines, no inline tests) has fourteen function pairs, one for `NewWizard` and one for `ResumeWizard`. Both wizards already share `WizardStep`, `WizardControl`, `Dialog<WizardControl>`, `MountWizard`, and nine fields with identical names and types (`workspace_id`, `step`, `profile`, `target`, `mounts`, `resource_allocation`, `aws_options`, `sizing_error`, `form`). `NewWizard` adds fifteen fields for bundles, project directory, worktree, and remote preflight; `ResumeWizard` adds seven for the session id, move preparation, and queue discard. Net about 300 to 330 lines removed over twelve small commits, each leaving `cargo test -p mj-tui` green.

Create `mj-tui/src/wizards/draft.rs` declared from `wizards.rs` beside `mod dashboard;`. Define:

    pub(super) enum DraftChange {
        FieldEdit, ProfileSelected, BundleSelected, TargetSelected, ReadOnlyToggled,
        ResourcesAdjusted, AttachmentOpened, AttachmentEditorOpened, AttachmentRemoved, ReviewLeft,
    }

    pub(super) trait WizardDraft: Sized {
        fn step(&self) -> WizardStep;              fn set_step(&mut self, step: WizardStep);
        fn profile(&self) -> usize;                fn set_profile(&mut self, index: usize);
        fn target(&self) -> usize;                 fn set_target(&mut self, index: usize);
        fn mounts(&self) -> &MountWizard;          fn mounts_mut(&mut self) -> &mut MountWizard;
        fn form(&self) -> &RefCell<Dialog<WizardControl>>;
        fn form_mut(&mut self) -> &mut Dialog<WizardControl>;
        fn resource_allocation(&self) -> Option<&SessionResourceAllocation>;
        fn sizing_error(&self) -> Option<&str>;
        fn sizing_mut(&mut self) -> (&BTreeMap<String, Vec<SessionResourceAllocation>>, &mut Option<SessionResourceAllocation>, &mut Option<String>);
        fn into_mode(self) -> Mode;
        fn profile_count(&self, dashboard: &DashboardState) -> usize;
        fn target_rejection(&self, dashboard: &DashboardState, target_id: &str) -> Option<String>;
        fn prepares_target_on_select(&self, dashboard: &DashboardState) -> bool;
        fn previous_allocation<'a>(&self, dashboard: &'a DashboardState) -> Option<&'a SessionResourceAllocation>;
        fn input_locked(&self) -> bool;
        fn review_back_step(&self, dashboard: &DashboardState) -> WizardStep;
        fn note_draft_change(&mut self, dashboard: &mut DashboardState, change: DraftChange);
        fn declare_extra_step(&self, dashboard: &DashboardState, form: &mut Dialog<WizardControl>);
        fn declare_review_extras(&self, dashboard: &DashboardState, form: &mut Dialog<WizardControl>) -> bool;
        fn handle_step_event(&mut self, dashboard: &mut DashboardState, event: &Event) -> bool;
        fn apply_extra_field_edit(&mut self, dashboard: &mut DashboardState, id: WizardControl, edit: FieldEdit) -> Result<(), FieldEdit>;
        fn apply_extra_interaction(&mut self, dashboard: &mut DashboardState, interaction: &Interaction<WizardControl>) -> bool;
        fn activate_extra(self, dashboard: &mut DashboardState, id: WizardControl) -> Result<DashboardAction, Self>;
        fn handle_extra_shortcut(&mut self, dashboard: &DashboardState, key: KeyEvent);
        fn reenter_review(self, dashboard: &mut DashboardState) -> DashboardAction;
        fn submit_review(self, dashboard: &mut DashboardState) -> DashboardAction;
        fn advance(self, dashboard: &mut DashboardState) -> DashboardAction;
    }

Hook bodies are exactly what each side does today. `profile_count` is `enabled_profiles().count()` for New and `compatible_profiles(&session_id).len()` for Resume. `target_rejection` is `target_readiness_rejection` for New and `resume_target_rejection` for Resume; afterwards delete `ResumeWizard::can_advance_target` (`wizards.rs:383-393`). `previous_allocation` is `None` for New and the session's stored allocation for Resume. `input_locked` is `bundle_creation_in_flight` for New and false for Resume. `review_back_step` is ProjectDirectory or Bundle for New (`dashboard.rs:1224-1229`) and Target for Resume. `note_draft_change` for New calls `invalidate_new_remote_preflight` on `BundleSelected`, `TargetSelected`, `AttachmentOpened`, `ReviewLeft` and nothing otherwise; for Resume it calls `invalidate_move_preparation` on everything except `BundleSelected`. Neither reacts to the discard-queue toggle. `reenter_review` for Resume re-requests move preparation when `moving` (`dashboard.rs:2306-2313`), otherwise both just put the draft back. `submit_review` is `preflight_create_session_action` for New and the profile lookup plus `preflight_resume_session_action` for Resume. `advance` keeps the two existing `advance_*_wizard` bodies. `activate_extra` for New handles the NewBundle step, Bundle plus Add, and ProjectDirectory validation, returning `Err(self)` for Back so the shared Back arm still runs; Resume always returns `Err(self)`.

Add a helper on `DashboardState`: `fn keep<W: WizardDraft>(&mut self, wizard: W) -> DashboardAction { self.mode = wizard.into_mode(); DashboardAction::None }`.

Convert in this order, one commit each, running `cargo test -p mj-tui wizards::` after each and the named tests first.

Step 0: add the enum, the trait, and both impls with accessors, `into_mode`, and `previous_allocation`. Add each hook when the step that needs it arrives, always for both types.

Step 1: `adjust_wizard_resources<W>` replaces `adjust_new_resources` (`:1761`) and `adjust_resume_resources` (`:1771`), which are byte-identical apart from type. Tests: `resume_target_step_minus_halves_container_size_through_the_key_path`, `new_target_step_minus_halves_container_size_when_focus_is_off_content`, `revisiting_or_reselecting_a_target_preserves_edited_resources`.

Step 2: `prepare_wizard_target<W>` replaces `prepare_new_target` (`:1619`) and `prepare_resume_target` (`:1680`), differing only in `previous_allocation`; fold the two arms of `apply_aws_resource_options` (`:1550-1617`) into one generic body. Tests: `opening_session_wizards_prefetches_all_aws_sizes`, `resume_keeps_the_sessions_size_instead_of_the_hosts_latest_size`, `new_session_defaults_to_the_latest_size_on_its_host_and_clamps_to_capacity`, `ec2_size_controls_use_exact_doubling_steps`.

Step 3: `complete_wizard_mount_source<W>` (`:1305` / `:2325`), `validate_wizard_mount<W>` (`:1328` / `:2348`), and the two arms of `apply_mount_source_completions` (`:1785-1818`); these differ only by `Mode::New` versus `Mode::Resume`. Tests: `directory_completion_is_bounded_and_keyboard_selectable`, `new_session_mount_wizard_adds_mount_and_preserves_typed_source`, `failed_source_validation_does_not_add_new_or_resume_mounts`, `resume_dialog_attaches_an_additional_resource`, `a_source_the_host_forces_read_only_cannot_be_unchecked`.

Step 4: `activate_wizard_mount<W>` (`:1242` / `:2250`) using `reenter_review`; also make `begin_mount_editor` and `edit_selected_mount` in `wizards.rs:494-516` generic. Tests: step 3's plus `resume_review_edits_an_existing_attached_directory_in_place`, `resume_dialog_can_remove_a_previous_resource`, `removing_a_move_attachment_invalidates_and_reprepares_the_review`, `the_read_only_checkbox_rides_the_mount_into_the_created_session`.

Step 5: `handle_wizard_shortcut<W>` (`:1041` / `:2058`) using `note_draft_change(AttachmentRemoved)` and `(ResourcesAdjusted)`, `reenter_review`, and `handle_extra_shortcut` for the Resume-only `q` toggle. Tests: `unavailable_target_blocks_launch_and_refresh_allows_recovery`, `resume_can_convert_to_another_harness`, `a_raw_conversion_resume_launches_only_after_it_is_confirmed`, `queue_choice_keeps_a_ready_move_confirmation_when_only_prepared_queue_exists`, `new_bundle_editor_submits_all_sources_once_and_advances_after_success`, `bare_ssh_new_session_selects_target_then_raw_project_without_attachments`.

Step 6: `activate_wizard_review<W>` (`:1209` / `:2208`) using `review_back_step`, `submit_review`, and `note_draft_change`. Tests: `wizard_back_activation_preserves_the_draft_and_cancel_closes_it`, `resume_back_activation_preserves_the_draft_and_cancel_closes_it`, `stale_move_preparation_is_ignored_after_back_and_reentering_review`, `failed_move_preparation_stays_visible_and_retry_requests_preparation`, `new_session_wizard_sends_subagent_choice`, `isolated_creation_review_checks_prerequisites_before_enabling_create`.

Step 7: `activate_wizard_control<W>` (`:856` / `:987`) using `activate_extra`. Tests: `bundle_step_pins_the_new_bundle_action_beside_the_list`, `bundle_step_without_bundles_routes_everything_to_the_creator`, every `new_bundle_editor_*`, `target_next_focuses_the_project_field_and_footer_keys_do_not_edit_it`, `resume_target_next_mouse_release_advances_to_review`.

Step 8: extract the shared mount part of the field-edit twins (`:695-759` and `:784-831`) into `fn apply_mount_field_edit(&self, mounts: &mut MountWizard, id, edit) -> bool` and write `apply_wizard_field_edit<W>` as `apply_extra_field_edit` followed by the mount part. This step intentionally aligns two small differences: both sides now clear the completion candidates and the errors on a non-key edit (today New clears the project-directory error and Resume clears the candidates). Record it in the commit message. Tests: `new_session_mount_wizard_adds_mount_and_preserves_typed_source`, `directory_completion_is_bounded_and_keyboard_selectable`, `raw_localhost_uses_local_project_history_and_warns_for_kimi`, `new_bundle_editor_adds_multiple_repositories_and_removes_selected`.

Step 9: `apply_wizard_interaction<W>` (`:434` / `:557`) using `apply_extra_interaction`, `note_draft_change` for `FieldEdit`, `ProfileSelected`, `TargetSelected`, `ReadOnlyToggled`, and `prepares_target_on_select`. Tests: `new_session_wizard_shows_subagent_checkbox_only_for_claude_and_codex`, `review_hides_the_worktree_choice_for_isolated_targets`, `resume_refuses_a_target_the_session_cannot_use_and_says_why`, `resume_marks_an_unusable_target_row_as_disabled`, `cancelling_a_wizard_invalidates_checks_before_reopening_the_same_form`.

Step 10: `handle_wizard_event<W>` (`:297` / `:375`) using `input_locked` and `handle_step_event`; change `component_events.rs:392-393` to call it. Tests: the whole `wizards/tests.rs` file.

Step 11: `declare_wizard_controls<W>` (`:3` / `:166`) using `profile_count`, `target_rejection`, `declare_extra_step`, `declare_review_extras`; delete `can_advance_target`. Tests: `new_session_wizard_renders_and_focuses_explicit_navigation_buttons`, `resume_profile_step_*`, `move_wizard_labels_each_step_as_move`, and the review tests from step 6.

Leave unmerged: `preflight_create_session_action` versus `preflight_resume_session_action` (no shared logic), the Target arms of `advance_new_wizard` versus `advance_resume_wizard` (60 lines of New-only recipe logic versus 20 of Resume-only move logic; only their rejection preamble could be shared and is optional), and the two `text_input_focused` methods (ten lines each).

Constraints. Generic code must never hold `wizard.form().borrow()` across a call to `wizard.form_mut()`. `activate_extra(self) -> Result<DashboardAction, Self>` exists to avoid cloning the wizard; do not introduce `self.mode.clone()` patterns. `note_draft_change` for New invalidates in-flight preflight answers, so call it only for the change set listed above.

### Milestone 6: small single-purpose cleanups

Each of these is one commit.

ActiveChat delegation. `impl std::ops::Deref for ActiveChat { type Target = ChatState; }` and `DerefMut`, following the precedent of `Dialog<K>` at `mj-chat/src/components/dialog.rs:409-419`. Then delete every `ActiveChat` method whose body is exactly `self.state.<same name>(…)` (about twenty at `mj-chat/src/chat/active.rs:980-1012` and `:2524-2613`), making the corresponding `ChatState` method `pub` where it is `pub(super)` (`second_opinion_active`, `animation_changed`, `acknowledge_render`, `set_subagent_count`, `encoded_draft`). Keep methods that read `ActiveChat`'s own fields (`session_id`, `session_feed_open`, `session_retiring`) and any that rename (`draft` calls `encoded_draft`, `latest_event_ordinal` calls `latest_seq`, `has_open_review` calls `second_opinion_active`): for those three renames, rename the `ChatState` method to the public name if it has no other callers, otherwise keep the wrapper. Callers in `mj-cli` compile unchanged through auto-deref.

One truncation helper. Add `pub fn truncate_to_cells(text: &str, width: usize, options: Truncate) -> String` in `mj-chat/src/components/text_layout.rs`, where `Truncate { collapse_whitespace: bool, trim_punctuation: bool }`. It measures in display cells with `unicode_width`, appends `…` when it cuts, and when `trim_punctuation` is set strips trailing whitespace, ASCII punctuation, and the set `… – — ‘ ’ “ ” • ·` before the ellipsis (that character set is currently copied at `mj-tui/src/widgets.rs:24-33` and `mj-chat/src/chat/rendering.rs:738-745`; make `trim_before_ellipsis` in `rendering.rs` the single definition and have both use it). Replace `truncate_text` (`mj-tui/src/widgets.rs:13`, collapse and trim), `truncate_display_text` (`mj-tui/src/render.rs:883`, neither), `clip` (`mj-tui/src/palette.rs:379`, neither; note it currently counts chars, and switching to cells is a correctness improvement for wide characters), and the inlined copy in `workspace_label` (`mj-tui/src/workspaces.rs:1010-1030`). Leave `clipped_display` in `controls.rs` (it reserves a glyph) and `truncate_line_to_width` in `rendering.rs` (it is span-aware). Existing tests for the replaced functions move to the new helper.

One text-prompt dialog. `render_rename_editor` (`mj-tui/src/dialogs.rs:739`) and `render_config_id_editor` (`:778`) are the same forty lines with different header and title strings; `handle_rename_event` (`:2257`) and `handle_config_id_event` (`:1888`) are the same shape (cancel; edit the field; activate with an empty-value check that sets a notice). Add `fn render_text_prompt(frame, area, form: &RefCell<Dialog<DialogControl>>, value: &TextInput, header: &str, title: &str, surfaces)` and `fn handle_text_prompt_event(form, value: &mut TextInput, event, empty_message: &str) -> TextPromptOutcome` with `enum TextPromptOutcome { Cancel, Edited, Rejected, Submit }`, and rewrite the four functions around them. The two handlers keep their own Submit branches (they build different `DashboardAction`s) and the rename handler keeps its `return_to` parent handling.

One row viewport. `second_opinion.rs` `scroll_by` (`:402-412`), `turn_review.rs` `scroll_verdict` (`:394`) and `scroll_overview` (`:415`) each clamp a top row against `total.saturating_sub(height)` and report whether it moved; the second-opinion one also tracks a `follow` flag that is true at the end. Add `pub(crate) struct RowViewport { pub top_row: usize, pub follow: bool }` in `mj-chat/src/chat/viewport.rs` (or beside `transcript.rs`) with `fn scroll_by(&mut self, delta: isize, total_rows: usize, height: usize) -> bool` and `fn clamp(&mut self, total_rows: usize, height: usize)`, and use it for the three. Leave the primary transcript's `TranscriptViewport` alone; it freezes row estimates during a scrollbar drag and is not the same thing. This saves about 40 lines, not the 150 first estimated.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel4`.

Before starting each milestone, confirm a clean tree and note the head commit:

    git status --short
    git log --oneline -1

After each milestone (and each step of Milestone 5), run the crate tests named in that milestone first, then the full checks:

    cargo test -p mj-chat -p mj-tui -p mj-cli
    cargo test
    cargo clippy --all-targets -- -D warnings

Expected: every test passes, clippy reports no warnings. Then stage only the files you changed and commit with a message that names the milestone and step, for example `Redraw once per wakeup and drop the manual repaint flags` or `Unify the wizard mount activation twins`.

To see the repaint behaviour of Milestone 1 by hand:

    cargo run -p mj-cli --
    # in another terminal
    top -p "$(pgrep -n mj)"

Expected: after the dashboard settles, CPU stays at or near 0.0 while idle; typing or opening the F2 palette repaints immediately.

## Validation and Acceptance

Milestone 1 is accepted when all tests pass, the two new loop tests (`an_unchanged_clock_tick_does_not_redraw`, `a_feed_update_redraws_without_a_dirty_mark`) pass, no file in the three crates mentions `mark_render_changed`, `mark_visible_changed`, `take_render_changed`, `render_change_revision`, `visible_revision`, `rat_event`, or `Outcome::`, and the manual idle check shows no periodic repaint.

Milestone 2 is accepted when all tests pass, `component_events.rs` contains no `match &self.mode` outside `handle_component_event`, `mj-tui/src/modal_surface.rs` contains the two `Mode::surface` accessors as the only exhaustive per-variant lists besides `handle_component_event` and `render_modal` (the accessors live in the new module, not in `lib.rs` as first written, because that is where the trait is), and the new Help-overlay test passes.

Milestone 3 is accepted when the two panic tests pass and `grep -c "updates.send" mj-cli/src/dashboard/io.rs` reports only the `report` function's own send.

Milestone 4 is accepted when all tests pass, `chat/input.rs` defines no word, grapheme, or line boundary functions, and the manual composer check passes.

Milestone 5 is accepted when all tests pass after every step and `wizards/dashboard.rs` has no function whose name contains `_new_` or `_resume_` among the pairs listed above except the two `preflight_*` functions, the two `advance_*` bodies, and the two `text_input_focused` methods.

Milestone 6 is accepted when all tests pass and each replaced function is gone.

## Idempotence and Recovery

Every milestone is a separate commit on the current branch, so a failed milestone is recovered by `git restore` of the touched files or `git reset --hard` to the previous milestone's commit. Within Milestone 5, every step is its own commit for the same reason; if a step's tests fail and the cause is not obvious within a reasonable time, revert that step alone and record the failure in Surprises & Discoveries with the failing test name and output, then continue with the next step (the steps after 5 are independent of one another except that 10 and 11 depend on the hooks introduced earlier).

Deleting the repaint calls in Milestone 1 is mechanical and safe to redo: the acceptance grep tells you whether any remain.

## Artifacts and Notes

The generic composer design considered and deferred for Milestone 4, for the record: a trait `EditBuffer { type Killed; fn text(&self) -> &str; fn replace(&mut self, range, inserted: &Killed) -> (usize, Killed); fn snap(&self, cursor, direction) -> usize; … }` implemented for `String` and for `PromptPayload` (whose `replace` is `attachments::replace_range` and whose `snap` is `attachments::snap_cursor`), with `TextInput<B: EditBuffer = String>` holding `buffer: B` and `kill_buffer: B::Killed`. It would let the composer be `TextInput<PromptPayload>` and delete most of `chat/input.rs`, at the cost of a generic parameter on every `TextInput` (the default keeps existing spellings compiling, but `From` impls must be written for `TextInput<String>` explicitly) and an `edit_buffer` escape hatch for in-place image replacement. Net about 130 lines. Revisit if a second marker-aware editor is needed.

## Interfaces and Dependencies

`mj-chat/src/components/scope.rs` after Milestone 1:

    pub struct EventResult<A> { pub consumed: bool, pub action: Option<A> }
    impl<A> EventResult<A> {
        pub fn ignored() -> Self;
        pub fn handled() -> Self;
        pub fn with_action(action: A) -> Self;
    }

`mj-chat/src/text_input.rs` keeps `EditOutcome` as it is. `apply_field_edit`, `TextField::apply` and `PathField::apply` return `EditOutcome` in place of `rat_event::Outcome`, and `mj_chat::components` re-exports `EditOutcome` where it used to re-export `Outcome` and `ConsumedEvent`.

`mj-cli/src/dashboard.rs` after Milestone 1: `DashboardContext` has no `dirty` field and no `drawn_size`; `fn draw(&mut self) -> Result<()>` always draws; `fn dispatch_event(…) -> (bool /* consumed */, bool /* batch continues */)`; `fn drain_feeds(&mut self) -> bool` reports whether it applied a background message; `fn clock_tick_redraws(&mut self) -> bool` is the clock arm's whole condition; `fn maybe_open_startup_session(&mut self) -> bool` reports whether the pick ran.

`mj-controller/src/pollers.rs` after Milestone 1: `Feed` has `pub fn take_delivered(&mut self) -> bool`, set by `next_ready` when it produced a message.

`mj-tui/src/modal_surface.rs` after Milestone 2: `ModalSurface`, `DialogModal`, and `impl Mode { fn surface(&self) -> Option<&dyn ModalSurface>; fn surface_mut(&mut self) -> Option<&mut dyn ModalSurface>; }` as specified in Milestone 2.

`mj-cli/src/dashboard/io.rs` after Milestone 3: `fn report(operation: &str, updates: &UnboundedSender<DashboardIoUpdate>, update: DashboardIoUpdate)` and `fn blocking_result<T>(operation: &str, joined: Result<anyhow::Result<T>, JoinError>) -> Result<T, String>`; the public signatures of `spawn_io`, `spawn_critical_io`, `spawn_cancellable_io`, `spawn_cancellable_io_with_token`, `spawn_async_job`, `spawn_critical_async`, `spawn_background_async` are unchanged.

`mj-chat/src/text_input.rs` after Milestone 4: `pub fn previous_word_start(text: &str, cursor: usize) -> usize`, `pub fn next_word_end(text: &str, cursor: usize) -> usize`, `pub fn line_start(text: &str, cursor: usize) -> usize`, `pub fn line_end(text: &str, cursor: usize) -> usize`; on `TextInput`: `move_to_line_start(bool)`, `move_to_line_end(bool)`, `move_vertical(isize)`, `kill_to_line_start()`, `kill_to_line_end(bool)`.

`mj-tui/src/wizards/draft.rs` after Milestone 5: `DraftChange` and `WizardDraft` as specified in Milestone 5, implemented for `NewWizard` and `ResumeWizard`.

Dependencies removed: `rat-event` from `mj-chat/Cargo.toml` (Milestone 1). No dependencies added.

## Revision notes

2026-09-16, after implementing Milestone 1. Recorded what the milestone actually
required beyond the written plan: `Feed::take_delivered` and a `bool` from
`drain_feeds` (a timer wakeup could otherwise swallow a background update the
drain applied), `record_event_handled` on the dashboard's plain mouse paths (the
batch loop's new `consumed` gate replaces a "changed" gate those paths fed), a
`bool` from `maybe_open_startup_session`, and keeping `EditOutcome` as an enum
because one caller branches on `Changed` to decide whether to re-run a search.
The two named loop tests are written at the `DashboardState` plus `TestBackend`
level because `DashboardContext::open` needs a real terminal and fourteen
pollers. Each of these is in the Decision Log with its reason.

2026-09-16, after implementing Milestone 2. Recorded the four decisions that
departed from the written milestone (an overridable `text_input_focused` in
place of `text_controls`, a macro for the two `Mode` accessors, an early return
in place of the last `match &self.mode`, and moving the wizards'
`text_input_focused` bodies rather than delegating to them), the two facts that
forced the shape of the code (`SetupDialog`'s private fields, `ContainerEditFocus`
being test-only), and the honest line count.

2026-09-16, after implementing Milestone 3. Recorded the five decisions that
departed from the written milestone (`report` generic over the channel message so
the lifecycle channel can use it, reporting before dropping the critical-operation
guard, extending the single send path to `mj-cli/src/pollers.rs`, a third panic
test for the cancellable helper, and converting the one `let _ = …send(…)` in
`dashboard.rs`), the three send sites that legitimately remain inline, the
`report` parameter rename that the new free function forced, and the honest line
count. The reason for each is in the Decision Log; the pattern is the same as
Milestone 2, where the written estimate also assumed that replacing a block with
a call always shortens the file.

2026-09-17, after implementing Milestone 4. Recorded the four decisions that
departed from the written milestone (the kills return `bool` so Ctrl-U and
Ctrl-K can still answer `EditOutcome::Changed`; five private routing helpers in
`handle_key` rather than eight inline `multiline` guards; deleting the
composer's word-motion methods outright and pointing their five callers in
`chat.rs` at the free functions; and splitting the milestone into two commits
with the test move in the first), the clippy rule that forced the helpers'
names, and the two facts that made the port safe to do mechanically: the
composer's chained Ctrl-K and `TextInput::kill(range, append)` already build the
same kill buffer, and a single-line `TextInput` can never hold a newline, so the
new line motions reduce to the old whole-value ones.

2026-09-17, after implementing Milestone 5. Recorded the step reordering the
written plan required to compile (three steps called bodies that later steps
unified), the six decisions that departed from the written milestone (a mutable
AWS-options accessor, two hook return types, the reading of the two attachment
`DraftChange` variants, `target_advance_enabled` in place of a deleted
`can_advance_target`, the attachment-editor helpers losing their step
out-parameter, and the `pub(super)` widenings the sibling-module trait impls
need), the two milestone-named tests that no longer exist under those names,
the four rather than two side effects step 8 aligned, and the honest line count.
