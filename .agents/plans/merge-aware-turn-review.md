# Keep merged-in history out of a turn's review

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir reviews each agent turn automatically: when the primary agent finishes, a reviewer agent is told to run one `git diff` between a baseline (what the last completed review covered) and a capture (the workspace as the turn left it), and to report findings. Issue #1285 shows that a turn which merges upstream into its branch, as `AGENTS.md` requires when a push is rejected (`git fetch` then `git merge origin/master`), gets a review of upstream's commits as if the turn had written them. On 2026-10-14 a review filed two findings against commits from another session that the turn had only merged.

After this change the review diff contains the turn's own commits, the way it resolved any merge conflicts, and its uncommitted edits, but not the changes that a merge brought in from another line of history. A person sees it by running the scenario in "Validation and Acceptance": a repository where the turn commits, merges a branch that touches other files and builds on the turn's own earlier commit, resolves a conflict, and edits a file; the captured review diff names only the turn's files and hunks.

## Progress

- [x] (2026-10-19 10:40Z) Read the capture path (`mj-worker/src/review/capture.rs`), the Git plumbing (`mj-checkpoint/src/archive/git.rs`), the protocol (`mj-core/src/relay/protocol.rs`), the driver (`mj-review/src/driver.rs`) and the prompt (`mj-review/src/lanes.rs`).
- [x] (2026-10-19 11:10Z) Prototyped the design with shell Git in `/tmp`: tag-of-tree baseline, first-parent merge walk, three-way replay anchored at each merge's first parent.
- [x] (2026-10-19 12:20Z) Git plumbing in `mj-checkpoint`: shared `ScratchIndex`, `capture_paths` from an explicit start, `merge_trees`, `record_review_point` and `read_review_point`.
- [x] (2026-10-19 12:40Z) Worker capture: one HEAD snapshot per capture, review points as baselines, merged-in lines folded into the diff base (`review_base`).
- [x] (2026-10-19 12:50Z) Protocol and prompt: `RepoDelta.merged_in`, the reviewer note.
- [x] (2026-10-19 13:10Z) Tests: the #1285 regression and the bare-tree upgrade case; the regression fails with the replay disabled and with the replay anchored at the baseline commit.
- [x] (2026-10-19 13:20Z) `BoundedGit` forwards command input; workspace clippy clean.
- [x] (2026-10-19 14:05Z) Full suites of the touched crates pass: mj-checkpoint 144, mj-controller 2001, mj-core 499, mj-review 27, mj-worker 678 plus its integration tests.

## Surprises & Discoveries

- Observation: The Git 2.38 `git merge-tree --write-tree` command, which would do the merge replay in one call, is not available everywhere Mjolnir runs a worker. `.agents/plans/git-startup-recovery.md` records that Mjolnir must start with Git 2.25.1 on an SSH machine, and Ubuntu 22.04 ships Git 2.34.
  Evidence: `git merge-tree` before 2.38 accepts only the three-tree trivial form and exits 129 on `--write-tree`.

- Observation: The replay and the review point work on Git 2.25.1. Built from the kernel.org tarball into `/tmp`, put first on `PATH` through a wrapper that sends only `git init` (whose `-b` flag the test fixtures use and 2.25 lacks) to the system Git, it ran every production Git command of the capture tests and the startup pin test.
  Evidence: `PATH=/tmp/git225/wrap:$PATH cargo test -p brokk-mj-worker --lib -- review::capture first_daemon_start_pins` reported `18 passed`.

- Observation: `git rev-list --first-parent H --not B` excludes every ancestor of `B`, including ancestors reached through `B`'s second parents, so the walk stops at the baseline even when `B` is itself a merge.
  Evidence: with `B` a merge of `up` into `main` and `H` a commit on `up`, `git rev-list --first-parent H ^B` printed only `H`.

