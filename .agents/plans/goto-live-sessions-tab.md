# Add a Live tab to the session dialog so any running session can be reached from anywhere

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

Delegation note: per the user's standing instruction, Fable owns design, planning and review; each milestone below is implemented by an Opus or Sonnet agent, one milestone per agent, with Fable reviewing the result against the milestone's acceptance text before the next starts. The implementing agent must not edit this file; Fable updates `Progress` and the living sections after each review.

## Purpose / Big Picture

Mjolnir's terminal dashboard (the program `mj`) shows a list of sessions down the left side. A "session" is one coding-agent conversation with its own working copy of a repository. Sessions are grouped into "workspaces", which the dashboard draws as a horizontal row of tabs across the top, much like tabs in a browser.

The problem is that the session list only ever shows the sessions belonging to the workspace whose tab is currently selected. If an agent is running in another workspace, there is no way to see it, let alone jump to it, without first switching workspace tabs and hunting through the list that appears. The only cross-workspace movement that exists today is `prefix+o`, which jumps to whichever session most urgently needs a person. That is useful when something is waiting for you, and useless when you simply want to get to a session you have in mind.

After this change, pressing `prefix+g` opens a dialog listing **every running session in every workspace** on one screen, each row naming the workspace it lives in, with a search box and the same state filters the main list uses. Pressing Enter on a row takes you there: if the session is in another workspace, the dashboard switches to that workspace tab and opens the session's conversation, exactly as though you had clicked the tab and then the row.

You can see it working by starting `mj` with sessions in at least two workspaces, pressing `prefix+g`, and confirming that sessions from both workspaces appear in one list with a WORKSPACE column, and that Enter on a session from the workspace you are *not* currently viewing switches tabs and opens that conversation.

This reuses the dialog that `prefix+g` already opens. That dialog currently has three tabs, all of which list sessions you **could** have: sessions that stopped and can be resumed, sessions from other agent tools that Mjolnir could adopt, and sessions that only survive in the search index. This plan adds a fourth kind of list, for sessions you **already** have and that are running right now, and makes it the tab the dialog opens on.

## Progress

- [x] (2026-09-19 00:18Z) M1 Extract the cross-workspace focus sequence out of `step_attention` into `focus_session_anywhere`, with no user-visible change. Commit `bc38c55b`; 623 tests unchanged, clippy clean. One ownership fix beyond the two anticipated: `view.selected_session_id = Some(session_id.to_owned())`.
- [x] (2026-09-19 00:34Z) M2 Add the `ResumeTab::Live` variant and `ResumeRowStatus::Running`, build Live rows from every workspace on the `DashboardState` side, give the Live tab its own substring search, and make Enter focus the selected session. Commit `6a245c54`; 625 tests (623 + 2), clippy clean. Beyond the spec: the tab count was a literal in **five** places, not four — `ControlKind::Tabs { len: 3 }` would have dropped clicks on the fourth tab — so all now read `ResumeTab::COUNT`; Left stepped by a literal 2 and now steps `COUNT - 1`; `move_recovery` is gated to `Hel` rows; `is_listed_top_level_session` became `pub(crate)`. Two items handed to M4: switching from Live to a history tab with text typed must re-issue the index query (the edit guard suppressed it), and `search_focus_pending` is honored only inside `apply_wiki_search`, so opening on Live will not focus the box by itself.
- [x] (2026-09-19 00:45Z) M3 Replace the PROFILE and TARGET columns with a WORKSPACE column on the Live tab only. Commit `7f4ad4cd`; 626 tests (625 + 1), clippy clean. Header and row renderer share one `layout.profile == 0` rule so they cannot disagree; the new test compares the header's and a row's starting columns across tabs, and the implementer confirmed it catches a deliberately reintroduced separator by a two-cell offset.
- [x] (2026-09-19 01:13Z) M4 Open the dialog on the Live tab, add the state filters, rename the command's user-facing words, update the documentation. Commit `ccbf19bf`; 628 crate tests (626 + 2), workspace suite green across 31 crates (3982 passed), clippy clean. `g sessions` fit at width 200 with no hint dropped. The "focus the box once the index is ready" promise became `ResumeDialog::take_pending_search_focus`, honored at open, on index answers, and on entering a tab; `switch_resume_tab` now returns the action so leaving Live with a query issues the index search on both the arrow and click paths. Twenty-two existing tests that assumed the dialog opened on the Mjolnir tab now walk the tab strip first.
- [ ] M5 Settle three user-visible words and one focus choice M4 left alone (see Outcomes), then write the retrospective.

Use timestamps of the form `(2026-09-18 21:30Z)` when checking an item off, so a later reader can see how long each milestone took.

## Surprises & Discoveries

- Observation: the dialog's search would empty the Live tab. With any text in the search box, the row builder keeps only rows the SessionWiki index returned. A running session has no index rank unless a search happened to match its transcript, so a Live list built the obvious way would vanish the moment someone typed a session's name into the box — the opposite of what the box is for.
  Evidence: `mj-tui/src/resume.rs:753` reads `.filter(|row| !searching || row.wiki_rank.is_some())`, and the doc comment at lines 726-729 says "There is one search path". The Live tab needs a second one; see the Decision Log.

