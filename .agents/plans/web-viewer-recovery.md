# Recover web viewer startup without leaving the dashboard

This plan is maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

When the viewer port is occupied, users should see which address failed and recover from the same dialog. They can retry, let the operating system reserve another port, or inspect the listener and explicitly confirm stopping an identified Mjolnir server. The active daemon and its sessions must survive recovery. Ready must mean a listener has actually been bound.

## Progress

- [x] (2026-09-07) Traced the error from HTTPS binding through daemon status to the close-only dialog.
- [x] (2026-09-07) Added supervised listener recovery and protocol 11 viewer actions.
- [x] (2026-09-07) Added listener inspection, identity checks, and Linux process-handle termination.
- [x] (2026-09-07) Added recovery buttons, live startup polling, listener details, and stop confirmation.
- [x] (2026-09-07) Focused recovery tests, full Cargo suite, and Clippy passed initially.
- [x] (2026-09-07) Real-daemon HTTPS inspection, confirmed stop-and-retry, and alternate-port recovery passed; all temporary daemons exited before their files were removed.
- [x] (2026-09-07) Final `cargo test --quiet`, Clippy, formatting, Python syntax, and diff checks passed. Prepared the coherent implementation for the required current-branch commit.

## Surprises & Discoveries

The original daemon announced Ready before binding. A bind failure ended the entire phone control task, so changing its message alone could not provide recovery. The management protocol freezes `WebViewerStatus`; richer information uses new versioned actions rather than changing that wire shape.

The real-listener test caught duplicate PIDs in sysinfo refresh input, which caused its dead-process pruning to remove the current process. Deduplicating the input fixed it. The isolated HTTPS smoke test also caught missing user metadata: the default sysinfo refresh omits ownership and command arguments. Inspection now explicitly requests user, command, and executable metadata.

## Decision Log

Use an operating-system-assigned port for the alternative-port action and retain the bound socket when serving. This avoids probing a free port and losing it before startup. Alternative ports apply to the running daemon; the dialog must say so. Keep HTTPS and its hostname when changing ports.

Keep the existing controller and remote session bridge alive while retrying the listener. Inspect and stop processes on background tasks. Require explicit confirmation, same-user Mjolnir identity, and fresh listener ownership verification before a graceful termination; never escalate automatically to a force kill.

Linux stopping uses process handles that cannot be redirected by PID reuse. Other platforms show listener inspection but explain that stopping must be done in the application; alternative-port recovery remains available. HTTP and HTTPS share the same server ownership and two-second graceful-shutdown bound.

## Outcomes & Retrospective

The functional recovery and dialog tests pass, along with the final full suite and Clippy run. The retained HTTPS acceptance test proves that an occupied port reports Failed, inspection identifies the competing same-user daemon, confirmation stops that daemon and recovers the original port, and selecting another port preserves the existing server while serving HTTPS at the new URL. The earlier one-line diagnostic change is superseded by a recoverable listener lifecycle.

Recovery is temporary until daemon restart, as stated in the dialog and documentation. In-app stopping is Linux-only; other platforms can inspect and select another port. Unrelated applications and the current daemon cannot be stopped through this UI. No new dependency or workspace crate was introduced.

## Context and Orientation

`mj-controller/src/hel_server.rs` owns the HTTP router, TLS serving, and shared access/recovery types. `mj-cli/src/server.rs` assembles viewer state and runs the supervised listener alongside its controller. `mj-cli/src/web_viewer.rs` reserves sockets, handles retry commands, and inspects listener ownership. `mj-cli/src/daemon.rs` owns the background service and authenticated local protocol. `mj-cli/src/dashboard/actions.rs` performs cancellable background dashboard requests. `mj-tui/src/dialogs.rs` renders the modal and handles keyboard and pointer events; `mj-tui/src/lib.rs` defines its actions and re-exports shared responses.

## Plan of Work

