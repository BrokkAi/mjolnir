# Commit classification for the 2026-09-29 test-and-fix campaign

Range `59a58d62..3efdf890` (2026-09-26 to 2026-09-29, 132 commits). Prepared by a Sonnet subagent from the commit bodies and the user-facing parts of the diffs; **risk** marks a commit over about 300 non-test lines or one that changed a state machine, event routing, or ownership. The mission cards in `test-and-fix-2026-09-29.md` name the hashes they must reach; this file is the reference for triage and fix waves.

## Not probeable

Release bumps db138b70, 0afeff39, c931f15f. CI, lint, dependency chores 532c5763, d31cb395, b3fb800f, e76286d6, d187be00, 6af9e1ec, 1ec5f3f5, c6925d8b, 07e76c4d, ec62aa31. Docs and plan records 01df6dc6, dfcfff83, 3eefdd16, 6385b5b0, 2d98f154, 84048c22, 8b2b017d, cd37bace, 6e37fe12, 25fd1576, 9ccb5bc6, d7fb1f99, 19dd3e70. Test wording c45822f1, 364012f9, cb542e18. Homebrew formula name bf1c3df4 (probeable only on a Homebrew install). Merges f2f1f47a, dc44e1ef, 6ad463e5, 97de6b8b, 010187e3, abe8c297, 32076f16, 0116f46e, fc671d66, 8afd70cb, 7a1037c9, 15a692a4, 31798070, d0de11f0, 9f545a8b, 05e688d3, 5ca5e0e3, 5bd645d9, 18da1ca7, 13f0a95c, 28048e36, 6da878c7, abe49acb, 89235168, 389a1666, 50b1d330, 3ba9a98d, c1072323.

## Flags and config keys added, renamed, or removed

- Removed: `mj new --mj-subagents`, `--native-subagents` (4c03a8d5). Added: `--subagents native|all-models|single-model|none`, `--subagent-model`, `--subagent-effort` (the last two require `--subagents`; `single-model` requires `--subagent-model`). With no `--subagents`, the last accepted new-session choice per instance is reused; the initial choice is Native.
- Move (0cffddc6): `mj move --prepare`, `--allow-large-transfer`, `--exclude REPOSITORY:PATH` (repeatable); new `mj move-sources --session <id> [--cleanup <operation-id> --yes]`.
- Config version 13 to 14 (9ff63dca): global `[build_cache] enabled` is a legacy input migrated to `machines.<id>.build_cache` and dropped on the next save; budgets in 8c37339d.
- Environment entries accept `{ from_secret = "NAME" }` (from `secrets.toml` beside `config.toml`) and `{ from_env = "NAME" }` (6f89e55f). `mj doctor` reports a group/world-readable `secrets.toml` and plain-text `*_API_KEY`/`*_TOKEN` values. `mj doctor --smoke` exits non-zero on a failed requested smoke (3921dfb5).
- Web viewer: the `revision` SSE event is gone; refresh is a reconnect plus snapshot reload (e5f2e6df).
- API events (bbdef44b): generic `error` replaced by `turn_ended` (`outcome.kind`), `command_ended`, `session_fault`, `legacy_notice` (historical errors carry `original_type: "error"`).
- Schema migrations 57, 59–63, 64, 65; daemon protocol 40; worker protocol 27.

## (A) Terminal dashboard

- 3ba01992 **risk** (909 lines): session selection and the active conversation pane are one owner; `>` marks the active conversation; an empty active pane has no selection. Pass: marker always matches the active pane; Down/Up activates the pinned pane or opens Browse; filtering never switches the conversation and keeps the active row with an "Outside filter" label; drafts survive switches; layout and marker restored after restart.
- 27e9a1ac: Sessions footer lacks "Enter open" until a row is selected.
- 678a61ba: Up/Down at list boundaries reach adjacent enabled controls (Settings → Machines → Build cache Back/Save; project Create); disabled actions skipped.
- ba1f31f5 **risk** (691 lines): Model and Effort modals are anchored, names-only dropdowns that filter as you type and commit on click or Enter; accent reserved for focus, selection, primary actions, activity.
- 40922d77: wizard Subagents/Model/Effort are compact combo boxes; previews commit only on Enter/Tab/click; Esc dismisses locally; "Refresh profiles" action.
- 20b5749a: clicks inside an open dropdown go to the dropdown.
- 31877b53, 0b9ae9c3: accent color on enabled controls before focus in all five themes; disabled muted.
- 1b2ef725: attention badges visible on 1–2 character workspace names, wide and combining Unicode.
- 13ba3651 **risk** (303 lines): plain URLs are links; trailing punctuation excluded; table cells keep links and inline styles; non-http(s) link copies with a notice.
- 743ae5d4 **risk** (517 lines): clicking a markdown link opens the browser (webbrowser crate; WSL uses cmd.exe); only http/https; failure is a notice; identical-text and wrapped links open the right destination.
- ce5c2b51 **risk** (1075 lines): guided Q&A on TUI and web; one question at a time, progress, unanswered warning, separate partial submit whose default returns to the first unanswered question; long text wraps.
- 99838634: prompt footer "Subagents · N/M"; "Subagents · 0" with none; web button same form, hidden with no children.
- e25427f6: macOS shows Cmd-V (macOS only).
- 3b604eed: Esc during an autonomous native goal turn sends CancelTurn; between turns Esc is a no-op.

