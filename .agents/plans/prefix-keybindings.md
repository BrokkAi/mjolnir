# Align mj's keyboard with herdr: a configurable tmux-style prefix key

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` at the repository root. This file lives at `.agents/plans/prefix-keybindings.md`.

Delegation note for the implementer: per the user's standing instruction, Fable owns design, planning and review; the milestones below are meant to be implemented by Opus or Sonnet agents, one milestone (or one commit-sized slice of a milestone) per agent, with Fable reviewing each result against the acceptance text before the next milestone starts.

## Purpose / Big Picture

Today mj's terminal surface is driven by compile-time chords: `Alt-N` creates a session, `Alt-Q` detaches, `F7` opens settings. Nothing can be rebound, and the scheme shares nothing with herdr, the terminal multiplexer in `../herdr` that the same person uses daily. herdr follows the tmux model: one prefix key (`ctrl+b`), then a mnemonic letter; every binding editable in a `[keys]` table of its TOML config; a help overlay generated from the bindings actually in force.

After this change, a herdr or tmux user drives mj without learning anything new: `ctrl+b` then `c` creates a session, `ctrl+b q` detaches, `ctrl+b ?` lists every key, `ctrl+b s` opens settings. Every key, including the prefix, is editable in `config.toml` under `[keys]` with herdr's key-string syntax, and an edit takes effect within about a second because mj already reloads its config continuously. The composer's readline editing keys (`Ctrl-A`, `Ctrl-E`, `Ctrl-W`, `Ctrl-U`, `Alt-B`, `Alt-F` and the rest) are untouched. The one key the prefix takes over, `Ctrl-B` backward-character, is still reachable by pressing `ctrl+b` twice, as in tmux.

To see it working: run `cargo run -p mj-cli` (or the installed `mj`), press `ctrl+b`, watch the footer change to a `PREFIX` banner, press `?`, and read a help overlay whose rows say `ctrl+b c  Create session`, `ctrl+b q  Detach from this terminal`, and so on. Then add

    [keys]
    prefix = "ctrl+space"

to `config.toml`, wait a second, and observe that `ctrl+b` is backward-character in the composer again while `ctrl+space ?` opens the same help.

## Progress

- [x] (2026-09-17) M0: plan committed at `.agents/plans/prefix-keybindings.md`.
- [x] (2026-09-17) M1: `mj-core` key-string parser, `KeysConfig`, `Keybinds`, defaults, validation, unit tests; `Config.keys` wired; Settings modal hides the section.
- [x] (2026-09-17) M2: registry rewritten around `pane_keys` + `action`; prefix router on `DashboardState`; mj-cli event loop uses the router; footer and help overlay read live bindings and show the `PREFIX` banner; all Alt/F-key defaults gone; existing tests rewritten; PTY test updated with `\x02q`.
- [x] (2026-09-17) M3: `Alt-T` and `Alt-V` leave the composer; `ToggleTranscriptRendering` and `ToggleDictation` reach the visible conversation through mj-cli's action executor.
- [x] (2026-09-17) M4: shared list navigation gains `j`/`k`, `G`, `ctrl+d`/`ctrl+u`; local aliases collapse onto it; help overlay gains `/` filter.
- [x] (2026-09-17) M5: user docs, hint strings, README, troubleshooting, config reference, regenerated screenshots.
- [x] (2026-09-17) Final: `cargo test` and `cargo clippy --all-targets -- -D warnings` on the dev profile, outside the sandbox; retrospective written.

## Surprises & Discoveries

- Observation: `mj-core` (crate `brokk-mj-core`) does not depend on `crossterm`; only `mj-chat`, `mj-tui` and `mj-cli` do.
  Evidence: `mj-core/Cargo.toml` dependency list. Consequence: key combos in the config layer are described with mj's own small enum, and `mj-tui` converts crossterm events to it.
- Observation: the footer fitting code protects the help and palette hints by matching the literal text `"F1 "` and `"F2 "`.
  Evidence: `mj-chat/src/theme.rs:275-283`. Consequence: it needs a caller-supplied predicate once those keys are gone.
- Observation: adding a field to `Config` breaks nine `Config { .. }` struct literals outside `mj-core`, not only the version literals the plan asked to grep for.
  Evidence: `cargo clippy --all-targets` reported `missing field `keys`` in `mj-core/src/config/tests.rs`, `mj-core/src/state/tests.rs`, `mj-tui/src/test_support.rs`, `mj-tui/src/dialogs/tests.rs`, three files under `mj-controller/src`, and four literals in `mj-cli/tests/import_e2e.rs`. Consequence: each gained `keys: Default::default()`.
- Observation: no test hard-codes the config version; only the documentation does.
  Evidence: grepping the workspace for `version = 10` and `"version": 10` matched `docs/src/content/docs/configuration.md` twice and nothing else; every test formats `CONFIG_VERSION`. Consequence: the bump to 11 touched only that file outside `mj-core`.
- Observation: `keys` must sit among the other table-valued fields in `Config`, because TOML cannot write a bare value after a table.
  Evidence: the field is declared after `build_cache` and before `profiles`; `keys_section_is_omitted_from_serialized_defaults` round-trips a non-default `[keys]` through `save_to`/`load_from`.
- Observation: on macOS the test helper `ctrl_key` produces `SUPER`, and the dashboard remaps `SUPER` to `CONTROL` for accelerators.
  Evidence: `mj-tui/src/test_support.rs:85-96`, `mj-tui/src/lib.rs:894-916`. Consequence: the prefix is matched as literal `CONTROL` on every platform and tests build it with a dedicated helper.

- Observation: `ChatFooter` is `Copy`, and two call sites in `render_in` read `regions.footer`, so an owned `banner: Option<Line<'static>>` would not compile without dropping `Copy` from `ChatFooter` and `ChatRegions`.
  Evidence: `mj-chat/src/chat/active/render.rs:128` and `:255` both destructure `regions.footer`. Consequence: the field is `Option<&'a Line<'static>>`; `mj-tui/src/combined.rs` owns the line in a local and passes `banner.as_ref()`.

- Observation: the old three-group footer fitter dropped whole groups in order and protected hints by matching the literal text `"F1 "`/`"F2 "`, which no longer works once every chord lives in one group.
  Evidence: `mj-chat/src/theme.rs:270-289` before the change. Consequence: `fit_footer_items` now removes the right-most unprotected hint of the left-most non-empty group, and falls back to the left-most protected hint only when nothing unprotected is left, so palette survives to about 20 columns and the help key to 6.

- Observation: the footer's `ctrl+b then: ` heading rides on the chord group's first hint, so at the narrowest widths — where only palette and help remain — the row reads `: palette · ? keys` with no prefix in sight.
  Evidence: `footer_drops_pane_hints_before_chord_hints_and_keeps_help_longest` asserts exactly that at width 20. Consequence: accepted; the banner and the help overlay both name the prefix, and re-adding it after fitting would need a second fitting pass.

- Observation: running a command through the router bypasses `handle_key_at`, which is the only place that cleared `modal_click_transition`/`suppress_modal_release`; without that a command run from a chord left the next mouse click suppressed.
  Evidence: `footer_help_scrolls_and_closes_without_leaking_to_background_commands` failed with the help overlay still open after clicking its `×`. Consequence: `route_bound_key` clears both on a `Press`.

- Observation: `DashboardState::key_labels` wants pane keys first (`n / N / ctrl+b c` reads best in the help overlay), but prose such as "press … to create a session" is read from the composer as often as from a pane, where a bare `n` is text.
  Evidence: `the_empty_prompt_distinguishes_no_session_from_no_conversation` rendered "Press n to create a session". Consequence: a separate `first_key_label` puts the configured binding first and falls back to a pane key.

- Observation: the help overlay grew past 60 rows, so the `Composer` section fell off the bottom of the test's terminal.
  Evidence: `help_overlay_lists_every_registry_command_with_its_primary_key` failed on `rendered.contains("Composer")` at 200x60. Consequence: the test draws at 200x100; the overlay itself scrolls.

- Observation: `cargo check --workspace` fails in this checkout because `mj-desktop` needs GTK/WebKit system libraries. The default workspace members exclude it.
  Evidence: `Cargo.toml:7` lists ten default members without `mj-desktop`; `cargo check --workspace` fails in `pango-sys`, `cairo-sys-rs`, `javascriptcore-rs-sys`. Consequence: validation uses plain `cargo test` and `cargo clippy --all-targets`, which honour `default-members`.