First expose serving an already-bound listener in the controller. Add `mj-cli/src/web_viewer.rs` for service status, serialized recovery requests, the listener supervision loop, and process inspection. Connect it to daemon state without changing the frozen management subset. Increment the protocol version and extend its compatibility fixture.

Then extend the TUI web dialog with retry, another-port, and inspection controls. Show inspected processes and require a separate confirmation before stopping an eligible process. Keep errors and pending work visible. Fetch startup status in a cancellable background polling task so a Starting dialog becomes Ready without reopening it. Keep modal dimensions bounded and buttons usable on smaller terminals.

## Milestones

The first milestone is the recoverable listener and versioned daemon commands. Its proof is real-socket tests showing Failed while the original port is occupied and Ready with successful HTTP after retry or another-port selection. The second milestone is the dialog: normal keyboard and pointer input dispatches these actions, busy state appears immediately, and stopping requires a separate confirmation with Cancel selected. The final milestone is real-process HTTPS acceptance and complete workspace validation, with evidence recorded below.

## Concrete Steps

Work in `/home/ryan/code/mjolnir`. Inspect shared subprocess and form helpers before adding behavior. Use module-level tests for real socket conflicts, retry and alternate-port recovery, URL rewriting, process identity guards, and dialog event transitions. Run `cargo fmt --all -- --check`, `cargo test` outside the sandbox, and `cargo clippy --all-targets -- -D warnings`. Review `git diff --check`, stage only changed files, and commit on the current branch without pushing.

For the retained Linux acceptance test, run `cargo build -p brokk-mjolnir`, then `python3 tests/e2e/web_viewer_recovery.py --mj target/debug/mj` outside the networking sandbox. It requires Python and OpenSSL, creates private test configuration under `target`, uses the existing shared authenticated daemon-request helper, and terminates only the daemons it created. It removes their files only after process handles report exit. The three PASS lines cover ownership inspection, confirmed stop-and-retry, and alternate-port HTTPS.

## Validation and Acceptance

Occupying a loopback port must produce Failed with the exact address and no Ready announcement. Another-port recovery must reserve a different port, publish its actual URL, and serve HTTP. Releasing the original port and retrying must succeed. Cancelling during failure or startup must finish promptly. Inspection must identify a real test listener; unsafe or stale termination requests must be rejected. Dialog tests must activate recovery through normal input, preserve Close while busy, and keep the address and actions visible at ordinary terminal sizes.

## Idempotence and Recovery

Recovery is serialized by the listener owner. Requests against a healthy or already-recovering viewer must not interrupt it. Retrying does not change stored configuration. Socket handles are dropped on failure or cancellation. Process termination is explicit, graceful, and bounded; failure remains visible with another-port recovery available.

## Artifacts and Notes

Original failure: `run Mjolnir HTTPS phone server: Address already in use (os error 98)`. The first committed fix added the bind address, but left only Close and offered no recovery.

Final acceptance evidence:

    PASS: occupied HTTPS port reports Failed and inspection identifies the owning Mjolnir daemon
    PASS: StopAndRetry stops only the confirmed daemon and serves HTTPS on the original port
    PASS: AnotherPort serves HTTPS at the new URL while the existing server remains available

The final full Rust suite and Clippy both exited successfully. A test assertion that a released ephemeral port must remain free was removed: that global availability can race concurrent tests. The bounded server-exit assertion remains, and real-daemon recovery verifies rebinding through the advertised interface.

## Interfaces and Dependencies

Use existing Tokio listeners, channels, cancellation tokens, Axum server support, sysinfo process metadata, and shared subprocess helpers. No new workspace crate is needed. The frozen Ping/Status/Stop protocol remains compatible with older versions; only new viewer operations require the incremented protocol version.

Revision note: initial plan records the user's expanded requirement for functional port and existing-server recovery.

Revision note: recorded implementation milestones, initial checks, real-process inspection discoveries, and platform boundaries.

Revision note: completed final validation and retained the isolated HTTPS regression harness; documented outcomes, limitations, commands, and acceptance evidence.
