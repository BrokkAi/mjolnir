# Give the web viewer a Settings page that shares the terminal's settings model

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir has two user interfaces. The terminal dashboard (`mj`, crate `mj-tui`) has a Settings dialog where a person edits their agent profiles, projects, runtimes, machines, code review, continuation, sub-agents, SessionWiki archiving, web access, display and privacy settings. The browser "web viewer" (served by the background daemon, files under `mj-controller/src/web/`) has no settings at all: its menu offers only Targets, Quota and Sign out. Someone who uses Mjolnir from a phone or a browser tab cannot see or change how Mjolnir is configured.

After this change the web viewer's menu has a Settings item. It opens a page with the same groups, rows and one-line summaries as the terminal's Settings root (Setup, Sessions, Display, Privacy, Other). Each row opens its page, where the same fields can be changed with browser controls (checkboxes, selects, text and number inputs), collection entries can be added and removed, and Save writes the configuration file through the same merge and validation the terminal uses. A change saved in the browser shows up in the terminal within a second, and vice versa.

To see it working: start an isolated instance, open the viewer, sign in, choose Menu → Settings, open Sessions → Continuation, untick Enabled, press Save, and observe that `mj --instance <name> config show` (or the terminal Settings) reports continuation off, and the Settings root row now says "Off".


## Progress

- [x] (2026-10-27 15:10Z) Research: mapped the terminal Settings model (`mj-tui/src/setup/`), the save path (`mj-cli/src/dashboard/io/spawn.rs`), the daemon HTTP surfaces, the viewer page pattern and the web test harness.
- [ ] Milestone 1: shared settings model in `mj-client/src/settings/`, used by the terminal.
- [ ] Milestone 2: daemon settings routes on the signed-in viewer surface.
- [ ] Milestone 3: the web Settings page.
- [ ] Milestone 4: choices that need discovery (code review and sub-agent model and effort).
- [ ] Full validation, documentation, retrospective.


## Surprises & Discoveries

- Observation: the browser surface deliberately cannot edit configuration today, and the snapshot it receives deliberately omits private configuration.
  Evidence: `mj-controller/src/server/actions.rs` documents `ControllerAction` with "Secret/config editing is intentionally not representable here", and `ViewerSnapshot::from_config_state` in `mj-controller/src/server/viewer_types.rs` "never copies profile homes/environment, SSH hosts/keys, container environment, AWS details". This plan keeps both promises: the snapshot is unchanged, the action enum is unchanged, and settings travel only through dedicated routes described below.

- Observation: there are two authenticated HTTP surfaces. The viewer's own routes (`mj-controller/src/server/routes.rs`, layer `require_session`) accept only the signed-in viewer cookie. The `/api/v1` routes (`mj-controller/src/server/api/routes.rs`, layer `require_api_auth`) also accept the bearer token from the api-token file, which agents running inside sessions use through `mj api`.
  Evidence: `require_api_auth` checks `Authorization: Bearer` first and falls back to the cookie; `require_session` calls only `authenticated_viewer`.

- Observation: the terminal does not write configuration through the daemon. `mj-cli` writes the file under its lock (`Config::update_to`), and the daemon notices by polling the file every 500 ms (`spawn_manager_target_refresher` in `mj-controller/src/daemon/process.rs`), installs it and publishes a new snapshot revision.

- Observation: literal environment variable values in profiles and container runtimes are plain strings in the configuration and the terminal shows them unmasked; references (`{from_env}`, `{from_secret}`) are objects. `secrets.toml` never enters the configuration value.


## Decision Log

- Decision: one settings model, shared by both interfaces, lives in a new module `mj-client/src/settings/` (not a new crate). The terminal keeps only terminal concerns (focus, text cursors, popups, glyphs, theme detection against the terminal background).
  Rationale: `AGENTS.md` asks that shared interpretation of shared data live in one place, and that a fact have one owner. `mj-tui` and `mj-controller` both already depend on `mj-client`, and nothing in the model needs a terminal. A new crate is not justified by a dependency boundary.
  Date/Author: 2026-10-27, Claude.