- Observation: two hard-coded chords survived the M2 sweep because they live inside dialog key handlers rather than the registry: the target-actions dialog cancelled its running test on a literal `Alt-X` (`mj-tui/src/dialogs.rs:652-660`), and the new/resume wizard rechecked target readiness on a literal `F5` (`mj-tui/src/wizards/dashboard.rs:491`).
  Evidence: `grep -rn "Alt-\[A-Z\]\|F\[1-9\] " --include=*.rs mj-tui/src mj-cli/src mj-chat/src` after the first M2 commit still matched both, plus their hint strings. Consequence: both are retired. `CommandId::CancelOperation` is now allowed through exactly that one modal — `command_allowed_now` asks `target_test_running()` — and its dispatch arm calls `cancel_target_test()` first. The wizard's `F5` arm was deleted outright, because `dispatch_command(Refresh)` already clears `target_readiness` and already survives modals, so the Refresh chord does the recheck with no wizard-specific code at all.

- Observation: a dozen user-visible strings named keys that no longer exist, in crates that have no `DashboardState` at hand.
  Evidence: `Alt-Q quits` in `mj-cli/src/dashboard/io.rs` and `drafts.rs`, `F2 → Next spinner style` in `io.rs`, `F7 Settings` twice in `mj-cli/src/go.rs`, `press Alt-V to transcribe` in `mj-chat/src/chat/active/dispatch.rs`, and four `F6/Shift-F6 panes` fragments in `mj-chat/src/chat/elicitation.rs`. Consequence: the mj-cli strings read `DashboardState::first_key_label` (made `pub` for this); `mj-cli/src/go.rs` has only a `Config`, so it reads `Config::keybinds().labels(KeyAction::OpenSettings)`; the chat crate cannot know the host's bindings, so the dictation notice points at the microphone control and the elicitation footers simply drop the pane fragment.

- Observation: a third composer test depended on the `Alt-T` arm. `editor_preserves_uppercase_text_while_shortcuts_remain_case_insensitive` proved that a shifted Alt chord still reaches its lowercase shortcut by pressing `Alt-Shift-T` and asserting the render mode flipped.
  Evidence: it failed with `left: Rich, right: Raw` at `mj-chat/src/chat/tests.rs:1944` once the arm was gone. Consequence: the test now presses `Alt-Shift-B` and asserts the cursor moved back a word, which makes the same case-insensitivity point with a key that stays in the composer.

- Observation: `active_voice_remains_stoppable_after_availability_is_lost` also pressed `Alt-V`.
  Evidence: `grep -rn "Alt-V" mj-chat/src` plus the test body at `mj-chat/src/chat/tests.rs:378`. Consequence: it asks `dictation_toggle_action()` instead, and still checks the mouse path separately.

- Observation: mj-cli's action executor cannot be exercised from a test, because `DashboardContext` owns a `TerminalGuard` and a live `Controller`.
  Evidence: `mj-cli/src/dashboard.rs:218-260`. Consequence: the arm's body is a free function, `apply_chat_toggle(&mut DashboardState, Option<&mut ActiveChat>, ChatToggle)` in `mj-cli/src/dashboard/actions.rs`, which the new test calls directly with a chat it opened itself.

- Observation: `DashboardContext::visible_chat` borrows the whole context, so the "no conversation" notice cannot be written in its `else` branch.
  Evidence: `visible_chat` destructures `active_chat`, `opening_chat_session` and `dashboard` together (`mj-cli/src/dashboard/session_state.rs:150`). Consequence: a sibling `dashboard_and_visible_chat` returns both borrows at once, by asking `visible_chat()` first and then re-splitting the struct.

- Observation: `ActiveChat::open` needs a Tokio reactor, so the new mj-cli test is a `#[tokio::test]`.
  Evidence: it first panicked with "there is no reactor running" at `mj-chat/src/chat/active.rs:716`.

- Observation: the readline audit the M4 section asked for holds with nothing to
  change. Every row of herdr's text-field table is implemented twice, once in the
  composer and once in `TextField`.
  Evidence: `mj-chat/src/chat/keys.rs` matches Ctrl-A/E, Ctrl-B/F, Ctrl-H, Ctrl-D,
  Ctrl-U, Ctrl-K, Ctrl-W, Ctrl-Y, Ctrl-Left/Right, Ctrl-Backspace, Ctrl-Delete,
  Ctrl-P/N and Ctrl-R under `CONTROL`, and Alt-B/F, Alt-D, Alt-Delete,
  Alt-Backspace, Alt-Enter and Alt-Up under `ALT`; `mj-chat/src/text_input.rs`
  matches the same set (`Char('w') | Backspace` under `CONTROL` covers Ctrl-W and
  Ctrl-Backspace together) plus the unmodified Left/Right/Home/End/Backspace/Delete
  rows. Consequence: no code change in M4.

- Observation: two of the four call sites of the shared `list_selection` walk
  display rows rather than item indices, so "eight down" cannot be arithmetic
  there; a heading or a disabled row would be counted as a step.
  Evidence: `Form::list_selection` in `mj-chat/src/components/scope.rs` falls back
  to the free function only when `row_map` is empty. Consequence: the row-mapped
  path gained `step_rows`, which walks one selectable row at a time and stops at
  the end of the list, and the existing Up/Down arms now call it with `times` of 1
  rather than repeating their own `find`.

- Observation: giving the palette `ctrl+u` and `ctrl+d` for list paging took them
  away from the query field, where they were readline's kill-to-line-start and
  delete-forward. That contradicts this milestone's own rule that a focused text
  field keeps these keys as text.
  Evidence: `handle_palette_event` in `mj-tui/src/palette.rs` chooses `browse`
  before the form sees the key, so the `TextField` never received it.
  Consequence: `browse` takes the two chords only while `palette.query.is_empty()`,
  where readline would do nothing anyway; with text in the query they fall through
  to the field. `palette_ctrl_u_and_ctrl_d_edit_the_query_until_it_is_empty` holds
  both halves: Ctrl-D at the start of `rename` leaves `ename` with the selection
  unmoved, and the same chord on an empty query pages the list.

- Observation: the help overlay's filter line cannot be part of the scrolled
  paragraph, because scrolling would carry it off the top of the frame.
  Evidence: `render_help` scrolls one `Paragraph` by `overlay.scroll`.
  Consequence: the block is rendered on its own, the filter line is drawn into the
  first inner row, and the list is drawn into what is left, with the popup one row
  taller while the filter is showing.
- Observation: the committed documentation screenshots under `docs/` still carry the
  old help-overlay footer, because M4 changed its text and the regeneration test is
  `#[ignore]`d.
  Evidence: `generate_documentation_screenshots` in `mj-tui/src/docs_screenshots.rs`
  is the only test that writes the SVGs, and it is ignored; no non-ignored test
  asserts on the overlay's frame text, so `cargo test` stays green either way.
  Consequence: left for M5, which already regenerates them.

## Decision Log

- Decision: adopt herdr's full prefix model and drop mj's direct `Alt` and function-key defaults, instead of only renaming letters.
  Rationale: the user chose this explicitly ("prefix mode, drop direct chords", "align it to tmux-style"). Direct chords stay available through `[keys]` for anyone who wants them.
  Date/Author: 2026-09-17, Fable with the user.

- Decision: the default prefix is `ctrl+b`, as in herdr and tmux, although the composer uses `Ctrl-B` for backward-character.
  Rationale: identical muscle memory across the two tools is the point. tmux chose `Ctrl-B` over screen's `Ctrl-A` because backward-character is duplicated by the Left arrow while beginning-of-line is not, so it is the cheapest readline key to give up. Pressing the prefix twice sends the literal key. The user does not run mj inside herdr. Anyone who disagrees sets `prefix = "ctrl+space"` or any other modified chord.
  Date/Author: 2026-09-17, Fable.

- Decision: bindings live in a `[keys]` table in mj's existing `config.toml`, using herdr's key-string syntax and one snake_case field per action.
  Rationale: mj already has one strict, versioned, atomically written, continuously reloaded config file; a second file would need its own path, lock, reload and docs. Reusing herdr's syntax lets a line be copied between the two files.
  Date/Author: 2026-09-17, Fable.

