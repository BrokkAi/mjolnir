# Restore web responsiveness and audit the live UI

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

The user requested a real Chromium walkthrough of the running web UI, with primary-workspace activity read-only and small tasks permitted in the existing test workspace. The first request cannot load: the live daemon accepts TCP on port 3765 but never completes TLS. Restore responsiveness before evaluating navigation, transcript reading, mobile layout, and test-session prompting. Fix demonstrated bugs and report UX recommendations separately from verified fixes.

## Progress

- [x] Located the live daemon and confirmed installed Playwright/Chromium launches.
- [x] Reproduced advertised-hostname and direct-loopback TLS timeouts repeatedly.
- [x] Captured a stack showing synchronous transcript rendering in the web control loop.
- [x] User authorized restarting the shared daemon after fixing and testing.
- [x] Move expensive transcript projection into supervised background work with bounded, latest-state scheduling; add behavior regressions.
- [x] Validate changes and restart the daemon using the supported command. Full tests, formatting, and Clippy passed on September 5; verify again after subsequent fixes.
- [x] Complete primary read-only conversation, new-session wizard, targets, and quota walkthroughs at desktop and phone sizes. User confirmed plandiag as the test workspace.
- [x] Complete two real-provider smoke tasks in plandiag: create/read a 16-byte file and independently verify its byte count and trailing newline.
- [x] Fix live-discovered dashboard title overflow and silent asynchronous launch failure; deploy and verify both fixes with authenticated Chromium.
- [x] Run final validation: full cargo test, cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, nine viewer unit tests, and four live Chromium layout tests passed.
- [x] Complete implementation and review; commit only task changes on the current branch and report results. No push is authorized by this request.

## Surprises & Discoveries

The daemon management endpoint responds while the web listener stalls. A focused debugger sample of PID 3027718, followed by immediate detach, shows `TranscriptSnapshot::from_materialized` beneath `mj-cli/src/server.rs:738` inside the control future selected alongside HTTP serving at line 1305. Repeated direct requests fail before the TLS server hello. This is evidence of serving starvation, not a failed login. The installed daemon predates the current checkout's worker fix; do not assume source edits are live until restart.

## Decision Log

Keep primary sessions unchanged: no prompts, stops, configuration changes, or file edits through the primary workspace. Viewing may ordinarily acknowledge read state, so intercept that mutation for primary conversations. The user separately authorized the shared daemon restart after diagnosis. Use the supported restart operation, whose help states detached workers remain running.

Fix the synchronous expensive projection rather than only spawning HTTP on another task. Bound projection concurrency, coalesce superseded updates, propagate failures, and prevent late results from reviving removed sessions. Existing `SessionManagerUpdates` already coalesces by session; preserve that property.

Report asynchronous launch failures independently of session records. Provisioning legitimately removes a provisional session after rollback, so retaining or reviving that record solely to display an error would invent invalid capabilities. Keep the most recent 16 workspace-scoped notices in the daemon's web state, publish them immediately after failed completion, and preserve them across controller reloads. The public notice carries only its identifier and workspace identifier; a fixed browser message points to private daemon logs. Dismissal is browser-local, and restarting the daemon clears this bounded history. Do not substitute an input-specific validation check for this general completion-reporting fix.

## Outcomes & Retrospective

The responsiveness fix is running in daemon PID 2984111, with its executable inode verified against the built binary. HTTPS returned 200 in 63 ms after previously timing out before the TLS handshake. The first restart raced an older attached client and started the old executable; retrying with the supported MJ_DEV_RESTART_STALE_DAEMON=1 setting installed the current build. Detached primary workers were not individually stopped or prompted.

Authenticated Chromium rendered primary conversations without JavaScript errors at 1440×1000 and 390×844. Test conversation, wizard, targets, and quota fit at phone widths with 44-pixel controls. A long primary session title caused document width 12901 at a 390-pixel viewport; a bounded title layout is being added.

