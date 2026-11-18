# Review setup comparison: 3× Terra/high vs Terra/high + Sol/high

Started 2026-11-13 by session 531e9936 at Jonathan's request. The question:
on real mjolnir bugs, does a native mj review with three gpt-6.1-terra/high
reviewers catch more known defects than one with gpt-6.1-terra/high plus
gpt-6.1-sol/high? The two setups cost about the same; equal reviewer count was
not a goal.

Run data (instance, sessions, transcripts, verdicts) is outside the
repository in `/mnt/optane/review-arms-20261113` (`REVIEW_ARMS_DIR`). These
files are the scripts, specifications and corpus.

## Method

- **Corpus** (`corpus/`): `Hard-won:` test markers traced to the commit that
  introduced each defect, before 2026-11-01. No reviewers were configured
  before 2026-11-05 (dated config backups), so neither model had filtered
  these bugs. Three agents built 40 candidates under `corpus/SPEC.md`;
  `corpus.jsonl` holds the 30 selected, in seeded random order, and
  `dropped.json` gives the reason for each of the other 10.
- **Runs** (`driver.py`): a private mj instance (`mjx` wraps the installed
  `mj` with a clean environment) whose four Codex profiles point at the real
  Codex homes, so mj load-balances reviewers across them. For each bug and
  setup, the driver creates a carrier session on the local Podman target with
  `--at <intro> --base <parent> --no-later-history`, sets the review start to
  the parent, sends the commit message (trailers removed) as the task, runs
  `mj review start`, and captures each reviewer's handback, transcript and
  usage. Every step writes a file; a started job resumes from its files.
- **Isolation**: no later commits, branches, remotes or alternates; build
  caches off (they hold later mjolnir builds); no project memory, GitHub
  credentials or events; a private instance, so no session history to search.
  `inventory.py` records the checkout before and after each review.
