# Drag-and-Drop Any File into Prompt in TUI and Web (#935)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This repository keeps its ExecPlan standard in `.agents/PLANS.md`; maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Today Mjolnir's prompt attachment pipeline accepts only PNG, JPEG, and WebP images (capped at 700 KiB after image optimization, stored digest-only without filenames, and rejected when a session does not advertise ACP `prompt_capabilities.image`). Users who drag a log file, PDF, JSON/CSV fixture, screenshot on a non-image harness, or source file into either the TUI or Web composer cannot attach it — the Web UI ignores or rejects non-images, and the TUI leaves the client-local path as plain text that container and SSH workers cannot read.

After this plan is complete, users can drag, drop, paste, or `/attach` arbitrary files (text, code, logs, PDFs, JSON/CSV, archives, binaries, and images) into both the TUI and Web composers across Local, Container, and SSH sessions. Both surfaces upload bytes through the unified `POST /api/v1/sessions/{id}/attachments` endpoint built in `#1044`, store content-addressed verified blobs in the session's private attachment store, transfer them over the worker relay protocol (protocol 90) into `<worker-root>/attachments/`, materialize a readable worker-side file preserving the sanitized filename and extension (`<worker-root>/attachments/files/<sha256-prefix>/<sanitized_filename>`), and deliver a clean prompt reference that works across all 8 ACP harnesses without passing client-local host paths to workers.

## Progress

- [x] (2026-11-23 23:21Z) Completed codebase & ACP harness audit (`.mj/agents/cd116d9aa028ac43f4684438ca2a96f3/dnd-any-file-map.md`, `.mj/agents/8fee3292a4e04b7e370965f21fc9d486/harness-file-support.md`) and unified TUI + Web HTTP attachment transport in `#1044`.
- [x] (2026-11-23 23:34Z) Slice 1: Core `FileAttachmentRef` & `AttachmentStore` file support, relay protocol 90 (`FileAttachmentPresent` / `InstallFileAttachment` / `ReadFileAttachment`), worker materialization & ACP resolution, session-runtime/resume sync, and transcript rendering.
- [x] (2026-11-24 00:09Z) Slice 2: Generalized `POST /api/v1/sessions/{id}/attachments`, `ControllerAction::Prompt` file attachments, `HttpDaemonClient::upload_file_attachment`, Web viewer (`viewer.js`) drag-drop/paste/chips/submission, and Playwright e2e coverage (`tests/e2e/web/attachments.spec.js`).
- [x] (2026-11-24 00:36Z) Slice 3: Generalized TUI (`mj-chat`) pasted-path detection, `/attach`, composer file chips & backward-compatible drafts (`PromptPayload` / `PromptPayloadWire`, `mj-core/src/draft.rs` `Envelope` + `merge_drafts` + `validate_reference_draft`, `terminal_queue_startup_prompt`), image-to-file fallback for non-image harnesses, and `TZ=UTC` `mj-chat` golden/unit coverage.

## Surprises & Discoveries

- Observation: `mj-core/src/attachment.rs::AttachmentRef` carries `#[serde(deny_unknown_fields)]`, validates PNG/JPEG/WebP signatures and dimensions, enforces `MAX_IMAGE_BYTES = 700 KiB`, and stores blobs solely by 64-char SHA-256 hex (no filename or extension).
  Evidence: Mutating `AttachmentRef` in place would break existing wire/draft/checkpoint compatibility. Adding a distinct `FileAttachmentRef` (`mj-file-attachment:<base64url-json>`, encoded as `ContentBlock::ResourceLink`) alongside `AttachmentRef` preserves 100% backward compatibility for existing image attachments while carrying sanitized filename + MIME + size metadata.

- Observation: `mj-worker/src/acp.rs::prompt_for_harness` passes all 8 ACP harnesses through a single transform without non-image `ResourceLink`/`embeddedContext` capability handling, and harnesses/bridges vary in whether they read ACP `ResourceLink` URIs.
  Evidence: Materializing each attached file on the worker under `<worker-root>/attachments/files/<sha256[..16]>/<sanitized_filename>` (preserving the sanitized original filename and extension) and resolving the `mj-file-attachment:` `ResourceLink` at the ACP boundary into a `ContentBlock::Text` naming the file, MIME type, size, and local worker path works reliably across all 8 harnesses and all placements (Local, Container, SSH).

