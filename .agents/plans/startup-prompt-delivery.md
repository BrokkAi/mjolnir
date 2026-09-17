# Deliver a prompt typed while a session starts, even after the user moves on

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to
date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Today, when a person creates or resumes a session in the terminal dashboard,
the prompt band shows a "standby composer": the real chat composer, parked
while the harness (Codex, Claude Code, and so on) starts. They can type into
it, but pressing Enter only shows "Sending opens when the session is live; the
draft is kept." The text becomes a prompt only if they stay on that session
until it attaches. If they select another session to keep working, the text
sits unsent until they come back. Worse, in the seconds between the new-session
wizard closing and the daemon registering the session, there is no standby at
all, so keystrokes go to whichever session was selected before.

After this change the flow is: create a session, type a prompt while it
starts, press Enter, move to any other session or workspace, and the prompt is
delivered to the new session as soon as its harness is ready. The daemon (the
long-running background process `mj daemon` that owns sessions) performs the
delivery, so it does not matter which session the dashboard shows or whether
the dashboard is still running. If delivery fails, the text is put back into
the session's saved draft and a notice says so. Nothing typed is lost.

How to see it working: run the dashboard, press the new-session key, finish
the wizard, start typing immediately, press Enter, then press Down to select
another session. When you return, the new session's transcript shows your
prompt and the agent's reply in progress. Quitting the dashboard right after
Enter still delivers the prompt.

## Progress

- [x] (2026-09-17) Investigate current behaviour and write this ExecPlan.
- [x] (2026-09-17) Milestone 1: daemon `QueueStartupPrompt` action,
      per-session startup queue with a supervised drain task, draft
      restoration on failure, WikiRestore hand-off moved onto the queue,
      shutdown drain, tests.
- [x] (2026-09-17) Milestone 2: standby composer Enter queues and previews;
      dashboard action and io update; failure restores the draft; tests
      updated.
- [ ] Milestone 3: launch standby that captures typing before the session is
      registered and adopts into the session's standby on registration.
- [ ] Workspace `cargo clippy --all-targets -- -D warnings` and `cargo test`
      (outside the sandbox) pass; manual scenario checked.

## Surprises & Discoveries

- Observation: the two earlier changes in this area (commits `538650d8` and
  `4e5148f4`) never implemented delivery. Both say in their messages that
  Enter is consumed with an explanation. The missing piece is a feature, not a
  regression.
- Milestone 1: the HTTP API's `wiki_restore` has no request-scoped
  cancellation token to thread into `queue_startup_step`, so it passes a fresh
  token. Nothing else holds that token; the shutdown drain cancels each
  queue's own token directly, so the step is still cancelled on shutdown.
- Milestone 1: the planned test "an `InstallHandoff` queued first is installed
  before the prompt" cannot assert a successful install in a unit test. A test
  session manager is remote, and `install_prompt_context` is refused outside
  the controller daemon by design (`session_manager/remote.rs`). The test
  therefore asserts the ordering from the other side: with the hand-off
  failing, the prompt behind it is never submitted and comes back as the
  draft, which is only possible if the hand-off ran first.
- Milestone 1: a failed `InstallHandoff` aborts the whole queue, so a prompt
  behind it returns to the draft rather than being sent without the context it
  was meant to read. Both notices are posted.
- Milestone 2: the composer band already draws previews straight from
  `ChatState::queued_prompts` (`render_composer_band`, and
  `desired_prompt_height` counts them), so the standby band needed no
  rendering change; appending the preview was enough.
- Milestone 2: the standby branch also refuses an empty prompt with the
  command notice. Images cannot reach a standby composer, but that guard keeps
  an image-only submit from queueing empty text.
- Observation: the SessionWiki restore hand-off (`restore_wiki_session` in
  `mj-controller/src/daemon/state.rs`) spawns an unsupervised task that waits
  for the same readiness condition a first prompt would wait for, then builds
  and installs context. A queued prompt would be submitted before that context
  exists. Milestone 1 puts both on one ordered queue.

## Decision Log

