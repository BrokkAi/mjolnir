# Split brokk-mj-core into four layered crates

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture


Today every line of the control plane lives in one Rust crate, `brokk-mj-core` (library name `hel`, sources under `src/`), about 152,000 lines including colocated tests. Rust compiles a crate's front end (parsing, type checking, borrow checking, metadata) on one thread, and an edit anywhere in the crate recompiles the whole crate and relinks one test binary with about 1,640 tests. Measured on 2026-09-02 on a 120-core host under load: touching `src/hel_chat.rs` costs 22 s to rebuild the `mj` binary and 15 s to rebuild the core test binary; a full non-incremental build of the crate is 43 s of compiler time, of which about 33 s is the serial front end.

After this change the same code lives in four crates layered by dependency: a foundation (`brokk-mj-core`, keeping the `hel` library name), a target-side worker (`brokk-mj-worker`), the daemon-side controller (`brokk-mj-controller`), and the conversation view state (`brokk-mj-chat`). An edit in the chat layer recompiles about 23,000 lines and links a chat-only test binary; an edit in the worker layer recompiles about 10,000 lines. The worker and controller crates have no dependency on each other, so Cargo builds them in parallel. Nothing user-visible changes: `mj`, the daemon, the TUI, the web surface, and the container worker behave exactly as before, and every existing test passes in its new crate. The proof is the timing comparison in `Outcomes & Retrospective` plus a green `cargo test` and `cargo clippy --all-targets -- -D warnings`.

Ryan Svihla approved the split on 2026-09-02.


## Progress


- [x] (2026-09-02 15:30Z) Measured the module graph (script in `Artifacts and Notes`), chose the four layers, and listed every edge that crosses a layer boundary in the wrong direction (see `Context and Orientation`).
- [x] (2026-09-02 15:45Z) Recorded the pre-split baseline timings (see `Artifacts and Notes`).
- [x] (2026-09-02 21:05Z) M1a: text helpers and browser transcript types move out of `hel_chat`; reset-time helpers move out of `usage_format`; `hel_import` stops naming `ChatState`.
- [x] (2026-09-02 18:35Z) M1b: launch-config and MCP types move out of `hel_worker_runtime`; `terminate_process_group` moves to `hel_subprocess`; `harness_authentication_marker` moves to `hel_config`.
- [x] (2026-09-02 21:40Z) M1 verification: the edge script reports zero escaping edges in non-test and test code (after switching `src/hel_review/host.rs` to `hel_transcript`); `cargo clippy --all-targets -- -D warnings` clean; `cargo test` green (1663 core, 270 tui, 111 cli); committed.
- [x] (2026-09-02 18:04Z) M2: created `mj-worker/`, `mj-controller/`, `mj-chat/`; moved every module with `git mv`; `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings` and `cargo test` green over the default members; `--workspace` additionally builds `mj-desktop`, which this host cannot compile because the GTK development libraries are absent (a pre-existing environment limit, not a split effect); `cargo build --bin mj` and `cargo build --target x86_64-unknown-linux-musl --bin mj` link; the edge script prints two empty reports.
- [ ] M3: update CI, publish, coverage, release-version script, RELEASING.md, CONTRIBUTING.md; commit. The downstream half (rewriting `hel::` paths in `mj-tui`, `mj-cli`, `mj-desktop` and adding the new workspace dependencies) was done inside M2, because the workspace has to compile at the end of M2.
- [ ] M4: re-measure the three baseline timings and record them in `Outcomes & Retrospective`; push.


## Surprises & Discoveries


- Observation: the foundation is bigger than a naive "shared types" layer because the target-side worker replays relay events through the same projection code the daemon uses.
  Evidence: `src/hel_worker.rs` calls `crate::hel_projection::project_relay_event` and `apply_committed_projection_event`; `src/hel_projection.rs` in turn uses normalisation helpers from `src/hel_acp.rs`. So `hel_worker`, `hel_projection`, `hel_state`, `hel_acp`, and `hel_archive` all belong in the foundation.

- Observation: the review module is two things glued together. `host.rs` (3,238 lines) is the daemon-side review host and depends on the controller, session manager, and chat; `lanes.rs`, `delta.rs`, `driver.rs`, `verdict.rs`, and `mcp.rs` are protocol and target-side logic used by the worker runtime, and depend only on foundation modules.
  Evidence: per-file `crate::` imports listed in `Artifacts and Notes`.