## (B) Session lifecycle and daemon

- 89874f12: startup step ids use `-`; first prompt, `--model`/`--effort` on `mj new`, and a child's model/prompt now run.
- 05d2c739: failed startup steps of vanished sessions no longer log every 30 s.
- 5165e5ba: cancelled startup steps of a removed session settle without waiting; no infinite retry after restart.
- aaf5bf06: local bare worker launch no longer killed by process-group cleanup ("left no exit record").
- 138020d7 **risk**: process groups owned through pipe drain; a completed command terminates descendants.
- 670a5bb7, 7f0d41b9 **risk**, a046063c: one routine_checkpoint_wait predicate; deferral backoff 30 s doubling to 10 min; failed/deferred checkpoint removes its stage and archive; daemon-start sweep of `checkpoint-<hex>-stage`, `.hel.zip`, `.checkpoint-capture-*` in local worker roots with one info line.
- 7414cc74: checkpoint restore checks out the archived commit before updating branch refs (no phantom staged deletions).
- db529082: native local paths (spaces, Windows) accepted for recovery checkpoints.
- 5fb73b66: lifecycle action futures boxed (debug-daemon stack overflow on checkpoint).
- 4b1b60b5 **risk**: upgrade waits bounded to seconds; the gate drains and refuses deferrable work; `wait_agents` re-runs on the next daemon with the original deadline.
- c8d27f3c **risk**: daemon is the sole owner of credential reconciliation, admitted via RecoveryGate.
- f7272bb0 **risk**: a binary under a Cargo profile directory (`.fingerprint`) refuses to start, replace, stop or migrate the default instance.
- c52bd0dd: `MJ_INSTANCE` no longer stamped into worker environments.
- 3921dfb5: read-only attachment fallback on non-Linux Docker; `mj doctor --smoke` exit code.
- 7bdfccfe: setup/doctor Apple checks (macOS).
- 6f89e55f **risk** (1037 lines, 51 files): secret references; see flags.

## (C) Sub-agents and delegation