- Observation: the number of tabs is written down as a literal in four places, and getting one wrong is a runtime panic, not a compile error. The per-tab search-hit counts are a fixed-size array `[usize; 3]`, indexed by `ResumeTab::index()`. Renumbering `Archive` to 3 while leaving the array at three entries compiles cleanly and panics on the first search.
  Evidence: the return type at `mj-tui/src/resume.rs:735`, the literal `[0usize; 3]` at line 740, the reset `[0; 3]` at line 785, and the field `resume_hit_counts` that line 795 assigns into. The arrow-key handler at line 1359 has its own literal, `% 3`.

- Observation: the rows for the three existing tabs are built by a free function over `&State` and `&Config`, but the rule that decides whether a session is listed at all, and the attention level a state filter needs, are both methods on `DashboardState` that read fields only it has. So Live rows cannot be built inside the existing builder without either duplicating those rules or threading half of `DashboardState` through a function that was deliberately kept pure.
  Evidence: `build_resume_rows` at `mj-tui/src/resume.rs:730-775` and `merged_resume_rows` at line 533 take `&State`; `is_listed_top_level_session` at `mj-tui/src/dashboard_sessions.rs:349-356` reads `self.state.subagents`, `self.transition_kind(..)` and `self.config.advanced.show_stopped_sessions`.

- Observation: the search box is disabled until the SessionWiki index reports ready, because today every search goes to the index. Live search does not need the index, so that gate has to learn which tab is showing or the box will refuse typing on a tab that could answer immediately.
  Evidence: `ResumeDialog::search_enabled` at `mj-tui/src/resume.rs:323-325` returns `self.wiki_status.state == WikiIndexState::Ready`; `search_placeholder` at 328 prints "Indexing…" from the same state.

- Observation: the sequence that moves the dashboard to a session in a different workspace already exists in full, inlined inside the attention-stepping function. It is 40 lines and includes several details that are easy to omit if reimplemented: leaving a sub-agent workspace first, creating the target workspace's saved view state if it has never been visited, clearing a filter that would hide the session you asked for, and un-collapsing the project heading the session sits under.
  Evidence: `mj-tui/src/dashboard_sessions.rs:661-701`, inside `step_attention`.

- Observation: enumerating sessions across all workspaces has an established idiom in this file, and it is subtle. `is_listed_top_level_session` takes a workspace id as its second argument, and the cross-workspace callers pass **the session's own** workspace id, which makes the workspace check pass for every session while still applying the other listing rules.
  Evidence: `attention_queue` at `mj-tui/src/dashboard_sessions.rs:549-576` calls `self.is_listed_top_level_session(session, &session.workspace_id)`.

- Observation: Enter in the dialog dispatches on the row's key, not its status, after two status checks that only reject lost or unavailable rows. So a new key variant gets its own arm and cannot be mistaken for a resume.
  Evidence: `activate_selected_resume_row` at `mj-tui/src/resume.rs:1421-1466`; the `match row.key` is at line 1435.

## Decision Log

- Decision: add a fourth tab to the existing dialog rather than building a separate dialog on `prefix+g` and moving the resume list to another key.
  Rationale: the two kinds of list are genuinely different — sessions you already have versus sessions you could have — but they want the same shell: a searchable list with a transcript preview, a tab strip, and row selection. A separate dialog would duplicate all of that. Keeping one key also avoids spending a second prefix letter and avoids breaking the muscle memory of `prefix+g`. The cost, accepted knowingly, is that one key now leads to both kinds of list, with the tab strip carrying the distinction.
  Date/Author: 2026-09-18, Claude Opus 5 with Jonathan Ellis.

- Decision: the Live tab is the **first** tab (index 0), shifting the existing three to indices 1, 2 and 3, and the dialog opens on it.
  Rationale: the dialog opens on Live, so Live must be at the left end of the strip for the arrow keys to read naturally — Right then walks from "already have" through to the most archival "could have". The cost is that resuming a stopped session takes one extra keypress (Right) after `prefix+g`.
  Date/Author: 2026-09-18, Claude Opus 5 with Jonathan Ellis.

- Decision: Live rows are built on the `DashboardState` side, by a new method `live_resume_rows`, and are used by `rebuild_resume_rows` when the Live tab is showing; the pure builder `build_resume_rows` is left to the three existing tabs.
  Rationale: the listing rule and the attention level are `DashboardState` methods (see Surprises). Building Live rows where those methods are reachable reuses them; building them in the pure function would mean copying the rules or threading state through a function kept pure on purpose. The doc comment on `build_resume_rows` that claims a single search path must be corrected to say there are two and why.
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: on the Live tab, the search box is a case-insensitive substring match over the session's title, id and workspace name, applied while building the Live rows. The SessionWiki index is not consulted for the Live tab, and `search_enabled` returns true on the Live tab regardless of index state.
  Rationale: the point of searching a list of running sessions is to find one by name, now. The index is for transcripts of sessions that may no longer exist, and waiting on it, or emptying the list when it has no rank for a running session, would make the box useless here. This mirrors how the main session list's `/` search works (`session_matches_filter` in `mj-tui/src/dashboard_sessions.rs:217`).
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: add a status variant `ResumeRowStatus::Running` for Live rows rather than reusing `Resumable`.
  Rationale: `Resumable` is documented as "Enter opens the resume wizard". Enter actually dispatches on the row key, so reuse would have worked, but the name would mislead the next reader. The variant costs three match arms: `is_recoverable` returns true, `warning` and `explanation` return `None`.
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: Delete on a Live row does nothing except set a notice pointing at the Sessions pane.
  Rationale: the dialog's Delete destroys a session's *record*, which is the right tool for a stopped or lost session. Stopping or deleting a running session has confirmations and consequences that belong to the dashboard's own session actions, and the dialog should not grow a second copy.
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: the preview pane shows nothing for a Live row unless a SessionWiki search matched that session's transcript, in which case it shows the matching passages as it does for the other tabs.
  Rationale: the pane is fed from the index. Showing the live transcript would be a second transcript renderer inside a dialog, which is out of scope. State this limitation in the docs rather than hide it.
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: the command's user-facing words change — label "Sessions", footer word "sessions", description covering both kinds of list — but the `[keys] resume` configuration field keeps its name.
  Rationale: `KeysConfig` is declared with `deny_unknown_fields`, so any existing `config.toml` containing `resume = "..."` would become a hard validation error and fail to load. Revisit only alongside a release that already breaks configuration. The footer word costs two more columns (`g resume` to `g sessions`); the footer had roughly ten columns of slack at width 200 after the filter-letter change, so this fits, and the render test that pins the footer string must be updated.
  Date/Author: 2026-09-18, Claude Fable 5.1.

