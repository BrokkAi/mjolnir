# Review only the commits a session wrote, by reading its HEAD reflog

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. It builds on `.agents/plans/merge-aware-turn-review.md`, which is checked in, and replaces the merge folding that plan introduced.

## Purpose / Big Picture

Mjolnir reviews each agent turn automatically: when the primary agent finishes, a reviewer agent runs one `git diff` between a base tree and a capture of the workspace, and reports findings. The base must contain everything the turn did not write, or the reviewer files findings against other people's code. `.agents/plans/merge-aware-turn-review.md` (issue #1285) kept the lines a *merge commit* brought in out of the diff. It left fast-forwards and rebases in, on the grounds that the commit graph cannot tell another person's commit from one the turn pushed.

That gap is the common case. On 2026-10-21 a bifrost-service session (`178a9e1f748c`) ran `git checkout main && git pull --ff-only origin main`, which fast-forwarded five commits by another developer, then started a new branch from `origin/main` after its own pull request was squash-merged. GitHub squash merges are ordinary single-parent commits, so nothing was a merge. Four reviews in a row reported the same finding in `client/components/AdminView.tsx`, a file only the other developer had touched, and the agent spent each corrective pass explaining that the code was not its own.

After this change the base holds every commit the turn did not create in its own worktree, however it arrived: by merge, fast-forward, pull, rebase onto another line, branch switch, or reset. The diff holds the commits the worktree created during the turn, its merge resolutions, and its uncommitted edits. That includes work of the turn's own that reached HEAD inside someone else's squash commit. To see it, run the tests named in "Validation and Acceptance". They build repositories where another worktree or clone writes the foreign commits, and they check that the captured patch names only the turn's lines.

## Progress

- [x] (2026-10-21 12:10Z) Diagnosed session `178a9e1f748c` from its clone's reflogs: the out-of-scope commits arrived by `pull --ff-only` and by `checkout` of a branch made from `origin/main`.
- [x] (2026-10-21 12:40Z) Design: HEAD reflog attribution, forward reconstruction of the base, carried-over pending commits, visible fallback.
- [x] (2026-10-21 13:05Z) Git plumbing in `mj-checkpoint/src/archive/git.rs`: HEAD reflog reader and classifier, review point `reflog`/`pending` fields, conflict policy for `merge_trees`.
- [x] (2026-10-21 13:20Z) Attribution in `mj-worker/src/review/attribution.rs`, wired into `capture_repository_deltas` and `initialize_review_baselines`; the first-parent merge folding is gone.
- [x] (2026-10-21 13:25Z) Protocol `RepoDelta.unattributed`, prompt text in `mj-review/src/lanes.rs`, driver mapping.
- [x] (2026-10-21 13:45Z) Tests: the #1285 regression with its upstream moved to another worktree, `a_pulled_or_squashed_upstream_is_not_the_turns_work`, `work_left_on_another_branch_is_reviewed_when_it_returns`, `a_head_move_the_reflog_cannot_explain_is_reviewed_whole_and_says_why`. Each guard was checked by disabling it: with no attribution the #1285 and squash tests fail; without the revert of unreachable commits the squash test fails; without the pending carry the left-branch test fails; with conflict markers instead of `--ours` in that revert the squash test fails on `draft`.
- [x] (2026-10-21 14:30Z) Full suites of the touched crates and workspace clippy: mj-checkpoint 144, mj-core 503, mj-review 27, mj-worker 685 plus its integration tests, mj-controller 2007 of 2008. The one failure, `installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths`, needs `shasum`, which this Fedora host lacks; it touches no code this change edits.

## Surprises & Discoveries

- Observation: Within one second every reflog entry has the same timestamp, so a baseline cannot be found in the reflog by timestamp and commit alone. Tests run whole scenarios inside one second.
  Evidence: a scratch repository's `git log -g --date=raw --format='%H %gd %gs'` printed nine entries, all `HEAD@{1792584094 +0200}`, three of them on the same commit.

- Observation: The #1285 regression test made its "upstream" commits in the session's own worktree (`git checkout -qb upstream`, commit, checkout back). Under reflog attribution those are the turn's own commits. In practice upstream work comes from another clone or worktree, so the test now writes it in a linked worktree, whose HEAD reflog is separate.
  Evidence: `mj-worker/src/review/capture.rs`, `a_merge_during_the_turn_reviews_only_the_turns_work`, before this change.

