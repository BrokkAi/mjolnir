//! Which part of a review capture the turn wrote.
//!
//! A review diffs a base tree against the capture, and the base must hold
//! everything the turn did not write, or the reviewer reports other people's
//! code. The commit graph cannot say who wrote a commit: upstream's commits
//! arrive by fast-forward and by checking out a branch made from upstream, and
//! the turn's own pushed work comes back inside other people's squash commits.
//! The worktree's HEAD reflog can: it records every commit this checkout made
//! and every time HEAD only moved to a commit that already existed. That
//! reflog is the one owner of the answer here.

use super::capture::{object_exists, run};
use anyhow::{Context, Result};
use mj_checkpoint::archive::{
    GitCommandRunner, MergeConflicts, ReflogAnchor, ReflogEntry, empty_tree_id, merge_trees,
    read_review_point,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The most commits a review walks between its baseline and HEAD. A longer
/// history is reviewed whole, with the reason in the prompt.
const HISTORY_LIMIT: usize = 10_000;

/// The most pending commits a review point carries; the newest are kept.
const PENDING_LIMIT: usize = 64;

/// What a review diffs from.
pub(crate) struct ReviewBase {
    /// The tree the capture is diffed from: the baseline, plus every change
    /// the turn did not write.
    pub tree: String,
    /// The commits of other history the turn merged, pulled or switched to,
    /// whose changes `tree` holds.
    pub merged_in: Vec<String>,
    /// Why `tree` is the plain baseline although HEAD moved, so the diff may
    /// hold changes the turn did not write.
    pub unattributed: Option<String>,
    /// The turn's commits that the capture does not hold, for the next review
    /// point to carry.
    pub pending: Vec<String>,
}

/// Where the review point of a capture taken now sits in HEAD's reflog, or
/// `None` when the reflog does not end at `head`.
pub(crate) fn reflog_anchor(reflog: &[ReflogEntry], head: Option<&str>) -> Option<ReflogAnchor> {
    match (reflog.last(), head) {
        (None, None) => Some(ReflogAnchor {
            entries: 0,
            timestamp: None,
        }),
        (Some(last), Some(head)) if last.commit == head => Some(ReflogAnchor {
            entries: reflog.len(),
            timestamp: Some(last.timestamp),
        }),
        _ => None,
    }
}

/// The tree a review compares `capture` with, so the diff holds the turn's
/// commits, its merge resolutions and its uncommitted edits, and nothing it
/// merged, pulled or switched to.
///
/// The reflog entries after the baseline's say which commits this worktree
/// made: those are the turn's, along with the baseline's pending commits.
/// Every commit HEAD reaches that the baseline commit does not then gets a
/// reduced tree, its tree without the turn's commits, built forward from the
/// baseline so no revert crosses a merge:
///
/// - a commit the turn made takes its parent's reduced tree; a merge it made
///   takes the merge of its parents' reduced trees, which leaves out the
///   resolution so the diff shows it, with conflict markers where the merged
///   lines met;
/// - any other commit keeps its own tree unless one of the turn's commits is
///   below it, and otherwise has its own change replayed onto its parent's
///   reduced tree.
///
/// A commit the turn made by amending, rebasing or cherry-picking a commit
/// that is not its own in this span, such as one an earlier review covered,
/// is the turn's only for what it changed beyond that counterpart: the
/// counterpart's change is replayed into the reduced tree. An amend names its
/// counterpart in the reflog, the commit HEAD held before it. A rebase step or
/// cherry-pick keeps its counterpart's author, authoring time and subject,
/// which are matched among the commits HEAD has left since the baseline.
///
/// HEAD's reduced tree still holds the turn's commits that HEAD no longer
/// reaches but that came back in someone else's squash or rebase, so those
/// are reverted out of it, settling conflicts to the base's side. The turn's
/// commits the capture does not hold at all, such as work left on another
/// branch, become `pending` so a later review still counts them as the
/// turn's. Last, the baseline's reviewed uncommitted edits are merged back in.
///
/// When the reflog cannot account for how HEAD moved since the baseline, the
/// review diffs from the baseline tree and says why in `unattributed`.
pub(crate) fn review_base(
    git: &dyn GitCommandRunner,
    repository: &Path,
    baseline: &str,
    head: Option<&str>,
    reflog: Result<&[ReflogEntry], &str>,
    capture: &str,
) -> Result<ReviewBase> {
    let point = read_review_point(git, repository, baseline)?;
    let plain = |unattributed: Option<String>, pending: Vec<String>| {
        if let Some(reason) = &unattributed {
            tracing::warn!(
                repository = %repository.display(),
                reason = %reason,
                "the review cannot tell the turn's commits from other history; it reviews every change since the baseline"
            );
        }
        ReviewBase {
            tree: baseline.to_owned(),
            merged_in: Vec::new(),
            unattributed,
            pending,
        }
    };
    // Nothing committed means nothing to attribute.
    let Some(head) = head else {
        return Ok(plain(None, point.pending));
    };
    // A fallback's whole-tree diff reviews the pending commits HEAD now
    // reaches or the capture otherwise holds, and carries only the rest.
    let fallback = |reason: String| -> Result<ReviewBase> {
        let mut history = History::new(git, repository);
        let mut carried = Vec::new();
        for commit in history.read_existing(&point.pending)? {
            if history.is_ancestor(&commit, head)? {
                continue;
            }
            let before = history.change_base(&commit)?;
            let after = history.tree(&commit)?;
            if before != after && !history.holds(&after, &before, capture)? {
                carried.push(commit);
            }
        }
        Ok(plain(Some(reason), carried))
    };
    let Some(anchor) = point.reflog else {
        if point.head.as_deref() == Some(head) {
            return Ok(plain(None, point.pending.clone()));
        }
        return fallback(
            "the baseline does not record its place in the HEAD reflog; it was taken by an \
             earlier release, or while HEAD had no reflog"
                .to_owned(),
        );
    };
    if let Some(since) = &point.head
        && !object_exists(git, repository, &format!("{since}^{{commit}}"))
    {
        // The commit outlives a rewrite in the reflog for at least 30 days, so
        // this takes deliberate history deletion.
        return fallback(format!(
            "the baseline's commit {since} is no longer in the repository"
        ));
    }
    let reflog = match reflog {
        Ok(reflog) => reflog,
        Err(error) => {
            return fallback(format!("the HEAD reflog could not be read: {error}"));
        }
    };
    let Some((before, since)) = entries_since(reflog, anchor, point.head.as_deref()) else {
        return fallback(
            "the HEAD reflog no longer holds the entry the baseline was taken at".to_owned(),
        );
    };
    if since
        .last()
        .map_or(point.head.as_deref(), |last| Some(last.commit.as_str()))
        != Some(head)
    {
        return fallback("HEAD moved without a reflog entry since the baseline".to_owned());
    }
    if since.is_empty() {
        return Ok(plain(None, point.pending.clone()));
    }
    let made = match commits_made(before, since) {
        Ok(made) => made,
        Err(reason) => return fallback(reason),
    };

    let mut history = History::new(git, repository);
    for entry in reflog {
        history.learn(&entry.commit, &entry.tree, &entry.parents);
    }
    let pending = history.read_existing(&point.pending)?;
    let mut own = pending;
    for made in &made {
        if !own.contains(&made.commit) {
            own.push(made.commit.clone());
        }
    }
    let own_set = own.iter().cloned().collect::<HashSet<_>>();
    let positions = before
        .last()
        .into_iter()
        .chain(since)
        .map(|entry| entry.commit.as_str())
        .collect::<Vec<_>>();
    let counterparts = counterparts(&mut history, &made, &own_set, &positions, head)?;
    let Some(order) = history.walk(head, point.head.as_deref())? else {
        return fallback(format!(
            "HEAD's history since the baseline is longer than {HISTORY_LIMIT} commits"
        ));
    };
    let reachable = order.iter().cloned().collect::<HashSet<_>>();

    // Parents come before their children in `order`, so every reduced tree a
    // commit needs is already known.
    let mut reduced = HashMap::<String, String>::new();
    for commit in &order {
        let parents = history.parents(commit)?;
        let mine = own_set.contains(commit);
        if !mine && !parents.iter().any(|parent| reduced.contains_key(parent)) {
            continue;
        }
        // The parents' reduced trees, merged for a merge commit.
        let start = match parents.as_slice() {
            [] => history.empty_tree()?,
            [parent] => history.reduced(&reduced, parent)?,
            _ => history.merge_parents(&parents, Some(&reduced))?,
        };
        // A rewrite keeps its counterpart's change, which for a merge that
        // `rebase --rebase-merges` recreated is the reviewed resolution.
        let tree = if !mine {
            history.replay(commit, &start)?
        } else if let Some(counterpart) = counterparts.get(commit) {
            history.replay(counterpart, &start)?
        } else {
            start
        };
        reduced.insert(commit.clone(), tree);
    }
    let mut base = history.reduced(&reduced, head)?;

    let mut still_pending = Vec::new();
    for commit in own
        .iter()
        .rev()
        .filter(|commit| !reachable.contains(*commit))
    {
        // What the turn changed: beyond its parent, beyond the unresolved
        // merge of a merge's parents, and beyond a rewrite's counterpart.
        let mut before = history.change_base(commit)?;
        if let Some(counterpart) = counterparts.get(commit) {
            before = history.replay(counterpart, &before)?;
        }
        let after = history.tree(commit)?;
        if before == after {
            continue;
        }
        base = history.merge3(&after, &base, &before, MergeConflicts::KeepOurs)?;
        if !history.holds(&after, &before, capture)? {
            still_pending.push(commit.clone());
        }
    }
    still_pending.truncate(PENDING_LIMIT);
    still_pending.reverse();

    let reviewed = match &point.head {
        Some(since) => history.tree(since)?,
        None => history.empty_tree()?,
    };
    let tree = history.merge3(&reviewed, &point.tree, &base, MergeConflicts::Mark)?;

    // The other history the base holds, named by where the turn met it: a
    // commit the turn built on or merged, or HEAD itself.
    let mut merged_in = Vec::new();
    for commit in own.iter().filter(|commit| reachable.contains(*commit)) {
        for parent in history.parents(commit)? {
            if reachable.contains(&parent)
                && !own_set.contains(&parent)
                && !merged_in.contains(&parent)
            {
                merged_in.push(parent);
            }
        }
    }
    if reachable.contains(head)
        && !own_set.contains(head)
        && !merged_in.iter().any(|tip| tip == head)
    {
        merged_in.push(head.to_owned());
    }
    Ok(ReviewBase {
        tree,
        merged_in,
        unattributed: None,
        pending: still_pending,
    })
}

/// The reflog entries up to and including the baseline's, and those after
/// it, or `None` when the reflog no longer holds it.
fn entries_since<'a>(
    reflog: &'a [ReflogEntry],
    anchor: ReflogAnchor,
    head: Option<&str>,
) -> Option<(&'a [ReflogEntry], &'a [ReflogEntry])> {
    if anchor.entries == 0 {
        return head.is_none().then_some((&reflog[..0], reflog));
    }
    let matches = |entry: &ReflogEntry| {
        Some(entry.timestamp) == anchor.timestamp && Some(entry.commit.as_str()) == head
    };
    // Expiry removes entries from the oldest end only, so the baseline's entry
    // is at its recorded place or earlier.
    let last = (anchor.entries - 1).min(reflog.len().checked_sub(1)?);
    let index = (0..=last).rev().find(|index| matches(&reflog[*index]))?;
    Some(reflog.split_at(index + 1))
}

