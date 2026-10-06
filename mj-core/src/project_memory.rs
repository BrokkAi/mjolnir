//! Project-scoped persistent memory shared across harnesses.
//!
//! Each session works with a private replica that the controller synchronizes
//! with the canonical project memory tree.

use crate::hex::lower_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{DiffTag, TextDiff};

pub const MEMORY_INDEX: &str = "MEMORY.md";
pub const MAX_DOCUMENT_BYTES: usize = 100 * 1024;
pub const MAX_VIRTUAL_PATH_BYTES: usize = 1024;
pub const STARTUP_INDEX_LINES: usize = 200;
pub const STARTUP_INDEX_BYTES: usize = 25 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemorySnapshot {
    pub files: BTreeMap<String, String>,
}

impl ProjectMemorySnapshot {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Version of a whole memory tree: a hash over its sorted paths and contents.
/// Two trees have the same version exactly when they hold the same files.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TreeVersion(pub String);

impl ProjectMemorySnapshot {
    pub fn version(&self) -> TreeVersion {
        let mut hash = Sha256::new();
        hash.update(b"mj-project-memory-tree-v1\0");
        hash.update((self.files.len() as u64).to_be_bytes());
        for (path, content) in &self.files {
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            hash.update((content.len() as u64).to_be_bytes());
            hash.update(content.as_bytes());
        }
        TreeVersion(lower_hex(hash.finalize()))
    }
}

/// One file where both sides changed the same lines, so the line merge could
/// not decide. `replica_wins` is the line merge with the replica's text in
/// every conflicting block; `TreeMerge::tree` already holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedFile {
    pub path: String,
    pub base: Option<String>,
    pub canonical: String,
    pub replica: String,
    pub replica_wins: String,
}

/// Result of a three-way tree merge. `tree` is always complete and clean: a
/// caller that cannot resolve `conflicts` better can store it as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeMerge {
    pub tree: ProjectMemorySnapshot,
    pub conflicts: Vec<ConflictedFile>,
}

/// Merge a session replica into the canonical tree relative to the baseline
/// the replica was seeded from, line by line within each file.
pub fn merge_trees(
    base: &ProjectMemorySnapshot,
    canonical: &ProjectMemorySnapshot,
    replica: &ProjectMemorySnapshot,
) -> TreeMerge {
    let paths = base
        .files
        .keys()
        .chain(canonical.files.keys())
        .chain(replica.files.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut tree = ProjectMemorySnapshot::default();
    let mut conflicts = Vec::new();

    for path in paths {
        let base_content = base.files.get(&path);
        let canonical_content = canonical.files.get(&path);
        let replica_content = replica.files.get(&path);
        let merged = if canonical_content == replica_content {
            canonical_content.cloned()
        } else if base_content == canonical_content {
            replica_content.cloned()
        } else if base_content == replica_content {
            canonical_content.cloned()
        } else {
            match (base_content, canonical_content, replica_content) {
                (None, Some(canonical_text), Some(replica_text)) => {
                    conflicts.push(ConflictedFile {
                        path: path.clone(),
                        base: None,
                        canonical: canonical_text.clone(),
                        replica: replica_text.clone(),
                        replica_wins: replica_text.clone(),
                    });
                    Some(replica_text.clone())
                }
                (Some(_), None, Some(replica_text)) => Some(replica_text.clone()),
                (Some(_), Some(canonical_text), None) => Some(canonical_text.clone()),
                (Some(base_text), Some(canonical_text), Some(replica_text)) => {
                    let (replica_wins, conflicted) =
                        merge_file_lines(base_text, canonical_text, replica_text);
                    if conflicted {
                        conflicts.push(ConflictedFile {
                            path: path.clone(),
                            base: Some(base_text.clone()),
                            canonical: canonical_text.clone(),
                            replica: replica_text.clone(),
                            replica_wins: replica_wins.clone(),
                        });
                    }
                    Some(replica_wins)
                }
                // These cases are covered by the unchanged-side rules above.
                (None, None, None)
                | (None, None, Some(_))
                | (None, Some(_), None)
                | (Some(_), None, None) => None,
            }
        };

        if let Some(content) = merged.filter(|content| !content.trim().is_empty()) {
            tree.files.insert(path, content);
        }
    }

    resolve_path_collisions(&mut tree, replica);
    TreeMerge { tree, conflicts }
}

/// A file/descendant collision cannot be installed as a tree. Keep the
/// colliding path present in the replica; if both or neither are present,
/// retain the ancestor as a deterministic tie-break.
fn resolve_path_collisions(tree: &mut ProjectMemorySnapshot, replica: &ProjectMemorySnapshot) {
    let mut retained = BTreeSet::new();
    for path in tree.files.keys() {
        let Some(ancestor) = first_retained_ancestor(path, &retained) else {
            retained.insert(path.clone());
            continue;
        };

        let ancestor_is_replica = replica.files.contains_key(&ancestor);
        let path_is_replica = replica.files.contains_key(path);
        if path_is_replica && !ancestor_is_replica {
            retained.remove(&ancestor);
            retained.insert(path.clone());
        }
    }
    tree.files.retain(|path, _| retained.contains(path));
}

fn first_retained_ancestor(path: &str, retained: &BTreeSet<String>) -> Option<String> {
    let mut end = path.len();
    while let Some(separator) = path[..end].rfind('/') {
        if separator == 0 {
            break;
        }
        let ancestor = &path[..separator];
        if retained.contains(ancestor) {
            return retained.get(ancestor).cloned();
        }
        end = separator;
    }
    None
}

#[derive(Debug, Clone)]
struct LineEdit {
    old: std::ops::Range<usize>,
    replacement: std::ops::Range<usize>,
}

fn line_edits(base: &[&str], side: &[&str]) -> Vec<LineEdit> {
    let diff = TextDiff::from_slices(base, side);
    let mut edits = Vec::new();
    let mut pending: Option<LineEdit> = None;
    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            if let Some(edit) = pending.take() {
                edits.push(edit);
            }
            continue;
        }

        let old = op.old_range();
        let replacement = op.new_range();
        if let Some(edit) = &mut pending {
            edit.old.end = old.end;
            edit.replacement.end = replacement.end;
        } else {
            pending = Some(LineEdit { old, replacement });
        }
    }
    if let Some(edit) = pending {
        edits.push(edit);
    }
    edits
}

