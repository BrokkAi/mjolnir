//! Move-only, disk-backed workspace capture. Never builds checkpoint payloads.
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use mj_core::move_workspace::*;
use sha2::{Digest, Sha256};

const MANIFEST: &str = "move-workspace.json";

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = mj_core::subprocess::run_with_input(
        Command::new("git").arg("-C").arg(root).args(args),
        &[],
    )?;
    ensure!(
        output.status.success(),
        "git {} in {}: {}",
        args.join(" "),
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(root, args)?)?.trim_end().to_owned())
}

fn git_file(root: &Path, args: &[&str], destination: &Path) -> Result<()> {
    let status = mj_core::subprocess::run_to_file(
        Command::new("git").arg("-C").arg(root).args(args),
        File::create(destination)?,
    )?;
    ensure!(
        status.success(),
        "Move git {} failed in {} ({status})",
        args.join(" "),
        root.display()
    );
    Ok(())
}

fn paths(root: &Path, ignored: bool) -> Result<Vec<PathBuf>> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if ignored {
        args.push("--ignored");
    }
    git(root, &args)?
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(mj_core::path_input::from_git_bytes)
        .collect()
}

fn repository_root(repository: &WorkspaceRepository) -> Result<PathBuf> {
    mj_checkpoint::archive::validate_component(&repository.id, "Move repository")?;
    Ok(PathBuf::from(git_text(
        &repository.root,
        &["rev-parse", "--show-toplevel"],
    )?))
}