- Decision: the browser never interprets configuration. The daemon renders each settings page into plain data (labels, summaries, control kinds, choices, help) and applies each edit to the draft itself; the page sends an edit intent and the current draft, and receives the new draft and its rendering.
  Rationale: the terminal's labels, summaries, choices and edit rules (clearing to a default, whole-number ranges, rebuilding an entry when its type changes, resetting effort when a model changes, removing a machine's runtimes with it) are about a thousand lines of Rust. Re-implementing them in JavaScript would create a second predicate for every one of those facts. A stateless request per edit costs one round trip, which is small next to typing, and keeps the daemon free of per-browser state that a restart would lose.
  Date/Author: 2026-10-27, Claude.

- Decision: settings routes live only on the viewer's cookie-authenticated surface (`/api/settings…` inside the `protected` group of `mj-controller/src/server/routes.rs`), never under `/api/v1`.
  Rationale: the bearer token is handed to agents inside sessions. An agent must not be able to rewrite the user's profiles, machines or runtimes. The signed-in browser is the user.
  Date/Author: 2026-10-27, Claude.

- Decision: literal environment values never leave the daemon. In everything sent to the browser, each literal value under `profiles.<id>.environment` and `targets.<id>.environment` is replaced by the marker object `{"mj_hidden_value": true}`. The page shows such an entry as "Set (hidden)" and offers to replace it. On save the daemon puts the real value back from the file before merging; a marker whose value has since vanished from the file is refused with an explanation. Other details the snapshot omits (profile account directories, SSH hosts and key file paths, AWS profile names, container images) are shown on the Settings page, because they are not secrets, the page is fetched only on demand by the signed-in user, and the user cannot edit what they cannot see.
  Rationale: keeps secrets out of browser memory, caches and screenshots while giving the page full parity. The marker cannot deserialize as an environment value, so it can never be written to the file by accident.
  Date/Author: 2026-10-27, Claude.

- Decision: flows that need the terminal itself stay there, and the page says so in place with the exact command. Signing a profile in runs an interactive login in a terminal (`mj login --profile <id>`), so the web profile page shows that command instead of a Sign in button. Detect profiles, Detect runtimes, the machine build-cache preview and "Install mbx", the SessionWiki archive-space estimate and remote `~` resolution are left to the terminal in this plan and named on the pages where the terminal offers them.
  Rationale: these flows probe the host, other machines over SSH, or hand the terminal to a child process. They are separate features with their own failure modes; folding them into this plan would delay the core page. The notice gives the user the next step instead of a dead end.
  Date/Author: 2026-10-27, Claude.

- Decision: saving holds daemon admission under its own label, "settings save", taken with `crate::upgrade::activity_unless_draining` so a pending upgrade handoff refuses new saves (the browser's `request` helper already retries through the handoff's 503), and it holds the daemon's `config_mutation` lock while writing so it serializes with the daemon's other configuration writers.
  Rationale: `AGENTS.md` requires every daemon-owned operation to participate in handoff admission with a label of its own; the write is short and bounded.
  Date/Author: 2026-10-27, Claude.


## Outcomes & Retrospective

Nothing yet.


## Context and Orientation

Configuration. The user's configuration is a TOML file loaded into `mj_core::config::Config` (`mj-core/src/config.rs`). Serializing a `Config` with serde produces the "stored shape" JSON value, in which a runtime names its machine (`StoredConfig`). `Config::update_to(path, edit)` takes a file lock, loads, applies `edit`, validates and writes atomically, preserving comments. `Config::with_local_targets()` adds the built-in local runtimes (bare, podman, docker) that exist without being written to the file.

