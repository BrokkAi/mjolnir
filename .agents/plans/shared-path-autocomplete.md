# Shared path autocomplete for every path input

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Wherever Mjolnir asks for a filesystem path, the user can now ask for completions. In the terminal UI, pressing Ctrl-Space in any path field opens a popup of matching directories (or files, where the field wants a file) on the host that owns that path: the controller machine for local settings and bundle sources, the SSH host for remote bare targets and remote container engines. Up and Down move through the popup, Enter inserts the highlighted entry, Escape or any edit closes it. In the web UI, typing in the project-directory or repository-source field shows live suggestions from the same host after a short pause, without ever rewriting the text being typed. The desktop application embeds the web UI and gains the same behavior.

Before this change, completion existed only for the mount-source field of the new-session and move wizards, drawn as plain lines of text, and it refused the "bare" targets that own every project-directory path.

## Progress

- [x] (2026-09-18 14:35Z) Surveyed every path input, the existing mount-only completion, the SSH command plumbing, and the web surfaces. Wrote this plan.
- [x] (2026-09-18 15:00Z) Milestone 1: core completion primitives, shared host, one controller entry point. Committed as 05da4a5a.
- [x] (2026-09-18 15:00Z) Milestone 2: `PathInput` completion state, `ControlKind::PathField`, popup rendering and form routing. Committed with milestone 1.
- [x] (2026-09-18 15:40Z) Milestone 3: every terminal screen wired through one routing helper; generic dashboard job with its own cancellation slot.
- [ ] Milestone 4: web endpoint and live suggestions in `pathField()`.
- [ ] Full validation, commit, push.

## Surprises & Discoveries

- Observation: The only existing completion trigger is Ctrl-Space, not Tab, because `Form` consumes Tab for focus movement.
  Evidence: `mj-tui/src/wizards/dashboard.rs:176-187` matches `KeyModifiers::CONTROL` with `KeyCode::Char(' ')`.
- Observation: The mount completer refuses bare targets, yet bare targets are the only ones that ask for a project directory, so no project-directory field could ever complete.
  Evidence: `mj-controller/src/controller.rs:586-588` bails with "resource path completion is unsupported for bare targets".
- Observation: A host abstraction already exists for the cache code and does exactly what completion needs: it names a machine (local or SSH), keys cached answers per machine, and passes user text to `sh -c` as `$1` instead of interpolating it.
  Evidence: `mj-controller/src/controller/cache_host.rs`, `CacheHost::shell_command`.
- Observation: Ctrl-Space is unreliable in browsers (macOS reserves it for input-source switching), so the web surface must use live suggestions.
- Observation: The form moves focus on Tab before it reports the popup dismissal, so by the time a screen routes `PathDismiss` the focused control is already the next field. The rule became "only the focused field may keep a popup", enforced by `CompletesPaths::dismiss_unfocused_completions` at the start of routing, which also covers clicks and any other focus change.
  Evidence: the `tab_leaves_the_field_and_closes_the_popup` test in `mj-tui/src/wizards/tests.rs` failed under the narrower "dismiss the focused input" rule.
- Observation: A popup drawn in the middle of a form is overdrawn by the rows rendered after it in the same frame. The mount editor and the container editor redraw the focused path field after the rest of the form, the same trick the access combobox already used.
- Observation: The old single-candidate wizard test passed a prefix that no longer matched the field, so its assertion never ran. It now asserts that a single candidate is inserted without opening a popup.

## Decision Log

- Decision: The terminal keeps Ctrl-Space on demand; the web uses live debounced suggestions. The controller method, candidate format, staleness rules, and endpoint are shared; only the trigger differs per client.
  Rationale: Tab is focus navigation in terminal forms; browsers do not deliver Ctrl-Space reliably. The trigger is a client convention, not shared logic.
  Date/Author: 2026-09-18 / Jonathan Ellis with Claude
- Decision: Keep `CacheHost` where it is and extend it with path-aware constructors and one cached home probe, rather than moving or renaming it.
  Rationale: It already models "a machine, not a runtime". Moving it to mj-core would drag controller-only helpers along; renaming touches many call sites for no behavior change.
  Date/Author: 2026-09-18 / Claude