/// A commit this worktree made, and what the reflog says it replaced.
struct Made {
    commit: String,
    /// The commit HEAD held before an amend, which the amend replaced.
    amended: Option<String>,
    /// Whether it replays another commit, as a rebase step or cherry-pick
    /// does.
    replays: bool,
}

/// The commits this worktree made in `since`, oldest first, or why the
/// entries do not account for HEAD's movement.
///
/// Every commit made here sits on the commit HEAD held before it, or on that
/// commit's parent when it replaced it, as an amend or a rebase squash does.
/// A commit that sits elsewhere means HEAD moved without an entry in between.
fn commits_made(before: &[ReflogEntry], since: &[ReflogEntry]) -> Result<Vec<Made>, String> {
    let mut previous = before.last();
    let mut made = Vec::new();
    for entry in since {
        if previous.is_some_and(|previous| previous.commit == entry.commit) {
            continue;
        }
        if !entry.moves_only() {
            let first = entry.parents.first();
            let follows = first.is_none()
                || previous.is_some_and(|previous| {
                    Some(&previous.commit) == first || previous.parents.first() == first
                });
            if !follows {
                return Err(format!(
                    "the HEAD reflog records commit {} on a commit HEAD did not hold, so HEAD \
                     moved without a reflog entry",
                    entry.commit
                ));
            }
            made.push(Made {
                commit: entry.commit.clone(),
                amended: entry
                    .amends()
                    .then(|| previous.map(|previous| previous.commit.clone()))
                    .flatten(),
                replays: entry.replays(),
            });
        }
        previous = Some(entry);
    }
    Ok(made)
}