The first two disposable launches exposed missing error reporting: a non-Git directory and then a repository with no HEAD passed preflight, were accepted, then disappeared after asynchronous provisioning failed. Logs confirmed rollback removed each provisional session. A third launch succeeded after a scratch-only initial commit. Session faf6222399b44c992710493c2e623a15 in plandiag ran codex3 on localhost and completed both requested smoke checks. Its worktree is /tmp/mj-live-web-JxL76v/.mj/worktrees/faf6222399b44c992710493c2e623a15; web-ui-smoke.txt contains WEB_UI_SMOKE_OK followed by a newline. Primary API writes were intercepted and blocked, including read receipts.

The test session was stopped through the UI after both tasks; its snapshot confirms lifecycle stopped. The normal stop flow creates a recovery copy and tears down its target, so the historical worktree path above is not a promise of a remaining live directory. Draft restoration after back/open and authenticated page reload passed. At 320×568, targets and quota pages had no document overflow or undersized controls; Escape closed the menu. The Chromium page-error list remained empty.

UX recommendations from the observed screens: distinguish an available but idle session from an actively running turn (the dashboard currently says running and idle together); make the empty queue/shell section more compact on small phones (it consumes substantial height above the composer); and use more desktop width or offer a split list/conversation view. Launch preflight should explain that raw projects require a Git repository with a valid HEAD before accepting a launch. These are recommendations, not claims of implemented changes.

The final build is serving as PID 3330791. CSS computed from the live page confirms the three-line session-title clamp: the previously overflowing 390-pixel dashboard now has scrollWidth 390 and title height 72. A deliberately invalid plandiag launch from /tmp/mj-live-web-failure-IVRDyg rolled back and left a visible generic error notice. Chromium verified the notice survives page reload, is absent on the primary dashboard, and can be dismissed. The public snapshot still contains no failed provisional session. All four layout.spec.js tests passed against this daemon, including 320×568, 390×844, and 900×900 route/accessibility checks. Nine standalone viewer tests passed. Final sequential full Cargo tests, Clippy with warnings denied, and formatting checks all passed. Chromium was closed after the audit; the patched daemon remains running. The scoped audit is complete, with the UX recommendations above left for follow-up rather than silently expanding this task.

## Context and Orientation

`mj-cli/src/server.rs` combines HTTP serving with a control loop that consumes session-manager views and publishes snapshots. Its worker-update branch currently constructs chat and browser transcripts synchronously. `mj-controller/src/hel_session_manager.rs` supplies a coalesced latest-state feed. `mj-chat/src/hel_chat/transcript.rs` creates browser transcript entries. `mj-controller/src/web/` owns HTML, CSS, and JavaScript; `tests/e2e/web/` contains Playwright tests and the installed browser driver. The existing reliability lab uses fake providers and is not a substitute for this requested live test.

## Plan of Work

First add a bounded background projection pipeline near its sole server call site, with completion handling in the control loop. Test that held projection work does not prevent unrelated requests or progress and that only the newest queued snapshot is projected next. Review the implementation before running the focused tests and full repository validation.

Then build the CLI and run `./target/debug/mj daemon restart`. Authenticate a fresh Chromium context using the code from `daemon status`, without printing or recording credentials. Inspect workspace names and current sessions before choosing the test workspace. Exercise the primary UI only with reads, and submit one or two explicitly harmless tasks only in an unambiguous test workspace. Capture screenshots without login credentials. If workspace identity is ambiguous, ask before submitting.

Finally separate reproduced defects, fixes, and UX recommendations. Add further scoped fixes only if warranted by live observations, with corresponding tests.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Use `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Build the CLI with `cargo build -p brokk-mjolnir`. All Cargo test commands must run outside the restricted sandbox. Do not change build storage. Use Playwright from `tests/e2e/web/node_modules/@playwright/test`; a private interactive Node driver is already available for live checks. Run the standalone viewer checks with `npm --prefix tests/e2e/web run test:unit`.

## Validation and Acceptance

A direct local HTTPS request and Chromium login must succeed while real sessions are streaming. Desktop and phone-width screens must remain navigable, test prompts must reach a real provider and settle visibly, and the primary workspace must receive no actions. Regression tests must prove background projection responsiveness and latest-state ordering. Record exactly which checks succeeded and any blockers rather than claiming a full walkthrough when only connectivity was tested.

## Idempotence and Recovery

Do not restart or stop workers individually. Browser contexts are disposable. Keep credentials out of committed notes, screenshots, and traces. Preserve existing changes and commit only task-owned files. If a live fix requires additional authority beyond the approved daemon restart and test-workspace tasks, ask before taking that action.

## Artifacts and Notes

Live evidence so far: Chromium navigation timed out after 30 seconds; `curl --noproxy '*' -k --connect-timeout 3 --max-time 5 https://127.0.0.1:3765/` returned no HTTP status. The focused daemon stack linked `sanitize_terminal_text`, `materialized_chat_entries_with_diffstats`, `TranscriptSnapshot::from_materialized`, and the server worker-update branch.

