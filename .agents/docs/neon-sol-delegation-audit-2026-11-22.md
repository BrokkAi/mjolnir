# Neon Primary / Sol Subtle Delegation Audit (2026-11-22)

Read-only empirical audit of Mjolnir's live SQLite store (`~/.local/share/mjolnir/mj.sqlite3`) on November 22, 2026, investigating why spend stats showed ~3× spend on Gemini 4 Neon (primary) versus GPT-6.2 Sol (`subtle`), where Neon was doing delegatable work locally, and how `straightforward` (GPT-6 Luna / Grok 4.7) performed versus `subtle` (GPT-6.2 Sol) across 230 managed workers.

All dollar figures below are known API-equivalent estimates using `mj-core/src/usage/pricing.rs` (`gemini-4-neon` $3/$15/$0.15/$0 per M; `gpt-6.2-sol` $2/$10/$0.10/$2.50; `gpt-6-luna` $0.10/$0.50/$0.01/$0.125; `claude-opus-5-5` $4/$20/$0.20/$5.00; Antigravity `thought_tokens` is a subset of `output_tokens` and `input_tokens` is uncached input). Native harness logs were never read; only Mjolnir's own store records were used.

## 1. Decomposing the ~3× Neon-to-Sol Spend Ratio

Snapshot through **22:14:42Z (16:14:42 CST)** on 2026-11-22 (local day start `06:00:00Z`):

| Window | All known USD | Neon USD | Sol family USD | Neon / Sol |
|---|---:|---:|---:|---:|
| Local day (06:00Z–22:14Z) | $916.47 | $565.84 (1,256 turns) | $180.31 (264 turns) | **3.14×** |
| UTC day (00:00Z–22:14Z) | $1,070.20 | $675.48 (1,617 turns) | $190.35 (271 turns) | **3.55×** |
| Dashboard rolling active roots | $857.49 | $561.86 | $161.34 | **3.48×** |
| Local post-crossover (15:47:18Z–22:14Z) | $584.98 | $313.15 | $173.58 | **1.80×** |

Three non-behavioral factors accounted for a large share of the raw 3.14× global ratio:

