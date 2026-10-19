# Collapse mailbox events in conversation views

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Mailbox deliveries currently fill conversation history with system text and child reports. Each event should occupy one line by default in both the web and terminal conversation views. Clicking its right arrow opens its complete text and changes the arrow to point down. Opening one event must leave its neighbors closed, including when summaries repeat or a live update arrives.

## Progress

- [x] (2026-10-19) Locate canonical mailbox formatting and both renderers; confirm the user wants both surfaces and explicitly authorizes pushing.
- [x] (2026-10-19) Prototype browser disclosure styling.
- [x] (2026-10-19) Put mailbox interpretation in the shared client transcript layer and expose bounded structured events to the browser.
- [x] (2026-10-19) Render and toggle terminal event rows through the existing frame-local click and selection infrastructure.
- [x] (2026-10-19) Extend conversation golden cases for closed, opened, updated, and collapsed events on both surfaces; focused golden tests and 56 browser unit tests pass.
- [x] (2026-10-19) Run affected tests, full touched-crate suites, workspace Clippy, browser checks, and formatting. The existing cleanup timeout failure passed when rerun alone.
- [x] (2026-10-19) Review and stage the final change set for the required current-branch commit and authorized push to origin/master.

## Surprises & Discoveries

The durable transcript stores one system row per delivery, potentially containing several events. `mj-transcript/src/projection/observation.rs::delivered_external_events` uses `Mailbox events:` followed by two-space-indented event bullets and four-space-indented report bodies. Historical sessions can have accumulated events in one row. Browser stable IDs are currently reserved for incoming messages. Terminal tool click targets already follow scrolling and selection coordinates.

The terminal's ordinary Rich system body interprets Markdown, which hides literal HTML tags and changes report bullets. The copy check exposed this before completion. Expanded mailbox details now use the existing Raw body renderer, preserving the exact event text while retaining Rich disclosure headers. The copy check passes for Unicode, literal tags, and nested bullets. Mailbox toggles change rows only below the clicked header, so an existing viewport anchor remains valid without jumping to the delivery's first event.

## Decision Log

Use the existing canonical mailbox text as input to one parser in `mj-client/src/transcript.rs`. This supports old stored transcripts without changing durable formats. Expose event line arrays on `BrowserTranscriptEntry`, deriving them after browser line clipping so detail cannot bypass the browser transcript size limit. Terminal expansion remains client-local and keyed by entry position and event index, the append-only order within a delivery. Preserve Raw terminal rendering. Native browser details/summary controls provide click and keyboard support. Date/author: 2026-10-19, Codex.

## Context and Orientation

`mj-transcript` converts worker observations to stored transcript items. `mj-client/src/transcript.rs` converts those items to `ChatEntry` and browser entries. `mj-client/src/web.rs` defines browser response structures. `mj-controller/src/web/viewer.js` and `viewer.css` render those responses; the desktop shell uses the same viewer. `mj-chat/src/chat/transcript.rs` owns terminal row caching, click targets, scrolling, and selection, and its `render.rs` draws entries. `mj-chat/src/chat/pointer.rs` sends synthetic clicks to transcript toggles. Tests use checked-in rendered output, called golden files, to protect visible behavior.

## Plan of Work

First add the shared parser, with event lines retaining their full header and any report continuation. Add a browser event field and populate it only from the already bounded lines; compare it when deciding whether a browser entry changed and update existing response fixtures.

Next draw compact terminal mailbox rows in Rich mode, truncating each event header to the viewport width and using existing wrapping and copy mappings for expanded bodies. Record event-header positions from the same rendering as the cache. Extend transient click targets with the event index and keep event expansion in ChatState; invalidate rows after toggles and preserve a valid scroll anchor. Feed copy and link resolution the same cached mailbox tracking data. Do not change Raw rendering or process lifecycle behavior.

Finally make the browser consume the structured events, retain each open disclosure by event index during repaint, and show ellipsized single-line summaries plus full details. Extend the existing terminal rich-transcript golden and browser conversation golden to exercise multiline reports, nested bullets, Unicode, duplicate headers, live updates, narrow viewports, and independent click toggles. Review generated golden diffs before accepting them.

## Concrete Steps

Work at the repository root. Use ordinary Cargo commands with existing mbx caching; do not redirect target storage. Run every Cargo test outside the restricted sandbox. Run focused golden tests during implementation, then `cargo test -p brokk-mj-client -p brokk-mj-chat -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings` in the dev profile. Unit tests retain their isolated data configuration; any manual binary invocation must specify `--instance mailbox-fold-test`. Browser checks run from `tests/e2e/web`: `node --test *.unit.test.mjs` and `PLAYWRIGHT_BROWSERS_PATH=/home/jonathan/.cache/ms-playwright npx playwright test --project deterministic`, elevated where Chromium requires it. Use `MJ_UPDATE_GOLDEN=1` only while deliberately recording expected output, then rerun without it. Run `cargo fmt --all -- --check` and `git diff --check`.

## Validation and Acceptance

Golden output must show one arrow and one unwrapped line for each closed event; opening one event reveals its full text and leaves the others closed. A click after resizing or scrolling must address the visible event. Duplicate summaries must remain independent. Live updates must retain open state. Browser keyboard activation must work, and long event text must not cause horizontal page overflow. Existing ordinary system entries and Raw terminal text must remain readable. Required suites and Clippy must pass before committing.

## Idempotence and Recovery

No migration, installation, or live daemon mutation is needed. UI state is local and reversible. Rerun failed focused tests after fixing their cause. Stage only changed files and commit on the existing branch. Push to its configured upstream as authorized; if rejected due to upstream movement, fetch and merge upstream, resolve conflicts, validate affected changes, and push the merge commit without rebasing.

## Outcomes & Retrospective

Both conversation views now show one clipped header per mailbox event, with right/down arrows and independent click toggles. Shared parsing supports old and new delivery rows. Terminal copy mappings preserve literal report text, scrolling stays anchored, and browser open state survives updates. Raw terminal rendering remains complete.

Validation completed: 412 terminal tests, 41 client unit tests, one client process-probe integration test, and 2,007 controller tests passed in the full run; the remaining controller cleanup deadline test passed alone in 2.34 seconds. Fifteen existing Rust tests were ignored. Documentation tests passed (none defined). All 56 browser unit tests and 102 deterministic browser cases passed; three existing lab-dependent cases were skipped. Workspace Clippy, Rust formatting, staged-diff checks, and the macOS golden shortcut comparison passed. The cleanup deadline test needs no changes for this feature.

Revision: initial plan after the user confirmed both surfaces; the browser prototype will use the shared parser instead of retaining a second interpretation in JavaScript.

Revision: shared parsing, both renderers, and golden coverage are complete. Full browser and Rust validation are running; expanded report text deliberately uses literal rendering after the copy check exposed Markdown interpretation.

Revision: validation is complete, including the isolated retry of the unrelated cleanup timeout. The reviewed change set is ready for the final commit and requested push.
