# Add Antigravity ACP sessions

This living plan follows .agents/PLANS.md.

## Purpose / Big Picture

Enable explicit Antigravity profiles using a host-installed agy and isolated copies of Google credentials. ACP is the JSON RPC protocol between worker and harness.

## Progress

- [x] 2026-11-12: Read integration map and empirical agy 1.5.6 findings.
- [x] 2026-11-12: Implement identity, ambient executable lookup, token staging/freshness, selectors, permissions, instructions, stdio MCP and exhaustive handling. Store changes are separately owned.
- [x] 2026-11-12: Validate installed discovery, copied-home restart/resume and isolated mj file edit, command, memory MCP and model/effort switches. Original login hash unchanged; test session destroyed and daemon stopped.
- [x] 2026-11-12: Commit harness separately from the store change without pushing. Fix the combined-review login finding; account-boundary regression passes.
- [x] Parent verified the completed isolated E2E and clean combined review through dae8ecdde; original Google credentials stayed unchanged, and the test session and daemon were cleaned up.
- [x] Published the reviewed implementation as 49d62f2d8. Stopped the installer during compilation at the user's request; no installed binaries, live schema or profile changed.
- [x] Verified agy 1.5.6's structured `-p /usage --output-format json`: Gemini and Claude/GPT each have independent weekly and five-hour buckets. The command uses zero model turns/tokens. A separate minimal Claude Opus 5.5 prompt succeeded.
- [x] Added daemon-owned quota polling through the installed CLI, model-scoped selection/recovery, and two visible rows named `agy (Ge)` and `agy (Cl)`.
- [x] Recorded CLI data proves independent exhaustion and reset recovery; TUI/viewer goldens and four browser quota tests pass. The Claude probe decreased only the third-party pool; Gemini was unchanged.
- [x] The real Rust quota collector passed against the installed agy using a copied login (2.68 seconds, zero model turns). Fixed the review finding about empty readings: the daemon now publishes the configured pool rows independently of quota windows; the browser preserves both rows and expanded details during failures. Updated viewer and browser goldens pass.
- [ ] Finish real isolated quota-collector validation, full touched-crate suites, workspace clippy and review before pushing or installing the quota extension.
- [ ] Install only after quota support is understood and validated; apply the pre-approved migration, add profile `agy` using /home/jonathan/.gemini, and complete the installed-build isolated smoke test.

## Surprises & Discoveries

agy advertises both suffixed legacy models and base model selectors. Prefer configOptions. Only stdio MCP is advertised. Token-only staging needs no settings.json. Both profile-home and workspace AGENTS.md reach agy. agy 1.5.6 chooses the alphabetically first workspace root as its default command directory when additional directories are present, even with the requested cwd first in the list; explicit-path commands work. This upstream behavior is recorded rather than hidden by changing requested workspace attachments.

## Decision Log

Use antigravity harness identity, ~/.gemini source home, and token copies. The user-facing profile is `agy`. No managed harness installation, native subagents or automatic reviewer ranking. Configured approvals select default; unconstrained selects auto with skip-permissions. Plan entry and exit passed real ACP probing; exit restores target policy. Native checkpoint export is explicitly unsupported.

The user requires quota support before installation. Query the CLI's own documented `/usage` command, not reverse-engineered HTTP endpoints. Its JSON command.data.groups[].buckets[] supplies gemini-weekly, gemini-5h, 3p-weekly and 3p-5h, remaining_fraction and UTC reset_time. Store one copy of each window; scope Gemini to gemini model names and the third-party group to claude/gpt names. Extend the shared scope matcher to allow comma-separated model fragments. The daemon remains the sole quota poller. Run the command with a temporary HOME containing a credential copy; the top-level CLI does not accept ACP's --gemini-dir flag. Reuse process capture and credential-file helpers, and surface malformed or failed CLI output as an error instead of n/a.

Display two rows for this one profile, suffixed (Ge) and (Cl). One shared row list must drive TUI drawing, selection, scrolling and rename lookup so a second quota row does not select another profile. Preserve both pools in the viewer projection and display the same two labels there. Keep the existing persisted quota shape, including its optional string model_scope, and reuse the existing cache and refresh cadence.

## Outcomes & Retrospective

The isolated session completed edits, commands and mj-memory search, and applied base-model and thought-level changes through mj. Direct ACP restart/resume preserves conversation history. The parent verified these results and accepted the clean combined review. Final validation results are recorded in the report directory. The store plan tracks the remaining CLI historical-fixture correction before the user-authorized installation and profile setup.

Quota support now uses the CLI's own zero-turn JSON export, preserves four scoped windows through the daemon cache and viewer, and renders separate pool rows in both UIs. TUI navigation and rename use the same expanded row list as rendering. Reset-cache recovery matches both the window label and model scope, so same-named Week/5H windows cannot cross pools. The original login stayed unchanged throughout the live probes. Before/after readings were Gemini weekly 99.96924% unchanged, Gemini five-hour 99.87694% unchanged, Claude/GPT weekly 99.86148% to 99.78672%, and Claude/GPT five-hour 99.72296% to 99.57343% after the minimal Opus prompt.

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

2026-11-12: Parent resumed from the handoff, verified FINAL-EVIDENCE.md and acceptance-summary.md against the actual logs, and accepted the isolated smoke test and cleanup. Remaining publication work is to merge current master, finish the shared store fixture checks, install through scripts/install.sh, verify the live revision-126 migration, and add profiles.antigravity with home /home/jonathan/.gemini. The installed-build smoke test must also use the isolated agy-test instance.

2026-11-12: User stopped installation until both subscription quota pools are understood. The installer was still compiling and replaced no binaries; live schema remains 125 and no profile was added. The direct quota and Claude probes succeeded using copied credentials under /home/jonathan/.cache/mjolnir/build/agy-quota-probe. User chose two quota rows, (Ge)/(Cl), and shortened the profile name to agy. These later decisions supersede the earlier no-quota cut and profile name above.

2026-11-12: The quota collector and scoped tests passed. Review found that deriving browser rows from available windows collapsed them during failures. ViewerProfile now publishes quota_pools from the same shared Antigravity pool definition as the TUI, keeping row identity independent of readings. The browser golden covers an exhausted Claude pool followed by a failed probe with its details kept open.