- Observation: `mj-core/src/relay.rs::MAX_FRAME_BYTES` is `8 * 1024 * 1024` (8 MiB), and `InstallAttachment` encodes bytes as standard base64 inside a single relay JSON frame.
  Evidence: A 5 MiB raw file limit (`MAX_FILE_BYTES = 5 * 1024 * 1024`) base64-encodes to ~6.67 MiB, leaving >1.3 MiB headroom inside the existing 8 MiB single-frame relay budget without needing a separate chunked relay transfer protocol.

## Decision Log

- Decision: Keep `AttachmentRef` (`mj-attachment:`, `ContentBlock::Image`) unchanged for optimized inline images, and introduce a distinct `FileAttachmentRef` (`mj-file-attachment:`, `ContentBlock::ResourceLink`) sharing the same content-addressed `attachments/<sha256>` directory and `mj-attachments/` checkpoint archive prefix.
  Rationale: Preserves strict wire, draft, and checkpoint compatibility with existing images and older workers, avoids parallel storage directories, and lets `ContentBlock::ResourceLink` carry lightweight metadata in durable relay commands/drafts/transcripts without embedding megabytes of bytes.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Cap generic file attachments at `MAX_FILE_BYTES = 5 * 1024 * 1024` (5 MiB) and `MAX_FILES = 10` per prompt, explicitly derived from `mj_core::relay::MAX_FRAME_BYTES` (8 MiB) minus base64 expansion (4/3) and JSON envelope overhead.
  Rationale: Follows `AGENTS.md` (every cap is derived from a documented/measured system limit and documented in a comment) and fits cleanly inside one relay RPC without inventing a second chunked transfer mechanism.
  Date/Author: 2026-11-23 / Coordinator

- Decision: Gate the new worker relay RPCs (`FileAttachmentPresent`, `InstallFileAttachment`, `ReadFileAttachment`) on `RELAY_FILE_ATTACHMENT_PROTOCOL = 90`, refusing clearly on the controller side when submitting a file attachment to an older worker.
  Rationale: Preserves rolling worker compatibility with running pre-90 workers while failing visibly with actionable guidance ("restart the session to attach files").
  Date/Author: 2026-11-23 / Coordinator

- Decision: Materialize worker-side files under `<worker-root>/attachments/files/<sha256[..16]>/<sanitized_filename>` (hardlink from immutable `<worker-root>/attachments/<sha256>` with atomic temp-file fallback on cross-device/unsupported FS) and resolve `mj-file-attachment:` `ResourceLink` blocks to readable `ContentBlock::Text` pointers at the ACP boundary.
  Rationale: Extension-sensitive tools and harnesses receive a real local file path with its original extension (e.g., `.pdf`, `.csv`, `.rs`, `.log`), isolated per digest prefix to avoid filename collisions, while never exposing client-local paths to containers or remote SSH hosts.
  Date/Author: 2026-11-23 / Coordinator

- Decision: When an image is dropped/attached on a session whose harness does not support `prompt_images_supported`, fall back automatically to attaching it as a generic file attachment rather than refusing.
  Rationale: Satisfies `AGENTS.md` ("Never leave the user stuck") — the agent can still inspect the image file on disk or via tools instead of hitting a dead end.
  Date/Author: 2026-11-23 / Coordinator

- Decision: In the TUI composer (`pasted_paths.rs`), recognize arbitrary pasted absolute file paths syntactically without blocking the event loop, verify `metadata.is_file()` on the background attachment task, and silently restore the exact pasted text if a pasted path does not exist or is a directory (while `/attach <path>` reports explicit errors).
  Rationale: Prevents false-positive errors when users paste URL paths like `/api/v1/sessions` or directory paths, keeps event/render loops 100% non-blocking, and ensures real dragged/dropped files attach reliably across macOS, Linux, and Windows/WSL.
  Date/Author: 2026-11-23 / Coordinator

## Plan of Work

### Slice 1: Core, Relay Protocol 90, Worker Materialization, Session Runtime, Drafts & Transcripts