- Decision: Completion state lives inside `PathInput`, with a new `ControlKind::PathField` so the shared `Form` routes popup keys and clicks. No per-prefix cache.
  Rationale: Every screen already owns a `PathInput`; the form already knows how to route an anchored popup for comboboxes. A cache keyed by prefix alone goes stale when the target changes.
  Date/Author: 2026-09-18 / Claude
- Decision: Container-internal paths (mount destination, repository destination) do not complete; the chat composer `/attach` argument is out of scope.
  Rationale: The container does not exist before provisioning. The composer has its own autocomplete state machine and is a separate change.
  Date/Author: 2026-09-18 / Jonathan Ellis
- Decision: Fields that accept a URL or `owner/repo` as well as a path complete only when the text starts with `/`, `~`, `.`, or a Windows drive.
  Rationale: The same predicate already decides "local path, not a URL" in `mj-core/src/remote_git.rs`; sharing it keeps one interpretation.
  Date/Author: 2026-09-18 / Claude
- Decision: The web ignores the controller's `insert` (common prefix) and only shows candidates.
  Rationale: Live suggestions must never rewrite text the user is still typing.
  Date/Author: 2026-09-18 / Claude

## Outcomes & Retrospective

Pending.

## Context and Orientation

Mjolnir is a Rust workspace. The pieces this plan touches:

- `mj-core` holds shared types with no I/O policy: `mj-core/src/path_input.rs` interprets `~` (`needs_home`, `expand_home`, `expand_local`), `mj-core/src/targets.rs` and `mj-core/src/targets/ssh.rs` build command lines (`CommandSpec`) and describe target kinds (`TargetTemplate`), and today also hold `local_directory_completions`, `path_completion`, and `ssh_directory_completions`. `mj-core/src/config/machines.rs` defines `Machine { Local, Ssh, AwsEc2 }`, a configured computer as distinct from a runtime.
- `mj-controller` runs commands. `mj-controller/src/controller/cache_host.rs` defines `CacheHost { Local, Ssh(SshTarget) }` with `for_target`, `for_machine`, `key()`, `command()`, and `shell_command()`. `mj-controller/src/controller.rs` has `complete_mount_source` (to be deleted), `resolve_input_path`, `resolve_target_input_path`, `resolve_machine_input_path`, and `resolve_ssh_input_path` (a `$HOME` probe duplicated in `controller/mbx.rs`). The web server lives in `mj-controller/src/server/` (routes, handlers, actions) with browser assets in `mj-controller/src/web/` (vanilla JavaScript, no build step, CSP forbids inline script and style); the controller-side loop that services web requests is `mj-controller/src/server_runtime/`.
- `mj-chat` holds terminal widgets. `mj-chat/src/components/scope.rs` defines `Form`, `ControlKind`, and `Interaction`; `mj-chat/src/components/controls.rs` defines `TextField` and `ComboBox`; `mj-chat/src/components/layout.rs` defines `AutocompletePopup`; `mj-chat/src/path_input.rs` defines `PathInput`, a newtype over `TextInput`.
- `mj-tui` holds terminal screens: new-session and move wizards (`mj-tui/src/wizards*`), the running-container editor (`mj-tui/src/dialogs/container.rs`), Settings (`mj-tui/src/setup.rs`), and the repository-origin repair dialog (`mj-tui/src/dialogs.rs`). `DashboardAction` is the enum of things a screen asks the host process to do (`mj-tui/src/lib.rs`).
- `mj-cli` runs the terminal application. `mj-cli/src/dashboard/actions.rs` turns a `DashboardAction` into supervised background work through `spawn_cancellable_io` (`mj-cli/src/dashboard/io/spawn.rs`), and `mj-cli/src/dashboard/io.rs` delivers `DashboardIoUpdate` results back to the screen. `mj-cli/src/dashboard/drafts.rs` tracks one in-flight path job and cancels it when the screen's "path input context" fingerprint (`path_input_context()` in `mj-tui/src/wizards/dashboard/paths.rs`) changes.