pub fn inspect(repositories: &[WorkspaceRepository]) -> Result<WorkspaceAssessment> {
    let mut assessment = WorkspaceAssessment::default();
    for repository in repositories {
        let root = repository_root(repository)?;
        ensure!(
            git(&root, &["ls-files", "--unmerged", "-z"])?.is_empty(),
            "resolve merge conflicts in {} before Move",
            repository.id
        );
        ensure!(
            git_text(&root, &["rev-parse", "--shared-index-path"])?.is_empty(),
            "{} uses a split Git index; run git update-index --no-split-index there before Move",
            repository.id
        );
        ensure!(
            git(&root, &["ls-files", "-v", "-z"])?
                .split(|byte| *byte == 0)
                .filter_map(|entry| entry.first())
                .all(|flag| *flag != b'S' && !flag.is_ascii_lowercase()),
            "{} has skip-worktree or assume-unchanged files; clear those flags before Move so all tracked edits can be verified",
            repository.id
        );
        // Include alternate object stores and staged blobs, which can be much
        // larger than this checkout's own objects directory.
        let history_bytes: u64 = git_text(
            &root,
            &["rev-list", "--disk-usage", "--objects", "--all", "--reflog"],
        )?
        .parse()
        .context("read Move Git history size")?;
        let mut checkout_bytes = 0u64;
        for line in git(&root, &["ls-tree", "-r", "-l", "-z", "HEAD"])?
            .split(|byte| *byte == 0)
            .filter(|line| !line.is_empty())
        {
            let header = line
                .split(|byte| *byte == b'\t')
                .next()
                .context("invalid Git tree entry")?;
            let size = std::str::from_utf8(header)?
                .split_whitespace()
                .nth(3)
                .context("Git tree size missing")?;
            if size != "-" {
                checkout_bytes = checkout_bytes.saturating_add(size.parse::<u64>()?);
            }
        }
        let mut edits_bytes = 0u64;
        for path in git(&root, &["diff", "--name-only", "-z", "HEAD"])?
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = root.join(mj_core::path_input::from_git_bytes(path)?);
            match fs::symlink_metadata(path) {
                Ok(metadata) => edits_bytes = edits_bytes.saturating_add(metadata.len()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        // Binary patch encoding and object repacking need overhead; this is
        // an estimate shown before the source is sealed, not a checkpoint limit.
        assessment.required_bytes = assessment
            .required_bytes
            .saturating_add(checkout_bytes)
            .saturating_add(history_bytes.saturating_mul(2))
            .saturating_add(edits_bytes.saturating_mul(2));
        for path in paths(&root, false)? {
            let location = WorkspacePath {
                repository: repository.id.clone(),
                path,
            };
            location.validate()?;
            if mj_checkpoint::archive::is_secret_like_path(&location.path) {
                assessment.credential_files += 1;
                continue;
            }
            let metadata = fs::symlink_metadata(root.join(&location.path))?;
            ensure!(
                metadata.is_file() || metadata.is_symlink(),
                "unsupported Move file {}",
                location.path.display()
            );
            if metadata.is_symlink() {
                mj_checkpoint::archive::validate_symlink_target(
                    &location.path,
                    &fs::read_link(root.join(&location.path))?,
                )?;
            }
            assessment.files.push(WorkspaceFile {
                location,
                bytes: metadata.len(),
            });
        }
        for path in paths(&root, true)? {
            let metadata = fs::symlink_metadata(root.join(path))?;
            assessment.ignored_files += 1;
            assessment.ignored_bytes = assessment.ignored_bytes.saturating_add(metadata.len());
        }
    }
    assessment.files.sort_by(|a, b| a.location.cmp(&b.location));
    assessment.roots = file_tree(&assessment);
    assessment.initially_expanded = initial_expansion(&assessment.roots);
    Ok(assessment)
}

fn digest(path: &Path) -> Result<(u64, String)> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut size = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((size, mj_core::hex::lower_hex(hash.finalize())))
}

fn add_file(manifest: &mut WorkspaceManifest, root: &Path, relative: PathBuf) -> Result<()> {
    let (bytes, sha256) = digest(&root.join(&relative))?;
    manifest.files.push(TransferFile {
        path: relative,
        bytes,
        sha256,
    });
    Ok(())
}

pub fn capture(
    repositories: &[WorkspaceRepository],
    selection: &WorkspaceSelection,
    destination: &Path,
) -> Result<WorkspaceManifest> {
    let assessment = inspect(repositories)?;
    selection.validate(&assessment)?;
    fs::create_dir_all(destination.parent().context("Move stage has no parent")?)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(destination.with_extension("move-lock"))?;
    lock.try_lock()
        .context("another process owns this Move capture; retry after it finishes")?;
    if destination.join(MANIFEST).is_file() {
        return verify(destination);
    }
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::create_dir_all(destination)?;
    let mut manifest = WorkspaceManifest {
        version: 1,
        repositories: Vec::new(),
        files: Vec::new(),
    };
    for repository in repositories {
        let root = repository_root(repository)?;
        let relative = PathBuf::from(&repository.id);
        let stage = destination.join(&relative);
        fs::create_dir(&stage)?;
        let head = git_text(&root, &["rev-parse", "HEAD"])?;
        let branch = git_text(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let branch = (branch != "HEAD").then_some(branch);
        let status = git_text(&root, &["status", "--porcelain=v1", "--untracked-files=no"])?;
        let refs = git_text(
            &root,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )?
        .lines()
        .map(|line| {
            let (name, hash) = line.split_once(' ').context("invalid Git ref")?;
            Ok((name.to_owned(), hash.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
        let symbolic_refs = git_text(&root, &["for-each-ref", "--format=%(refname) %(symref)"])?
            .lines()
            .filter_map(|line| {
                line.split_once(' ')
                    .filter(|(_, target)| !target.is_empty())
            })
            .map(|(name, target)| (name.to_owned(), target.to_owned()))
            .collect::<Vec<_>>();
        let stash_log_path = PathBuf::from(git_text(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "logs/refs/stash",
            ],
        )?);
        let stash_log = match fs::read_to_string(stash_log_path) {
            Ok(log) => Some(log),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let bundle = stage.join("history.bundle");
        let mut bundle_args = vec![
            "bundle",
            "create",
            bundle.to_str().context("non-UTF8 Move stage")?,
            "--all",
            "HEAD",
        ];
        let stash_hashes: Vec<_> = stash_log
            .as_deref()
            .unwrap_or("")
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        bundle_args.extend(stash_hashes);
        git_file(&root, &bundle_args, &stage.join("git-output"))?;
        fs::remove_file(stage.join("git-output"))?;
        git_file(
            &root,
            &["diff", "--binary", "--full-index", "--cached", "HEAD"],
            &stage.join("staged.patch"),
        )?;
        git_file(
            &root,
            &["diff", "--binary", "--full-index"],
            &stage.join("unstaged.patch"),
        )?;
        let index = PathBuf::from(git_text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?);
        fs::copy(index, stage.join("index"))?;
        for name in ["history.bundle", "staged.patch", "unstaged.patch", "index"] {
            add_file(&mut manifest, destination, relative.join(name))?;
        }
        let mut untracked = Vec::new();
        for file in assessment
            .files
            .iter()
            .filter(|f| f.location.repository == repository.id && selection.includes(&f.location))
        {
            let input = root.join(&file.location.path);
            let before = fs::symlink_metadata(&input)?;
            let tar_relative = relative
                .join("untracked")
                .join(format!("{:08}.tar", untracked.len()));
            let tar_path = destination.join(&tar_relative);
            fs::create_dir_all(tar_path.parent().unwrap())?;
            // Tar carries symlinks and permission bits with bounded memory.
            // Each file gets its own tar so retries transfer independent files.
            let mut archive = tar::Builder::new(File::create(&tar_path)?);
            archive.follow_symlinks(false);
            archive.append_path_with_name(&input, &file.location.path)?;
            archive.finish()?;
            let after = fs::symlink_metadata(&input)?;
            ensure!(
                before.len() == after.len() && before.modified()? == after.modified()?,
                "{} changed during Move capture; source retained",
                input.display()
            );
            add_file(&mut manifest, destination, tar_relative)?;
            untracked.push(file.location.clone());
        }
        for (name, args) in [
            (
                "staged.patch",
                vec!["diff", "--binary", "--full-index", "--cached", "HEAD"],
            ),
            ("unstaged.patch", vec!["diff", "--binary", "--full-index"]),
        ] {
            let verification = stage.join("verify.patch");
            git_file(&root, &args, &verification)?;
            ensure!(
                digest(&verification)? == digest(&stage.join(name))?,
                "tracked edits changed during Move capture; source retained"
            );
            fs::remove_file(verification)?;
        }
        ensure!(
            head == git_text(&root, &["rev-parse", "HEAD"])?
                && status
                    == git_text(&root, &["status", "--porcelain=v1", "--untracked-files=no"])?,
            "repository {} changed during Move capture; source retained",
            repository.id
        );
        manifest.repositories.push(TransferredRepository {
            id: repository.id.clone(),
            head,
            branch,
            refs,
            symbolic_refs,
            stash_log,
            status,
            untracked,
        });
    }
    let temporary_manifest = destination.join("manifest.pending");
    let mut file = File::create(&temporary_manifest)?;
    serde_json::to_writer(&mut file, &manifest)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temporary_manifest, destination.join(MANIFEST))?;
    Ok(manifest)
}

pub fn verify(source: &Path) -> Result<WorkspaceManifest> {
    let manifest: WorkspaceManifest = serde_json::from_reader(File::open(source.join(MANIFEST))?)?;
    ensure!(
        manifest.version == 1,
        "unsupported Move workspace version {}",
        manifest.version
    );
    let mut expected = std::collections::BTreeSet::new();
    let mut repository_ids = std::collections::BTreeSet::new();
    for repository in &manifest.repositories {
        ensure!(
            repository_ids.insert(&repository.id),
            "duplicate Move repository"
        );
        for name in ["history.bundle", "staged.patch", "unstaged.patch", "index"] {
            expected.insert(PathBuf::from(&repository.id).join(name));
        }
        for index in 0..repository.untracked.len() {
            expected.insert(
                PathBuf::from(&repository.id)
                    .join("untracked")
                    .join(format!("{index:08}.tar")),
            );
        }
        mj_checkpoint::archive::validate_component(&repository.id, "Move repository")?;
        let valid_oid = |value: &str| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
        };
        ensure!(
            valid_oid(&repository.head)
                && repository
                    .refs
                    .iter()
                    .all(|(name, hash)| name.starts_with("refs/") && valid_oid(hash)),
            "invalid Move Git identities"
        );
        let mut seen = std::collections::BTreeSet::new();
        for path in &repository.untracked {
            ensure!(
                path.repository == repository.id && seen.insert(&path.path),
                "invalid Move untracked inventory"
            );
            path.validate()?;
        }
    }
    for file in &manifest.files {
        ensure!(
            expected.remove(&file.path),
            "unexpected or duplicate Move payload"
        );
        let mut prefix = source.to_path_buf();
        for component in file.path.components() {
            prefix.push(component);
            ensure!(
                !fs::symlink_metadata(&prefix)?.is_symlink(),
                "Move payload traverses a symlink"
            );
        }
        WorkspacePath {
            repository: "transfer".into(),
            path: file.path.clone(),
        }
        .validate()?;
        let (bytes, hash) = digest(&source.join(&file.path))?;
        ensure!(
            bytes == file.bytes && hash == file.sha256,
            "Move transfer verification failed for {}",
            file.path.display()
        );
    }
    ensure!(
        expected.is_empty(),
        "Move manifest is missing required payloads"
    );
    Ok(manifest)
}

pub fn restore(source: &Path, repositories: &[WorkspaceRepository]) -> Result<()> {
    let manifest = verify(source)?;
    for repository in &manifest.repositories {
        let destination = repositories
            .iter()
            .find(|r| r.id == repository.id)
            .context("Move destination repository missing")?;
        let root_path = repository_root(destination)?;
        let root = &root_path;
        let stage = source.join(&repository.id);
        ensure!(
            root.is_dir(),
            "Move destination {} is missing",
            root.display()
        );
        let bundle = stage.join("history.bundle");
        git(
            root,
            &[
                "fetch",
                "--no-tags",
                bundle.to_str().context("non-UTF8 Move bundle")?,
                &repository.head,
            ],
        )?;
        git(root, &["checkout", "--detach", "--force", &repository.head])?;
        for line in git_text(root, &["for-each-ref", "--format=%(refname)"])?.lines() {
            if !repository.refs.iter().any(|(name, _)| name == line) {
                git(root, &["update-ref", "--no-deref", "-d", line])?;
            }
        }
        for (name, hash) in &repository.refs {
            // Fetch the complete bundle before installing saved refs.
            git(
                root,
                &["fetch", "--no-tags", bundle.to_str().unwrap(), hash],
            )?;
            git(root, &["update-ref", "--no-deref", name, hash])?;
        }
        for (name, target) in &repository.symbolic_refs {
            ensure!(
                name.starts_with("refs/") && target.starts_with("refs/"),
                "invalid symbolic Move ref"
            );
            git(root, &["symbolic-ref", name, target])?;
        }
        if let Some(branch) = &repository.branch {
            git(root, &["checkout", "--no-guess", branch])?;
        }
        for (name, index) in [("staged.patch", true), ("unstaged.patch", false)] {
            let path = stage.join(name);
            if fs::metadata(&path)?.len() != 0 {
                let mut args = vec!["apply", "--binary", "--whitespace=nowarn"];
                if index {
                    args.push("--index");
                }
                args.push(path.to_str().context("non-UTF8 Move patch")?);
                git(root, &args)?;
            }
        }
        let index = PathBuf::from(git_text(
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?);
        fs::copy(stage.join("index"), index)?;
        if let Some(log) = &repository.stash_log {
            let path = PathBuf::from(git_text(
                root,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "logs/refs/stash",
                ],
            )?);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, log)?;
        }
        for file in manifest.files.iter().filter(|f| {
            f.path
                .starts_with(Path::new(&repository.id).join("untracked"))
        }) {
            let mut archive = tar::Archive::new(File::open(source.join(&file.path))?);
            let mut count = 0;
            for entry in archive.entries()? {
                count += 1;
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                ensure!(
                    repository.untracked.iter().any(|p| p.path == path),
                    "unlisted Move file {}",
                    path.display()
                );
                if entry.header().entry_type().is_symlink() {
                    let target = entry.link_name()?.context("Move symlink target missing")?;
                    mj_checkpoint::archive::validate_symlink_target(&path, &target)?;
                }
                ensure!(
                    entry.header().entry_type().is_file()
                        || entry.header().entry_type().is_gnu_sparse()
                        || entry.header().entry_type().is_symlink(),
                    "unsupported Move archive entry"
                );
                let mut parent = root.to_path_buf();
                if let Some(ancestors) = path.parent() {
                    for component in ancestors.components() {
                        parent.push(component);
                        match fs::symlink_metadata(&parent) {
                            Ok(metadata) => ensure!(
                                metadata.is_dir() && !metadata.is_symlink(),
                                "Move destination parent is not a directory"
                            ),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                ensure!(
                    entry.unpack_in(root)?,
                    "unsafe Move file path {}",
                    path.display()
                );
            }
            ensure!(count == 1, "Move file payload must have exactly one entry");
        }
        ensure!(
            git_text(root, &["rev-parse", "HEAD"])? == repository.head,
            "Move HEAD verification failed"
        );
        ensure!(
            git_text(root, &["status", "--porcelain=v1", "--untracked-files=no"])?
                == repository.status,
            "Move index/worktree verification failed"
        );
    }
    Ok(())
}

pub fn execute(command: WorkspaceCommand) -> Result<serde_json::Value> {
    match command {
        WorkspaceCommand::Inspect { repositories } => {
            Ok(serde_json::to_value(inspect(&repositories)?)?)
        }
        WorkspaceCommand::Capture {
            repositories,
            selection,
            destination,
        } => Ok(serde_json::to_value(capture(
            &repositories,
            &selection,
            &destination,
        )?)?),
        WorkspaceCommand::Restore {
            source,
            repositories,
        } => {
            restore(&source, &repositories)?;
            Ok(serde_json::Value::Null)
        }
        WorkspaceCommand::Verify { source } => Ok(serde_json::to_value(verify(&source)?)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "-b", "main"]).unwrap();
        git(root, &["config", "user.email", "move@example.invalid"]).unwrap();
        git(root, &["config", "user.name", "Move test"]).unwrap();
        fs::write(root.join(".git/empty-ignore"), "").unwrap();
        git(
            root,
            &[
                "config",
                "core.excludesFile",
                root.join(".git/empty-ignore").to_str().unwrap(),
            ],
        )
        .unwrap();
        fs::write(root.join("tracked"), "base\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        git(root, &["add", "."]).unwrap();
        git(root, &["commit", "-m", "base"]).unwrap();
    }

    #[test]
    #[ignore = "acceptance test writes and verifies more than 8 GiB of temporary workspace data"]
    fn move_transfers_a_file_larger_than_the_checkpoint_payload_limit() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        init(&source);
        git(
            directory.path(),
            &["clone", source.to_str().unwrap(), target.to_str().unwrap()],
        )
        .unwrap();
        let bytes = 8 * 1024 * 1024 * 1024 + 17;
        {
            use std::io::Write;
            let mut file = File::create(source.join("large-build-output")).unwrap();
            let block = vec![0x5a; 1024 * 1024];
            let mut remaining = bytes;
            while remaining > 0 {
                let count = remaining.min(block.len() as u64) as usize;
                file.write_all(&block[..count]).unwrap();
                remaining -= count as u64;
            }
        }
        let stage = directory.path().join("move-stage");
        let manifest = capture(
            &[WorkspaceRepository {
                id: "repo".into(),
                root: source.clone(),
            }],
            &WorkspaceSelection {
                acknowledge_large_transfer: true,
                ..Default::default()
            },
            &stage,
        )
        .unwrap();
        assert!(manifest.files.iter().any(|file| file.bytes > bytes));
        restore(
            &stage,
            &[WorkspaceRepository {
                id: "repo".into(),
                root: target.clone(),
            }],
        )
        .unwrap();
        assert_eq!(
            fs::metadata(target.join("large-build-output"))
                .unwrap()
                .len(),
            bytes
        );
        assert_eq!(
            digest(&source.join("large-build-output")).unwrap(),
            digest(&target.join("large-build-output")).unwrap()
        );
    }

    #[test]
    fn move_preflight_reports_index_settings_that_hide_tracked_edits() {
        let directory = tempfile::tempdir().unwrap();
        init(directory.path());
        let repositories = [WorkspaceRepository {
            id: "repo".into(),
            root: directory.path().into(),
        }];
        git(
            directory.path(),
            &["update-index", "--assume-unchanged", "tracked"],
        )
        .unwrap();
        fs::write(directory.path().join("tracked"), "hidden edit").unwrap();
        assert!(
            inspect(&repositories)
                .unwrap_err()
                .to_string()
                .contains("assume-unchanged")
        );
        git(
            directory.path(),
            &["update-index", "--no-assume-unchanged", "tracked"],
        )
        .unwrap();
        git(directory.path(), &["update-index", "--split-index"]).unwrap();
        assert!(
            inspect(&repositories)
                .unwrap_err()
                .to_string()
                .contains("--no-split-index")
        );
        git(directory.path(), &["update-index", "--no-split-index"]).unwrap();
        assert!(inspect(&repositories).is_ok());
    }

    #[test]
    fn disk_backed_move_preserves_git_state_and_selected_large_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        init(&source);
        git(
            temp.path(),
            &["clone", source.to_str().unwrap(), target.to_str().unwrap()],
        )
        .unwrap();
        fs::write(source.join("committed-new"), "new commit\n").unwrap();
        git(&source, &["add", "."]).unwrap();
        git(&source, &["commit", "-m", "new head"]).unwrap();
        fs::write(source.join("tracked"), "stashed\n").unwrap();
        git(&source, &["stash", "push", "-m", "saved work"]).unwrap();
        fs::write(source.join("tracked"), "second stash\n").unwrap();
        git(&source, &["stash", "push", "-m", "another saved change"]).unwrap();
        git(
            &source,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/heads/main",
            ],
        )
        .unwrap();
        fs::write(source.join("tracked"), "staged\n").unwrap();
        git(&source, &["add", "tracked"]).unwrap();
        fs::write(source.join("tracked"), "unstaged\n").unwrap();
        let body = vec![b'x'; 512 * 1024 + 17];
        fs::write(source.join("large"), &body).unwrap();
        File::create(source.join("sparse"))
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();
        fs::write(source.join("excluded"), "keep at source").unwrap();
        fs::write(source.join("ignored"), "ignored").unwrap();
        fs::write(source.join(".env"), "private").unwrap();
        let repos = vec![WorkspaceRepository {
            id: "repo".into(),
            root: source.clone(),
        }];
        let assessment = inspect(&repos).unwrap();
        assert_eq!(assessment.ignored_files, 1);
        assert_eq!(assessment.credential_files, 1);
        let selection = WorkspaceSelection {
            exclusions: vec![WorkspacePath {
                repository: "repo".into(),
                path: "excluded".into(),
            }],
            ..Default::default()
        };
        let stage = temp.path().join("stage");
        capture(&repos, &selection, &stage).unwrap();
        restore(
            &stage,
            &[WorkspaceRepository {
                id: "repo".into(),
                root: target.clone(),
            }],
        )
        .unwrap();
        for args in [
            vec!["rev-parse", "HEAD"],
            vec!["status", "--porcelain=v1", "--untracked-files=no"],
            vec!["ls-files", "--stage"],
            vec!["stash", "list"],
            vec![
                "for-each-ref",
                "--format=%(refname) %(objectname) %(symref)",
            ],
            vec!["show", "stash@{1}:tracked"],
            vec!["diff", "--binary"],
            vec!["diff", "--cached", "--binary"],
        ] {
            assert_eq!(
                git(&source, &args).unwrap(),
                git(&target, &args).unwrap(),
                "{args:?}"
            );
        }
        assert_eq!(fs::read(target.join("large")).unwrap(), body);
        assert_eq!(
            digest(&source.join("sparse")).unwrap(),
            digest(&target.join("sparse")).unwrap()
        );
        assert!(!target.join("excluded").exists());
        assert!(source.join("excluded").exists());
        assert!(!target.join("ignored").exists());
        assert!(!target.join(".env").exists());
        fs::write(stage.join("repo/staged.patch"), "corrupt").unwrap();
        assert!(
            verify(&stage)
                .unwrap_err()
                .to_string()
                .contains("staged.patch")
        );
    }
}