- **`mj-core/src/attachment.rs`**:
  - Add `MAX_FILES: usize = 10`, `MAX_FILE_BYTES: usize = 5 * 1024 * 1024` (with comment citing `relay::MAX_FRAME_BYTES` 8 MiB minus 4/3 base64 + JSON envelope), `FILE_URI_PREFIX = "mj-file-attachment:"`, and `FileAttachmentRef { sha256: String, filename: String, mime_type: String, size: usize }` (`#[serde(deny_unknown_fields)]`).
  - Implement strict `sanitize_filename(&str) -> String` (single path segment, strips directories/drive prefixes, rejects/replaces control chars, NUL, path separators, `.`/`..`, trims whitespace, caps length at 128 bytes preserving UTF-8 and extension, falls back to `"attachment"` when empty) and `validate()`/`verify(bytes)`/`uri()`/`from_uri(uri)`/`content_block()` (`ContentBlock::ResourceLink`).
  - Add `file_reference(&ResourceLink) -> Result<Option<FileAttachmentRef>>`, `file_references(&[ContentBlock]) -> Result<Vec<FileAttachmentRef>>` (enforcing `<= MAX_FILES`), and update `has_references(&[ContentBlock])` to detect both image and file references.
  - Extend `AttachmentStore` with `install_file`, `read_file`, `contains_file`, `materialize_file(&self, reference: &FileAttachmentRef) -> Result<PathBuf>` (writing to `<root>/files/<sha256[..16]>/<sanitized_filename>` atomically, verifying existing content before reusing), and update `archive_artifacts` / `restore_artifact` to allow valid blobs up to `MAX_FILE_BYTES` (allowing 0-byte generic files while keeping `AttachmentRef::validate` strictly `> 0` and `<= MAX_IMAGE_BYTES`) and skip the `files/` materialized subdirectory during archive export.
  - Update `AttachmentStore::resolve(&self, prompt: &mut [ContentBlock])` to resolve BOTH `mj-attachment:` images (base64 inline) AND `mj-file-attachment:` `ResourceLink`s (materializing on the worker and replacing the block with a `ContentBlock::Text` naming filename, MIME type, size, and worker filesystem path).
- **`mj-core/src/relay.rs` & `mj-core/src/relay/protocol.rs`**:
  - Add `RELAY_FILE_ATTACHMENT_PROTOCOL: u32 = 90` and bump `RELAY_PROTOCOL_VERSION = RELAY_FILE_ATTACHMENT_PROTOCOL`.
  - Add `RelayRequest::{FileAttachmentPresent { reference }, InstallFileAttachment { reference, data }, ReadFileAttachment { reference }}` gated on protocol 90, reusing existing `RelayResponsePayload::{AttachmentPresent, AttachmentInstalled, AttachmentData}`.
- **`mj-worker/src/worker_runtime/unix.rs` & `mj-worker/src/relay/requests.rs` & `mj-worker/src/acp/session.rs`**:
  - Handle the 3 new `RelayRequest` variants in `unix.rs` (checking base64 ceiling `MAX_FILE_BYTES.div_ceil(3) * 4`, calling `store.contains_file`/`install_file`/`read_file`).
  - Update `PromptAttachments` error wording (`"could not load attached files/images"`) and ensure `PromptAttachments` triggers whenever `mj_core::attachment::has_references(&prompt)` is true (covering file-only prompts too!).
- **`mj-session-runtime/src/worker_client/relay.rs`, `mj-session-runtime/src/session_manager/standalone.rs`, `mj-controller/src/controller/resume/repair.rs`**:
  - Add `file_attachment_present`, `ensure_file_attachment`, `cache_file_attachment`, checking negotiated worker protocol `< RELAY_FILE_ATTACHMENT_PROTOCOL` and returning a clear `Refusal` if the worker predates file attachments.
  - Sync all `file_references(prompt)` alongside image `references(prompt)` in `submit` and `standalone.rs`, and verify/repair them in `resume/repair.rs`.
- **`mj-core/src/transcript.rs`**:
  - Render `mj-file-attachment:` `ResourceLink` blocks cleanly in `mj-core/src/transcript.rs` (`[file: <filename>]` rather than leaking base64 URIs).

### Slice 2: Unified HTTP Upload, Prompt Submission, Web Viewer & Playwright E2E