fn edits_overlap(left: &LineEdit, right: &LineEdit) -> bool {
    let left_insert = left.old.is_empty();
    let right_insert = right.old.is_empty();
    match (left_insert, right_insert) {
        (true, true) => left.old.start == right.old.start,
        (true, false) => right.old.start < left.old.start && left.old.start < right.old.end,
        (false, true) => left.old.start < right.old.start && right.old.start < left.old.end,
        (false, false) => left.old.start < right.old.end && right.old.start < left.old.end,
    }
}

fn edit_overlaps_region(edit: &LineEdit, start: usize, end: usize) -> bool {
    if edit.old.is_empty() {
        start < edit.old.start && edit.old.start < end
    } else {
        edit.old.start < end && edit.old.end > start
    }
}

fn apply_region_edits<'a>(
    base: &[&'a str],
    side: &[&'a str],
    edits: &[LineEdit],
    start: usize,
    end: usize,
) -> Vec<&'a str> {
    let mut output = Vec::new();
    let mut cursor = start;
    for edit in edits {
        output.extend_from_slice(&base[cursor..edit.old.start]);
        output.extend_from_slice(&side[edit.replacement.clone()]);
        cursor = edit.old.end;
    }
    output.extend_from_slice(&base[cursor..end]);
    output
}

fn side_line_offset(base_position: usize, edits: &[LineEdit], edit_count: usize) -> usize {
    let mut output_position = 0;
    let mut base_position_before = 0;
    for edit in &edits[..edit_count] {
        output_position += edit.old.start - base_position_before;
        output_position += edit.replacement.len();
        base_position_before = edit.old.end;
    }
    output_position + base_position - base_position_before
}

fn split_shared_boundary_insertions(
    edits: &mut Vec<LineEdit>,
    side: &[&str],
    other_edits: &[LineEdit],
    other_side: &[&str],
) {
    let mut insertions = Vec::new();
    for insertion in other_edits.iter().filter(|edit| edit.old.is_empty()) {
        let insertion_text = &other_side[insertion.replacement.clone()];
        for edit in edits.iter_mut().filter(|edit| !edit.old.is_empty()) {
            let extra_lines = edit.replacement.len().saturating_sub(edit.old.len());
            if insertion_text.is_empty() || extra_lines < insertion_text.len() {
                continue;
            }

            if edit.old.end == insertion.old.start {
                let split_at = edit.replacement.end - insertion_text.len();
                if &side[split_at..edit.replacement.end] == insertion_text
                    && split_at - edit.replacement.start >= edit.old.len()
                {
                    insertions.push(LineEdit {
                        old: insertion.old.start..insertion.old.start,
                        replacement: split_at..edit.replacement.end,
                    });
                    edit.replacement.end = split_at;
                    break;
                }
            }

            if edit.old.start == insertion.old.start {
                let split_at = edit.replacement.start + insertion_text.len();
                if &side[edit.replacement.start..split_at] == insertion_text
                    && edit.replacement.end - split_at >= edit.old.len()
                {
                    insertions.push(LineEdit {
                        old: insertion.old.start..insertion.old.start,
                        replacement: edit.replacement.start..split_at,
                    });
                    edit.replacement.start = split_at;
                    break;
                }
            }
        }
    }
    edits.extend(insertions);
    edits.sort_by_key(|edit| (edit.old.start, edit.old.end));
}