- Observation: the edge script cannot see multi-line braced imports, so it under-reports. Its regex needs `crate::<module>::<word>`, and a `use crate::hel_worker_runtime::{` line ends in a brace, so three controller imports were invisible to it: `hel_controller/worker_binary.rs` and `hel_controller/reviewer.rs` pulled in the launch types plus the constants `DISCOVER_LOGIN_PATH_ENV`, `REVIEWER_DIR`, and `REVIEWER_PROFILE_DIR`. Those constants describe the worker root's file layout that the controller stages into, so they moved to `hel_worker_launch` with the types. Treat the script as a floor, not a ceiling: grep for the module name as well.

- Observation: `src/hel_review/bifrost.rs` also built `hel_worker_runtime::ReviewMcpServer`, which the layer table did not anticipate. It now names `hel_worker_launch`, so `bifrost.rs` no longer forces a foundation-to-worker edge and the M2 question of where it lands stays open.

- Observation: `hel_import` did not name `ChatState` for a type it could swap out. `canonical_import_session` built a whole `ChatState` from the events an importer synthesized, folded them into `ChatEntry` values, and converted those into a `MaterializedSession`; both of its callers are inside `hel_import`, so the function could not simply move up into `hel_chat`. The fix was to split the fold: the entry builders (`push_streamed_entry`, `apply_session_update_to_entries`, `apply_runtime_event_to_entries`, plus `ChatEntry::plain` and `ChatEntry::tool`) moved into `hel_transcript`, and the entry-to-session conversion became `hel_projection::materialized_session_from_entries`. `hel_chat` now delegates to the same builders, so the imported and the live transcript still come from one implementation. `hel_projection::imported_materialized_session` keeps the small event dispatch the importers need.
  Evidence: `src/hel_import.rs` `canonical_import_session`; `src/hel_chat.rs` `apply_adapter`, `apply_session_update_at`; `src/hel_transcript.rs`; `src/hel_projection.rs`.

- Observation: moving the three `materialized_*` text helpers pulled a larger set with them than the plan expected, because they sit on top of the chat view's ACP-to-text layer: `sanitize_terminal_text` (and its escape-consuming helpers, from `hel_chat/rendering.rs`), `content_block_text`, `tool_status`, `plan_status`, `tool_content_details`, `tool_diff_paths`, `tool_location_details`, the terminal-output summaries, and `format_diffstat`. None of them touch chat state, so they belong below it, and `hel_chat` re-exports the names it still uses.

- Observation: `src/hel_review/bifrost.rs` has two callers, not one. `hel_worker_runtime/reviewer.rs` builds its change packet and `hel_review/host.rs` calls `review_mcp_servers`. Moving it into the worker crate would therefore have forced a controller-to-worker edge, which is exactly the edge this split exists to remove, so it stayed in the foundation's `hel_review`.
  Evidence: `mj-worker/src/hel_worker_runtime/reviewer.rs` `changed_functions_packet`; `mj-controller/src/hel_review_host.rs` `hel::hel_review::bifrost::review_mcp_servers`.

- Observation: `include_str!` is the one thing a crate split cannot move freely. `hel_doctor::setup_instructions` embedded `../docs/PODMAN.md` and `../docs/DOCKER.md`, and from `mj-controller/src` that path escapes the package directory, which `cargo package` cannot carry. The docs stayed where humans and the README expect them and the foundation now exposes them as `hel_targets::PODMAN_DOCUMENTATION` and `DOCKER_DOCUMENTATION`, beside the `*_DOCUMENTATION_PATH` constants that already named the files; the root package's `include` list already carried `docs/`. The web, icon and font assets moved with `hel_server` into `mj-controller/src/`, so their relative paths still resolve.

- Observation: three tests re-exec the test binary with `--exact <name>` built from `module_path!()` with the `hel::` prefix stripped. In the controller crate the prefix is `mj_controller::`, so the child filter matched nothing and the parent failed with "the isolated broker retirement test never ran". Fourteen call sites needed the new prefix. A test that names its own crate is a hidden dependency on the crate layout.
  Evidence: `mj-controller/src/hel_controller/lifecycle.rs`, `checkpoint.rs`, `resume.rs`, `provisioning.rs`, `hel_session_manager.rs`.

- Observation: `env!("CARGO_MANIFEST_DIR")` is the same trap in integration tests. `tests/import_e2e.rs` read `scripts/*.sh` relative to the manifest directory; after the move to `mj-cli/tests/` that directory is the crate, not the repository, so the test now takes its parent.

- Observation: `iana-time-zone` is a direct dependency that no source file names. `chrono`'s `clock` feature pulls it regardless, so it stayed in the root manifest rather than being moved or dropped in a split commit.

