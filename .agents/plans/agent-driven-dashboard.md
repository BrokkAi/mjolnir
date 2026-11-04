# Let an agent arrange the dashboard and stage settings for the user

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Today a person who wants a different dashboard arrangement or a different setting has to learn the pane controls and click through the Settings panel themselves. After this change they can ask the agent they are already talking to: "maximize the Profiles pane", "show session 3f2a beside this one", or "turn the notification bell off and set auto-AFK to 30 minutes". The agent runs one `mj ui` command, the terminal dashboard (the full-screen `mj` interface) the person is looking at changes as if they had pressed the keys, and for settings it opens the Settings panel on the right page with the changes already filled in but not saved, so the person reviews them and presses Save.

To see it working, open the dashboard of an isolated instance, start a session in it, and from a shell inside that instance run, for example:

    mj --instance ui-demo ui pane profiles maximized
    mj --instance ui-demo ui settings notify.bell=false advanced.auto_afk_minutes=30

The first command maximizes the Profiles pane at once. The second opens Settings on the Notifications page with the bell unticked and a notice saying an agent staged two changes. The Advanced page shows 30 minutes. Nothing reaches `config.toml` until Save is pressed.


## Progress

- [x] (2026-11-04 10:40Z) Research: mapped the settings model, pane layout ownership, runtime feed, notices, `/api/v1` and the CLI.
- [x] (2026-11-04 10:55Z) Wrote this plan.
- [x] (2026-11-04 11:40Z) Milestone 1: shared request types in `mj-client`, daemon publication, `POST /api/v1/ui/requests`, `mj ui` CLI.
- [x] (2026-11-04 12:20Z) Milestone 2: the terminal dashboard applies requests (pane sizes, showing a session, staged settings).
- [x] (2026-11-04 12:50Z) Milestone 3: the bundled `mj` skill tells agents about `mj ui`.
- [x] (2026-11-04 12:45Z) Manual check in isolated instance `uidemo7ca2` through a real dashboard on a pseudo-terminal (transcript in Artifacts).
- [x] (2026-11-04 13:10Z) Workspace clippy clean; full suites of mj-client, mj-core, mj-tui, mj-controller and mj-cli run.
- [x] (2026-11-04 13:30Z) Commit and push.
- [ ] Later work (not in this plan): the web viewer, and an `mj-agents` MCP tool so container and remote sessions can send requests.


## Surprises & Discoveries

- Observation: the daemon's own Settings routes deliberately refuse bearer-token callers, because agents hold that token and must not rewrite the user's configuration. Staging edits in the user's own Settings panel keeps that rule: the agent proposes, the person saves.
  Evidence: `mj-controller/src/server/settings.rs` lines 1-3 and `mj-controller/src/server/routes.rs` near line 178.
- Observation: pane sizes and conversation layouts are owned by the dashboard while it runs. It loads them once at startup and writes them in the background. Nothing pushes layout to a running dashboard. So the request has to be input to the dashboard, not a write to the store, or the dashboard and the store would both decide the layout.
  Evidence: `mj-cli/src/dashboard/workspace_settings.rs`; `DaemonAction::SaveWorkspacePaneSizes` and `SaveWorkspaceLayout` in `mj-client/src/daemon.rs`.
- Observation: the `--instance` flag did not isolate the first manual run, because the agent's own session exported `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, and those take precedence. The development-build guard refused to touch the default instance, as designed. Clearing those variables for the test processes fixed it.
  Evidence: "refusing to replace Mjolnir daemon … /root/.local/share/mjolnir is the default Mjolnir instance".
- Observation: a fresh instance's dashboard opens with a first-run dialog. A settings request sent at that moment was held and opened Settings once the dialog closed, which exercised the hold path for real.


## Decision Log

- Decision: model the feature as "UI requests", inputs to the dashboard that the dashboard applies through its existing command paths, rather than as writes to the stored layout or the configuration.
  Rationale: the dashboard is the single owner of its live layout, and the Settings panel is the single owner of a settings draft. A request that writes either one directly would create a second writer that races the dashboard's own background saves. As an input, a request can only do what a keypress could do.
  Date/Author: 2026-11-04, agent.
