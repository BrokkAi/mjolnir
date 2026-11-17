# Show profile sign-in in a modal over the live dashboard

This ExecPlan is a living document. It follows `.agents/PLANS.md` from the
repository root and must be kept up to date as implementation and validation
proceed.

## Purpose / Big Picture

When a person chooses **Sign in** for an agent profile, the dashboard should
remain visibly present while a bounded sign-in dialog appears over it. The
harness or bridge still runs in a real daemon-owned pseudo-terminal, but that
terminal is confined to the dialog rather than replacing the whole dashboard.
The visible result is a normal Mjolnir screen with its panes and Settings
context around the edges, a clearly bordered **Sign in** modal in the middle,
and the harness's browser URL, device code, prompts, and completion message
inside that modal.

## Progress

- [x] (2026-11-13) Trace the current full-screen login pane, daemon-owned PTY,
  event routing, status refresh, and existing golden test.
- [x] Render the dashboard underneath the login terminal and overlay a centered,
  bounded sign-in modal.
- [x] Derive the PTY size from the modal interior, including resize handling.
- [x] Extend the render golden to prove the dashboard remains visible around an
  opaque modal and that fragmented harness output is still preserved.
- [x] Run formatting, focused tests, full touched-crate tests, and workspace
  Clippy; review the final diff.
- [x] Commit only task-owned files on the current branch and push to
  `origin/master`.

## Surprises & Discoveries

- Observation: the current implementation already avoids suspending the
  enclosing application, but `mj-cli/src/dashboard/surface.rs` returns early
  and draws only the login terminal, so the visible result is a full-screen
  replacement rather than a modal.
  Evidence: `DashboardContext::draw` calls `login.render(frame)` before drawing
  the combined dashboard.

- Observation: the daemon-to-client login protocol and process ownership already
  satisfy the important background-work rules. The remaining gap is render
  composition and the size communicated to the daemon PTY.
  Evidence: `ActiveLogin::connection` runs off the event loop, keeps the daemon
  placement lease, and streams input and output with bounded channels.

- Observation: preserving the dashboard frame while sign-in is open means the
  normal dashboard render continues to run underneath the modal. The login
  overlay must therefore suppress selection extraction and drawn-conversation
  read receipts for that frame.
  Evidence: the modal covers the conversation pane center even though it leaves
  the surrounding dashboard visible.

- Observation: this agent session exports `MJ_SESSION_ID` and
  `MJ_SESSION_BASH_ENV`. Those markers make daemon fixtures rebuild the account
  login environment, replacing their temporary `HOME` and dropping
  `MJ_TEST_HOOK*`, so daemon integration tests must run with those variables
  unset.
  Evidence: the two failing daemon-startup tests pass immediately when those
  two variables are removed.

## Decision Log

- Decision: keep the daemon-owned PTY connection and render its `vt100` screen
  inside a centered TUI dialog.
  Rationale: harness login and preparation need a real interactive terminal,
  and the existing daemon ownership preserves remote placement and cancellation
  behavior.
  Date/Author: 2026-11-13 / Codex

- Decision: render the normal combined dashboard first and draw the login modal
  last in the same frame.
  Rationale: the person should see that Mjolnir remains active, while the
  modal's opaque background and border keep terminal input legible. Rendering
  in one frame avoids a separate handoff and makes resize behavior atomic.
  Date/Author: 2026-11-13 / Codex

- Decision: derive PTY columns and rows from the same shared modal geometry used
  for drawing.
  Rationale: one calculation must own the terminal dimensions so the child
  application cannot draw for a larger region than the modal exposes.
  Date/Author: 2026-11-13 / Codex

- Decision: continue routing every keyboard, paste, mouse, and resize event
  while the modal is open to the modal, with `Ctrl+]` cancelling and `Ctrl+Q`
  requesting a dashboard quit.
  Rationale: the dashboard is visible but not interactive during sign-in;
  keystrokes must reach the harness without triggering hidden dashboard
  controls.
  Date/Author: 2026-11-13 / Codex

## Outcomes & Retrospective

The TUI now keeps the combined dashboard on screen and overlays an opaque,
centered **Sign in** modal. The modal uses one geometry function for both
drawing and the daemon PTY dimensions, so resize handling keeps the harness
inside the visible dialog. The golden covers initial connection, successful
completion with fragmented ANSI output, and a failure status while showing the
dashboard around the modal.

Validation completed with all features: 586 TUI tests, 251 CLI unit tests, 22
daemon-startup integrations, and the remaining CLI integration suites pass.
`cargo fmt --all -- --check`, `git diff --check`, and workspace
`cargo clippy --all-targets -- -D warnings` pass. Implementation commit
`816afd679` was merged with upstream and pushed to `origin/master`.

## Context and Orientation