- Observation: clean-build parallelism is a small win; edit scope is the real win. The front end is serial per crate and the crates chain foundation, then controller, then chat, so the critical path only shrinks from ~152K to ~140K lines. But a chat edit recompiles 23K lines instead of 152K.


## Decision Log


- Decision: keep `brokk-mj-core` (library `hel`) as the foundation crate rather than introducing a fifth "model" crate and turning core into a facade.
  Rationale: the foundation is the majority of what downstream crates import (`hel::hel_config`, `hel::hel_state`, `hel::hel_targets`, `hel::hel_database` account for most `hel::` uses in `mj-cli` and `mj-tui`), so keeping its name avoids most path churn. A facade would give every type two public paths, which is confusing and unnecessary.
  Date/Author: 2026-09-02, Fable with Jonathan.

- Decision: new crates are `brokk-mj-worker` (dir `mj-worker/`, lib `mj_worker`), `brokk-mj-controller` (dir `mj-controller/`, lib `mj_controller`), `brokk-mj-chat` (dir `mj-chat/`, lib `mj_chat`). Module names inside them keep their `hel_` prefixes (so the controller type is `mj_controller::hel_controller::Controller`).
  Rationale: this repo may re-merge with the sibling `mjolnir` tree; renaming modules would make that diff enormous for no behavioural gain. The double naming is ugly but mechanical and reversible.
  Date/Author: 2026-09-02, Fable.

- Decision: cut the cycle-forming edges inside the single crate first (M1), with the old paths kept as `pub use` re-exports where a caller in a not-yet-moved module still needs them, and only then create crates (M2).
  Rationale: M1 is safe, testable, and useful even if M2 stalls. Re-exports let M1a and M1b run in parallel on disjoint files.
  Date/Author: 2026-09-02, Fable.

- Decision: `WorkerLaunchConfig::enforce_execution_policy` stays in `hel_worker_runtime` as an inherent impl on a foundation type, rather than moving with the struct.
  Rationale: it translates a target's execution policy into harness environment variables, which is worker behaviour, and it is private to the runtime module tree (`unix.rs` and `relay_tests.rs` are its only callers). Rust allows an inherent impl anywhere in the crate that defines the type, and M2 will turn it into a free function or extension trait when the two modules land in different crates. Its serde and file-IO impls (`read`, `write`) did move, because they only touch `std::fs` and `hel_config::atomic_write`.
  Date/Author: 2026-09-02, Fable.

- Decision: `BrowserTranscript`, `BrowserTranscriptEntry` and `BrowserDiffStat` moved into `hel_server`, not into `hel_transcript`.
  Rationale: they carry no chat state, and the chat view builds them only to answer the browser API, so they are that API's wire shape. `hel_server` already owns every other response type on that route. Chat may depend on the controller, so `hel_chat/transcript.rs` imports them from `hel_server`; `mj-cli` now names `hel::hel_server::BrowserTranscript`.
  Date/Author: 2026-09-02, Fable.

- Decision: `materialized_session_from_entries` and `imported_materialized_session` landed in `hel_projection`, while the per-event entry builders landed in `hel_transcript`.
  Rationale: `hel_projection` already owns the conversions between a `MaterializedSession` and the canonical archive shape, and `hel_transcript` already owns `ChatEntry`. Splitting them this way kept each function beside the type it produces and avoided giving `hel_transcript` a dependency on `hel_state`'s session shapes.
  Date/Author: 2026-09-02, Fable.

- Decision: the controller crate must not depend on the worker crate.
  Rationale: this is what lets Cargo build the two in parallel. Everything the controller needs from `hel_worker_runtime` today is a handful of plain data types (`WorkerOwnership`, `WorkerLaunchConfig`, `ReviewerLaunchConfig`, `ProjectMemoryLaunchConfig`, `ProjectMemoryMcpDelivery`, `ReviewMcpServer`, `ReviewMcpDelivery`) plus one function (`terminate_process_group`); those move to the foundation in M1b.
  Date/Author: 2026-09-02, Fable.

- Decision: `hel_review/bifrost.rs` stays in the foundation instead of moving into `brokk-mj-worker`.
  Rationale: the review host calls `review_mcp_servers` from it, so a worker-crate home would make the controller depend on the worker. Its own dependencies are `hel_config` and `hel_worker_launch`, both foundation, so it sits in the foundation without pulling anything up.
  Date/Author: 2026-09-02, Fable.