- Decision: the Live tab opens with the session list focused, not the search box; `/` moves to the box as it already does. The history tabs are unchanged in themselves.
  Rationale: with the box focused, `j`/`k`, Enter and the state letters typed into the box until the person pressed `↓` or Tab, which the Milestone 4 docs had to explain. herdr's navigator and mj's own Sessions pane both open on the list and use `/` for search; matching them means the keys that matter on a list of running sessions work the instant the dialog appears. Consequence to settle by evidence in Milestone 5: the "focus the box once the index answers" flag, `search_focus_pending`, has no purpose on a tab that never asks the index, and if it fires on a late answer it would pull focus off the list. If no production path opens the dialog on a history tab, the flag and `take_pending_search_focus` are removed rather than guarded.
  Date/Author: 2026-09-19, Jonathan Ellis with Claude Fable 5.1.

- Decision: the Sessions pane's **Resume** button is renamed **Open**, and the dialog's modal title becomes **Sessions**.
  Rationale: both words described the history tabs. The dialog now opens on running sessions, so "Resume" on the button misstates what pressing it shows, and a title of "Resume a session" contradicts the tab beneath it. "Open" names what Enter does on the tab the dialog opens on; "Sessions" matches the command label and lets the tab strip say which list is showing.
  Date/Author: 2026-09-19, Jonathan Ellis with Claude Fable 5.1.

- Decision: state filters apply to the Live tab only.
  Rationale: the filters narrow by what a session is doing now. A stopped, importable or archived session has no current state, so offering the filters there would invite a filter that silently matches nothing.
  Date/Author: 2026-09-18, Claude Opus 5.

- Decision: extract the cross-workspace focus sequence before adding the tab, as its own milestone with no user-visible change.
  Rationale: it is 40 lines of state manipulation that the new tab needs verbatim. Copying it would leave two copies to drift apart. Extracting it first, and proving both callers still behave, means the new tab is a small addition on top of a known-good primitive.
  Date/Author: 2026-09-18, Claude Opus 5.

## Outcomes & Retrospective

To be written as each milestone completes, and summarised at the end. Compare the result against the Purpose section: can a person reach a running session in another workspace from one screen, and do the preview, search and filtering behave as described?

## Context and Orientation

Everything in this plan is in the `mj-tui` crate, at `mj-tui/` from the repository root. `mj-tui` draws the dashboard and interprets keystrokes; it holds no network or process logic. The type that holds all dashboard state and answers keys is `DashboardState`, spread across several files by topic: `mj-tui/src/dashboard_sessions.rs` for the session list, `mj-tui/src/dashboard_workspaces.rs` for workspaces, and `mj-tui/src/resume.rs` for the dialog this plan changes.

Some terms used throughout, in plain language.

A **workspace** is a named group of sessions, drawn as one tab in the horizontal strip at the top of the dashboard. A workspace has an id (a string) and a display name; `DashboardState::workspace_display_name(&self, workspace_id: &str) -> &str` at `mj-tui/src/dashboard_workspaces.rs:49` turns the first into the second.

A **session record** is `SessionRecord`, and every session the daemon knows about lives in the map `self.state.sessions`, keyed by session id. Each record carries the field `workspace_id` saying which workspace it belongs to.

The **prefix** is a two-step keyboard convention borrowed from tmux: you press `ctrl+b`, release it, then press a second key. `prefix+g` therefore means "`ctrl+b`, then `g`". The dialog this plan changes is what `prefix+g` opens.

A **modal** or **dialog** is a panel drawn over the dashboard that takes the keyboard until dismissed. This one is the struct `ResumeDialog` at `mj-tui/src/resume.rs:219`, held in the dashboard's `mode` field as `Mode::ResumeDialog(..)`, and constructed in one place, at `mj-tui/src/resume.rs:872`, where line 875 sets its initial `tab`.

**SessionWiki** is a separate search index of session transcripts. The dialog queries it when text is typed in the search box, and the preview pane below the list shows what it returns. Its readiness is `dialog.wiki_status.state`.

The dialog today, all in `mj-tui/src/resume.rs`:

