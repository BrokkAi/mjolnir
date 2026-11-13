# Add Antigravity ACP sessions

This living plan follows .agents/PLANS.md.

## Purpose / Big Picture

Enable explicit Antigravity profiles using isolated copies of Google credentials. ACP is the JSON RPC protocol between worker and harness. This plan records the initial delivery; `.agents/plans/antigravity-managed-runtime.md` adds pinned installation on session targets and supersedes the original host-installed-only restriction.

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
- [x] Full merged client/controller/core/TUI/CLI validation passed, including 21 isolated daemon startup regressions. The worker suite exposed a shared fixture omission: its legacy mode list lacked Antigravity's default mode. Add that mode only for Antigravity so the existing rejected-restoration negative case still has no default fallback; all five affected ACP tests pass.
- [x] Workspace `cargo clippy --all-targets -- -D warnings` passed on the final merged source (2m08s).
- [x] Final review was clean through a31379ca4; pushed the reviewed implementation and completed scripts/install.sh from that frozen source.
- [x] The installed daemon completed its normal handoff at protocol 93. The live store is at revision 126 with minimum compatible revision 126 and neither harness CHECK remains.
- [x] Set github.watch.use_gpu=false after installation and before starting the upgraded daemon, as requested by the user.
- [x] The user changed the final profile name to `gemini`. Configured it with /home/jonathan/.gemini and removed the identical `antigravity` profile that startup discovery had added. The live daemon discovers all eight models and collects all four scoped quota windows without error.
- [x] The installed build completed a real Claude Opus 5.5 ACP turn in the isolated agy-installed instance: session 8b599b4368e092ca09d1473648051cf8 returned AGY_INSTALLED_OK with no tool calls. Destroyed that test session and stopped its daemon.

## Surprises & Discoveries

agy advertises both suffixed legacy models and base model selectors. Prefer configOptions. Only stdio MCP is advertised. Token-only staging needs no settings.json. Both profile-home and workspace AGENTS.md reach agy. agy 1.5.6 chooses the alphabetically first workspace root as its default command directory when additional directories are present, even with the requested cwd first in the list; explicit-path commands work. This upstream behavior is recorded rather than hidden by changing requested workspace attachments.

## Decision Log

Use antigravity harness identity, ~/.gemini source home, and token copies. The user-facing profile is `gemini`, superseding the earlier `agy` choice; the executable remains `agy`. The initial cut omitted managed installation; the managed-runtime follow-up adds it. Native subagents and automatic reviewer ranking remain outside this implementation. Configured approvals select default; unconstrained selects auto with skip-permissions. Plan entry and exit passed real ACP probing; exit restores target policy. Native checkpoint export was initially unsupported and is now implemented; see the 2026-11-13 entry below.

The user requires quota support before installation. Query the CLI's own documented `/usage` command, not reverse-engineered HTTP endpoints. Its JSON command.data.groups[].buckets[] supplies gemini-weekly, gemini-5h, 3p-weekly and 3p-5h, remaining_fraction and UTC reset_time. Store one copy of each window; scope Gemini to gemini model names and the third-party group to claude/gpt names. Extend the shared scope matcher to allow comma-separated model fragments. The daemon remains the sole quota poller. Run the command with a temporary HOME containing a credential copy; the top-level CLI does not accept ACP's --gemini-dir flag. Reuse process capture and credential-file helpers, and surface malformed or failed CLI output as an error instead of n/a.

Display two rows for this one profile, suffixed (Ge) and (Cl). One shared row list must drive TUI drawing, selection, scrolling and rename lookup so a second quota row does not select another profile. Preserve both pools in the viewer projection and display the same two labels there. Keep the existing persisted quota shape, including its optional string model_scope, and reuse the existing cache and refresh cadence.

Advance the daemon protocol to 93 for the new HarnessKind wire value: a protocol-92 client cannot deserialize Antigravity sessions or profiles. Coordinate the number at the final merge with concurrent wire changes; no additional database migration is required. The merge of subscription-weighting work adds monthly_price_usd to quota reports; Antigravity leaves it unknown, consistent with the other non-Claude adapters.

The user explicitly marked the extra-directory shell-cwd defect as won't fix for now after clarification: a bare pwd can run in the attached memory root instead of the primary project, while explicit command paths work. Keep the additional directories intact and document the upstream behavior; do not hide it with a fallback or remove requested attachments.

The final profile name is gemini, so both UIs label its pool rows `gemini (Ge)` and `gemini (Cl)`. Disable GPU scoring with github.watch.use_gpu=false before the upgraded daemon first starts; preserve all other preferences and profile settings.

## Outcomes & Retrospective

The isolated session completed edits, commands and mj-memory search, and applied base-model and thought-level changes through mj. Direct ACP restart/resume preserves conversation history. The parent verified these results and accepted the clean combined review. Final validation results are recorded in the report directory. The store plan records the completed CLI historical-fixture correction and live migration.

