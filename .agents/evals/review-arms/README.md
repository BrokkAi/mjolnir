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
./pipeline.sh jobs.json verify2                           # batches + Opus verifiers as diffs finish
python3 score.py verify2                                  # sessions.csv, groups.csv, session_groups.csv
python3 pareto.py verify2 [--severities high,medium] [--verdicts confirmed,plausible]
python3 compare.py verify2 Terra+Terra+Terra Terra+Sol Terra+Terra+Sol
python3 consistency.py verify verify2
python3 knownbug.py verify2 <known-bug.jsonl>
```