- Decision: test fixtures needed across crates become `#[doc(hidden)] pub` rather than being gated on the `test-hooks` feature.
  Rationale: `test-hooks` is not a "expose test helpers" switch; it compiles the crash-boundary hooks of the reliability laboratory into the binary and changes runtime behaviour. Gating fixtures on it would force every ordinary `cargo test` run to enable it. Two items needed this: `hel_database::install_isolated_test_writer` and `hel_session_manager::replacement_session_test_fixture` (with its result type). Both are thin wrappers over code that is public anyway, so nothing test-only leaks into the library. The new crates still declare `test-hooks = ["hel/test-hooks"]` so `mj-cli` can turn the real hooks on through them.
  Date/Author: 2026-09-02, Fable.

- Decision: `WorkerLaunchConfig::enforce_execution_policy` became the free function `mj_worker::hel_worker_runtime::enforce_execution_policy(&mut config)` rather than an extension trait.
  Rationale: it has two callers, both inside `mj-worker`. A trait would add a name to import at each call site and a public surface to document for no gain.
  Date/Author: 2026-09-02, Fable.

- Decision: the two container-runtime documentation pages are embedded from the foundation (`hel_targets::PODMAN_DOCUMENTATION` and `DOCKER_DOCUMENTATION`) instead of moving `docs/PODMAN.md` and `docs/DOCKER.md` under `mj-controller/`.
  Rationale: `cargo package` cannot carry a file from outside the package directory, and these pages are human documentation that the README and `docs/SSH.md` link to; the repository's own convention reserves `docs/` for readers. The root package already lists them in its `include`, so publishing keeps working with no plumbing change.
  Date/Author: 2026-09-02, Fable.

- Decision: `tests/import_e2e.rs` moved to `mj-cli/tests/` and the four `scripts/test-*-import-e2e.sh` runners now pass `-p brokk-mjolnir`.
  Rationale: the test needs the controller, which the foundation crate cannot depend on, and it drives the `mj` binary through `CARGO_BIN_EXE_mj`, which only the crate that defines that binary provides.
  Date/Author: 2026-09-02, Fable.

- Decision: the M1 re-exports at the old paths were removed wherever no caller in the owning crate still needed the name, and demoted to plain `use` where one did.
  Rationale: a cross-crate `pub use` gives every type two public paths, which is what the plan set out to avoid. `hel_worker_runtime`'s launch-type and `terminate_process_group` re-exports are gone, `hel_setup::harness_authentication_marker` is now a private import with `mj-cli` naming `hel::hel_config`, and `hel_chat`'s `materialized_*` re-exports are `pub(super)` with `mj-tui` and `mj-cli` naming `hel::hel_transcript`.
  Date/Author: 2026-09-02, Fable.


## Outcomes & Retrospective


To be written at M4 with the before/after timing table.


## Context and Orientation


The workspace root `Cargo.toml` defines the package `brokk-mj-core` whose library is named `hel` (see the `[lib] name = "hel"` table). Its modules are declared in `src/lib.rs` as `pub mod hel_config;` and so on; each module is either a single file `src/<name>.rs` or a file plus a directory `src/<name>/`. Other workspace members are `mj-tui/` (the terminal UI, package `brokk-mj-tui`), `mj-cli/` (the `mj` binary, package `brokk-mjolnir`, which contains the daemon, the TUI host, the web server host, and the `mj worker` subcommand that runs inside containers), `mj-desktop/`, and `voice-worker/`. `mj-tui` and `mj-cli` refer to core modules as `hel::hel_config::Config` and similar.

"Layer" in this plan means a set of modules that may depend on lower layers but never on higher ones. The four layers, lowest first:

Foundation (stays in `brokk-mj-core`, library `hel`): `clock`, `termination`, `hel_config`, `hel_subprocess`, `hel_elicitation`, `hel_workspace`, `hel_transcript`, `hel_diff`, `hel_archive`, `hel_state`, `hel_projection`, `hel_worker` (the transport-neutral relay protocol core, despite its name), `hel_worker_protocol`, `hel_targets`, `hel_database`, `hel_second_opinion`, `hel_acp`, `hel_credentials`, `hel_skills`, `hel_project_memory`, `hel_test_hooks`, `hel_resources`, `hel_local_git`, `hel_checkpoint`, `hel_terminal`, and `hel_review` minus its `host.rs` and `bifrost.rs` files. Roughly 40,000 lines.

Worker (`brokk-mj-worker`): `hel_worker_runtime` (the target-side daemon and stdio proxy that runs inside a container or on an SSH host), `hel_user_shell`, and `hel_review/bifrost.rs` if only the runtime uses it. Roughly 10,000 lines.

