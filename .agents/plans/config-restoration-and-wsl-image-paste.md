# Coordinate config saves, retain session choices, and paste WSL images

This ExecPlan follows `.agents/PLANS.md` and remains current throughout implementation. The user approved two targeted fixes identified in the local Belgr comparison and additionally requested working image paste on this WSL host.

## Purpose / Big Picture

Concurrent settings changes should preserve each other's unrelated fields. A model or effort accepted for a running session should survive worker and ACP bridge replacement without changing defaults for other sessions. Pasting a Windows screenshot into the terminal composer under WSL should attach an image that reaches the agent even when it runs inside a container. The terminal must stay responsive during clipboard access and config writes.

## Progress

- [x] Inspect current config writers, accepted-config relay events, composer/remote submission, host WSL environment, and the installed Claude Code clipboard mechanism.
- [x] Delegate config transactions and the clipboard/composer implementation to Luna agents with separate ownership; parent owns session restoration, design, live verification, and integration.
- [x] Implement cross-process config transactions and convert production field mutations.
- [x] Restore accepted session model/effort before accepting new prompts after worker or bridge restart.
- [x] Implement bounded WSL image clipboard reading, visible attachments, real ACP image content, and draft/retry preservation.
- [x] Run focused behavior tests, full Cargo tests, Clippy, formatting, and appropriate live checks.
- [x] Record evidence and commit validated changes on the current branch.

## Surprises & Discoveries

`HelConfig::save_review_to` reloads current config and atomically replaces the file, but it does not serialize the read/edit/write operation. A stable sibling lock file must coordinate updates, because locking the replaced config inode cannot serialize later opens. Belgr's process-local mutex is insufficient for independent Mjolnir processes.

The relay already records `ConfigurationUpdated` after ACP accepts a selector. Worker startup constructs a new `LaunchSpec` from the profile and native session identity without feeding those durable choices back into ACP. The live review test repeatedly observed Sol reset to the profile default after automatic worker upgrades. Restoring the current session is the intended fix; making every live selection a global default is outside scope.

This host reports a Microsoft WSL2 kernel and has Windows PowerShell available. The installed Claude Code binary contains a WSL path that uses `System.Windows.Forms.Clipboard.GetImage()` in an STA PowerShell process, encodes PNG into a memory stream, and returns base64. Mjolnir's existing `hel_clipboard` only calls arboard text methods, and its arboard dependency disables image support. Transporting a Linux-local filename would not deliver image bytes to a container agent.

## Decision Log

Completed selectors are canonicalized when the existing durable SetConfig command completes, using the returned catalogue. This captures model-dependent effort resets or disappearance, resolves raw/canonical aliases, and adds no journal event or schema. The live bridge uses the same accepted-pair logic. Only startup after restoration counts as an opened running bridge, preserving the actual restore error instead of retrying it as a crash.

One image per composer is supported, capped at 700 KiB to fit base64 inside the existing 1 MiB command budget. The complete prompt is checked too. Ctrl-Alt-V avoids terminal interception of Ctrl-V; Alt-V remains dictation. Ctrl-X removes the image. Ctrl-Alt-R restores the latest refused prompt into an empty composer. Refused images are saved with drafts, so a failed image and a newer attachment remain recoverable. Unsupported multi-image queued prompts refuse edits rather than dropping content. Local slash/shell commands refuse attached images rather than silently discarding them.


Use one config transaction helper holding a cross-process sibling-file lock around latest load, field edits, validation, and atomic replacement. Convert mutation sites to closures over the fresh configuration; serializing stale whole-object saves alone does not prevent lost updates. Keep all work in existing background operations and preserve newer-schema read-only rules.

Reuse the session's durable relay state as the source for accepted model/effort restoration. Apply model before model-dependent effort, refresh advertised choices, and make restoration complete before the ready/session-configured event allows queued prompts through. Retain choices through in-process bridge restarts as well. Reject or visibly fail restoration when a previously accepted choice is unavailable; do not silently fall back. Do not replay permission/plan-mode choices that could override current execution policy.

Use a bounded Windows clipboard process under WSL, with fixed script text and separate arguments, no shell interpolation and no shared Windows temporary file. Clipboard reads run off the UI loop. The composer owns visible/removable image attachments and sends actual ACP image content through the existing relay. Failed delivery and saved drafts must retain the image payload, and ordinary text fields remain text-only. Use existing framing/size constraints to bound attachments.

## Outcomes & Retrospective

Implemented both targeted Belgr ports and image paste. The full workspace suite passed 2,336 tests (excluding two nested config-test child invocations), and workspace Clippy passed with warnings denied. A focused final package run covers the last clipboard/import refinements. No user configuration, profile homes, or clipboard contents were modified.