The enum `ResumeTab` at line 76 has three variants, `Hel`, `Import` and `Archive`. It has three methods: `index()` mapping each variant to 0, 1 or 2; `from_index(usize)` mapping back, with a catch-all `_` arm for the last case; and `includes(&ResumeRow) -> bool`, which decides whether a row belongs on that tab by matching on the row's key.

The enum `ResumeRowKey` at line 114 identifies a row: `Hel(String)` for a Mjolnir session record keyed by session id, `Native(HarnessKind, String)` for a session belonging to another agent tool that Mjolnir has not adopted, and `Archive(String)` for one that survives only in the index. `ResumeTab::includes` keys entirely off this enum, which is why adding a tab means adding a key variant too. `ResumeRow::session_id()` at line 201 returns the session id only for the `Hel` variant; it is used both by the checkpoint-size decoration in the row builder and by a `retain` in `rebuild_resume_rows` (line 796) that hides rows whose session has an operation in flight.

The struct `ResumeRow` at line 166 is one row's content: `key`, `profile_id`, `title`, `origin` (where the session ran plus the project it opened), `details`, `last_activity_ms`, `status`, and a group of fields relating to index matches, of which `wiki_rank: Option<usize>` matters here: a row has one only if the index ranked it for the current query.

The enum `ResumeRowStatus` at line 126 says what a row can do: `Resumable`, `Importable`, `Restorable`, `Lost`, `DataLoss`. Its `is_recoverable()` is used only to style the title (line 2050); `warning()` and `explanation()` describe the two failure variants.

Rows are built in two layers. `merged_resume_rows(config, state, profiles, wiki)` at line 533 turns records, native scans and index hits into `ResumeRow`s for all three tabs; `build_resume_rows` at line 730 filters those to the current tab, keeps only index-ranked rows while searching (line 753), decorates them with checkpoint sizes (lines 757-765), sorts by index rank while searching (lines 769-773), and returns them together with the per-tab hit counts `[usize; 3]`. `DashboardState::rebuild_resume_rows` at line 782 calls it and stores the result in `self.resume_rows` and `self.resume_hit_counts`, then applies the in-flight `retain`. Every change to the dialog's inputs calls `rebuild_resume_rows`.

Columns are laid out by `struct RowLayout { title, profile, origin, activity }` at line 1470, filled by `row_layout(width: u16) -> RowLayout` at line 1477, which has one caller at line 1644. The header row is drawn by `resume_header_line(layout: &RowLayout)` at line 2015, printing PROFILE, TARGET, LAST ACTIVE and SESSION with two spaces between cells. Row cells are drawn further down: the origin cell around lines 2057-2064 and the profile cell at line 2082, with the same two-space separators.

Three places give each tab its words, as `match` arms over `ResumeTab`: line 1745 the footer hint, line 1780 the label on the action button (`"Resume"`, `"Import"`, `"Restore"`), and line 1826 the heading above the list. The tab strip itself lists the tabs as an array of `(ResumeTab, &str)` pairs at lines 1797-1801, and `empty_search_message` at line 1861 uses a copy of that array to say where else a search matched.

Arrow keys switch tabs at lines 1357-1361 with `ResumeTab::from_index((dialog.tab.index() + step) % 3)`. Delete on a row calls `destroy_selected_resume_row` (line 1400), which matches on the row key. Enter calls `activate_selected_resume_row` (line 1421), which rejects lost or unavailable rows and then matches on the row key at line 1435, calling `self.cancel_modal()` to close the dialog before acting.

The relevant part of the session list, in `mj-tui/src/dashboard_sessions.rs`:

`is_listed_top_level_session(&self, session: &SessionRecord, workspace_id: &str) -> bool` at line 349 decides whether a session is listed at all. Its first check is `session.workspace_id == workspace_id`; the others exclude sub-agent sessions and, unless `config.advanced.show_stopped_sessions` is on, stopped ones.

`attention_queue(&self)` at line 549 is the existing example of walking every workspace at once: it iterates `self.state.sessions.values()` and filters with `self.is_listed_top_level_session(session, &session.workspace_id)`, passing each session's own workspace id so the workspace check always passes.

`attention_level(&self, session_id: &str) -> AttentionLevel` is what the state filters test against. `SessionStateFilter` in `mj-tui/src/lib.rs:116` has `from_letter(char) -> Option<Option<Self>>` at line 128 (`a` yields `None`, meaning all) and `admits(AttentionLevel) -> bool` at line 148.

`step_attention(&mut self, delta: isize) -> DashboardAction` at line 643 picks a session from the queue and moves to it. Lines 661 to 701 are the part Milestone 1 extracts, and they do five things in order: if the dashboard is inside a sub-agent workspace, leave it with `close_subagent_workspace()`; if the target's workspace is not the active one, look up or create that workspace's saved view state in the `workspace_views` map, record the target session as its selection, set its focus to the composer, and return `DashboardAction::SelectWorkspace { workspace_id }` so the host switches tabs; otherwise un-collapse the project heading the session sits under, clear the session filter if it would hide the session, select the session, and return `open_selected_session()`.

`DashboardAction` is how `mj-tui` asks the program around it to do something it cannot do itself. `SelectWorkspace { workspace_id }` means "switch to this workspace tab"; the host then restores that workspace's recorded selection and opens its conversation, which is why recording the selection before returning is what makes the jump land on the right session.