Mjolnir's terminal UI is the `mj` binary's dashboard. Its top-level event loop
and state live in `mj-cli/src/dashboard.rs`; drawing is implemented in
`mj-cli/src/dashboard/surface.rs`. The combined dashboard is rendered by
`mj-tui/src/combined.rs`. Reusable terminal widgets and shared modal geometry
live in `mj-tui/src` and `mj-chat/src/modal.rs`.

A profile is a saved coding-agent account configuration. Choosing **Sign in**
from Settings asks the daemon to start a disposable login process. The client
opens a dedicated connection and sends terminal input and output over it. The
daemon process, its PTY ownership, and cleanup are in
`mj-controller/src/login_terminal.rs`; the dashboard-side connection is in
`mj-cli/src/dashboard/login_terminal.rs`; and terminal emulation and rendering
are in `mj-tui/src/login_terminal.rs`.

The dashboard's existing modal helpers clear an opaque rectangle and preserve
two screen cells around it. The login dialog should use the same visual
language while remaining larger than ordinary confirmation dialogs because it
hosts the harness's own full-screen interface.

## Plan of Work

First, change `mj-tui/src/login_terminal.rs` so `LoginPane` owns one function
that calculates the centered dialog rectangle from the frame area. Use that
function both for drawing and for calculating the PTY's inner rows and columns.
Draw a clear, opaque, bordered dialog with a title naming the profile, the
emulated terminal body, a wrapped status area, and one control hint row. Keep
ANSI parsing, bracketed paste, keyboard encoding, callbacks, and terminal resize
behavior unchanged.

Next, change `mj-cli/src/dashboard/surface.rs` so the ordinary combined
dashboard is rendered first even while `ActiveLogin` exists, then overlay the
login dialog in the same draw closure. Keep all input routed exclusively to
`ActiveLogin` while it exists. Preserve the existing behavior that visible chat
read receipts are not advanced by a frame covered by sign-in.

Finally, update the login terminal golden test to render representative
dashboard text behind the modal. Verify that background cells remain visible
around the dialog, the dialog interior is opaque, fragmented ANSI output is
preserved, completion and failure statuses render, and the PTY size matches the
dialog interior.

## Concrete Steps

Work from the repository root:

    rg -n "fn draw|LoginPane::terminal_size|login\\.render" \
      mj-cli/src/dashboard/surface.rs mj-cli/src/dashboard/login_terminal.rs \
      mj-tui/src/login_terminal.rs

After editing, run formatting and the focused tests:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-tui login_terminal
    cargo test -p brokk-mjolnir dashboard

Then run validation for every touched crate and the required workspace Clippy:

    cargo test -p brokk-mj-tui -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings

If the golden output changes for the intended geometry, inspect it before
accepting and record the updated count and behavior here.

## Validation and Acceptance

Acceptance requires all of the following:

- With a 76-column by 12-row test terminal, dashboard text is visible outside
  the sign-in dialog; the dialog keeps a centered border and does not occupy
  the full screen.
- The login parser displays the URL, device code, saved-credentials message,
  and later completion or failure text within the dialog.
- Bracketed paste and keyboard encoding still send the correct bytes.
- Resizing recomputes the same dialog interior dimensions sent to the daemon
  PTY.
- `Ctrl+]` cancels and returns to Settings; `Ctrl+Q` requests dashboard quit;
  dashboard keys outside those chords cannot activate hidden controls.
- A successful exit refreshes profile login statuses and reports success. A
  daemon disconnect or non-zero exit reports its actionable failure.
- Formatting, focused tests, full tests for touched crates, and workspace Clippy
  pass.

## Idempotence and Recovery

The change has no database migration and does not modify credentials. Rendering
and resize calculations are deterministic. Re-running tests and builds is
safe. If a harness fails during sign-in, the disposable daemon PTY is terminated
as it is today, and the user can retry **Sign in** from Settings.

## Artifacts and Notes

The pre-change golden at `mj-tui/tests/golden/login-terminal.txt` shows the
login border occupying the entire 76-by-12 frame. The completed golden must
instead show the same terminal transcript inside a centered modal with dashboard
text visible around it.

## Interfaces and Dependencies

In `mj-tui/src/login_terminal.rs`, `LoginPane` must expose one geometry function
that returns the centered modal `ratatui::layout::Rect` for a screen area.
`LoginPane::terminal_size(width, height)` remains the CLI-facing sizing function
and returns the content dimensions inside that modal after removing borders,
status rows, and controls.

In `mj-cli/src/dashboard/surface.rs`, `DashboardContext::draw` must call
`mj_tui::combined::render_combined` for the dashboard and then call
`LoginPane::render` with the same frame when `self.login` is present.

No wire protocol, database schema, configuration format, or daemon lifecycle
change is required. Existing dependencies `ratatui`, `vt100`, and `tui-term`
remain sufficient.

Revision: 2026-11-13 initial plan recorded from the requested in-TUI sign-in
dialog behavior.