- Decision: settings requests stage edits in an unsaved Settings draft; they never save.
  Rationale: this preserves the existing security boundary that bearer-token callers (agents) cannot rewrite the user's configuration, while still sparing the person the navigation. The person sees exactly what changed and presses Save.
  Date/Author: 2026-11-04, agent.
- Decision: publish requests in the runtime feed's metadata, next to `notices`, as a short bounded list with increasing ids and the daemon's feed incarnation. A dashboard applies only requests newer than the ones present when it attached.
  Rationale: this reuses the mechanism every dashboard already follows, so no new connection or protocol action is needed. Requests are short-lived and do not belong in the database: one that arrives with no dashboard open has nobody to show it to. Carrying the incarnation means a dashboard that outlives a daemon restart applies the new daemon's requests, even though that daemon numbers them from 1 again.
  Date/Author: 2026-11-04, agent.
- Decision: the daemon validates a request fully before publishing it. It checks that the session exists for "show session". For settings, it applies the edits to a draft of the current configuration with the same shared functions the Settings panel uses and checks that the result is a valid configuration. Invalid requests get HTTP 400 with the reason. A request made while no dashboard is attached is refused with HTTP 409 and a message saying to open `mj`.
  Rationale: the agent learns about mistakes immediately and can correct them in conversation. Publishing something nobody can apply would report success for work that never happens.
  Date/Author: 2026-11-04, agent.
- Decision: settings paths use the Settings panel's own page paths, joined with dots (`notify.bell`, `interface.theme`), and also accept the stored spellings of the Interface page's settings (`theme`, `spinner`, `sessions_side`, `keys.prefix`). Both are translated in one helper in `mj-client/src/settings/draft.rs`.
  Rationale: agents read the TOML reference, people read the panel; both spellings should work, and one helper owns the mapping.
  Date/Author: 2026-11-04, agent.
- Decision: one readiness rule decides whether a request applies now or waits, and both the arrival path (`apply_ui_request`) and the release path (`take_ui_followup`) use it. Requests wait behind older held requests, so they always apply in the order made. Pane sizes never wait. Staged settings join an open Settings only while the person is not typing a value, searching, editing review settings or saving.
  Rationale: a first version special-cased "Settings is open" at arrival only. That let a new request overtake held ones, and could switch Settings pages under a value the person was typing. Asking one predicate on both paths removes both cases.
  Date/Author: 2026-11-04, agent.
- Decision: this plan covers the terminal dashboard and localhost sessions, which reach the daemon with the `mj` CLI. The web viewer and an `mj-agents` MCP tool for container and remote sessions are deferred.
  Rationale: the terminal dashboard is where panes exist. The web viewer has no split panes and keeps its settings draft in the browser. The MCP tool crosses the worker relay journal, which needs its own compatibility work. Both can be added on top of the same request type later.
  Date/Author: 2026-11-04, agent.


## Outcomes & Retrospective

An agent in a localhost session can now resize the dashboard's support panes, show a session in the main conversation or a new split, and stage settings in an unsaved Settings draft, all with `mj ui`. The daemon checks each request whole before publishing it: bad paths or values come back as HTTP 400 with the reason, and a request with no dashboard attached comes back as 409. Staged settings never reach `config.toml` until the person presses Save, which keeps the rule that bearer-token callers cannot rewrite the configuration. Remaining work: the web viewer does not act on requests, and container or remote sessions cannot send them until an `mj-agents` tool exists. Lesson: the existing notice feed was the right carrier, since the dashboard already followed it across daemon restarts. The one thing it lacked was a per-incarnation identity, so a restarted daemon's requests are not mistaken for old ones.


## Context and Orientation