- **Audit** (`audit.py`): scans every reviewer tool call for network or
  GitHub access, history escapes, paths outside the workspace, memory or
  session tools, and builds or tests, in both its arguments and its output
  (command output is in each item's `terminal_outputs`). Hits are read by
  hand; output hits are usually repository text that mentions such paths.
  The command patterns are a heuristic list. From stage 2 on, the inventory
  also records ignored files, so a build or test run that creates new
  ignored output in the fresh checkout (`target/`, `__pycache__/`) shows up.
- **Judging** (`judge/SPEC.md`): one judge per batch of bugs sees each bug's
  defect record and its reviewers' outputs under random IDs, and labels each
  output caught, partial or missed. Every verdict is then checked by hand.

## Problems found while setting up

- `mj new --at X --base B` records the review start at `X`, not `B`
  (`refs/mj/session-start/...` resolves to `X`; `B` is only `refs/mj/base`),
  so `mj review start` would review an empty diff. The driver moves the start
  ref to `B` before each review. This is likely an mj defect.
- A finished review is delivered to the carrier and then cleared from
  `mj review status`, so the verdict field can come and go between polls.
  The driver treats "cleared after its reviewers were seen" as finished.
- Review results reach the carrier as mailbox messages, one partial result per
  reviewer and then the combined review, and the DSH carrier often acted on
  them: reading files, editing, and once starting a background script.
  Reviewers read the carrier's workspace, so this could change what a
  still-running reviewer sees. In stage 1 only read-only calls happened before
  the final delivery (`carrier_check.py`). From stage 2 on, the driver puts
  the carrier in plan mode with web fetch off before the review starts, and
  interrupts any carrier turn afterward.
- The carrier must be told that the message is context for a separate
  reviewer. Wording addressed to "you" made it review the change itself.

## Stage 1 result (10 bugs, 50 reviewer outputs)

- **Floor effect.** One of 50 outputs caught its known bug: Sol found that
  worker bootstrap drops the container image's Rust toolchain variables
  (`container-env-lost-during-worker-bootstrap`). No output was partial.
  Setup A (3× Terra) caught 0 of 10 bugs; setup B (Terra + Sol) caught 1 of
  10. All 50 verdicts were checked by hand.
- **Repeat Terra runs mostly report the same findings.** Grouping
  near-duplicates by hand, setup A produced 26 distinct findings over the 10
  bugs and setup B 27. These findings are not verified as real bugs.
- **Cost.** Tokens per reviewer: Terra median 207k, Sol median 405k, mostly
  cached reads. Per review that is about 654k for A and 640k for B.
- **Audit.** No forbidden access. Two regex hits were false positives: a
  Markdown link example inside a finding, and repository source text that
  mentions `/var/lib/hel`. The checkout was unchanged after every review.

With about 2% recall per reviewer, 30 bugs would give only a few catches in
total. That cannot separate the two setups. Stage 2 was paused for
Jonathan's decision after one run (`handback-reminder-stops-work`, setup B),
which has not been judged.

## Pooled-findings result (stage 1, verified by Opus/high)

Jonathan chose to score the findings the stage 1 reviews already produced.
`verify/SPEC.md`: the 96 findings from the 50 outputs were split per finding,
shuffled with random IDs (no reviewer, model or setup labels), and given to
five Opus/high Claude Code sessions with a checkout holding history through
2026-11-13. Each verifier grouped duplicates per commit and marked each group
confirmed, plausible or rejected, with a trace at the introducing commit.

- 33 distinct problems: 27 confirmed, 2 plausible, 4 rejected. Twelve
  confirmed groups cite a later commit that fixed them. All 14 cited SHAs
  exist, come after the introducing commit, and match the claim. The known
  Hard-won bug was confirmed with its actual fix commit (`af22566c5`).
- `score.py` scores per reviewer session (`verify/sessions.csv`,
  `groups.csv`, `session_groups.csv`), so setups can be composed afterward.
  Each bug has 4 Terra sessions and 1 Sol session.
- Averaged over every combination, Terra + Sol found 23.25 confirmed
  distinct problems across the 10 diffs, against 20.5 for 3× Terra (+13%).
  Per bug, Terra + Sol was ahead on 4, behind on 2, and tied on 4. The
  bootstrap 95% interval of the total difference is −1.5 to +7.75, so this
  is not significant.
- Terra runs repeat each other: 16 of the 21 confirmed problems Terra found
  were found by at least 3 of its 4 sessions. Sol found 6 confirmed problems
  no Terra session found, including 1 of the 3 high-severity ones (the known
  bug). Sol missed 5 problems that at least 3 Terra sessions found. A single
  session found 17.0 (Terra, mean) and 16 (Sol) confirmed problems.
- Per-bug differences have a standard deviation of about 0.8 problems; at
  the observed mean difference (0.28 per diff), about 65 diffs would be needed
  for 80% power.

## Stage 2 result: 70 diffs, latency and findings (verified by Opus/high)

Jonathan asked for about 65 more diffs, a 3× Ember/high setup, and the
latency-versus-findings Pareto frontier over combinations of up to three
reviewers. He then dropped Ember after its pilot, because it was too slow to
be useful: 3 to 4 minutes and 1.3M to 2.4M tokens per session, against about
45 seconds and 0.25M tokens for Terra. The one finished Ember review is kept
in `runs-ember/` and left out of the analysis.

Method:

- 70 diffs: the 10 from stage 1, the other 20 Hard-won diffs, and 40
  ordinary commits from before 2026-11-01, sampled by `select_diffs.py` from
  2,087 eligible commits (`corpus/diffs.jsonl`).
- Every diff has at least 3 Terra/high and 3 Sol/high sessions (setups A, D
  and D2, plus stage 1's B): 431 sessions in all, 221 Terra and 210 Sol.
  Sessions are independent, so combinations are built from them afterward.
- The carrier is put in plan mode with web fetch off before its first message.
  It sometimes lists files, so the driver now requires an unchanged checkout
  after its turn rather than no tool use.
- `pipeline.sh` verified diffs as their reviews finished:
  `build_verify.py --append --ready-jobs` built blind batches, and
  `verify_driver.py` ran one Opus/high Claude Code session per batch,
  round-robin over `claude` to `claude7`. `claude6` was dropped after it
  failed with ACP `auth_required`. There were 32 batches, and all 10 stage 1
  diffs were verified again from scratch.
- One review was refused at start with "relay hello timed out after 60
  seconds" and retried. The private instance's image was re-pinned from
  `d12b0a38` to `aa3e7f32` after the old one was pruned. The harness pins
  were unchanged (codex 0.168.0, codex-acp 1.15.0).
- Latency is each reviewer's own turn, from its prompt to the end of the
  turn. An option's latency is the median over diffs of its slowest member.
  This assumes members start together, and it excludes session startup.
  Cost is mj's estimated API price, at 10^10 ticks per dollar.

Results (`results/`):

- 200 distinct problems: 156 confirmed, 21 plausible, 23 rejected.
- Verifier consistency on the 96 findings verified twice: 93.8% got the same
  verdict, and 95.8% agreed on confirmed versus not confirmed. Severity
  matched 72.8% of the time, and every disagreement was off by one level.
- Frontier on confirmed problems (`results/pareto.md`):

  | Reviewers | Median latency (s) | Confirmed (70 diffs) | Est. cost per diff ($) | Frontier share |
  |---|---:|---:|---:|---:|
  | Terra | 43.1 | 84.9 | 0.044 | 1.00 |
  | 2× Terra | 47.3 | 106.3 | 0.087 | 1.00 |
  | 3× Terra | 51.7 | 117.3 | 0.131 | 1.00 |
  | Sol + Terra | 91.9 | 120.3 | 0.211 | 0.64 |
  | Sol + 2× Terra | 93.7 | 134.5 | 0.255 | 1.00 |

  Off the frontier: Sol (90.2 s, 79.0), 2× Sol (100.0 s, 98.7), 2× Sol +
  Terra (101.1 s, 132.3) and 3× Sol (103.6 s, 109.0). The frontier share is
  how often an option stays on the frontier over 1,000 bootstrap resamples of
  the diffs.
- Paired differences against 3× Terra, with bootstrap 95% intervals
  (`compare.py`):
  - Terra + Sol: +3.0 confirmed (−7.3 to +13.4), but +10.4 medium- or
    high-severity confirmed (+2.6 to +18.8).
  - Sol + 2× Terra: +17.3 confirmed (+8.2 to +26.8) and +16.8 medium or
    high (+9.1 to +25.4).
  - On tokens, 3× Terra and Terra + Sol cost about the same (735k and 679k
    per diff). On estimated price they do not ($0.131 and $0.211).
- Severity: a single Sol session found 10.0 confirmed high-severity problems
  against Terra's 4.7 (+1.3 to +10.0). Counting only medium and high, 2× Sol
  + Terra also joins the frontier (`results/pareto-sev-high-medium.md`).
- Known bugs (`knownbug.py`): the known defect appears among the verified
  groups for 12 of the 30 Hard-won diffs (`results/known-bug.jsonl`).
  Expected catches out of 30: Terra 6.7, Sol 8.3, 3× Terra 9.0, Terra + Sol
  10.6, Sol + 2× Terra 11.3.
- `results/unfixed-at-head.csv`: 62 confirmed problems whose evidence says
  they are still present at HEAD (2026-11-13). This comes from a text match
  on the verifier's evidence and has not been checked.

Limits: the verifier is a model, and some low-severity "confirmed" groups
are only lost test coverage. Latency was measured while about 10 reviews
shared four Codex profiles, so it includes provider load at that time.

## Stage 3 result: Sol/medium in place of Sol/high (verified by Opus/high)

Jonathan asked how much faster Sol/medium is than Sol/high, and how much it
loses when paired with one or two Terra/high reviewers.

Method:

- Setup E: 2 Sol/medium sessions on each of the 70 stage 2 diffs, 140 in all.
  Everything else matches stage 2. The private daemon replaced itself with the
  installed build (2.71.0+bec7a9a9) at the first command; nothing between the
  two builds touches review code, and reviewers run with `subagents: none`, so
  the changed delegation text does not reach them.
- All 70 diffs were verified again from scratch in `verify3`, with every
  session's findings pooled: 571 sessions, 697 findings, 32 batches. Grading
  only the new findings against stage 2's groups would have judged Sol/medium
  by a different procedure, with a verifier that knew which findings were new.
  8 diffs had no findings from any session and needed no batch.
- Four verifier turns on `claude5` stopped with `stop_reason: QuotaLimit`.
  `verify_driver.py` now sets such a session aside and runs the batch again in
  a fresh session on a profile that has not hit its limit.
- The audit found nothing. All 70 checkouts were unchanged, and every hit was
  a false positive: words like "origin" in search patterns, URLs in findings,
  the handback call, and one `git diff` of the checkout's own base and head
  trees.

Results (`results/stage3/`):

- 200 distinct problems: 160 confirmed, 11 plausible, 29 rejected. The
  Terra-only and Sol/high options reproduce stage 2 within 2 confirmed (for
  example Sol/high + 2× Terra 136.2 against 134.5). On the 597 findings
  verified in both passes, 89.1% got the same verdict and 91.6% agreed on
  confirmed versus not.
- Per session: Sol/medium has a median latency of 46.0 s (90th percentile
  66 s), against 90.9 s (141 s) for Sol/high and 42.7 s for Terra/high. It
  uses 233k tokens against 398k, and costs an estimated $0.100 against $0.168.
  Alone, it finds 48.5 confirmed problems against Sol/high's 80.7 (−32.2, 95%
  −43.0 to −22.2), and 8.5 high-severity problems against 11.0.
- Paired with Terra/high (`compare.py`, 95% bootstrap intervals over diffs):

  | Option | Median latency (s) | Confirmed | Medium or high | Est. cost per diff ($) |
  |---|---:|---:|---:|---:|
  | Sol/high + Terra | 91.8 | 122.3 | 84.1 | 0.211 |
  | Sol/medium + Terra | 52.1 | 99.2 | 68.1 | 0.143 |
  | 2× Terra | 47.3 | 106.8 | 71.0 | 0.087 |
  | Sol/high + 2× Terra | 93.9 | 136.2 | 91.5 | 0.255 |
  | Sol/medium + 2× Terra | 54.3 | 116.0 | 77.8 | 0.187 |
  | 3× Terra | 51.7 | 117.5 | 76.8 | 0.131 |

  - With one Terra, Sol/medium in place of Sol/high saves 40 s and loses 23.0
    confirmed (−31.9 to −14.9) and 15.9 medium or high (−23.7 to −9.2). It also
    loses to 2× Terra by 7.6 confirmed (−13.5 to −1.9) while being slower and
    costlier, so it is off the frontier.
  - With two Terra, Sol/medium saves 40 s and loses 20.2 confirmed (−28.3 to
    −12.9) and 13.7 medium or high (−20.7 to −7.6). Against 3× Terra it is a
    tie: −1.5 confirmed (−6.1 to +3.1) and +1.1 medium or high, for 2.6 s more
    and $0.056 more per diff. Counting only medium and high, it joins the
    frontier (share 0.74).
  - High severity: Sol/medium loses about 2.2 high-severity problems to
    Sol/high in both pairings, but finds more than the Terra it replaces
    (Sol/medium + 2× Terra has about 2.2 more than 3× Terra).
- Known bugs (`knownbug.py --groups-from verify2`, because `known-bug.jsonl`
  names stage 2 groups): expected catches out of 30 are 9.7 for Sol/medium +
  Terra (8.3 for 2× Terra, 10.4 for Sol/high + Terra), and 10.7 for Sol/medium
  + 2× Terra (9.0 for 3× Terra, 11.3 for Sol/high + 2× Terra). Only 12
  Hard-won diffs have a matching group, so these differences are small.

## Stage 4 result: xhigh, Greptile and non-GPT reviewers (verified by Opus/high)

Jonathan asked to compare Greptile's Base and Plus tiers, higher efforts, and
reviewers other than GPT models, and to find the best combination of up to
three that is not too slow or expensive.

Method:

- New sessions on all 70 diffs:
  - Terra/xhigh and Sol/xhigh, 2 each (arms H and K; the first J jobs also
    carried 2 DSF/low reviewers).
  - Ember/xhigh, Gemini 4 Hydrogen/high and Kimi K3.1 Pro/high, 1 each (arm L).
    Muse's highest effort is xhigh.
  - Greptile Base and Plus, 1 each, through `greptile_driver.py`.
- **Greptile** runs through its CLI (`greptile review --branch <base> --json`,
  plus `--plus`) on a named branch at each diff's commit. Notes:
  - Greptile allows one review per commit at a time and cancels the older one,
    so a diff's tiers run one after another.
  - About 50 dispatches in a few minutes got HTTP 403 for a while. Runs that
    were already dispatched are fetched with `greptile review show` rather than
    paid for again; runs that Greptile cancelled or failed are dispatched again.
  - Latency comes from Greptile's own `createdAt` and `completedAt`, within
    1.5 s of the CLI's wall time. Cost is the list price, $1 (Base) or $3 (Plus).
  - Its context cannot be pinned to the diff's commit, and an org default rule
    reads today's AGENTS.md. Any leak can only help Greptile.
- **Dropped:**
  - DSF/high (4 to 7 minutes per review; kept in `runs-dsf-high/`).
  - DSF/low, which was no faster (126 to 561 s) and handed back in 12 of 35
    runs.
  - GLM 5.3 and GLM 5.3 Flash (about 450 s each, and Z.ai rate-limited 12
    concurrent sessions; kept in `runs-glm/`).
  - DeepSeek on DSH ends its turn with its findings instead of calling
    handback, which leaves mj's review incomplete. The driver scores a final
    message when there is no handback and records `handback: false`.
- `verify5` pooled every session of this study (1,236 sessions, 1,928 findings)
  in one blind pass and gave 415 problem groups: 288 confirmed, 36 plausible,
  91 rejected. `build_verify.py --arms` keeps out arms OT, OS, BT and BS, which
  belong to another experiment in the same data directory: their reviewers get
  extra static-analysis context.
- Audit (`results/stage4/audit-review.md`): no later-history leak. One rule
  violation: an Ember session compiled and ran two scratch programs with
  `rustc`.

Per reviewer (`results/stage4/per-reviewer.md`):

| Reviewer | Median latency (s) | Confirmed per diff | Medium or high | Rejected | Est. cost ($) | Handed back |
|---|---:|---:|---:|---:|---:|---:|
| Greptile Plus | 266 | 2.34 | 1.33 | 15% | 3.00 | n/a |
| Terra/xhigh | 84 | 2.15 | 1.31 | 15% | 0.093 | 100% |
| Sol/xhigh | 214 | 1.77 | 1.27 | 2% | 0.330 | 100% |
| Greptile Base | 147 | 1.64 | 0.93 | 8% | 1.00 | n/a |
| Terra/high | 43 | 1.24 | 0.81 | 15% | 0.044 | 100% |
| Sol/high | 91 | 1.14 | 0.90 | 3% | 0.168 | 100% |
| Gemini/high | 200 | 1.04 | 0.71 | 2% | unpriced | 69/70 |
| Kimi/high | 340 | 0.93 | 0.66 | 10% | unpriced | 63/70 |
| Sol/medium | 46 | 0.69 | 0.55 | 1% | 0.100 | 100% |
| Ember/xhigh | 292 | 0.41 | 0.29 | 11% | unpriced | 100% |

Frontier on confirmed problems (`results/stage4/pareto.md`; caps of 1 or 2
sessions for the new configurations):

| Reviewers | Median latency (s) | Confirmed (70 diffs) | Est. cost per diff ($) | Frontier share |
|---|---:|---:|---:|---:|
| 3× Terra/high | 52 | 118 | 0.131 | 1.00 |
| Terra/xhigh | 84 | 150.5 | 0.093 | 0.36 |
| Terra/high + 2× Terra/xhigh | 94 | 188.3 | 0.230 | 0.94 |
| Sol/high + 2× Terra/xhigh | 103 | 191 | 0.353 | 0.81 |
| Greptile Base + 2× Terra/xhigh | 151 | 213 | 1.186 | 1.00 |
| Greptile Plus + 2× Terra/xhigh | 266 | 236 | 3.186 | 1.00 |

On medium and high severity only, Sol/medium + 2× Terra/xhigh (93 s, 105) and
Sol/xhigh + 2× Terra/xhigh (212 s, 120) join the frontier.

- 2× Terra/xhigh finds 181 confirmed against 3× Terra/high's 118 (+63, 95%
  +43 to +84) in 93 s, and beats Greptile Plus alone (164).