Quota support now uses the CLI's own zero-turn JSON export, preserves four scoped windows through the daemon cache and viewer, and renders separate pool rows in both UIs. TUI navigation and rename use the same expanded row list as rendering. Reset-cache recovery matches both the window label and model scope, so same-named Week/5H windows cannot cross pools. The original login stayed unchanged throughout the live probes. Before/after readings were Gemini weekly 99.96924% unchanged, Gemini five-hour 99.87694% unchanged, Claude/GPT weekly 99.86148% to 99.78672%, and Claude/GPT five-hour 99.72296% to 99.57343% after the minimal Opus prompt.

Installation and profile setup are complete. The installed a31379ca4 daemon runs protocol 93 and migrated the live store to revision/floor 126. The gemini profile uses /home/jonathan/.gemini, and the default daemon has already persisted a successful quota reading containing both Gemini and Claude/GPT windows. The installed-build isolated smoke selected claude-opus-5-5 and completed in 1.85 seconds. Doctor reports the new profile and all eight advertised models ready; its unrelated pre-existing diagnostics remain. GPU scoring is off. Platform CI was dispatched as run 43284330599; it was still queued at installation acceptance. Target-managed agy installation was added by the managed-runtime follow-up and native checkpoint export by the 2026-11-13 entry below. The user accepted the upstream extra-directory command-cwd limitation.

## Context and Orientation

mj-core/src/config/harness.rs owns identity, home layout and enforcement. harness_runtime.rs supplies launch commands; harness_install.rs chooses ambient versus managed programs. Controller stages credentials and discovers capabilities through workers. Database compatibility is owned by a separate child; wait for the parent to sequence isolated e2e against that change.

## Plan of Work

Extend HarnessKind and all exhaustive consumers with deliberate handling. Launch agy acp using the staged home via --gemini-dir, disabling telemetry. Parse token.expiry as RFC3339 freshness. Stage only required credentials and verified instructions. The parent assigned database constraint removal and unknown-harness preservation to another child; do not modify database code or add a migration. Add an ignored installed discovery integration test.

## Concrete Steps

From repository root run cargo check --workspace to locate exhaustive consumers. Probe using scripts/acp.py in /home/jonathan/.cache/mjolnir/build/agy-probe2 against a token-only COPY of ~/.gemini, never its live directory. Run targeted cargo tests elevated. Finish with cargo clippy --all-targets -- -D warnings and touched-crate suites, all in dev profile.

## Validation and Acceptance

Use isolated MJ_CONFIG_DIR and MJ_DATA_DIR and --instance agy-test for implementation probes and agy-installed for the installed-build smoke. Verify discovery sends no prompt, base models and effort switching, file edits and commands, mj-memory tools and resume. Hash original token before and after credential probes. Stop isolated daemons and all agy processes. Save initial text evidence in .mj/agents/d477ce3a33899a689a3fb5b0f4562335 and final acceptance evidence in .mj/agents/35cd6ee67e57560bb12ce5b621a1387a/finish.

## Idempotence and Recovery

Never modify applied migrations. Stage only changed files for a master commit and obtain the configured review before pushing to origin/master. Probe credentials are disposable copies; terminate their owning processes before removal. The installed live store requires a revision-126-compatible daemon; do not downgrade it.

## Artifacts and Notes

Input map: .mj/agents/ce76d64142ef2ccbe14a4303db2345de/map.md. Empirical findings: /home/jonathan/.cache/mjolnir/build/agy-probe2/FINDINGS.md.

## Interfaces and Dependencies

Reuse generic ACP configOptions and stdio MCP delivery, existing subprocess helpers and freshness reconciliation. The initial version required agy on each target; the managed-runtime follow-up installs the pin through the existing target-side resolver.

2026-11-12: Parent revised decision 9: database changes removed from this assignment. Initial isolated startup exposed cross-table trigger rename validation in the old admission helper; removed implementation patch retained only as evidence in report directory. Direct agy resume, instructions, token-only credentials, selectors and stdio MCP validated.

2026-11-12: Completed isolated acceptance with original Google token unchanged and verified stdio mj-memory.search_sessions. Added fixture support and early target executable lookup after existing integration tests found missing coverage. Additional-directory cwd defect reproduced directly; see directory-probe.log and directory-primary-probe.log in report directory.

2026-11-12: Combined review found custom-home CLI login omitted --gemini-dir. Native login now uses agy acp --login with the configured home; regression with a home containing spaces proves the default account remains untouched. Corrected TUI harness-name expectation to include Antigravity.

2026-11-12: Parent resumed from the handoff, verified FINAL-EVIDENCE.md and acceptance-summary.md against the actual logs, and accepted the isolated smoke test and cleanup. Remaining publication work is to merge current master, finish the shared store fixture checks, install through scripts/install.sh, verify the live revision-126 migration, and add profiles.antigravity with home /home/jonathan/.gemini. The installed-build smoke test must also use the isolated agy-test instance.