/// The commit each of the turn's rewrites replaced, where that commit is not
/// the turn's: a reviewed commit, or someone else's, whose change the rewrite
/// carries and the review must not show as the turn's.
///
/// An amend of one of the turn's own commits inherits that commit's
/// counterpart. A rebase step or cherry-pick is matched by author, authoring
/// time and subject among the commits HEAD held since the baseline and no
/// longer reaches; a match that is not unique is left unmatched, which
/// reviews the commit whole.
fn counterparts(
    history: &mut History<'_>,
    made: &[Made],
    own: &HashSet<String>,
    positions: &[&str],
    head: &str,
) -> Result<HashMap<String, String>> {
    let replaying = made
        .iter()
        .filter(|made| made.replays)
        .map(|made| made.commit.as_str())
        .collect::<Vec<_>>();
    let (identities, left) = if replaying.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        (
            history.identities(&replaying)?,
            history.left_behind(positions, head, own)?,
        )
    };
    let mut counterparts = HashMap::<String, String>::new();
    for made in made {
        let counterpart = match &made.amended {
            Some(previous) if own.contains(previous) => counterparts.get(previous).cloned(),
            Some(previous) => Some(previous.clone()),
            None if made.replays => identities
                .get(&made.commit)
                .and_then(|identity| left.get(identity))
                .cloned()
                .flatten(),
            None => None,
        };
        if let Some(counterpart) = counterpart {
            counterparts.insert(made.commit.clone(), counterpart);
        }
    }
    Ok(counterparts)
}