- A third reviewer added to 2× Terra/xhigh (`results/stage4/third-reviewer.txt`):
  Greptile Plus +55, Sol/xhigh +30.5, Gemini +21 (+11 to +33; +12 medium or
  high), Kimi +15, Sol/high +10, Terra/high +7, Ember +5. Gemini is the best
  non-GPT addition but takes 200 s.
- Known bugs (`results/stage4/knownbug.txt`): out of 30, Greptile Plus alone
  catches 12, Terra/xhigh 9, 2× Terra/xhigh 10, and adding Gemini, Sol/high,
  Sol/xhigh or Greptile Base to 2× Terra/xhigh gives 11.

## Missing-fact labels (for a static-analysis hypothesis)

Jonathan asked, on 2026-11-14, for the one fact each defect's reviewer needed
that was not in the diff, labeled by kind, to test whether a static analysis
tool can improve review.

- Items (`label_facts.py build`): the 30 Hard-won diffs with their known
  defect, and the 31 sampled diffs that have a confirmed problem in `verify3`,
  each with its most severe one. The 9 sampled diffs with no confirmed
  problem were left out at his request.
- Labeling (`label/SPEC.md`, `label/workflow.js`): a Claude Code workflow of
  13 Opus/high agents, one per batch of 5, working in this repository. Each
  item gets a defect check, the fact with `file:line` at the intro commit, its
  kind (affected callers, path, value flow, or none with a reason: in_diff,
  intent or external), a second kind when two fit, and the tool query that
  would have produced the fact. 1.13M tokens, 4.7 minutes.
- `results/missing-facts.jsonl` (`label_facts.py collect`). All 61 defects
  were confirmed at their intro commit.

  | Kind | Hard-won | Sampled | All |
  |---|---:|---:|---:|
  | affected callers | 3 | 8 | 11 |
  | path | 6 | 6 | 12 |
  | value flow | 9 | 7 | 16 |
  | none: in the diff | 5 | 6 | 11 |
  | none: outside the repository | 7 | 4 | 11 |
  | none: intent | 0 | 0 | 0 |

- 39 of 61 (64%) need a repository fact a code-analysis tool could supply.
  45 of the 61 labels name a second kind (path/value flow 13, callers/value
  flow 10, callers/path 8; 14 pair a tool kind with none), so the split among
  the three tool kinds is less certain than the tool versus none split. Each label is a single Opus judgment; agreement between
  labelers has not been measured.

Second pass on the 11 labeled "none: in the diff" (`label/SPEC-pass2.md`,
`label/workflow-pass2.js`, `results/missing-facts-pass2.jsonl`). These
defects were visible yet committed, so Jonathan asked again what fact a
static analysis tool could have stated explicitly. One Opus/high agent per
item also wrote the tool's output and rated how specific it would be.

- 10 of 11 get a tool-statable fact: value flow 7, path 2, affected
  callers 1. Specificity: high 3 (a focus handler hiding its focused button;
  a new field whose only read is an emptiness check; `unused_mut` under the
  Windows cfg that CI lints), medium 7. The one `none` is
  `palette-scattered-match-beats-literal`, a scoring-weight error (`logic`).
- Combined with the first pass, 49 of 61 defects have a fact a tool could
  surface; 11 depend on behavior outside the repository and 1 on logic.
- The second-pass question asks for a fact to surface, which invites an
  answer; several outputs assume a tool that joins facts across files and
  languages, and medium-specificity outputs still need the reviewer to judge
  intent.

## Relation briefs, stage A (oracle): does a reviewer act on the decisive fact?

Plan, frozen sets and decision rule: `brief/PLAN.md` (committed before any
run). Results: `results/brief-stage-a/`.

- Briefs: a gpt-6.1-sol/high writer per Hard-won diff, with the reviewers'
  isolation, wrote a blind relation brief (`brief_driver.py`). It fully held
  the decisive fact for 7 of 18 primary diffs and partly for 8. An Opus
  editor inserted the census relation where it was absent, a leak judge read
  each brief as a review output, and every carrying item was checked by hand
  (`brief_oracle.py`). One rewrite removed the decisive fact and that diff
  was rerun. 13 of 22 primary and secondary oracle briefs are still judged
  partial (12) or caught (1) as text: for these the neutral relation itself
  points at the defect, and the plan keeps it.
- Runs: oracle arms `OT` (3 Terra/high) and `OS` (3 Sol/high) on all 30
  diffs, 180 sessions; every reviewer received the brief and every audit hit
  was a false positive. Baseline: the stored no-brief Terra/high and
  Sol/high sessions on the same diffs (191).