fn merge_file_lines(base_text: &str, canonical_text: &str, replica_text: &str) -> (String, bool) {
    let base_text = line_merge_input(base_text);
    let canonical_text = line_merge_input(canonical_text);
    let replica_text = line_merge_input(replica_text);
    let base = base_text.split_inclusive('\n').collect::<Vec<_>>();
    let canonical = canonical_text.split_inclusive('\n').collect::<Vec<_>>();
    let replica = replica_text.split_inclusive('\n').collect::<Vec<_>>();
    let mut canonical_edits = line_edits(&base, &canonical);
    let mut replica_edits = line_edits(&base, &replica);
    split_shared_boundary_insertions(&mut canonical_edits, &canonical, &replica_edits, &replica);
    split_shared_boundary_insertions(&mut replica_edits, &replica, &canonical_edits, &canonical);
    let mut canonical_index = 0;
    let mut replica_index = 0;
    let mut base_cursor = 0;
    let mut output = Vec::new();
    let mut conflicted = false;

    while canonical_index < canonical_edits.len() || replica_index < replica_edits.len() {
        let canonical_edit = canonical_edits.get(canonical_index);
        let replica_edit = replica_edits.get(replica_index);

        if let (Some(canonical_edit), Some(replica_edit)) = (canonical_edit, replica_edit)
            && canonical_edit.old.is_empty()
            && replica_edit.old.is_empty()
            && canonical_edit.old.start == replica_edit.old.start
        {
            output.extend_from_slice(&base[base_cursor..canonical_edit.old.start]);
            let canonical_insert = &canonical[canonical_edit.replacement.clone()];
            let replica_insert = &replica[replica_edit.replacement.clone()];
            output.extend_from_slice(canonical_insert);
            if canonical_insert != replica_insert {
                output.extend_from_slice(replica_insert);
            }
            base_cursor = canonical_edit.old.start;
            canonical_index += 1;
            replica_index += 1;
            continue;
        }

        if let (Some(canonical_edit), Some(replica_edit)) = (canonical_edit, replica_edit)
            && canonical_edit.old.start == replica_edit.old.start
            && (canonical_edit.old.is_empty() || replica_edit.old.is_empty())
        {
            let (edit, side, next_index) = if canonical_edit.old.is_empty() {
                (canonical_edit, &canonical, &mut canonical_index)
            } else {
                (replica_edit, &replica, &mut replica_index)
            };
            output.extend_from_slice(&base[base_cursor..edit.old.start]);
            output.extend_from_slice(&side[edit.replacement.clone()]);
            base_cursor = edit.old.end;
            *next_index += 1;
            continue;
        }

        let overlaps = match (canonical_edit, replica_edit) {
            (Some(canonical_edit), Some(replica_edit)) => {
                edits_overlap(canonical_edit, replica_edit)
            }
            _ => false,
        };
        if !overlaps {
            let take_canonical = match (canonical_edit, replica_edit) {
                (Some(canonical_edit), Some(replica_edit)) => {
                    canonical_edit.old.start < replica_edit.old.start
                }
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => unreachable!(),
            };
            let (edit, side, index) = if take_canonical {
                (canonical_edit.unwrap(), &canonical, &mut canonical_index)
            } else {
                (replica_edit.unwrap(), &replica, &mut replica_index)
            };
            output.extend_from_slice(&base[base_cursor..edit.old.start]);
            output.extend_from_slice(&side[edit.replacement.clone()]);
            base_cursor = edit.old.end;
            *index += 1;
            continue;
        }

        let canonical_edit = canonical_edit.unwrap();
        let replica_edit = replica_edit.unwrap();
        let start = canonical_edit.old.start.min(replica_edit.old.start);
        let mut end = canonical_edit.old.end.max(replica_edit.old.end);
        let mut canonical_end = canonical_index;
        let mut replica_end = replica_index;

        loop {
            let previous = (canonical_end, replica_end, end);
            while canonical_end < canonical_edits.len()
                && edit_overlaps_region(&canonical_edits[canonical_end], start, end)
            {
                end = end.max(canonical_edits[canonical_end].old.end);
                canonical_end += 1;
            }
            while replica_end < replica_edits.len()
                && edit_overlaps_region(&replica_edits[replica_end], start, end)
            {
                end = end.max(replica_edits[replica_end].old.end);
                replica_end += 1;
            }
            if previous == (canonical_end, replica_end, end) {
                break;
            }
        }

        let canonical_region = apply_region_edits(
            &base,
            &canonical,
            &canonical_edits[canonical_index..canonical_end],
            start,
            end,
        );
        let replica_region = apply_region_edits(
            &base,
            &replica,
            &replica_edits[replica_index..replica_end],
            start,
            end,
        );
        let base_region = &base[start..end];
        output.extend_from_slice(&base[base_cursor..start]);

        if canonical_region == replica_region {
            output.extend_from_slice(&canonical_region);
        } else if canonical_region == base_region {
            output.extend_from_slice(&replica_region);
        } else if replica_region == base_region {
            output.extend_from_slice(&canonical_region);
        } else if canonical_region.len() == base_region.len()
            && replica_region.len() == base_region.len()
        {
            let canonical_modified = frontmatter_modified_lines(&canonical);
            let replica_modified = frontmatter_modified_lines(&replica);
            let canonical_line_start = side_line_offset(start, &canonical_edits, canonical_index);
            let replica_line_start = side_line_offset(start, &replica_edits, replica_index);
            for offset in 0..base_region.len() {
                let base_line = base_region[offset];
                let canonical_line = canonical_region[offset];
                let replica_line = replica_region[offset];
                let canonical_line_index = canonical_line_start + offset;
                let replica_line_index = replica_line_start + offset;
                if canonical_line == replica_line {
                    output.push(canonical_line);
                } else if canonical_line == base_line
                    || (replica_line != base_line
                        && canonical_modified[canonical_line_index]
                        && replica_modified[replica_line_index])
                {
                    output.push(replica_line);
                } else if replica_line == base_line {
                    output.push(canonical_line);
                } else {
                    output.push(replica_line);
                    conflicted = true;
                }
            }
        } else {
            output.extend_from_slice(&replica_region);
            conflicted = true;
        }

        base_cursor = end;
        canonical_index = canonical_end;
        replica_index = replica_end;
    }

    output.extend_from_slice(&base[base_cursor..]);
    (output.concat(), conflicted)
}

fn line_merge_input(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

fn frontmatter_modified_lines(lines: &[&str]) -> Vec<bool> {
    let mut modified = vec![false; lines.len()];
    if lines.first().is_none_or(|line| line.trim() != "---") {
        return modified;
    }
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            break;
        }
        let Some((key, _)) = line.trim_start().split_once(':') else {
            continue;
        };
        modified[index] = key.trim() == "modified";
    }
    modified
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOutcome {
    Swapped,
    /// The canonical tree no longer has the expected version. Read and merge again.
    Changed,
}

/// Read the canonical tree, following project-merge redirects.
pub fn read_canonical(canonical_root: &Path) -> Result<ProjectMemorySnapshot> {
    let root = resolve_canonical_root(canonical_root)?;
    ProjectMemoryStore::new(root).snapshot()
}