- Observation: The #1285 merge folding also hid the turn's own work. A turn that merged its own feature branch into main with `--no-ff` had the feature commits folded into the base as "merged in", because the fold could not tell the merged line's authorship either.

- Observation: `git cherry-pick --ff` records `cherry-pick: fast-forward`, which reads the same as picking a commit whose subject is "fast-forward". Only `merge` and `pull` fast-forwards count as moves; the cherry-pick case counts as a commit, which reviews one extra commit at worst. A rebase with nothing to replay records `rebase (start)` and `rebase (finish)`.
  Evidence: `git log -g --format=%gs HEAD` in a scratch repository after `git rebase main`, `git cherry-pick --ff main`, `git pull . main` and `git merge --ff-only main`.

- Observation: With the HEAD reflog deleted, `git log -g HEAD` still prints one entry, the current commit, and on an unborn branch it fails with status 128. The reader therefore asks `git reflog exists HEAD` first.
  Evidence: a scratch repository after `rm .git/logs/HEAD` printed its HEAD commit from `git log -g --format=%H HEAD` with status 0.

- Observation: The diagnosed session's own commit `662a02e4` (production milestone 4) was made on one branch while the review captured another, so no review ever covered it until the plain tree diff swept it up with everything else. That is the case the pending carry exists for.
  Evidence: the clone's HEAD reflog showed `commit` at 11:48:49 on `issue-651-envoy-gateway-prod`, then `checkout: moving from issue-651-envoy-gateway-prod to issue-651-envoy-gateway` in the same second, before the 11:54 review's capture.

## Decision Log

- Decision: The worktree's HEAD reflog is the owner of "which commits did this turn create". A reflog entry whose message records a commit being made (`commit:`, `commit (amend):`, `commit (merge):`, `merge … : Merge made by …`, `pull: Merge made by …`, `cherry-pick:`, `revert:`, `rebase (pick):` and the other rebase actions, `am:`) names a commit the turn wrote. An entry that only moves HEAD (`checkout: moving from`, `reset: moving to`, `merge …: Fast-forward`, `pull …: Fast-forward`, rebase `(start)`/`(finish)`/`(abort)`, `clone: from`, `branch:`) brings in commits the turn did not write. An entry with any other message is counted as written by the turn.
  Rationale: The commit graph cannot separate authorship, and remote-tracking reflogs cannot either: the turn's own pushed work comes back inside other people's squash commits. The HEAD reflog is per worktree (a linked worktree has its own), Git writes it by default in every non-bare repository, and it records how HEAD reached each commit. Counting unknown messages as written by the turn errs toward reviewing more.
  Date/Author: 2026-10-21, Claude.

- Decision: The base is rebuilt forward. Every commit reachable from HEAD and not from the baseline commit gets a reduced tree, meaning its tree without the turn's commits. A commit the turn wrote takes its parent's reduced tree, or for a merge the merge of its parents' reduced trees (which drops the resolution, so the diff shows it). A commit someone else wrote keeps its tree when nothing below it is the turn's; otherwise its own change is replayed onto its parent's reduced tree. The base is the reduced tree of HEAD, with the baseline's reviewed uncommitted edits merged back in.
  Rationale: Reverting the turn's commits out of HEAD in reverse order was tried first. When a turn's commit and a merge touch the same lines, it nests conflict markers inside conflict markers. Rebuilding forward never reverts across a merge. On the #1285 scenario it produces exactly the base that test expects.
  Date/Author: 2026-10-21, Claude.

- Decision: Commits the turn wrote that HEAD no longer reaches (squash-merged upstream, rebased, amended, left on another branch, reset away) are reverted out of the rebuilt base with conflicts resolved to the base's side (`git merge-file --ours`). Their presence in the captured worktree is tested with a second, marker-keeping merge. Those whose content is absent are recorded as `pending` in the new review point, so the next review still counts them as the turn's.
  Rationale: The turn's work can return inside a squash commit someone else made, and without the revert it would be hidden. Resolving conflicts to the base keeps amended and rebased originals from adding marker noise. Work committed on a branch the turn then left was never in any capture. Without the carry, it would be counted as someone else's when it came back.
  Date/Author: 2026-10-21, Claude.