Live verification generated a real 256x256 PNG in Windows memory and transported 350,476 base64 bytes through PowerShell, the shared subprocess executor, and the application parser. A separate tmux socket delivered Ctrl-Alt-V correctly through crossterm. The actual Windows clipboard initially reported an image, then text, and finally failed with CLIPBRD_E_CANT_OPEN. The final reader retries transient exceptions and reports a useful bounded failure. Actual clipboard image paste could not be completed while Windows refused access; generated-image transport and actor submission are verified separately. Nothing from the clipboard was sent to a model. Temporary probe source was removed; ignored logs remain under target/task-followup/.

## Context and Orientation

`src/hel_config.rs` reads/writes global config. Controller and CLI config mutations currently live in `mj-controller/src/hel_controller.rs`, its `resume.rs` module, and `mj-cli/src/import.rs` and `dashboard/io.rs`. `src/hel_acp.rs` owns ACP sessions and restarts native bridges; `mj-worker/src/hel_worker_runtime/unix.rs` records accepted changes in the durable relay and launches ACP. `src/hel_worker.rs` and `src/hel_worker/journal.rs` own the persisted session state.

`mj-chat/src/hel_clipboard.rs` is the platform clipboard boundary. `mj-chat/src/hel_chat.rs` owns draft and input state, `hel_chat/active.rs` runs background jobs, and `hel_chat/remote.rs` turns submissions into relay requests. The relay already transports ACP content blocks, including images. Draft persistence currently stores a string, so any richer draft representation must decode existing plain-text drafts unchanged.

## Plan of Work

First implement and test config transactions independently of session restoration and clipboard work. A cross-process test should run two disjoint mutations against one config file and observe both in the result; rejected mutations must leave the file intact.

For session restoration, trace accepted values from relay recording to worker startup and bridge restart. Supply only the session selectors needed by this change, using existing ACP selector interpretation. Tests should run an adapter that resets its configuration on load, record accepted model and effort, restart it, and prove the first subsequent prompt sees the restored pair. Failed changes must not become restoration defaults.

For image paste, implement the WSL provider and composer state together so successful extraction cannot end as a text-only prompt. Exercise image-only and mixed prompts, removal, refused delivery, queued restoration, and session draft round-tripping. Test process buffers with more than 64 KiB. Live verification should inspect the Windows API on this host and, when an image is available, prove the actual PNG reaches the application's attachment path. Do not overwrite the user's clipboard merely to simplify a test.

Finally review integrated changes and run required checks. Commit explicit task-owned files directly on `hel2`; do not change branch or import Belgr's branding/routing commits.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Put ignored evidence/build logs under `target/task-followup/`. Run every `cargo test` outside the sandbox. Run focused package tests as implementation stabilizes, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Use a disposable configuration and fake ACP harness for restart tests; use a separate tmux socket for terminal checks. Inspect actual Windows clipboard metadata without printing or publishing private clipboard contents.

## Validation and Acceptance

Two processes editing unrelated global config fields preserve both changes. A current session keeps accepted model/effort after worker upgrade and ACP bridge restart, with restoration preceding any prompt; other sessions' defaults are unchanged. WSL image paste produces a visible attachment and sends PNG data as ACP image content. Cancellation, refused delivery, and switching away do not silently lose pasted images. Plain-text paste remains intact. All required Rust checks pass and evidence describes any host-specific limitation precisely.

## Idempotence and Recovery

Keep lock files stable and release locks by closing their handles. Stop owned child processes before removing any working files. Preserve global config schema compatibility and user clipboard contents. Use durable accepted session values rather than a second global preference store. Stage only task-owned paths and keep unrelated upstream changes separate.

## Artifacts and Notes

The reviewed Belgr ideas came from `24ff153` (accepted selectors and config write serialization) and `12b5270` (accepted model-route persistence). They require adaptation to this architecture. The inspected Claude Code installation is `/home/jonathan/.local/share/claude/versions/2.1.261`; only the platform mechanism is reused, not its minified implementation.

## Interfaces and Dependencies

Prefer an idiomatic `HelConfig::update`/`update_to` closure API using standard file locking. Extend the existing ACP startup input with session-specific accepted selectors and share existing model/effort interpretation. Keep clipboard content and composer attachments in the chat crate; use existing subprocess helpers and existing ACP image types. Add image encoding dependencies only if required by the platform reader. No new workspace crate is needed.

Created 2026-09-05 after approval of the Belgr-inspired fixes and WSL image-paste work.

Validation logs: target/task-followup/all-tests.log, final-chat-cli-tests.log, clippy.log, session-config-tests.log, wsl-live-final.log, and tmux-paste.log. The config subprocess test confirms another process owns the stable lock before release, then verifies both disjoint edits survive. The raw conversion fixture now persists its initial configuration, matching production's disk-backed source.

Publication checkpoints: `17d719e3` contains config transactions and accepted-selector restoration. Image paste and this evidence are committed as the next checkpoint on `hel2`. Upstream is `origin/master`; publication retains its three incoming commits by merging before push.