/// What the attribution knows of the repository's commits, read in as few
/// Git calls as it can.
struct History<'a> {
    git: &'a dyn GitCommandRunner,
    repository: &'a Path,
    commits: HashMap<String, (String, Vec<String>)>,
    empty_tree: Option<String>,
}

impl<'a> History<'a> {
    fn new(git: &'a dyn GitCommandRunner, repository: &'a Path) -> Self {
        Self {
            git,
            repository,
            commits: HashMap::new(),
            empty_tree: None,
        }
    }

    fn learn(&mut self, commit: &str, tree: &str, parents: &[String]) {
        self.commits
            .entry(commit.to_owned())
            .or_insert_with(|| (tree.to_owned(), parents.to_vec()));
    }

    /// Reads `commits` and returns those still in the repository, in order.
    fn read_existing(&mut self, commits: &[String]) -> Result<Vec<String>> {
        if commits.is_empty() {
            return Ok(Vec::new());
        }
        let mut arguments = vec!["log", "--no-walk=unsorted", "--ignore-missing"];
        arguments.extend(LOG_FORMAT);
        arguments.extend(commits.iter().map(String::as_str));
        let found = self.read_log(&arguments, "read the pending review commits")?;
        Ok(commits
            .iter()
            .filter(|commit| found.contains(*commit))
            .cloned()
            .collect())
    }

    /// The commits HEAD reaches and `since` does not, parents first, or
    /// `None` when there are more than [`HISTORY_LIMIT`].
    fn walk(&mut self, head: &str, since: Option<&str>) -> Result<Option<Vec<String>>> {
        let limit = format!("--max-count={}", HISTORY_LIMIT + 1);
        let exclude = since.map(|since| format!("^{since}"));
        let mut arguments = vec!["log", "--topo-order", limit.as_str()];
        arguments.extend(LOG_FORMAT);
        arguments.push(head);
        arguments.extend(exclude.as_deref());
        let mut order = self.read_log(&arguments, "list the history since the review baseline")?;
        if order.len() > HISTORY_LIMIT {
            return Ok(None);
        }
        order.reverse();
        Ok(Some(order))
    }