- **`mj-server/src/server/handlers.rs`, `validation.rs`, `actions.rs`, `viewer_types.rs` & `mj-client/src/http/sessions.rs`**:
  - Generalize `POST /api/v1/sessions/{id}/attachments`:
    - Accept optional `X-Mjolnir-Filename` header (URL-decoded or raw UTF-8 header; sanitize via `FileAttachmentRef::sanitize_filename`) and optional `?mode=file` query param.
    - Dispatch cleanly: if `mode != "file"` AND the uploaded bytes/Content-Type are PNG/JPEG/WebP AND (`loopback_authenticated` or `prompt_images_supported`), keep the existing image optimization + `AttachmentRef` path (preserving existing response fields `data_base64`, `mime_type`, `width`, `height`, `attachment`); otherwise store as `FileAttachmentRef` (validating size `<= MAX_FILE_BYTES`, inferring/normalizing MIME type from `Content-Type` header or filename extension with fallback `application/octet-stream`) and return `{ ..., file_attachment: Some(file_ref) }`!
    - For explicit image-only callers without a filename and unsupported image bytes where `mode` is not set and `X-Mjolnir-Filename` is absent, check existing server tests to preserve expected 400 errors on corrupt anonymous image uploads while accepting any named file (`X-Mjolnir-Filename` or `mode=file`)!
  - Extend `ControllerAction::Prompt` with `#[serde(default)] files: Vec<FileAttachmentRef>` (and `ViewerPromptImage` / response type cleanly with additive `file_attachment: Option<FileAttachmentRef>`). Validate every submitted `FileAttachmentRef` in `decode_prompt_images_off_task` against `AttachmentStore::controller(session_id)` (preventing cross-session smuggling and enforcing `files.len() <= MAX_FILES` and `ensure_prompt_command_fits`), then build `ContentBlock::ResourceLink` blocks into the submitted `RelayCommand::Prompt`.
  - Add `HttpDaemonClient::upload_file_attachment(&self, id: &str, filename: &str, bytes: Vec<u8>, mime_type: &str) -> Result<FileAttachmentRef>` (and `upload_prompt_attachment` helper if shared cleanly).
- **`mj-server/src/web/viewer.js` & `mj-server/src/web/viewer.css`**:
  - Generalize composer attachment state from image-only to typed entries (image vs file), accepting ANY dropped or pasted `File` (and file-picker selection if present), sending `X-Mjolnir-Filename` + `Content-Type` to `POST /api/v1/sessions/{id}/attachments`.
  - When `!prompt_images_supported`, upload dropped images with `?mode=file` so they attach seamlessly as files instead of failing.
  - Render clear chips for both image thumbnails and generic file attachments (filename + human-readable size + uploading/error state + remove button).
  - Preserve session-pinned async upload reconciliation (switching sessions clears chips and ignores late completions from prior sessions), mixed-drop partial failure notices (valid files keep uploading while oversized/failed files report a notice), queued-prompt summary rendering for `resource_link` blocks (`[file: name]`), and normal `text/plain` paste behavior!
- **`tests/e2e/web/attachments.spec.js`**:
  - Extend Playwright specs to cover dropping text, PDF/binary, mixed valid + oversized/failing drops, session-switch cancellation/ignoring late responses, image fallback on non-image sessions, and paste behavior (file paste attaches; `text/plain` paste inserts text).

### Slice 3: TUI Composer, Pasted Paths, `/attach`, Drafts & Goldens (`mj-chat`, `mj-tui`)

- **`mj-chat/src/chat/pasted_paths.rs`**:
  - Generalize `image_paths` -> `pasted_file_paths` recognizing arbitrary absolute file paths (Unix `/...`, quoted, backslash-escaped, `file://`, Windows drive & UNC paths for WSL `wslpath -u`), rejecting bare `/` or prose/URLs (`http://`, `https://`), and flagging whether each path has an image extension.
- **`mj-chat/src/chat/attachments.rs`, `mj-core/src/draft.rs`, & `mj-daemon/src/daemon/terminal_operations.rs`**:
  - Extend `PromptPayload` and `PromptPayloadWire` with `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub files: Vec<PromptFile>` (tracking inline byte range, number, filename, size, `Option<FileAttachmentRef>`, placeholder state, and marker text like `[file #1: report.pdf]`).
  - Preserve 100% backward compatibility with existing plain-text and image-only drafts, and emit `ContentBlock::ResourceLink` for file ranges in `PromptPayload::content_blocks()`.
  - Update `mj-core/src/draft.rs` (`Envelope::decode`, `Envelope::encode`, `merge_drafts`, `validate_reference_draft`) and `PromptPayload::trimmed` / range-shifting helpers so tracked `files` entries receive the same range validation, renumbering, and range-shifting during draft recovery/merge as `images`, and allow installed `FileAttachmentRef` `ResourceLink` blocks in `terminal_queue_startup_prompt`.