Mjolnir (`mj`) is a Rust workspace. The daemon (`mj daemon-run`, code in `mj-controller`) is the long-running process that owns the database, the sessions and an HTTP API. The terminal dashboard is the full-screen interface started by plain `mj`; its state and drawing live in `mj-tui`, and the code that connects it to the daemon lives in `mj-cli/src/dashboard.rs` and `mj-cli/src/dashboard/`. `mj-client` holds types and logic shared by the daemon and its clients. Agents run inside sessions; localhost sessions get the `mj` CLI, the environment variable `MJ_SESSION_ID` naming their own session, and a bundled skill (`mj-core/assets/skills/mj/SKILL.md`, with `references/configuration.md`) that explains how to use it.

The runtime feed is how the daemon tells clients what changed. `mj-client/src/runtime_feed.rs` defines `RuntimeProjection`, whose `metadata: RuntimeMetadata` carries daemon-wide facts such as `notices: Vec<RuntimeNotice>`. A notice is a short message with an increasing `id`. The daemon keeps the last 32 in `RuntimeState::notices` (`mj-controller/src/daemon.rs`, pushed by `push_notice` in `mj-controller/src/daemon/views.rs`) and copies them into every published snapshot (`mj-controller/src/daemon/feed.rs`, where `full.metadata.notices` is filled). The dashboard's poller (`mj-controller/src/pollers/remote.rs`, `spawn_remote_dashboard_worker_poller`) turns each metadata field into a `tokio::sync::watch` channel. The dashboard loop (`mj-cli/src/dashboard.rs`) waits on them, and `drain_runtime_notices` (`mj-cli/src/dashboard/drains.rs`) shows the notices whose id is above the highest one present at attach time. UI requests follow exactly this path.

Pane sizes: the dashboard has three support panes, Sessions, Targets and Profiles (called `quota` in code). Each is Minimized, Standard or Maximized (`mj_core::workspace::PaneSize`). `DashboardState::set_pane_size(SupportPane, PaneSize)` in `mj-tui/src/dashboard_panes.rs` applies one size, makes a maximum exclusive and marks the workspace modified. The dashboard loop then persists the sizes. The conversation area can be split; `DashboardAction::OpenSessionInSplit { session_id, direction }` opens a session beside the current one, handled by `DashboardContext::open_session_in_split` in `mj-cli/src/dashboard.rs`. `DashboardState::focus_session_anywhere(workspace_id, session_id)` in `mj-tui/src/dashboard_sessions.rs` shows a session in the main pane, switching workspace if needed.

Settings: `mj-client/src/settings/` is the settings model shared by the terminal and web Settings panels. A draft is the configuration as JSON with every optional field filled in (`draft::open`). Settings are addressed by page path, a list of keys such as `["notify", "bell"]`. The synthetic `interface` page gathers `keys.prefix`, `theme`, `spinner` and `sessions_side` (`draft::storage_path` maps them). `edit::apply_value` sets a scalar from typed text or a chosen value, `edit::toggle` flips a checkbox and applies its side effects, `schema::choices` lists a setting's allowed values, and `draft::config_from_draft` turns a draft back into a `Config`. In the terminal, `SetupDialog` (`mj-tui/src/setup.rs`) is the Settings panel. `DashboardState::begin_settings_section` (`mj-tui/src/setup/events.rs`) opens it on a page, `SetupDialog::apply_value` and `toggle_path` (`mj-tui/src/setup/draft.rs`) edit its draft, and `is_dirty` says whether there is anything to save.

The HTTP API: `mj-controller/src/server/api/routes.rs` lists the `/api/v1` routes, authenticated by a bearer token that `mj` CLI commands read through `ApiClient` (`mj-cli/src/api_client.rs`). Handlers reach the daemon through the `SubagentBackend` trait (`mj-controller/src/server/api/subagent_backend.rs`), implemented by `ApiBackend` (`mj-controller/src/server_runtime/api.rs`), which forwards daemon operations to the `ExportRuntime` trait implemented by `RuntimeState`. CLI subcommands are declared with clap in `mj-cli/src/main.rs` and implemented in `mj-cli/src/api_commands.rs`.


## Plan of Work

