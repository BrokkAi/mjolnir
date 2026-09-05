# Codex OAuth prompt dictation

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users can click a microphone on the prompt's bottom border, speak, then click again to transcribe speech into their editable draft using a configured Codex subscription. The control is disabled without usable Codex OAuth credentials or the capture helper. Recording and network work must leave the terminal responsive.

## Progress

- [x] (2026-09-04) Inspected existing voice worker, chat runtime, and latest Anvil transcription source.
- [x] (2026-09-04) Replaced local recognition with bounded audio capture and subscription transcription.
- [x] (2026-09-04) Integrated background credential discovery, recording lifecycle, and border button.
- [x] (2026-09-04) Validated the serial full suite, 330 chat tests, 9 voice-worker tests, formatting, Clippy, and live WSLg capture/cancellation.

## Surprises & Discoveries

The sibling Anvil checkout is one commit behind its remote. Fetching exposed commit `73ca21b23632a5082884ab0adbc774eb5c64e4da`, which adds `CodexClient::transcribe` and `TranscribeRequest`. Its package remains version 0.27.1, so a revision-pinned Git dependency is required to consume this API. The existing voice worker uses local Sherpa recognition and a JSON line protocol.

## Decision Log

Use the text presentation microphone `🎙︎`, with display-width-aware placement and mouse hit testing, to match the existing border controls. Reuse the isolated capture worker and replace its local recognition backend. Use explicit configured profile auth paths, preferring the current session's Codex profile; do credential file reads in background tasks. A newline finishes capture; EOF cancels without transcription. This distinguishes deliberate transcription from quitting the UI. These decisions were made on 2026-09-04.

## Outcomes & Retrospective

The microphone control and profile-scoped OAuth transcription are implemented. A three-second WSLg smoke test produced nonzero audio levels and cancelled with an empty result and exit status 0, without uploading audio. Final validation passed: `cargo test -- --test-threads=1`, 330 focused chat tests, 9 voice-worker tests, `cargo clippy --all-targets -- -D warnings`, final focused chat/worker Clippy, formatting, and regenerated dependency reports. A second WSLg capture verified backend/format diagnostics; an invalid Pulse server reported error 6, Connection refused, with WSL context. Live cloud transcription was not exercised; the client integration uses Anvil's transcription API.

WSLg exposes a PulseAudio socket through PULSE_SERVER while ALSA lists only a null device on this host. Linux capture therefore connects directly through dynamically loaded libpulse-simple when PULSE_SERVER is configured. Other environments use CPAL. Backend and format diagnostics go to worker stderr, which the parent logs.

The parallel full suite encountered two timing-sensitive worker test failures (WouldBlock and an elapsed restart timeout); the complete serial rerun passed. Supplemental notice generation also exposed an existing missing Zstandard native-payload audit, so its installed BSD license is now included.

## Context and Orientation

`voice-worker/src/backend.rs` captures native microphone audio. `mj-chat/src/speech.rs` supervises the worker protocol. `mj-chat/src/hel_chat/active.rs` owns background feeds and applies results to the draft. `mj-chat/src/hel_chat.rs` handles chat input. The Anvil dependency is shared through the root workspace manifest, also used by `mj-controller` for utility inference. Unrelated edits in `src/hel_worker.rs` and `.agents/docs/claude-autonomous-turns.md` must remain untouched.

## Plan of Work

First replace the worker backend with CPAL microphone capture, PCM WAV encoding, and Anvil subscription transcription using an explicit auth path. Remove Sherpa dependencies. Next change the parent protocol to distinguish finish from cancel and keep bounded cleanup. Discover available configured Codex credentials off the UI loop and expose availability to chat state. Render and route a bottom-border microphone button using the same geometry for drawing and clicks. Finally test disabled input, recording completion/cancellation, WAV encoding, and worker failure reporting.

## Concrete Steps

From `/home/jonathan/Projects/hel`, run focused package tests for `brokk-mj-chat` and `brokk-mj-voice-worker`, then `cargo test` and `cargo clippy --all-targets -- -D warnings`. All Cargo tests run with elevated sandbox permissions. Run `cargo fmt --all -- --check`. Regenerate the dependency license report if dependencies change its contents. Stage explicit changed paths and commit on the current branch without pushing.

## Validation and Acceptance

With no configured Codex OAuth profile, the microphone is dim and clicking it or pressing Alt-V does not start recording. With usable credentials and the helper installed, clicking records immediately, clicking again uploads the WAV and inserts the returned text without submitting the prompt. Leaving the chat cancels capture/upload. Failures show a notice and restore the control. Tests use fake workers and credentials, avoiding real microphone and subscription requests; live microphone verification requires an interactive audio device.

## Idempotence and Recovery

Changes are additive to the existing control flow and replace the obsolete recognition backend. Failed recording attempts can be retried. Worker teardown is bounded and cancels before resources are dropped. No credentials are copied into repository files or test output.

## Artifacts and Notes

Latest API source can be inspected with `git -C ../anvil show origin/master:crates/anvil-llm/src/transcribe.rs`; the sibling checkout itself remains unchanged.

## Interfaces and Dependencies

Use `anvil_llm::codex_client::CodexClient::with_auth_path` and its async `transcribe(TranscribeRequest)` method. The worker accepts `--codex-auth PATH`. Parent `speech::VoiceCommand` distinguishes `Finish` and `Cancel`; worker output remains JSON status, level, result, and error events. Background availability returns an optional profile auth path, never token contents.
