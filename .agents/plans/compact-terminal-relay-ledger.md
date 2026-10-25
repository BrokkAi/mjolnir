# Compact terminal relay commands

This plan follows `.agents/PLANS.md`. The worker owns durable relay state: commands, their acceptance ordinals, execution dispatches, and completion outcomes. Long-running delegating sessions currently retain two full copies of each terminal command and can exceed the snapshot byte budget, preventing restart.

## Purpose and acceptance

Workers must reopen old large snapshots and retain lost-acknowledgement deduplication without retaining terminal command bodies. A regression submits more than 512 large commands, prunes history, reopens the worker, and retries a retained command without executing twice. A legacy pretty-printed snapshot exceeding the normal write budget must reopen and persist compactly.

## Progress

- [x] 2026-10-25: traced readers of both ledger maps and the snapshot loader/writer.
- [x] 2026-10-25: implemented terminal receipt identity, compact persistence, bounded legacy reads, and revision 20.
- [x] 2026-10-25: validated legacy recovery, 405,252-byte incident-copy snapshot, relay regressions, full touched-crate suites, formatting, and workspace clippy; unrelated Node lease flake remains under parallel tests and passes alone.

## Surprises & Discoveries

Lost-ACK retries compare full commands except mailbox deliveries, which compare event keys. Legacy retry assessment also needs prompt classification and whether a newer command cancels automated recovery. Relay-local history releases can fail after completion; their retry must still retry garbage collection. Controller production code does not inspect ledger entries. Steering transcript summaries used to read the delivered target from the post-event dispatch map; capture that text before publication so it survives body removal. The first relay run exposed old terminal-dispatch assertions, which now inspect receipts instead. An untouched Node lease test passed when rerun alone after failing under the parallel suite.

## Decision Log

Keep active handled commands unchanged inside an optional command field. Terminal receipts retain a canonical SHA-256 command identity, mailbox identity where applicable, and the two legacy assessment predicates. Remove terminal dispatches after every event finishes all state transitions. Preserve history-release retry behavior using receipt metadata. Raise relay state revision because older writers cannot read missing command bodies. Allow legacy reads up to four times the write budget; writes and staging both use compact JSON.

## Context and implementation milestones

First update `mj-core/src/relay/snapshot.rs` and `snapshot/apply.rs`: add receipt identity, compact terminal records after applying all event side effects, and teach legacy assessment to read receipt predicates. Update worker retry admission in `mj-worker/src/relay/commands.rs`, including completed history-release retries. Then update `mj-worker/src/relay.rs` loading and `relay/journal.rs` persistence to migrate legacy snapshots before serving work. Read caps bound allocation while allowing oversized legacy pretty JSON.

Finally extend existing journal regression coverage. Use temporary worker directories only. Copy the incident snapshot outside its read-only live directory and run the same compaction path on that copy. No live worker writes, commits, or pushes are authorized.

## Validation

Run `cargo test -p mj-worker relay --no-fail-fast` and `cargo test -p mj-core relay --no-fail-fast` outside the restricted sandbox, then full touched-crate suites and `cargo clippy -p mj-worker -p mj-core --all-targets -- -D warnings`. Run `cargo fmt --all -- --check`. Record output in the assigned report directory. Every daemon test retains its existing isolated data configuration.

## Recovery and artifacts

Snapshot migration is forward-only and written atomically using the existing writer; journal digests and frontiers do not change. Test roots are temporary. Logs and size evidence belong in `.mj/agents/d8c8beb5777aa8fee685a55e98ac1b61/`.

## Outcomes & Retrospective

The 256 relay-module tests pass, including oversized legacy loading and the 512-entry large-body retry regression. The copied incident worker opens and persists a 405,252-byte snapshot, down from 16,730,556 bytes. The live directory is untouched. Full final validation is complete: core and all worker integration/relay tests pass; the worker library has an unrelated Node lease flake that passes alone. Workspace clippy and formatting pass. The untouched lease test also passed alone on a clean HEAD source-archive build.

Update 2026-10-25: implementation complete; regressions and existing suites are in progress. Preserved steering summary ordering after the first regression run exposed its dependence on terminal dispatch bodies.

Update 2026-10-25: incident-copy startup succeeded and all relay-module regressions passed. Canonicalize JSON object order before hashing so equal commands with reordered extension metadata remain equal retries. Full suites exposed an unrelated untouched launcher ETXTBSY flake, which passed alone; final full validation is running against frozen implementation source.

Update 2026-10-25: final validation and clean HEAD baseline checks complete. Both unrelated flaky tests pass alone on the final implementation and unmodified HEAD. No commits or pushes were made; the parent owns acceptance and publication.
