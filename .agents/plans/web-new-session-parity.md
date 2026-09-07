# Make web session creation reliable and match shared launch sources


This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


People should be able to create a bundle while starting a web session, choose recent raw-host project directories from the same history the terminal uses, and select options reliably while live session updates arrive. Project dividers should make the session list as easy to scan as the terminal. Chromium tests must exercise actual interaction, not just construction of controls.

## Progress


- [x] (2026-09-05) Inspected the web wizard, snapshot refresh path, terminal bundle helper, and directory history source.
- [x] (2026-09-05) Implement shared quick-bundle persistence, authenticated bundle endpoint, bounded background jobs, and shared host-key recent-directory projection.
- [x] (2026-09-05) Preserve wizard and Resume controls through unrelated revisions, use full-row choices, validate empty selections, and guard late launch/bundle results.
- [x] (2026-09-05) Make project dividers clear and validate grouping in Chromium and Firefox.
- [x] (2026-09-05) Run focused Chromium/Firefox checks, the full Cargo suite, Clippy, formatting, and isolated browser/TUI integration.
- [x] (2026-09-05) Rebuild/restart the live daemon and verify the deployed wizard read-only in Chromium and Firefox.
- [x] (2026-09-05) Complete final review and prepare the owned implementation and tests for commit on the current branch; no push requested.

## Surprises & Discoveries


Every successful web snapshot refresh calls `renderRoute`, which calls `renderNewForm` and replaces all wizard controls. This can interrupt native option selection and text focus even when no relevant wizard state changed. Question forms already preserve their keyed nodes, but the wizard does not. The existing project groups already use projected project keys and labels, but their headings have only small dim uppercase styling.

The user reports Firefox on Android. The old choices were native HTML dropdowns, which Android presents as popup radio choices. Full-row native radio/checkbox controls make both touch targets and checked state visible without that popup. Headless Firefox needed its actual icon fixture path fixed: the daemon serves `src/icons/icon.svg`, not `src/web/icon.svg`. Firefox fetched that icon while Chromium did not. Required multi-select questions also blocked a custom replacement answer; validation now follows the active answer, and optional radio groups can be cleared explicitly.

The full Rust suite exposed an old Resume test fixture that did not model retained card state. That fixture and its radio counterpart were updated. A subsequent full run hit the existing quota stale-boundary test's two separate wall-clock reads; the targeted CLI suite passed on rerun with no quota product changes. Clippy required grouping the server's typed request senders rather than adding an eighth positional constructor argument.

## Decision Log


Use existing terminal sources rather than browser storage for recents or a parallel bundle interpretation. Terminal bundle creation currently calls `create_quick_bundle` in `mj-cli/src/dashboard/io.rs`; terminal raw-host history calls `HelState::project_directories` with `local` or the configured SSH host. Expose those same decisions to the browser. Keep blocking filesystem/configuration work off the HTTP/control loops.

Preserve interactive nodes during unrelated snapshots instead of suppressing live updates globally. Keep all changes on the current branch, with no push unless requested. Primary workspace remains read-only during live audits; use controlled fixtures or the existing disposable plandiag workspace for writes.

The backend contract is authenticated `POST /api/bundles` accepting `{source}` and returning `{bundle_id}` after publishing the saved configuration to the browser snapshot. Each target projects `recent_project_directories` using shared `project_history_host` and `HelState::project_directories`. Bundle creation will share the terminal's source parser and transaction helper. Mount attachments and container resource editors are additional parity gaps discovered in the audit, but are not part of this scoped implementation; existing launch defaults remain unchanged.

## Outcomes & Retrospective


Implementation is complete. The isolated real-daemon browser/TUI run passed with two clients, one SSE reconnect, and zero leaked processes. Its normal browser scenario passed, including simultaneous bundle creation, equivalent-source reuse, and immediate snapshot visibility; its expanded matrix passed all 21 tests. Chromium and Firefox fixture suites passed, including in-progress gestures, raw-host recents, draft retention, optional/custom answers, duplicate submission protection, and late-response navigation guards. A final long-question case proved the larger choices needed an internally scrolling question panel; after the CSS fix, all six question-flow tests passed in both engines. The final full Cargo run and Clippy passed. The updated daemon is live, and both browsers verified 14 recent directories from the actual shared history, editable-path focus retention, and reachable bundle creation with zero page errors and zero API writes. Firefox checks use a phone-sized desktop-engine context, not a physical Android handset. Mount attachments and container sizing remain TUI-only editors.