Test helpers in `mj-tui/src/tests.rs`: `dashboard_with_attention_mix()` at line 3168 builds a dashboard whose sessions span two workspace ids and names them with `set_workspace_names`; `running_session()` builds one live record. Dialog-specific tests live in `mj-tui/src/resume/tests.rs`, where `focus_resume_control` at line 143 shows how to drive the dialog's controls.

## Plan of Work

### Milestone 1 — make the cross-workspace jump callable

The goal is that the 40-line sequence inside `step_attention` becomes a method anyone can call, and nothing a user can see changes. At the end, `step_attention` is shorter and there is a new method that takes a workspace id and a session id and moves the dashboard there.

In `mj-tui/src/dashboard_sessions.rs`, in the same `impl DashboardState` block that holds `step_attention`, add:

    /// Moves the dashboard to one session, wherever it lives.
    ///
    /// A session in another workspace is reached by recording it as that
    /// workspace's selection and asking the host to switch: the host restores
    /// the selection when the tab changes and opens its conversation, exactly
    /// as it does for a tab the person clicks.
    pub(crate) fn focus_session_anywhere(
        &mut self,
        workspace_id: &str,
        session_id: &str,
    ) -> DashboardAction

Move the body of `step_attention` lines 661 to 701 into it verbatim, replacing `target.workspace_id` with the `workspace_id` parameter and `target.session_id` with `session_id`. Two ownership details: the original moves an owned `target.workspace_id` into `DashboardAction::SelectWorkspace`, so the new version needs `workspace_id.to_owned()` there; and `workspace_views.entry(..)` needs an owned key, so `workspace_id.to_owned()` there too. Keep `close_subagent_workspace()` before the workspace comparison, as it is now.

Then replace those lines in `step_attention` with `self.focus_session_anywhere(&target.workspace_id, &target.session_id)`.

Acceptance is that the existing tests pass unchanged, because no behavior moved. Do not add a test asserting the method exists; confirm instead that the attention-stepping tests, which cover both the same-workspace and cross-workspace paths, still pass. Find them with `grep -n "attention" mj-tui/src/tests.rs`.

### Milestone 2 — the Live tab exists, searches by name, and Enter goes there

At the end of this milestone, `prefix+g` still opens on the Resume tab, but pressing Left once reaches a new Live tab listing every running session in every workspace; typing in the search box narrows that list by name; and Enter on a row moves the dashboard to that session.

Start with the enums in `mj-tui/src/resume.rs`. Add `Live` as the first variant of `ResumeTab` and renumber: `Live` is 0, `Hel` is 1, `Import` is 2, `Archive` is 3. Update `index()` and `from_index()`; note the catch-all `_` arm in `from_index` must now return `Archive` while `2` returns `Import`. Add `Live(String)` to `ResumeRowKey`, holding the session id, and give `ResumeTab::includes` the arm pairing `Self::Live` with `ResumeRowKey::Live(_)`. Give `ResumeRow::session_id()` a `Live(session_id)` arm returning it, so the in-flight `retain` in `rebuild_resume_rows` applies to Live rows too. Add `Running` to `ResumeRowStatus` with `is_recoverable` true and `warning`/`explanation` `None`.

Next, every literal that encodes the tab count. Change `% 3` at line 1359 to `% 4`. Change the hit-count array from three entries to four in all four places: the return type of `build_resume_rows` (line 735), the literal at line 740, the reset at line 785, and the declared type of the `resume_hit_counts` field on `DashboardState` (find it with `grep -rn "resume_hit_counts" mj-tui/src/`). Leave the loop at line 743 over `[Hel, Import, Archive]` as it is — the Live tab has no index hits to count — and leave `empty_search_message`'s array alone for the same reason, but make its fallback text read "No matching running sessions" when `dialog.tab` is `Live`. Add `(ResumeTab::Live, "Live")` as the first entry of the tab-strip array at line 1797.

Then the rows. Add to `DashboardState`, in `mj-tui/src/resume.rs` next to `rebuild_resume_rows`:

    /// Every running session in every workspace, newest activity first,
    /// narrowed by the search box's text when there is any.
    fn live_resume_rows(&self, dialog: &ResumeDialog) -> Vec<ResumeRow>

It walks `self.state.sessions.values()` with the cross-workspace idiom, `self.is_listed_top_level_session(session, &session.workspace_id)`, and additionally requires `session.state.is_active()` so that a stopped session shown by the display setting does not appear as "running". For each, build a `ResumeRow` with `key: ResumeRowKey::Live(session.id.clone())`, `profile_id: session.last_profile.clone()`, `title: session.display_title().to_owned()`, `origin: self.workspace_display_name(&session.workspace_id).to_owned()` (Milestone 3 gives this its column; until then it is simply what the TARGET column happens to show), `details` as the project name from `session.project_name(&self.config)`, `last_activity_ms` the way the `Hel` builder computes it at line 555, `status: ResumeRowStatus::Running`, and the remaining fields `false`/`None`. If the search box has text, keep only rows where the lower-cased query is a substring of the lower-cased title, session id or workspace name, mirroring `session_matches_filter` in `mj-tui/src/dashboard_sessions.rs:217`. Sort by `last_activity_ms` descending.