- Observation: `BoundedGit` in `mj-worker/src/review/capture.rs`, which runs the startup baseline under a deadline, dropped `GitCommand.stdin`, so the first command that needed input (`git mktag`) failed and the startup pin was never written.
  Evidence: `worker_runtime::relay_tests::first_daemon_start_pins_the_worktree_before_the_primary_harness` failed with `assertion failed: baseline.status.success()` until the input was attached with `CommandSpec::with_sensitive_stdin`, the executor's concurrent pipe path, which also honours the deadline.

- Observation: `refs/hel/review-baseline` is shared by every linked worktree of a repository, because only `refs/worktree/`, `refs/bisect/` and `refs/rewritten/` are per-worktree. A second worktree session finds the first one's pin at startup and keeps it. This predates and is independent of this plan; it was fixed separately under #1307, which pins each linked worktree at `refs/hel/worktrees/<name>/review-baseline`, records the owning session in the review point, and deletes the pins of removed worktrees.
  Evidence: `git update-ref refs/hel/review-baseline HEAD^{tree}` in a linked worktree is visible from the main worktree with `git rev-parse --verify refs/hel/review-baseline`.

## Decision Log

- Decision: A baseline records the HEAD commit it was taken at inside the one id that already travels as the baseline, as an annotated tag object that points at the captured tree and whose message is `head <commit id>`.
  Rationale: The tree and its commit are one fact and must have one owner. A tag id is still a tree-ish: `git diff`, `git ls-tree`, `git cat-file -e <id>^{tree}` and `git rev-parse <ref>^{tree}` all peel it, so the daemon's database, the relay protocol and older workers keep working with no schema or protocol change. A tag that points at a tree is ignored by `git log --all`, unlike a synthetic commit, so the user's history views gain nothing. A fixed tagger and date make the id deterministic. A plain tree id from an older worker is read as a baseline with no recorded HEAD.
  Date/Author: 2026-10-19, Claude.

- Decision: The turn's line is HEAD's first-parent line since the baseline's commit. For every merge commit on that line, oldest first, the changes each non-first parent brought in are folded into the diff base by a three-way merge whose ancestor is the merge base of that merge's first parent and the merged parent.
  Rationale: This is how Git itself names a branch's own history (`--first-parent`), and it is what #1285 asked for. Anchoring at the merge's first parent, not at the baseline commit, matters: when upstream already contains a commit the turn pushed earlier in the same turn, anchoring at the baseline would fold that commit in as upstream work and hide it from the review. A conflict between the reviewed baseline and the merged line leaves conflict markers in the base, so the diff shows both sides and the turn's resolution, the same view `git show --remerge-diff` gives.
  Date/Author: 2026-10-19, Claude.

- Decision: Replay merges with `git read-tree -m` into a scratch index and `git merge-file` for paths both sides changed, not with `git merge-tree --write-tree`.
  Rationale: One path that works on every Git Mjolnir supports (2.25 and later) instead of a modern path plus a fallback. The cost is that this replay does no rename detection; a file the merged line renamed while the baseline changed it shows in the review, which errs toward reviewing more.
  Date/Author: 2026-10-19, Claude.

- Decision: Commits that reach the first-parent line by fast-forward or by a rebase onto another line are still reviewed as the turn's work.
  Rationale: The commit graph cannot tell them apart from the turn's own pushed commits; only provenance (reflog messages) could, and reflogs can be disabled, expired or written differently by other tools, which would hide the turn's work. Reviewing too much is the safe direction. Merge commits for pull requests on such a line are still recognized, so their side branches are excluded.
  Date/Author: 2026-10-19, Claude.

- Decision: A baseline whose recorded commit is no longer in the repository is reviewed as a plain tree diff, with a warning in the worker log.
  Rationale: The commit stays reachable through the reflog for at least 30 days after a rewrite, so this needs deliberate history deletion. Failing the capture instead would keep the baseline from ever advancing.
  Date/Author: 2026-10-19, Claude.