## Context and Orientation


`mj-controller/src/web/viewer.js` implements the browser wizard, question forms, and grouped session cards. `viewer.css` styles their controls. `mj-controller/src/hel_server.rs` defines authenticated HTTP routes and the public snapshot. `mj-cli/src/server.rs` drives the shared daemon (the background process that owns web control requests). `mj-cli/src/dashboard/io.rs` performs terminal background configuration work. `src/hel_state.rs` owns persistent recent project directories. The terminal wizard in `mj-tui/src/wizards/dashboard.rs` selects which host's history to use.

## Plan of Work


First trace exact sources and background write/reload behavior before selecting the endpoint shape for bundle creation. Add recent project directories to the target projection using the terminal's host mapping. Provide a browser create-bundle path accepting the same local repository or GitHub source as the terminal, with immediate in-flight state and useful errors. Reuse the bundle-creation interpretation and persist through shared configuration machinery.

Next fix wizard refresh identity and stale async-result ownership, then audit empty configuration, target switches, preflight errors, dirty acknowledgement, Back/Cancel behavior, and repeated taps. Add Chromium tests that deliver snapshots while controls are focused and between pointer down/up. Improve project divider appearance without changing session action targets or adding hotkeys.

Finally run sequential Cargo validation outside the sandbox, browser tests with provider-free fixtures, and live read-only checks where useful. Review all changes and commit the validated result.

## Concrete Steps


Work from `/home/jonathan/Projects/hel`. Use `rg` to trace the source paths above. Run `npm --prefix tests/e2e/web run test:unit`, then run selected Playwright specs from `tests/e2e/web` with `MJ_BROWSER_SPEC` selecting the new tests. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` sequentially, with every Cargo test elevated. Never run simultaneous Cargo work in this target directory.

## Validation and Acceptance


A person can start a new-session wizard with no existing bundle, create or reuse a source-backed bundle, and continue with it selected. Raw-host recents are isolated to the same local or SSH host used by the terminal and remain editable. Snapshot updates do not close a picker, erase a draft, lose focus, or undo a choice. Duplicate taps cannot create duplicate bundle/launch operations. A late response cannot alter a different workspace or replacement wizard. Project headers separate the same projected groups as the terminal and contain no navigation hotkeys.

## Idempotence and Recovery


Use controlled browser API fixtures for creation and failures to avoid modifying primary configuration. Reuse existing-source bundle behavior and reject invalid sources before saving. Do not kill shared sessions or erase artifacts to resolve test problems. Cancel/leave must keep the browser responsive; any accepted background write that cannot be rolled back must be described accurately.

## Artifacts and Notes


Relevant initial evidence: `renderNewForm` ends with `newStep.replaceChildren(body)` and `refresh` calls `renderRoute` after each snapshot. The new gesture test sends a revision between pointer-down and pointer-up and proves that the original input remains connected and becomes checked.

Isolated integration artifacts are in `target/reliability-artifacts/browser-tui-convergence-seed-20260905-2859015`. `browser.log` reports one passing real-daemon workflow; `layout.log` reports 21 passing tests. The harness reports `passed clients=2 sse_reconnect=1 leaks=0`.

## Interfaces and Dependencies


Keep Rust work in existing crates, with shared helpers rather than a new crate. The browser consumes projected target recents and a typed authenticated bundle-creation response, not config files. The selected endpoint and result shape are recorded in the Decision Log. Reuse the installed Playwright/Chromium/Firefox harness and shared subprocess helpers.

Revision note: initial plan records the source audit, likely selection failure mechanism, scoped delegation, and required validation.

Revision note: implementation records bounded shared backend work, stale reload invalidation, cross-browser evidence, fixture repairs, and the isolated end-to-end result. Remaining parity gaps are explicitly bounded rather than silently adding mount and resource editors.

Revision note: final verification records the long-form scrolling fix, clean full Rust/Clippy results, and a read-only check of the deployed viewer in both browser engines. Product code is complete and ready for the required commit, without a push.