- Decision: the daemon delivers the prompt; the dashboard only captures and
  displays.
  Rationale: delivery must not depend on the selected session, the active
  workspace, or the dashboard process being alive. The daemon already has the
  readiness wait (`wait_for_ready_session`) and the submit-plus-history path
  (`SubmitSessionCommand`). A dashboard-local queue would have to guess
  readiness and would die with the dashboard.
  Date/Author: 2026-09-17, Fable.
- Decision: do not reuse `CreateSessionRequest.initial_prompt`.
  Rationale: it only seeds the saved draft, and the HTTP API path that uses it
  already delivers the same text through its own supervised follow-up.
  Changing its meaning would double-deliver for sub-agents.
  Date/Author: 2026-09-17, Fable.
- Decision: do not model the drain task as a lifecycle (`LifecycleKind`).
  Rationale: lifecycles are one-per-session with kind conflicts; the Create or
  Resume lifecycle for the same session is still running while the prompt
  waits.
  Date/Author: 2026-09-17, Fable.
- Decision: on failure, restore text into `SessionRecord.draft_input` and
  report with `push_notice`.
  Rationale: `draft_input` is what a freshly opened composer inherits, so the
  text reappears where the person expects it. `push_notice` reaches the
  dashboard through the runtime snapshot; `record_api_error` is read only by
  the HTTP API and `mj api`.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

To be written at completion.

## Context and Orientation

The workspace is a Rust monorepo. The pieces involved:

- `mj-cli` is the binary. `mj-cli/src/dashboard/` is the terminal dashboard's
  host: it owns background tasks, talks to the daemon, and feeds results into
  the pure UI state. `mj-cli/src/dashboard/io.rs` applies asynchronous results
  (`DashboardIoUpdate`, lifecycle updates) to state. `mj-cli/src/dashboard/
  actions.rs` turns UI actions (`DashboardAction`) into work.
  `mj-cli/src/dashboard/io/spawn.rs` holds the helpers that run that work on
  Tokio without blocking the event loop.
- `mj-tui` (package name `brokk-mj-tui`) is the dashboard's UI state and
  rendering. `DashboardState` lives in `mj-tui/src/lib.rs`. The standby
  composer logic is in `mj-tui/src/dashboard_standby.rs`; key routing is in
  `mj-tui/src/dashboard_input.rs`; the combined screen is drawn by
  `mj-tui/src/combined.rs`.
- `mj-chat` (package `brokk-mj-chat`) is the chat composer and transcript.
  `ChatState` (`mj-chat/src/chat.rs`) has a `standby` flag. Enter goes through
  `submit_input` in `mj-chat/src/chat/input_state.rs`. The composer band draws
  the input plus up to three "queued prompt" previews.
- `mj-client` holds the daemon socket protocol: `DaemonAction` (requests) and
  `DaemonReply` in `mj-client/src/daemon.rs`, with `PROTOCOL_VERSION` near the
  bottom of that file. A daemon rejects an older client and a client rejects a
  newer daemon, so adding a request variant needs a version bump.