- Decision: invalid `[keys]` entries are `Config::validate` errors (fatal at start, like every other mj config error), except that a user binding silently displaces a default binding on the same key. herdr instead warns and disables the offending binding.
  Rationale: mj's config is already strict, and a typo that silently disables a key is the kind of hidden failure the repository guidelines forbid. Displacing a default is not a failure: rebinding `prefix+g` to another action is the normal way to move a key. "User binding" means "differs from the default value of that field"; a user who writes the default value explicitly and also binds another field to the same key gets the silent-displacement behaviour rather than an error, which is acceptable and simpler than tracking which fields the file mentioned.
  Date/Author: 2026-09-17, Fable, refined from the Plan agent's review.

- Decision: `ctrl+c` and `ctrl+v` may not be used as direct (no-prefix) bindings.
  Rationale: the dashboard and composer treat them as cancel and paste before any command runs; a direct binding on them would be silently shadowed.
  Date/Author: 2026-09-17, Fable.

- Decision: the letter map. mj's session plays the role of herdr's tab (`prefix+c` create, `prefix+shift+t` rename). mj's workspace plays the role of herdr's workspace (`prefix+w` focus the workspace strip, `prefix+shift+n` open the workspace manager, `prefix+n`/`prefix+p`/`prefix+1..9` move between workspaces, since mj has no tabs to give those keys to). mj's support panes play the role of herdr's panes: `prefix+tab` and `prefix+shift+tab` cycle focus down and up the vertical stack, exactly as herdr does, and `prefix+z` and `prefix+b` are pane size and pane preset. Global keys copy herdr exactly: `?` help, `s` settings, `q` detach, `shift+r` refresh. mj-only commands take letters herdr leaves free: `g` resume picker (herdr's "goto" navigator is the nearest idea), `:` command palette (tmux's command prompt), `shift+c` cancel the in-flight operation (the shifted form of create, and non-destructive), `a` mark all read, `u` web viewer, `t` toggle transcript rendering, `m` dictation (microphone). Stop, restart, move, delete session, container settings, manage profiles, manage targets, change fast-start setup and spinner style stay unbound by default but are bindable. `v`, `x`, `h`, `j`, `k`, `l`, `shift+h`/`shift+j`/`shift+k`/`shift+l`, `minus` and `r` are deliberately left free for planned herdr-style split panes, where herdr uses `v` to split vertically, `x` to close a pane and `h`/`j`/`k`/`l` to move between them.
  Rationale: where the concept exists in herdr the key is identical; elsewhere the key is tmux's or a free mnemonic, and none collides with a herdr default a user might hit by habit and get a destructive result. `prefix+shift+c` cancels a launch or resume in flight (non-destructive); stopping a session stays keyless because a mis-hit must not act on a live session. Reserving the split-pane letters now costs nothing and avoids moving a published default later.
  Date/Author: 2026-09-17, Fable with the user.

- Decision: `Ctrl-PageUp`/`Ctrl-PageDown` workspace switching is removed from the defaults.
  Rationale: the user asked for no direct chords by default. A user who wants them writes `next_workspace = ["prefix+n", "ctrl+pagedown"]`.
  Date/Author: 2026-09-17, Fable.

- Decision: `Alt-T` (toggle transcript rendering) and `Alt-V` (dictation) leave the composer's private key handling and become registry commands. The composer's readline `Alt` keys (`Alt-B`, `Alt-F`, `Alt-D`, `Alt-Backspace`), `Alt-Enter`, `Alt-Up` and the history-search `Alt-R` stay, because they are text-editing keys that herdr's own text-field table also lists.
  Rationale: those two are application commands, not editing, so they belong where they can be rebound.
  Date/Author: 2026-09-17, Fable.

- Decision: key labels use herdr's spelling everywhere (`ctrl+b`, `shift+r`, `shift+tab`, `?`, `f5`). The help overlay and footer print the resolved prefix (`ctrl+b c`), while documentation writes `prefix+c` as herdr's docs do.
  Rationale: one formatter, one spelling; the resolved form in the UI tells a novice what to press, the abstract form in docs stays true after a rebind.
  Date/Author: 2026-09-17, Fable.

- Decision: bump `CONFIG_VERSION` from 10 to 11 when adding the `keys` section.
  Rationale: follows the repository's precedent that each new top-level section bumps the version; an older binary would reject a file containing `[keys]` anyway because every section denies unknown fields.
  Date/Author: 2026-09-17, Fable.

- Decision: the footer word for help is `keys`, not `help`, so the row ends `? keys` and the `PREFIX` banner can use the same words.
  Rationale: the plan's own target footer string and banner both write `? keys`; one word in both places means the banner is not teaching a second vocabulary.
  Date/Author: 2026-09-17, Opus implementing M2.

- Decision: `FooterGroup` keeps three slots in `theme::fit_footer_items` even though the dashboard now has two groups, with the third passed empty.
  Rationale: the chat crate shares the helper and has its own idea of groups; changing the arity would touch the chat footer for no behavioural gain.
  Date/Author: 2026-09-17, Opus implementing M2.

- Decision: the chat footer protects the last two host hints by index rather than by text.
  Rationale: the chat is handed `&[&str]` and cannot know which hint is the palette; the host already ranks palette and help last, so "the last two" is exactly the same set without the chat learning the dashboard's vocabulary.
  Date/Author: 2026-09-17, Opus implementing M2.

- Decision: `ToggleTranscriptRendering` and `ToggleDictation` are `Scope::Global` and always available, and mj-cli answers them with the notice "No conversation is open." until M3 wires the chat calls.
  Rationale: the plan asked for exactly this placeholder; a `Hidden` availability would have kept them out of the footer and the help overlay, which is where their new keys have to be advertised from the start.
  Date/Author: 2026-09-17, Opus implementing M2.

- Decision: the chat side of the two commands is three small public calls rather than one generic "run this ChatAction" entry point: `ChatState::toggle_render_mode`, `ChatState::dictation_toggle_action`, and `ActiveChat::toggle_transcript_rendering` / `ActiveChat::toggle_dictation`.
  Rationale: the host should not have to know `ChatAction`, and the dictation guard (`voice_available || voice_active`) must stay in one place. The microphone's own mouse path now reads that same guard through `dictation_toggle_action`, so the key and the click cannot drift apart.
  Date/Author: 2026-09-17, Opus implementing M3.

- Decision: mj-cli routes both variants through one `apply_chat_toggle` helper with a `ChatToggle` enum, instead of two inline arms.
  Rationale: the "No conversation is open." notice is written once, and the helper is the only part of the executor a unit test can reach without a terminal.
  Date/Author: 2026-09-17, Opus implementing M3.

- Decision: the help overlay's filter keeps `j`, `k` and `?` as filter text while it
  has focus, and only the keys that cannot be text — the arrows, the Page keys,
  Home and End — still scroll.
  Rationale: a filter that swallowed the letters a reader was typing would be worse
  than no filter. `Enter` closes from either state, because that is what the plan
  specifies and because a reader who has found the row wants the overlay gone.
  Date/Author: 2026-09-17, Opus implementing M4.

- Decision: the vim keys live in the shared `list_selection` rather than in the two
  dialogs that had their own `j`/`k` remaps, and those remaps are deleted.
  Rationale: the remaps rewrote the key event before the form saw it, so `G` and the
  ctrl pages would have needed the same rewrite in every dialog. One table means the
  resume picker, the wizard pickers, the combo boxes and every future list answer the
  same keys without knowing about them.
  Date/Author: 2026-09-17, Opus implementing M4.

## Outcomes & Retrospective

What shipped: mj's terminal surface now runs on a tmux-style prefix key. `mj-core`
gained a `[keys]` configuration section with herdr's key-string syntax, a parser,
a formatter, a resolver and fatal validation; `Config` carries it and
`CONFIG_VERSION` moved to 11. `mj-tui` gained a router (`mj-tui/src/keybinds.rs`)
that arms on the prefix, runs a bound command on the next key, forwards a second
prefix press as the literal key, and cancels on `Esc` or a mouse press. The
command registry lost its compile-time `Alt` and function-key chords and gained
an `action` column, so the footer, the help overlay and the command palette all
print the bindings actually in force. `Alt-T` and `Alt-V` left the composer and
became registry commands reaching the visible conversation through mj-cli's
action executor. Lists everywhere answer `j`/`k`, `G` and `ctrl+d`/`ctrl+u`, and
the help overlay has a `/` filter. The documentation, the README and the four
generated screenshots describe the prefix model rather than the old chords.

