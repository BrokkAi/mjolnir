//! Capture and restore of an unfinished Git operation in an independent clone.
//!
//! A clone stopped in a merge, cherry-pick, revert, rebase or bisect keeps the
//! operation in three places: unmerged index entries, the working-tree files
//! of the unmerged paths, and state files in the Git directory. A patch can
//! carry none of them, so the operation travels beside the snapshot's patches
//! and is written back after the rest of the clone is restored.

use super::*;

/// The Git-directory files an unfinished operation keeps. Each is found with
/// `git rev-parse --git-path`, which knows which ones belong to the worktree
/// and which to the repository. The names are those Git's merge, sequencer,
/// am, bisect and rerere code write.
const STATE_FILES: [&str; 20] = [
    "AUTO_MERGE",
    "BISECT_ANCESTORS_OK",
    "BISECT_EXPECTED_REV",
    "BISECT_FIRST_PARENT",
    "BISECT_HEAD",
    "BISECT_LOG",
    "BISECT_NAMES",
    "BISECT_RUN",
    "BISECT_START",
    "BISECT_TERMS",
    "CHERRY_PICK_HEAD",
    "MERGE_AUTOSTASH",
    "MERGE_HEAD",
    "MERGE_MODE",
    "MERGE_MSG",
    "MERGE_RR",
    "ORIG_HEAD",
    "REBASE_HEAD",
    "REVERT_HEAD",
    "SQUASH_MSG",
];

/// The Git-directory directories an unfinished operation keeps, copied whole.
const STATE_DIRECTORIES: [&str; 4] = ["rebase-apply", "rebase-merge", "rr-cache", "sequencer"];

/// The state entries whose presence means an operation is in progress. The
/// others, such as `ORIG_HEAD` and `rr-cache`, outlive the operation that
/// wrote them, so they are kept only alongside one.
const IN_PROGRESS_MARKERS: [&str; 9] = [
    "BISECT_START",
    "CHERRY_PICK_HEAD",
    "MERGE_HEAD",
    "REBASE_HEAD",
    "REVERT_HEAD",
    "SQUASH_MSG",
    "rebase-apply",
    "rebase-merge",
    "sequencer",
];

/// Ref namespaces only an operation in progress writes: bisect's good and bad
/// marks, and the labels of `git rebase --rebase-merges`.
pub(super) const OPERATION_REF_NAMESPACES: [&str; 2] = ["refs/bisect/", "refs/rewritten/"];

/// Todo lists, whose instructions may name commits by abbreviated ids.
const TODO_LISTS: [&str; 4] = [
    "rebase-merge/done",
    "rebase-merge/git-rebase-todo",
    "rebase-merge/git-rebase-todo.backup",
    "sequencer/todo",
];

/// The state directory that holds file contents rather than object names.
const RERERE_CACHE: &str = "rr-cache";

/// An unfinished operation read from a clone, ready to archive.
pub(super) struct CollectedOperation {
    pub(super) metadata: SavedGitOperation,
    pub(super) payloads: GitOperationPayloads,
    /// The objects the operation names, by type, for the temporary refs that
    /// put them in the committed bundle.
    commits: Vec<String>,
    tags: Vec<String>,
    contents: Vec<(String, String)>,
}

impl CollectedOperation {
    /// The unmerged paths, each once, in index order. The staged and
    /// unstaged patches leave them out.
    pub(super) fn unmerged_paths(&self) -> Vec<PathBuf> {
        unique_paths(&self.metadata.unmerged)
    }

    /// Whether a carried object is an annotated tag, which a bundle must hold
    /// even when the commit it names is already on the base.
    pub(super) fn carries_tags(&self) -> bool {
        !self.tags.is_empty()
    }

