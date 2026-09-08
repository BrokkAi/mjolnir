# Add voice input to the web composer

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

A web user can dictate into the session composer with a microphone button, stop recording, and edit the resulting text before sending. Existing text and image attachments survive recording, cancellation, and failures. Audio is transcribed using a configured Codex subscription on the controller, including when the current session uses another harness.

## Progress

- [x] (2026-09-08) Inspected native dictation, web composer, HTTP routing, and controller request loop; settled shared interfaces.
- [x] (2026-09-08) Implemented browser capture and lifecycle; root reviewed and tested real Chromium audio capture plus failure and navigation behavior.
- [x] (2026-09-08) Implemented shared credentials and authenticated transcription routes; root reviewed limits, cancellation, and provider error handling.
- [x] (2026-09-08) Integrated supervised controller request handling, updated both CLI constructors, and strengthened backend behavior tests.
- [x] (2026-09-08) Passed full Cargo suite, final controller suite (707 passed, one ignored), 14 focused dictation tests, security headers, Clippy, formatting, 21 JavaScript unit tests, and 39 Chromium cases.
- [x] (2026-09-08) Completed review and validation; changes are included in the current-branch voice-input commit.

## Surprises & Discoveries

Native dictation already uses Anvil Codex transcription and profile-scoped subscription tokens. The controller already depends on Anvil; web dictation needs no native audio dependencies or additional provider credentials. Browser capture must produce WAV directly because media recorder formats differ between browsers.

## Decision Log

Use a browser AudioWorklet (an audio-processing background context) for streaming downmixing and conversion to 16 kHz mono signed 16-bit samples, and a separate worker for WAV assembly. This keeps audio processing off the page event loop. Record at most ten minutes, upload at most 20 MiB, and transcribe for at most 120 seconds. Limit simultaneous web uploads/transcriptions to two with immediate rejection of excess work. Audio is transient and never stored in session history or files.

Use the existing typed request-channel architecture: HTTP handlers ask the CLI controller for session-selected credential paths, while filesystem checks and provider calls run outside the controller loop. Move native credential discovery into the controller crate so both interfaces follow identical profile preference. Cancellation invalidates browser generations and cancels controller tokens on request teardown or shutdown.

## Outcomes & Retrospective

Voice input is implemented end to end, with no new Cargo dependencies. Native and web consumers share credential selection. Full Rust tests, Clippy, formatting, 21 JavaScript unit tests and 39 Chromium cases passed. Six voice cases include the actual Chromium microphone stream, AudioWorklet and WAV worker using a synthetic microphone. Physical microphones, live provider transcription, and iPhone Safari were not tested in this environment.

## Context and Orientation

`mj-controller/src/web/viewer.js`, `viewer.html`, and `viewer.css` implement the browser composer, draft persistence, image uploads, and session switching. New same-origin scripts handle capture and WAV assembly. `mj-controller/src/hel_server.rs` embeds static assets and authenticates API requests. `mj-cli/src/server.rs` owns the session configuration and processes typed requests in a Tokio select loop. `mj-chat/src/dictation.rs` currently discovers usable subscription credentials and will move into `mj-controller/src/hel_dictation.rs`; native callers will reference that shared module. Anvil's existing `CodexClient::transcribe` accepts WAV bytes and a cancellation token.

## Plan of Work

First implement browser lifecycle and shared backend independently with separate file ownership. The microphone starts permission acquisition, recording, then transcription. Cancel is available throughout; editing and images remain usable, while Send is disabled. Every session/workspace change, logout, and page departure stops audio tracks and rejects stale completions. Successful text appends to the current draft with appropriate whitespace and uses existing persistence. Errors identify permissions, secure context, microphone support, unavailable credentials, and server failures.

Next implement GET and POST `/api/sessions/{session_id}/dictation`, preserving existing authentication and request-forgery protection. GET reports availability; POST validates RIFF/WAV structure, mono PCM16 at 16 kHz, size and duration, then returns text. Acquire a two-slot semaphore before accepting upload data. The CLI resolves current session profile order and starts supervised asynchronous jobs. Credential inspection uses spawn_blocking; the provider request is asynchronous, cancellable, and bounded by timeout. No event-loop filesystem or network blocking is permitted.

Finally integrate static assets and browser/API shapes, review actual diffs, run validation, and commit only task files on the current branch. No release or deployment is part of this feature.

## Concrete Steps

Run commands from `/home/jonathan/Projects/hel`. Inspect `git diff --check` and run `cargo fmt --all -- --check`. Run `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings`. In `tests/e2e/web`, run `npm run test:unit` and the relevant Playwright suite using the repository browser harness. Update this plan with exact results and any environmental limitations before committing.

## Validation and Acceptance

Tests must prove streamed conversion across chunk boundaries and multiple input rates with more than 64 KiB of sample data. Browser tests must prove stop/transcribe appends without sending, cancellation discards late results and stops tracks, switching sessions preserves drafts, and failures leave text/images intact. Backend tests must prove authentication, session validation, credential preference, malformed/oversized audio rejection, concurrency limits, cancellation, and actionable errors without invoking the real paid provider. A manual successful dictation should leave editable text beside existing attachments; cancellation should stop the browser microphone indicator immediately.

## Idempotence and Recovery

Changes add no migration or durable audio storage. Retrying tests and restarting recording is safe. Cancelled jobs release capacity and discard audio. Keep unrelated changes out of the commit and retain the current branch.

## Artifacts and Notes

The first integrated build exposed a second ServerRequests constructor in mj-cli/src/web_viewer.rs; it now supplies the new channel. Audio review fixed fractional sample-position loss at chunk boundaries, final-chunk ordering, timeout recovery, and stale timers. Chromium AudioWorklet loading bypasses Playwright page routing, so the real capture test serves worker scripts over loopback.

Validation commands were cargo test, cargo test -q -p brokk-mj-controller, cargo test -q -p brokk-mj-controller dictation, cargo test -q -p brokk-mj-controller every_response_carries_the_security_headers, cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, npm run test:unit in tests/e2e/web, and Playwright with MJ_BROWSER_SPEC selecting attachments, plan-mode, new-session, resume, and voice. All passed.

## Interfaces and Dependencies

Expose `hel_dictation::auth_paths` and `available_auth` for native and web consumers. The typed request contains session ID, availability or transcription operation, cancellation token, and a one-shot reply channel. `execute(request, Option<Vec<PathBuf>>, shutdown)` receives paths selected by the controller, performs off-loop credential inspection and transcription, and replies with availability or text. HTTP JSON is `{available, reason?}` for GET and `{text}` for POST. Browser POST uses `Content-Type: audio/wav`. Serve new worker scripts under same-origin URLs and permit their execution through the existing content security policy.

Revision: updated on 2026-09-08 after final integration review and successful Rust, Chromium, audio conversion, and static checks. The implementation and validation record are included together in the current-branch commit.
