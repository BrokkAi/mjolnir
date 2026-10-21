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
- [x] (2026-10-21 15:40Z) Review round: a rewrite of a reviewed commit (amend, rebase step, cherry-pick) is the turn's only beyond its counterpart; fallbacks keep the pending commits; relay protocol 42 for `RepoDelta.unattributed`. Tests `a_rewrite_of_reviewed_commits_reviews_only_what_it_changed` and the extended `work_left_on_another_branch_is_reviewed_when_it_returns`; disabling counterparts, amend inheritance, replay matching, or the pending carry on fallback each fails one of them.
- [x] (2026-10-21 16:10Z) Full suites of the touched crates and workspace clippy after the review round: mj-checkpoint 144, mj-core 503, mj-review 27, mj-worker 686 plus its integration tests, mj-controller 2007 of 2008, with the same host-only `shasum` failure as before.
- [x] (2026-10-21 18:10Z) Third review round: one pending rule for attributed reviews and fallbacks alike, keyed on whether the capture holds the commit's change rather than on reachability, with conflicts counting as held only for commits HEAD reaches; amend originals are never pending. Test `pending_work_the_capture_lacks_is_reviewed_when_it_returns`; counting every conflict as held, skipping reachable pending commits in a fallback, or skipping reachable commits in an attributed review each fails it.
- [x] (2026-10-21 17:00Z) Second review round: `rebase (reset)` counts as a move; a recreated merge keeps its counterpart's resolution; a fallback carries only the pending commits HEAD does not reach and the capture does not hold. The rewrite test now replays a reviewed conflict-resolving merge with `git rebase --rebase-merges`, and the left-branch test has one pending commit return during a fallback while another stays pending. Disabling the reset rule, the merge counterpart, or the fallback filter each fails one of them.
- [x] (2026-10-21 14:30Z) Full suites of the touched crates and workspace clippy: mj-checkpoint 144, mj-core 503, mj-review 27, mj-worker 685 plus its integration tests, mj-controller 2007 of 2008. The one failure, `installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths`, needs `shasum`, which this Fedora host lacks; it touches no code this change edits.

## Surprises & Discoveries

- Observation: Within one second every reflog entry has the same timestamp, so a baseline cannot be found in the reflog by timestamp and commit alone. Tests run whole scenarios inside one second.
  Evidence: a scratch repository's `git log -g --date=raw --format='%H %gd %gs'` printed nine entries, all `HEAD@{1792584094 +0200}`, three of them on the same commit.

- Observation: The #1285 regression test made its "upstream" commits in the session's own worktree (`git checkout -qb upstream`, commit, checkout back). Under reflog attribution those are the turn's own commits. In practice upstream work comes from another clone or worktree, so the test now writes it in a linked worktree, whose HEAD reflog is separate.
  Evidence: `mj-worker/src/review/capture.rs`, `a_merge_during_the_turn_reviews_only_the_turns_work`, before this change.

- Observation: The #1285 merge folding also hid the turn's own work. A turn that merged its own feature branch into main with `--no-ff` had the feature commits folded into the base as "merged in", because the fold could not tell the merged line's authorship either.

- Observation: `git cherry-pick --ff` records `cherry-pick: fast-forward`, which reads the same as picking a commit whose subject is "fast-forward". Only `merge` and `pull` fast-forwards count as moves; the cherry-pick case counts as a commit, which reviews one extra commit at worst. A rebase with nothing to replay records `rebase (start)` and `rebase (finish)`.
  Evidence: `git log -g --format=%gs HEAD` in a scratch repository after `git rebase main`, `git cherry-pick --ff main`, `git pull . main` and `git merge --ff-only main`.

- Observation: The first version reduced every commit the turn made to its parent's tree, so an amend or rebase of a commit an earlier review covered put that whole commit back in the review. The plain tree diff before this change had shown only the amend's change. Found by the turn review of commit 424f97670.
  Evidence: amending the reviewed commit `two` after `git rebase main` produced a patch with `+fn one() {}` and all of `feature.rs`.

- Observation: `git rebase --rebase-merges` records `rebase (reset): 'onto'` when it moves HEAD back to the rebase target between replayed branches, and records the recreated merge as `rebase (merge)` (or `rebase (continue)` after a conflict) with the original merge's author, authoring time and subject. Found by the second turn review.
  Evidence: a scratch repository's HEAD reflog after `git rebase -r newbase` over a conflicting merge, and `git log -1 --format='%ae %at %s'` of the original and recreated merges.

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

- Decision: A commit the turn made that rewrites a commit outside the turn's own set (amend, rebase step, cherry-pick) keeps that counterpart's change in its reduced tree, so only what the rewrite changed is the turn's. An amend's counterpart is the commit HEAD held before it, inherited through amends of amends. A rebase step's or cherry-pick's, including a merge `--rebase-merges` recreates, is the unique commit HEAD left since the baseline with the same author email, authoring time and subject; none or several leave the commit reviewed whole. For a merge the counterpart's change is its resolution.
  Rationale: Fixing a review finding with `git commit --amend` is common, and re-reviewing the whole commit buries the fix. Git records no source for a rebase step in HEAD's reflog, but a rebase keeps the author, authoring time and message. Reworded rebase steps, squashes of several reviewed commits, and amends with `--reset-author` are not matched and are reviewed whole, which reviews too much, not too little.
  Date/Author: 2026-10-21, Claude.

- Decision: One rule decides whether a commit the turn made stays pending, in an attributed review and a fallback alike: it stays while the capture holds none of its change. A hunk is held when the capture has it. For a commit HEAD reaches, a hunk that conflicts also counts as held, because later history changed those lines again and the review saw the commit's change on the way. For a commit HEAD does not reach, a conflict means the capture has other lines there, so it does not count. A commit with any hunk held leaves pending, and the original an amend replaced never enters it.
  Rationale: Reachability alone dropped work the turn merged and then reverted, and a marker-keeping test dropped work a later mainline change conflicted with; both could return in someone else's commit unreviewed. Counting every conflict as not held, as the third review suggested, would keep a partly present commit pending and show its present part again in every later review, and would carry every commit whose lines the turn edited again. Amend originals are excluded because the turn's own amend carries what is left of them.
  Date/Author: 2026-10-21, Claude.

- Decision: Every fallback carries the baseline's pending commits that HEAD does not reach and the capture does not hold into the new point, and relay protocol 42 marks the worker that can send `RepoDelta.unattributed`.
  Rationale: A whole-tree diff reviews the pending work the capture holds, so carrying that again would review it twice, while dropping the rest would let work on a left branch be counted as someone else's once attribution resumes. `RepoDelta` refuses unknown fields, so a protocol-41 controller attached to a new worker would fail to decode a fallback capture. With the bump it refuses the worker at hello instead, which the controller already tests (`mj-controller/src/worker_client/tests.rs`, `future_protocol`). `tests/e2e/parent_report_notifications.py` now takes its protocol from hello.
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

Remaining limits are deliberate. A commit the turn writes with `merge --squash` from other history, or by cherry-picking a commit HEAD never held, counts as its own. A reworded rebase step, a squash of several reviewed commits, and an amend with `--reset-author` are reviewed whole. A reset or checkout that discards reviewed commits is not shown as a deletion. A session whose baseline predates this release, and whose HEAD has moved, gets one whole-tree review, with the reason in the prompt. The replay does no rename detection, as before.

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