    /// Points temporary refs below `prefix` at the objects the operation
    /// names, so the committed bundle carries them, and adds each ref to
    /// `refs` as soon as it exists so the caller removes it on every path.
    ///
    /// A bundle can only carry what refs reach. Commits become parents of one
    /// wrapper commit, whose tree holds the trees and blobs (an `AUTO_MERGE`
    /// tree, the stages of a conflict); annotated tags each get a ref.
    pub(super) fn carry_objects(
        &self,
        runner: &dyn GitCommandRunner,
        repository: &Path,
        prefix: &str,
        refs: &mut Vec<String>,
    ) -> Result<()> {
        for (index, tag) in self.tags.iter().enumerate() {
            let name = format!("{prefix}/tag/{index:08}");
            git_bytes(
                runner,
                repository,
                ["update-ref", &name, tag],
                &[],
                "preserve a Git operation tag for the checkpoint bundle",
            )?;
            refs.push(name);
        }
        if self.commits.is_empty() && self.contents.is_empty() {
            return Ok(());
        }
        let listing = self
            .contents
            .iter()
            .enumerate()
            .map(|(index, (kind, object))| {
                let mode = if kind == "tree" { "040000" } else { "100644" };
                format!("{mode} {kind} {object}\t{index:08}\n")
            })
            .collect::<String>();
        let tree = git_bytes(
            runner,
            repository,
            ["mktree"],
            listing.as_bytes(),
            "record the Git operation's trees and blobs",
        )?;
        let tree = trim_output(&tree, "decode the Git operation tree id")?;
        let mut arguments = vec![
            OsString::from("commit-tree"),
            OsString::from(tree),
            OsString::from("-m"),
            OsString::from("Mjolnir checkpoint: objects of an unfinished Git operation"),
        ];
        for commit in &self.commits {
            arguments.extend([OsString::from("-p"), OsString::from(commit)]);
        }
        // A clone need not have an identity configured, and a fixed one keeps
        // the wrapper the same for the same operation.
        let identity = [
            ("GIT_AUTHOR_NAME", "Mjolnir"),
            ("GIT_AUTHOR_EMAIL", "mjolnir@localhost"),
            ("GIT_AUTHOR_DATE", "@0 +0000"),
            ("GIT_COMMITTER_NAME", "Mjolnir"),
            ("GIT_COMMITTER_EMAIL", "mjolnir@localhost"),
            ("GIT_COMMITTER_DATE", "@0 +0000"),
        ];
        let wrapper = git_success(
            runner,
            repository,
            GitCommand {
                arguments,
                stdin: Vec::new(),
                env: identity
                    .into_iter()
                    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                    .collect(),
            },
            "record the Git operation's commits",
        )?;
        let wrapper = trim_output(&wrapper, "decode the Git operation commit id")?;
        let name = format!("{prefix}/objects");
        git_bytes(
            runner,
            repository,
            ["update-ref", &name, &wrapper],
            &[],
            "preserve the Git operation's objects for the checkpoint bundle",
        )?;
        refs.push(name);
        Ok(())
    }
}

fn unique_paths(entries: &[SavedUnmergedEntry]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        if paths.last() != Some(&entry.path) {
            paths.push(entry.path.clone());
        }
    }
    paths
}

