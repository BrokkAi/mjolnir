# Add Antigravity ACP sessions

This living plan follows .agents/PLANS.md.

## Purpose / Big Picture

Enable explicit Antigravity profiles using a host-installed agy and isolated copies of Google credentials. ACP is the JSON RPC protocol between worker and harness.

## Progress

- [x] 2026-11-12: Read integration map and empirical agy 1.5.6 findings.
- [x] 2026-11-12: Implement identity, ambient executable lookup, token staging/freshness, selectors, permissions, instructions, stdio MCP and exhaustive handling. Store changes are separately owned.
- [x] 2026-11-12: Validate installed discovery, copied-home restart/resume and isolated mj file edit, command, memory MCP and model/effort switches. Original login hash unchanged; test session destroyed and daemon stopped.
- [x] 2026-11-12: Commit harness separately from the store change without pushing. Fix the combined-review login finding; account-boundary regression passes.
- [ ] Parent acceptance of combined changes after final validation; detailed results are in the report directory.

## Surprises & Discoveries

agy advertises both suffixed legacy models and base model selectors. Prefer configOptions. Only stdio MCP is advertised. Token-only staging needs no settings.json. Both profile-home and workspace AGENTS.md reach agy. agy 1.5.6 chooses the alphabetically first workspace root as its default command directory when additional directories are present, even with the requested cwd first in the list; explicit-path commands work. This upstream behavior is recorded rather than hidden by changing requested workspace attachments.

## Decision Log

Use antigravity identity, ~/.gemini source home, and token copies. No installation, quota, native subagents or automatic reviewer ranking. Configured approvals select default; unconstrained selects auto with skip-permissions. Plan entry and exit passed real ACP probing; exit restores target policy. Native checkpoint export is explicitly unsupported.

## Outcomes & Retrospective

The isolated session completed edits, commands and mj-memory search, and applied base-model and thought-level changes through mj. Direct ACP restart/resume preserves conversation history. The parent owns combined acceptance. Final validation results and any unrelated baseline failures are recorded in the report directory.

## Context and Orientation

mj-core/src/config/harness.rs owns identity, home layout and enforcement. harness_runtime.rs supplies launch commands; harness_install.rs chooses ambient versus managed programs. Controller stages credentials and discovers capabilities through workers. Database compatibility is owned by a separate child; wait for the parent to sequence isolated e2e against that change.

## Plan of Work

Extend HarnessKind and all exhaustive consumers with deliberate handling. Launch agy acp using the staged home via --gemini-dir, disabling telemetry. Parse token.expiry as RFC3339 freshness. Stage only required credentials and verified instructions. The parent assigned database constraint removal and unknown-harness preservation to another child; do not modify database code or add a migration. Add an ignored installed discovery integration test.

## Concrete Steps

From repository root run cargo check --workspace to locate exhaustive consumers. Probe using scripts/acp.py in /home/jonathan/.cache/mjolnir/build/agy-probe2 against a token-only COPY of ~/.gemini, never its live directory. Run targeted cargo tests elevated. Finish with cargo clippy --all-targets -- -D warnings and touched-crate suites, all in dev profile.

## Validation and Acceptance

Use isolated MJ_CONFIG_DIR and MJ_DATA_DIR and --instance agy-test for every CLI execution. Verify discovery sends no prompt, base models and effort switching, file edits and commands, mj-memory tools and resume. Hash original token before and after. Stop isolated daemon and all agy processes. Save text evidence in .mj/agents/d477ce3a33899a689a3fb5b0f4562335.

## Idempotence and Recovery

Never modify applied migrations. Stage only changed files for a master commit; do not push. Probe credentials are disposable copies; terminate their owning processes before removal.

## Artifacts and Notes

Input map: .mj/agents/ce76d64142ef2ccbe14a4303db2345de/map.md. Empirical findings: /home/jonathan/.cache/mjolnir/build/agy-probe2/FINDINGS.md.

## Interfaces and Dependencies

Reuse generic ACP configOptions and stdio MCP delivery, existing subprocess helpers and freshness reconciliation. agy must be installed on each target.

2026-11-12: Parent revised decision 9: database changes removed from this assignment. Initial isolated startup exposed cross-table trigger rename validation in the old admission helper; removed implementation patch retained only as evidence in report directory. Direct agy resume, instructions, token-only credentials, selectors and stdio MCP validated.

2026-11-12: Completed isolated acceptance with original Google token unchanged and verified stdio mj-memory.search_sessions. Added fixture support and early target executable lookup after existing integration tests found missing coverage. Additional-directory cwd defect reproduced directly; see directory-probe.log and directory-primary-probe.log in report directory.

2026-11-12: Combined review found custom-home CLI login omitted --gemini-dir. Native login now uses agy acp --login with the configured home; regression with a home containing spaces proves the default account remains untouched. Corrected TUI harness-name expectation to include Antigravity.
