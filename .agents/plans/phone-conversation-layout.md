# Give the phone conversation more room

This ExecPlan is a living document. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current while implementing it. Follow `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

On a phone, a person should be able to read more of the conversation while writing a prompt. The web viewer will remove its browser voice controls, put task counts and the Sub-agents shortcut on one compact status row, reveal Send and image attachment controls only when the composer is in use, and reduce idle composer and history-control height. Native terminal dictation remains available. A deterministic browser test at 390 by 844 CSS pixels will prove the controls, row placement, send tap, and increased transcript viewport; before and after screenshots will make the change visible.

## Progress

- [x] (2026-10-04 13:32Z) Read the phone layout investigation and confirmed the requested detached worktree is clean at `5f9644ca6ed4d9e8a4e41c9322de468ef53fff7c`.
- [x] (2026-10-04 13:32Z) Removed browser voice controls, capture code, assets, routes, server request wiring, tests, offline-cache entries, and web-viewer instructions. Preserved the shared dictation module and native auth/provider dependencies pending the parent's review of the now-unreferenced helper.
- [x] (2026-10-04 13:44Z) Passed all 71 JavaScript unit tests, `cargo fmt --all -- --check`, all 263 tests selected by `cargo test -p brokk-mj-controller server::`, and `cargo clippy -p brokk-mj-controller --all-targets -- -D warnings`.
- [x] (2026-10-04 13:44Z) Prepared the first voice-removal commit; its server test compile took 10m39s and its Clippy check took 5m46s using the normal configured Cargo build setup.
- [x] (2026-10-04 14:15Z) Implemented the phone status row, focus-aware composer actions and height, compact Earlier messages control, and phone header spacing. Desktop keeps the Sub-agents button in its original composer location.
- [x] (2026-10-04 14:15Z) Added four deterministic phone browser cases, saved idle/focused/expanded screenshots, and recorded feed measurements. All four new cases pass.
- [x] (2026-10-04 14:15Z) Re-ran 71 JavaScript unit tests, `cargo fmt --all -- --check`, 263 controller server tests, and controller Clippy; all pass.
- [x] (2026-10-04 14:15Z) Full deterministic Playwright run completed with 121 passed and two remaining phone layout failures outside this change's scope; details and logs are recorded below and in the handback.
- [x] (2026-10-04 14:34Z) Made the compact-cards fixture return successful client-state and draft responses; all 20 compact-cards tests now pass. Final deterministic run has 121 passed, three skipped, and the same two out-of-scope phone viewport failures.

## Surprises & Discoveries

- Observation: The unit test that checks Playwright project membership launches Playwright as a child process, so running the suite inside this sandbox fails with `EPERM` before its assertions run.
  Evidence: The test passed with all 71 tests when the same command ran outside the restricted sandbox.
- Observation: The controller package is named `brokk-mj-controller` in `mj-controller/Cargo.toml`, while its Rust library is named `mj_controller`.
  Evidence: `cargo test -p mj-controller server::` cannot resolve a package; use `cargo test -p brokk-mj-controller server::` instead.
- Observation: The server's `DictationRequest` queue was only consumed by the browser endpoint. Terminal dictation calls shared auth and provider code directly.
  Evidence: Repository search after removal found no production callers of `crate::dictation::execute`; `mj-chat/src/speech.rs` continues to use the shared client APIs.
- Observation: The browser voice block also contained the send-disabled-state helper. Removing it with the voice implementation caused an undefined-function browser error; the helper was restored without the voice-active check, and the new phone send-tap case now passes.
  Evidence: `phone composer hides actions until intent and keeps Send tappable after typing` passes in `phone-layout.spec.js`.
- Observation: The full browser project still has two phone layout failures outside this change's scope: a project chooser row ends at y=854.95 in an 844px viewport, and a long elicitation moves the compact composer bottom to y=615 in a 568px viewport. The phone conversation changes do not edit chooser or elicitation layout code; the question test was updated only to expect the new hidden idle Send control.
  Evidence: `tests/e2e/web/test-results/new-session-compact-source-950b1-pository-visible-on-a-phone-deterministic/error-context.md` and `tests/e2e/web/test-results/plan-mode-long-question-fo-cdb09--the-composer-off-the-phone-deterministic/error-context.md`.
- Observation: The desktop composer geometry test raced with its unmocked draft save: after 400ms, the 404 error paragraph changed the form height. The compact-cards fixture now provides empty client state and accepts draft saves; its full 20-test file passes.
  Evidence: `tests/e2e/web/compact-cards.spec.js` mocks both session client-state and draft routes.

## Decision Log

- Decision: Keep the work in two commits: remove browser voice first, then add phone layout changes.
  Rationale: This preserves the requested review boundary between a feature removal and layout behavior.
  Date/Author: 2026-10-04, Codex.
- Decision: Remove the browser route and its server request queue, but retain `mj-controller/src/dictation.rs` and its public module for now.
  Rationale: The module re-exports shared auth helpers and describes shared web/native dictation support. A repository search found no native use of its executor, but the user asked that shared items be checked before deletion; preserving it avoids an unapproved public API deletion while the parent reviews the finding.
  Date/Author: 2026-10-04, Codex.
- Decision: Use phone-only CSS and a JavaScript class that reflects actual focus and composer contents. Do not change the viewport metadata, visualViewport keyboard sizing, or elicitation code.
  Rationale: The task assigns those areas elsewhere and asks that desktop remain unchanged.
  Date/Author: 2026-10-04, Codex.
- Decision: Update the existing long-question browser assertion to account for Send being intentionally hidden while the prompt is empty and idle; retain its composer-in-viewport check.
  Rationale: This keeps the existing layout regression meaningful without changing question-card behavior or styles. Its remaining viewport failure is reported to the owner of that concurrent work.
  Date/Author: 2026-10-04, Codex.

## Outcomes & Retrospective

The browser voice surface and endpoint are removed in commit `b32d4116`. The phone layout and four focused interaction tests are complete in the next commit. At 390 by 844, the transcript scroll box grew from 438.09px to 609px idle and 577px focused; visible transcript space grew from about 378.09px to 595px idle and 576.5px focused. The prompt box is 42px idle and 76px focused. The expanded task list spans 370px and scrolls inside a 40dvh content region.

The full deterministic project reports 121 passed, three skipped, and two phone-layout failures described above. Unit tests, formatting, controller server tests, and Clippy pass. Native dictation and the shared controller dictation module remain untouched; the latter has no production executor caller after browser route removal and is retained pending parent review.

## Context and Orientation

`mj-controller/src/web/viewer.html` defines the conversation page. `viewer.js` renders queue, shell, background-task, prompt-settings, and sub-agent state; it also owns the editable prompt and image attachments. `viewer.css` defines the transcript's flex layout, the 76-pixel prompt editor, touch controls, and existing phone rules. The `<details id="conversation-side">` card currently follows the transcript, while the Sub-agents button is in the composer. The server embeds HTML, CSS, and JavaScript from these files through `mj-controller/src/server/assets.rs` and serves them through `mj-controller/src/server/routes.rs`; `server/handlers.rs` holds the authenticated endpoint implementations. `server_runtime/run.rs` owns request queues and background tasks. A “deterministic Playwright test” is a browser test that intercepts network requests with fixture data and does not require a live daemon.

The initial phone fixture used a 390 by 844 viewport with three transcript entries, one queued prompt, one user shell, one background task, and one child sub-agent. Before this change, the transcript scroll box measured 438.1 pixels, or 51.9% of the viewport. The Earlier messages button consumed 46 pixels plus its following 14-pixel line gap. The composer used a 76-pixel prompt box, a 46-pixel Sub-agents row, and a 46-pixel Send/Voice/Attach row. Opening task details reduced the transcript scroll box to 149.1 pixels.

## Plan of Work

The first commit removes only the web viewer's voice surface. Delete its HTML controls, JavaScript capture and upload state, CSS, worklet and worker files, service-worker cache entries, server asset handlers and route, controller dispatch channel, web documentation, and voice-specific browser/server tests. Bump the service-worker cache version because its shell asset list changed. Keep terminal dictation files and the shared client auth/provider code untouched. Keep `mj-controller/src/dictation.rs` until its public/shared role receives review.

The second commit changes only narrow screens, using a breakpoint no wider than 640 CSS pixels. Put the task summary and Sub-agents shortcut on one status row; show only non-zero task counts in its compact label; let expanded details use the full row width with a bounded scrolling body. Reflect composer focus in a class on the conversation so phones hide the status row and Model/Effort readout while the text box is focused. Show Send and Attach when the form has focus, text, or images, and preserve the toolbar through pointer/focus transfer so tapping Send cannot hide the click target. Keep Attach gated by `prompt_images_supported`. Compact the idle prompt text box to about 42 pixels, allow it to grow on focus or text, turn Earlier messages into a compact text control, and trim only non-interactive phone padding while keeping interactive targets at least 36 pixels. Add deterministic browser tests and save before/after screenshots in the assigned report directory.

## Concrete Steps

Run JavaScript checks from `tests/e2e/web`:

    npm ci
    node --test *.unit.test.mjs
    PLAYWRIGHT_BROWSERS_PATH="$HOME/.cache/ms-playwright" npx playwright test --project deterministic

If the JavaScript suite's nested process or Chromium launch is denied by the sandbox, rerun the same command outside it. Run Rust formatting from the repository root with `cargo fmt --all -- --check`. Run the controller tests and lint from the repository root, outside the restricted sandbox because the test suite uses loopback TCP and Unix sockets:

    cargo test -p brokk-mj-controller server::
    cargo clippy -p brokk-mj-controller --all-targets -- -D warnings

Do not set `CARGO_TARGET_DIR` or otherwise redirect Cargo's default target directory. Do not run the full Cargo test command concurrently with Playwright.

## Validation and Acceptance

The JavaScript unit command should report 71 or more passing tests and no failures. The controller server test filter should pass, including `browser_dictation_routes_are_not_exposed`, which expects the old JSON dictation route and both browser audio assets to return HTTP 404. Clippy should finish with warnings denied. The deterministic Playwright project should pass, including named phone tests proving voice controls are absent; Send and Attach are hidden before intent, appear on focus, survive a typed tap on Send, and hide again when the empty form blurs; the compact summary and Sub-agents button share a top edge; and the feed viewport grows in idle and focused states. The test viewport is exactly 390 by 844 CSS pixels with touch enabled. Record measured pixels and percentages in the handback. Save phone screenshots for the idle, focused, and expanded-details states.

## Idempotence and Recovery

The UI changes are static assets and reversible source edits. Repeat the isolated browser tests as needed; they intercept network requests and use no live session state. Rust tests use the existing isolated test setup. If a test fails, keep its output and correct the code before committing. Stage only files changed for the current checkpoint, commit directly on this detached worktree, and do not push or create a branch.

## Artifacts and Notes

The investigation and original baseline screenshots are at `/home/jonathan/Projects/mjolnir/.mj/agents/d569aabfacfc2e7b9c7818fc225c2aba/layout-investigation.md` and in that directory's `phone-*.png` files. The final report and copied before/after phone screenshots go in the report directory supplied with this task.

## Interfaces and Dependencies

The phone layout must continue to use the existing `#conversation-side`, `#subagents-button`, `#prompt-text`, `#send-button`, `#attach-image`, and `#prompt-settings` elements, unless the desktop button needs a visually hidden counterpart to preserve its current placement. JavaScript remains in the existing viewer module; do not add a workspace crate. Use Playwright 1.62.1 from `tests/e2e/web/package.json` for browser verification.