- `mj-controller` implements the daemon. `mj-controller/src/daemon.rs` defines
  `RuntimeState`, the daemon's in-memory state. `mj-controller/src/daemon/
  actions.rs` dispatches each `DaemonAction`. `mj-controller/src/daemon/
  state.rs` has `RuntimeState` methods, including `wait_for_ready_session`.
  `mj-controller/src/daemon/lifecycle.rs` runs create/resume/close operations
  as supervised tasks. `mj-controller/src/daemon/process.rs` runs the daemon
  and its shutdown epilogue.

Terms used below:

- "Session record" is `mj_core::state::SessionRecord`, the durable row for a
  session. Its `state` field (`SessionState`) says whether it is
  `Provisioning`, `Running`, `Stopped`, and so on. Its `draft_input` field is
  the saved composer text a freshly opened composer inherits.
- "Transition" is a lifecycle in progress. `DashboardState::transition_kind`
  returns `Starting`, `Resuming`, `Moving`, `Stopping`, or `Destroying`. The
  standby composer appears for `Starting` and `Resuming`
  (`standby_prompt_session` in `mj-tui/src/dashboard_standby.rs`).
- "Ready" for a first prompt means the session manager has a handle for the
  session, the worker is connected, and the harness reports its own session is
  ready (`native_session_is_ready`, which includes ACP readiness). This is what
  `RuntimeState::wait_for_ready_session` checks, polling every 250 ms for up to
  30 minutes, and giving up early if the record leaves the set
  `Provisioning | Running | Disconnected | Checkpointing`.
- "Relay command" is `mj_core::relay::RelayCommand`; a prompt is
  `RelayCommand::Prompt { prompt: vec![ContentBlock::Text(TextContent::new(text))] }`.

Where typing is lost today, with evidence:

- `mj-chat/src/chat/input_state.rs`, `submit_input`: when `self.standby` is
  true it sets a notice and returns `ChatAction::None`.
- `mj-cli/src/dashboard/drafts.rs` `open_chat_session` and
  `mj-cli/src/dashboard/io.rs` (`ChatOpened` arm) move the standby draft into
  the real composer only when that session attaches, and attaches happen only
  for the selected session in the active workspace.
- `mj-cli/src/dashboard/actions.rs` `start_session_launch_with_repository_preflight`
  runs after the wizard has already closed (`finish_session_mount_preflight`
  calls `cancel_modal`). `spawn_dashboard_create_session` in
  `mj-cli/src/dashboard/io/spawn.rs` then loads the controller, probes Git
  remotes (30 s deadline), and registers with the daemon before it reports
  `DashboardCreateSessionUpdate::Registered`. Until then the previous
  selection is still on screen and takes the keys.

## Plan of Work

### Milestone 1: the daemon queues and delivers startup prompts

Scope: after this milestone a client can ask the daemon to deliver a prompt to
a starting or resuming session, and the daemon does so once the session is
ready, or restores the text as the session's draft and posts a notice if it
cannot. The SessionWiki restore hand-off rides the same queue so it is always
installed before the first prompt. Nothing in the dashboard changes yet.

Protocol. In `mj-client/src/daemon.rs`, add to `DaemonAction`, next to
`SubmitSessionCommand`:

    QueueStartupPrompt {
        session_id: String,
        text: String,
        /// The saved draft text this prompt was typed from, if the client
        /// also persisted it. Cleared after a successful submit so the
        /// delivered prompt does not reappear as a draft.
        #[serde(default)]
        inherited_draft: Option<String>,
    },

Add `DaemonClient::queue_startup_prompt(session_id, text, inherited_draft)
-> Result<()>` after `submit_session_command`, matching `DaemonReply::Done`.
Bump `PROTOCOL_VERSION` from 23 to 24. Do not add a
`released_protocol_transcripts` entry; those are for released versions.

State. In `mj-controller/src/daemon.rs`, add to `RuntimeState`:

    startup_prompts: Mutex<BTreeMap<String, StartupQueue>>,

with, near `ActiveLifecycle`:

    pub(crate) enum StartupStep {
        InstallHandoff(Box<mj_core::archive::CanonicalSessionSnapshot>),
        Prompt { text: String, inherited_draft: Option<String> },
    }

    struct StartupQueue {
        pending: VecDeque<StartupStep>,
        in_flight: bool,
        cancel: CancellationToken,
        task: Option<JoinHandle<()>>,
    }

Initialise the map in `RuntimeState::new_with_controller_loader` in
`mj-controller/src/daemon/state.rs`.

Queueing. In `mj-controller/src/daemon/state.rs` add
`queue_startup_step(self: &Arc<Self>, session_id: &str, step: StartupStep,
cancellation: &CancellationToken) -> Result<()>`. It refuses with an error
unless `self.session_state(session_id)` is one of `Provisioning | Running |
Disconnected | Checkpointing` (the same set `wait_for_ready_session` accepts),
pushes the step, and if the queue has no live task spawns the drain with
`cancel = cancellation.child_token()`. `cancellation` is the daemon-wide token
`handle_action` already receives as its last parameter; `DaemonAction::Stop`
cancels it. Spawn with the outer-and-inner double `tokio::spawn` pattern from
`mj-controller/src/daemon/lifecycle.rs` (the block starting around line 137),
so a panic in the drain becomes an error the outer task reports instead of a
silently dead task.

Drain. The task first awaits `wait_for_ready_session(session_id)` selected
against `cancel.cancelled()`. Make `wait_for_ready_session` `pub(super)` and
add one check it lacks: if the handle's view has
`error: Some(ViewError::TargetMissing(detail))`, bail with "session {id} lost
its target: {detail}" instead of waiting out 30 minutes (compare
`apply_followup` in `mj-controller/src/server_runtime/api.rs`). Then loop: lock
the map, pop the front step, set `in_flight = true`, unlock; run the step;
lock, clear `in_flight`; if `pending` is empty remove the entry and return.
Popping under the lock while `in_flight` is set is what stops a second
`QueueStartupPrompt` from spawning a second drain and reordering prompts.

`Prompt` step: `handle.submit(new_command_id("startup")?, RelayCommand::Prompt
{ .. })`, then record history with `crate::database::record_prompt` the way
the `SubmitSessionCommand` arm in `mj-controller/src/daemon/actions.rs` does
(look up `bundle_id` from the in-memory controller; warn, do not fail, when
the write fails). If `inherited_draft` is `Some`, clear it with
`crate::database::clear_session_draft_input_if_matches` and the in-memory
record update, again as that arm does, then `publish_revision()`.

`InstallHandoff` step: the body of `install_archive_handoff` from
`state.rs` minus its own readiness wait. Change `restore_wiki_session` to
enqueue `StartupStep::InstallHandoff` instead of `tokio::spawn`, threading
`cancellation` in from the `WikiRestore` arm of `handle_action`.

Failure. On any error or cancellation, gather the failed prompt text plus the
text of every remaining `Prompt` step, and call a new
`append_draft_input(session_id, text)`: read the in-memory record's
`draft_input`, join the non-empty parts of `[existing, text]` with a blank
line, persist with `blocking(|| crate::database::set_session_draft_input(..))`
(`mj-controller/src/database/sessions.rs`), set the in-memory field, and
`publish_revision()`. The database is the source of truth because the target
refresher reloads the controller from disk regularly. Then
`push_notice(session_id, format!("Your prompt could not be sent to session {}
({reason}); it is back in the composer draft.", short id))`, then
`tracing::warn!`. A dropped `InstallHandoff` keeps the notice text
`restore_wiki_session` uses today. If `set_session_draft_input` fails because
the session was destroyed, log and notice; do not retry. Do not use
`save_detached_session_draft`; that table is client-owned.

Shutdown. Add `cancel_and_join_startup_prompts(&self) -> Result<()>`: cancel
every token, join every task under one shared 1 s deadline, return the first
join error. In `mj-controller/src/daemon/process.rs`, in the shutdown
epilogue, call it through `record_daemon_cleanup(&mut outcome, "drain startup
prompts", ..)` right after the `cancel_and_wait_lifecycles` step and before
the session manager shutdown, while the database writer is still alive.

Action. In `mj-controller/src/daemon/actions.rs`, add the
`QueueStartupPrompt` arm next to `SubmitSessionCommand`: reject blank text
with an error, call `queue_startup_step(.., StartupStep::Prompt { .. },
cancellation)`, reply `DaemonReply::Done`.

Tests in `mj-controller/src/daemon/tests.rs`, using `TestRemoteManager`,
`test_runtime_state_with_manager`, and a `runtime_test_session` inserted into
the in-memory controller (existing tests show all three). Publish a
disconnected view first, then a ready one shaped like `ready_view` in
`mj-controller/src/server_runtime/api/tests.rs` (`connected: true`,
`native_session_id` set, `acp_ready: Some(true)`). Cover: not submitted while
disconnected, then one `RemoteSessionRequest::Submit` with the text after the
ready view, and a second queued prompt delivered second; an `InstallHandoff`
queued first is installed before the prompt; flipping the record to `Stopped`
ends the wait, leaves the text in the in-memory `draft_input` joined onto an
existing draft, and pushes a notice; a refused submit restores that text and
the rest of the queue; `cancel_and_join_startup_prompts` returns within its
bound while a drain is waiting; `handle_action(QueueStartupPrompt)` errors for
a `Stopped` or unknown session and for blank text. Unit tests have no
database writer, so assert on the request channel, the in-memory record, and
the notice text.

### Milestone 2: Enter in the standby composer queues the prompt

Scope: after this milestone, pressing Enter in the standby composer during a
Starting or Resuming transition hands the text to the daemon, clears the
input, and shows the text as a queued preview in the band. Selecting another
session no longer strands the prompt.

`mj-chat/src/chat/input_state.rs`, `submit_input`, standby branch: if the
input parses as a local command (`parse_local_command` returns `Some`) or
starts with `!`, keep the notice but reword it to "Commands open when the
session is live; the draft is kept." Otherwise clear the input, append a
`QueuedPrompt` with `kind: QueuedCommandKind::Prompt`, a locally generated id,
and no images (image paste is refused in standby), and return
`ChatAction::Prompt(text)`. Add a public `take_queued_prompts()` or similar
accessor if the host needs the texts. Update the doc comment on
`ChatState::standby` in `mj-chat/src/chat.rs` and the test
`a_standby_composer_edits_like_the_real_one_but_never_sends` in
`mj-chat/src/chat/tests.rs` to the new contract.

`mj-tui/src/lib.rs`: add `DashboardAction::QueueStartupPrompt { session_id:
String, text: String }`. `mj-tui/src/dashboard_standby.rs`
`handle_standby_prompt_key`: when the standby returns `ChatAction::Prompt(text)`
return that action instead of `DashboardAction::None`. Add helpers the host
needs: `restore_standby_prompt(session_id, text)` that removes the matching
queued preview and puts `text` back in front of the current draft.

`mj-cli/src/dashboard/actions.rs`: handle `QueueStartupPrompt` by spawning a
daemon request with `spawn_critical_async` (copy `spawn_dashboard_rename` in
`mj-cli/src/dashboard/io/spawn.rs`), yielding
`DashboardIoUpdate::StartupPromptQueued { session_id, text, result }`. Pass
`inherited_draft` as the session record's current `draft_input` when it equals
the text, else `None`. `mj-cli/src/dashboard/io.rs`: on `Err`, call
`restore_standby_prompt` and `set_failure_notice`; on `Ok`, nothing.

Hand-off stays as it is: when the real chat opens, only the draft is carried
(`take_standby_prompt_draft`). The delivered prompt reaches the transcript
through the normal feed. Update `mj-tui/src/tests.rs`
`enter_during_a_starting_transition_does_not_send_or_clear_the_draft` to
assert the new contract and rename it accordingly. Add a test that a failed
queue result restores the text as the draft.

### Milestone 3: capture typing before the session is registered

Scope: after this milestone, typing that starts the instant the new-session
wizard closes lands in a launch standby composer rather than in the previous
session, and is adopted by the new session's standby when the daemon
registers it.

`mj-tui/src/lib.rs` `DashboardState`: add `launch_standby: Option<ChatState>`.
Add `begin_launch_standby(&mut self, header: SessionHeaderIdentity)` that
builds a standby `ChatState` (see `build_standby_prompt` for the pieces),
calls `set_current_session(None)` so the previous chat is neither drawn nor
given keys, and `focus_prompt()`. Add `adopt_launch_standby(&mut self,
session_id: &str) -> Vec<String>` that moves the launch standby into
`standby_prompts[session_id]` and returns the texts of its queued previews so
the host can send each as `QueueStartupPrompt`. Add `has_launch_standby()`.

`mj-tui/src/dashboard_standby.rs` and `mj-tui/src/dashboard_input.rs` (paste):
when `standby_prompt_session()` is `None` and `launch_standby` is `Some`, route
keys and paste to it. Enter there queues locally: the standby's own
`submit_input` already appends the preview; the dashboard returns
`DashboardAction::None` because there is no session id yet.

`mj-cli/src/dashboard/actions.rs`, CreateSession arm of
`start_session_launch_with_repository_preflight`: call `begin_launch_standby`
with a header built from the action (bundle id or project directory as
target, profile id as profile, empty title). `mj-cli/src/dashboard/io.rs`,
`Registered` arm: after `begin_session_operation(Launching)` and before
`focus_prompt`, call `adopt_launch_standby(&session_id)` and spawn a
`QueueStartupPrompt` for each returned text in order. `Failed` and
`RemoteRepair` arms: leave the launch standby in place so a retry keeps the
text; `show_launch_failure` already offers Retry. Drop the launch standby only
when a launch registers.

`mj-tui/src/combined.rs`: where the surface is chosen (the `selected_transition`
and `standby_drawn` logic), when there is no selected transition and
`launch_standby` is `Some`, draw the transition panel with the label
"Starting" and the launch standby in the prompt band. Factor the inner part of
`draw_standby_prompt` to take `&mut ChatState` so both callers share it.

Tests in `mj-tui/src/tests.rs`: typing after `begin_launch_standby` with a
different session selected edits the launch standby and not that session;
Enter queues a preview and returns `DashboardAction::None`;
`adopt_launch_standby("s")` moves draft and previews into
`standby_prompts["s"]` and returns the texts.

## Concrete Steps

All commands run from the repository root `/home/jonathan/Projects/hel`.

    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-controller daemon::tests
    cargo test -p brokk-mj-chat
    cargo test -p brokk-mj-tui
    cargo test -p mj-cli
    cargo test

Run `cargo test` outside the restricted sandbox; the suite uses loopback and
Unix sockets. Commit after each milestone with only the files that changed.

## Validation and Acceptance

Automated: the tests named in each milestone fail before and pass after. The
workspace clippy and test commands exit 0.

Manual: with the daemon running, start `mj` (the dashboard). Create a session.
As soon as the wizard closes, type "say hello" and press Enter. The prompt
band shows the text as a queued preview and the input is empty. Press Down to
select another session. Wait for the notice "Session … is ready". Select the
new session: its transcript shows "say hello" as the first user message and a
reply in progress. Repeat, staying on the session: the transcript shows the
prompt shortly after attach. Repeat with Resume of a stopped session. Repeat,
quitting the dashboard right after Enter and restarting it: the prompt still
arrives. Force a failure by stopping the session during startup: the notice
"Your prompt could not be sent to session … ; it is back in the composer
draft." appears, and opening that session later shows the text in the
composer.

## Idempotence and Recovery

Every edit is additive or a contract change covered by tests. The protocol
bump means a dashboard built from this tree needs a daemon built from this
tree; `mj daemon` restarts on protocol mismatch as it does for every bump. If
the drain task fails in a way not anticipated, the text is in `draft_input`
and the daemon log has a warning with the session id.

## Artifacts and Notes

To be filled in with test transcripts as milestones complete.

## Interfaces and Dependencies

In `mj-client/src/daemon.rs`:

    DaemonAction::QueueStartupPrompt { session_id: String, text: String, inherited_draft: Option<String> }
    impl DaemonClient { pub async fn queue_startup_prompt(&mut self, session_id: String, text: String, inherited_draft: Option<String>) -> Result<()> }
    pub const PROTOCOL_VERSION: u32 = 24;

In `mj-controller/src/daemon/state.rs` (on `RuntimeState`):

    pub(crate) fn queue_startup_step(self: &Arc<Self>, session_id: &str, step: StartupStep, cancellation: &CancellationToken) -> Result<()>
    pub(super) async fn wait_for_ready_session(&self, session_id: &str) -> Result<ManagedSessionHandle>
    pub(super) async fn append_draft_input(&self, session_id: &str, text: &str) -> Result<()>
    pub(crate) async fn cancel_and_join_startup_prompts(&self) -> Result<()>

In `mj-tui/src/lib.rs`:

    DashboardAction::QueueStartupPrompt { session_id: String, text: String }
    impl DashboardState {
        pub fn begin_launch_standby(&mut self, header: SessionHeaderIdentity);
        pub fn adopt_launch_standby(&mut self, session_id: &str) -> Vec<String>;
        pub fn restore_standby_prompt(&mut self, session_id: &str, text: &str);
    }

In `mj-cli/src/dashboard/io.rs`:

    DashboardIoUpdate::StartupPromptQueued { session_id: String, text: String, result: Result<(), String> }