1. **1.50× base token pricing:** Neon is priced 1.5× Sol on ordinary input, output, and cache-read tokens. Across the local day, `3.138× = 1.500× rate × 1.940× token volume × 1.079× mix`.
2. **Mid-day `[subagents.subtle]` switch (`15:22Z–15:47Z` / `09:22–09:47 CST`):** Through `15:21:59Z`, `subtle` spawns still routed to Neon (21 local / 27 UTC launches; 26 local / 38 UTC managed Neon children spending **$68.98 local / $109.55 UTC**). Starting `15:47:18Z`, `subtle` spawns routed to GPT-6.2 Sol. Separately, **$69.38** of local Sol spend was automated code reviewers, not subtle workers.
3. **Ten `mode: none` Neon roots ($87.07 local = 15.4% of all Neon spend):** Eight Bifrost P3/P4 PR-fix helpers, the P4 Option B runner, and the P2 lead successor were created via `mj new` omitting `--subagents`. On Antigravity (and DSH), `SubagentPolicy::default_for(harness)` normalized omitted policy to `None`, persisting `{"mode":"none"}` even though all eight PR-fix prompts instructed them to use `spawn`. One helper (#14267) fell back to three native Antigravity `invoke_subagent` calls; the ten `mode: none` roots spent **83.0%** ($72.27) of their spend in direct edit/build-test turns.

### Clean cohort: Mjolnir-enabled Neon roots vs. Opus baseline

Stripping out `mode: none` roots and non-root Neon children still leaves a clear behavioral gap:

| Cohort (local day) | Parent USD | Subtle Sol USD | Parent / Subtle Sol | Bucket A: plan / delegate / review (≤2 local calls) | Bucket B: moderate inspect (3–5 calls) | Bucket C: heavy local read (≥6 calls, no edits) | Bucket D: direct edits or build/test |
|---|---:|---:|---:|---:|---:|---:|---:|
| **25 Mjolnir-enabled Neon roots (local day)** | $409.79 | $67.50 | **6.07×** | 18.3% ($74.89) | 7.5% ($30.93) | 18.2% ($74.58) | **56.0%** ($229.40) |
| **Same cohort, post-15:47:18Z only (22 roots)** | $223.74 | $67.50 | **3.31×** | — | — | — | **55.4%** ($123.94) |
| **6 Opus-primary roots (local day)** | $137.11 | $20.42 | 6.72× *(at 2.5× rate)* | **39.1%** ($53.59) | 18.6% ($25.52) | 20.3% ($27.85) | **22.0%** ($30.15) |

*Note: Bucket D attributes whole relay turns containing edits or build/test commands, so it includes planning/messaging occurring within those turns.*

Across the 25 Mjolnir-enabled Neon roots, parents ran **2,957 shell commands, 2,141 `read_file`, 1,153 `edit_file`, and 59 `write_file`** calls locally against **201 spawn attempts**, with **191 turns of ≥5** ($186.56), **101 turns of ≥10** ($132.07), and **34 turns of ≥20** ($67.11) direct calls without spawning. Thirteen of the 25 spawned zero subtle workers all day.

## 2. Six Recurring Under-Delegation Patterns on Neon

1. **Taking over locally after a stalled `straightforward` (Luna) child instead of escalating to `subtle` (Sol)** *(largest behavioral leak)*:
   - Triggered by the old `DELEGATION_ROUTING` sentence *"Correct one incomplete handback with failing evidence; take over a second failure and report it."*
   - Examples: *P1 #14705 PR A activation owner* closed a Luna child after three incomplete returns on cross-crate Slice 2 and absorbed Slice 2 + Slice 1b in one 28-minute, 19.3M-token, $4.98 turn (58 reads, 57 commands, 12 edits) plus 15.35M tokens of follow-up fixes; *P3 lead* took over #14762 after closing a stalled Luna merge audit; *P1 #14589 stack* wrote the Rust benchmark harness locally (6.55M tokens) after closing a stalled probe child; *Automove Reliability* took over PR 3 tests locally after two incomplete Luna handbacks.
   - Good contrast: *P2 #15153 const fail-open* and *MBX Crate Splits* escalated stalled Luna slices straight to Sol ($0.83 parent coordination cost on const fail-open), and Sol completed them cleanly.
2. **Stopping delegation after diagnosis:** Neon delegated forensics well, then implemented locally once the map arrived (*Automove Reliability* implemented #1631/#1341/#1639 locally in 209 calls / 37 edits / ~41% of parent tokens after four forensic handbacks; *P1 #14196 fix D* spent a 19-minute, $5.04, 169-call / 60-edit turn implementing #15243 HOME isolation across 18 files and only offloaded test runs to Luna; *New Sessions Dialog Grouping* made 107 parent edits after only three Luna spawns).
3. **Keeping multi-round review fixes and fixture repairs local while using Luna only as a rerun bot:** Review tools intentionally stay top-level-only because the primary must distinguish targeted fixes, scope creep, and design rethinks. However, *Repair Broken CI* (four serial review-fix rounds: 75 calls, 14 edits, 52.5% of parent tokens), *Selected-root fallback design* (9.62M tokens / 11 edits across two review-fix turns), and *Antigravity Background Task Restart* all kept mechanical follow-up edits local while pinging a child only to rerun tests.
4. **Absorbing multi-branch merges, conflicts, and CI failure triage locally:** *MBX Crate Splits* spent one $5.95 / 71-call turn (22.2% of parent tokens) on 3-branch integration + manifests + workflows and 52 calls on post-push CI triage; *P1 M3 chain* did a 6.12M-token / 53-call cascade merge locally; *13570 + 13454 owner* spent 32.8% of parent tokens resolving five branch-A conflicts locally.
5. **Re-doing codebase archaeology before planning or after specialist handbacks:** Bucket C accounted for $74.58 (18.2%) of Mjolnir-enabled Neon spend (e.g., 60 calls / $5.03 right after Sol's hosted diagnosis on *P1 #14705*; 61 preflight reads on *MBX Crate Splits*; 36 history searches + 12 reads + 9 fetches on *P2 #15153*).
6. **High-context amplification and short wakes:** On heavy Bifrost lanes, **96.2%** of Neon tokens were cached reads and uncached input averaged ~45K/turn (confirmed real uncached input in `mj-worker/src/acp/antigravity_usage.rs`, not cache-write conflation). Short mailbox wakes (≤45s, ≤2 tools) were 42.7% of post-cutoff turns and 13.7% of tokens (0.8–1.0M cached tokens per wake); multi-step local edit turns multiplied that context across dozens of inner steps ($4–$6 per turn).

**Positive baselines observed on the same day:** *Issue Triage* ($0.76 Neon vs. $3.32 across 6 Sol workers = 0.23×, 31 parent calls), *Selected-roots verifier* (zero post-cutoff `edit_file` calls; 10.98M Neon vs. 17.18M across 4 Sol audits), *P4 lead* ($9.09 Neon, 3 targeted Sol spawns, edits restricted to ledger/briefs), and *P2 #15153 const fail-open* post-crossover (39.05M Neon vs. 75.79M across 7 Sol workers).

## 3. Empirical `straightforward` (Luna) vs. `subtle` (Sol) Outcomes (230 Workers)

Snapshot through **23:57:44Z** on 2026-11-22 (local day `06:00Z+`, excluding the audit subtree and formal reviewers/oracles): **230 verified managed workers** (132 Luna, 6 Grok, 92 Sol).

### Overall outcomes

| Model / Role | Children | Clean first | Complete after reminder | Initial partial | Final complete | Final partial | Corrective steering | Closed incomplete | Sent to Sol | Median first turn | Median first tools |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Luna (`straightforward`)** | 132 | 72 | 2 | **55 (42%)** | 97 | **32** | 39 | **29** | 6 | 109s | 10 |
| **Grok (`straightforward`)** | 6 | 5 | 0 | 0 | 5 | 0 | 0 | 0 | 0 | 347s | 33.5 |
| **Sol (`subtle`)** | 92 | 78 | 2 | **0 (0%)** | **80 / 80** *(10 open, 2 cancelled)* | **0** | 4 | **0** | — | 471s | 58 |

### Luna outcomes by assigned scope vs. Sol

| Assigned scope | Luna children | Luna clean first | Luna final complete | Luna closed incomplete | Luna median first turn / tools | Sol clean / final (assessed) |
|---|---:|---:|---:|---:|---:|---:|
| Quick lookup / check | 15 | **14 / 15 (93%)** | **15 / 15 (100%)** | 0 | 47s / 8 | — |
| Multi-step read-only exploration | 39 | **30 / 39 (77%)** *(31 w/ reminder)* | **39 / 39 (100%)** | 0 | 84s / 10 | 48 / 49 → **49 / 49 (100%)** |
| Scripted test / build / gate | 45 | **24 / 45 (53%)** | **34 / 45 (76%)** | 8 | 302s / 10 | **3 / 3 (100%)** |
| Benchmark / multi-stage ops | 12 | **2 / 12 (17%)** *(3 w/ reminder)* | **3 / 12 (25%)** | **7** | 312s / 14 | — |
| Small / local edit | 2 | 0 / 1 *(1 unknown)* | 0 / 1 | 1 | 557s / 12 | — |
| **Multi-file / cross-crate edit** | 19 | **2 / 19 (11%)** | **6 / 19 (32%)** | **13** | **108s / 14** | 27 / 28 → **28 / 28 (100%)** *(median 850s / 73 tools)* |

### Key findings on task-type routing and handback reminders

- **Scope and branching — not elapsed wall-clock duration — predict Luna reliability.** Luna's first-pass completion across duration bins was non-monotonic: `<2 min` 43/67 (64%), `2–5 min` 10/28 (36%), `5–15 min` 11/17 (65%), `15+ min` 10/17 (59%). Within first turns ≥5 min, Luna completed **17/22 (77%)** of scripted gates (including multiple clean **50–73 minute** Ready gates at 1/20th of Sol's token cost), vs. only **1/4 (25%)** of multi-file edits and **2/6 (33%)** of benchmark/ops runs.
- **Matched same-task retries confirm the scope cliff:** On *MBX Crate Splits* (`t1ae`, `t1f`), Luna returned after ~96s / ~108s with zero implementation (one even after a handback reminder), was closed as stalled, and Sol completed the exact same slices in 672s / 514s. Partial Luna Round 4 fixes on `#15244` also completed cleanly once moved to Sol.
- **Handback checks already exist, with two notable limits:**
  - Workers already receive `"a long wait is not a reason to hand back"` (`mj-core/src/session_instructions.rs:120`), and GLiDE's handback check (`mj-worker/src/worker_runtime/subagents.rs:1114-1187`) nudges once per assignment before letting a second handback through.
  - Across the 20 recorded `handback-reminder-*` missing-report/running-process reminders (16 Luna turns in 13 children: 9 complete, 6 partial, 1 unlinked; 4 Sol turns: 4/4 complete), **19 of 20 named `codex-code-mode-host`** as a live process (and in **11 of 20** it was the *only* surviving process — a false positive when work was already done). Meanwhile, most of Luna's 55 partial handbacks returned without triggering a reminder.

## 4. Resulting Design Decisions

1. **Default `--subagents mjolnir` for fresh session creation** (landed in `b45964fd1` / `f136c222f`): daemon admission (`start_create_session_inner`) resolves omitted `subagents` to `Mjolnir` with an automatic fallback to `default_for(harness)` + notice on typed availability refusals (without overwriting remembered preferences), preserving explicit `mjolnir`/`native`/`none`, `mj move`, managed children/reviewers/oracles, historical serde/SQL defaults, and wiki archive restore; mirrored across `mj go`/`mj simple`, TUI Create, Web Create, `mj acp`, CLI help, and docs.
2. **Escalate stalled/incomplete `straightforward` children to `subtle`** (landed in `b45964fd1` / `f136c222f`): updated `DELEGATION_ROUTING` and `TWO_TASK_TYPES` to escalate repeated `straightforward` stalls/failures to `subtle` when available instead of local takeover.
3. **Keep review tools and judgment top-level, delegate accepted follow-up execution:** review tools stay top-level-only because primary judgment is required to distinguish targeted fix vs. scope creep vs. design rethink. Delegation instructions (`subagent-delegation.md` and `DELEGATION_ROUTING`) state the ownership principle without brittle rules: keep fix-scope / override / rethink decisions on the parent, then delegate resulting edits, fixture fixes, and merge conflicts beyond a few calls—preferring `send_message` to resume the slice's child rather than editing locally and using a child only to rerun tests.
4. **Route by scope and branching, not brief length or command runtime:** updated `TWO_TASK_TYPES`, `ONE_TASK_TYPE`, `SPAWN_TWO_TASK_TYPES`, `SPAWN_ONE_TASK_TYPE`, `task_type` parameter description, and skill/config docs to keep `straightforward` for bounded single-path work (locating/mapping code, exact build/test/gate commands, small mechanical edits) and route multi-file implementation (even from a settled design) and multi-stage operational runs or troubleshooting to `subtle`.