    /// Runs a `git log` with [`LOG_FORMAT`], learns every commit it prints,
    /// and returns them in its order.
    fn read_log(&mut self, arguments: &[&str], action: &str) -> Result<Vec<String>> {
        let output = run(self.git, self.repository, arguments)?;
        anyhow::ensure!(
            output.status == 0,
            "{action} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let mut commits = Vec::new();
        for record in output.stdout.split(|byte| *byte == 0) {
            let record = String::from_utf8_lossy(record);
            let mut fields = record.trim().splitn(3, '\x1f');
            let (Some(commit), Some(tree), Some(parents)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let parents = parents
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            self.learn(commit, tree, &parents);
            commits.push(commit.to_owned());
        }
        Ok(commits)
    }

    fn known(&mut self, commit: &str) -> Result<&(String, Vec<String>)> {
        if !self.commits.contains_key(commit) {
            let mut arguments = vec!["log", "--no-walk"];
            arguments.extend(LOG_FORMAT);
            arguments.push(commit);
            self.read_log(&arguments, "read a commit for the review base")?;
        }
        self.commits
            .get(commit)
            .with_context(|| format!("commit {commit} is not in the repository"))
    }

    fn tree(&mut self, commit: &str) -> Result<String> {
        Ok(self.known(commit)?.0.clone())
    }

    fn parents(&mut self, commit: &str) -> Result<Vec<String>> {
        Ok(self.known(commit)?.1.clone())
    }

    /// The author, authoring time and subject of each of `commits`, which a
    /// rebase or cherry-pick keeps.
    fn identities(&mut self, commits: &[&str]) -> Result<HashMap<String, String>> {
        let mut arguments = vec!["log", "--no-walk=unsorted"];
        arguments.extend(IDENTITY_FORMAT);
        arguments.extend(commits);
        Ok(self
            .read_identities(&arguments, "read the turn's rewritten commits")?
            .into_iter()
            .collect())
    }

    /// The commits HEAD held at `positions` that `head` no longer reaches and
    /// that are not in `own`, by identity; `None` marks an identity two of
    /// them share. Empty when there are more than [`HISTORY_LIMIT`], which
    /// leaves every rewrite unmatched.
    fn left_behind(
        &mut self,
        positions: &[&str],
        head: &str,
        own: &HashSet<String>,
    ) -> Result<HashMap<String, Option<String>>> {
        let limit = format!("--max-count={}", HISTORY_LIMIT + 1);
        let exclude = format!("^{head}");
        let mut arguments = vec!["log", limit.as_str()];
        arguments.extend(IDENTITY_FORMAT);
        arguments.extend(positions);
        arguments.push(&exclude);
        let commits = self.read_identities(&arguments, "list the commits HEAD left")?;
        let mut left = HashMap::<String, Option<String>>::new();
        if commits.len() > HISTORY_LIMIT {
            return Ok(left);
        }
        for (commit, identity) in commits {
            if own.contains(&commit) {
                continue;
            }
            left.entry(identity)
                .and_modify(|found| *found = None)
                .or_insert(Some(commit));
        }
        Ok(left)
    }

    fn read_identities(&self, arguments: &[&str], action: &str) -> Result<Vec<(String, String)>> {
        let output = run(self.git, self.repository, arguments)?;
        anyhow::ensure!(
            output.status == 0,
            "{action} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|record| {
                let record = String::from_utf8_lossy(record);
                let (commit, identity) = record.trim_start().split_once('\x1f')?;
                Some((commit.to_owned(), identity.to_owned()))
            })
            .collect())
    }

    /// The tree `commit`'s own change is measured from: its parent's tree, the
    /// empty tree for a root commit, or the unresolved merge of a merge's
    /// parents.
    fn change_base(&mut self, commit: &str) -> Result<String> {
        let parents = self.parents(commit)?;
        match parents.as_slice() {
            [] => self.empty_tree(),
            [parent] => self.tree(parent),
            _ => self.merge_parents(&parents, None),
        }
    }

