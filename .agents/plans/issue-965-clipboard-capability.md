# Gate clipboard images on the active agent's advertised capability

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout the work. The user approved this plan on 2026-09-23, asking for a local capability POC first. Implementation proceeds for #965 only.

## Purpose / Big Picture


Issue #965 asks for clipboard image paste only when the active agent advertises image prompt support through ACP, the protocol connecting Mjolnir to its agent harness. Image capture, normalization, editable image markers, and prompt serialization already exist. Finish the capability gate so an unsupported or not-yet-initialized agent receives ordinary text but never an image from the terminal composer. Keep useful drafts intact when capabilities change during asynchronous work.

After this change, Ctrl+V, Cmd+V where the terminal forwards it, and an empty bracketed-paste event all use the same policy. An image-capable agent can receive clipboard images; another agent can receive clipboard text and gets an actionable notice for an image-only paste. Dictation remains a separate host action (`prefix+m`).

## Progress


- [x] (2026-09-23) Read the issue, recent triage, repository instructions, and current clipboard, attachment, capability, and submission paths.
- [x] (2026-09-23) Claimed only #965 for this planning/implementation cycle.
- [x] (2026-09-23) Applied separate requested triage: #1031 now has `impact:small`; #932 and #976 are closed with `wontfix` and reason `not planned`.
- [x] (2026-09-23) Prepared this single-issue plan for user review.
- [x] (2026-09-23) Received approval; first probed local Claude ACP 0.81.0 and Codex ACP 1.11.5 initialization (no session creation or prompts). Both advertised `promptCapabilities.image: true`; all four existing browser image-prompt tests passed.
- [x] (2026-09-23) Shared the browser predicate and applied it to initial and refreshed terminal state.
- [x] (2026-09-23) Gated asynchronous image admission, `/attach`, saved draft submission, and the remote supervisor; clipboard results carry their original input target.
- [x] (2026-09-23) Added behavior coverage and platform documentation; all 581 TUI tests pass (one existing ignored capture test).
- [x] (2026-09-23) Full dev-profile workspace tests, final chat tests (581 passing), Astro docs check, formatting, and diff review pass.
- [x] (2026-09-23) All-targets clippy passed.
- [x] (2026-09-23) Committed implementation as `e0e9dd83`; the push encountered concurrently published Claude turn changes and merged them cleanly as `3391a719`.
- [x] (2026-09-23) Merged-tree checks passed: 581 chat tests, 448 core tests, 33 Claude worker tests, four browser image tests, formatting, and all-targets clippy.
- [x] (2026-09-23) Pushed implementation `e0e9dd83` through merge `3391a719` to origin/master; #965 is closed and `agent-in-progress` removed.
- [x] (2026-09-23) Prepared a separate #1018 review plan after #965 was published. Its implementation awaits approval; #1063, #1073, and #1083 remain later in the queue.

## Surprises & Discoveries


The local POC sent only ACP `initialize` to the pinned installed Claude ACP 0.81.0 and Codex ACP 1.11.5 executables, then terminated their process groups. Both returned protocol 1 with `agentCapabilities.promptCapabilities.image: true`. No sessions or model prompts were created. The existing browser `image_prompt` test filter ran four tests successfully. Physical desktop clipboard access was not exercised; this host has no isolated Xvfb/xclip setup, and a capability handshake does not prove native clipboard integration on every platform.

Current composer tests show that Alt+V is no longer bound to dictation; the host owns `prefix+m`. Corrected the plan and documentation without changing voice bindings.

An attachment admitted before capability loss may finish normalization into its existing marker. It stays in the draft and cannot be sent until support returns. This preserves the user's image without leaving an unresolvable pending marker or deleting shared immutable blobs.

The ticket's original description predates substantial implementation. `mj-chat/src/clipboard.rs` already supports native clipboard reads and a Windows clipboard helper under WSL, normalizes images, and deliberately prefers an image when both image and text representations exist. `mj-chat/src/chat/attachments.rs` already installs optimized image bytes into the existing attachment store. Reuse these paths.

Mixed-format regression tests now run through native format selection and real image optimization using a hand-written clipboard fake. The first test incorrectly assumed the existing optimizer always returned PNG; it actually chooses JPEG for opaque pixels. The test now verifies that image content is selected and encoded without imposing a codec change.

The browser's capability policy already exists as `agent_accepts_prompt_images` in `mj-controller/src/server_runtime/support.rs`: it checks the actual initialized agent's `prompt_capabilities.image`, defaulting to false when capabilities are absent. The terminal does not currently carry this fact. Its `clipboard_is_text_only` checks only question dialogs and history search.

The production clipboard completion path is `ChatIoUpdate::Clipboard` in `mj-chat/src/chat/active/io.rs`. Image results go directly to `queue_attachment`, bypassing `ChatState::handle_clipboard_content`. Gating only that latter function would leave the actual asynchronous route open.

Draft-generation checks already reject clipboard results after input edits. Capability changes and changes of the active input surface need explicit coverage too. Existing attachment sequence numbers and marker ownership should continue to determine whether an attachment result belongs to the current draft.