- Decision: A review point records where it sits in the HEAD reflog as `reflog <entry count> <timestamp of the newest entry>`. A review finds its baseline's entry at that index from the oldest end, or, after reflog expiry removed old entries, by searching backward for that timestamp and commit.
  Rationale: Entries expire only from the old end, so the count locates the entry exactly even when one second holds several entries on one commit. The point stays deterministic: equal states give equal ids.
  Date/Author: 2026-10-21, Claude.

- Decision: When the reflog cannot account for HEAD's movement, the review diffs from the baseline tree, with nothing separated. That happens when the baseline came from an earlier release, the entry is gone, HEAD moved without an entry, or the reflog cannot be read. The worker logs a warning, and `RepoDelta.unattributed` carries the reason into the prompt, which tells the reviewer to confirm with `git log` that the turn wrote the code before reporting. The #1285 first-parent merge folding is removed.
  Rationale: One owner decides attribution. Keeping the merge folding as a second path would leave two answers to one question and hide when the primary one is unavailable. Reviewing too much is the safe direction, and the reason makes it visible. A session upgraded mid-flight falls back once, for the first review whose baseline predates this release.
  Date/Author: 2026-10-21, Claude.

- Decision: Moves are not the turn's work, including a reset or checkout that discards reviewed commits.
  Rationale: The graph cannot tell "discard" from "switch to other work", and the review covers what the turn wrote. A deletion the turn commits is still reviewed.
  Date/Author: 2026-10-21, Claude.

## Outcomes & Retrospective

A turn's review now holds the commits its own worktree made, its merge resolutions and its uncommitted edits. Commits it pulled, fast-forwarded, merged or switched to stay out, and so do other people's squash commits, even when those carry the turn's own work: that work is reverted back out of the base so it is still reviewed. Work left on another branch is carried as pending until it returns. The review point stays one tag naming the captured tree, so the daemon's records did not change; the relay protocol gained one optional field.

Remaining limits are deliberate. A commit the turn writes with `cherry-pick` or `merge --squash` from other history counts as its own. A reset or checkout that discards reviewed commits is not shown as a deletion. A session whose baseline predates this release, and whose HEAD has moved, gets one whole-tree review, with the reason in the prompt. The replay does no rename detection, as before.

## Context and Orientation

The worker (`mj-worker`, one process per session) does all Git work for reviews. `mj-worker/src/review/capture.rs` has three entry points. `initialize_review_baselines` runs before the primary agent starts and pins a starting point. `capture_repository_deltas` runs when a review starts: it captures the workspace as a tree built from HEAD plus every changed path, computes the base, and diffs the two. `advance_baselines` pins a reviewed capture as the next baseline. The Git primitives live in `mj-checkpoint/src/archive/git.rs` and are re-exported from `mj-checkpoint/src/archive.rs`.

A review point is the id a capture hands on and a completed review pins as the next baseline. It is an annotated Git tag that points at the captured tree. Its message holds `head <commit>` (HEAD when captured) and `session <id>`; this change adds `reflog …` and `pending <commit>` lines. A tag is still a tree-ish: Git resolves it to a tree wherever a tree is expected, so the daemon stores the id as an opaque string and nothing outside the worker parses it.

The daemon (`mj-controller`) sends baselines back in `ReviewerRequest::CaptureDelta`. The worker answers with one `RepoDelta` per repository (`mj-core/src/relay/protocol.rs`), whose `baseline_tree` is the base and whose `merged_in` lists commits the base already holds. The review driver (`mj-review/src/driver.rs`) turns those into `RepositoryCapture` values, and `mj-review/src/lanes.rs` writes the prompt.

Terms. The HEAD reflog is Git's log of every value HEAD has held in one worktree, with the action that set it (`git log -g HEAD`). A tree-ish is any id Git can resolve to a tree. A three-way merge combines two versions of a tree from a common ancestor, keeping each side's changes. The "reduced tree" of a commit is defined in the Decision Log above.

## Plan of Work

In `mj-checkpoint/src/archive/git.rs`, add `ReflogEntry { commit, tree, parents, timestamp, message }` and `read_head_reflog`, which runs `git log -g --no-abbrev --date=raw -z --no-show-signature --format=%H%x1f%T%x1f%P%x1f%gd%x1f%gs HEAD` and returns the entries oldest first, or none when HEAD has no reflog. Add `ReflogEntry::moves_only()` holding the message classification. Extend `ReviewPoint` with `reflog: Option<ReflogAnchor>` and `pending: Vec<String>`, and `record_review_point` and `read_review_point` with them. Give `merge_trees` a `MergeConflicts` argument (`Mark` or `KeepOurs`), which adds `--ours` to `git merge-file`.

