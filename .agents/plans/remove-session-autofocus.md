# Keep Create and Move from changing conversation focus

This ExecPlan follows `.agents/PLANS.md` and is maintained as work proceeds.

## Purpose / Big Picture

Starting or completing Create and Move must leave the current conversation, pane, and keyboard focus unchanged. A new session appears in the list; the user explicitly selects it before typing. A moved session already displayed in a pane reconnects there without revealing or focusing that pane. Existing per-session drafts and queued prompts survive the operation. Explicit Open/Resume and startup restoration retain their existing behavior.

## Progress

- [x] (2026-11-03) Trace launch registration/completion, pre-registration composer, lifecycle start, and pane attachment paths. Confirm with the user that Create also stays on the current conversation immediately after its wizard closes.
- [x] (2026-11-03) Remove creation-specific automatic selection, its temporary composer and render/input branches, and Move focus changes at start and completion. Lifecycle completions now leave attachment reconciliation to its existing owner.
- [x] (2026-11-03) Adapt lifecycle ordering and registered-composer clipboard regressions, remove tests for the deleted feature, update docs, and add a real tmux Create navigation case. Focused clipboard checks pass; full validation is running.
- [x] (2026-11-03) Full touched-crate suites passed on the final source: TUI 560 passed/2 ignored; CLI 231 unit tests plus its ACP, daemon-startup, instance, logging, store-divergence and 14 PTY tests passed. Workspace Clippy, formatting, Python syntax and diff checks passed.
- [x] (2026-11-03) Real tmux navigation regression passed in the isolated `no-session-autofocus` instance with deterministic ACP and copied dev binaries. Source review is complete; delivery uses the required current-branch commit and origin/master push.

## Surprises & Discoveries

Create immediately clears Browse and focuses a global launch composer before the daemon assigns a session ID. Removing only completion focus would leave this earlier focus change in place. Move completion calls `open_chat_session`, which reveals and focuses a pane even if that session was in an unfocused pin; reconnection must instead use the existing pane-specific attachment method. These are navigation changes, separate from the worker connection needed to keep an open moved conversation usable. Real-terminal validation also exposed that Enter on a Starting row was blocked by the general operation guard: autofocus had hidden the lack of an explicit route into its standby composer. Open now asks the existing standby-composer owner, which excludes failed sessions even when an attachment is still in flight.

## Decision Log

- Decision: Delete the global pre-registration launch composer and its input/render branches; keep ordinary per-session standby composers.
  Rationale: The user explicitly chose to stay in the existing conversation after Create. A hidden global composer must not capture text meant for that conversation. Once registered, a new session can be selected and already has the regular standby composer.
  Date/Author: 2026-11-03 / user and Codex.
- Decision: Move reconnection runs through the existing attachment reconciler, which follows only current pane assignments and never calls the navigation-opening entry point.
  Rationale: Reconnection refreshes a conversation already selected by the user; it must not choose a new conversation or focus a different pane.
  Date/Author: 2026-11-03 / Codex.

- Decision: Explicit Open focuses an existing selected standby composer during launch, resume, Move or attachment, while failed sessions retain recovery controls.
  Rationale: Removing autofocus must still leave a direct user action for typing during launch; the shared standby predicate owns whether such an input surface exists.
  Date/Author: 2026-11-03 / Codex.

## Outcomes & Retrospective

Implemented and validated. Create and Move leave navigation and keyboard focus alone; the anonymous pre-registration composer is deleted. Ordinary session-owned drafts, queues and pane reconnection remain, and explicit Enter opens a selected transitioning session without starting duplicate work. The host default instance and its live configuration/workers were untouched; the tmux test used private data and completed its cleanup.

## Context and Orientation