Controller (`brokk-mj-controller`): `hel_controller`, `hel_session_manager`, `hel_server`, `hel_import`, `hel_worker_client`, `hel_quota`, `claude_usage`, `codex_usage`, `grok_usage`, `hel_doctor`, `hel_setup`, `hel_git_proxy`, `hel_recovery`, `hel_compaction`, `hel_tailscale`, `hel_desktop`, `hel_readline`, and `hel_review/host.rs` as a module named `hel_review_host`. Roughly 75,000 lines.

Chat (`brokk-mj-chat`): `hel_chat`, `speech`, `hel_text_input`, `hel_selection`, `usage_format`, `hel_clipboard`. Roughly 23,000 lines. Chat depends on the controller (it constructs a `Controller` for reviewer staging and uses `SessionManagerControl`).

An "escaping edge" is a `crate::x::y` reference from a module in one layer to a module in a higher layer, or between worker and controller in either direction. On 2026-09-02 the non-test escaping edges were exactly these:

    hel_acp -> hel_terminal::wait_for_exit                       (both foundation after this plan: fine)
    hel_acp -> hel_worker_runtime::{ReviewMcpServer, ReviewMcpDelivery}
    hel_compaction -> hel_chat::{materialized_content_text, materialized_chunks_text}
    hel_config -> hel_review::lanes::ReviewTier                  (both foundation: fine)
    hel_import -> hel_chat::ChatState
    hel_projection -> hel_chat::materialized_content_text
    hel_quota -> usage_format::{format_reset_local_seconds, normalize_reset_text, normalize_reset_epoch_seconds, format_reset_local}
    hel_review (host.rs) -> hel_chat::{materialized_chunks_text, materialized_content_text}
    hel_server -> hel_chat::BrowserTranscript
    hel_state -> hel_chat::materialized_content_text
    hel_worker -> hel_review::lanes::ReviewSubagentRequest       (both foundation: fine)
    hel_worker -> hel_worker_runtime::ReviewerLaunchConfig
    hel_worker_runtime -> hel_review::{lanes, driver::REVIEWER_ROLE, delta}   (worker -> foundation after host leaves review: fine)
    hel_worker_runtime -> hel_setup::harness_authentication_marker
    hel_checkpoint -> hel_worker_runtime::{WorkerLaunchConfig, ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery}
    hel_controller -> hel_worker_runtime::WorkerOwnership
    hel_review (host.rs) -> hel_worker_runtime::{ReviewMcpServer, ReviewerLaunchConfig}
    hel_session_manager -> hel_worker_runtime::ReviewerLaunchConfig
    hel_worker_client -> hel_worker_runtime::ReviewerLaunchConfig

and the test-only escaping edges were:

    codex_usage tests -> usage_format::format_reset_local_seconds
    hel_controller tests -> hel_worker_runtime::terminate_process_group
    hel_credentials tests -> hel_setup::harness_authentication_marker
    hel_database tests -> hel_chat::{materialized_tool_diffstats, materialized_content_text, materialized_chunks_text}
    hel_projection tests -> hel_chat::{materialized_chunks_text, materialized_content_text}
    hel_review (host.rs) tests -> hel_session_manager::{RemoteSessionPublisher, SessionManagerShutdown}, hel_worker_client::{RelayAttachment, StartedReviewer}, hel_worker_runtime::{ReviewerLaunchConfig, ReviewMcpServer}
    hel_server tests -> hel_chat::BrowserTranscriptEntry
    hel_terminal -> hel_worker_runtime::terminate_process_group

Cargo does not allow a dev-dependency cycle between crates, so test-only edges matter as much as non-test ones.


## Plan of Work


M1a (chat-side helpers; may run in parallel with M1b because the file sets are disjoint). Move `materialized_content_text`, `materialized_chunks_text`, and `materialized_tool_diffstats` from `src/hel_chat.rs` (or wherever in `src/hel_chat/` they are defined) into `src/hel_transcript.rs`, which is a foundation module that already owns transcript text shapes. Leave `pub use crate::hel_transcript::{...}` re-exports at the old location in `hel_chat` so any caller not touched in M1a still compiles; M2 removes the re-exports when `hel_chat` moves. Update the callers in `src/hel_database.rs` and `src/hel_database/tests.rs`, `src/hel_state.rs`, `src/hel_projection.rs` and its tests, and `src/hel_compaction.rs` to import from `hel_transcript`. Do not edit `src/hel_review/host.rs`; it keeps using the re-export and M2 fixes it when the file moves. Move `BrowserTranscript` and `BrowserTranscriptEntry` (defined in `hel_chat`, consumed by `src/hel_server.rs`) into `hel_server` if `hel_chat` only constructs them for the server, else into `hel_transcript`; the deciding question is whether the types reference any `hel_chat` state. Find the one use of `crate::hel_chat::ChatState` in `src/hel_import.rs` and either move that function into `hel_chat` (if it builds chat state from an import) or replace the dependency with the foundation type it actually needs. Move the four reset-time helpers `format_reset_local_seconds`, `normalize_reset_text`, `normalize_reset_epoch_seconds`, and `format_reset_local` from `src/usage_format.rs` into `src/hel_quota.rs` (a controller module that `usage_format` may depend on, since chat sits above the controller) and have `usage_format` and `src/codex_usage.rs` import them from there.

