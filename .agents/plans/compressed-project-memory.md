# Remove project-memory snapshot caps and compress relay transfers

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Move, Suspend and recovery must keep working when project memory grows beyond one MiB. Remove that cap and compress transferred trees, preserving compatibility with older workers. A relay is the controller-to-worker connection; its protocol number determines the representation sent.

## Progress

- [x] (2026-10-25) Trace snapshot validation, blocking IO, relay framing and protocol negotiation.
- [x] (2026-10-25) Remove snapshot budget and implement protocol-44 gzip wire trees.
- [x] (2026-10-25) Add large-tree and mixed-protocol regression coverage.
- [x] (2026-10-25) Focused tests, touched-crate clippy, formatting and diff checks passed; remaining transport bounds recorded.

## Surprises & Discoveries

Memory trees are connection-only and never enter durable command or event budgets. Relay frames still have an eight MiB ceiling, including JSON/base64 overhead and both trees in a snapshot response.

## Decision Log

The parent specified unlimited snapshots, gzip and protocol 44 on 2026-10-25. Use an untagged wire enum: legacy `{ "files": {...} }` or `{ "gzip": "..." }`, where gzip contains JSON of the files map. Keep compression out of serde so all expansion/compression runs in explicit blocking jobs. Preserve validation of paths and nonempty documents. No new dependencies or persisted formats are required. Do not commit, per the delegation.

## Outcomes & Retrospective

Snapshot caps are removed, protocol-44 compression is implemented, and docs are corrected. Final core memory tests (12), core relay tests (10), worker memory tests (7), controller worker-client tests (34), controller memory-merge tests (7), final worker encoding/protocol checks (one each), and touched-crate clippy pass. No failures. Logs and exact remaining errors/bounds are under `.mj/agents/e4df78407c9a8aa6773c967114a75525/`.

## Context and Orientation

`mj-core/src/project_memory.rs` owns filesystem snapshots and validation. `mj-core/src/relay/protocol.rs` owns relay messages. `mj-worker/src/worker_runtime/unix/requests.rs` executes connection-only memory requests in serialized blocking jobs. `mj-controller/src/worker_client/relay.rs` negotiates the outbound encoding and expands replies in blocking jobs. `mj-controller/src/project_memory_merge.rs` synchronizes canonical memory with worker replicas.

## Plan of Work

First remove only the snapshot byte budget from collect/install/replace. Add `ProjectMemoryWireTree` with explicit encode/decode methods using existing flate2 and base64 dependencies and advance the relay version to 44. Update worker handlers and controller calls, keeping the worker always compressed and the controller plain below 44. Next extend filesystem/worker regressions beyond three MiB and add a controller test against independently encoded protocol-43/44 replies. Review launcher checks and documentation for current-version assumptions.

## Concrete Steps

From the repository root, run elevated `cargo test -p brokk-mj-core project_memory`, `cargo test -p brokk-mj-core relay`, `cargo test -p brokk-mj-worker project_memory`, `cargo test -p brokk-mj-controller project_memory_merge`, and `cargo test -p brokk-mj-controller worker_client`. Run `cargo clippy -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller --all-targets -- -D warnings` and `cargo fmt --all -- --check`. Preserve existing isolated test data directories; do not start live instances or alter Cargo targets. Save text logs under `.mj/agents/e4df78407c9a8aa6773c967114a75525/`.

## Validation and Acceptance

A tree over three MiB must survive install, collect and replace. Controller requests to protocol 43 must match the former plain JSON shape; protocol 44 requests must carry gzip, and either response must decode identically. Worker replies must be compressed and accept either request encoding. Focused suites and clippy must pass; report baseline failures and rerun any suspected flake alone.

## Idempotence and Recovery

All tests use temporary data and may be repeated. No production store, session, or build configuration is changed. Review the diff without staging or committing.

## Artifacts and Notes

Remaining bounds and exact error paths are collected in the report directory, along with test output.

## Interfaces and Dependencies

`ProjectMemoryWireTree::encode(ProjectMemorySnapshot, u32) -> anyhow::Result<Self>` selects plain or gzip. `decode(self) -> anyhow::Result<ProjectMemorySnapshot>` accepts either. `RELAY_COMPRESSED_PROJECT_MEMORY_PROTOCOL` is 44; `RELAY_PROTOCOL_VERSION` references it. Compression uses flate2's existing Rust backend and standard base64 with no inflated-size bound.

Initial plan records the delegated design and implementation checkpoints. Updated after implementation to record completed regression coverage and initial successful checks.

Completion update: all focused checks passed. A feature-test comment was corrected after validation; executable code was unchanged. No commit was made. Live production sessions and full workspace suites were not exercised.