`mj-cli/src/dashboard/actions.rs` admits background creation and lifecycle work. `mj-cli/src/dashboard/io.rs` applies registration and lifecycle completions. `mj-cli/src/dashboard/drafts.rs` has two attachment entry points: `open_chat_session` reveals/focuses a pane, while `open_chat_session_into` attaches within the specified pane. `mj-tui/src/dashboard_standby.rs` holds both ordinary session-owned standby composers and the global pre-registration launch composer. The latter also has special branches in `combined.rs`, `dashboard_input.rs`, `lib.rs`, and clipboard/render tests. `mj-tui/src/dashboard_workspaces.rs::finish_new_session` is the completion-driven focus helper to remove.

## Plan of Work

Remove the global launch-composer field, methods, rendering, input ownership and clipboard cases, along with its CLI creation hook and registration handoff. Keep per-session startup prompt admission and delivery unchanged. Delete `finish_new_session` and its caller. Lifecycle admission retains composer parking for the operated session but only explicit Resume navigates to it. Completion retires the old attachment and seeds transcripts only for already-displayed sessions. The existing attachment reconciler reconnects current pane assignments, so lifecycle completion has no separate opening path that can change navigation.

Extend existing tests for asynchronous state changes, pane assignments, drafts and clipboard identity instead of creating test-only navigation helpers. Replace removed-feature tests with coverage of Create/Move preserving focus and registered-session input. Update the New session section of `docs/src/content/docs/terminal-surface.mdx` to describe explicit selection and retained background launch progress.

## Concrete Steps

Work from the repository root and keep the existing mbx build layout. Run focused affected tests during editing. Every Cargo test must run outside the sandbox and preserve existing temporary config/data isolation. At the final source state run `cargo test -p brokk-mj-tui -p brokk-mjolnir` and `cargo clippy --all-targets -- -D warnings` in the dev profile, then `cargo fmt --all -- --check` and `git diff --check`. Any manual binary/daemon test must use `--instance no-session-autofocus`. Check no install is running before committing. Stage only this task's changes, commit on the current branch, push `HEAD:master`, and merge upstream if it has advanced without rebasing.

## Validation and Acceptance

After Create closes, the same pane and conversation retain focus and draft text while registration and launch finish. Selecting the new row allows its normal standby input and later live composer to share that session's draft. Moving another session in an unfocused pin must preserve both that pin's assignment and the focused pane, including if the user changes focus again before completion. A late completion cannot reselect a conversation that disappeared temporarily from a state refresh. Clipboard replies still target the composer instance that requested them. Existing startup prompt delivery and Move draft tests remain green.

## Idempotence and Recovery

This removes client-side automatic navigation; there are no migrations or live configuration edits. Keep background tasks, notices, cancellation and per-session startup delivery intact. Test runs use isolated directories and may be repeated. Failed validation requires fixing and rerunning only the affected tests before the final checks.

## Artifacts and Notes

Focused clipboard routing tests passed after adapting the image-paste case to a Tokio runtime, because registered composers start asynchronous image preparation. No await is used there, so no attachment installation runs. The final `--navigation-only` tmux run checks an initially empty view, explicit opening during launch, and an already-open draft across a second creation, including continued typing. It passed with seed 20261103; evidence is `/mnt/optane/tmp/mj-no-session-autofocus/artifacts/tui-components-seed-20261103-190023/live-evidence.json`. The driver uses `MJ_INSTANCE=no-session-autofocus`, private configuration/data, and explicit host quota probing so its fake harness never provisions a container. Test-owned storage from an earlier attempt was unmounted and removed after its processes stopped. Full TUI/CLI suites and workspace Clippy pass.

## Interfaces and Dependencies

Reuse `ConversationNavigation`, pane assignments, `SessionAttachment`, and session-owned `ChatState` composers. Do not add a second focus owner, a configuration switch, new crates, or new dependencies. Session creation still belongs to the daemon; the UI only changes how its own selection responds.

2026-11-03: Completed removal and final validation, including explicit Open during an active transition and preservation of failed-session recovery when an old attach is still pending. No configuration or persisted handoff format changed.