Three defaults moved while the work was under way, all to keep herdr's
split-pane letters free for the panes mj plans to grow: dictation went from
`prefix+v` to `prefix+m`, cancel-operation from `prefix+x` to `prefix+shift+c`,
and pane cycling dropped its `prefix+j`/`prefix+k` aliases and kept
`prefix+tab`/`prefix+shift+tab` only. The reserved set is `v`, `x`, `h`, `j`,
`k`, `l`, `minus` and `r`, which is what herdr uses for splitting, closing and
moving between panes.

Two hard-coded chords survived the first M2 sweep because they lived in dialog
key handlers rather than in the registry: the target-actions dialog cancelled a
running test on a literal `Alt-X`, and the new/resume wizard rechecked target
readiness on a literal `F5`. Only a grep found them. The cheap invariant for the
next person is to grep for `KeyCode::F(` and `KeyModifiers::ALT` outside the
composer and the text field; anything that matches is a chord the registry does
not know about.

The M4 review caught a regression of its own: giving the command palette
`ctrl+u` and `ctrl+d` for paging took them away from the palette's query field,
where they are readline's kill-to-line-start and delete-forward. The paging keys
now apply only while the query is empty. The readline audit the plan asked for
needed no code at all: the composer and `TextField` already implement every row
of herdr's text-field table.

What was not done: the plan's manual `cargo run -p mj-cli` walkthrough has not
been performed interactively. Nobody has watched the `PREFIX` banner appear, hit
`ctrl+b ctrl+b` in a live composer, edited a running `config.toml` to
`prefix = "ctrl+space"` and waited for the reload, or confirmed that two fields
bound to the same key stop `mj` from starting. All four behaviours have unit or
integration coverage, but the interactive check is left for the user.

## Context and Orientation

mj is a Rust workspace. The crates that matter here are `mj-core` (configuration and shared types, published as `brokk-mj-core`), `mj-chat` (the conversation view, the composer, and reusable dialog widgets), `mj-tui` (the dashboard: panes, wizards, help overlay, footer), and `mj-cli` (the binary, which owns the terminal event loop). A "command" is one named thing the dashboard can do; the full list is the `CommandId` enum in `mj-tui/src/actions.rs`, and each command has one `CommandSpec` entry in the `COMMANDS` table there, giving its label, its keys, its scope (which pane must have focus), its footer word, and an availability function. The footer (`mj-tui/src/render/footer.rs`), the help overlay (`mj-tui/src/help.rs`) and the command palette (`mj-tui/src/palette.rs`) all read that table, so a key and its advertisement cannot drift apart. Keep that property.

Key presses arrive in `mj-cli/src/dashboard.rs` in the `select!` loop around lines 500-600. Today, before anything else, `global_chord_event` asks `mj_tui::global_chord` whether the key is one of the "global chords" (an `Alt` letter or function key that works from every surface, including while typing in the composer). If so the command is dispatched there; otherwise the event is forwarded to the selection engine, then to `DashboardState::handle_key_at` (`mj-tui/src/dashboard_input.rs:53`) for the panes and dialogs, or to `ChatState::handle_key` (`mj-chat/src/chat/keys.rs`) when the composer has focus. The composer's own chords (`Alt-T`, `Alt-V`) and readline keys are matched inside `ChatState::handle_key`.

Configuration is `Config` in `mj-core/src/config.rs`, a TOML file at `MJ_CONFIG_DIR/config.toml` (default `~/.config/mjolnir/config.toml`). Every section is a serde struct with `deny_unknown_fields`; `Config::load_from` parses then calls `Config::validate`, and any error is fatal at start. The daemon reloads the file every 500 ms and the CLI receives new snapshots on a watch channel, drained in `mj-cli/src/dashboard/drains.rs:393-420`, which calls `DashboardState::set_config` (`mj-tui/src/ingest.rs:511`). The Settings modal (`mj-tui/src/setup.rs`) edits a JSON draft of `Config` and saves through a three-way merge, so a section it does not display survives a save untouched. The dashboard owns a `config: Config` field (`mj-tui/src/lib.rs:574`).

herdr, in `/home/jonathan/Projects/herdr`, is the reference. Its key-string parser and formatter are `parse_key_combo`, `format_key_combo`, `parse_modifier_token`, `parse_range_modifiers` in `src/config/keybinds.rs:1074-1330`; its defaults are `impl Default for KeysConfig` in `src/config/model.rs:1081-1155`; its prefix state machine is `route_key_press` in `src/client/shell/input.rs:555-640`; its status-bar banner is `src/client/shell/render.rs:86-98`; its help filter is `src/client/shell/overlay_input.rs:780-871`. herdr is Apache-2.0 and mj is GPL-3.0; porting small functions from herdr is licence-compatible, but write them fresh in mj's style rather than pasting, and do not copy herdr's Kitty-protocol `TerminalKey` machinery, which mj does not need.

Terms used below. "Direct binding": a key that runs a command with no prefix, such as `f5`. "Prefix binding": a key that runs a command only after the prefix, written `prefix+f5`. "Pane key": a plain key (Enter, Space, `e`, `?`, digits, `j`/`k`) that a focused pane answers and that the composer would read as text; these are not configurable and are unchanged by this plan. "Pending": the state between pressing the prefix and the next key.

## Plan of Work

### M0: plan file

Copy this file to `.agents/plans/prefix-keybindings.md` and commit it alone. From then on update that copy.

### M1: the `[keys]` section in `mj-core`

Create `mj-core/src/config/keys.rs` and register it in `mj-core/src/config.rs` beside the other submodules (`mod keys; pub use keys::*;`).

Terminal-agnostic key types, all `Copy`, `Eq`, `Hash`, `Ord`:

    pub enum KeyName { Char(char), Enter, Esc, Tab, Backspace, Delete, Insert, Home, End, PageUp, PageDown, Left, Right, Up, Down, F(u8) }
    pub struct Modifiers { pub ctrl: bool, pub alt: bool, pub shift: bool, pub super_: bool }
    pub struct KeyCombo { pub name: KeyName, pub modifiers: Modifiers }

`shift+tab` is `Tab` with `shift`; there is no BackTab variant, the mj-tui bridge maps crossterm's `BackTab` to it.

Parser and formatter, with herdr's grammar: `pub fn parse_key_combo(&str) -> Option<KeyCombo>`, `pub fn format_key_combo(KeyCombo) -> String`, `pub fn normalize_key_combo(KeyCombo) -> KeyCombo`. Tokens are joined by `+`. Modifiers: `ctrl`/`control`, `alt`/`option`/`meta`, `shift`, `cmd`/`command`/`super`. Names: `space`, `enter`/`return`, `esc`/`escape`, `tab`, `backspace`/`bs`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `left`, `right`, `up`, `down`, `minus`, `plus`, `comma`, `period`, `slash`, `backslash`, `quote`, `semicolon`, `colon`, `percent`, `ampersand`, `backtick`, `f1`..`f12`, and any single character. An uppercase letter normalizes to the lowercase letter plus `shift`; `shift` on a non-letter character is dropped because the terminal already delivered the shifted symbol. Formatting prints modifiers in the order `ctrl`, `alt`, `shift`, `super` (`cmd` on macOS), then the name in lowercase, so `format(parse(s)) == s` for every canonical string.

The binding value type and the config struct:

    #[serde(untagged)]
    pub enum BindingConfig { One(String), Many(Vec<String>) }   // default One("")