/// Reads the operation `repository` is in the middle of, or `None` when it
/// has no unmerged entries and no operation in progress. `None` leaves the
/// snapshot exactly as it was before operations were captured.
pub(super) fn collect(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<Option<CollectedOperation>> {
    let listed = git_bytes(
        runner,
        repository,
        ["ls-files", "--unmerged", "-z"],
        &[],
        "inspect clone merge conflicts",
    )?;
    let mut unmerged = Vec::new();
    for path in unmerged_paths(&listed)? {
        let relative = mj_core::path_input::from_git_bytes(&path.path)?;
        validate_worktree_relative_path(&relative)?;
        for (index, stage) in path.stages.iter().enumerate() {
            if let Some(stage) = stage {
                unmerged.push(SavedUnmergedEntry {
                    path: relative.clone(),
                    stage: index as u8 + 1,
                    mode: stage.mode.clone(),
                    object: stage.object.clone(),
                });
            }
        }
    }
    let locations = git_paths(
        runner,
        repository,
        STATE_FILES.iter().chain(&STATE_DIRECTORIES).copied(),
    )?;
    let mut in_progress = false;
    for (name, location) in &locations {
        if IN_PROGRESS_MARKERS.contains(name) && entry_exists(location)? {
            in_progress = true;
        }
    }
    if unmerged.is_empty() && !in_progress {
        return Ok(None);
    }

    let paths = unique_paths(&unmerged);
    refuse_index_entries_below(runner, repository, &paths)?;
    let (conflicted_worktree_tar, missing_worktree_paths) =
        build_conflicted_worktree_tar(repository, &paths)?;
    let state = read_state(&locations)?;
    let state_tar = build_state_tar(&state)?;
    let named = named_objects(runner, repository, &unmerged, &state)?;
    let mut commits = Vec::new();
    let mut tags = Vec::new();
    let mut contents = Vec::new();
    for (object, kind) in &named {
        match kind.as_str() {
            "commit" => commits.push(object.clone()),
            "tag" => tags.push(object.clone()),
            "tree" | "blob" => contents.push((kind.clone(), object.clone())),
            other => bail!("Git operation names object {object} of unknown type {other}"),
        }
    }
    Ok(Some(CollectedOperation {
        metadata: SavedGitOperation {
            unmerged,
            missing_worktree_paths,
            objects: named.into_keys().collect(),
        },
        payloads: GitOperationPayloads {
            conflicted_worktree_tar,
            state_tar,
        },
        commits,
        tags,
        contents,
    }))
}

fn entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

/// Where Git keeps each named Git-directory entry in this checkout.
pub(super) fn git_paths<'a>(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    names: impl Iterator<Item = &'a str>,
) -> Result<Vec<(&'a str, PathBuf)>> {
    let names = names.collect::<Vec<_>>();
    let mut arguments = vec![
        OsString::from("rev-parse"),
        OsString::from("--path-format=absolute"),
    ];
    for name in &names {
        arguments.extend([OsString::from("--git-path"), OsString::from(name)]);
    }
    let output = git_success(
        runner,
        repository,
        GitCommand {
            arguments,
            stdin: Vec::new(),
            env: Vec::new(),
        },
        "locate Git directory entries",
    )?;
    let lines = output
        .strip_suffix(b"\n")
        .unwrap_or(&output)
        .split(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    ensure!(
        lines.len() == names.len(),
        "git rev-parse located {} of {} Git directory entries",
        lines.len(),
        names.len()
    );
    names
        .into_iter()
        .zip(lines)
        .map(|(name, line)| {
            let path = mj_core::path_input::from_git_bytes(line)?;
            ensure!(
                path.is_absolute(),
                "Git reported a relative location {} for {name}",
                path.display()
            );
            Ok((name, path))
        })
        .collect()
}

/// A patch leaves out an excluded path together with everything below it, so
/// an index entry below an unmerged path would silently fall out of the
/// staged patch. Git's merge backend never leaves one: it moves the file
/// aside instead, as `path~HEAD`.
fn refuse_index_entries_below(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    unmerged: &[PathBuf],
) -> Result<()> {
    let unmerged = unmerged
        .iter()
        .map(PathBuf::as_path)
        .collect::<BTreeSet<_>>();
    let listed = git_bytes(
        runner,
        repository,
        ["ls-files", "-z"],
        &[],
        "list the clone's index",
    )?;
    for path in listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = mj_core::path_input::from_git_bytes(path)?;
        if let Some(directory) = path
            .ancestors()
            .skip(1)
            .find(|ancestor| unmerged.contains(ancestor))
        {
            bail!(
                "cannot checkpoint unmerged path '{}': the index also holds '{}' below it",
                directory.display(),
                path.display()
            );
        }
    }
    Ok(())
}

/// One Git-directory entry of an operation's state.
struct StateEntry {
    /// The name `git rev-parse --git-path` resolves, with `/` between parts.
    name: String,
    metadata: fs::Metadata,
    /// The bytes of a file; `None` for a directory.
    contents: Option<Vec<u8>>,
}

impl StateEntry {
    fn is_named_by_todo_list(&self) -> bool {
        TODO_LISTS.contains(&self.name.as_str())
    }

    fn holds_object_names(&self) -> bool {
        self.name != RERERE_CACHE && !self.name.starts_with("rr-cache/")
    }
}