2026-11-12: User stopped installation until both subscription quota pools are understood. The installer was still compiling and replaced no binaries; live schema remains 125 and no profile was added. The direct quota and Claude probes succeeded using copied credentials under /home/jonathan/.cache/mjolnir/build/agy-quota-probe. User chose two quota rows, (Ge)/(Cl), and shortened the profile name to agy. These later decisions supersede the earlier no-quota cut and profile name above.

2026-11-12: The quota collector and scoped tests passed. Review found that deriving browser rows from available windows collapsed them during failures. ViewerProfile now publishes quota_pools from the same shared Antigravity pool definition as the TUI, keeping row identity independent of readings. The browser golden covers an exhausted Claude pool followed by a failed probe with its details kept open.

2026-11-12: Final merged suites: client 56 passed/1 ignored plus integration, controller 2584 passed/26 ignored, core 645 passed/3 ignored plus compatibility cases, TUI 581 passed/2 ignored, CLI 254 unit tests and all 21 startup tests passed along with the other isolated integration/PTY cases. Worker 833 passed/11 ignored with one negative-mode fixture failure; after restricting the added default mode to Antigravity, that failure and all four earlier cross-harness ACP regressions pass (five tests, 0.28 seconds). The user accepted the observed extra-directory cwd limitation as won't fix for now.

2026-11-12: Completed the authorized installation, normal daemon handoff, live revision-126 migration, and installed-build Opus smoke after final review. Applied the user's latest profile name, gemini, and disabled GPU scoring before daemon startup. Removed only startup discovery's identical unused profile, preserving unrelated settings. The isolated smoke session was destroyed and its daemon stopped; final evidence is in the finish report directory.

2026-11-12: Added a pointer to the #1520 managed-runtime follow-up so the initial ambient-only scope is not mistaken for the current design. The extra-directory cwd limitation is unchanged; the native-checkpoint limitation was lifted on 2026-11-13.

2026-11-13: Added native checkpoint support. agy 1.5.8 keeps each conversation in `antigravity-cli/conversations/<ACP session id>.db`, a SQLite database in WAL mode, written at `session/new`. A checkpoint carries a SQLite backup-API snapshot of that file (the same snapshot code OpenCode uses; copying the file alone lost the newest turn, which is still in the `-wal` file while agy runs) and every file under `antigravity-cli/brain/<id>/`, where agy keeps the full text of tool output that the database stores truncated. It never carries the login (`antigravity-oauth-token`, the only file with credentials; the database holds none), logs, the indexes shared by all conversations, `-wal`/`-shm` files or other conversations. A restore removes the stale `-wal`/`-shm` files of every SQLite artifact it writes (one rule shared with OpenCode). The database records every workspace root the conversation was given (the launch directory and each additional directory, such as the project-memory replica) as `file://` URIs in `trajectory_metadata_blob` (id `main`), twice: as a repeated protobuf field 1 (whose field 1 is the URI) and as a repeated string field 7. agy writes the record at `session/new` and never rewrites it on `session/load`; the roots are in alphabetical order with nothing marking the launch directory, and agy runs commands in the alphabetically first of the recorded roots and the request's roots, so a recorded root that no longer exists makes every command fail ("no such directory"). A restore at another path therefore records the new launch directory as the record's only root (in the first root message and the first root URI, with every other field copied as it was; `mj-checkpoint/src/native/antigravity.rs`). No recorded root is kept, whether or not it exists on the restoring host: a root that still exists there may be the old tree, and agy would run commands in it. Every worker start passes the additional directories to `session/resume` or `session/load` again (`mj-worker/src/worker_runtime/unix.rs:324`, `acp/session.rs:286,294`, `acp/launch.rs:480-483,536,552`), so agy's roots become the new launch directory plus the request's directories. URIs use agy's own Go-style escaping, because the `url` crate leaves `'()[]!*^|` unescaped where agy writes `%27`, `%28`, and so on. A record whose two lists name different sets of roots, holds a root that is not a file URI, or is of any other shape, fails the restore. `session/load` of a conversation with no database succeeds and only the next prompt fails, so the missing-session classifier stays false and the artifact is required for prompted sessions.

Validation 2026-11-13: `an_antigravity_checkpoint_keeps_the_live_conversation_and_follows_the_workspace` (live WAL-only turn, restore at a longer Unicode/space path with recorded roots that still exist, mismatched record refused). Isolated instance `agy-ckpt` with real agy 1.5.8: suspend and resume through mj restored the conversation (secret word and written text recalled); the real restore code at another path collapsed a five-root record (two of the roots still existed on the host) to the one new root, and real agy resumed it with an extra directory passed only in the request: history recalled, the default directory was the extra directory, and a relative `cat note.txt` there worked. The original login hash was unchanged and the sessions, daemon and credential copies were removed.