- 4c03a8d5 **risk** (2144 lines, 102 files): four policies; single-model spawn uses fixed model/effort and omits `list_profiles`; Claude/Codex Mjolnir children cannot delegate natively; last accepted choice persisted per instance.
- 549a6571 **risk** (1164 lines): `send_input` acknowledged after the parent worker persists its queue; one dispatcher orders per-child delivery; interrupts do not block on startup.
- 07c499fb **risk** (1687 lines): delegation is a daemon-owned service; works with the web viewer disabled and no dashboard (#1171).
- e1253507 **risk**: both `wait` forms use the worker's Jev verdicts; settled targets returned during unrelated activity; survives restart.
- 8cecc1c0 **risk** (2065 lines): unified Jev turn assessment under worker ownership; capacity verdict schedules recovery once; new user work supersedes.
- 0804e9f7 **risk**: mj-agents tools exposed upfront (Claude `alwaysLoad`; Codex `omit_tools_from = ["deferred"]`); guidance describes routing.
- a0f4009a, e9b5def7, 9fbf746c, 4dfe8379, 3efdf890, 19dd3e70: parent guidance text (delegate investigation, suites, slices; keep design/review/commit; re-task once).
- 0f15b38a: Codex code-mode parents told not to poll the exec cell every second.
- 7029361e: `send_input` to a child that just handed back no longer fails with "reserved for a lifecycle operation" (#1186).

## (D) Quota, profiles, credentials

- 2eae48b2 **risk** (901 lines): banked resets (`5d 7h [1]`) and live countdowns, TUI and web; custom Codex providers excluded.
- e398a42d: quota verdict classifier examples; usage-limit messages naming a reset time count as quota.
- c8d27f3c, 6f89e55f, 660c78e9: see B and G.

## (E) Web viewer

- e5f2e6df **risk** (1119 lines), 33c2011d, bf0dce4c: keyed projections and bounded deltas with incarnation cursors; refresh is reconnect plus reload.
- 770d2768 **risk**: draft writers drain the latest value; attachments and drafts explicitly owned.
- ce5c2b51, 2eae48b2, 99838634, 4c03a8d5, 0cffddc6: web sides of A, D, C, H.

## (F) CLI and HTTP API

- bbdef44b **risk** (1771 lines, migration 57): durable event classification (see flags); a checkpoint succeeds only after its verified archive and metadata are durable; `mj wait` exit codes unchanged.
- e1253507, 4c03a8d5, 0cffddc6, 3921dfb5, 6f89e55f, 7bdfccfe: see above.
- 221e1343: bare project directories keep their raw context identity at API and web admission (remote paths validated by the target owner).

## (G) Harness-specific

- 1af3b8d5 **risk**: Claude bridge pin 0.84.0 (Claude Code 2.1.284, Sonnet 5.5).
- dc436f0c: goal controls from `_meta.jetbrains.air.goal`; AskUserQuestion "Other" paired by `question_<n>_custom`; 0.81.0 shapes still accepted.
- c343265c, 660c78e9: Codex adapter pin 1.13.4 (660c78e9 mentions 1.13.5; verify with `mj doctor`).
- 660c78e9 **risk** (366 lines): Guardian mode preapproves owned MCP tools (memory, delegation, reviewer) for Codex and Claude (#1176).
- 61a1cfaf **risk** (622 lines): Codex command catalogues and goal state survive `session/load`.
- a8013b3b **risk** (736 lines): one Kimi OAuth refresher shared by quota and inference; once-only 401 retry.
- a3d4f61f: provider retry backoff resets when the refusal sequence ends.
- Grok and Muse: no commit in the range names them.

## (H) Move, workspace transfer, build cache, machines

- 0cffddc6 **risk** (4447 lines, 60 files, migration 64): Move transfers outside recovery checkpoints; metadata-only handoff for unchanged environments; rsync transfer verified before the destination starts; HEAD, refs, stashes, index, dirty work preserved; ≥1 GB "Choose files" page with exclusions and consent; retained sources via `mj move-sources`.
- 55527484 **risk** (972 lines): unchanged Move environments preserved through swaps and recovery; Resume guard; queued receipts wait for readiness.
- 6fc2ed80: macOS rsync refused before copying (macOS only).
- 9ff63dca **risk** (516 lines): per-machine build caching; global policy page removed.
- 8c37339d **risk** (1520 lines): build cache budgets as shared machine policy; stale-write conflicts rejected.
- ed937e10: mbx only on Linux cache hosts.
- 221e1343 **risk** (720 lines): native macOS workers on SSH hosts (cannot run here).

## (I) Concurrency sweep and its unwinding

Sweep commits, oldest first, all **risk**: bbdef44b, c2617cce, 07c499fb, 08984a09, 8cecc1c0, 272d95e8, 98960791, bf0dce4c, e5f2e6df, 33c2011d, 770d2768, 0a98bc4f (migrations 59–63, receipts and tombstones, review state machine, incarnation triggers), then 7029361e (unwind: 86 files, +1327/−5460, migration 65, worker protocol 27); a8013b3b and 1ec5f3f5 undo the Kimi vendor service.

Kept by 7029361e: identity-checked recovery attempts, atomic gate close, the single Move ownership map, field-owned resume saves, compare-and-set recovery writes, close route decided inside lifecycle admission, bounded per-stream request supervisor, process-group and pipe supervision, PID birth check, cancellable SSH connect, `delegation_effects` idempotency record, the durable startup queue (settled rows deleted), `worker_restart_intents`.

What a tester could see if the unwinding left something broken:

1. Review cancellation on restart: notice "Turn review was cancelled when Mjolnir restarted; the next review covers the same changes"; baseline does not advance (#1185 tracks the sidecar fix).
2. Delivery and double actions: plain submits under a stable command id; the worker journal keeps the newest 512 terminal command ids so a retry after a lost reply gets the original acceptance. Broken: a prompt runs twice, or a submit is dropped silently.
3. Delegation replay: a spawn replayed after a restart still produces one child; `delegation_effects` row deleted once the parent has its result.
4. Startup queue: a refused prompt goes back to the draft with a notice; uncertain failures retry under the same id; five failed rounds give up.
5. Old stores: migration 65 drops `session_incarnations`, its triggers and `turn_review_state.orchestration`; stale delegation records and settled steps pruned at start; 2.23.2/2.23.3 workers (protocol 26) keep reconnecting.
6. Events not arriving: runtime feed is the TUI authority (770d2768); delta cursors incarnation-scoped (bf0dce4c); a cursor expiry forces a full reset; a deleted session must not reappear.
7. Stale projections: rows, children, checkpoint metadata patched from changed keys only (33c2011d); TUI, web and `mj sessions` must agree.
8. Wrong-incarnation callbacks: lifecycle callbacks carry operation identity (98960791, 0a98bc4f); nothing stays "suspending"; a destroyed session's worker must not survive.
9. Move ownership: only one of source or destination owns the resources.
10. Credentials: one Kimi refresher; daemon-owned sync admitted through RecoveryGate.
11. Store guard: `open_writable` calls `ensure_may_control_store` (cb542e18).

Follow-up daemon fixes on the sweep: 05d2c739, 5165e5ba, 89874f12, aaf5bf06, 4b1b60b5.

Highest-risk first probes: Move (0cffddc6, 55527484), unified selection (3ba01992), four-way subagent policy (4c03a8d5), daemon restart during a review and a spawn (7029361e), event and outcome classification (bbdef44b).