/// Replace the canonical tree with `tree` only if it still has `expected`.
/// The per-project lock is held only for the compare and the write.
pub fn swap_canonical(
    canonical_root: &Path,
    expected: &TreeVersion,
    tree: &ProjectMemorySnapshot,
) -> Result<SwapOutcome> {
    let mut locks = MEMORY_LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("project memory lock registry poisoned");
    let canonical_root = resolve_canonical_root(canonical_root)?;
    let lock = locks
        .entry(canonical_root.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().expect("project memory lock poisoned");
    drop(locks);

    let store = ProjectMemoryStore::new(canonical_root);
    let current = store.snapshot()?;
    if current.version() != expected.clone() {
        return Ok(SwapOutcome::Changed);
    }
    store.replace_tree(tree)?;
    Ok(SwapOutcome::Swapped)
}

/// Worker answer to a whole-tree replace request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaReplaceOutcome {
    Replaced,
    /// The replica changed after the controller read it; nothing was written.
    ReplicaChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryMemoryIdentity {
    Network {
        url: String,
    },
    Github {
        owner: String,
        repository: String,
    },
    Local {
        canonical_root: PathBuf,
    },
    Remote {
        target: String,
        canonical_root: PathBuf,
    },
}

impl RepositoryMemoryIdentity {
    /// Reconstruct the identity used by legacy launches from their configured
    /// sources, independently of names in a newly discovered checkout.
    pub fn from_configured(repository: &crate::config::ProjectRepository) -> Result<Self> {
        if let Some(source) = repository.github.as_deref() {
            let identity = crate::repository::RepositoryIdentity::from_remote(source)
                .with_context(|| {
                    format!("parse repository source {source:?} for project memory")
                })?;
            return Ok((&identity).into());
        }
        let root = repository
            .local
            .as_ref()
            .context("project repository has no source for memory identity")?;
        Ok(Self::Local {
            canonical_root: crate::local_git::main_worktree_root(root)
                .or_else(|_| std::fs::canonicalize(root).map_err(anyhow::Error::from))
                .unwrap_or_else(|_| root.clone()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectMemoryIdentity {
    Project {
        key: String,
    },
    Repository {
        repository: RepositoryMemoryIdentity,
    },
    Bundle {
        primary: RepositoryMemoryIdentity,
        members: Vec<RepositoryMemoryIdentity>,
    },
}

impl ProjectMemoryIdentity {
    /// The exact identity used before accepted project snapshots were stored.
    /// Discovery and worker launch share this calculation so raw directories
    /// cannot borrow memory from an unrelated configured project.
    pub fn for_legacy_session(
        session: &crate::state::SessionRecord,
        bundle: Option<&crate::config::ProjectBundle>,
        parent_worktree: Option<&crate::state::ManagedWorktree>,
    ) -> Result<Self> {
        if let Some(worktree) = session.managed_worktree.as_ref().or(parent_worktree) {
            return Ok(Self::Repository {
                repository: RepositoryMemoryIdentity::Local {
                    canonical_root: std::fs::canonicalize(&worktree.source_repository)
                        .unwrap_or_else(|_| worktree.source_repository.clone()),
                },
            });
        }
        if let Some(bundle) = bundle.filter(|_| session.project_directory.is_none()) {
            let primary = RepositoryMemoryIdentity::from_configured(
                bundle.primary().context("bundle primary is missing")?,
            )?;
            let members = bundle
                .repositories
                .iter()
                .map(RepositoryMemoryIdentity::from_configured)
                .collect::<Result<Vec<_>>>()?;
            return Ok(Self::bundle(primary, members));
        }
        let project = session
            .project_directory
            .as_ref()
            .context("raw session project directory is missing")?;
        let repository = match session.target.as_ref() {
            Some(crate::state::TargetLocator::LocalBare { .. }) => {
                RepositoryMemoryIdentity::Local {
                    canonical_root: std::fs::canonicalize(project)
                        .unwrap_or_else(|_| project.clone()),
                }
            }
            _ => RepositoryMemoryIdentity::Remote {
                target: session.target_template_id.clone(),
                canonical_root: project.clone(),
            },
        };
        Ok(Self::Repository { repository })
    }

    /// Stable, non-secret directory key for controller-side memory storage.
    pub fn key(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self).context("encode project memory identity")?;
        Ok(lower_hex(Sha256::digest(encoded)))
    }

    pub fn bundle(
        primary: RepositoryMemoryIdentity,
        mut members: Vec<RepositoryMemoryIdentity>,
    ) -> Self {
        members.sort_by_key(identity_sort_key);
        members.dedup();
        if members.len() == 1 {
            return Self::Repository {
                repository: primary,
            };
        }
        Self::Bundle { primary, members }
    }
}

impl From<&crate::repository::RepositoryIdentity> for RepositoryMemoryIdentity {
    fn from(identity: &crate::repository::RepositoryIdentity) -> Self {
        use crate::repository::RepositoryIdentity;
        match identity {
            RepositoryIdentity::Github(owner, repository) => Self::Github {
                owner: owner.clone(),
                repository: repository.clone(),
            },
            RepositoryIdentity::Network(url) => Self::Network { url: url.clone() },
            RepositoryIdentity::Local(root) => Self::Local {
                canonical_root: root.clone(),
            },
            RepositoryIdentity::RemoteDirectory { host, root } => Self::Remote {
                target: host.clone(),
                canonical_root: root.clone(),
            },
        }
    }
}

impl crate::repository::ProjectBundleSnapshot {
    pub fn memory_identity(&self) -> Result<ProjectMemoryIdentity> {
        if self.bundle.repositories.len() == 1 {
            Ok(ProjectMemoryIdentity::Repository {
                repository: self
                    .identities
                    .get(&self.bundle.primary_repo)
                    .context("project primary identity is missing")?
                    .into(),
            })
        } else {
            Ok(ProjectMemoryIdentity::Project { key: self.key()? })
        }
    }
}

fn identity_sort_key(identity: &RepositoryMemoryIdentity) -> String {
    serde_json::to_string(identity).expect("repository memory identity is serializable")
}

#[derive(Debug, Clone)]
pub struct ProjectMemoryStore {
    root: PathBuf,
}

impl ProjectMemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn startup_index(&self) -> Result<Option<String>> {
        let path = self.root.join(MEMORY_INDEX);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(truncate_startup_index(&content)))
    }

    /// Capture the safe, model-visible tree for controller synchronization.
    pub fn snapshot(&self) -> Result<ProjectMemorySnapshot> {
        let mut entries = Vec::new();
        if self.root.is_dir() {
            collect_entries(&self.root, &self.root, &mut entries)?;
        }
        let mut files = BTreeMap::new();
        let mut total = 0_usize;
        for entry in entries {
            anyhow::ensure!(
                entry.bytes <= MAX_DOCUMENT_BYTES,
                "project memory snapshot document {} exceeds {MAX_DOCUMENT_BYTES} bytes",
                entry.path
            );
            ensure_snapshot_budget(&mut total, entry.bytes)?;
            let relative = validate_virtual_path(&entry.path)?;
            let content = fs::read_to_string(self.root.join(relative))
                .with_context(|| format!("read memory snapshot document {}", entry.path))?;
            ensure_snapshot_budget(&mut total, content.len().saturating_sub(entry.bytes))?;
            files.insert(entry.path, content);
        }
        Ok(ProjectMemorySnapshot { files })
    }

    /// Apply documents from a legacy relay snapshot. This stays additive so an
    /// older controller cannot delete documents it does not know to sync.
    pub fn install_snapshot(&self, snapshot: &ProjectMemorySnapshot) -> Result<bool> {
        let mut total = 0_usize;
        let mut changed = false;
        for (path, content) in &snapshot.files {
            ensure_snapshot_budget(&mut total, content.len())?;
            let relative = validate_virtual_path(path)?;
            reject_symlink_path(&self.root, &relative, true)?;
            ensure_snapshot_document(content)?;
            let destination = self.root.join(relative);
            match fs::read_to_string(&destination) {
                Ok(existing) if existing == *content => continue,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("read memory document {}", destination.display())
                    });
                }
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::config::atomic_write(&destination, content.as_bytes())?;
            changed = true;
        }
        Ok(changed)
    }

    /// Make the directory hold exactly `tree`: write changed documents and
    /// delete documents that `tree` omits. Returns whether anything changed.
    pub fn replace_tree(&self, tree: &ProjectMemorySnapshot) -> Result<bool> {
        let mut total = 0_usize;
        let mut desired = BTreeMap::<String, (PathBuf, &str)>::new();
        let mut relative_paths = BTreeSet::new();
        for (path, content) in &tree.files {
            ensure_snapshot_budget(&mut total, content.len())?;
            let relative = validate_virtual_path(path)?;
            reject_symlink_path(&self.root, &relative, true)?;
            ensure_snapshot_document(content)?;
            anyhow::ensure!(
                relative_paths.insert(relative.clone()),
                "project memory snapshot contains duplicate paths"
            );
            desired.insert(path.clone(), (relative, content));
        }
        for relative in &relative_paths {
            let mut parent = relative.parent();
            while let Some(path) = parent {
                anyhow::ensure!(
                    !relative_paths.contains(path),
                    "project memory snapshot contains both a document and its parent"
                );
                parent = path.parent();
            }
        }

        reject_symlink_path(&self.root, Path::new(""), true)?;
        if let Ok(metadata) = fs::symlink_metadata(&self.root) {
            anyhow::ensure!(metadata.is_dir(), "project memory root is not a directory");
        }
        let current = self.snapshot()?;
        let mut changed = false;

        for path in current.files.keys() {
            if desired.contains_key(path) || path == "/memory-redirect.json" {
                continue;
            }
            let relative = validate_virtual_path(path)?;
            reject_symlink_path(&self.root, &relative, false)?;
            let file = self.root.join(relative);
            match fs::remove_file(&file) {
                Ok(()) => changed = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("delete memory document {}", file.display()));
                }
            }
        }

        prune_empty_memory_directories(&self.root, &mut changed)?;

        for (path, (relative, content)) in desired {
            if path == "/memory-redirect.json"
                || current
                    .files
                    .get(&path)
                    .is_some_and(|existing| existing == content)
            {
                continue;
            }
            reject_symlink_path(&self.root, &relative, true)?;
            let destination = self.root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create memory directory {}", parent.display()))?;
            }
            crate::config::atomic_write(&destination, content.as_bytes())
                .with_context(|| format!("write memory document {}", destination.display()))?;
            changed = true;
        }
        Ok(changed)
    }
}