M1b (worker-runtime types). Create a new foundation module `src/hel_worker_launch.rs`, declared in `src/lib.rs`, and move these definitions from `src/hel_worker_runtime.rs` into it with their impls and serde derives intact: `WorkerOwnership`, `WorkerLaunchConfig`, `ReviewerLaunchConfig`, `ProjectMemoryLaunchConfig`, `ProjectMemoryMcpDelivery`, `ReviewMcpServer`, `ReviewMcpDelivery`, and any small helper types they embed. Leave `pub use crate::hel_worker_launch::{...}` re-exports in `hel_worker_runtime` so untouched callers compile. Update the callers in `src/hel_worker.rs`, `src/hel_acp.rs`, `src/hel_checkpoint.rs`, `src/hel_controller/` (the `WorkerOwnership` use), `src/hel_session_manager.rs`, `src/hel_worker_client.rs`, and `src/hel_review/host.rs` (import lines only) to use `hel_worker_launch`. Move `terminate_process_group` from `hel_worker_runtime` (its body is in `src/hel_worker_runtime/unix.rs`) into `src/hel_subprocess.rs`, with a re-export at the old path, and update `src/hel_terminal.rs` and the controller test that uses it. Move `harness_authentication_marker` from `src/hel_setup.rs` into `src/hel_config.rs`, re-export from `hel_setup`, and update `src/hel_worker_runtime.rs` and the `hel_credentials` test.

M1 verification. Run the edge script from `Artifacts and Notes` with the final layer assignment; it must print no escaping edges in either the non-test or the test report. Note that re-exports hide edges from the script only when the importer names the old path, so the script is also the check that every caller was actually updated. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`. Commit as one commit per milestone half.

M2 (crates). Create `mj-worker/Cargo.toml`, `mj-controller/Cargo.toml`, `mj-chat/Cargo.toml` mirroring the root manifest's style (`version.workspace = true`, `edition = "2024"`, `license`, `repository`, `publish = true`, `[lints] workspace = true`), each with `[lib] name = "mj_worker"` and so on. Add them to `[workspace] members` and `default-members` and to `[workspace.dependencies]` alongside the existing `hel` and `hel-tui` entries, using the same `{ path, package, version = "2.0.0" }` shape so `node scripts/release-version.mjs check` keeps working. Each new crate depends on `hel` (the foundation) and, for chat, on `mj_controller`. The foundation feature `test-hooks` must be forwarded: each new crate declares `test-hooks = ["hel/test-hooks"]` and `mj-cli` enables it through whichever crate it already enables it on. `git mv` the module files into `mj-worker/src/`, `mj-controller/src/`, `mj-chat/src/`, declare them in each crate's `src/lib.rs`, and delete the declarations from the root `src/lib.rs`. `src/hel_review/host.rs` becomes `mj-controller/src/hel_review_host.rs` (fix its `super::` references to name `hel::hel_review::` items). Inside moved files, rewrite `crate::<module>` to `hel::<module>` when the module stayed in the foundation, to `mj_controller::<module>` when a chat file names a controller module, and leave `crate::` when both sides moved to the same crate. Remove the M1 re-exports that are now unreachable. Split `[dependencies]` in the root manifest: each external crate goes to every crate that uses it (`grep -l` for the crate name per directory decides); the root keeps only what foundation modules use. Run `cargo check --workspace --all-targets` until green. Colocated tests move with their modules; fixture code shared across layers (for example `hel_session_manager::replacement_session_test_fixture` used by chat tests) stays in the crate that owns the type and is exposed under `#[cfg(any(test, feature = "test-hooks"))]` or a `pub fn` behind `#[doc(hidden)]`, whichever the existing `test-hooks` feature already uses for similar cases.