- Judging: all 371 outputs in blind mixed batches, two Opus/high judges
  (366 of 371 agree); the 138 caught or partial verdicts and 5
  disagreements were checked by hand (the 5 became partial).

Catch rate per session (share of a diff's sessions that caught its defect,
mean over diffs, 95% bootstrap interval over diffs):

| Set | Model | No brief | Oracle brief | Effect (points) |
|---|---|---:|---:|---:|
| Primary (18) | Terra/high | 20.4% | 59.3% | +38.9 (+16.7 to +61.1) |
| Primary (18) | Sol/high | 22.2% | 48.1% | +25.9 (+7.4 to +46.3) |
| Secondary (4) | Terra/high | 33.3% | 50.0% | +16.7 |
| Secondary (4) | Sol/high | 25.0% | 25.0% | 0 |
| Control (8) | Terra/high | 20.8% | 16.7% | −4.2 |
| Control (8) | Sol/high | 37.5% | 25.0% | −12.5 |

- Decision (`brief_score.py score`): **supports building**. Terra's effect
  is at least 30 points and control 1 passes (no gain on control diffs), so
  stage B runs.
- Not only leakage: on the 8 primary diffs whose oracle brief the leak judge
  rated missed, the effect is +29.2 points for Terra (+4.2 to +62.5) and
  +33.3 for Sol (0 to +70.8).
- The effect is concentrated: 7 diffs move from rarely caught to mostly or
  always caught; on 6 diffs no reviewer acted on the fact even with it in the
  prompt (`per-diff.jsonl`).
- Expected catches of 30 for one reviewer: Terra 6.7 → 14.0, Sol 8.0 → 11.7
  with the oracle brief (stored Sol + 2× Terra without a brief: 11.3).
- Cost: medians per session, tokens Terra 260k → 189k, Sol 484k → 431k;
  latency Terra 44 s → 32 s, Sol 103 s → 100 s; tool calls Terra 8 → 5.
- Limits: the oracle guarantees the fact, so this is an upper bound for a
  real tool. The baseline sessions ran one day earlier with the same harness
  pins. Judges could sometimes tell from an output that it used a brief.
  18 primary diffs give wide intervals.

## Relation briefs, stage B (blind): does a brief written without the answer help?

Stage A's rule said supports building, so stage B ran (`brief/PLAN.md`).
Results: `results/brief-stage-b/`.

- Briefs: the stage A blind briefs for the 30 Hard-won diffs, after a leak
  check (18 missed, 3 partial, 1 caught as text; 4 rewritten to remove
  consequence wording only), and new blind briefs for the 40 ordinary diffs.
  Every leak verdict is bound to a hash of the text its judge read.
- Runs: `BT` (3 Terra/high) and `BS` (3 Sol/high) on the 30 Hard-won diffs,
  `BT` on the 40 ordinary diffs: 300 sessions, all with the brief, all
  checkouts unchanged, every audit hit a false positive. One reviewer's model
  was recorded from its usage and launch after its session suspended before
  capture (`runs/container-env-lost-during-worker-bootstrap/BS/reviewers-repaired.json`).
- Judging round B: no-brief, oracle and blind sessions together (551
  outputs), two Opus/high judges agreeing on all; 210 caught or partial
  verdicts checked by hand. Round B reproduces stage A's oracle numbers
  exactly.

Hard-won catch rate per session (mean over diffs, 95% interval of the effect):

| Set | Model | No brief | Blind brief | Oracle brief |
|---|---|---:|---:|---:|
| Primary (18) | Terra/high | 20.4% | 55.6% (+35.2; +14.8 to +55.6) | 59.3% |
| Primary (18) | Sol/high | 22.2% | 40.7% (+18.5; +1.9 to +37.0) | 48.1% |
| Secondary (4) | Terra/high | 33.3% | 41.7% | 50.0% |
| Secondary (4) | Sol/high | 25.0% | 25.0% | 25.0% |
| Control (8) | Terra/high | 20.8% | 12.5% | 16.7% |
| Control (8) | Sol/high | 37.5% | 33.3% | 25.0% |

- The blind brief keeps about 90% of the oracle effect for Terra and 70% for
  Sol, with no gain on control diffs (control 1 passes for both briefs).
- Expected catches of 30 for one reviewer: Terra 6.7 → 12.7 with a blind
  brief (14.0 oracle); Sol 8.0 → 11.0 (11.7). One Terra/high with a blind
  brief (12.7) exceeds stored Sol + 2× Terra without one (11.3).
- Reviewer cost falls: Terra median tokens 260k → 199k, latency 44 s → 35 s,
  tool calls 8 → 5. The brief itself came from a Sol/high writer session
  (about 5 minutes and 560k tokens per diff), which a tool would replace.

Control 2, ordinary diffs (`brief_control2.py`; findings of the stored
no-brief Terra/high sessions and the blind-brief ones pooled and verified
blind in `verify-brief`, 82 groups: 57 confirmed, 7 plausible, 18 rejected):

- Confirmed problems per session: 0.917 without a brief, 0.808 with it
  (ratio 0.88; difference −0.11, 95% −0.25 to +0.04). **Control 2 fails** on
  the 90% threshold by point estimate; the difference is not significant.
  High-severity problems are equal (0.075 per session in both); medium falls
  0.36 → 0.28 and low 0.48 → 0.45. Across a diff's three sessions, 48
  distinct confirmed problems without a brief, 45 with it.
- Rejected findings rise from 0.125 to 0.167 per session. None cites a brief
  anchor outside the changed lines; 14 cite a brief anchor when changed-line
  anchors count too. Read by hand
  (`results/brief-stage-b/control2-citing-handmarks.json`), 6 of the 14 rely
  on a relation a brief item states, within the limit of 8, so the citing
  half of control 2 passes. Five of those six were rejected because the
  behaviour predates the diff, not because it is false: the brief led
  reviewers to real behaviour outside the change.
- Reading: on ordinary diffs the brief costs a little coverage of low and
  medium problems and adds a few false findings, consistent with reviewers
  exploring less (fewer tool calls). The no-brief sessions ran a day earlier.

## Relation briefs, stage C (Bifrost): does a brief from today's tool help?