## Outcomes & Retrospective

The review diff now holds the turn's own commits, its merge resolutions and its uncommitted edits; content merged in from other lines is folded into the diff base and named in the prompt. Baselines carry their HEAD commit in the one id that already travelled, so neither the daemon's records nor the relay protocol changed shape, and baselines from earlier workers still review as plain tree diffs. Remaining limits are deliberate and documented in `review_base`: commits that arrive by fast-forward or rebase are still reviewed, and the replay does no rename detection. The shared baseline ref across linked worktrees, found on the way, is a separate defect.

## Context and Orientation

The worker (`mj-worker`, one process per session) owns Git work for reviews. `mj-worker/src/review/capture.rs` has three entry points. `initialize_review_baselines` runs before the primary agent starts and pins the starting point at the ref `refs/hel/review-baseline`. `capture_repository_deltas` runs when a review starts: it reads `git status`, builds a tree of HEAD plus every changed path in a scratch index (a temporary index file named by `GIT_INDEX_FILE`, so the user's real index is untouched), and diffs the baseline against it. `advance_baselines` pins the reviewed capture as the next baseline. The Git primitives they use (`capture_paths`, `pin_review_tree`, `diff_between_trees`, `numstat_between_trees`, `REVIEW_BASELINE_REF`) live in `mj-checkpoint/src/archive/git.rs` and are re-exported from `mj-checkpoint/src/archive.rs`.

The daemon (`mj-controller`) stores each repository's baseline id as an opaque string in the `baselines` JSON column (`mj-controller/src/database/reviews.rs`) and sends it back in `ReviewerRequest::CaptureDelta { baselines }`. The worker answers with one `RepoDelta` per repository (`mj-core/src/relay/protocol.rs`): `baseline_tree` (what the patch is diffed from), `current_tree` (the capture, which becomes the next baseline), the bounded patch, a diffstat and per-file counts. The review driver (`mj-review/src/driver.rs`) turns those into `RepositoryCapture` values, and `mj-review/src/lanes.rs` writes the reviewer prompt, which tells the reviewer to run `git -C <root> diff --no-ext-diff <baseline_tree> <current_tree>`.

Terms used here. A tree-ish is any Git object id that Git can resolve to a tree: a tree, a commit, or a tag that points at either. The first-parent line of a commit is the chain reached by following only each commit's first parent; a merge's first parent is the branch that was checked out when it was made, and its other parents are the lines merged in. A three-way merge combines two versions of a tree using their common ancestor, so each side's changes relative to the ancestor are kept.

## Plan of Work

In `mj-checkpoint/src/archive/git.rs`, factor the scratch index setup that `capture_paths` and `capture_worktree_tree` duplicate into one private helper that creates the file in the Git directory, hands its `GIT_INDEX_FILE` environment to a closure, and removes it on every exit. Replace the `CaptureBase` enum, whose `Tree` variant is unused, with an `Option<&str>` start: the commit or tree to begin from, or `None` for an empty tree, so the worker passes the HEAD it resolved instead of `capture_paths` resolving HEAD a second time. Add `merge_trees(runner, repository, ancestor: Option<&str>, ours, theirs) -> Result<String>`, which runs `git read-tree -m -i --aggressive` in a scratch index, resolves each remaining path (a content merge with `git merge-file` when both sides hold regular files, otherwise the `ours` entry), and writes the tree. Add `record_review_point(runner, repository, tree, head: Option<&str>) -> Result<String>` (a `git mktag` call) and `read_review_point(runner, repository, id) -> Result<ReviewPoint>` with `ReviewPoint { tree, head }`. Update the `REVIEW_BASELINE_REF` documentation.

In `mj-worker/src/review/capture.rs`, resolve HEAD once at the start of each repository's capture, capture from that commit, and return a review point id as `current_tree`. When a baseline exists, compute the diff base: read the baseline's point; if it has a recorded commit that still exists and HEAD differs from it, list the merges on HEAD's first-parent line since that commit (`git rev-list --first-parent --merges --parents --reverse HEAD --not <commit>`) and fold each non-first parent into the baseline tree with `merge_trees`, anchored at `git merge-base <first parent> <merged parent>`. Report the merged parents as `merged_in`. `initialize_review_baselines` pins a review point too, and `pinned_review_baseline` returns the pinned id rather than peeling it to its tree.

In `mj-core/src/relay/protocol.rs`, add `merged_in: Vec<String>` to `RepoDelta` with `#[serde(default, skip_serializing_if = "Vec::is_empty")]`, so a daemon reading an older worker gets an empty list. In `mj-review`, carry it into `RepositoryCapture` and add one sentence to that repository's section of the prompt when it is not empty.

## Concrete Steps

From the repository root:

    cargo test -p brokk-mj-worker --lib review::capture
    cargo test -p brokk-mj-worker --lib first_daemon_start_pins
    cargo test -p brokk-mj-review
    cargo clippy --all-targets -- -D warnings

The first command runs 17 tests, including `a_merge_during_the_turn_reviews_only_the_turns_work` and `a_bare_tree_baseline_from_an_earlier_worker_is_still_reviewed`, and reports `17 passed`. Run Cargo tests outside a restricted sandbox; the worker suite uses loopback sockets.

## Validation and Acceptance

The test `a_merge_during_the_turn_reviews_only_the_turns_work` in `mj-worker/src/review/capture.rs` builds a real repository: a baseline, a turn commit that the merged line later builds on, a merged line that adds a file and edits a large shared file in both a separate hunk and a hunk the turn also changed, a conflict the turn resolves, and an uncommitted edit. It asserts that the captured patch contains the turn's commit, its resolution and its edit, and none of the merged line's own file or hunks. It fails before the change (the patch contains the merged line's file) and passes after. A second test feeds a baseline pinned as a plain tree, as earlier workers pinned it, and expects the turn's edit to be reviewed.

## Idempotence and Recovery

Captures write only Git objects and remove their scratch index and temporary files on every exit path; running a capture twice records identical review point ids for identical states. No database migration is involved.

## Artifacts and Notes

Prototype transcript (shell Git, upstream built on the turn's pushed commit `C1`, turn commits `C2`, merges `origin/main`):

    *   3209f90 Merge remote-tracking branch 'origin/main'
    |\
    | * 03e9672 U1
    * | 0dc7922 C2
    |/
    * a96d04e C1
    * 896efe9 base
    === effective diff
    -1
    +1-turn
    -5
    +5-turn2

Regression checks on the real test (`cargo test -p brokk-mj-worker --lib a_merge_during_the_turn`). With the replay disabled the test fails; with the replay anchored at the baseline commit instead of each merge's first parent it fails with:

    "+line 10 turn" is missing: diff --git a/shared.txt b/shared.txt

## Interfaces and Dependencies

In `mj-checkpoint/src/archive/git.rs`:

    pub fn capture_paths(runner: &dyn GitCommandRunner, repository: &Path, start: Option<&str>, paths: &[PathBuf]) -> Result<String>;
    pub fn merge_trees(runner: &dyn GitCommandRunner, repository: &Path, ancestor: Option<&str>, ours: &str, theirs: &str) -> Result<String>;
    pub struct ReviewPoint { pub tree: String, pub head: Option<String> }
    pub fn record_review_point(runner: &dyn GitCommandRunner, repository: &Path, tree: &str, head: Option<&str>) -> Result<String>;
    pub fn read_review_point(runner: &dyn GitCommandRunner, repository: &Path, id: &str) -> Result<ReviewPoint>;

In `mj-core/src/relay/protocol.rs`, `RepoDelta` gains `pub merged_in: Vec<String>`. In `mj-review/src/lanes.rs`, `RepositoryCapture` gains `pub merged_in: Vec<String>`.
