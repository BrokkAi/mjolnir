# Let controller and UI crates build independently

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Local development currently follows the crate chain `brokk-mj-core -> brokk-mj-controller -> brokk-mj-chat -> brokk-mj-tui -> brokk-mjolnir`. An implementation-only edit in the controller therefore recompiles the conversation and terminal layers even though they consume only a small client-facing contract. This refactor introduces `brokk-mj-client` as that contract. Controller and chat will both depend on it, allowing Cargo to compile the controller branch independently from the chat/TUI branch and keeping chat/TUI fresh after controller-only edits. Runtime behavior, persistence, worker ownership, wire formats, and user-facing output must remain unchanged.

## Progress

- [x] (2026-09-09) Audited the manifests and every `mj_controller::` use in `mj-chat` and `mj-tui`; selected a client-contract boundary and deferred a core split.
- [x] (2026-09-09) Created `brokk-mj-client` and moved shared display models, pure helpers, media helpers, and dependency plumbing behind compatibility re-exports.
- [x] (2026-09-09) Added UI-facing session and reviewer-staging adapters while keeping actors, leases, recovery, and blocking staging work in controller.
- [x] (2026-09-09) Removed every direct controller dependency and source reference from chat and TUI; moved their replacement-actor fake into the client crate.
- [x] (2026-09-09) Ran focused and full validation and measured a warm controller-only rebuild; the implementation is ready for its final commit and push.

## Surprises & Discoveries

- Observation: a prior four-crate split already improved chat and worker rebuilds, but recorded that the remaining clean-build critical path is foundation, controller, then chat.
  Evidence: `.agents/plans/split-core-into-layered-crates.md` records the dependency chain and its 2026-09-02 timings.

- Observation: `hel_session_manager.rs` combines a small UI-facing handle with roughly 5,000 lines of controller-owned actor, lease, recovery, projection, and subprocess behavior.
  Evidence: chat calls state observation, submit, sync, elicitation, reviewer, and reacquisition methods; controller lifecycle code additionally calls `lease_connection` and owns `StandaloneSession`.

- Observation: the attempted current timing run was contaminated by several unrelated concurrent Cargo/rustc jobs on the shared 120-core host.
  Evidence: `ps -C cargo -C rustc` showed several builds running for minutes. The diagnostic build was stopped after it reached core, controller, then overlapping chat/TUI compilation; no baseline number is claimed.

- Observation: after warming the normal development build and confirming no other Cargo/rustc jobs were running, touching `mj-controller/src/hel_quota.rs` rebuilt only `brokk-mj-controller` and `brokk-mjolnir`.
  Evidence: `cargo build --locked --bin mj --timings` finished in 23.03 seconds and did not compile `brokk-mj-client`, `brokk-mj-chat`, or `brokk-mj-tui`. The report was written under `target/cargo-timings/`.

- Observation: the first broad `cargo test` invocation passed all library and ordinary integration suites but the six-test PTY binary missed its five-second startup marker while the host was cold. Five of six passed on an isolated serial retry; the remaining test passed on immediate isolated retry, and then the complete PTY suite passed 6/6 serially.
  Evidence: the failures emitted either no dashboard bytes or only the terminal capability query before the fixed five-second deadline; there was no behavior assertion failure once startup completed.

- Observation: an additional `cargo test --workspace` attempt cannot compile the optional desktop member on this host because the GLib/GTK/WebKit pkg-config development packages are not installed.
  Evidence: the build scripts for `glib-sys`, `gdk-sys`, `javascriptcore-rs-sys`, and related crates reported missing system `.pc` files. CI installs these native packages; this source refactor does not alter the desktop dependency graph.

## Decision Log

- Decision: add `brokk-mj-client` instead of expanding `brokk-mj-core`.
  Rationale: core is the worker-facing foundation and already contains about 80,000 Rust source lines. The client boundary has a distinct ownership and compilation purpose, and image/authentication helpers should not enter worker builds.
  Date/Author: 2026-09-09, Codex with Jonathan.

- Decision: optimize local rebuilds first and defer splitting core.
  Rationale: the user selected local edit-to-binary latency and the controller/UI edge as the first pass. This removes a proven invalidation edge with less state-machine risk than reorganizing persistence and worker internals.
  Date/Author: 2026-09-09, Codex with Jonathan.

- Decision: keep session actors, connection leases, lifecycle channels, recovery, and shutdown in controller; expose the UI subset through object-safe client adapters.
  Rationale: moving the whole session manager would relocate controller implementation rather than create a small stable contract, and would put concurrency-sensitive ownership in a broadly shared crate.
  Date/Author: 2026-09-09, Codex.

## Outcomes & Retrospective

The new client crate is a real compilation and publication boundary. Chat and TUI now depend on `brokk-mj-client` and contain no controller references, while the final CLI joins the controller and UI branches. The controller keeps the concurrency-sensitive implementation and exposes narrow client adapters; old controller model paths remain as compatibility re-exports.

The acceptance case is proven: a warm controller implementation touch rebuilt only controller and the final binary in 23.03 seconds, leaving client, chat, and TUI fresh. Client tests passed 7/7, the new controller adapter test passed, the broad default-member test run passed all non-PTY suites, and the isolated PTY suite passed 6/6 after one transient startup-deadline retry. Clippy passed with warnings denied, the portable x86-64 musl worker built, release versions and package contents checked, the generated license report matched, and the configured cargo-deny license check passed.

This improves local controller-edit latency but does not shorten the shared-core critical path. A core edit still invalidates both branches through the client/foundation boundary; splitting core remains deliberately deferred until measurements identify a similarly clear low-risk boundary.

## Context and Orientation