fn prune_empty_memory_directories(root: &Path, changed: &mut bool) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut directories = Vec::new();
    collect_visible_directories(root, root, &mut directories)?;
    for directory in directories {
        match fs::remove_dir(&directory) {
            Ok(()) => *changed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let mut entries = fs::read_dir(&directory)
                    .with_context(|| format!("inspect memory directory {}", directory.display()))?;
                if entries.next().transpose()?.is_none() {
                    return Err(error).with_context(|| {
                        format!("remove empty memory directory {}", directory.display())
                    });
                }
            }
        }
    }
    Ok(())
}

fn collect_visible_directories(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("list memory directory {}", directory.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_symlink()
            || entry.file_name().to_string_lossy().starts_with('.')
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        collect_visible_directories(root, &path, output)?;
        if path != root {
            output.push(path);
        }
    }
    Ok(())
}

fn ensure_snapshot_budget(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total
        .checked_add(bytes)
        .context("memory snapshot size overflow")?;
    anyhow::ensure!(
        *total <= MAX_SNAPSHOT_BYTES,
        "project memory snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"
    );
    Ok(())
}

fn ensure_snapshot_document(content: &str) -> Result<()> {
    anyhow::ensure!(
        content.len() <= MAX_DOCUMENT_BYTES,
        "project memory snapshot document exceeds {MAX_DOCUMENT_BYTES} bytes"
    );
    anyhow::ensure!(
        !content.trim().is_empty(),
        "project memory snapshot contains an empty document"
    );
    Ok(())
}

static MEMORY_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

