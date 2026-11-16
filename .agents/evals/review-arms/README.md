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
- Not run: the plan's report-only label of whether each P and S brief
  contains the decisive fact.

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

## Running

The scripts read the corpus from this directory and keep everything else
under `REVIEW_ARMS_DIR`. To set up a fresh data directory:

```sh
export REVIEW_ARMS_DIR=/mnt/optane/review-arms-<date>
mkdir -p $REVIEW_ARMS_DIR/instance/{config,data,carrier-dsh}
# Edit the carrier `home` and the Podman image ID first; both are specific to the first run.
cp instance-config.toml $REVIEW_ARMS_DIR/instance/config/config.toml
cp carrier-AGENTS.md $REVIEW_ARMS_DIR/instance/carrier-dsh/AGENTS.md
grep '^DEEPSEEK_API_KEY' ~/.config/mjolnir/secrets.toml > $REVIEW_ARMS_DIR/instance/config/secrets.toml
chmod 600 $REVIEW_ARMS_DIR/instance/config/secrets.toml
./mjx doctor --json
./mjx workspaces create "Review arms" --json > $REVIEW_ARMS_DIR/instance/workspace.json
```

Then:

```sh
python3 driver.py --jobs jobs.json --concurrency 6   # [{"bug": "<id>", "arm": "A"|"B"}]
python3 carrier_check.py
python3 audit.py
```

`carrier_test.py <bug>` checks that the carrier answers a bug's context
without tools, and then destroys its session.

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