- **`mj-chat/src/chat/{composer.rs, input_state.rs, active.rs, remote.rs}`**:
  - Run `PastedPath::resolve()` + `fs::metadata(&path)` + file reading + HTTP upload (`HttpDaemonClient::upload_attachment` / `upload_file_attachment`) strictly off the event/render loop.
  - For pasted paths: if a path does not exist or is not a regular file (`!metadata.is_file()`), restore the exact original pasted text silently (so pasting `/api/v1/sessions` or a directory path never surprises the user with an error). If a real file exceeds `MAX_FILE_BYTES`, restore/report a clear size notice.
  - For `/attach <path>`: support any file (not only images), reporting clear errors if missing, a directory, or oversized.
  - When an image file is pasted or `/attach`ed on a session without image capability (`!prompt_images_supported`), upload it as a generic file attachment automatically instead of refusing.
- **Validation**:
  - Targeted tests across `brokk-mj-core`, `brokk-mj-worker`, `brokk-mj-session-runtime`, `brokk-mj-controller`, `brokk-mj-server`, `brokk-mj-daemon`, `brokk-mj-client`, `TZ=UTC cargo test -p brokk-mj-chat`, and Playwright `tests/e2e/web/attachments.spec.js` + deterministic web suite (never concurrently with Cargo).

## Outcomes & Retrospective

Slice 1 implements generic file references and relay protocol 90. The worker verifies file blobs before admitting a prompt and resolves its links to readable paths before ACP dispatch. Materialization uses atomic copies so edits to the path supplied to the harness leave the original blob intact. The remaining slices connect the upload and composer entry points.

The focused attachment, transcript, worker relay, and resume checks passed. The full session-runtime suite passed 237 tests, including the protocol 89 refusal, and reported two failures that reproduced on base revision `b3a1df58c`. Durable startup-draft and `merge_drafts` file support were deferred to Slice 3 so `mj-core/src/draft.rs`, `mj-daemon/src/daemon/terminal_operations.rs`, and `mj-chat/src/chat/attachments.rs` land together with one wire shape. The selected Clippy checks passed.

Slice 2 generalized `POST /api/v1/sessions/{id}/attachments` with `?mode=file` (enforcing `DefaultBodyLimit::max(MAX_FILE_BYTES)` before buffering, percent-decoding `X-Mjolnir-Filename`, and normalizing `Content-Type`), extended `ControllerAction::Prompt` with `files` verified against the submitting session's store before writing inline image blobs, added `HttpDaemonClient::upload_file_attachment`, and unified the Web viewer's composer upload queue, chips, drag-drop/paste/picker, pending/queued display, and image-to-file fallback. Server, client, and daemon targeted tests passed (with the pre-existing `phone_snapshot_projects_capability_gated_and_agent_commands_with_provenance` failure reproduced on base `be6557e7e`), Clippy passed across all three crates, and the Web suite passed 100 Node tests and 156 Playwright browser tests (updating the 16 `#attach-image` label/visibility entries in `viewer_conversation.txt`).

Slice 3 completed `#935` across the TUI composer and durable drafts: generalized `pasted_paths.rs` to arbitrary absolute paths (excluding single-segment slash commands and silently restoring missing/non-regular pasted paths off the event loop), added `PromptFile` / `AttachedFile` markers (`[file N: <filename>]`) with backward-compatible `PromptPayload` / `PromptPayloadWire` serialization and atomic editing, updated `mj-core/src/draft.rs` (`Envelope`, `merge_drafts`, `validate_reference_draft`) and `mj-daemon/src/daemon/terminal_operations.rs` (`terminal_queue_startup_prompt`) in lockstep, enabled `/attach` and automatic image-to-file fallback on non-image sessions, and updated standby `/attach` dispatch. Validation passed: `brokk-mj-core` draft (5/5), `brokk-mj-daemon` startup_prompt (7/7), `TZ=UTC` `brokk-mj-chat` (423 passed, 2 ignored, including all 31 goldens), `brokk-mj-tui` `pasted_image_paths` (1/1), `brokk-mjolnir` `startup_prompts` (4/4), and Clippy `-D warnings` across all 5 packages.