In `rebuild_resume_rows`, when `dialog.tab == ResumeTab::Live`, use `self.live_resume_rows(dialog)` for `self.resume_rows` and leave `self.resume_hit_counts` at all zeros; otherwise call `build_resume_rows` as now. The `retain` that follows applies in both cases. In `build_resume_rows`, gate the checkpoint-size decoration (lines 757-765) to `ResumeRowKey::Hel` rows so a running session that was once resumed from a checkpoint does not show a stale size; and rewrite the doc comment at lines 726-729 to say there are two search paths — the index for the three history tabs, a name match for the Live tab — and why.

Make the search box usable on the Live tab: `search_enabled` (line 323) returns `self.tab == ResumeTab::Live || self.wiki_status.state == WikiIndexState::Ready`, and `search_placeholder` (line 328) returns `None` on the Live tab. Check that the code which issues index queries on each edit of the search box does not also fire on the Live tab; if it does, guard it with the tab, because the answer would be ignored anyway.

Give the tab its words, as new arms beside the existing ones: the footer hint at line 1745 is `"Enter opens · ←/→ tabs · / searches · Tab moves"` — *opens*, not *resumes*, and no Delete — the action-button label at line 1780 is `"Open"`, and the heading at line 1826 is `"Running sessions · every workspace · newest first"`.

Make Enter act. In `activate_selected_resume_row`, add an arm to the `match row.key` at line 1435:

    ResumeRowKey::Live(session_id) => {
        let Some(workspace_id) = self
            .state
            .sessions
            .get(&session_id)
            .map(|session| session.workspace_id.clone())
        else {
            self.notices.set("That session is no longer running.");
            return DashboardAction::None;
        };
        self.cancel_modal();
        self.focus_session_anywhere(&workspace_id, &session_id)
    }

Make Delete refuse. In `destroy_selected_resume_row` (line 1400), add an arm for `ResumeRowKey::Live(_)` that sets the notice "Stop or delete a running session from the Sessions pane." and returns `DashboardAction::None`.

Acceptance is behavioral. Add two tests to `mj-tui/src/resume/tests.rs`, following the style of `selecting_a_row_resumes_a_hel_record_and_imports_a_native_session` at line 1058 and using `dashboard_with_attention_mix` from `mj-tui/src/tests.rs` for two workspaces. The first opens the dialog, moves Left to the Live tab, and asserts that the rows' keys include `ResumeRowKey::Live` entries whose sessions span both workspace ids; then types part of one session's title into the search box and asserts only matching rows remain, and that the list is not empty. The second selects a Live row whose session is in the inactive workspace, presses Enter, and asserts the returned action is `DashboardAction::SelectWorkspace` naming that workspace and that the dialog has closed. Both should fail before the change and pass after; write each first and watch it fail for the right reason.

### Milestone 3 — the columns say what matters for a running session

At the end of this milestone, the Live tab shows a WORKSPACE column where the other tabs show PROFILE and TARGET.

The profile and the target answer "what was this session configured to run as", which is what you need when deciding whether to resume something. For a running session the question is "where is it", and the answer is its workspace. So on the Live tab, drop those two columns and spend their width on a workspace column plus a wider session title.

`row_layout` at line 1477 has one caller, at line 1644. Give it a `tab: ResumeTab` parameter. For `Live`, compute a layout whose `profile` is zero, whose `origin` holds a workspace name (the same `24.min(width / 3).max(8)` the target column uses is fine), and whose `title` takes the width the profile column and its separator released. Give `resume_header_line` at line 2015 the same parameter, printing WORKSPACE instead of TARGET on the Live tab and, when `layout.profile` is zero, omitting the PROFILE cell **and its two-space separator**, so no stray gap opens at the left of every row. Apply the same zero-width rule to the row-cell rendering at lines 2057-2082, which draws the profile cell and its separator; if that code does not already have the layout's tab in scope, pass it through.

The row's `origin` already holds the workspace name from Milestone 2. Update the doc comment on `ResumeRow::origin` to say it holds the workspace name for Live rows.

Acceptance: the Live tab's header reads WORKSPACE where the Resume tab's reads TARGET, each Live row names its workspace, and switching tabs with the arrow keys re-lays out the columns with no leading gap. Add a render test to `mj-tui/src/resume/tests.rs`, alongside `resume_table_has_headers_repeated_profiles_and_last_active_values` at line 1273, asserting the Live tab's header line contains WORKSPACE and not PROFILE, and that the Resume tab's still contains both PROFILE and TARGET.

### Milestone 4 — open on Live, filter by state, rename the words, and document it

At the end of this milestone `prefix+g` opens on the Live tab, the `a`, `b`, `w`, `i` and `d` keys narrow it the way they narrow the main session list, the command is called "Sessions" wherever a person sees its name, and the documentation describes all of this.

Open on Live: change the initial `tab: ResumeTab::Hel` where `ResumeDialog` is constructed (search for `Mode::ResumeDialog(ResumeDialog {`) to `ResumeTab::Live`. Two consequences found during Milestone 2 must be handled here. First, the dialog's "focus the search box once it can take focus" flag, `search_focus_pending`, is honored only inside `apply_wiki_search`, which runs when an index answer arrives; on the Live tab no index query is issued, so nothing would ever clear the flag and the box would not take focus on open. Honor the flag at construction time when the tab is Live, or in `switch_resume_tab`, so the box is focused immediately on a tab whose search needs no index. Second, Milestone 2 stopped the search box's edits from issuing index queries while the Live tab is showing, because the answer would be discarded; the cost is that switching from Live to a history tab with text already typed shows no index matches until the next keystroke. Make `switch_resume_tab` issue `wiki_search_action()` when leaving the Live tab with a non-empty query, and add a test: type on Live, press Right, and assert an index search action is returned.

