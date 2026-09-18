# Make the terminal dashboard tell you where to go next

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir's terminal dashboard (`mj`, drawn by the `mj-tui` crate and driven by `mj-cli`) already knows a great deal about every session: whether an agent is working, whether it has asked a question and is waiting, whether it finished and you have not read the answer. What it does not do is *route* that knowledge to you. If a session two workspaces away asks a question, the only signal is a coloured `!` on a row you are not looking at. There is no key that jumps to the session that needs you, no bell, no terminal-title change, no way to search the live session list, and several smaller inconsistencies that make the surface harder to learn than it needs to be.

Herdr (https://github.com/herdrdev/herdr) is a tmux-style multiplexer for coding agents that gets this right: a priority-sorted agent panel, `prefix+o` to jump to the agent that needs attention, in-app or terminal or system notifications, a searchable session navigator with state filters, git branch on every row, and a three-line first-run tour. This plan brings the same usability to Mjolnir's dashboard without changing its architecture (one conversation surface fed by the daemon, not a terminal multiplexer).

After this plan is complete a user can: press `prefix+o` from anywhere and land on the session that most needs them; hear a bell and see their terminal tab title change when an agent blocks or finishes while they are elsewhere; see a count of waiting sessions on every workspace tab and folded project heading; press `/` in the Sessions pane and filter by name or by state letter; find "Create session" in the command palette by typing "cre"; see the git branch of every worktree session on its row; open a changed-files overlay for the selected session; run the dashboard in a 64-column phone terminal; read the last several notices instead of only the latest; run with `NO_COLOR` and an ASCII symbol set; and get a one-time hint on first launch that tells them the prefix key exists.

## Progress

Milestone numbers match the "Plan of Work" section.

- [x] (2026-09-18 21:30Z) M1 Attention routing: `prefix+o` next-attention command, `prefix+shift+o` previous, `[advanced] session_order = "priority"`, attention badges on workspace tabs and folded project headings. Documented in terminal-surface, sessions, and configuration pages.
- [x] (2026-09-18 23:10Z) M2 Notifications: `[notify]` config (mode off/terminal/system, bell, delay, title), BEL and window title from the terminal loop after each frame, macOS/Linux desktop notification via `osascript`/`notify-send` off the loop, suppression for the visible session, Setup section under Display, documented in configuration and terminal-surface pages.
- [x] (2026-09-19 00:20Z) M3 Finding things: `/` search in the Sessions pane with `b w i d a` state letters and an empty-result hint, fuzzy palette ranking (prefix, then in-order label match scored by word starts and adjacency, then description) with a Recent group of the last five commands, Create/Resume/Workspaces listed in the palette. Naming: left as is (see Decision Log); the Manage runtimes description now says "target" so either word finds it.
- [x] (2026-09-19 01:30Z) M4 Keyboard consistency: one-time tmux/screen prefix collision notice (client-hints.json beside config.toml), a letter for every confirmation button derived from its label and printed under the text, `manage_machines` and `restart_daemon` bindable, Stop and Restart ask only while the agent is mid-turn, Esc closes help when its filter is empty, Esc on a pane clears the notice. The first-launch prefix hint from M7 landed here too because it shares the hints store.
- [x] (2026-09-19 03:00Z) M5 Per-session context: the checkout's branch, upstream distance, and changed-file count on the session title line (read on the target about once a minute per visible live session, through the same executor path go mode uses), and a `prefix+d` changed-files overlay with per-file kinds and line counts. Context-window usage was dropped (see Surprises & Discoveries).
- [x] (2026-09-19 06:30Z) M6a Terminal integration: the notice bar keeps a thirty-entry history behind a **Recent messages** palette command and counts failures that overwrite each other (`2 failures · latest: …`); a Monochrome theme (`theme = "mono"`, also forced by `NO_COLOR`) that carries focus and selection with bold and reverse video; an ASCII symbol set (`[advanced] symbols`, or automatic on the Linux console and non-UTF-8 locales) covering status marks, transitions, badges, pane controls, borders, rules, ellipses, quota bars, scrollbars, footer separators, the spinner, and the branch text.
- [x] (2026-09-19 08:00Z) M6b Narrow layout: between 60 and 79 columns the workspace tabs and a compact Sessions list stack above the conversation, Targets and Quota become one summary row each below it, and every hitbox follows; the "Terminal too small" floor moved to 60 columns. The second transcript column is dropped: upstream's conversation split panes (`.agents/plans/conversation-split-panes.md`, merged 2026-09-19) already show several conversations side by side.
- [x] (2026-09-19 08:40Z) M7 Onboarding and docs: the Terminal surface page no longer claims a Commands button in the Sessions pane, the sessions page lists the ASCII status marks, and the Setup screenshot caption names Notifications. The first-launch prefix hint landed in M4; per-feature docs landed with each milestone.

## Surprises & Discoveries

- Observation: `mj-tui/src/render_changes.rs` is not a diff view despite its name; it is repaint invalidation for clocks and spinners. There is no changed-files view anywhere in the terminal surface; `mj diff` exists only on the CLI.
  Evidence: the module doc at `mj-tui/src/render_changes.rs:1-7`.
- Observation: The palette hides the four commands new users type first (Create session, Resume, Workspaces, Switch workspace) on purpose, because each has a visible button. Typing "create" in the palette matches nothing.
  Evidence: `PALETTE_HIDDEN` at `mj-tui/src/actions.rs:835-841`.
- Observation: Provider-reported usage deliberately excludes context occupancy: `mj-core/src/usage.rs` opens with "Context occupancy is deliberately excluded", and no harness reports a context-window percentage over the relay. Showing an estimate would be a guess, so the per-session context figure was dropped from M5.
  Evidence: `mj-core/src/usage.rs:1`.
- Observation: The controller already reads a session's checkout branch on its target for `mj go` (`session_working_context`), through the target's own process executor, so container and SSH sessions get a branch without any host-side git.
  Evidence: `mj-controller/src/controller/backend.rs`, `session_working_context`; the git status probe is its sibling `session_git_status`.
- Observation: While M1 to M5 were in progress, master gained tiled conversation panes (`prefix+v`, `prefix+minus`, `prefix+h/j/k/l`, `prefix+x`) and a 2.13.0 release. The merge conflicted only where both sides appended to the same key tables and test file; the two semantic changes were `current_session_id` becoming a per-pane method and `WorkspaceViewState` gaining a `conversation_layout` field. The pinned second transcript planned for M6 is superseded by those panes.
  Evidence: `git log ade2fadb..origin/master`; conflicts in `mj-core/src/config/keys.rs`, `mj-tui/src/keybinds.rs`, `mj-tui/src/tests.rs`, `docs/src/content/docs/configuration.md`.
- Observation: The delete confirmation has three buttons but its body text advertises only `Y` and `N`; the third button (delete the branch too) is reachable only by Tab.
  Evidence: `mj-tui/src/dialogs/render.rs:1006-1014` and `mj-tui/src/dialogs.rs:1158-1180`.

## Decision Log

- Decision: Implement all seven milestones in one plan, committed per milestone, rather than seven plans.
  Rationale: The user asked for the whole review to be implemented as a series. The milestones share the command registry (`mj-tui/src/actions.rs`) and the configuration types, so one document keeps the shared decisions in one place. Each milestone is still independently verifiable.
  Date/Author: 2026-09-18, Claude.
- Decision: The narrow layout reuses the one frame routine with three geometry switches (full-width sidebar band, forced compact Sessions, support panes below) rather than a second renderer.
  Rationale: Upstream's tiled conversation panes made the frame routine the only place that knows how panes, standby composers, and transitions are drawn; duplicating it for narrow terminals would have to be kept in step by hand. The stacked band is one more entry for the existing height allocator, which already handles a fixed-height band.
  Date/Author: 2026-09-19, Claude.
- Decision: Every glyph the dashboard draws goes through one `Glyphs` table in `mj-chat/src/theme.rs`, selected per frame like the theme, rather than a per-site ASCII fallback.
  Rationale: The audit found thirty-odd distinct glyphs across a dozen files; a table is the only way a new glyph cannot be added without an ASCII counterpart, and the test that draws the whole dashboard in ASCII and rejects any non-ASCII cell enforces it.
  Date/Author: 2026-09-19, Claude.
- Decision: Milestones land on master as they complete, by merging `origin/master` into the working branch and pushing, rather than accumulating on the worktree branch.
  Rationale: The user asked for milestones to merge as they happen and then for commits to go to master directly; a long-lived branch had already drifted sixteen commits behind.
  Date/Author: 2026-09-19, user and Claude.
- Decision: The next-attention key is `prefix+o` and the previous-attention key is `prefix+shift+o`.
  Rationale: Herdr uses `prefix+o` for "focus the notification target"; users moving between the tools keep one habit. `o` is unbound in Mjolnir today.
  Date/Author: 2026-09-18, Claude.
- Decision: Attention priority order is: waiting for input (a pending question or review decision) > failed (error, lost, unreachable, failed review) > unread agent activity > working > idle > inactive. Ties break by most recent activity. The queue lists waiting, failed, and unread.
  Rationale: This is the order in which a human would want to be interrupted, and matches Herdr's Blocked > Done > Working > Idle order. A failed session was added as its own level between waiting and unread because a launch failure needs a person as much as a question does, and the row already draws it red.
  Date/Author: 2026-09-18, Claude.
- Decision: The attention ladder is reordered to Failed > Unreachable > Waiting > Unread > Working > Idle > Inactive, with `Unreachable` as a new level, and a badge names the most urgent level's glyph with the total count of sessions that need a person (` ×1`, ` !2`, ` ✓3`) instead of separate waiting and unread counts. The band colour, the row symbol, the workspace tabs, the folded project headings, the minimized Sessions title, and the footer count all read that one summary.
  Rationale: A failure or an unreachable worker makes everything else the session says untrustworthy, so it has to outrank a question rather than sit below it; an unreachable worker is not the same as a failure and was previously flattened into one. Two counts made a person read two numbers to learn one thing, and the old minimized title counted only elicitations, which disagreed with the row symbols beside it. Consequences: an idle session with unread output is now the attention colour instead of blue, a working session with unread output is the activity colour instead of attention, and a failed review or failed stop draws `×` even while a question is pending.
  Date/Author: 2026-09-18, user and Claude.
- Decision: Cross-workspace jumps reuse the host's tab-switch path rather than a new action. `step_attention` writes the target session into that workspace's saved view state and returns `SelectWorkspace`; the host's `select_workspace` restores the selection and opens its chat through `follow_selected_session`.
  Rationale: No new `DashboardAction` and no new host code; the same path a clicked tab takes already saves drafts, cancels a pending attach, and opens the restored selection.
  Date/Author: 2026-09-18, Claude.
- Decision: Docs are updated in the milestone that adds a feature, not deferred to M7. M7 keeps only the onboarding hint and the pre-existing doc drift fixes.
  Rationale: A milestone is not verifiable if its keys are undocumented, and the plan says each milestone stands alone.
  Date/Author: 2026-09-18, Claude.
- Decision: Notifications live in a new `[notify]` section of `config.toml`, not under `[advanced]`.
  Rationale: `[advanced]` is documented as diagnostic display tuning. Notifications are a first-class preference with several fields (mode, sound, debounce) and deserve their own section, mirroring Herdr's `[toast]`/`[sound]` split in a single table.
  Date/Author: 2026-09-18, Claude.
- Decision: "Runtimes" (Setup, `[targets.<id>]`) and "Targets" (the pane, the docs page) stay as they are.
  Rationale: The docs define a target as a runtime on a machine, and Setup edits the runtime half; renaming either would break that model and every doc page that uses it. The findability problem is solved by fuzzy palette search over label and description, and the Manage runtimes description now contains the word target.
  Date/Author: 2026-09-18, Claude.
- Decision: Stop and Restart confirm only when the session's attention level is Working; an idle session and a retry of a failed stop run at once.
  Rationale: The review called the asymmetry with Delete a rough edge, but a confirmation on every Stop would tax the deliberate palette action that the palette-only access already protects. The case a mis-click costs something is an agent mid-turn, so that is the case that asks.
  Date/Author: 2026-09-19, Claude.
- Decision: One-time hints are a JSON file beside `config.toml`, not rows in the daemon database.
  Rationale: They belong to the person's terminal habits, need no migration classification, and reset with a fresh configuration directory.
  Date/Author: 2026-09-19, Claude.
- Decision: The Sessions pane search is a live filter (rows disappear as you type) rather than a separate navigator dialog.
  Rationale: The dashboard has one screen by design (`mj-tui/src/combined.rs:1-6`); an extra modal for search would contradict that. State-letter filters (`a` all, `b` blocked, `w` working, `i` idle, `d` done) work the same way as Herdr's navigator so the vocabulary carries over.
  Date/Author: 2026-09-18, Claude.

## Outcomes & Retrospective

Complete, 2026-09-19. Every item of the usability review is on master except the two that were consciously dropped: per-session context-window usage (no harness reports it) and a pinned second transcript (upstream's tiled conversation panes cover it). Against the purpose: a person can now press `prefix+o` and land on the session that needs them, hear a bell and read the terminal title from another tab, search the live list, find any command by typing part of its name, see each checkout's branch and open its changed files, run the dashboard on a 60-column phone terminal or a color-less console, read the notices that overwrote each other, and learn the prefix key on first launch. What remains is what only a live terminal can prove: the bell and title on real emulators and multiplexers, the desktop notification helpers on macOS and Linux, and the stacked layout under a real touch keyboard; the e2e harness covers the layout guard and the hints, not those.

Lessons: the shared-table pattern (attention level, confirmation letters, glyphs) paid for itself every time, because each later milestone consumed the earlier table instead of adding a second scale; and merging master after every milestone rather than at the end kept the one large merge to four both-sides-appended conflicts.

M2 landed with four notify tests; the episode model (one report per session per attention level, started when first seen) is what keeps a question the agent answers itself quiet and keeps a mode change from replaying old events. The host writes BEL and the title after the frame, so a bell never precedes the row it is about. There is no sound file support: Herdr's per-agent sounds were left out because BEL already reaches every terminal and the desktop notification carries the platform sound.

M1 landed with 535 mj-tui tests passing (six new), clippy clean, and the full workspace suite green. The row symbol now reads the shared `attention_level`, so the queue, the badges, and the symbol cannot disagree. The footer hint `o next (N)` only appears while something is waiting, which doubles as a global signal on every pane.

## Context and Orientation

The dashboard is one screen. `mj-tui/src/combined.rs` draws a Workspaces tab row, a Sessions sidebar, the Conversation transcript of the selected session, the Prompt composer, the Targets and Quota support panes, and a one-line footer. Every other surface (wizards, dialogs, help, the command palette, Setup) is a modal `Mode` drawn on top; the list of modes is the `Mode` enum in `mj-tui/src/lib.rs`.

"The daemon" is the long-lived `mj daemon` process that owns sessions; the dashboard is a client that receives state snapshots from it and sends it actions. In `mj-tui` this shows up as `DashboardState` (in `mj-tui/src/lib.rs`), which holds a `State` snapshot (`self.state.sessions` is a map of session id to `SessionRecord`), per-session detail in `self.session_details` (a map of session id to `SessionDetail`, defined in `mj-tui/src/ingest.rs`), and the configuration in `self.config`. The dashboard returns a `DashboardAction` from every input handler; `mj-cli/src/dashboard.rs` and `mj-cli/src/dashboard/actions.rs` turn those into daemon calls. `mj-tui` never performs I/O itself.

"Attention state" of a session is computed today in `SessionRowFacts::status_symbol` in `mj-tui/src/render/sessions.rs` (around line 680). A session "needs input" when `SessionDetail::pending_elicitations` is non-empty (an elicitation is a form or question the agent sent over the Agent Client Protocol and is waiting on). A session has "unread" activity when `SessionDetail::has_unread()` is true. A session is "working" when `detail.activity.is_idle(detail.current_turn_started_at)` is false.

The command registry is `COMMANDS` in `mj-tui/src/actions.rs`. Every command is one `CommandSpec` with an id, label, description, scope, optional non-configurable pane keys, an optional configurable `KeyAction`, a footer word, and an availability function. The footer (`mj-tui/src/render/footer.rs`), the help overlay (`mj-tui/src/help.rs`), and the palette (`mj-tui/src/palette.rs`) are all generated from this table. `DashboardState::dispatch_command` in the same file runs a command by id.

Configurable keys are the `KeyAction` enum and its defaults in the `key_actions!` macro in `mj-core/src/config/keys.rs`. Adding a variant there requires a matching arm in `command_for_action` in `mj-tui/src/keybinds.rs` (the match is exhaustive on purpose). The prefix router `DashboardState::route_bound_key` is in `mj-tui/src/keybinds.rs`. Keys are written in `config.toml` under `[keys]` as `prefix+o`, `ctrl+alt+o`, and so on.

The Sessions pane lists sessions of the active workspace grouped by project. `DashboardState::ordered_sessions` in `mj-tui/src/dashboard_sessions.rs` decides the order (project groups sorted by name, sessions by creation). `sessions_rows` turns that into headings and rows. Folded projects are `self.collapsed_project_keys`. Workspace tabs are drawn by `render_workspace_tabs` in `mj-tui/src/workspaces.rs`; the ordered workspace ids come from `dashboard.workspace_ids()`.

Notices are the one-line message that replaces the footer hints. `Notices` is in `mj-chat/src/chat.rs` (around line 1466): one slot, protected failures with a minimum display time, shared by every view through an `Arc<Mutex<..>>`. `DashboardState::set_notice` in `mj-tui/src/ingest.rs` writes to it.

The terminal itself is owned by `mj-cli/src/main.rs` (raw mode, alternate screen, mouse capture, around line 1435) and the event loop in `mj-cli/src/dashboard.rs`. This is the only place that can write escape sequences such as BEL (`\x07`) or an OSC title (`\x1b]0;title\x07`).

Configuration types live in `mj-core/src/config/` (`ui.rs` holds `AdvancedConfig`, `SessionsSide`, the spinner, and the theme). The Setup dialog that edits configuration in the terminal is generated from a JSON schema in `mj-tui/src/setup/schema.rs`, so a new config field needs a schema entry with a label before it appears in Setup.

Tests for `mj-tui` are colocated `#[cfg(test)]` modules; `mj-tui/src/test_support.rs` has `drawn(dashboard, width, height)` to render into a test terminal and return the rows as strings, `key(code)` to make a key event, and fixtures for sessions. Run every `cargo test` outside the restricted sandbox because the suite uses loopback sockets.

## Plan of Work

### M1 Attention routing

Add an `AttentionLevel` enum to `mj-tui/src/dashboard_sessions.rs`, ordered `Waiting > Unread > Working > Idle > Inactive`, with `DashboardState::attention_level(&self, session_id) -> AttentionLevel` computed from the same facts `status_symbol` uses (pending elicitations, review phases awaiting a decision, unread, activity). Make `status_symbol` call it so the two cannot drift. Add `DashboardState::attention_queue(&self) -> Vec<(String /*workspace*/, String /*session*/)>` listing every non-subagent session across all workspaces whose level is `Waiting` or `Unread`, sorted by level then by most recent activity (`SessionDetail::last_activity_at_ms`, newest first).

Add `CommandId::NextAttention` and `CommandId::PreviousAttention` to the registry with labels "Next session needing you" and "Previous session needing you", scope `Global`, footer word `next`, and `KeyAction::NextAttention = "prefix+o"`, `KeyAction::PreviousAttention = "prefix+shift+o"`. Dispatch walks the queue from the currently selected session (wrapping), switches the workspace if the target lives elsewhere (reuse the same `DashboardAction::SelectWorkspace` path that `SelectWorkspaceNext` uses, then select and open the session), unfolds the project if it was folded, and sets the notice `Nothing is waiting for you` when the queue is empty. Because switching workspace is asynchronous through the daemon, record the pending session id in a new `DashboardState::pending_attention_session: Option<String>` so that when the workspace snapshot arrives (`set_active_workspace`), the dashboard selects and opens that session.

Add `[advanced] session_order = "project" | "priority"` (`SessionOrder` enum in `mj-core/src/config/ui.rs`, default `project`) and expose it in Setup's Advanced section. In `ordered_sessions`, when `priority` is selected, sort sessions by attention level then recent activity and do not group by project; `expanded_sessions_rows` must then emit no project headings (headings are only meaningful for grouped order), and the `1`-`9` fold keys must become no-ops with a notice.

Add attention counts to the workspace tab strip: after each tab label, append ` !N` where N is the number of `Waiting` sessions in that workspace (and `✓N` for unread when there are no waiting ones), styled with the attention colour from `theme::palette()`. Do the same on folded project headings in `render/sessions.rs`. Counting across workspaces needs every session's detail; `self.session_details` is already fed for all sessions (see the review projection note in `.agents/docs/adversarial-review-ux-follow-up.md`, "retains the daemon's complete review projection for all sessions"), so no new daemon traffic is required.

Tests: `next_attention_selects_the_waiting_session_before_the_unread_one`, `next_attention_wraps_and_reports_an_empty_queue`, `next_attention_switches_workspace_when_the_waiting_session_is_elsewhere`, `priority_order_lists_waiting_first_without_project_headings`, `workspace_tabs_show_waiting_counts`.

### M2 Notifications

Add `NotifyConfig` to `mj-core/src/config/` with fields `mode: NotifyMode` (`off`, `terminal`, `system`; default `terminal`), `bell: bool` (default true), `delay_seconds: u64` (default 2, the debounce before a notification fires so a question the agent answers itself does not ring), and `title: bool` (default true, whether the terminal title reflects the waiting count). Serialize under `[notify]`, skip when default, add to Setup under Interface.

Notifications are a dashboard-to-CLI contract: `DashboardState` computes, `mj-cli` emits. Add `DashboardState::notification_events(&mut self, now_ms) -> Vec<NotificationEvent>` that diffs the attention queue against the last one it reported and returns `SessionWaiting { session_id, title }` and `SessionFinished { .. }` events for sessions that crossed into `Waiting` or `Unread` at least `delay_seconds` ago and are not the session whose conversation is open. Add `DashboardState::terminal_title(&self) -> String` returning `mj` or `mj · 2 waiting` or `mj · 1 waiting · 3 unread`.

In `mj-cli/src/dashboard.rs`, after each snapshot is applied and on the existing tick, call `notification_events`; for `terminal` mode write BEL to stdout when `bell` is on (`crossterm::execute!(stdout, Print("\x07"))`); for `system` mode also spawn `osascript -e 'display notification ...'` on macOS or `notify-send` on Linux through the shared subprocess helper, never blocking the loop. When `title` is on, emit `crossterm::terminal::SetTitle` whenever `terminal_title()` changes, and restore an empty title on exit next to the existing `PopKeyboardEnhancementFlags` cleanup.

Tests: `a_new_question_produces_one_waiting_event_after_the_delay`, `the_open_session_never_notifies`, `the_title_counts_waiting_and_unread`. The subprocess path is exercised by a fake command in the unit test for the CLI helper.

### M3 Finding things

Sessions filter: add `sessions_filter: Option<SessionsFilter { query: String, state: Option<AttentionLevel> }>` to `DashboardState`. In `dashboard_input.rs`, when Sessions has focus, `/` opens the filter (the pane title shows `Sessions /query`), typed characters edit it, `Esc` clears it, `Enter` keeps it and returns focus to the list, and while the filter is open the letters `a b w i d` are state shortcuts only when the query is empty (matching Herdr: all, blocked, working, idle, done); after any other character they are text. `ordered_sessions` applies the filter (name, project short name, profile id, target id, case-insensitive substring). Add `CommandId::FilterSessions` with pane key `/` so the footer and help advertise it.

Palette: replace `rank` in `mj-tui/src/palette.rs` with subsequence fuzzy matching (every query character appears in order in the label, scored by contiguity and word-start hits; prefix matches still rank first), keep `PALETTE_HIDDEN` only for `Palette` itself, and show Create/Resume/Workspaces with their button name as the key hint. Add a `recent_commands: VecDeque<CommandId>` (capacity 5) to `DashboardState` that `dispatch_command` pushes to; the palette lists them first under a "Recent" heading when the query is empty.

Naming: rename the label "Manage runtimes" to "Manage targets", the Setup section "Runtimes" to "Targets", and make the project-heading word in Sessions match Setup's "Projects" everywhere a heading or command is user-visible. Config keys on disk do not change.

Tests: `slash_filters_sessions_by_name`, `state_letters_filter_the_list`, `fuzzy_palette_finds_create_session_from_cre`, `recent_commands_lead_an_empty_palette`.

### M4 Keyboard consistency

Prefix collision: in `mj-cli` startup, if `TMUX` or `STY` is set and the configured prefix is `ctrl+b`, set a one-time notice `Running inside tmux: press ctrl+b twice, or set [keys] prefix` (one-time means recorded in the state database's client preferences, not shown again once dismissed).

Confirmations: give every `Confirmation` variant letter accelerators derived from its button labels (first letter, unique within the dialog), print them in the button row as `[Y]es`, and route the letter to `activate_confirmation_button`. Delete becomes `[N]o  [Y]es  [B]ranch too`. Add `Confirmation::StopSession` and `Confirmation::RestartSession` with one-line bodies, used by `dispatch_command` for `StopSession` and `RestartSession`; the retry-a-failed-stop path stays unconfirmed because it is already the recovery of a decision the user made.

Bindability: give `ManageMachines` and `RestartDaemon` `KeyAction` variants (`manage_machines = ""`, `restart_daemon = ""`).

Esc: in help, `Esc` with a focused non-empty filter clears it and leaves the filter; a second `Esc` closes help (already so); change it to one `Esc` closing help when the query is already empty. On a pane, `Esc` dismisses the notice if one is showing.

### M5 Per-session context

Git branch: `SessionRecord` already knows its managed worktree path for bare sessions. Add `DashboardAction::ProbeGitStatus { session_id }` emitted when a session becomes visible and at most once a minute, handled in `mj-cli/src/dashboard/actions.rs` by running `git rev-parse --abbrev-ref HEAD` and `git rev-list --left-right --count @{upstream}...HEAD` on the target through the existing shell-command path, and feed the result back with `DashboardState::set_git_status(session_id, GitStatus { branch, ahead, behind })`. Show ` ⎇ branch ↑1 ↓2` on the metadata line of the row.

Changed files: add `CommandId::ChangedFiles` (`prefix+d`, label "Changed files") that opens a `Mode::ChangedFiles` overlay listing `git status --porcelain` of the session's workspace with counts, refreshable with `r`, closed with `Esc`. The list is fetched through the same shell path as the git probe.

Context usage: check what the harness reports over ACP (`mj-core/src/relay` and the transcript usage items). If a context-window percentage or token count is present in `SessionDetail::activity` or the transcript, show `ctx 34%` on the row; if it is not present for a harness, show nothing rather than an estimate. Record the finding in Surprises & Discoveries.

### M6 Layout and terminal integration

Notice history: extend `Notices` with a bounded history (last 20) and add `CommandId::NoticeLog` ("Recent messages", palette only) opening a scrollable overlay. Failures stack: when a protected failure notice is showing and another arrives, show `2 failures · latest: ...` and keep both in history.

NO_COLOR and ASCII: read `NO_COLOR` at theme load (`mj-chat/src/theme.rs`) to select a monochrome palette; add `[advanced] symbols = "unicode" | "ascii"` with an ASCII status set (`*` working, `!` waiting, `+` unread, `-` idle, `.` unknown, `?` unreachable, `x` failed, `^` starting, `~` resuming, `<>` moving, `#` checkpointing, `v` stopping, `=` stopped) and plain `-`, `=`, `+` size controls, defaulting to `ascii` when `TERM` is `linux` or `LANG` lacks `UTF-8`.

Narrow layout: when width is below 80 columns but at least 60, draw Sessions above Conversation in one column, minimized Targets and Quota as one summary line each, and keep the footer; replace the "Terminal too small" message with this layout. Under 60 columns keep the message.

Wide layout: when width is at least 160 columns and a second session is pinned (`CommandId::PinTranscript`, `prefix+shift+p`, "Pin this conversation beside the next one"), draw a read-only transcript of the pinned session to the right of the active one at equal width. The pinned column has no composer; the footer names the pin key to unpin.

### M7 Onboarding and docs

On the first launch that shows a dashboard (recorded as a client preference `saw_prefix_hint`), set the notice `Press ctrl+b ? for keys · ctrl+b : for commands` using the real prefix label. Fix the Terminal surface page: the Sessions pane has Create and Resume, and Commands appears on the onboarding screen and in the footer. Document `prefix+o`, `prefix+shift+o`, `/` filtering and its state letters, `[notify]`, `session_order`, `symbols`, `prefix+d`, `prefix+shift+p`, the narrow layout, the notice log, and `NO_COLOR` in `docs/src/content/docs/terminal-surface.mdx`, `sessions.md`, and `configuration.md`. Recapture the dashboard screenshot only if the default appearance changed.

## Concrete Steps

All commands run from the repository root, `/Users/ryansvihla/code/mjolnir/.claude/worktrees/humming-mapping-kazoo`, outside the restricted sandbox.

    cargo test -p brokk-mj-tui
    cargo test -p brokk-mj-core -p brokk-mj-chat
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Commit after each milestone with a message naming the milestone. Update `Progress` with a timestamp at every stopping point.

## Validation and Acceptance

Per milestone, the named unit tests fail before the change and pass after. End-to-end, with two sessions open in different workspaces and one of them asking a question through its harness: pressing `prefix+o` from the other workspace lands on the asking session with its question visible; the terminal tab title reads `mj · 1 waiting` while it waits; the workspace tab shows `!1`; typing `/` then `b` in Sessions shows only that session; typing `cre` in the palette lists Create session first; the row shows the worktree branch; `prefix+d` lists the files it changed; resizing the terminal to 70 columns stacks the panes instead of showing "Terminal too small"; `NO_COLOR=1 mj` draws in monochrome; and a fresh install prints the prefix hint in the footer once.

## Idempotence and Recovery

Every step is additive code plus configuration fields that default to the old behaviour, so a partially completed milestone leaves the dashboard working as before. Configuration changes are schema additions only; no migration of `config.toml` is needed and no state-database migration is required except the client preference rows in M4 and M7, which are compatible additions (new keys in an existing preferences table, ignored by older readers).

## Artifacts and Notes

M1 test run:

    cargo test -p brokk-mj-tui
    test result: ok. 535 passed; 0 failed; 2 ignored

New tests: `attention_levels_rank_a_question_above_unread_above_idle`, `next_attention_opens_the_waiting_session_and_wraps_through_the_queue`, `next_attention_reports_an_empty_queue_and_unfolds_a_folded_project`, `the_footer_names_the_next_key_only_while_something_waits`, `workspace_tabs_and_folded_headings_carry_attention_badges`, `priority_order_lists_waiting_first_without_project_headings`.

## Interfaces and Dependencies

In `mj-tui/src/dashboard_sessions.rs`:

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum AttentionLevel { Inactive, Idle, Working, Unread, Waiting }
    impl DashboardState {
        pub fn attention_level(&self, session_id: &str) -> AttentionLevel;
        pub fn attention_queue(&self) -> Vec<AttentionEntry>;  // AttentionEntry { workspace_id, session_id, level, last_activity_ms }
        pub fn next_attention(&mut self) -> DashboardAction;
        pub fn previous_attention(&mut self) -> DashboardAction;
    }

In `mj-core/src/config/ui.rs`:

    pub enum SessionOrder { Project, Priority }   // AdvancedConfig::session_order
    pub enum SymbolSet { Unicode, Ascii }         // AdvancedConfig::symbols

In `mj-core/src/config/notify.rs`:

    pub struct NotifyConfig { pub mode: NotifyMode, pub bell: bool, pub delay_seconds: u64, pub title: bool }
    pub enum NotifyMode { Off, Terminal, System }

In `mj-tui/src/lib.rs`:

    pub enum NotificationEvent { SessionWaiting { session_id: String, title: String }, SessionFinished { session_id: String, title: String } }
    impl DashboardState {
        pub fn notification_events(&mut self, now_ms: u64) -> Vec<NotificationEvent>;
        pub fn terminal_title(&self) -> String;
    }

New `CommandId` variants: `NextAttention`, `PreviousAttention`, `FilterSessions`, `ChangedFiles`, `NoticeLog`, `PinTranscript`. New `KeyAction` variants: `NextAttention`, `PreviousAttention`, `ChangedFiles`, `PinTranscript`, `ManageMachines`, `RestartDaemon`.

No new crates are required: `crossterm` already provides `SetTitle` and `Print`, and the shared subprocess helpers in `mj-core` run the notification and git commands.

## Revision notes

- 2026-09-18: Plan created from the usability review comparing the dashboard with Herdr 0.9.1. All seven milestones are unstarted.
- 2026-09-19: M7 complete; plan closed with the retrospective above.
- 2026-09-19: M6b complete (stacked layout from 60 columns).
- 2026-09-19: M6a complete (notice history, Monochrome and NO_COLOR, ASCII symbols). M6 split into 6a and 6b so the narrow layout lands on its own.
- 2026-09-19: Merged origin/master (2.13.0, conversation split panes). Dropped the M6 second-transcript item as superseded; recorded the merge-as-you-go decision.
- 2026-09-19: M5 complete. The branch went onto the session title line rather than the metadata line: at a 40-column sidebar the target and profile already fill the metadata line, and names rarely do the same to the title line. A narrow sidebar keeps the branch and drops the counts.
- 2026-09-19: M4 complete. Confirmation letters come from one table (`confirmation_accelerators`) and are printed by `confirmation_key_line`, replacing the hand-written `Y: Yes N / Esc: No` line.
- 2026-09-19: M3 complete. Palette search ranks instead of excluding, so a query that prefix-matches one label still lists weaker matches below it; two registry tests were updated to that rule.
- 2026-09-18: M2 complete. `[notify]` has `bell` rather than Herdr's sound-file fields, and the title is independent of the mode.
- 2026-09-18: M1 complete. Added the `Failed` attention level and the saved-view mechanism for cross-workspace jumps; moved per-feature documentation into each milestone.