fn read_state(locations: &[(&str, PathBuf)]) -> Result<Vec<StateEntry>> {
    let mut entries = Vec::new();
    for (name, location) in locations {
        let metadata = match fs::symlink_metadata(location) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", location.display()));
            }
        };
        if STATE_DIRECTORIES.contains(name) {
            ensure!(
                metadata.is_dir(),
                "Git operation state {} is not a directory",
                location.display()
            );
            read_state_directory(name, location, metadata, &mut entries)?;
        } else {
            ensure!(
                metadata.is_file(),
                "Git operation state {} is not a file",
                location.display()
            );
            entries.push(StateEntry {
                name: (*name).to_owned(),
                contents: Some(
                    fs::read(location).with_context(|| format!("read {}", location.display()))?,
                ),
                metadata,
            });
        }
    }
    Ok(entries)
}

fn read_state_directory(
    name: &str,
    location: &Path,
    metadata: fs::Metadata,
    entries: &mut Vec<StateEntry>,
) -> Result<()> {
    entries.push(StateEntry {
        name: name.to_owned(),
        metadata,
        contents: None,
    });
    let mut children = fs::read_dir(location)
        .with_context(|| format!("list {}", location.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("list {}", location.display()))?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = child.path();
        let child_name = child.file_name();
        let child_name = format!(
            "{name}/{}",
            child_name
                .to_str()
                .with_context(|| format!("Git operation state {} is not UTF-8", path.display()))?
        );
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        if metadata.is_dir() {
            read_state_directory(&child_name, &path, metadata, entries)?;
        } else {
            ensure!(
                metadata.is_file(),
                "unsupported Git operation state entry {}",
                path.display()
            );
            entries.push(StateEntry {
                name: child_name,
                contents: Some(
                    fs::read(&path).with_context(|| format!("read {}", path.display()))?,
                ),
                metadata,
            });
        }
    }
    Ok(())
}

fn build_state_tar(entries: &[StateEntry]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut output);
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header.set_metadata_in_mode(&entry.metadata, tar::HeaderMode::Deterministic);
            let contents = entry.contents.as_deref().unwrap_or_default();
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, &entry.name, contents)
                .with_context(|| format!("archive Git operation state {}", entry.name))?;
        }
        builder.finish().context("finish Git operation state tar")?;
    }
    Ok(output)
}

/// Every object the unmerged entries and the state files name, keyed by id,
/// with its type. A state file names an object by its full id, except for
/// a todo list instruction, which may abbreviate the commit it names. A name
/// that is no object here, such as a conflict hash in `MERGE_RR`, is skipped.
/// A submodule conflict's stages name commits of the submodule, which this
/// repository does not hold.
fn named_objects(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    unmerged: &[SavedUnmergedEntry],
    state: &[StateEntry],
) -> Result<BTreeMap<String, String>> {
    // Each request, and whether only a commit may answer it.
    let mut requests = BTreeMap::<String, bool>::new();
    let mut request = |name: String, commit_only: bool| {
        let entry = requests.entry(name).or_insert(commit_only);
        *entry &= commit_only;
    };
    for entry in unmerged.iter().filter(|entry| entry.mode != "160000") {
        request(entry.object.clone(), false);
    }
    for entry in state {
        let Some(contents) = &entry.contents else {
            continue;
        };
        if entry.holds_object_names() {
            for name in full_object_ids(contents) {
                request(name, false);
            }
        }
        if entry.is_named_by_todo_list() {
            for name in todo_commits(contents) {
                request(name, true);
            }
        }
    }
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input = requests
        .keys()
        .map(|name| format!("{name}\n"))
        .collect::<String>();
    let output = git_bytes(
        runner,
        repository,
        ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        input.as_bytes(),
        "resolve the objects a Git operation names",
    )?;
    let output = std::str::from_utf8(&output).context("decode git cat-file --batch-check")?;
    let answers = output.lines().collect::<Vec<_>>();
    ensure!(
        answers.len() == requests.len(),
        "git cat-file answered {} of {} object names",
        answers.len(),
        requests.len()
    );
    let mut named = BTreeMap::new();
    for ((_, commit_only), answer) in requests.into_iter().zip(answers) {
        let (object, kind) = answer
            .split_once(' ')
            .with_context(|| format!("malformed git cat-file answer {answer:?}"))?;
        if matches!(kind, "missing" | "ambiguous") || (commit_only && kind != "commit") {
            continue;
        }
        ensure!(
            mj_core::remote_git::is_object_id(object),
            "git cat-file answered {answer:?}"
        );
        named.insert(object.to_owned(), kind.to_owned());
    }
    Ok(named)
}