M3 (downstream and release plumbing). In `mj-tui/`, `mj-cli/`, and `mj-desktop/`, rewrite `hel::<module>` to `mj_worker::<module>`, `mj_controller::<module>`, or `mj_chat::<module>` according to the layer table, add the new crates to their `[dependencies]` via the workspace entries, and leave foundation paths untouched. Update `.github/workflows/ci.yml` (the `cargo package -p brokk-mj-core` steps need equivalents for the three new crates, or a loop), `.github/workflows/coverage.yml` (add `-p` flags for the new crates to both `llvm-cov report` lines), `.github/workflows/publish.yml` (the dependency-ordered publish loop becomes `brokk-mj-voice-worker brokk-mj-core brokk-mj-worker brokk-mj-controller brokk-mj-chat brokk-mj-tui brokk-mj-desktop brokk-mjolnir`; the comment about dependency order must be rewritten), `scripts/release-version.mjs` (add the new path dependencies to whatever list it checks), `scripts/check-coverage.mjs` (module path lists now span four `src/` trees), `RELEASING.md` (publish order and the sentence listing published crates), and `CONTRIBUTING.md` (the paragraph explaining the crate layout, if any; add one if absent). Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `node scripts/release-version.mjs check v2.0.0`. Commit.

M4 (measure). Repeat the three baseline commands from `Artifacts and Notes` on the same host and record the results next to the baseline in `Outcomes & Retrospective`. Also record `cargo build --timings` output showing `brokk-mj-worker` and `brokk-mj-controller` overlapping. Push to `origin master`.


## Concrete Steps


All commands run from the repository root `/home/jonathan/Projects/hel`. `cargo test` must run outside the restricted sandbox because the suite opens loopback sockets. The host builds for its native target since 2026-09-02; the container worker needs `--target x86_64-unknown-linux-musl` explicitly, which is unaffected by this plan.