    /// Whether `capture` holds the change from `before` to `after`: reverting
    /// it, with any conflict kept, would change the capture.
    fn holds(&mut self, after: &str, before: &str, capture: &str) -> Result<bool> {
        Ok(self.merge3(after, capture, before, MergeConflicts::Mark)? != capture)
    }

    fn is_ancestor(&self, commit: &str, head: &str) -> Result<bool> {
        let output = run(
            self.git,
            self.repository,
            &["merge-base", "--is-ancestor", commit, head],
        )?;
        match output.status {
            0 => Ok(true),
            1 => Ok(false),
            status => anyhow::bail!(
                "checking whether {head} contains {commit} failed with status {status}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    /// `onto` with `commit`'s own change applied, keeping conflict markers.
    fn replay(&mut self, commit: &str, onto: &str) -> Result<String> {
        let ancestor = self.change_base(commit)?;
        let theirs = self.tree(commit)?;
        self.merge3(&ancestor, onto, &theirs, MergeConflicts::Mark)
    }

    /// `commit`'s reduced tree: its entry in `reduced`, or its own tree when
    /// none of the turn's commits are below it.
    fn reduced(&mut self, reduced: &HashMap<String, String>, commit: &str) -> Result<String> {
        match reduced.get(commit) {
            Some(tree) => Ok(tree.clone()),
            None => self.tree(commit),
        }
    }

    fn empty_tree(&mut self) -> Result<String> {
        if self.empty_tree.is_none() {
            self.empty_tree = Some(empty_tree_id(self.git, self.repository)?);
        }
        Ok(self.empty_tree.clone().expect("set above"))
    }

    /// The merge of a merge commit's parents as Git would make it before any
    /// resolution: of their reduced trees when `reduced` is given, and of
    /// their own trees otherwise. Each further parent of an octopus is merged
    /// in from its merge base with the first.
    fn merge_parents(
        &mut self,
        parents: &[String],
        reduced: Option<&HashMap<String, String>>,
    ) -> Result<String> {
        let side = |history: &mut Self, commit: &str| match reduced {
            Some(reduced) => history.reduced(reduced, commit),
            None => history.tree(commit),
        };
        let first = &parents[0];
        let mut merged = side(self, first)?;
        for parent in &parents[1..] {
            let ancestor = match merge_base(self.git, self.repository, first, parent)? {
                Some(base) => side(self, &base)?,
                None => self.empty_tree()?,
            };
            let theirs = side(self, parent)?;
            merged = self.merge3(&ancestor, &merged, &theirs, MergeConflicts::Mark)?;
        }
        Ok(merged)
    }

    /// Three-way merges trees, answering without Git when one side is
    /// unchanged.
    fn merge3(
        &mut self,
        ancestor: &str,
        ours: &str,
        theirs: &str,
        conflicts: MergeConflicts,
    ) -> Result<String> {
        if ours == theirs || theirs == ancestor {
            return Ok(ours.to_owned());
        }
        if ours == ancestor {
            return Ok(theirs.to_owned());
        }
        merge_trees(
            self.git,
            self.repository,
            Some(ancestor),
            ours,
            theirs,
            conflicts,
        )
    }
}

/// The `git log` options that print one commit, its tree and its parents per
/// NUL-separated record, whatever the user's log configuration.
const LOG_FORMAT: [&str; 4] = [
    "--no-show-signature",
    "--no-decorate",
    "-z",
    "--format=%H%x1f%T%x1f%P",
];

/// The `git log` options that print a commit and its author, authoring time
/// and subject per NUL-separated record.
const IDENTITY_FORMAT: [&str; 4] = [
    "--no-show-signature",
    "--no-decorate",
    "-z",
    "--format=%H%x1f%ae%x1f%at%x1f%s",
];

/// The best common ancestor of two commits, or `None` for unrelated histories.
fn merge_base(
    git: &dyn GitCommandRunner,
    repository: &Path,
    left: &str,
    right: &str,
) -> Result<Option<String>> {
    let output = run(git, repository, &["merge-base", left, right])?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        1 => Ok(None),
        status => anyhow::bail!(
            "finding the merge base of {left} and {right} failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}