/// Every full SHA-1 or SHA-256 id in `contents`, as a word on its own.
fn full_object_ids(contents: &[u8]) -> impl Iterator<Item = String> + '_ {
    contents
        .split(|byte| !byte.is_ascii_alphanumeric())
        .filter(|word| {
            matches!(word.len(), 40 | 64)
                && word
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
        .map(|word| String::from_utf8_lossy(word).into_owned())
}

/// The commits a todo list's instructions name, as written: `pick <commit>`
/// and its kin, `fixup -C <commit>`, `merge -C <commit> <label>` and
/// `reset <commit>`. Labels and comments are not commits.
fn todo_commits(contents: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(contents);
    let mut commits = Vec::new();
    for line in text.lines() {
        let mut words = line.split_ascii_whitespace();
        let Some(command) = words.next() else {
            continue;
        };
        let names_commit = matches!(
            command,
            "pick"
                | "p"
                | "reword"
                | "r"
                | "edit"
                | "e"
                | "squash"
                | "s"
                | "fixup"
                | "f"
                | "drop"
                | "d"
                | "revert"
                | "merge"
                | "m"
                | "reset"
                | "t"
        );
        if !names_commit {
            continue;
        }
        let mut argument = words.next();
        if matches!(argument, Some("-C" | "-c")) {
            argument = words.next();
        }
        if let Some(argument) = argument
            && (4..=64).contains(&argument.len())
            && argument.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            commits.push(argument.to_ascii_lowercase());
        }
    }
    commits
}

/// Fails unless every object the operation names is in the restored clone.
/// The bundle was imported just before, so a missing object means the
/// archive cannot give the operation back.
pub(super) fn verify_objects(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    operation: &SavedGitOperation,
) -> Result<()> {
    if operation.objects.is_empty() {
        return Ok(());
    }
    let input = operation
        .objects
        .iter()
        .map(|object| format!("{object}\n"))
        .collect::<String>();
    let output = git_bytes(
        runner,
        repository,
        ["cat-file", "--batch-check=%(objectname)"],
        input.as_bytes(),
        "verify the Git operation's objects",
    )?;
    let output = std::str::from_utf8(&output).context("decode git cat-file --batch-check")?;
    let answers = output.lines().collect::<Vec<_>>();
    ensure!(
        answers.len() == operation.objects.len(),
        "git cat-file answered {} of {} Git operation objects",
        answers.len(),
        operation.objects.len()
    );
    for (object, answer) in operation.objects.iter().zip(answers) {
        ensure!(
            answer.eq_ignore_ascii_case(object),
            "restore Git operation: object {object} is missing from the restored clone"
        );
    }
    Ok(())
}

/// Writes the operation back into a clone whose commits, index and working
/// tree are otherwise restored: the unmerged index entries, the files of the
/// unmerged paths, and the state files.
pub(super) fn restore(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    operation: &SavedGitOperation,
    payloads: &GitOperationPayloads,
) -> Result<()> {
    let paths = unique_paths(&operation.unmerged);
    if !paths.is_empty() {
        // A mode-0 record removes every stage of its path, which is how Git
        // asks for a path's stage-0 entry to give way to its conflict stages.
        let mut records = Vec::new();
        let mut started = BTreeSet::new();
        for entry in &operation.unmerged {
            let path = entry.path.as_os_str().as_encoded_bytes();
            if started.insert(entry.path.as_path()) {
                records.extend(format!("0 {}\t", "0".repeat(entry.object.len())).into_bytes());
                records.extend_from_slice(path);
                records.push(0);
            }
            records
                .extend(format!("{} {} {}\t", entry.mode, entry.object, entry.stage).into_bytes());
            records.extend_from_slice(path);
            records.push(0);
        }
        git_bytes(
            runner,
            repository,
            ["update-index", "-z", "--index-info"],
            &records,
            "restore the unmerged index entries",
        )?;
    }

    let unmerged = paths.iter().map(PathBuf::as_path).collect::<BTreeSet<_>>();
    let mut archive = tar::Archive::new(Cursor::new(&payloads.conflicted_worktree_tar));
    for entry in archive.entries().context("read conflicted-file tar")? {
        let entry = entry.context("read conflicted-file tar entry")?;
        let path = entry.path().context("read conflicted-file tar path")?;
        ensure!(
            unmerged.contains(path.as_ref()),
            "conflicted-file tar holds '{}', which is not unmerged",
            path.display()
        );
    }
    restore_worktree_tar(
        repository,
        &payloads.conflicted_worktree_tar,
        WorktreeTar::Conflicted,
    )?;
    for path in &operation.missing_worktree_paths {
        ensure!(
            unmerged.contains(path.as_path()),
            "missing working-tree path '{}' is not unmerged",
            path.display()
        );
        ensure_no_symlink_ancestors(repository, path)?;
        remove_worktree_file(&repository.join(path))?;
    }
    restore_state(runner, repository, &payloads.state_tar)
}