Terms: a "target" is a configured place sessions run (`TargetTemplate`: local or SSH, bare or container). A "bare" target runs the harness directly on the machine; it is the only kind that asks for a project directory. A "machine" is the computer behind a target. The "controller host" is the machine running Mjolnir itself. A "popup" is the anchored list `AutocompletePopup` draws above or below a field.

Path inputs and the host each belongs to:

- New-session wizard project directory: the target's machine (local or SSH). Directories.
- New-session wizard bundle source: controller host, and only when the text looks like a path. Directories.
- Mount source (new-session wizard, move wizard, container editor): the container engine host (local or SSH). Directories.
- Settings keys classified `PathKind::Local` in `mj-tui/src/setup/schema.rs` (`profiles.*.home`, `phone.tls_cert`, `phone.tls_key`, `machines.*.identity_file`, `bundles.*.repositories.*.local`): controller host. Files too for the three file keys.
- Settings keys classified `PathKind::Target` (`machines.*.workspace_prefix`, `machines.*.build_cache.directory`, `targets.*.workspace_storage.root`): the configured `Machine`. Directories.
- Repository-origin repair replacement: controller host, only when it looks like a path. Directories.
- Web project directory: the target's machine. Web repository source: controller host when it looks like a path.

Not completed: mount destinations, container-editor destinations, and `bundles.*.repositories.*.destination` (inside a container that does not exist yet); `/attach` in the chat composer.

## Plan of Work

### Milestone 1: core primitives and one controller entry point

Create `mj-core/src/path_completion.rs` and register it in `mj-core/src/lib.rs`. It holds `CompletionKind { Directories, Any }` (serde snake_case), `CompletionHost { Local, Target(String), Machine(Box<config::Machine>) }`, `PathCompletion { candidates: Vec<String>, insert: Option<String>, truncated: bool }` (serde, Default), `MAX_CANDIDATES = 50`, `looks_like_path(&str) -> bool`, and the three primitives moved from `targets.rs` and `targets/ssh.rs`: `local_completions(prefix, kind)`, `ssh_completions(ssh, prefix, kind, executor)`, and `common_insert(prefix, candidates)`. For `Any`, the local walk includes files without a trailing slash; the remote command becomes `ls -dp -- '<quoted prefix>'* 2>/dev/null` so directories carry the slash. For `Directories` the remote command stays `ls -d -- '<quoted prefix>'*/ 2>/dev/null`. The remote output filter keeps only lines that extend the prefix by exactly one component and contain no U+FFFD. `looks_like_path` reuses `is_windows_absolute_path` from `remote_git.rs`, and `remote_git.rs` calls the shared predicate. Delete the old three functions.

Extend `CacheHost` with `for_path_target(&targets::TargetTemplate) -> Self` (every local kind and AwsEc2 map to `Local`; every SSH kind, including `SshBare`, maps to `Ssh`), `for_path_machine(&config::Machine) -> Result<Self>` (expands `identity_file` with `expand_local` exactly as `resolve_ssh_input_path` does), and `home(&self, executor) -> Result<PathBuf>`. `home` returns `dirs::home_dir()` for `Local` and otherwise runs `printf '%s' "$HOME"` through `shell_command`, requiring exit status 0 and an absolute result, caching successes for ten minutes in a static map keyed by `key()` in the style of `mbx.rs`. Replace `mbx::home` and `resolve_ssh_input_path` with it.

Create `mj-controller/src/controller/path_completion.rs` with `Controller::complete_path(&self, host: &CompletionHost, prefix: &str, kind: CompletionKind, executor) -> Result<PathCompletion>`: empty prefix returns the default; map the host to a `CacheHost` (`Target` looks up `config.targets`); probe home only when `needs_home`; `expand_home`; re-append a trailing `/` the expansion dropped; dispatch to the local or SSH primitive; fold candidates back to `~/...` when a home was used (files fold without a slash); truncate to `MAX_CANDIDATES` and set `truncated`; compute `insert` with `common_insert` over the folded list. Delete `complete_mount_source`.

### Milestone 2: widget and form