Edge check (run after M1 and after M3; adapt the `mods` discovery to read every crate's `src/lib.rs` after M2):

    python3 .agents/plans/split-core-edges.py

The script is stored next to this plan and its expected output after M1 is two empty reports:

    NON-TEST ESCAPING EDGES:
    TEST-ONLY ESCAPING EDGES:

Validation per milestone:

    cargo fmt
    cargo clippy --all-targets -- -D warnings
    cargo test

Baseline and M4 timings (run each after a warm `cargo build --bin mj`):

    touch src/hel_chat.rs   # after M2: mj-chat/src/hel_chat.rs
    /usr/bin/time -f 'wall %e s' cargo build --bin mj
    touch src/hel_chat.rs
    /usr/bin/time -f 'wall %e s' cargo test -p brokk-mj-core --no-run   # after M2: -p brokk-mj-chat
    touch src/hel_worker_runtime.rs   # after M2: mj-worker/src/hel_worker_runtime.rs
    /usr/bin/time -f 'wall %e s' cargo build --bin mj


## Validation and Acceptance


After M1: `cargo test` reports the same number of tests passing as before (1,642 in `brokk-mj-core` on 2026-09-02, plus the `mj-tui` and `mj-cli` suites), clippy is clean, and the edge script prints two empty reports.

After M3: `cargo test` passes with the same total test count spread over four core-derived crates; `cargo build --bin mj` produces a binary whose `mj --version` prints `2.0.0`; `cargo build --target x86_64-unknown-linux-musl --bin mj` still links (this is the container worker build); `node scripts/release-version.mjs check v2.0.0` passes; starting the TUI with `./target/debug/mj` shows the session list as before.

After M4: the chat-touch rebuild of `mj` and the chat test binary are both materially faster than the baseline on the same host, and `cargo build --timings` shows the worker and controller crates compiling concurrently.


## Idempotence and Recovery


Every milestone ends in a commit on `master`. M1a and M1b are independent and can be reverted individually with `git revert`. M2 is one large mechanical commit; if it fails midway, `git checkout -- . && git clean -fd mj-worker mj-controller mj-chat` returns to the M1 state. Do not push until M4's numbers are recorded, so a bad split never reaches `origin`.


## Artifacts and Notes


Baseline timings, 2026-09-02, load average between 30 and 90 on a 120-core host, warm target directory, host target:

    touch src/hel_chat.rs -> cargo build --bin mj                      22.30 s
    touch src/hel_chat.rs -> cargo test -p brokk-mj-core --no-run       15.03 s
    touch src/hel_worker_runtime.rs -> cargo build --bin mj             12.42 s
    libhel rlib size                                                     235 MB

Full non-incremental build of the core lib, `-Ztime-passes`, 2026-09-02:

    coherence_checking 5.0 s, type_check_crate 11.3 s, MIR_borrow_checking 8.0 s,
    generate_crate_metadata 9.2 s, codegen_to_LLVM_IR 7.8 s, LLVM_passes 7.1 s, total 43.1 s

Per-file `crate::` imports of `src/hel_review/` (non-test), which justify splitting `host.rs` away:

    bifrost.rs: config worker_runtime
    delta.rs:   archive worker
    driver.rs:  second_opinion worker
    host.rs:    chat config controller database projection second_opinion session_manager state worker worker_runtime
    lanes.rs, verdict.rs, hel_review.rs: none
    mcp.rs:     project_memory

External crates used per layer on 2026-09-02, computed by grepping each module's sources for `<crate>::` and `use <crate>` (rename hyphens to underscores). Use this to split the root `[dependencies]` in M2; `iana-time-zone`, `qrcode`, and `sysinfo` were not found by name in any module, so check `Cargo.toml` comments and `build.rs` before dropping them:

    worker (mj-worker):        agent-client-protocol anyhow base64 libc serde serde_json tempfile tokio tracing
    controller (mj-controller): agent-client-protocol anyhow axum axum-server base64 chrono dirs getrandom hmac
                                http-body-util libc rayon reedline reqwest rusqlite serde serde_json sha2 tempfile
                                tokio tokio-stream tokio-util tower tracing url
    chat (mj-chat):            agent-client-protocol anyhow arboard chrono crossterm dirs pulldown-cmark ratatui
                                serde serde_json sha2 textwrap tokio tracing unicode-segmentation unicode-width
    foundation keeps:          agent-client-protocol anyhow base64 blake3 chrono dirs flate2 getrandom libc proptest
                                rayon regex rusqlite serde serde_json serde_yaml sha2 signal-hook similar tar
                                tempfile tokio tokio-util toml tracing url zip

Release and coverage plumbing that hard-codes the crate list: `scripts/release-version.mjs` line 21 `const internalPackages = ["hel", "hel-tui"];` (add `mj-worker`, `mj-controller`, `mj-chat` as `[workspace.dependencies]` keys); `scripts/check-coverage.mjs` lists source roots (`src/`, `mj-cli/src/`, `mj-tui/src/`, `voice-worker/src/`) and validates `module_exceptions` keys in the coverage baseline against reported module paths, so every exception keyed on a moved file must be re-keyed; `.github/workflows/coverage.yml` passes `-p` per crate to `llvm-cov report`; `.github/workflows/publish.yml` publishes in dependency order; `.github/workflows/ci.yml` packages `brokk-mj-core` explicitly.

Downstream `hel::` reference counts before M3 (module = count): mj-tui: hel_config 33, hel_targets 22, hel_state 15, hel_chat 12, usage_format 11, hel_selection 9, hel_worker 8, hel_text_input 7, hel_quota 6, hel_controller 3; mj-cli: hel_database 50, hel_server 45, hel_config 44, hel_chat 32, hel_state 29, hel_worker 21, hel_credentials 21, hel_targets 15, hel_session_manager 13, hel_controller 12, and a long tail; mj-desktop: hel_subprocess, hel_server, hel_desktop, hel_config once each.


## Interfaces and Dependencies


After M1b, `src/hel_worker_launch.rs` defines (moved verbatim, signatures unchanged):

    pub struct WorkerOwnership { ... }
    pub struct WorkerLaunchConfig { ... }
    pub struct ReviewerLaunchConfig { ... }
    pub struct ProjectMemoryLaunchConfig { ... }
    pub enum ProjectMemoryMcpDelivery { ... }
    pub struct ReviewMcpServer { ... }
    pub enum ReviewMcpDelivery { ... }

and `src/hel_subprocess.rs` gains:

    pub fn terminate_process_group(pid: u32, signal: libc::c_int)   // exact signature as today

After M1a, `src/hel_transcript.rs` defines `materialized_content_text`, `materialized_chunks_text`, `materialized_tool_diffstats` with their current signatures, and `src/hel_quota.rs` defines the four reset-time helpers with their current signatures.

After M2, the crate graph is:

    brokk-mj-core (hel)  <-  brokk-mj-worker (mj_worker)
    brokk-mj-core (hel)  <-  brokk-mj-controller (mj_controller)  <-  brokk-mj-chat (mj_chat)
    brokk-mj-tui  depends on hel, mj_controller, mj_chat
    brokk-mjolnir (mj-cli) depends on hel, mj_worker, mj_controller, mj_chat, hel-tui
    brokk-mj-desktop depends on hel, mj_controller

`brokk-mj-controller` must not list `brokk-mj-worker` as a dependency or dev-dependency.