fn restore_state(runner: &dyn GitCommandRunner, repository: &Path, bytes: &[u8]) -> Result<()> {
    validate_operation_state_tar(bytes)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut entries = Vec::new();
    for entry in archive.entries().context("read Git operation state tar")? {
        let mut entry = entry.context("read Git operation state tar entry")?;
        let path = entry.path()?.into_owned();
        let directory = entry.header().entry_type().is_dir();
        let mode = entry.header().mode().unwrap_or(0o644);
        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .with_context(|| format!("read Git operation state {}", path.display()))?;
        entries.push((path, directory, mode, contents));
    }
    let names = entries
        .iter()
        .filter_map(|(path, ..)| top_level_state_name(path))
        .collect::<BTreeSet<_>>();
    let locations = git_paths(runner, repository, names.into_iter())?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    for (path, directory, mode, contents) in entries {
        let name = top_level_state_name(&path).context("Git operation state entry has no name")?;
        let mut destination = locations[name].clone();
        destination.extend(path.components().skip(1));
        if directory {
            fs::create_dir_all(&destination)
                .with_context(|| format!("create {}", destination.display()))?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
        {
            Ok(file) => file,
            // A recorded resolution the clone already has is as good as the
            // archived one, and the cache is shared by the repository's
            // worktrees.
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && name == RERERE_CACHE =>
            {
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("write Git operation state {}", destination.display())
                });
            }
        };
        file.write_all(&contents)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write Git operation state {}", destination.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o777))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    Ok(())
}

/// The state entry an archived path belongs to: its first component, when
/// that is one Git keeps.
fn top_level_state_name(path: &Path) -> Option<&'static str> {
    let Some(Component::Normal(first)) = path.components().next() else {
        return None;
    };
    STATE_FILES
        .iter()
        .chain(&STATE_DIRECTORIES)
        .copied()
        .find(|name| first == std::ffi::OsStr::new(name))
}

pub(in crate::archive) fn validate_operation_state_tar(bytes: &[u8]) -> Result<()> {
    validate_operation_state_tar_reader(Cursor::new(bytes))
}

/// Checks that every entry of an operation state tar is a Git state file or
/// lies inside a Git state directory, so a restore writes only those.
pub(in crate::archive) fn validate_operation_state_tar_reader(reader: impl Read) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("read Git operation state tar")? {
        let entry = entry.context("read Git operation state tar entry")?;
        let path = entry.path().context("read Git operation state tar path")?;
        validate_archive_relative_path(&path)?;
        let entry_type = entry.header().entry_type();
        let name = top_level_state_name(&path).with_context(|| {
            format!(
                "Git operation state tar holds '{}', which is not Git operation state",
                path.display()
            )
        })?;
        if STATE_DIRECTORIES.contains(&name) {
            ensure!(
                entry_type.is_file() || entry_type.is_dir(),
                "Git operation state '{}' is neither a file nor a directory",
                path.display()
            );
        } else {
            ensure!(
                entry_type.is_file() && path.components().count() == 1,
                "Git operation state '{}' is not a file",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