The terminal Settings dialog. `mj-tui/src/setup.rs` and `mj-tui/src/setup/*.rs` implement it. The dialog holds a "draft": the stored-shape JSON of the configuration, plus every optional field filled in with its default so the form can show it (`schema::expand`), plus the local machine entry (`ensure_local_machine` in `draft.rs`). It also keeps `original`, the stored-shape JSON before any edit. A "path" is the list of keys from the root to a value, for example `["targets", "podman", "image"]`; `draft.rs::pointer` turns a path into a JSON pointer, after `storage_path` maps the synthetic Interface page (`["interface", "theme"]` is stored at `["theme"]`, `["interface", "prefix"]` at `["keys", "prefix"]`). The files divide as follows. `page.rs` lists the root groups (`ROOT_GROUPS`), which keys a page shows (`visible_keys`, `hidden_key`), whether a page is a collection the user names entries in (`is_collection`), row labels and breadcrumbs, and value summaries (with terminal glyphs). `schema.rs` holds defaults, labels, help, choices, choice labels, what an empty value means (`null_label`), whole-number ranges, path kinds, and the root summaries (`section_summary`); only its theme report needs the terminal. `draft.rs` holds path helpers, `config_from_draft` (turning a draft back into a `Config`), and the edit rules in `SetupDialog::apply_value`, `add`, `apply_editor`; `events.rs` holds Remove. `form.rs::reconcile_controls` decides each row's control kind; `form.rs::actions` decides which of Back, Add, Remove, Detect, Sign in and Save a page shows. `review_settings.rs` is the separate Code Review dialog.

Saving from the terminal. `SetupDialog::save` sends `DashboardAction::SaveSetup { original, updated }`; `mj-cli/src/dashboard/io/spawn.rs::save_setup_at` validates the updated configuration, checks changed profiles are ready (`HarnessProfile::ensure_ready`), and under `Config::update_to` performs a three-way merge (`merge_setup_edit`): fields this edit did not change keep whatever the file has now, fields it changed are applied, and a field both this edit and another client changed is refused. `State::validate_setup_update` (in `mj-core/src/state.rs`) refuses changes to a profile, project or runtime an active session uses. Tests for this live in `mj-cli/src/dashboard/io.rs` (search `save_setup_at`).

The daemon and the web viewer. The daemon (`mj daemon-run`, crate `mj-controller`) serves the web viewer over HTTP. `mj-controller/src/server/routes.rs::router` registers the viewer's routes; the `protected` group requires the signed-in viewer cookie (`require_session`). Errors on that surface are `ApiError` (`mj-controller/src/server/errors.rs`). Blocking work runs in `tokio::task::spawn_blocking`. Upgrade admission is in `mj-controller/src/upgrade.rs` (`activity`, `activity_unless_draining`). The viewer itself is `mj-controller/src/web/viewer.html`, `viewer.js` and `viewer.css`, plain JavaScript with no framework; pages are `<section class="page">` elements, routes are URL hashes parsed by `ROUTE_PATTERNS`, written by `routeHash`, titled in `applyRoute` and drawn by `renderRoute`. All DOM is built with `el()` and `textContent`, never markup strings. `request(url, options)` performs authenticated JSON requests, retries through upgrade handoffs and throws an `Error` with the server's message.

Web tests. `tests/e2e/web/` holds Playwright specs run with `npm test` from that directory (CI job `web-viewer` in `.github/workflows/ci.yml`). The deterministic specs load the real viewer files and answer its requests from fixtures with `page.route`; `golden.mjs::assertGolden(name, text)` compares page text with `tests/e2e/web/golden/<name>.txt` (set `MJ_UPDATE_GOLDEN=1` to rewrite). Rust golden files go through `mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), name, text)` into `<crate>/tests/golden/<name>.txt`. Viewer HTTP router tests build the router with `app()` in `mj-controller/src/server/tests.rs` (login code `123456`).


## Plan of Work

### Milestone 1: one settings model for both interfaces