One macro, `key_actions!`, declares every bindable action once as `Variant / field_name = default` and expands to three things: `pub enum KeyAction` with `KeyAction::ALL` and `field_name()`, `pub struct KeysConfig` (`#[serde(default, deny_unknown_fields)]`, a `pub prefix: String` field defaulting to `"ctrl+b"`, then one `pub <field>: BindingConfig` per action) with `is_default()` and `field(KeyAction) -> &BindingConfig`, and the default table. The action list and defaults are exactly:

    Help / help = "prefix+?"
    OpenSettings / settings = "prefix+s"
    Detach / detach = "prefix+q"
    NewSession / new_session = "prefix+c"
    Resume / resume = "prefix+g"
    WorkspaceManager / workspace_manager = "prefix+shift+n"
    FocusWorkspaces / focus_workspaces = "prefix+w"
    NextWorkspace / next_workspace = "prefix+n"
    PreviousWorkspace / previous_workspace = "prefix+p"
    SwitchWorkspace / switch_workspace = "prefix+1..9"
    NextPane / next_pane = ["prefix+tab", "prefix+j"]
    PreviousPane / previous_pane = ["prefix+shift+tab", "prefix+k"]
    PaneSize / pane_size = "prefix+z"
    PanePreset / pane_preset = "prefix+b"
    Refresh / refresh = "prefix+shift+r"
    Palette / palette = "prefix+:"
    CancelOperation / cancel_operation = "prefix+x"
    MarkAllRead / mark_all_read = "prefix+a"
    WebViewer / web_viewer = "prefix+u"
    RenameSession / rename_session = "prefix+shift+t"
    ToggleTranscriptRendering / toggle_transcript_rendering = "prefix+t"
    ToggleDictation / toggle_dictation = "prefix+v"
    StopSession / stop_session = ""
    RestartSession / restart_session = ""
    MoveSession / move_session = ""
    DeleteSession / delete_session = ""
    ContainerSettings / container_settings = ""
    ManageProfiles / manage_profiles = ""
    ManageTargets / manage_targets = ""
    ChangeGoSetup / change_go_setup = ""
    CycleSpinner / cycle_spinner = ""

The resolved form:

    pub enum Trigger { Direct, Prefix }
    pub struct Binding { pub trigger: Trigger, pub combo: KeyCombo, pub index: Option<usize> }  // index is Some(0..9) for a 1..9 range
    pub struct KeyMatch { pub action: KeyAction, pub index: Option<usize> }
    pub struct Keybinds { pub prefix: KeyCombo, actions: BTreeMap<KeyAction, Vec<Binding>> }
    impl Keybinds {
        pub fn bindings(&self, KeyAction) -> &[Binding];
        pub fn resolve_direct(&self, KeyCombo) -> Option<KeyMatch>;
        pub fn resolve_prefix(&self, KeyCombo) -> Option<KeyMatch>;
        pub fn labels(&self, KeyAction) -> Vec<String>;            // "ctrl+b c", "f5"
        pub fn prefix_rhs_labels(&self, KeyAction) -> Vec<String>; // "c" for prefix bindings only
        pub fn prefix_label(&self) -> String;                      // "ctrl+b"
    }
    impl Default for Keybinds  // KeysConfig::default().resolve().expect(...)
    impl KeysConfig { pub fn resolve(&self) -> anyhow::Result<Keybinds>; }
    impl Config { pub fn keybinds(&self) -> Keybinds; }  // resolve(), falling back to Default only for an already-validated Config

`resolve` rules, in order, each failure an error of the form `keys.<field> = "<raw>": <reason>`: the prefix must parse and must carry `ctrl`, `alt` or `super`, or be a function key; then bindings are read in two passes, user fields first (a field whose value differs from the default) and default fields second; an empty string means unbound; a `prefix+` marker gives `Trigger::Prefix`; the `1..9` range form is valid only for `switch_workspace` and expands to nine bindings with `index` 0..8; an unparseable string is an error; a direct binding on an unmodified printable character, or on `ctrl+c` or `ctrl+v`, is an error suggesting the `prefix+` form; a prefix binding whose key equals the prefix is an error (that key is reserved for sending the literal prefix); a conflict on the same `(trigger, combo)` between two user fields is an error naming both fields; a default that conflicts with a user binding is dropped silently; two defaults conflicting is an error that a unit test guarantees cannot happen.

Wire it into `Config` (`mj-core/src/config.rs:333-366`): field `#[serde(default, skip_serializing_if = "KeysConfig::is_default")] pub keys: KeysConfig`, the matching `Default` line, and `self.keys.resolve()?;` at the end of `validate()`. Bump `CONFIG_VERSION` to 11 and widen the in-memory upgrade range from `1..=9` to `1..=10`; grep tests and `docs/src/content/docs/configuration.md` for the old literal. In `mj-tui/src/setup.rs:189-209` add `"keys"` to the excluded top-level keys so the Settings modal does not try to render string-or-list values it has no editor for.

Tests, in `mj-core/src/config/keys.rs` and `mj-core/src/config/tests.rs`, named in the repository's sentence style: `parse_key_combo_accepts_herdr_syntax_and_named_punctuation`, `format_key_combo_round_trips_every_default_binding`, `default_keybinds_resolve_without_conflicts`, `a_user_binding_displaces_the_default_it_collides_with_silently`, `two_user_bindings_on_one_trigger_fail_validation`, `an_unmodified_printable_direct_binding_fails_validation`, `ctrl_c_and_ctrl_v_cannot_be_direct_bindings`, `a_prefix_that_is_not_a_modified_chord_fails_validation`, `binding_the_prefix_itself_after_the_prefix_fails_validation`, `an_empty_string_unbinds_a_default`, `a_range_binding_is_only_valid_for_switch_workspace`, `switch_workspace_range_yields_nine_indexed_bindings`, `config_load_reports_key_errors_fatally` (through `Config::load_from` on a temp file, following the pattern at `config/tests.rs:616-671`), `unknown_keys_fields_are_rejected`, `keys_section_is_omitted_from_serialized_defaults`.

This milestone compiles and tests on its own; nothing consumes `Keybinds` yet.

### M2: registry, router, event loop, footer and help

This is the large milestone. It changes behaviour, so land it as one coherent set of commits; the footer tests break as soon as the registry changes, which is why footer and help are in the same milestone.

Registry (`mj-tui/src/actions.rs`). Add `CommandId` variants `CycleFocusReverse`, `FocusWorkspaces`, `SwitchWorkspace`, `ToggleTranscriptRendering`, `ToggleDictation`. Delete `KeyHint::alt`, `KeyHint::ctrl`, `KeyHint::is_chord`; keep `KeyHint::plain` and `matches`. On `CommandSpec` rename `keys` to `pane_keys: &'static [KeyHint]` and add `action: Option<mj_core::config::KeyAction>`. Every spec that had an Alt or function key loses it and gains the matching `action` (the map is one-to-one with the table in M1; `CycleFocus` keeps pane key `Tab` and gets `NextPane`; the new `CycleFocusReverse` gets `PreviousPane` and no footer word; `Help` keeps pane key `?` and gets `Help`; `NewSessionWizard` keeps pane keys `n`/`N`; `OpenConfig` loses `F7`; `SelectWorkspacePrevious`/`Next` lose the Ctrl-Page keys). `SwitchWorkspace` has scope Global, is hidden from the palette, and its `dispatch_command` arm does nothing because the router calls a new `DashboardState::select_workspace_index(usize) -> DashboardAction` (sibling of `select_adjacent_workspace` in `mj-tui/src/workspaces.rs`). `FocusWorkspaces` sets `focus = Focus::Workspaces` and `workspace_control_focus = Tabs`. `CycleFocusReverse` calls `cycle_focus(true)`. `ToggleTranscriptRendering` and `ToggleDictation` return two new `DashboardAction` variants of the same names, which mj-cli forwards to the visible chat (in M2 the forwarding may post the notice "No conversation is open" when there is none; M3 fills in the chat calls). Delete `GLOBAL_CHORDS` and `global_chord`; rename `spec_for_key` to `pane_command_for_key` (same body over `pane_keys`, same composer exclusion for characters); rename `global_chord_allowed` to `command_allowed_now`, add the new navigation commands to its `!modal_open()` arm, and add `pub fn survives_chat_modal(CommandId) -> bool` returning the set mj-cli currently inlines (`Help`, `QuitDetach`, `TogglePanePreset`, `Refresh`). Footer ranks within the chord group: create 0, resume 1, read 2, size 3, panes 4, cancel 5, detach 6, web 7, refresh 8, settings 9, rendering 10, palette 11, help 12; dictation has no footer word. Delete `FooterGroup::Function`. Rewrite the stale comment on the `Refresh` arm that talks about F5 shadowing the wizard.

Tests in `actions.rs` change accordingly: `session_transition_commands_bind_no_key` also asserts `Keybinds::default().bindings(action).is_empty()` for those actions; `workspace_manager_runs_from_its_button_without_a_key_or_palette_row` becomes `workspace_manager_has_a_prefix_key_and_no_palette_row`; `no_two_commands_claim_the_same_key_in_one_pane` iterates `pane_keys`; add `every_key_action_maps_to_exactly_one_command`.