Plan `brief/PLAN-bifrost.md` (#13150). Results: `results/brief-stage-c/`.
Decision: **site context needed**. The brief does not help as built.

- Briefs: Bifrost's `relation_brief` (#13149) for the 30 Hard-won and 40
  ordinary diffs, from bifrost-dev 2e76c40839: the head of PR #13409, whose
  brief code merged unchanged as a80fdbe025. The first run, on 0.15.3, was
  discarded before judging. #13398 dropped proved callers when it trimmed a
  scan to 8 KiB, which left 23 briefs empty and 394 items in all. That run's
  briefs and sessions are kept in `briefs/bifrost-0.15.3/` and
  `runs-stageC-0.15.3/`. The rerun has 0 failed briefs, 7 empty and 1189 items,
  and Bifrost marks every item incomplete. The 32 briefs that 479392b9
  generated separately came out byte-identical. A known defect of this binary,
  #13399 (symlinks reported unreadable), empties JS/TS scans on 3 diffs
  (`bifrost-briefs-RUNS.md`). The 2 Hard-won diffs among them caught nothing
  with the blind brief either, so it does not change the decision.
- Runs: `FT` (3 Terra/high) and `FS` (3 Sol/high) on the 30 Hard-won diffs,
  and `FT` on the 40 ordinary diffs: 300 sessions. Each one's context was
  checked to hold the new brief.
- Judging round F: no-brief, blind and Bifrost sessions together (551
  outputs), two Opus/high judges in the blinded verifier checkout
  (`verify_driver.py --kind judge`). The judges agree on 549 outputs. All 173
  caught or partial verdicts and the 2 disagreements were checked by hand:
  an independent pass over all 173, and a reading of every partial and both
  disagreements. Both disagreements became missed (timeout-only findings).
  Round F reproduces stage B's no-brief and blind numbers exactly.

Hard-won catch rate per session (mean over diffs; 95% interval of the effect):

| Set | Model | No brief | Bifrost brief | Blind brief |
|---|---|---:|---:|---:|
| Primary (18) | Terra/high | 20.4% | 20.4% (+0.0; −7.4 to +7.4) | 55.6% |
| Primary (18) | Sol/high | 22.2% | 24.1% (+1.9; −5.6 to +9.3) | 40.7% |
| Secondary (4) | Terra/high | 33.3% | 33.3% | 41.7% |
| Secondary (4) | Sol/high | 25.0% | 25.0% | 25.0% |
| Control (8) | Terra/high | 20.8% | 16.7% | 12.5% |
| Control (8) | Sol/high | 37.5% | 37.5% | 33.3% |

- The blind brief's lead over the Bifrost brief is 35.2 points for Terra
  (16.7 to 53.7) and 16.7 for Sol (1.9 to 33.3). Outcome 2 of the rule holds:
  E is below 15 points for both models and the top of its interval is below
  30. Control 1 passes (control-set gain −4.2 Terra, 0 Sol).
- Per diff (`per-diff-hardwon.jsonl`): the blind brief turned misses into
  catches on terminal-id-reused, held-prompt-ignores-interrupt,
  stale-model-launch-spec, symbol-change-keeps-cached-transcript,
  catalog-cache-key-collision and web-feed-self-deadlock. The Bifrost brief,
  with 5 to 25 callers and paths on each, added one catch per model on
  stale-model-launch-spec and web-feed-self-deadlock and none on the other
  four. It names where the change reaches, not what the decisive relation is.
- Expected catches of 30 for one reviewer: Terra 6.7 without a brief, 6.3
  with the Bifrost brief, 12.7 with the blind one; Sol 8.0, 8.3, 11.0.
- Reviewer cost: Terra median tokens 260k → 231k, latency 44 s → 38 s, tool
  calls 8 → 7; Sol 484k → 519k, 103 s → 126 s, 12 → 12.

Control 2, ordinary diffs (`brief_control2.py --arm FT`; no-brief and
Bifrost-brief Terra/high findings pooled and verified blind in
`verify-bifrost`; 75 groups: 46 confirmed, 12 plausible, 17 rejected):

- Confirmed problems per session: 0.842 without a brief, 0.675 with the
  Bifrost brief (ratio 0.80; difference −0.17, 95% −0.29 to −0.05).
  **Control 2 fails.** Under the plan this is the brief's cost, not a change
  of decision; the blind brief's ratio was 0.88.
- No rejected finding cites a brief anchor outside the changed lines. 3 cite
  an anchor when changed-line anchors count too, and by hand none relies on
  what the brief states (`control2-citing-handmarks.json`). The citing half
  passes.

Does the brief contain the decisive fact (report-only; one Opus/high judge
with the census record and the stage A editor's rule;
`bifrost-contains.jsonl`)? Bifrost: 0 yes, 13 partial, 9 no of the 22 primary
and secondary diffs; blind: 7, 10, 5. Every partial anchors the decisive
location but misses a second relation: state shared between two parties (one
cache key, one lock, a concurrent writer), a counterpart path (an interrupt,
hit-test order, a cache's invalidation key, an error path), or a parameter or
alias. The four diffs where the blind brief contained the fact and turned
misses into catches (catalog-cache-key-collision, stale-model-launch-spec,
symbol-change-keeps-cached-transcript, web-feed-self-deadlock) are partial or
no for Bifrost.

## Relation briefs, ablation: is the decisive relation what the blind brief adds?

Plan `brief/PLAN-ablation.md`, registered before any run. Results:
`results/brief-ablation/`. Outcome: **the relation carries it**.

- Diffs: the 10 where the blind brief lifted catches. Chosen by outcome, so
  the arms compare only with each other, on these diffs.
- Arm D is the stage C Bifrost brief plus the decisive item. The stage A
  editor's census item was rewritten by a carbon/high editor into the Bifrost
  brief's form: anchors, completeness tag, no item longer than the brief's
  longest, and at most two items per diff (8 diffs needed two).
  - Contains check: all 10 state the fact.
  - Leak check: two judges, all `missed`.
- Arm X is the blind brief minus the editor's matching item. Exactly one line
  is removed per diff.
- Runs: 3 Terra/high and 3 Sol/high per diff and arm, 120 sessions.
  mj-estimated API cost $13.32. The codex quota ran out mid-run; 3
  interrupted jobs were relaunched from scratch.
- Judging round G: gemini-4-carbon/high judges on Jonathan's instruction
  (rounds A to F used Opus/high). They rejudged every compared arm on the 10
  diffs, 304 outputs. The two judges agree on all; an independent carbon hand
  check of all 158 caught or partial verdicts changed none. Round G
  reproduces round F's no-brief, Bifrost and blind rates within 3.3 points
  for every arm and model, so there is no judge-consistency caveat.

Catch rate per session on the 10 diffs (mean over diffs):

| Model | No brief | Bifrost (F) | Bifrost + item (D) | Blind − item (X) | Blind (B) |
|---|---:|---:|---:|---:|---:|
| Terra/high | 20.0% | 20.0% | 73.3% | 46.7% | 90.0% |
| Sol/high | 33.3% | 40.0% | 63.3% | 56.7% | 70.0% |

- Closure, (D − F) / (B − F), the share of the gap the decisive item closes
  alone: Terra 0.76 (95% 0.44 to 1.0), Sol 0.78 (0.25 to 1.0). Terra is
  above the plan's 0.6.
- Removing the item from the blind brief costs Terra 43 points (13 to 73)
  and Sol 13 (0 to 33).
- Per diff for Terra (`ablation.json`), the item alone took six diffs to 3/3
  where the plain Bifrost brief caught at most 1/3. On those diffs the blind
  brief without it mostly fell back to 0 or 1 of 3:
  - catalog-cache-key-collision
  - codex-effort-alias-lost
  - stale-model-launch-spec
  - symbol-change-keeps-cached-transcript
  - terminal-id-reused
  - web-feed-self-deadlock
- The item did not help on two diffs, where the blind brief's other items
  carry the catch (blind minus the item still catches):
  - metadata-checkpoint-overwrites-checkout: 0/3 with D, 3/3 without the
    item;
  - child-model-wrong-efforts: 2/3 with D, 3/3.
- Reading: for these bugs, one stated relation of the census kind accounts
  for about three quarters of the blind brief's lift, in Bifrost's own
  format. The rest is the writer's other items. This supports building the
  relations (#13211 site context, shared state first) over brief prose. The
  items here were written by hand from the census; Bifrost does not compute
  them today, so the result says what the engine must state, not that it
  can.

## Relation briefs, dilution: does the decisive fact survive among its siblings?

Plan `brief/PLAN-dilution.md`, registered before any run. Results:
`results/brief-dilution/`. Outcome: **selection is not the bottleneck**.

- Arm W is arm D's brief with every true fact of six kinds added. The kinds
  are argument values at call sites, field and state writes, held scopes,
  keys and comparisons, lifetimes, and shared state, for the functions the
  diff changes and the call sites the brief cites. No budget.
  - One blind carbon/high editor per diff listed 91 to 174 facts, and 1,550
    for the 43-file symbol-change-keeps-cached-transcript.
  - A blind truth check dropped 0 to 6.7% per diff as false.
  - Recall of the decisive fact: the editors stated it on 1 of 10 diffs
    (web-feed-self-deadlock, a held lock at a call). On the other 9 they had
    only pieces of it: the counterpart outside the diff's functions (a
    cache, a writer, a restart loop), or the link that joins true pieces.
  - Briefs grew from 4-8 KB to 30-52 KB. The 300 KB symbol-change brief was
    refused by the mj API (413), so W ran on 9 diffs and every arm is
    compared on those 9.
  - Leak check: every caught or partial verdict of the two Opus judges
    quotes arm D's own item, none a sibling, so no sibling was dropped.
- Runs: 3 Terra/high and 3 Sol/high per diff, 54 sessions, mj-estimated API
  cost $6.72. The daemon replaced on 2026-11-16 destroys reviewer sessions
  when their role ends (mj 8fe3c40bd), so driver.py now reads each handback
  from the store. Every deviation is listed in
  `results/brief-dilution/deviations.md`.
- Blinding: with no reviewer transcript, `audit.py` cannot check these
  reviewers' tool calls and reports them as unaudited. What still holds is
  the checkout inventory check (no later history in the reviewer's
  checkout), which passed for every run.
- Judging round H: gemini-4-carbon/high, 220 outputs. The judges agree on
  all; the hand check of 104 caught or partial verdicts changed none. Round
  H reproduces round G's D and F exactly.

Catch rate per session on the 9 diffs (mean over diffs):

| Model | Bifrost (F) | Bifrost + item (D) | D + siblings (W) | Blind (B, round G) |
|---|---:|---:|---:|---:|
| Terra/high | 22.2% | 70.4% | 81.5% | 88.9% |
| Sol/high | 33.3% | 59.3% | 59.3% | 66.7% |

- Retention, (W − F) / (D − F): Terra 1.23 (95% 1.0 to 1.9), Sol 1.0 (0.25
  to 3.0). Terra is above the plan's 0.7.
- Reading: on these diffs a brief that states every fact of these kinds
  keeps the decisive one's lift; W ≥ D may come from siblings that state
  pieces of it. The open questions are elsewhere: whether the engine
  computes these facts on real code, and whether a complete listing fits on
  a large diff.

## Wizard-of-Oz site tool: do reviewers fetch the decisive fact when they must ask?

Plan `brief/PLAN-oz.md`, registered before any run. Results:
`results/brief-oz/`. Outcome (Terra, primary): **querying fails**, because
Terra never asked.

- Arm Q: no brief. Reviewers have one MCP tool, `site_context`, which
  answers for a function or `path:line` from arm W's facts (the truth-checked
  siblings plus arm D's item), with completeness per answer. It is a
  stand-in server reached by a forced-command ssh. The fixtures live only on
  the server host; a blinding probe from a reviewer container could not read
  them. Run from a second mj instance, so no other arm sees the tool.
- Retrieval, from the server's call log:

  | Model | Sessions | Called the tool | Calls | Got the decisive item |
  |---|---:|---:|---:|---:|
  | Terra/high | 30 | 0 | 0 | 0 |
  | Sol/high | 30 | 26 | 71 | 9 |

  Every Terra reviewer loaded the tool and listed it, then never called it.
- Runs: 60 sessions on 10 diffs, mj-estimated API cost $6.87. Judging round
  I (carbon/high, 244 outputs: Q, D, F and no brief) agrees on all; the hand
  check of 104 caught or partial verdicts changed none. W's labels come from
  round H, on its 9 diffs.
- Blinding: as for arm W, reviewer tool calls are unaudited (no transcript).
  The fixtures were outside every reviewer's reach (probe before the run),
  the checkout inventory check passed, and the server log shows only
  `site_context` calls.

Catch rate per session on the 10 diffs (mean over diffs):

| Model | No brief (N) | Bifrost (F) | Site tool (Q) | Bifrost + item (D) |
|---|---:|---:|---:|---:|
| Terra/high | 20.0% | 20.0% | 26.7% | 73.3% |
| Sol/high | 33.3% | 40.0% | 53.3% | 63.3% |

- Terra: Q − N +6.7 points (−6.7 to +20), Q − D −46.7 (−73.3 to −16.7).
- Sol: Q − N +20.0 (+6.7 to +36.7), Q − D −10.0 (−26.7 to +3.3), Q − W on
  the 9 diffs −11.1 (−29.6 to +7.4). By the plan's rule Sol alone would
  read "querying works" (Q ≥ D − 10).
- Reading: when the reviewer asks, the facts behind a query tool deliver
  most of what handing them over does (Sol). Whether it asks depends on the
  model: Terra/high, the best reviewer in this study, ignores an optional
  tool. A tool-only design needs either a review procedure that makes the
  reviewer query the changed code's sites, or the facts pushed into the
  brief as well; that needs its own registered arm.

## Bifrost tool coverage: can a code graph produce the decisive fact?

For the Bifrost session, at Jonathan's request (`toolcov/SPEC.md`,
`toolcov/workflow.js`, `results/tool-coverage.jsonl`): for each relation in
the census, a Sonnet/high agent asked Bifrost 0.15.2 (latest release) a
diff-only question at the introducing commit and classed the result.

| Set | A: computed today | A0: anchor, relation not stated | B: graph relation not exposed | C: outside a code graph |
|---|---:|---:|---:|---:|
| Primary (18) | 0 | 9 | 7 | 2 |
| Secondary (4) | 0 | 3 | 1 | 0 |

- A means the output states the decisive relation (a reviewer review added A0
  for outputs that contain the anchor but not the relation). No record is A.
  The probe classed `github-token-in-container-argv` A (the field scan shows
  `extra_run_args` extended into `container_run_args`), but the census fact
  also needs those arguments to become a logged `CommandSpec`, which the
  output does not show, so it is A0.
- A0 outputs usually list the right caller or field use among others but not
  the part that matters: the constant argument, the lock held at the call,
  read versus write, or which branch a call sits in. Two A0 anchors came only
  from `query_code` text search, not a graph relation.
- The first run was invalid: every worktree of the clone shares one cache
  (`.bifrost/cache`, 18.8 GB), and concurrent processes waited on its build
  lock, so 6 records had every call time out. The rerun was serial.
- 16 calls failed with "Analyzer store failure ... Rust file routes
  unavailable: ValidationFailed" (`usage_graph`, `scan_usages_by_location`),
  so some B classes rest partly on failed calls. A first call on a worktree
  took up to 12 minutes; `usage_graph` calls took 3 to 9 minutes; most
  results reported `complete=false`.
- Probe 2 (`toolcov/SPEC-probe2.md`, `toolcov/workflow-probe2.js`,
  `results/tool-coverage-probe2.jsonl`), at the Bifrost session's advice,
  asked 10 of the records with query steps (`call_input`, `state_events_of`,
  `guards_of`, `control_relations`) and `:type flow` policies through
  `run_policy`, one Opus/high agent at a time. No record works: every best
  outcome is incomplete. `call_input` returns no rows where `call_sites_to`
  has the binding; `state_events_of` records no write for a struct-literal
  field; `guards_of` omits an enum `match` arm and still reports complete;
  Rust flow runs end inconclusive (async, early returns and indexed stores
  have no value-flow snapshot) yet report status `clean` with exit 0; fixed
  budgets (source mappings 33,288, scope nodes 20,000, nested entries
  8,000,000) are exhausted on this workspace; two flow runs gave no output in
  20 minutes. These became tickets under BrokkAi/bifrost-dev#13143.

## Terra as the brief writer (V1, round J)

**Sol writes.** Terra/high wrote all 22 mechanically valid, non-leaking briefs,
but failed the effectiveness rule registered in
[`brief/PLAN-writer-terra.md`](brief/PLAN-writer-terra.md). The MVP default writer
stays Sol/high. This experiment uses the same Terra/high reviewers in both
brief arms; BT reuses stage B's Sol-written briefs and reviewer outputs.

On the 18 primary Hard-won relation diffs, the mean per-diff caught share was:

| Reviewer context | Sessions | Catch rate |
| --- | ---: | ---: |
| No brief | 60 | 20.4% |
| Sol/high brief (BT) | 54 | 55.6% |
| Terra/high brief (TT) | 54 | 29.6% |

Paired bootstrap intervals over diffs (10,000 resamples) give TT minus BT
**−25.9 points [−44.4, −9.3]**, and TT minus no brief
**+9.3 points [−5.6, +27.8]**. TT is more than 10 points below BT, and its
gain over no brief does not have an interval above zero. Both registered
criteria fail. On the four secondary diffs, TT and BT both caught 41.7%,
against 33.3% without a brief; these diffs do not enter the decision.

Two Carbon/high judges on gemini2 labelled every Terra brief `missed` when
given it as a review output: 44 verdicts, no rewrites. Final lint and
inventory checks passed for all 22 writers. A transcript audit covered all
144 tool items; eight heuristic matches were checked and were brief text or
repository searches, with no isolation violation observed.

The pre-judging amendment selected Sol/high on codex4 for all round-J judges
and the hand check, conserving Carbon for the leak checks and V2 census.
Three batches contained 98, 98 and 74 outputs. The standard builder also
included 66 stored no-brief Sol reviewer outputs; these are excluded from
every V1 decision and comparison. All 270 judge pairs agreed: 86 caught,
two partial and 182 missed. The hand check covered all 88 non-missed outputs,
changing one secondary partial to missed. On the same Terra reviewer
sessions from the ten ablation diffs, round J and round G's Carbon judgments
gave BT 90.0% in both rounds, and no brief 23.3% versus 20.0%. Neither arm
crossed the registered 10-point judge-consistency threshold.

Writer API cost estimates totalled $1.18 for Terra and $4.37 for Sol. Median
cost was $0.0515 versus $0.1978, reported total tokens 233,994 versus 507,276,
and wall time 103.7 versus 197.1 seconds. Writer wall time runs from launch
intent through the final writer turn, including provisioning and lint fixes.
The 66 TT reviewers cost an estimated $2.8263; their individual latency is
unknown because the retained handbacks lack prompt timestamps and child
transcripts. Per-job wall times are recorded separately. These are API cost
estimates, not actual subscription charges.

TT's reviewer image was `8819974fc5ff…`; BT used the pinned `8eee5a3ab7e2…`.
Both immutable image histories record the same install pins: Codex 0.168.0,
codex-acp 1.15.0 and dsh-acp 0.6.0. This verifies install provenance rather
than runtime package contents; other image changes remain a possible
confound. Terra's writer image `a13305cd304b…` was recorded for every job but
is no longer available locally for package inspection. The evidence concerns
these Mjolnir diffs, not a general comparison across repositories.

The blinded decisive-fact containment diagnostic ran in one Sol/high session
on codex4. A one-line plan amendment, committed as `5a00ed102` before the
run, moved it from Carbon when the Gemini weekly pool was closed after
census use. The stage-A rule and all 44 brief texts were unchanged;
the writer key stayed outside the uploaded inputs.

| Set | Writer | Full fact (`yes`) | Weaker/different relation (`partial`) | `no` |
| --- | --- | ---: | ---: | ---: |
| Primary (18) | Sol/high | 7 | 8 | 3 |
| Primary (18) | Terra/high | 1 | 13 | 4 |
| Secondary (4) | Sol/high | 2 | 1 | 1 |
| Secondary (4) | Terra/high | 1 | 3 | 0 |

On the primary diffs, six pairs contained the full fact only in Sol's brief,
one in both, none only in Terra's, and eleven in neither. Across those six
Sol-only pairs, mean reviewer catch share was 66.7% for BT and 22.2% for TT.
The one primary diff where both contained the full fact,
`web-feed-self-deadlock`, was caught by all reviewers in both arms. This
points to weaker fact coverage as one explanation for TT's deficit; most
Terra briefs anchored a relevant location without stating the full relation.
These are descriptive comparisons from a single blinded judge, separate from
the registered reading. **The Sol-writes decision is unchanged.**

All 44 output IDs, brief hashes and quoted matching items were validated.
The diagnostic used 815,370 reported tokens, an estimated API price of
$0.2861, and 178.3 seconds from recorded prompt to completion. Session
configuration confirmed high effort; the usage attribution's effort field
was null. Details, per-brief labels and quoted evidence are in
[`contains-summary.json`](results/brief-writer-terra/contains-summary.json)
and [`contains.jsonl`](results/brief-writer-terra/contains.jsonl).

Results: [`summary.json`](results/brief-writer-terra/summary.json),
[`per-diff.json`](results/brief-writer-terra/per-diff.json),
[`usage.jsonl`](results/brief-writer-terra/usage.jsonl), and the accompanying
audit, image and reviewer metadata under `results/brief-writer-terra/`.
Raw inputs and judgments remain under
`/mnt/optane/review-arms-20261113/brief-judge-j/`; the blinding key remains in
that data root's `private/key-brief-judge-j.json`.

## Sonnet as the brief writer (round K)

**Sol writes.** On the 18 primary diffs, the same Terra/high reviewer model
caught 22.2% with Sonnet 5.5/high briefs, 55.6% with Sol/high briefs, and
20.4% without a brief. Round K judged those three conditions together.
Both preregistered criteria for accepting Sonnet failed.

| Brief given to the reviewer | Primary catch rate (18 diffs) | Secondary catch rate (4 diffs) |
| --- | ---: | ---: |
| None | 20.4% | 33.3% |
| Sol/high (BT) | 55.6% | 41.7% |
| Sonnet 5.5/high (TS) | 22.2% | 33.3% |
| Terra/high (TT, historical round J) | 29.6% | 41.7% |

Mean catch rates weight every diff equally. With 10,000 paired bootstrap
resamples over primary diffs, TS minus BT was **−33.3 points**
(95% CI **−53.7 to −14.8**) and TS minus no brief was **+1.9 points**
(**−9.3 to +16.7**). The secondary differences were −8.3 points
(−25.0 to 0.0) and 0.0 points (0.0 to 0.0), respectively.
TT's historical result is shown alongside round K for context.

Two Sol/high judges on codex4 labeled each of 270 outputs in three
blinded mixed batches. They agreed on 265 outputs. The hand check
covered 87 outputs and resolved five split verdicts as missed. The
registered readout uses Terra reviewers only. The other 66 stored Sol
no-brief outputs went through the same judging process. The 138 shared
BT and no-brief Terra handbacks received identical final labels in J
and K. Both arms had a 0.0-point catch-rate change, including within
each diff set.

All 22 Sonnet briefs passed final lint and checkout inventory checks.
After the allowed wording-only rewrites, three residual leak labels
were caught: `activity-recorder-kills-api`,
`pending-assessment-hidden-when-idle`, and `web-feed-self-deadlock`.
`github-token-in-container-argv` remained partial. Both judges agreed
on all four rechecks. The study retained these briefs with their
relations intact, so these residual labels are a caveat when
interpreting the reviewer results.

In the separate blinded containment diagnostic, the judge marked
the full decisive fact in 2/18 Sonnet briefs and 6/18 Sol briefs on the
primary diffs. The secondary counts were 2/4 and 1/4, respectively.
Primary yes/partial/no counts were 2/14/2 for Sonnet and 6/10/2 for
Sol. This diagnostic used one Sol/high judge and sits outside the
registered writer decision.

| Writer | Estimated cost, all 22 | Median cost per brief | Median tokens | Median wall time |
| --- | ---: | ---: | ---: | ---: |
| Sol/high | $4.37 | $0.198 | 507,276 | 197.1 s |
| Terra/high | $1.18 | $0.0515 | 233,994 | 103.7 s |
| Sonnet 5.5/high | $5.06 | $0.206 | 536,261 | 304.1 s |

Wall time runs from launch intent to the final writer turn, including
setup and lint repairs. Estimated API-equivalent cost was $2.73 for the
66 TS reviewers and $2.57 for the 16 grading and rewrite sessions.
The $10.36 study subtotal covers writers, reviewers, graders and
rewrites. Carrier work is outside that subtotal. Individual TS reviewer
latency was unavailable from retained handbacks.

Two carrier workers were replaced during the batch. Their explicitly
refused configuration calls were retried on the original sessions
before either carrier received its first prompt. The 22 carrier IDs
and 66 reviewer IDs were unique. The experiment records the distinct
writer and review image IDs per job. This comparison covers the frozen
Mjolnir corpus and includes stored comparison reviews from earlier runs.

The [registered plan](brief/PLAN-writer-sonnet.md),
[round K summary](results/brief-writer-sonnet/summary.json), and
[cost summary](results/brief-writer-sonnet/cost-summary.json) accompany
the per-diff readout, leak checks, and containment results.

## Running

The scripts read the corpus from this directory and keep everything else
under `REVIEW_ARMS_DIR`. Runs use the installed mj on the default instance;
the earlier private instance is retired. The default instance needs the
experiment's `carrier` profile and reviewer profiles before launching jobs.
To initialize a fresh experiment's reviewer workspace record:

```sh
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-<date>
export REVIEW_ARMS_INSTANCE=default
mkdir -p "$REVIEW_ARMS_DIR/default-instance"
./mjx doctor --json
./mjx workspaces create "Review arms" --json > "$REVIEW_ARMS_DIR/default-instance/workspace.json"
```

Scorers and imports do not require a reviewer workspace record. Writer and
reviewer drivers read it only when launching a session. The verifier creates
its own record at `default-instance/verify/workspace.json` and needs no
reviewer workspace; the Git-only checkout at `verify/repo` is shared data,
not daemon state. Existing verifier session records refer to the default
daemon. Verifier runs explicitly reject `REVIEW_ARMS_INSTANCE=private`
before doing any work, rather than sending those IDs to the retired daemon.

Then:

```sh
python3 driver.py --jobs jobs.json --concurrency 6   # [{"bug": "<id>", "arm": "A"|"B"}]
python3 carrier_check.py
python3 audit.py
```

`carrier_test.py <bug>` checks that the carrier answers a bug's context
without tools, and then destroys its session.

The writer-study helper retains its historical filename,
`brief_writer_terra.py`, and selects a study with `--writer terra` (default)
or `--writer sonnet`. Sonnet writers use `brief_driver.py --kind blind-sonnet`
with an explicit Claude `--profile`; their three-Terra reviewer arm is TS.
The shared prompt, lint and blinding rules are unchanged. Set the selected
Claude profile and codex4 to 25% floors using the existing profile-specific
`REVIEW_ARMS_QUOTA_FLOOR` syntax before starting work.

Sonnet's leak/rewrite artifacts use `briefs/blind-sonnet*`, its paired
containment inputs use `briefs/writer-contains-sonnet`, and results go to
`results/brief-writer-sonnet/`. The readout uses round K's mixed TS, BT and
no-brief judgments, with round J as a consistency reference. TT's round-J
result is historical context beside the reading. These paths keep the
completed Terra study separate:

```sh
python3 -B brief_writer_terra.py leak-build --writer sonnet
python3 -B brief_writer_terra.py leak-collect --writer sonnet
python3 -B brief_writer_terra.py contains-build --writer sonnet
python3 -B brief_writer_terra.py usage --writer sonnet
python3 -B brief_writer_terra.py readout --writer sonnet --name brief-judge-k
```

Stage 2 (pooled findings, per-session scoring). Run long drivers with
`setsid nohup`, because a harness background task has a time limit:

```sh
python3 select_diffs.py <repo> 40 > corpus/diffs.jsonl
python3 driver.py --jobs jobs.json --concurrency 10        # arms A, D, D2 (C = Ember, dropped)
./pipeline.sh jobs.json verify2                           # batches + Opus verifiers as diffs finish;
                                                          # creates the verifier checkout, workspace and SPEC.md
python3 score.py verify2                                  # sessions.csv, groups.csv, session_groups.csv
python3 pareto.py verify2 [--severities high,medium] [--verdicts confirmed,plausible]
python3 compare.py verify2 Terra+Terra+Terra Terra+Sol Terra+Terra+Sol
python3 consistency.py verify verify2
python3 knownbug.py verify2 <known-bug.jsonl>
```

Stage 3 (Sol/medium, all diffs verified again). Name each configuration with
its effort once a model has run at two efforts:

```sh
python3 driver.py --jobs stage3-jobs.json --concurrency 10   # arm E, 2× Sol/medium per diff
./pipeline.sh stage3-jobs.json verify3
python3 audit.py runs/<diff>/E ...                           # paths relative to REVIEW_ARMS_DIR
python3 score.py verify3
python3 pareto.py verify3 --sessions Sol/medium=2 [--severities high,medium]
python3 compare.py verify3 Sol/high+Terra/high Sol/medium+Terra/high Terra/high+Terra/high
python3 consistency.py verify2 verify3
python3 knownbug.py verify3 results/known-bug.jsonl --groups-from verify2
```

Stage 4 (new configurations, Greptile, all verified together). Configure the
Greptile key in `~/.secrets/greptile_api_key`, and a dsh-review profile whose
home holds no instructions (see `instance-config.toml` notes above):

```sh
python3 driver.py --jobs jobs.json --concurrency 10     # arms H, K, L (I, J: DSF/low, dropped)
python3 greptile_driver.py --jobs greptile-jobs.json --concurrency 3   # [{"bug": .., "tier": "base"|"plus"}]
python3 audit.py runs/<diff>/<arm> ...
python3 build_verify.py verify5 --arms A,B,D,D2,E,G-base,G-plus,H,J,K,L,I --ready-jobs all-jobs.json  # every job of those arms
python3 verify_driver.py verify5 --concurrency 7
python3 score.py verify5
python3 pareto.py verify5 --exclude DSF/low --bootstrap 300 \
  --sessions Sol/medium=2,Terra/xhigh=2,Sol/xhigh=2,Greptile/base=1,Greptile/plus=1,Ember/xhigh=1,Gemini/high=1,Kimi/high=1
python3 compare.py verify5 Terra/xhigh+Terra/xhigh Terra/xhigh+Terra/xhigh+Gemini/high
python3 knownbug.py verify5 results/known-bug.jsonl --groups-from verify2 --exclude DSF/low
uv run --with matplotlib python3 plot_pareto.py <out-dir> [--no-greptile-combos]  # latency vs. findings, both frontiers + Greptile
```


## Bifrost bug corpus and registered arms

`corpus-bifrost/README.md` records Milestone1, including the UTC correction,
30 confirmed bugs,40 ordinary controls and quota-amended census. The stage
registration is `brief/PLAN-bifrost-bugs.md`; execution and cleanup follow
`.agents/plans/bifrost-brief-arms.md`. Relation-stage membership comes from the
frozen Sol census, not from reviewer outcomes. The17 relation diffs run first
in A (no brief) and BT (blind Sol brief), each with3 Terra/high reviewers.
Later13 non-relation diffs and40 controls wait for the primary reading and
remaining-budget decision.

Use the production default instance through `mjx`; do not start the private
review-arms daemon or edit the main config. Shared source settings preserve
mjolnir defaults and can select the frozen Bifrost inputs:

```sh
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-bifrost-20261116
export REVIEW_ARMS_INSTANCE=default
export REVIEW_ARMS_REPO=/mnt/optane/tmp/d2fa-bifrost/bifrost-dev
export REVIEW_ARMS_CORPUS="$REVIEW_ARMS_DIR/corpus/corpus.jsonl"
export REVIEW_ARMS_DIFFS="$REVIEW_ARMS_DIR/corpus/diffs.jsonl"
export REVIEW_ARMS_BUNDLE=bifrost-dev
export REVIEW_ARMS_REPO_DIR=bifrost-dev
export REVIEW_ARMS_SETS="$REVIEW_ARMS_DIR/arms/sets.json"
export REVIEW_ARMS_QUOTA_FLOOR=codex4=25
```

The existing quota parser takes `profile=percent`; `codex4=25` expresses this
study's25percent floor. A missing report holds launches. A quota stop is
reported without switching model/profile. The manifest has P=17 relation
IDs,S=13 other known-bug IDs,C=[]; ordinary controls stay in their own jobs
file. `REVIEW_ARMS_REPO` selects host Git reads, while bundle and repository
directory select the actual container source. Set all three consistently.
Workspace records are `default-instance/workspace.json` and the separate
`default-instance/verify/workspace.json` under the data root.

Writers, paired leak checks and both catch judges are Sol/high on codex4.
Writer prompts/tools and historical blinding are stageB's. Reviewer selection
uses the explicit3Terra/high roster through normal review. New reviewers are
destroyed at role end (#1554/#1559): stored handbacks and usage support catch
judging and control2, while tool-call audits and latency are unavailable.
No transcript capture is added, and a missing audit is never called clean.
Existing repository-inventory checks still apply; their coverage and the
prompt-based access restrictions retain the blinding limits described above.

## Bifrost-dev oracle arm: does the decisive fact lift reviewers?

Registration: `brief/PLAN-bifrost-oracle.md`, with its 2026-11-18 amendment
(round L judged by Opus/high on claude7). Readout:
`brief/bifrost-oracle/READOUT.md`.

- **Facts carry, as an upper bound.** On the 17 bifrost-dev relation diffs,
  Terra/high caught 17.6% with no brief, 7.8% with the blind Sol brief and
  45.1% with an oracle brief. OT − A is +27.5 points (+11.8 to +45.1), and
  OT − BT is +37.3 (+17.6 to +58.8).
- 15 of the 17 oracle briefs still read as caught (8) or partial (7) to the
  leak judges after the rewrite, so the lift may partly come from the brief
  naming the defect. The caught-leak diffs gain +37.5 points and the
  partial-leak diffs +23.8. The two missed-leak diffs don't move.
- All 17 OT carriers ran on a different Podman image from the stored A and BT
  carriers, so the brief and the image changed together; the lift is an
  observed cohort difference.
- The lift is on 8 diffs; 9 don't move. It shows only that reviewers benefit
  when the fact is supplied, not that Bifrost queries can find it.
- Round L's final A and BT labels equal the stored Sol labels. That is partly
  by construction: the parent applied the stored round's transient-failure
  precedent to three retained-fill rows. The assessor's unadjusted labels
  differ by 3.9 points on A, still under the 10-point caveat, and give
  OT − A of +25.5 (+9.8 to +43.1), still Facts carry. The recorded new spend
  is $10.56 or more at API-equivalent rates; carrier cost coverage is
  incomplete, so the full cost is unknown.