Create `mj-client/src/settings/mod.rs` and declare `pub mod settings;` in `mj-client/src/lib.rs`. Move into it, unchanged in behavior and made `pub`: from `mj-tui/src/setup/schema.rs` everything except `theme_report` and `draft_theme_report`; from `page.rs` `ROOT_GROUPS`, `PageRow`, `page_plan`, `root_group`, `hidden_root_key`, `visible_keys`, `hidden_key`, `is_collection`, `row_label`, `breadcrumb`; from `draft.rs` `storage_path`, `pointer`, `only_unset_fields`, `config_from_draft`, `ensure_local_machine`. Split them into `settings/schema.rs` (defaults, labels, help, choices, summaries, numbers, path kinds), `settings/page.rs` (page structure) and `settings/draft.rs` (paths, opening a draft from a `Config`, turning a draft back into one). Add `settings::draft::open(config: &Config) -> (Value, Value)` returning `(original, draft)` exactly as `SetupDialog::new` builds them today, and make `SetupDialog::new` call it.

`schema::section_summary` and `schema::choice_label` currently call the terminal theme report for the theme. Give the shared `section_summary` a parameter `theme: &dyn Fn(&Value) -> String` that names the theme value; the terminal passes its existing `draft_theme_report`, and the daemon passes `choice_label` of the theme. The `ThemeChoice` labels themselves come from `mj_core::config::ThemeChoice` and need no terminal.

Extract the edit rules into pure functions over a draft in `settings/edit.rs`, and make the terminal call them so there is one implementation:

    pub enum SettingInput { Text(String), Choice(Value), Toggle(bool) }
    pub fn apply_value(draft: &mut Value, path: &[String], input: SettingInput) -> Result<(), String>;
    pub fn add_entry(draft: &mut Value, collection: &[String], name: Option<&str>) -> Result<Vec<String>, String>;
    pub fn remove_entry(draft: &mut Value, entry: &[String]) -> Result<RemoveOutcome, String>;

`apply_value` is the body of `SetupDialog::apply_value` minus the terminal's field bookkeeping: clearing to a default, local path expansion (`mj_core::path_input::expand_local`), relative destination checks, whole numbers, build-cache budgets, prefix validation, type changes rebuilding an entry from defaults, and resetting a sub-agent effort when its model changes. It does not know about the build-cache "user managed" guard (that needs the terminal's machine preview), which stays in the terminal's wrapper. `add_entry` is `add` plus `apply_editor` for a named entry; `remove_entry` is the Remove handler, including removing a machine's runtimes and refusing to remove the local machine, and returns the runtimes it removed so the caller can report them. The terminal keeps re-binding its pending text fields after these return.

Move `save_setup_at` and `merge_setup_edit` from `mj-cli/src/dashboard/io/spawn.rs` to `settings/save.rs` as `pub fn save_at(path, original, updated, state) -> Result<Config>` and `pub fn merge_edit(...)`, and move their tests from `mj-cli/src/dashboard/io.rs` with them. `mj-cli` calls the moved functions.

At the end of this milestone nothing a user can see has changed. Proof: the terminal's Settings goldens (`mj-tui/tests/golden/settings-root.txt`, `settings-search.txt`, `review-settings.txt`, `build-cache.txt` and the rest) and the `mj-tui` setup tests pass unchanged, and the moved save tests pass in `mj-client`.

### Milestone 2: daemon settings routes

Add `settings/view.rs` to `mj-client`: a serializable rendering of one page of a draft.

    pub struct SettingsPage { path, breadcrumb: Vec<String>, help: String, groups: Vec<SettingsGroup>, can_add: Option<String /* button label */>, can_remove: bool, notices: Vec<String> }
    pub struct SettingsGroup { heading: Option<String>, rows: Vec<SettingsRow> }
    pub struct SettingsRow { path: Vec<String>, label: String, help: String, control: SettingsControl }
    pub enum SettingsControl {
        Page { summary: String },
        Checkbox { checked: bool },
        Choice { value: Value, choices: Vec<SettingsChoice> },   // SettingsChoice { value: Value, label: String }
        Text { value: String, placeholder: String, number: bool },
        Hidden,                                                   // a literal environment value the browser may replace but not read
        ReadOnly { text: String },
    }
    pub fn render_page(draft: &Value, path: &[String]) -> Result<SettingsPage, String>;