Before/after screenshots and walkthrough captures are local, uncommitted evidence in target/live-web-audit-20260905. Important files are mobile-dashboard.png (before), mobile-dashboard-fixed.png, desktop-dashboard-fixed.png, mobile-launch-failure-fixed.png, small-phone-test-streaming.png, and small-phone-test-complete.png. They may contain private session text and must not be published automatically.

## Interfaces and Dependencies

Reuse Tokio background task facilities and existing session/transcript types. No new crate or external package is required. Keep concurrency supervision and error reporting explicit. The browser uses the already-installed Chromium and Playwright dependencies.

## Follow-up: idle semantics and project validation

The user accepted permanent blue titles for truly idle sessions in both web and TUI, explicitly requiring the displayed [idle] label to agree with underlying activity and excluding background work. Use one classification in mj-chat/src/usage_format.rs for activity formatting and the idle predicate. Preserve a known active turn even when its clock timestamp is unavailable; an autonomous harness turn must not fall through to idle merely because the materialized start time has not arrived. Publish the resulting idle fact in ViewerSession rather than interpreting formatted text in JavaScript. The TUI must no longer require unread content for its idle blue summary/title.

For web project validation, the TUI already validates a raw directory and valid Git HEAD with Controller::validate_project_directory in supervised background work before advancing its wizard. Reuse that same helper through the web preflight request channel, retaining private diagnostics on the controller and returning an actionable safe validation message to the browser. Keep general asynchronous launch-failure reporting intact.

Desktop-width/split-pane work was filed as https://github.com/BrokkAi/mjolnir/issues/976. The user asked what compact Queue and shells meant; it was explained as reducing the empty collapsed panel above the phone composer. No change to that panel is authorized or included in this follow-up.

The follow-up implementation is integrated and serving in daemon PID 521303. A guarded Chromium check confirmed that all eight running session projections agree between is_idle and their displayed activity. It verified blue on a genuinely idle dashboard title and no blue on a background-working title. The invalid raw directory stayed on the Project step with an actionable valid-HEAD error; the valid scratch repository reached Review. No session was launched, no primary API writes were attempted, and no page errors occurred. All four live layout tests and eleven viewer unit tests passed. Full sequential Cargo tests, Clippy with warnings denied, and formatting checks passed. The follow-up is complete and committed on the current branch without pushing. The existing TUI process must be relaunched to use the updated rendering code; it was not terminated during this audit.

The shared activity projection now retains typed execution state and active user-shell facts. A present but invalid clock does not erase work; the UI shows Turn, Step, or BG without an invented elapsed time. Closing/closed states are not idle. Regression tests also cover the formerly incorrect autonomous-turn fallthrough and TUI read/unread idle colors. Before the final successful validation run, corrected regression fixtures included a miscomputed 1000-second interval (16m40s), an old minimized-grid yellow expectation, and a missing isolated-target definition.

Revision note: initial plan records the unexpected live serving blocker and approved restart before implementation. Confirmed the CLI package name and repaired obsolete standalone browser-test asset paths and cache-version assumptions; all eight standalone checks now pass.

Revision note (September 5, live audit): recorded successful deployment, desktop/mobile checks, real-provider test tasks, and two newly reproduced UI defects. All Cargo validations are now strictly sequential: the earlier mbx rejection was caused by overlapping validations in this checkout on NFS, not shared writes between separate checkout targets. No mbx change is part of this task.