/// Redirects are published while holding both store locks and the decision
/// registry. A stale sync cannot decide to write the old store during a merge.
pub fn resolve_canonical_root(root: &Path) -> Result<PathBuf> {
    let mut root = root.to_owned();
    let mut visited = std::collections::BTreeSet::new();
    loop {
        ensure_memory_alias_unique(&mut visited, &root)?;
        let alias = root
            .parent()
            .context("memory store has no project directory")?
            .join("memory-redirect.json");
        match fs::read(&alias) {
            Ok(bytes) => {
                root = serde_json::from_slice(&bytes).context("decode project memory redirect")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(root),
            Err(error) => return Err(error).context("read project memory redirect"),
        }
    }
}

fn ensure_memory_alias_unique(
    visited: &mut std::collections::BTreeSet<PathBuf>,
    root: &Path,
) -> Result<()> {
    if !visited.insert(root.to_owned()) {
        bail!("project memory redirect cycle at {}", root.display());
    }
    Ok(())
}

pub fn merge_canonical_stores(old: &Path, new: &Path) -> Result<Vec<String>> {
    let mut locks = MEMORY_LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("project memory lock registry poisoned");
    let old = resolve_canonical_root(old)?;
    let new = resolve_canonical_root(new)?;
    if old == new {
        return Ok(Vec::new());
    }
    let old_lock = locks
        .entry(old.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let new_lock = locks
        .entry(new.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let (_first_guard, _second_guard) = if old < new {
        (
            old_lock.lock().expect("project memory lock poisoned"),
            new_lock.lock().expect("project memory lock poisoned"),
        )
    } else {
        (
            new_lock.lock().expect("project memory lock poisoned"),
            old_lock.lock().expect("project memory lock poisoned"),
        )
    };
    let old_snapshot = ProjectMemoryStore::new(&old).snapshot()?;
    let new_store = ProjectMemoryStore::new(&new);
    let current = new_store.snapshot()?;
    let merged = merge_trees(&ProjectMemorySnapshot::default(), &current, &old_snapshot);
    new_store.replace_tree(&merged.tree)?;
    let parent = old
        .parent()
        .context("old memory project directory is missing")?;
    fs::create_dir_all(parent)?;
    crate::config::atomic_write(
        &parent.join("memory-redirect.json"),
        &serde_json::to_vec(&new)?,
    )?;
    drop(locks);
    Ok(merged
        .conflicts
        .into_iter()
        .map(|conflict| conflict.path)
        .collect())
}

pub fn truncate_startup_index(content: &str) -> String {
    let mut output = String::new();
    for (index, line) in content.lines().enumerate() {
        if index >= STARTUP_INDEX_LINES {
            break;
        }
        let separator = usize::from(!output.is_empty());
        if output.len() + separator + line.len() > STARTUP_INDEX_BYTES {
            let remaining = STARTUP_INDEX_BYTES.saturating_sub(output.len() + separator);
            let boundary = floor_char_boundary(line, remaining);
            if separator == 1 && boundary > 0 {
                output.push('\n');
            }
            output.push_str(&line[..boundary]);
            break;
        }
        if separator == 1 {
            output.push('\n');
        }
        output.push_str(line);
    }
    output
}

pub const MEMORY_GUIDANCE: &str = "Persistent project memory is background context; verify it against current instructions and code. Keep one fact per Markdown file with frontmatter for name, description, and metadata.type. Add a one-line pointer to each file in MEMORY.md, update existing notes instead of duplicating them, and delete notes that are wrong.";

pub fn startup_prompt_context(
    store: &ProjectMemoryStore,
    repository_roots: &BTreeMap<String, PathBuf>,
) -> Result<String> {
    let index = store.startup_index()?.unwrap_or_default();
    let mut context = vec![
        "<mj-project-memory>".to_owned(),
        MEMORY_GUIDANCE.to_owned(),
        format!("Project memory directory: {}", store.root().display()),
    ];
    if repository_roots.len() > 1 {
        context.push("For this multi-root project, keep bundle-wide memories at the memory directory root and intentionally repository-specific notes under roots/<repository-id>/. Workspace roots:".into());
        context.extend(
            repository_roots
                .iter()
                .map(|(id, root)| format!("- {id}: {}", root.display())),
        );
    }
    context.push("<memory-index path=\"MEMORY.md\">".into());
    if index.is_empty() {
        context.push("(empty)".into());
    } else {
        context.push(index);
    }
    context.push("</memory-index>".into());
    context.push("</mj-project-memory>".into());
    Ok(context.join("\n"))
}

fn floor_char_boundary(text: &str, maximum: usize) -> usize {
    let mut boundary = maximum.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn validate_virtual_path(path: &str) -> Result<PathBuf> {
    if path.len() > MAX_VIRTUAL_PATH_BYTES {
        bail!("memory path exceeds {MAX_VIRTUAL_PATH_BYTES} bytes");
    }
    if !path.starts_with('/') || path.contains('\\') || path.chars().any(char::is_control) {
        bail!("memory path must be a safe virtual absolute path");
    }
    if path == "/" {
        bail!("memory document path cannot be the root");
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir => {}
            Component::Normal(segment) => {
                let text = segment.to_string_lossy();
                if text.starts_with('.')
                    || matches!(text.as_ref(), "skills" | "commands" | "agents" | "hooks")
                {
                    bail!("memory path contains a reserved segment");
                }
                relative.push(segment);
            }
            _ => bail!("memory path contains an unsafe segment"),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("memory document path cannot be empty");
    }
    Ok(relative)
}

fn reject_symlink_path(root: &Path, relative: &Path, missing_allowed: bool) -> Result<()> {
    let mut current = root.to_path_buf();
    if let Ok(metadata) = fs::symlink_metadata(root)
        && metadata.file_type().is_symlink()
    {
        bail!("memory root cannot be a symlink");
    }
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("memory path cannot traverse a symlink")
            }
            Ok(_) => {}
            Err(error) if missing_allowed && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct MemoryFileEntry {
    path: String,
    bytes: usize,
}

fn collect_entries(root: &Path, directory: &Path, output: &mut Vec<MemoryFileEntry>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("list memory directory {}", directory.display()))?
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if entry.file_type()?.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            collect_entries(root, &entry.path(), output)?;
        } else if metadata.is_file() && !entry.file_name().to_string_lossy().starts_with('.') {
            output.push(snapshot_entry(root, &entry.path())?);
        }
    }
    Ok(())
}

fn snapshot_entry(root: &Path, path: &Path) -> Result<MemoryFileEntry> {
    let metadata = fs::metadata(path)?;
    let relative = path.strip_prefix(root)?;
    let rendered = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
    Ok(MemoryFileEntry {
        path: rendered,
        bytes: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_snapshot(content: Option<&str>) -> ProjectMemorySnapshot {
        ProjectMemorySnapshot {
            files: content
                .map(|content| BTreeMap::from([("/memory.md".into(), content.into())]))
                .unwrap_or_default(),
        }
    }

    fn merge_file(base: Option<&str>, canonical: Option<&str>, replica: Option<&str>) -> TreeMerge {
        merge_trees(
            &file_snapshot(base),
            &file_snapshot(canonical),
            &file_snapshot(replica),
        )
    }

    fn seed_file(store: &ProjectMemoryStore, path: &str, content: &str) {
        let relative = validate_virtual_path(path).unwrap();
        let file = store.root.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }

    #[test]
    fn tree_versions_distinguish_trees_and_path_content_boundaries() {
        let first = ProjectMemorySnapshot {
            files: BTreeMap::from([("/a".into(), "bc".into())]),
        };
        let second = ProjectMemorySnapshot {
            files: BTreeMap::from([("/ab".into(), "c".into())]),
        };
        let third = ProjectMemorySnapshot {
            files: BTreeMap::from([("/a".into(), "bd".into())]),
        };
        assert_ne!(first.version(), second.version());
        assert_ne!(first.version(), third.version());
        assert_eq!(first.version(), first.clone().version());
    }

    // Hard-won: 2753a110: appending lines lost the canonical append when the base lacked a final newline.
    #[test]
    fn merge_trees_keeps_appends_when_base_has_no_final_newline() {
        let merged = merge_file(Some("base"), Some("base\ncanonical"), Some("base\nreplica"));
        assert_eq!(
            merged.tree.files["/memory.md"],
            "base\ncanonical\nreplica\n"
        );
        assert!(merged.conflicts.is_empty());
    }

    /// Two sessions edit the same memory file concurrently. Each row is
    /// (name, base, canonical, replica, merged, conflicted).
    #[test]
    fn merge_trees_combines_concurrent_session_edits() {
        type Case = (
            &'static str,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            bool,
        );
        let cases: [Case; 6] = [
            (
                "both sessions append a pointer line",
                Some("a\n"),
                Some("a\nc\n"),
                Some("a\nr\n"),
                Some("a\nc\nr\n"),
                false,
            ),
            (
                "edits to separate lines both survive",
                Some("1\n2\n3\n4\n5\n"),
                Some("X\n2\n3\n4\n5\n"),
                Some("1\n2\n3\n4\nY\n"),
                Some("X\n2\n3\n4\nY\n"),
                false,
            ),
            (
                "a deletion syncs",
                Some("a\n"),
                Some("a\n"),
                None,
                None,
                false,
            ),
            (
                "a canonical edit beats a replica deletion",
                Some("a\n"),
                Some("b\n"),
                None,
                Some("b\n"),
                false,
            ),
            (
                "a replica edit beats a canonical deletion",
                Some("a\n"),
                None,
                Some("b\n"),
                Some("b\n"),
                false,
            ),
            (
                "the replica wins a same-line conflict",
                Some("a\nb\nc\n"),
                Some("a\nC\nc\n"),
                Some("a\nR\nc\n"),
                Some("a\nR\nc\n"),
                true,
            ),
        ];
        for (name, base, canonical, replica, expected, conflicted) in cases {
            let merged = merge_file(base, canonical, replica);
            assert_eq!(
                merged.tree.files.get("/memory.md").map(String::as_str),
                expected,
                "{name}"
            );
            assert_eq!(!merged.conflicts.is_empty(), conflicted, "{name}");
            if let Some(conflict) = merged.conflicts.first() {
                assert_eq!(Some(conflict.replica_wins.as_str()), expected, "{name}");
            }
        }
    }

    // Hard-won: 2753a110: a merged file and its descendant made the tree impossible to install.
    #[test]
    fn merge_trees_keep_replica_paths_for_file_descendant_collisions() {
        let snapshot = |files: &[(&str, &str)]| ProjectMemorySnapshot {
            files: files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
        };
        let cases = [
            (
                snapshot(&[]),
                snapshot(&[("/a", "canonical parent")]),
                snapshot(&[("/a/b.md", "replica child")]),
                snapshot(&[("/a/b.md", "replica child")]),
            ),
            (
                snapshot(&[]),
                snapshot(&[("/a/b.md", "canonical child")]),
                snapshot(&[("/a", "replica parent")]),
                snapshot(&[("/a", "replica parent")]),
            ),
            (
                snapshot(&[("/a", "base parent"), ("/a/b.md", "base child")]),
                snapshot(&[("/a", "canonical parent")]),
                snapshot(&[("/a/b.md", "replica child")]),
                snapshot(&[("/a/b.md", "replica child")]),
            ),
        ];

        for (base, canonical, replica, expected) in cases {
            let merged = merge_trees(&base, &canonical, &replica);
            assert_eq!(merged.tree, expected);
            assert!(merged.conflicts.is_empty());

            let directory = tempfile::tempdir().unwrap();
            let store = ProjectMemoryStore::new(directory.path());
            assert!(store.replace_tree(&merged.tree).unwrap());
            assert_eq!(store.snapshot().unwrap(), expected);
        }
    }

    #[test]
    fn bundle_identity_is_independent_of_member_order() {
        let primary = RepositoryMemoryIdentity::Github {
            owner: "brokkai".into(),
            repository: "hel".into(),
        };
        let worker = RepositoryMemoryIdentity::Github {
            owner: "brokkai".into(),
            repository: "worker".into(),
        };
        assert_eq!(
            ProjectMemoryIdentity::bundle(primary.clone(), vec![worker.clone(), primary.clone()]),
            ProjectMemoryIdentity::bundle(primary.clone(), vec![primary, worker]),
        );
    }

    #[test]
    fn startup_index_honors_both_limits_without_splitting_utf8() {
        let many_lines = (0..250).map(|_| "memory").collect::<Vec<_>>().join("\n");
        assert_eq!(truncate_startup_index(&many_lines).lines().count(), 200);
        let large = "é".repeat(20_000);
        let truncated = truncate_startup_index(&large);
        assert!(truncated.len() <= STARTUP_INDEX_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn replace_tree_leaves_hidden_files_redirect_metadata_and_root_alone() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProjectMemoryStore::new(directory.path());
        seed_file(&store, "/visible.md", "remove");
        fs::write(directory.path().join(".private.md"), "private").unwrap();
        fs::create_dir_all(directory.path().join(".private-dir")).unwrap();
        fs::write(directory.path().join(".private-dir/keep.md"), "private").unwrap();
        fs::write(directory.path().join("memory-redirect.json"), "redirect").unwrap();

        assert!(
            store
                .replace_tree(&ProjectMemorySnapshot::default())
                .unwrap()
        );
        assert!(directory.path().is_dir());
        assert!(!directory.path().join("visible.md").exists());
        assert_eq!(
            fs::read_to_string(directory.path().join(".private.md")).unwrap(),
            "private"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join(".private-dir/keep.md")).unwrap(),
            "private"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("memory-redirect.json")).unwrap(),
            "redirect"
        );
    }

    #[test]
    fn replace_tree_validates_the_entire_tree_before_deleting() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProjectMemoryStore::new(directory.path());
        seed_file(&store, "/existing.md", "keep");
        let invalid = ProjectMemorySnapshot {
            files: BTreeMap::from([
                ("/valid.md".into(), "valid".into()),
                ("/../invalid.md".into(), "invalid".into()),
            ]),
        };

        assert!(store.replace_tree(&invalid).is_err());
        assert_eq!(store.snapshot().unwrap().files["/existing.md"], "keep");
        assert!(!directory.path().join("valid.md").exists());
    }

    #[test]
    fn swap_canonical_reports_a_changed_version_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project/memory");
        let store = ProjectMemoryStore::new(&root);
        store
            .install_snapshot(&ProjectMemorySnapshot {
                files: BTreeMap::from([("/memory.md".into(), "read version".into())]),
            })
            .unwrap();
        let expected = read_canonical(&root).unwrap().version();
        store
            .install_snapshot(&ProjectMemorySnapshot {
                files: BTreeMap::from([("/memory.md".into(), "changed later".into())]),
            })
            .unwrap();
        let replacement = ProjectMemorySnapshot {
            files: BTreeMap::from([("/replacement.md".into(), "must not write".into())]),
        };

        assert_eq!(
            swap_canonical(&root, &expected, &replacement).unwrap(),
            SwapOutcome::Changed
        );
        assert_eq!(
            read_canonical(&root).unwrap().files["/memory.md"],
            "changed later"
        );
        assert!(!root.join("replacement.md").exists());
    }

    #[test]
    fn project_merge_redirects_stale_canonical_swaps() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old/memory");
        let new = directory.path().join("new/memory");
        let snapshot = |text: &str| ProjectMemorySnapshot {
            files: BTreeMap::from([("/MEMORY.md".into(), text.into())]),
        };
        ProjectMemoryStore::new(&old)
            .install_snapshot(&snapshot("old project facts"))
            .unwrap();
        ProjectMemoryStore::new(&new)
            .install_snapshot(&snapshot("new project facts"))
            .unwrap();
        let conflicts = merge_canonical_stores(&old, &new).unwrap();
        assert_eq!(conflicts, vec!["/MEMORY.md"]);
        assert_eq!(resolve_canonical_root(&old).unwrap(), new);
        assert!(merge_canonical_stores(&old, &new).unwrap().is_empty());
        let migrated = ProjectMemoryStore::new(&new).snapshot().unwrap();
        assert_eq!(migrated.files["/MEMORY.md"], "old project facts");
        assert!(
            !migrated
                .files
                .keys()
                .any(|path| path.starts_with("/conflicts/"))
        );
        let stale_baseline = snapshot("old project facts");
        let stale_replica = ProjectMemorySnapshot {
            files: BTreeMap::from([
                ("/MEMORY.md".into(), "old project facts".into()),
                ("/late.md".into(), "accepted by old worker".into()),
            ]),
        };
        let current = read_canonical(&old).unwrap();
        let stale_merge = merge_trees(&stale_baseline, &current, &stale_replica);
        assert_eq!(
            swap_canonical(&old, &current.version(), &stale_merge.tree).unwrap(),
            SwapOutcome::Swapped
        );
        let result = read_canonical(&old).unwrap();
        assert_eq!(result.files["/late.md"], "accepted by old worker");
        assert_eq!(result.files["/MEMORY.md"], "old project facts");
        assert!(
            !result
                .files
                .keys()
                .any(|path| path.starts_with("/conflicts/"))
        );
        assert!(
            !ProjectMemoryStore::new(&old)
                .snapshot()
                .unwrap()
                .files
                .contains_key("/late.md")
        );
    }
}