State filters: add a field to `ResumeDialog`:

    /// The state the Live tab is narrowed to, or `None` for all. Ignored on
    /// the other tabs, which list sessions that have no current state.
    pub(crate) live_state: Option<SessionStateFilter>,

initialised to `None` at construction. In `live_resume_rows`, after the name match, drop rows whose `self.attention_level(&session.id)` the filter does not `admits`. In the dialog's key handler, in the block that already handles `/` and Delete when `focused != Search` (around line 1343), add an arm for a plain letter that `SessionStateFilter::from_letter` accepts, taken only when `dialog.tab == ResumeTab::Live`: set `live_state` from the result (`a` yields `None`) and call `rebuild_resume_rows` then `select_resume_row(0)`. Add ` · a/b/w/i/d filter` to the Live tab's footer hint. When a filter is set, say so in the list heading, for example "Running sessions · blocked · newest first", using `SessionStateFilter::label()`.

The command's words: in `mj-tui/src/actions.rs`, the `CommandSpec` for `CommandId::ResumeDialog` has a `label` and `description`; change the label to "Sessions" and the description to say it opens on every running session in every workspace, with the resumable, importable and archived lists on the tabs to the right. Change its `footer` from the current word to `footer_word!("sessions")`. This lengthens the footer by two columns; the render test `footer_groups_pane_keys_then_prefix_chords_in_rank_order` in `mj-tui/src/render/tests.rs` pins the exact footer string (`g resume` becomes `g sessions`) and must be updated. If that test shows a hint being dropped at width 200, stop and report rather than trimming something else.

Documentation. `docs/src/content/docs/terminal-surface.mdx` has the prefix chord table around line 222; its `prefix+g` entry must describe a dialog that opens on running sessions across every workspace and reaches the three history lists with the arrow keys, and the section on the resume dialog (search for "Resume") needs the Live tab, the name search, the state letters, and the preview limitation from the Decision Log. `docs/src/content/docs/configuration.md` describes each `[keys]` field around line 164; update the description of `resume` without renaming the field. Search `docs/src/content/docs/sessions.md` for sentences that describe `prefix+g` as opening the resume list only.

Acceptance is the Purpose section, exercised by hand: start the dashboard with sessions in two workspaces, press `prefix+g`, see running sessions from both workspaces with a WORKSPACE column, type to search, press `b` to keep only blocked ones and `a` to show all again, press Right to reach the Resume list, press Left to come back, and press Enter on a session in the other workspace to land in it. Add one test to `mj-tui/src/resume/tests.rs` that opens the dialog, asserts it is on the Live tab, presses `b`, and asserts only rows whose attention level is blocked remain, then presses `a` and asserts the full list returns.

## Concrete Steps

Work from the repository root, `/home/jonathan/Projects/hel4`.

After each milestone, run the crate's tests and the linter:

    cargo test -p brokk-mj-tui
    cargo clippy --all-targets -- -D warnings

Expect `test result: ok.` with zero failures, and clippy to print only a `Finished` line. Both must be run outside any restricted sandbox with elevated permissions, because the wider test suite opens loopback TCP and Unix sockets and a sandboxed run fails with `EPERM` or hangs, which is not a valid result.

Before the final commit of the plan, run the whole workspace's tests:

    cargo test

Expect every line of output matching `test result:` to say `ok` and `0 failed`. Both commands build the dev profile, which is where debug assertions and integer-overflow checks are active; do not validate on the release profile, which silently drops both.

Commit after each milestone. Stage only the files that milestone touched, naming them explicitly rather than using `git add -A`, because other agents may be working in sibling worktrees that share this checkout.

To see the dashboard rather than only its tests, the established loop in this repository is to drive `mj` under `tmux` so keystrokes can be sent and frames captured. Starting a dashboard talks to the daemon and therefore to real sessions; prefer a throwaway configuration by setting `MJ_CONFIG_DIR` and `MJ_DATA_DIR` to temporary paths.

## Validation and Acceptance

The test the feature must pass is the scenario in the Purpose section, restated as a sequence a person can follow.

Start the dashboard with at least two workspaces, each holding at least one running session, and with the dashboard showing workspace A. Press `ctrl+b` then `g`. A dialog opens with a tab strip whose leftmost tab is selected and reads Live, and a list below it headed "Running sessions · every workspace · newest first". The list contains sessions from both workspace A and workspace B. The column header row reads WORKSPACE where the Resume tab's reads TARGET, and each row names the workspace it belongs to.

Type part of a session's name. The list narrows to sessions whose name contains it, immediately, without waiting on the search index. Clear the box.

Move the selection to a session belonging to workspace B and press Enter. The dialog closes, the workspace tab strip switches to workspace B, and that session's conversation opens. This is the behavior that does not exist before the change and is the point of the plan.

Press `ctrl+b` then `g` again, and press `b`. The list narrows to sessions that are blocked, meaning waiting for a person or failed, and the heading says so. Press `a`. The full list returns. Press Right. The Resume tab appears, listing sessions that are not running and can be resumed, with PROFILE and TARGET columns restored. Press Left. The Live tab returns.