The root `Cargo.toml` defines `brokk-mj-core` with library name `hel`. `mj-controller/` owns daemon services and the session actors. `mj-chat/` owns conversation state and rendering. `mj-tui/` owns the combined terminal surface. `mj-cli/` is the final binary and may depend on both branches. The new `mj-client/` package is a publication and compilation boundary for the contract consumed by control surfaces.

The client contract includes display data such as review, quota, transcript, and web-viewer models; pure target compatibility; bounded image conversion; credential availability discovery; and abstract session operations. Controller retains HTTP/provider work, quota collection, provisioning, reviewer staging implementation, actor queues, connection leasing, checkpointing, and recovery.

## Plan of Work

Create `mj-client/Cargo.toml` and modules organized by behavior rather than by the controller files they came from. Add the package to workspace membership, workspace dependencies, release version synchronization, package checks, publication order, licensing, and human crate-layout documentation.

Move shared models and pure helpers into the client crate and re-export them from their old controller modules so controller and CLI call sites can migrate incrementally. Update chat and TUI to name the client crate directly. Keep serialization attributes, redacted `Debug`, error strings, size limits, and pure calculations byte-for-byte compatible where practical.

Define client-owned `SessionHandle` and `SessionControl` wrappers backed by object-safe traits whose asynchronous methods return boxed futures. Each wrapped handle owns its own state receiver so `changed(&mut self)` retains the current per-consumer watch semantics. Define reviewer action/result payloads in the contract. Implement adapters in controller over the existing concrete handles, forwarding directly to the existing bounded channels and pending-operation types. Do not add a task between the UI and the actor queues.

Define a client-owned reviewer-staging interface. Controller implements it with its current `Controller::stage_reviewer_profile`; chat invokes it from the existing `spawn_blocking` task. Pass this capability through chat preparation/construction from CLI. Tests use hand-written client fakes; controller keeps adapter integration tests against real actor channels.

## Concrete Steps

Run commands from `/home/jonathan/Projects/hel`. Use `cargo metadata --no-deps`, `cargo tree`, and targeted `rg` checks while integrating. Run all Cargo tests outside the restricted sandbox as required by `AGENTS.md`.

After implementation, run:

    cargo fmt --all -- --check
    cargo test --workspace
    cargo clippy --all-targets -- -D warnings
    cargo build --locked --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    node scripts/release-version.mjs check v2.5.0

Regenerate the license report if dependency ownership changes its generated content, and verify package file lists include `mj-client/LICENSE`.

For the acceptance measurement, warm the normal development build, confirm no unrelated Cargo/rustc jobs are running, touch a controller implementation file, and run `cargo build --locked --bin mj --timings`. Record both elapsed time and the workspace units Cargo rebuilt. Broader three-case medians remain useful for a future core-splitting decision, but are not required to prove this refactor removed the controller-to-UI invalidation edge. Do not claim measurements taken while unrelated builds saturate the shared host.

## Validation and Acceptance

`cargo tree -p brokk-mj-chat -e normal,dev` and the equivalent TUI command must contain no `brokk-mj-controller`. A controller-only edit followed by `cargo build --bin mj --timings` must not compile `brokk-mj-chat` or `brokk-mj-tui`. A core edit must allow controller compilation to overlap the chat/TUI branch once the client contract is available.

Behavior tests must preserve ordered prompt delivery, deferred prompts during leases, replacement-actor reacquisition, retirement behavior, elicitation delivery, reviewer launch and journals, chat draft preservation, image size/format limits, resume compatibility messages, quota formatting, and serialized browser/web-viewer responses. Full tests and Clippy must pass with warnings denied. The standalone portable worker must still compile without client-only dependencies.

## Idempotence and Recovery

The refactor is source-only and introduces no database or protocol migration. Work in coherent commits on the current branch. Stage only files changed for this refactor and preserve the unrelated untracked plan. If an extraction does not converge, restore compatibility through the old controller re-export while fixing the client implementation; do not weaken session ordering or lifecycle behavior.

## Artifacts and Notes

The dependency audit can be repeated with:

    rg -n 'mj_controller::|use mj_controller' mj-chat/src mj-tui/src
    cargo tree -p brokk-mj-chat -e normal,dev
    cargo tree -p brokk-mj-tui -e normal,dev

The pre-change source sizes were approximately 79,781 lines in core, 73,693 in controller, 37,046 in chat, and 38,323 in TUI, including colocated tests.

## Interfaces and Dependencies

`mj_client::session` owns cloneable `SessionHandle` and `SessionControl` wrappers, the object-safe implementation traits used by controller and test fakes, `ManagedSessionView`, `ViewError`, reviewer requests/results, pending submit/sync wrappers, and `new_command_id`. Methods retain the signatures and completion guarantees that chat currently receives from controller concrete types.

`mj_client::session::ReviewerStager` synchronously returns `anyhow::Result<ReviewerLaunchConfig>` from the current config/session/profile/generation inputs and must be `Send + Sync`; chat calls it only from `spawn_blocking`.

The client crate depends on `hel`, `anyhow`, `agent-client-protocol`, `anvil-client`, `getrandom`, `image`, `serde`, `tokio`, and `tracing` only where its final modules require them. It does not depend on controller, chat, TUI, Axum, Reqwest, Rusqlite, or process-management libraries.

Revision note (2026-09-09): Created the implementation plan from the accepted design, recording the existing dependency audit, user-selected priorities, adapter boundary, validation, and measurement constraints.

Revision note (2026-09-09): Closed the implementation and validation milestones, recorded the warm controller-only rebuild and PTY startup retry, and corrected the final client interface/dependency inventory.