The control kind is decided by one shared function `settings::page::row_kind`, extracted from `form.rs::reconcile_controls`, which the terminal also calls. Summaries use the shared `section_summary`, `choice_label` and `null_label`. `can_add` and `can_remove` follow `form.rs::actions`. `notices` carries the in-place pointers to terminal-only flows decided above (for example on `["profiles", id]`: "Sign in from a terminal: mj login --profile <id>").

Add `settings/hidden.rs` with `hide(value: &mut Value)` (replace literal environment values with the marker) and `restore(value: &mut Value, current: &Value) -> Result<(), String>` (put them back from the stored configuration, refusing a marker whose value is gone).

In `mj-controller/src/server/`, add `settings.rs` with three handlers registered in the `protected` group of `routes.rs`:

`GET /api/settings` loads the configuration from the file in `spawn_blocking` (`Config::load` then `with_local_targets`, as the terminal sees it), opens a draft, hides secrets in both values, and returns `{ "original": …, "draft": …, "page": render_page(draft, []) }`.

`POST /api/settings/edit` with `{ "draft": …, "path": [...], "edit": {"kind": "set", "input": {...}} | {"kind": "add", "name": …} | {"kind": "remove"} , "page": [...] }` applies the edit with the shared functions and returns `{ "draft": …, "page": render_page(draft, page_after_edit), "error": null }`. An edit refused by the rules (for example "Enter a whole number from 1 to 64.") returns HTTP 200 with the draft unchanged and `error` set to the message, so the page can show it next to the field. Pure computation; no admission, no file access.

`POST /api/settings/save` with `{ "original": …, "draft": … }` takes `activity_unless_draining("settings save")` and the daemon's `config_mutation` lock, then in `spawn_blocking` loads the current file, restores hidden values in both, turns the draft into a configuration (`config_from_draft`), and calls the shared `save_at` with the daemon's current session state for `validate_setup_update`. On success it returns a fresh `GET`-shaped document from the saved configuration. A conflict with another client's change or a refused change returns HTTP 409 or 400 with the message the terminal shows for the same refusal. The daemon's 500 ms poll installs the new configuration and publishes a snapshot revision, exactly as after a terminal save.

Update the comment on `ControllerAction` to say configuration is edited through the settings routes, not actions.

### Milestone 3: the web page

In `viewer.html` add `<button role="menuitem" data-route="settings">Settings</button>` before Sign out, and a `<section id="settings-page" class="page hidden">` with a breadcrumb, help text, the page body, a notice area, an error area (`role="alert"`) and a save bar (Save, Discard) that is visible only when the draft differs from the original. In `viewer.js` add the routes `#settings` and `#settings/<encoded path>` to `ROUTE_PATTERNS` and `routeHash`, the title in `applyRoute`, and `renderSettings()` in `renderRoute`. Settings state is `{ original, draft, page, pending, error }` in one object owned by the page; every request carries a generation number and a stale reply is dropped. Entering the route fetches `GET /api/settings` unless a draft is already open. Each control sends one edit: a checkbox on change, a select on change, a text or number input on blur or Enter. Rows of kind Page are links to their page's route. Add and Remove ask for a name (Add on a named collection) with the page's own inline form, not `prompt()`. Save posts the draft; Discard drops the draft and refetches. Leaving the Settings route with unsaved changes keeps the draft in memory so returning shows it, and the menu item shows "Settings •" while a draft is unsaved. In-flight requests disable the control that started them and show "Saving…" on the save bar. Add the needed styles to `viewer.css`, reusing `.card`, `.field` and the existing row styles, and check the phone layout at 390 px wide.

### Milestone 4: choices that need discovery