Router. New file `mj-tui/src/keybinds.rs` (`mod keybinds;` in `lib.rs`, and `pub use keybinds::KeyRoute;` replacing the `global_chord` re-export at `lib.rs:68`):

    pub enum KeyRoute { Forward, Consumed, Command { id: CommandId, index: Option<usize> } }
    pub(crate) fn key_event_combo(&KeyEvent) -> Option<KeyCombo>;      // crossterm to mj_core, normalized; BackTab -> Tab+shift; SUPER stays super
    pub(crate) fn command_for_action(KeyAction) -> CommandId;           // exhaustive match
    impl DashboardState {
        pub fn keybinds(&self) -> &Keybinds;
        pub fn route_bound_key(&mut self, &KeyEvent) -> KeyRoute;
        pub fn prefix_pending(&self) -> bool;
        pub fn cancel_prefix(&mut self);
        pub(crate) fn key_labels(&self, CommandId) -> Vec<String>;      // pane keys first, then live bindings: ["Enter", "ctrl+b c"]
        pub(crate) fn footer_key(&self, CommandId) -> Option<String>;   // "c" for a chord command, else the first pane key label
    }

`route_bound_key` handles Press and Repeat only and mirrors herdr. Not pending: a direct binding returns `Command`; the prefix itself sets pending and returns `Consumed`; anything else is `Forward`. Pending: the prefix again clears pending and returns `Forward`, so the literal `Ctrl-B` reaches the composer's backward-character arm at `mj-chat/src/chat/keys.rs:226`; `Esc` clears and returns `Consumed`; a prefix binding clears and returns `Command`; anything else clears, posts the notice `ctrl+b <key> is not bound; ctrl+b ? lists keys` (with the live prefix label), and returns `Consumed`. A mouse press while pending cancels it (one line in `handle_mouse`). `DashboardState` gains `prefix_pending: bool` and `keybinds: Keybinds`, initialised in `new()` from `config.keybinds()` and refreshed in `set_config` (`mj-tui/src/ingest.rs:511-526`) before `self.config` is replaced. Remove the Ctrl-PageUp/PageDown arm in `handle_key_at` (`dashboard_input.rs:84-94`). `handle_dashboard_key` and `dashboard_standby.rs:180` call `pane_command_for_key`.

The router must run in exactly one place per key press, and that place is the mj-cli event loop, because the loop forwards the literal prefix through `dispatch_event` into `handle_key_at`, and a second pass there would re-arm the prefix. In `mj-cli/src/dashboard.rs` replace the `global_chord_event(...).filter(...)` arm (lines 525-548) with a match on `context.dashboard.route_bound_key_event(&event)` (a thin wrapper that returns `Forward` for non-key events): `Consumed` ends the event; `Command { id, index }` is dropped when `!command_allowed_now(id)` or when a chat modal is open and `!survives_chat_modal(id)`, otherwise it runs the existing pointer-cancel lines, then `chat.detach()` for `QuitDetach` with a visible chat, `select_workspace_index` for `SwitchWorkspace`, or `dispatch_command`; `Forward` continues into the existing selection and dispatch path. Delete `global_chord_event` and `apply_global_focus_cycle`.

Footer. In `mj-tui/src/render/footer.rs`, `footer_commands` uses `footer_key(id)`; for the chord group the first item's text is prefixed with `ctrl+b then: ` (live prefix label) before fitting, so the reader always sees which key starts the chord. In `mj-chat/src/theme.rs` give `fit_footer_items` and `fit_footer` a `protected: impl Fn(&T) -> bool` parameter and delete the `"F1 "`/`"F2 "` literals; the dashboard protects `Palette` and `Help`; the chat footer marks the same two entries explicitly. Add `pub fn prefix_banner(prefix: &str, help_rhs: &str) -> Line<'static>` in the theme, rendering ` PREFIX ` in the accent style followed by `esc cancel · ctrl+b send · ? keys`. `render_footer` draws the banner whenever `prefix_pending()` is true, before the notice check; `ChatFooter` gains `banner: Option<Line<'static>>` that replaces its groups when set, and `mj-tui/src/combined.rs` passes it when pending. The dashboard footer now reads, at full width and Sessions focus:

    Enter open · Tab pane │ ctrl+b then: c create · g resume · a read · z size · b panes · q detach · u web · shift+r refresh · s settings · t rendering · : palette · ? keys

Help overlay (`mj-tui/src/help.rs`). `help_lines` prints `key_labels(id)` for each row and a first line `prefix: ctrl+b   (edit [keys] in config.toml)`; an unbound command shows `—`. Remove from `COMPOSER_KEYS` every row that names a registry command (`Alt-T`, `Alt-V`, `Alt-N`, `Alt-S`, `Alt-A`, `Alt-G`, `Alt-Q`, `Alt-W`, `F2`, `F4`, `F5`, `F7`), keep the readline rows, and add a row `ctrl+b ctrl+b  send a literal Ctrl-B to the composer (backward one character)`. Key handling: `Esc`, `Enter` and `?` close; `j`/`k`/arrows/PageUp/PageDown/Home/End scroll; `F1` is gone. The `/` filter is added in M4. Palette rows (`palette.rs:443-448`) print `key_labels(id).join(" / ")`.

Hint strings that name the old keys are rebuilt from `key_labels`: `combined.rs:855` ("Press ctrl+b c to create a session or ctrl+b g to resume one."), `combined.rs:869` and `:1014,1023` (cancel and detach), `dashboard_panes.rs:72` ("Select Sessions, Targets, or Quota before cycling the pane size."), `render.rs:278` and `render/sessions.rs:271` (settings), `dashboard_input.rs:517`. In `mj-chat`, the composer footer string at `chat/active/render.rs:329` drops "Alt-T rendering", and the dictation strings at `render.rs:327` and `chat/active/dispatch.rs:259` are reworded to point at the microphone control rather than a key, because the chat crate does not know the live binding.

Test support (`mj-tui/src/test_support.rs`): `prefix_key()` returns `KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)` (never `ctrl_key`, which is `SUPER` on macOS); `chord(&mut DashboardState, CommandId) -> DashboardAction` presses the first live binding of the command through `route_bound_key` and dispatches the result exactly as mj-cli would; `open_new_session_wizard` and `open_palette` wrap it. `alt_key` stays only for readline tests.