Add `mj-worker/src/review/attribution.rs` with `review_base`, which replaces the function of that name in `capture.rs` and returns the base tree, the foreign tips for `merged_in`, the `unattributed` reason, and the pending list for the next point. `capture_repository_deltas` reads the reflog once, computes the base before recording the new point, and records the point with its anchor and pending list. `initialize_review_baselines` records its anchor the same way.

In `mj-core/src/relay/protocol.rs`, add `unattributed: Option<String>` to `RepoDelta` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, and widen `merged_in`'s meaning to "history the turn merged, pulled or switched to". Carry it through `mj-review/src/driver.rs` into `RepositoryCapture` and add one prompt sentence when it is set.

## Concrete Steps

From the repository root:

    cargo test -p brokk-mj-checkpoint review_point
    cargo test -p brokk-mj-worker --lib review::
    cargo test -p brokk-mj-review
    cargo clippy --all-targets -- -D warnings

Run Cargo tests outside a restricted sandbox; the worker suite uses loopback sockets.

## Validation and Acceptance

`mj-worker/src/review/capture.rs` holds the scenarios, each on a real repository. `a_merge_during_the_turn_reviews_only_the_turns_work` (#1285) must keep its assertions with the upstream commits written in a linked worktree. `a_pulled_or_squashed_upstream_is_not_the_turns_work` is the bifrost scenario. Another clone pushes unrelated commits to `main`; the turn commits on a branch, pushes it, and a squash of that branch lands upstream; the turn pulls `main` with a fast-forward, starts a new branch from it, and commits again. The patch must contain both of the turn's commits, including the one that only reached HEAD inside the squash, and none of the other clone's lines. A pending-carry scenario commits on one branch, switches away before the review, and later checks that branch out again. The fallback scenario deletes the reflog and expects a whole-tree diff with `unattributed` set.

## Idempotence and Recovery

Captures write only Git objects and scratch index files, which are removed on every exit path. Equal states give equal review point ids. No database migration is involved, and the daemon and relay protocol gain one optional field.

## Artifacts and Notes

The diagnosed clone's HEAD reflog around the failing review (times +0200):

    cf5c853a 12:36:06 pull -q --ff-only origin main: Fast-forward
    d1433144 12:44:00 checkout: moving from main to issue-651-envoy-gateway-prod
    04228c25 12:44:11 commit (merge): Merge remote-tracking branch 'origin/main' into issue-651-envoy-gateway-prod
    a1821751 12:45:06 commit: Record the staging switch and document the production edge (#651)
    2a48b011 13:06:24 checkout: moving from issue-651-envoy-gateway-prod to issue-651-edge-production-script
    ccdaa250 13:07:28 commit: Add a script that moves the production NLB to Envoy Gateway (#651)

`2a48b011` is GitHub's squash of the session's own pull request #662, and `cf5c853a` and its four predecessors are another developer's.

## Interfaces and Dependencies

In `mj-checkpoint/src/archive/git.rs`:

    pub struct ReflogEntry { pub commit: String, pub tree: String, pub parents: Vec<String>, pub timestamp: i64, pub message: String }
    impl ReflogEntry { pub fn moves_only(&self) -> bool }
    pub fn read_head_reflog(runner: &dyn GitCommandRunner, repository: &Path) -> Result<Vec<ReflogEntry>>;
    pub struct ReflogAnchor { pub entries: usize, pub timestamp: Option<i64> }
    pub struct ReviewPoint { pub tree: String, pub head: Option<String>, pub reflog: Option<ReflogAnchor>, pub pending: Vec<String>, pub session: Option<String> }
    pub enum MergeConflicts { Mark, KeepOurs }
    pub fn merge_trees(runner, repository, ancestor: Option<&str>, ours: &str, theirs: &str, conflicts: MergeConflicts) -> Result<String>;

In `mj-core/src/relay/protocol.rs`, `RepoDelta` gains `pub unattributed: Option<String>`; in `mj-review/src/lanes.rs`, `RepositoryCapture` gains the same.