## Decision Log


Decision (2026-09-23, approved): preserve existing image-first selection when the active agent supports images. Ordinary terminal text paste remains text; a text-only agent or text-only input field reads only the clipboard's text representation. This preserves current deliberate image-paste behavior without making a mixed-format clipboard unusable for agents that accept only text.

Decision (2026-09-23): missing capabilities mean image paste is unavailable. Use the actual advertised capability, not a harness name, model name, or assumption based on prior sessions.

Decision (2026-09-23): relocate the existing browser capability predicate into the shared relay state and call it from both surfaces. Do not add a parallel interpretation or a new crate.

Decision (2026-09-23): preserve existing image-bearing drafts if the agent becomes unsupported or its capabilities become unknown. Block submission with a clear notice until the user removes the images or image support returns. Never silently send only the text portion of such a draft.

Decision (2026-09-23): complete and publish this issue before planning #1018, as explicitly requested. Continue to honor the prior instruction to push each completed implementation to origin/master on the current branch.

## Context and Orientation


The worker runs the agent and publishes initialized capabilities in `RelayOperationalState.agent_capabilities` in `mj-core/src/relay/snapshot.rs`. The daemon, Mjolnir's controller process, turns those capabilities into the browser's `prompt_images_supported` flag via `mj-controller/src/server_runtime/support.rs` and `run.rs`.

The terminal owns a `ChatState` in `mj-chat/src/chat.rs`. `ActiveChat::open` and `apply_session_view` in `mj-chat/src/chat/active.rs` initialize and refresh its facts from the session snapshot. `mj-chat/src/chat/keys.rs` maps clipboard shortcuts; `input.rs` maps an empty bracketed-paste event to the same action. `active/dispatch.rs` starts a background clipboard read. `active/io.rs` receives its result, starts background attachment normalization, and applies attachment completion.

`mj-chat/src/chat/input_state.rs` owns `clipboard_is_text_only`, `handle_clipboard_content`, `reserve_attachment`, `finish_attachment`, and `submit_input`. These are the points where content becomes part of a draft or a submitted prompt. `mj-chat/src/chat/remote.rs` later normalizes images before sending a prompt. Review these submission paths together so restored drafts, retry, and follow-up/steering cannot bypass the UI gate.

Relevant existing tests are in `mj-chat/src/chat/tests.rs`, `mj-chat/src/chat/active/tests.rs`, module-level tests in `mj-chat/src/clipboard.rs`, and `mj-controller/src/server_runtime/tests.rs`. Human-facing clipboard instructions belong in `docs/src/content/docs/terminal-surface.mdx`.

## Plan of Work


### Milestone 1: One capability policy reaches both surfaces


Move the boolean interpretation from `agent_accepts_prompt_images` into a public `RelayOperationalState::accepts_prompt_images(&self) -> bool` method in `mj-core/src/relay/snapshot.rs`. Adapt the daemon/browser caller and its existing tests. Add a false-by-default `prompt_images_supported` fact to `ChatState`, initialize it in `ActiveChat::open`, and refresh it in `apply_session_view`. Keep materialized-only and standby views conservative until actual agent capabilities arrive.

Acceptance is observable with synthetic session snapshots: absent or false image capability disables image admission; a true advertisement enables it; a later capability change updates the same open chat. The web continues to derive its flag from the identical predicate.

### Milestone 2: Enforce paste and submission behavior without losing drafts


Use the existing `read` and `read_text` clipboard functions according to the capability and the current input surface. Keep reads and image processing off the event loop. An unsupported agent with clipboard text gets that text normally; an image-only clipboard produces a helpful notice instead of inserting a marker. Preserve detailed errors for genuine clipboard/provider failures rather than presenting every failure as a capability problem.

Check eligibility again when `ChatIoUpdate::Clipboard` arrives and before reserving an image attachment. A read started in one draft or input surface must not insert an image into a question dialog, history search, history reader, or a replacement session after its context changes. Use existing draft generations, attachment sequences, and session ownership; extend the request context only where these facts are insufficient.

Guard `submit_input` and the corresponding dispatch/retry path for image-bearing drafts against unsupported or unknown capabilities. Preserve the full draft and display the reason for refusal. Text-only prompts remain usable. Apply the same admission rule to existing image `/attach` handling where it shares the attachment path, so it cannot bypass the paste gate; do not expand scope into arbitrary file uploads or #935.

Keep existing attachment-store ownership. Remove abandoned pending markers and temporary staging files through their normal lifecycle; do not delete immutable stored blobs that another draft, journal, or checkpoint may reference. Late attachment completion must not recreate a deleted marker or inject content into another session. Revalidate submission even if support changes after an attachment has successfully finished.

### Milestone 3: Prove behavior, document platform limits, and deliver