Milestone 1 makes requests exist end to end up to the feed. Create `mj-client/src/ui_request.rs`. It defines `UiPane` (sessions, targets, profiles), `SessionPlacement` (main, right, below), `SettingEdit { path: Vec<String>, value: serde_json::Value }`, `UiAction` (`PaneSize`, `ShowSession`, `StageSettings`), the published `UiRequest { incarnation, id, from_session_id, action }`, the HTTP body `UiRequestBody { from_session_id, action }` and the reply `UiRequestReceipt { id, dashboards }`. In `mj-client/src/settings/edit.rs` add `stage(draft, path, value)`, the one function that turns a JSON value into a settings edit. It uses `toggle` for booleans so side effects match a click, checks the value against `schema::choices` when the setting has a list of values, and otherwise goes through `apply_value` with text. Add `stage_all(draft, edits)` to apply a list atomically and check `config_from_draft`. In `mj-client/src/settings/draft.rs` add `page_path(path)`, which maps stored spellings to page paths. Add `ui_requests: Vec<UiRequest>` to `RuntimeMetadata` with `#[serde(default)]`. In the daemon, add `ui_requests: Mutex<VecDeque<UiRequest>>` and `next_ui_request_id` to `RuntimeState`, plus `RuntimeState::request_ui(body) -> Result<UiRequestReceipt, UiRequestRefusal>`. It refuses when no dashboard is attached, validates, appends (keeping 16), and publishes a revision. The feed copies the list into metadata. Expose it as `ExportRuntime::request_ui`, `SubagentBackend::request_ui`, and the route `POST /ui/requests` in `mj-controller/src/server/api/ui.rs`. Add `ApiClient::request_ui` and the `mj ui` subcommand with `pane`, `show` and `settings`, filling `from_session_id` from `MJ_SESSION_ID`.

Milestone 2 makes the dashboard act. The poller adds a `ui_requests` watch. The dashboard context keeps `applied_ui_request: Option<(String, u64)>`, initialised from the bootstrap list, and `drain_ui_requests` hands each newer request to `DashboardState::apply_ui_request`, collecting the `DashboardAction`s it returns. The main loop applies those actions with `apply_dashboard_action`, as it does for the Git identity follow-up. In `mj-tui`, the new module `mj-tui/src/ui_requests.rs` implements `apply_ui_request`. `PaneSize` calls `set_pane_size` and leaves a notice. `ShowSession` returns `focus_session_anywhere` for main, or `OpenSessionInSplit` for right or below. `StageSettings` opens Settings on the edited page with the edits staged, focuses the first edited row, and sets a notice naming the requesting session and the number of changes. If another dialog is open, the request is held, a notice says so, and `take_ui_followup` opens it when the dialog closes. If Settings is already open, the edits are added to its draft.

Milestone 3 documents it. Add a section to `mj-core/assets/skills/mj/SKILL.md` saying that requests to rearrange the dashboard or change a setting go through `mj ui`, that settings are staged for the person to save, and listing the forms. Point the configuration section at `mj ui settings` as the preferred way to change user-facing settings while a dashboard is open, keeping direct file edits for when none is.


## Concrete Steps

All commands run from the repository root.

    cargo build -p brokk-mjolnir
    cargo test -p brokk-mj-client ui_request
    cargo test -p brokk-mj-tui ui_requests
    cargo test -p brokk-mj-controller ui_requests
    cargo clippy --all-targets -- -D warnings

Manual check, in a separate terminal, using an isolated instance so no live sessions are touched:

    target/debug/mj --instance ui-demo
    target/debug/mj --instance ui-demo ui pane profiles maximized
    target/debug/mj --instance ui-demo ui settings notify.bell=false

Expected CLI output when a dashboard is open:

    Sent to 1 dashboard.

Without a dashboard:

    Error: the Mjolnir API answered 409 Conflict: no dashboard is open to show this; ask the person to start `mj`, then send it again

With a bad value:

    Error: the Mjolnir API answered 400 Bad Request: notify.mode: "loud" is not one of "off", "terminal", "system"

If your shell runs inside a Mjolnir session, clear `MJ_CONFIG_DIR` and `MJ_DATA_DIR` first (for example `env -u MJ_CONFIG_DIR -u MJ_DATA_DIR target/debug/mj --instance ui-demo …`). They take precedence over `--instance`, and a development build refuses to touch the default instance they point at.