Existing tests to rewrite with those helpers (the Plan agent's inventory; verify with `cargo test` and grep for `alt_key`, `F(`, `Alt-`): in `mj-tui/src/tests.rs` the Alt-G, Alt-Z (three), Alt-Q, plain-a, mark-all-read, cancel-operation, F3/F4/F5 tests and the palette helper; in `mj-tui/src/render/tests.rs` every footer string test (`the_footer_is_one_row_that_a_notice_takes_over`, `footer_lists_cancel_only_while_an_operation_is_in_flight`, `footer_ends_with_f1_help_at_every_focus` becomes `footer_ends_with_help_at_every_focus`, `footer_drops_whole_hints_when_the_width_runs_out`, `footer_groups_pane_alt_and_function_keys_in_that_order` becomes `footer_groups_pane_keys_then_prefix_chords_in_rank_order`, `footer_drops_pane_hints_before_alt_hints_and_keeps_help_longest`), the Alt-G, mark-all-read, new-session and F2 tests, `every_footer_hint_dispatches_the_command_it_names`, and the landmark strings near lines 1804-1812; `mj-tui/src/help.rs` tests; `mj-tui/src/wizards/tests.rs` (about twenty-five tests open the wizard with `alt_key('w')`), `dialogs/tests.rs:453`, `ingest/tests.rs`, `review_settings/tests.rs:148`, `setup/tests.rs:85`, `surface_controls.rs:548`, `palette.rs` tests (one uses `global_chord`), `workspaces/tests.rs:567`, `resume/tests.rs:1170` (footer assert becomes `g resume`), `go.rs:289,333`, `docs_screenshots.rs`; in `mj-cli/src/dashboard/tests.rs` the Alt-N, F5, F6/Shift-F6, Alt-S, F4 and modal tests become prefix sequences driven through a `route(&mut dashboard, &[KeyEvent]) -> KeyRoute` helper; in `mj-chat/src/chat/active/tests.rs` the footer fixture at `render.rs:417` and its assertions. In `mj-cli/tests/termination_pty.rs` the quit key becomes `b"\x02q"` (`0x02` is `Ctrl-B`) and the two `b"\x1bn"` writes become `b"\x02c"`; the `No live session` ready marker must be preserved when rewording `combined.rs:855`.

New behavioural tests: in `mj-tui/src/keybinds.rs`, `the_prefix_arms_pending_and_a_bound_key_runs_its_command`, `pressing_the_prefix_twice_forwards_the_literal_key_and_disarms`, `esc_cancels_a_pending_prefix_without_forwarding`, `an_unbound_key_after_the_prefix_is_swallowed_with_a_notice`, `a_direct_user_binding_runs_without_the_prefix` (config `refresh = "f5"`), `set_config_replaces_the_live_bindings`, `shift_and_uppercase_letters_match_the_same_binding` (`prefix+shift+n` matched from `Char('N')` with no modifier, `Char('N')` with SHIFT, and `Char('n')` with SHIFT), `colon_and_question_mark_match_with_or_without_the_shift_flag`, `switch_workspace_digits_carry_their_index`, `a_mouse_press_cancels_a_pending_prefix`; in `render/tests.rs`, `the_footer_shows_the_prefix_banner_while_a_chord_is_pending` (pane focus and composer focus) and `footer_chord_group_starts_with_the_live_prefix` (`prefix = "ctrl+a"`); in `help.rs`, `help_lists_user_bindings_and_marks_unbound_commands`; in `mj-cli`, `the_literal_prefix_reaches_the_composer_as_backward_char` and `a_bound_key_is_dropped_while_a_chat_modal_is_open_unless_it_survives_modals`.

### M3: composer commands move into the registry

In `mj-chat/src/chat/keys.rs` delete the `Alt-V` arm (172-181) and the `Alt-T` arm (288). Make `toggle_render_mode` (`chat/pointer.rs:56`) public and add `pub fn dictation_toggle_action(&self) -> ChatAction` carrying the `voice_available || voice_active` guard. In `mj-chat/src/chat/active/dispatch.rs` add `pub fn toggle_transcript_rendering(&mut self)` and `pub fn toggle_dictation(&mut self)`. In mj-cli's action executor (`mj-cli/src/dashboard/actions.rs`) route the two `DashboardAction` variants to the visible chat, or post "No conversation is open." Tests: `alt_v_is_disabled_until_voice_is_available` becomes `dictation_toggle_is_inert_until_voice_is_available`, `alt_t_toggles_rendering` becomes `toggle_render_mode_flips_between_rich_and_raw` and also asserts that `Alt-T` is now inert in the composer; new mj-cli test `prefix_t_toggles_rendering_of_the_visible_chat_from_a_pane`.

### M4: shared list conventions and the help filter

In `mj-chat/src/components/scope.rs`, the shared list navigation (`list_selection` around line 1516 and the method around 1113) receives the modifiers as well as the code and adds `j` down, `k` up, `G` last, `ctrl+d` eight down, `ctrl+u` eight up. A focused text field keeps these as text because fields return `Interaction::Edit` before list handling; relax the `ordinary` modifier gate only for the two ctrl keys on `ChoiceList`. Delete the now-redundant local `j`/`k` remaps in `mj-tui/src/resume.rs:1329-1336` and `mj-tui/src/wizards/dashboard.rs:468-478`. The support-pane lists in `dashboard_input.rs:379-400` are not forms, so add `G`, `ctrl+d`, `ctrl+u` arms beside their existing `j`/`k`. The palette's query owns letters, so it gains only the ctrl keys.

Help overlay filter: `HelpOverlay` gains `query: String` and `search_focused: bool`. `/` focuses the filter; while focused, printable characters and Backspace edit it and reset scroll, `Ctrl-U` clears it, `Esc` clears and unfocuses, `Enter` closes, arrows and Page keys still scroll. `help_lines(dashboard, query)` keeps a row when the query is a case-insensitive substring of its key label, command label or description, and keeps a group heading only if it has rows. The overlay title becomes ` ↑↓ scroll · / filter · Esc closes ` and a filter line is drawn under it whenever the filter is focused or non-empty. Tests: `vim_keys_move_a_choice_list_but_a_text_field_keeps_them_as_text`, `shift_g_jumps_to_the_last_row_and_ctrl_d_u_page_by_eight`, `help_filter_narrows_rows_by_key_or_label_and_esc_clears_it`, `help_closes_on_enter_and_question_mark_but_not_while_filtering`.

The readline audit the user asked for needs no code: mj's composer (`mj-chat/src/chat/keys.rs:223-298`) and `TextField` (`mj-chat/src/text_input.rs:265-332`) already implement every row of herdr's text-field table (Left/Right, Ctrl-B/F, Home/End, Ctrl-A/E, Alt-B/F, Backspace/Ctrl-H, Delete/Ctrl-D, Ctrl-U/K, Ctrl-W, Alt-Backspace, Ctrl-Backspace, Alt-D, Ctrl-Y). Record that in the retrospective.

### M5: documentation and screenshots

`docs/src/content/docs/terminal-surface.mdx`: replace the "Global keys" table (118-137) with a "Prefix key" section explaining the model in two paragraphs, the default table written in `prefix+` form, the double-press rule, and a pointer to `[keys]`; rewrite the pane-sizing prose (104-118) and the "Help, footer, and terminal setup" section (236-252) so the Alt/Meta paragraph becomes a note that tmux users must press `ctrl+b` twice or change the prefix. `quickstart.mdx:98-108`: new table. `troubleshooting.md:166-180`: replace the "Alt shortcuts arrive as Escape plus a letter" entry with "The prefix key does nothing" (tmux or screen is eating it; press it twice or set `prefix`), and add "A `[keys]` edit did not take effect" (validation errors are fatal at start; while running, the daemon keeps the last good config and logs the error, so run `mj doctor` or restart to see it). `configuration.md`: add a `## Keys [keys]` section between `[advanced]` and `[phone]` covering the syntax, `prefix+`, string-or-list values, `""` to unbind, `1..9`, the default table, the validation rules, and the herdr note; update the version line at 41. Update `sessions.md`, `containers.md`, `cli-reference.md`, `durability.md`, `workspaces-bundles.md`, `targets.md` wherever they name an Alt or F key, and `README.md:145`. Regenerate the documentation screenshots with `cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored` and commit the changed SVGs.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel3`, outside the restricted sandbox, on the dev profile.

After M1:

    cargo test -p brokk-mj-core
    cargo clippy -p brokk-mj-core --all-targets -- -D warnings

After each of M2 to M5, and at the end:

    cargo test
    cargo clippy --all-targets -- -D warnings

Manual check after M2 (and again at the end):

    cargo run -p mj-cli

Press `ctrl+b`: the footer shows ` PREFIX  esc cancel · ctrl+b send · ? keys`. Press `?`: the help overlay opens with rows such as `ctrl+b c` beside "Create session". Press `Esc`. Focus the composer of an open session, type `abc`, press `ctrl+b` twice: the cursor moves left one character. Press `ctrl+b` then `m`: the footer notice reads `ctrl+b m is not bound; ctrl+b ? lists keys`. Add to `config.toml`:

    [keys]
    prefix = "ctrl+space"
    refresh = ["prefix+shift+r", "f5"]

Within two seconds `ctrl+b` is backward-character on a single press, `ctrl+space ?` opens help, and `f5` refreshes. Then set `help = "prefix+space"` together with `pane_preset = "prefix+space"` and restart: `mj` exits with an error naming both fields.

Commit after each milestone with a message that names the milestone; do not push.

## Validation and Acceptance

Acceptance is behavioural: with a default config, every command listed in the M1 table runs from its prefix chord from both a pane and the composer; no `Alt` letter other than the composer's editing keys does anything; no function key does anything; `ctrl+b ctrl+b` in the composer moves the cursor left; the footer and the help overlay never name a key that is not live; a `[keys]` edit is honoured within the daemon's reload interval; an invalid `[keys]` entry stops `mj` from starting with an error naming the field; the PTY termination test passes with the new quit sequence; `cargo test` and `cargo clippy --all-targets -- -D warnings` are clean.

## Idempotence and Recovery

Every step is an ordinary source edit under git; re-running a milestone is safe. If a milestone is abandoned part-way, `git checkout -- <files>` restores the tree; commits are per milestone so a later milestone can be reverted alone. The config version bump means a `config.toml` written by the new binary is refused by an older one with a clear "newer version" error, which is the existing behaviour for every version bump; no data migration is involved.

## Artifacts and Notes

M1, `cargo test -p brokk-mj-core` (dev profile, outside the sandbox):

    test result: ok. 371 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.14s

M1 validation error text for the conflicting-bindings example (`help` and `pane_preset` both `"prefix+space"`):

    keys.pane_preset = "prefix+space": already bound by keys.help

M2 dashboard footer, full width, Sessions focus, nothing in flight:

    Enter open · Tab pane │ ctrl+b then: c create · g resume · a read · z size · b panes · q detach · u web · shift+r refresh · s settings · t rendering · : palette · ? keys

With a launch in flight the cancel hint takes its rank between panes and detach:

    … · b panes · shift+c cancel launch · q detach · …

M2 footer while the prefix is pending, at every focus:

     PREFIX  esc cancel · ctrl+b send · ? keys

M2 help overlay with the default bindings (the rows, without the frame):

    prefix: ctrl+b   (edit [keys] in config.toml)

    Sessions pane
      Enter                   Open session
      n / N / ctrl+b c        Create session
      ctrl+b a                Mark all read
      Space                   Fold project

    Selected session
      —                       Restart session
      ctrl+b shift+t          Rename session
      —                       Container settings
      —                       Stop session
      —                       Move session…
      —                       Delete session

    Targets pane
      Enter / e               Target actions

    Quota pane
      Enter / e               Rename profile

    Panes
      Tab / ctrl+b tab        Next pane
      ctrl+b shift+tab        Previous pane
      ctrl+b z                Pane size
      ctrl+b b                Pane preset

    Settings
      —                       Change fast-start setup
      —                       Manage agent profiles
      —                       Manage machines and runtimes
      ctrl+b s                Open settings
      —                       Next spinner style

    Anywhere
      ctrl+b g                Resume a session
      ctrl+b shift+c          Cancel operation
      ctrl+b shift+n          Workspaces
      ctrl+b w                Focus the workspace strip
      ctrl+b p                Previous workspace
      ctrl+b n                Next workspace
      ctrl+b 1-9              Switch to workspace by number
      ctrl+b u                Web viewer
      ctrl+b shift+r          Refresh targets and quotas
      ctrl+b q                Detach from this terminal
      ctrl+b t                Toggle transcript rendering
      ctrl+b m                Dictation
      ctrl+b :                Command palette
      ? / ctrl+b ?            Help

    Composer
      Enter                     send the prompt, or queue it while a turn is running
      Shift-Enter / Alt-Enter   start a new line
      Tab                       accept a completion, or move to the next pane
      Esc                       cancel the running turn or shell command
      PgUp / PgDn               scroll the transcript
      Up / Down                 walk prompt history, or move within the prompt
      Ctrl-R                    search prompt history
      Ctrl-V                    paste from the system clipboard
      Ctrl-A / Ctrl-E           start or end of the line
      Ctrl-B / Ctrl-F           back or forward one character
      Alt-B / Alt-F             back or forward one word
      Ctrl-H / Ctrl-D           delete before or after the cursor
      Alt-D                     delete the word after the cursor
      Ctrl-W                    delete the word before the cursor
      Ctrl-U / Ctrl-K           kill to the start or end of the line
      Ctrl-Y                    yank what was killed
      Ctrl-C                    stash the prompt into history and clear it
      Ctrl-P / Ctrl-N           previous or next line, or history
      ctrl+b ctrl+b             send a literal Ctrl-B to the composer (backward one character)

    ↑↓ scroll · Esc closes

Each row's description follows its label on the same line; it is left out above
so the key column stays readable.

M2, `cargo test` (dev profile, outside the sandbox), the crates this milestone
touched:

    mj_chat       test result: ok. 527 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.62s
    mj_core       test result: ok. 372 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.01s
    mj_tui        test result: ok. 505 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.47s
    mj            test result: ok. 118 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.15s
    mj_controller test result: ok. 1347 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out; finished in 63.84s
    mj_worker     test result: ok. 462 passed; 0 failed; 8 ignored; 0 measured; 0 filtered out; finished in 80.81s

M2, the PTY termination suite with the new `\x02q` quit sequence and `\x02c`
create sequence:

    test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.46s

`cargo clippy --all-targets -- -D warnings` exits 0.

One caution about running the whole suite: `mj-cli/tests/termination_pty.rs`
starts real daemons, so two concurrent `cargo test` runs in the same checkout
make `fixture_teardown_before_the_dashboard_is_ready_removes_its_storage` fail
with "another Mjolnir controller is already using …". Run the suite alone.

M3, `cargo test` (dev profile, outside the sandbox), the crates this milestone
touched:

    mj_chat  test result: ok. 527 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.56s
    mj_tui   test result: ok. 505 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.24s
    mj       test result: ok. 119 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.10s

The PTY suite, run on its own afterwards:

    test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.20s

`cargo clippy --all-targets -- -D warnings` exits 0.

Record here, as the work proceeds: the final footer string at full width, the help overlay text with defaults, a `cargo test` summary line per milestone, and the validation error text produced by the conflicting-bindings example.

M4, `cargo test` (dev profile, outside the sandbox), the crates this milestone
touched:

    mj_chat  test result: ok. 529 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.59s
    mj_tui   test result: ok. 508 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.23s

The PTY suite, run on its own afterwards:

    test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.44s

`cargo clippy --all-targets -- -D warnings` exits 0.

M4 help overlay while the filter is focused on `palette`, at the top of the
popup:

    filter: palette▏
    prefix: ctrl+b   (edit [keys] in config.toml)

    Anywhere
      ctrl+b :                Command palette

    ↑↓ scroll · / filter · Esc closes


### Final

`cargo test` (dev profile, outside the sandbox, whole workspace):

    mj_chat             test result: ok. 529 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.56s
    mj_checkpoint       test result: ok. 129 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.99s
    mj_client           test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.91s
    mj_controller       test result: ok. 1347 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out; finished in 63.30s
    mj_core             test result: ok. 372 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.43s
    mj_review           test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
    mj_transcript       test result: ok. 81 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
    mj_tui              test result: ok. 508 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.29s
    mj_worker (lib)     test result: ok. 462 passed; 0 failed; 8 ignored; 0 measured; 0 filtered out; finished in 34.53s
    mj_worker (bin)     test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
    worker_environment  test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.91s
    worker_proxy_exit   test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s
    worker_proxy_long_root test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
    mj (bin)            test result: ok. 119 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.95s
    daemon_startup      test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.37s
    import_e2e          test result: ok. 1 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 0.00s
    instance            test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
    logging             test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.39s
    store_divergence    test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.73s
    termination_pty     test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.34s

The doc-test targets all report `0 passed; 0 failed`.

`cargo clippy --all-targets -- -D warnings` exits 0 with no diagnostics.

`cargo test -p brokk-mjolnir --test termination_pty`, run on its own afterwards:

    test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.54s

M5 regenerated four screenshots under `docs/src/assets/screenshots/`. All four
carry the new footer row, and `command-palette.svg` also shows the live bindings
in the palette's key column:

    ctrl+b then: c create · g resume · a read · z size · b panes · q detach · u web · shift+r refresh · s settings · : palette · ? keys

No `Alt-` or function-key text remains in any of them.

## Interfaces and Dependencies

No new crates. `mj-core` gains `config::keys` with the types listed in M1; `mj-tui` gains `keybinds` with `KeyRoute`, the router methods, `key_event_combo`, `command_for_action`, and the label helpers; `mj-chat::theme::fit_footer_items` and `fit_footer` gain a `protected` predicate and the theme gains `prefix_banner`; `DashboardAction` gains `ToggleTranscriptRendering` and `ToggleDictation`; `DashboardState` gains `select_workspace_index`, `command_allowed_now` (renamed from `global_chord_allowed`) and `survives_chat_modal`. `mj_tui::global_chord`, `GLOBAL_CHORDS`, `spec_for_key` (renamed), `global_chord_event`, `apply_global_focus_cycle`, `KeyHint::alt`, `KeyHint::ctrl`, `KeyHint::is_chord` and `FooterGroup::Function` are removed.