For automated coverage, the tests named in the milestones are the meaningful ones: that Live rows span workspaces and that the name search narrows them without emptying them; that Enter on a row from an inactive workspace yields `DashboardAction::SelectWorkspace` naming that workspace; that the header line differs per tab; and that the state letters narrow and restore the Live list. Each should fail before its milestone's change and pass after.

Do not add tests that restate the implementation, such as asserting that `ResumeTab::index()` returns 0 for `Live`.

## Idempotence and Recovery

Every step here is an additive source edit and can be repeated safely; there is no migration, no schema change, and nothing written outside the working tree. If a milestone goes wrong, `git diff` shows exactly what changed and the milestone's files can be restored with `git checkout --`, but run `git status` first and keep anything you did not expect to find, because this checkout is shared with sibling worktrees.

Milestone 1 is the only step that moves existing code. If its tests fail, the likely cause is one of the two ownership details called out in its section, or `close_subagent_workspace()` having drifted after the workspace comparison.

If Milestone 2's tab arithmetic is wrong, the symptom is arrow keys skipping a tab or wrapping early; check the `% 4` at line 1359 and `from_index`'s catch-all. If searching panics with an index out of bounds, one of the four `[usize; 3]` sites was missed. If the Live list is empty while searching, the name match was not applied and the index-rank filter is still running for the Live tab.

## Artifacts and Notes

The extraction in Milestone 1 should leave `step_attention` ending like this, where before it held the whole sequence inline:

        let target = match position {
            Some(position) => {
                let len = queue.len() as isize;
                queue[(position as isize + delta).rem_euclid(len) as usize].clone()
            }
            None if delta < 0 => queue[queue.len() - 1].clone(),
            None => queue[0].clone(),
        };
        self.focus_session_anywhere(&target.workspace_id, &target.session_id)

A useful check after Milestone 2, before any rendering work, is to print the row keys the dialog built for the Live tab in a test and confirm they carry two distinct workspace ids. That proves the enumeration spans workspaces independently of whether the columns or the key handling are finished.

## Interfaces and Dependencies

No new crates or dependencies. Everything is within `mj-tui`, which already depends on `mj-core` for `SessionRecord` and the workspace types.

At the end of Milestone 1, in `mj-tui/src/dashboard_sessions.rs`:

    impl DashboardState {
        pub(crate) fn focus_session_anywhere(
            &mut self,
            workspace_id: &str,
            session_id: &str,
        ) -> DashboardAction;
    }

At the end of Milestone 2, in `mj-tui/src/resume.rs`:

    pub(crate) enum ResumeTab { Live, Hel, Import, Archive }
    pub(crate) enum ResumeRowKey { Live(String), Hel(String), Native(HarnessKind, String), Archive(String) }
    pub(crate) enum ResumeRowStatus { Running, Resumable, Importable, Restorable, Lost, DataLoss }

    impl DashboardState {
        fn live_resume_rows(&self, dialog: &ResumeDialog) -> Vec<ResumeRow>;
    }

with `ResumeTab::index` mapping `Live` to 0, `Hel` to 1, `Import` to 2 and `Archive` to 3, `from_index` inverting it, `includes` pairing `Live` with `ResumeRowKey::Live(_)`, `build_resume_rows` returning `(Vec<ResumeRow>, [usize; 4])`, and `DashboardState::resume_hit_counts: [usize; 4]`.

At the end of Milestone 3:

    fn row_layout(width: u16, tab: ResumeTab) -> RowLayout;
    fn resume_header_line(layout: &RowLayout, tab: ResumeTab) -> Line<'static>;

At the end of Milestone 4, `ResumeDialog` has `pub(crate) live_state: Option<SessionStateFilter>`, and `crate::SessionStateFilter::from_letter` and `admits` remain the only source of the letter-to-state mapping.

Reuse, rather than reimplement: `DashboardState::focus_session_anywhere` for movement, `DashboardState::is_listed_top_level_session` for what is listed, `DashboardState::workspace_display_name` for the workspace column, `DashboardState::attention_level` with `SessionStateFilter::admits` for the state filter, and the existing `ResumeDialog` shell for the tab strip, search box, preview pane and row selection.

## Revision Notes

- 2026-09-18, initial version (Claude Opus 5). Written after an audit comparing mj's hotkeys with herdr's found that mj had no equivalent of herdr's cross-machine `goto` navigator, and that the session list's scoping to one workspace was the reason. Related follow-up issues are BrokkAi/mjolnir#1093 and #1094; neither overlaps this plan.

- 2026-09-18, review revision (Claude Fable 5.1). Four corrections and their consequences. (1) The plan said searching would work on the Live tab; it would have emptied the tab, because `build_resume_rows` keeps only index-ranked rows while searching — Milestone 2 now gives the Live tab a name-based search and records the two-path design in the Decision Log. (2) The plan said a missed hit-count array would fail to compile; it is a fixed `[usize; 3]` indexed at runtime and would panic instead — all four sites are now named. (3) Live rows were to be built in the pure builder, which cannot reach the listing rule or attention levels — they are now built by `DashboardState::live_resume_rows`. (4) The search box is gated on the index being ready, which the Live tab does not need — `search_enabled` now learns the tab. Also added: a `Running` status instead of reusing `Resumable`; Delete refusing on Live rows; the preview limitation; the `g sessions` footer word with its render-test consequence; the exact construction site of `ResumeDialog` (line 872, `tab` at 875); and named test helpers.