Add focused tests using synthetic capabilities and the existing chat/result fixtures. Use hand-written clipboard fakes or a small local fake for format selection if needed, without a mocking framework or platform clipboard dependency. Prove successful image paste through the actual asynchronous completion route, refusal for false/missing capability, mixed-format precedence, text-only paste, restored-draft submission refusal, capability loss while a read or attachment is pending, modal/context changes, read/codec failure, and deleted-marker/closed-chat completion. Preserve Ctrl+V/Cmd+V routing, empty bracketed paste, and the existing dictation behavior.

Move or adapt existing browser predicate tests rather than duplicating the policy. Existing positive image tests must explicitly seed an image-capable agent; fixtures should not silently grant all chats capabilities. Keep meaningful helper/pipe fixtures larger than 64 KiB where subprocess output is exercised.

Document which clipboard is read on native macOS/Windows/Linux and WSL, the need for a local graphical clipboard on X11/Wayland, and terminal/SSH/tmux forwarding limits. Distinguish deterministic platform-adapter tests from physical clipboard validation. Record any platform not available for native testing instead of claiming that mocks establish native behavior.

With approval received, implement the milestones, run the required checks, and commit only the affected files on the current branch. Push `HEAD:master` to origin as previously authorized. If origin advances, merge without rebasing or changing branches and validate affected changes. Close #965 only after publication, then return with a plan for #1018 before implementing that issue.

## Concrete Steps


Work from `/home/jonathan/Projects/mjolnir3`. The user approved implementation after a local POC, which is now complete.

Run focused checks as changes become ready, using existing test names plus new behavior cases:

    env -u NO_COLOR cargo test -p brokk-mj-chat clipboard
    env -u NO_COLOR cargo test -p brokk-mj-chat image

Then run the mandatory final checks:

    cargo fmt --all -- --check
    git diff --check
    env -u NO_COLOR cargo test
    cargo clippy --all-targets -- -D warnings

Every Cargo test invocation runs with elevated permissions outside the restricted sandbox. Use normal mbx-backed build storage; do not change Cargo target directories or caching. Run any manual new-build daemon, TUI, or CLI exercise only with a separate named instance such as `--instance clipboard-965`, isolated from live sessions. Preserve the existing isolated stores used by automated tests.

## Validation and Acceptance


A capable agent receives an image prompt with the established MIME/content encoding and an editable draft marker. Unsupported or uninitialized agents receive clipboard text when present; an image-only paste adds no attachment and explains why. No normal text is lost, and mixed-format selection follows the approved policy. Capability changes during background work cannot bypass admission or submission. Previously saved images remain in the draft when submission is refused. Dialogs and history readers receive no stray attachment. Failure, dismissal, and cancellation leave no pending marker that later reappears.

Focused behavior tests must fail against the currently ungated terminal path and pass after the fix. The complete dev-profile suite and all-targets clippy must pass. There is no planned database migration, worker protocol change, harness pin change, or new dependency for this issue.

## Idempotence and Recovery


Clipboard reads are repeatable and read-only. Tests use isolated instances and stores. A failed paste preserves the existing draft and permits another attempt; a failed prompt retains text and images. Capability changes never authorize deleting a user's saved images. Continue to use the attachment store's existing immutable references and cleanup rules. Git work remains on the current branch, and only explicitly changed files are staged.

## Outcomes & Retrospective


The local adapter capability POC and terminal implementation are complete. All 581 TUI tests pass, including admission through the asynchronous clipboard completion path, refreshed capabilities, remote submission checks, draft recovery, and modal changes. Full workspace tests, final chat tests, docs checking, formatting, and diff review pass. Clippy passed. The initial push encountered concurrent changes on origin/master; they merged cleanly. Merged-tree validation passed and `3391a719` was published to origin/master; #965 is closed. Native clipboard access and image-first precedence were retained, with selection extracted behind a small platform boundary for fake-clipboard tests; physical clipboard integration was not exercised on macOS, Windows, Linux, or WSL. #1018 planning can now begin, with implementation awaiting its own user approval.

## Artifacts and Notes


The existing policy is equivalent to:

    operational.agent_capabilities.as_ref()
        .is_some_and(|capabilities| capabilities.prompt_capabilities.image)

The existing production completion route is:

    ChatIoUpdate::Clipboard -> queue_attachment -> reserve_attachment

These source facts explain why a shared capability predicate and a guard on asynchronous admission are required, rather than a change only to the synchronous clipboard-content handler.

## Interfaces and Dependencies


Expose `RelayOperationalState::accepts_prompt_images(&self) -> bool` from `mj-core/src/relay/snapshot.rs`, moving the existing controller-only interpretation there. Both the web projection and terminal snapshot application consume this method. `ChatState` keeps the resulting boolean for synchronous input decisions. Reuse `ClipboardContent`, `ClipboardImage`, `ChatIoUpdate::Clipboard`, the existing attachment queue, and `PromptPayload`; the existing ACP image encoding and attachment references remain the transport. Tests use the current chat/session fixtures and small local fakes. No library, workspace crate, schema, or protocol dependency is added.

Revision 2026-09-23: Initial review draft for #965 only, narrowed after the user clarified that both planning and implementation must proceed one issue at a time.