## Validation and Acceptance

Acceptance is behavioural. With a dashboard of instance `ui-demo` open, `mj --instance ui-demo ui pane sessions minimized` collapses the Sessions pane within a second, and the size is still minimized after the dashboard restarts, because the dashboard persisted it as its own change. `mj --instance ui-demo ui settings notify.bell=false` opens Settings on the Notifications page with the bell unticked. Pressing Escape asks whether to discard the unsaved change, which proves nothing was written. Pressing Save writes it. A settings request sent while the new-session wizard is open shows a notice and opens Settings when the wizard closes.

Automated tests, following the repository's "Which tests to add" rules. In `mj-tui`: staged settings make the dialog dirty and leave the saved configuration unchanged, and a request held behind an open modal opens only once the modal closes. Both test the state-machine transition directly. In `mj-controller`, `a_dashboard_request_is_published_only_whole_and_only_to_an_attached_dashboard` (in `mj-controller/src/daemon/ui_requests.rs`) checks three things. A request with no dashboard attached is refused. A request with one bad edit among good ones is refused and publishes nothing. A good request appears in the published runtime metadata the dashboard reads. In `mj-client`, `requests_from_a_restarted_daemon_apply_although_their_ids_restart` pins the cursor across daemon incarnations.


## Idempotence and Recovery

Every step is additive. Requests live only in daemon memory, so restarting the daemon clears them. Re-running `mj ui` just sends another request. Nothing migrates the database, and the runtime-feed field defaults to empty, so older dashboards ignore it and older daemons simply never send it.


## Artifacts and Notes

Manual run against isolated instance `uidemo7ca2`, with the dashboard on a pseudo-terminal:

    $ mj ui pane profiles maximized            (before the dashboard started)
    Error: the Mjolnir API answered 409 Conflict: no dashboard is open to show this; ask the person to start `mj`, then send it again
    $ mj ui settings notify.mode=loud
    Error: the Mjolnir API answered 400 Bad Request: notify.mode: "loud" is not one of "off", "terminal", "system"
    $ mj ui settings notify.bell=false advanced.auto_afk_minutes=30
    Staged 2 settings in Settings on 1 dashboard. Nothing is saved until the person presses Save.
      screen: Settings › Notifications, notice "An agent staged notify.bell = false, advanced.auto_afk_minutes = 30. Review and press Save …"
      Esc, Esc: "Discard Settings changes?"; config.toml contained neither key afterwards
    $ mj ui pane sessions minimized
    Sent to 1 dashboard.
      screen notice: "An agent set the Sessions pane to minimized."


## Interfaces and Dependencies

In `mj-client/src/ui_request.rs`:

    pub enum UiPane { Sessions, Targets, Profiles }
    pub enum SessionPlacement { Main, Right, Below }
    pub struct SettingEdit { pub path: Vec<String>, pub value: serde_json::Value }
    pub enum UiAction {
        PaneSize { pane: UiPane, size: mj_core::workspace::PaneSize },
        ShowSession { session_id: String, placement: SessionPlacement },
        StageSettings { edits: Vec<SettingEdit> },
    }
    pub struct UiRequestBody { pub from_session_id: Option<String>, pub action: UiAction }
    pub struct UiRequest { pub incarnation: String, pub id: u64, pub from_session_id: Option<String>, pub action: UiAction }
    pub struct UiRequestReceipt { pub id: u64, pub dashboards: usize }

In `mj-client/src/settings/edit.rs`:

    pub fn stage(draft: &mut Value, path: &[String], value: &Value) -> Result<(), String>
    pub fn stage_all(draft: &mut Value, edits: &[SettingEdit]) -> Result<(), String>

In `mj-tui`:

    impl DashboardState {
        pub fn apply_ui_request(&mut self, request: &UiRequest) -> DashboardAction;
        pub fn take_ui_followup(&mut self) -> DashboardAction;
    }

Revision note (2026-11-04): filled in Progress, Surprises, the readiness-rule decision, Outcomes and Artifacts after implementation and validation.