Change `PathInput` to `{ text: TextInput, completion: Completion }` where `Completion { candidates, selected, truncated, requested: Option<String> }`. Public methods: `completions()`, `completion_selected()`, `is_completing()`, `completion_pending()`, `control_kind()`, `request_completion() -> Option<String>` (None when empty or already pending), `apply_completion(prefix, PathCompletion) -> bool` (ignored unless the value still equals the prefix; sets the value to `insert` when present; opens the popup when more than one candidate), `select_completion(i)`, `accept_completion() -> bool`, `dismiss_completion()`. Keep `Deref` to `TextInput` and the conversions. `PathField::apply` dismisses after a changed edit.

In `scope.rs` add `ControlKind::PathField { len, selected, expanded }` (a field for cursor purposes, a popup for navigation), `Interaction::Complete(K)`, `Interaction::PathCommit(K, usize)`, `Interaction::PathDismiss(K)`, and `register_popup(id, area, row_map)` that leaves the field's cursor map intact. In `handle_key`, Ctrl-Space (`Char(' ')` with CONTROL, or `KeyCode::Null`) on a path field emits `Complete` before the modifier gate; while expanded, Tab moves focus and emits `PathDismiss`, Escape emits `PathDismiss`, Up/Down/Home/End/Page keys emit `Select`, Enter emits `PathCommit`; otherwise the ordinary field handling applies. Pointer handling treats the popup like a combobox popup: a row click commits, a press elsewhere dismisses.

In `controls.rs` factor `TextField::render` into `render_with_kind`. `PathField::render` keeps its signature, registers `input.control_kind()`, and when focused draws the popup with `AutocompletePopup` and a highlighted `List`, registering geometry through `register_popup`; while a request is pending it draws a one-row "Completing…" popup; when truncated the title says "first 50 matches · keep typing".

### Milestone 3: screens and dashboard plumbing

In `mj-tui/src/wizards/dashboard/paths.rs` add `trait CompletesPaths { fn focused_path_input(&mut self, dashboard: &DashboardState) -> Option<(&mut PathInput, CompletionHost, CompletionKind)> }` and `route_path_completion(dashboard, screen, interaction) -> Result<DashboardAction, Option<Interaction<K>>>`, which consumes `Complete` (emitting `DashboardAction::CompletePath { host, kind, prefix }`), `PathCommit`, `PathDismiss`, and `Select` while completing, and hands everything else back. Add `DashboardState::apply_path_completions(context, prefix, completion)`, which drops stale replies by comparing `path_input_context()`, then applies to the focused input of the current screen. Extend `path_input_context()` with the bundle source and a repository-origin arm. Implement `CompletesPaths` for `NewWizard`, `ResumeWizard`, `ContainerEditor`, `SetupDialog` (add `schema::completion_kind`), and `RepositoryOriginDialog`. Each screen declares its path controls with `input.control_kind()`, routes completion interactions before its own match, and sends path edits through `PathField::apply`. Remove the wizard's mount-only completion code and fields.

Replace `DashboardAction::CompleteMountSource` with `CompletePath { host, kind, prefix }` and `DashboardIoUpdate::MountCompletions` with `PathCompletions { context, prefix, result }`. The job captures `path_input_context()` when spawned, runs `complete_path` under a `CancellableProcessExecutor` with a five-second deadline, and registers its cancel token in a new `completion_job` slot in `drafts.rs`; `cancel_stale_path_input` cancels both slots when the context changes.

### Milestone 4: web

Add `PreflightRequest::CompletePath` carrying host, prefix, kind, and a oneshot reply. Add `POST /api/paths/complete` in the cookie-guarded router, accepting `{ target_id: Option<String>, prefix, kind }` with unknown fields rejected, prefixes over 4096 bytes and unknown targets rejected with 400, and controller failures mapped to 503 with non-leaky prose. The runtime loop services the request like a preflight: a `ProcessCancellationGuard`, `spawn_blocking`, a five-second deadline, and cancellation when the browser aborts.

