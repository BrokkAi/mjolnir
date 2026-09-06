# Edit pasted images as numbered composer markers

This plan follows `.agents/PLANS.md` and stays current through implementation.

## Purpose / Big Picture

The user wants ordinary Ctrl-V image paste and visible `[image N]` markers that disappear, with their attachment, when backspaced over. Image attachment behavior should belong to ordinary text editing rather than requiring a removal shortcut. Ctrl-V already reaches the clipboard reader when delivered by the terminal; Ctrl-Alt-V remains an optional fallback for terminals that intercept it. “Unsent” means submission failed before acceptance by the session, not a model refusing to answer.

## Progress

- [x] Inspect clipboard, composer editing, payload serialization, and submission paths.
- [x] Delegate the independent marker/payload module to Luna; root owns integration and review.
- [x] Integrate atomic marker editing, multiple image payloads, and durable drafts/retries.
- [x] Make Ctrl-V primary in help and remove the separate image-removal affordance.
- [x] Validate editing behavior and complete required Rust checks. The validated implementation is ready to commit on the current branch.

## Surprises & Discoveries

History setters clear the navigation index, so restoring a history payload must set the chosen index after assigning text/images. Existing navigation tests caught this integration regression and the fix retains the prior navigation behavior.

The synthetic tmux probe delivered Ctrl-V through crossterm to `PasteFromClipboard`, then Backspace removed the second queued image marker atomically. It did not access the Windows clipboard or contact a model. The prior Windows clipboard access limitation remains unverified end-to-end.

The first full suite encountered an unrelated `ExecutableFileBusy` error while starting the fake worker in `stop_worker_script_kills_a_matching_daemon_and_is_idempotent`. The final full rerun passed, including that worker test. A render test initially checked the normal footer while the paste notice owned that row; the test now clears the temporary notice before checking shortcut help.

The first implementation stored one image separately from input text. Every character edit ignored it, so Backspace could never remove it. Text mutation occurs in `input.rs`, history previews, autocomplete, and whole-draft replacement; all must respect tracked attachment ranges. The relay already accepts multiple ACP image blocks and limits a complete command to 1 MiB.

## Decision Log

Image bytes use `Arc<str>` so ordinary keystrokes and rich draft copies share the encoded image instead of copying hundreds of kilobytes. The JSON representation stays unchanged. Number ranges refer to UTF-8 byte offsets; only tracked ranges become ACP image blocks.

Track actual marker byte ranges and image bytes independently of visible marker spelling. Typing literal `[image 1]` must not fabricate an attachment. A deletion intersecting a tracked marker removes it atomically; cursor motion snaps across it. Rich kill/yank retains the attachment. Numbered markers are replaced with ACP image blocks in their original position at submission, preserving text/image ordering.

Preserve the existing versioned draft prefix and decode old `{text,image}` drafts into inline markers. New drafts store text and tracked images. Multiple small images can share a prompt within the existing total command limit. Keep PNG extraction on the background clipboard worker. Do not change Windows Terminal settings or the user's clipboard.

## Outcomes & Retrospective

The composer now inserts numbered markers at the caret, treats them atomically for deletion and motion, and preserves image bytes through kill/yank, draft reload, failed submissions, queued editing, and temporary history navigation. Ctrl-V is the primary advertised shortcut. Validation passed: 2,352 workspace Rust tests (12 ignored), including 372 chat tests; Clippy with all targets and warnings denied; rustfmt; and diff whitespace checks. The synthetic tmux check passed as described above. No implementation work remains.

## Context and Orientation

`mj-chat/src/hel_chat.rs` owns composer state, action selection, queued prompts and drafts. `hel_chat/input.rs` owns text mutation, kill/yank, and cursor movement. `hel_chat/history.rs` temporarily replaces composer text during search/navigation. `hel_chat/remote.rs` constructs relay commands and restores failed submissions. `hel_chat/active.rs` runs clipboard tasks and renders the composer/footer. `hel_clipboard.rs` already implements WSL PowerShell image extraction and bounds one encoded image. The new `hel_chat/attachments.rs` will own tracked marker editing and payload serialization without UI or process I/O.

## Plan of Work

First introduce a standalone `PromptPayload` with display text and `PromptImage` ranges, draft migration, ACP conversion, and editing helpers. The helpers must distinguish real image markers from lookalike text. Root then routes composer edits and cursor motion through those helpers, preserves rich kill buffers and temporary history drafts, and carries a complete payload through remote submission and saved failures. Finally adjust help to advertise Ctrl-V and ordinary Backspace, review the integration, and verify the user-facing behaviors.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. Keep ignored validation logs under `target/image-marker-ux/`. Run all `cargo test` commands outside the restricted sandbox. Run focused chat tests followed by `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Use isolated tmux for an input check if useful; never change user clipboard contents for a test.

## Validation and Acceptance

Pasting two images inserts `[image 1]` and `[image 2]` at the caret. Backspace or Delete removes the adjacent marker and the corresponding payload without touching surrounding Unicode text. Cursor motion cannot land inside a tracked marker. Kill/yank keeps image bytes; typed marker lookalikes remain text. Submission preserves interleaved text/image order and enforces the whole-command size budget. Draft save/reload, failed submission, and queued editing preserve the same attachments. Existing Ctrl-V handling stays active and is advertised as primary. Full checks pass and only task-owned files are committed.

## Idempotence and Recovery

Keep legacy draft decoding, rejected submission recovery, and text-only input behavior intact. Use the current branch, explicit staging, and no new branch or rebase. Background clipboard failures remain visible and bounded. No user configuration or clipboard writes are needed.

## Artifacts and Notes

Evidence is under ignored `target/image-marker-ux/`: `all-tests-final.log`, `clippy.log`, and `tmux.log`. The temporary tmux probe source was removed.

This follow-up replaces the out-of-band single-image state introduced in `6af2bdc8`. It retains the Windows extraction mechanism and global-config/model restoration fixes.

## Interfaces and Dependencies

Use existing ACP types, ClipboardImage, serde, and Unicode text helpers. `PromptPayload` owns text plus tracked `PromptImage` ranges. `replace_range`, `insert_image`, and `snap_cursor` centralize range behavior. No new crate or external dependency is needed.

Created 2026-09-05 to implement the user's numbered, backspace-removable image preference and clarify paste shortcuts.
