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