The terminal offers model and effort choices for Code Review (from the daemon's profile capability snapshot, `mj-controller/src/review_settings.rs::discover_review_settings`) and for sub-agents (`_subagent_options`, from `mj-controller/src/controller/profile_config.rs::subagent_options_with`). Give `render_page` an optional `SettingsDiscovery` argument that supplies those choice lists; the daemon fills it from what it already has in memory, and until it has an answer the rows render as `ReadOnly` with "Discovering models…" exactly as the terminal does. Edits validate the chosen value against the same lists.


## Concrete Steps

All commands run from the repository root, `/workspace/f9e94b5bad9069301c0a302764419efd/mjolnir`. Cargo tests must run outside the restricted sandbox because they use loopback sockets.

Milestone 1:

    cargo test -p brokk-mj-client settings
    cargo test -p brokk-mj-tui setup
    cargo test -p brokk-mjolnir save_setup

Milestone 2:

    cargo test -p brokk-mj-controller settings

Milestone 3:

    cd tests/e2e/web && npm ci && npx playwright install chromium && npx playwright test --project deterministic settings.spec.js

To see the page in a real daemon, build and run an isolated instance (never the default one; inside a Mjolnir session unset `MJ_CONFIG_DIR` and `MJ_DATA_DIR` first so `--instance` takes effect):

    cargo build -p brokk-mjolnir
    env -u MJ_CONFIG_DIR -u MJ_DATA_DIR ./target/debug/mj --instance websettings daemon start
    env -u MJ_CONFIG_DIR -u MJ_DATA_DIR ./target/debug/mj --instance websettings daemon status

Open the printed URL, enter the printed six-digit code, choose Menu → Settings.

Before finishing: `cargo clippy --all-targets -- -D warnings` for the workspace and the full test suites of `mj-client`, `mj-tui`, `mj-cli` and `mj-controller`, plus `npm test` in `tests/e2e/web`.


## Validation and Acceptance

Milestone 1 is accepted when the terminal behaves byte-for-byte as before: every existing `mj-tui` Settings golden and test passes without updating a golden file, and the moved save tests pass.

Milestone 2 is accepted with these tests. A Rust golden test drives the real router (`app()` in `mj-controller/src/server/tests.rs`) through sign-in, `GET /api/settings` on a fixture configuration that includes a profile with a literal environment value and a container runtime, an edit that turns continuation off, an edit that sets the sub-agent limit to 99 (refused with "Enter a whole number from 1 to 64."), and a save; the whole JSON exchange is compared with `mj-controller/tests/golden/viewer-settings-exchange.txt`. The same test asserts that the literal environment value's text appears in no response body, and that after the save the file still holds that value unchanged (security boundary). A second test saves a draft after another writer changed the same field in the file and expects HTTP 409 with the merge's conflict message (concurrency). A request with only the bearer token, no cookie, receives 401.

Milestone 3 is accepted with a Playwright golden: the deterministic spec answers `/api/settings…` with the responses recorded in the Milestone 2 golden, opens Menu → Settings, captures the root page, opens Continuation, unticks Enabled, saves, and compares the page text at desktop and 390 × 844 phone sizes with `tests/e2e/web/golden/viewer_settings.txt`; the spec also checks that the request bodies the page sent equal those in the Rust exchange, which ties the two sides together. Manually, in an isolated instance, the change is visible in the terminal Settings within a second of saving in the browser.

Milestone 4 is accepted when, in the exchange golden, the Code Review page offers the same model and effort choices the terminal's review dialog offers for the same capability snapshot.


## Idempotence and Recovery

Every step is additive or a move with its tests. If a move breaks a terminal golden, compare the moved function with its original in `git show HEAD:<path>`; goldens must not be regenerated to hide a difference. Settings saves are atomic file writes under the configuration lock; a failed save leaves the file as it was. Test daemons use `--instance` names and their own data directories and can be removed with `mj --instance <name> daemon stop`.


## Artifacts and Notes

None yet.


## Interfaces and Dependencies

No new crates or third-party dependencies. `mj-client` gains `pub mod settings` with submodules `schema`, `page`, `draft`, `edit`, `save`, `hidden` and `view`, whose public functions are named above. `mj-tui` and `mj-cli` call them in place of their private copies. `mj-controller` gains `src/server/settings.rs` with the three handlers and registers them in `src/server/routes.rs`. The web viewer gains the `#settings` route and its page in `viewer.html`, `viewer.js` and `viewer.css`.