In `viewer.js` add `attachPathSuggestions(input, complete)` directly above `pathField`, with per-instance state, a 250 ms debounce, an `AbortController` per request, and a `div.field-suggestions[role=listbox]` of `button.palette-row[role=option]` rows plus a dim "More matches — keep typing" row when truncated. Replies are ignored if aborted, if the value changed, or if the input lost focus. ArrowUp/ArrowDown move, Enter accepts and re-dispatches `input`, Escape hides, rows accept on click (with `mousedown` prevented so focus stays), blur hides. `pathField` gains an optional `complete` argument; the project-directory field passes the selected target and the repository-source field passes the local host gated by `looksLikePath`. Add `.field { position: relative }` and `.field-suggestions` styles to `viewer.css`.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2` on branch `hel2`. Run Cargo tests outside the restricted sandbox with elevated permissions.

    cargo test -p brokk-mj-core -p brokk-mj-controller     # after milestone 1
    cargo test -p brokk-mj-chat                             # after milestone 2
    cargo test -p brokk-mj-tui -p brokk-mjolnir             # after milestone 3
    cargo test -p brokk-mj-controller server::tests         # after milestone 4
    cd tests/e2e/web && npm test                            # after milestone 4
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Commit each validated milestone on `hel2`, staging only the files changed. Push to the configured upstream when everything is complete (authorized by the user on 2026-09-18).

## Validation and Acceptance

Terminal: start `mj`, begin a new session on a local bare target, type `~/pr` in the project directory, press Ctrl-Space. A popup appears listing `~/...` directories; Down then Enter inserts the highlighted directory; Escape closes the popup without changing the text; typing closes it. Repeat on an SSH bare target: the popup lists remote directories. In Settings, edit `machines.<id>.identity_file`, type `~/.ssh/id`, press Ctrl-Space: files are listed without a trailing slash. Typing `owner/repo` in the bundle source and pressing Ctrl-Space does nothing.

Web: open the viewer, start a new session on an SSH bare target, type `/srv/` in the project directory. After a pause, suggestions appear beneath the field and the typed text is unchanged. ArrowDown then Enter inserts a suggestion and lists its children. Typing `owner/repo` as a repository source sends no request.

Tests: the commands above pass. New behavior tests cover kinds and quoting in core, host mapping and home caching in the controller, popup routing in the form, one Ctrl-Space flow per terminal screen, stale-reply rejection, the web handler, the browser slice, and the Playwright flow.

## Idempotence and Recovery

All changes are source edits and can be re-applied. If a milestone's tests fail, fix forward on the branch. No configuration, database, or user data changes. Commits are per milestone so a failed milestone can be reverted alone.

## Artifacts and Notes

Pending.

## Interfaces and Dependencies

No new crates. In `mj-core/src/path_completion.rs`:

    pub enum CompletionKind { Directories, Any }
    pub enum CompletionHost { Local, Target(String), Machine(Box<crate::config::Machine>) }
    pub struct PathCompletion { pub candidates: Vec<String>, pub insert: Option<String>, pub truncated: bool }
    pub const MAX_CANDIDATES: usize = 50;
    pub fn looks_like_path(text: &str) -> bool;
    pub fn local_completions(prefix: &str, kind: CompletionKind) -> Vec<String>;
    pub fn ssh_completions(ssh: &SshTarget, prefix: &str, kind: CompletionKind, executor: &impl CommandExecutor) -> Result<Vec<String>>;
    pub fn common_insert(prefix: &str, candidates: &[String]) -> Option<String>;

In `mj-controller/src/controller/path_completion.rs`:

    impl Controller {
        pub fn complete_path(&self, host: &CompletionHost, prefix: &str, kind: CompletionKind, executor: &impl CommandExecutor) -> Result<PathCompletion>;
    }

In `mj-chat/src/components/scope.rs`: `ControlKind::PathField { len, selected, expanded }`, `Interaction::{Complete(K), PathCommit(K, usize), PathDismiss(K)}`, `Form::register_popup`.

In `mj-tui/src/lib.rs`: `DashboardAction::CompletePath { host: CompletionHost, kind: CompletionKind, prefix: String }`. In `mj-cli/src/dashboard/io.rs`: `DashboardIoUpdate::PathCompletions { context: String, prefix: String, result: Result<PathCompletion, String> }`.

Web: `POST /api/paths/complete` with body `{ "target_id": string|null, "prefix": string, "kind": "directories"|"any" }` and reply `{ "candidates": [string], "insert": string|null, "truncated": bool }`.
