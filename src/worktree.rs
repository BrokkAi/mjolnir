//! Git worktree support for `mj --worktree`.
//!
//! A worktree session clones the current project checkout into a linked
//! Git worktree below `<project>/.mjolnir/worktrees/` and points the ACP
//! session at the corresponding directory there. The directory should be
//! ignored so the nested worktree does not show up as untracked content.

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use crate::paths;

const WORKTREE_IGNORE_ENTRY: &str = ".mjolnir/worktrees/";

/// Adjectives for random worktree names (adjective-noun style, like
/// Docker container names). Kept short so the header label stays
/// compact.
const ADJECTIVES: &[&str] = &[
    "bold", "brave", "bright", "calm", "clear", "cozy", "crisp", "dark", "deep", "eager", "fair",
    "fast", "fine", "fresh", "glad", "happy", "jolly", "keen", "kind", "lucky", "merry", "neat",
    "noble", "odd", "pale", "proud", "quick", "quiet", "rare", "rich", "safe", "sharp", "silly",
    "slim", "soft", "swift", "tall", "thin", "vivid", "warm", "wild", "wise", "witty", "zany",
];

/// Nouns for random worktree names.
const NOUNS: &[&str] = &[
    "badger", "bear", "bird", "bloom", "brook", "cedar", "cloud", "coral", "crane", "dawn", "deer",
    "dove", "eagle", "ember", "falcon", "fern", "flame", "forge", "fox", "frost", "gem", "grove",
    "harbor", "hawk", "heron", "ivy", "jade", "lake", "lark", "leaf", "maple", "marsh", "moss",
    "oak", "orchid", "otter", "owl", "pine", "quartz", "raven", "reef", "ridge", "river", "robin",
    "sage", "storm", "thorn", "tide", "trout", "willow",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub project_root: PathBuf,
    pub worktree_root: PathBuf,
    pub session_cwd: PathBuf,
    /// True when the worktree was freshly created by this invocation
    /// (as opposed to being reused via `--worktree <name>`). Used by
    /// the exit prompt to decide whether to offer cleanup.
    pub was_created: bool,
}

/// Resolve the current Git project, ensure the Mjolnir worktree directory is
/// ignored when the user agrees, create a fresh linked worktree, and return the
/// directory that should be used as the ACP session cwd.
pub fn create_for_cwd_prompting(
    cwd: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<CreatedWorktree> {
    let project_root = git_toplevel(cwd)?;
    let created = create_for_project_cwd(&project_root, cwd)?;
    prompt_to_ignore_worktrees(&created.worktree_root, input, output)?;
    Ok(created)
}

fn create_for_project_cwd(project_root: &Path, cwd: &Path) -> Result<CreatedWorktree> {
    let relative_cwd = relative_cwd(project_root, cwd)?;
    let worktrees_dir = worktrees_dir(project_root);
    std::fs::create_dir_all(&worktrees_dir)
        .with_context(|| format!("create {}", worktrees_dir.display()))?;

    let worktree_root = unique_worktree_path(&worktrees_dir)?;
    run_git(
        project_root,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            worktree_root.as_os_str(),
            OsStr::new("HEAD"),
        ],
    )
    .with_context(|| format!("create git worktree {}", worktree_root.display()))?;

    let session_cwd = worktree_root.join(relative_cwd);
    std::fs::create_dir_all(&session_cwd)
        .with_context(|| format!("create session cwd {}", session_cwd.display()))?;

    Ok(CreatedWorktree {
        project_root: project_root.to_path_buf(),
        worktree_root,
        session_cwd,
        was_created: true,
    })
}

/// Open an existing worktree by name (short name under
/// `.mjolnir/worktrees/`) or by path (absolute or relative to `cwd`).
/// The target directory must already exist and be a registered Git
/// worktree.
pub fn open_existing_for_cwd_prompting(
    cwd: &Path,
    name_or_path: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<CreatedWorktree> {
    let project_root = git_toplevel(cwd)?;

    let wdir = worktrees_dir(&project_root);
    // Try short name first (most common), then treat as a cwd-relative
    // path, then as an absolute path.
    let worktree_root =
        if !name_or_path.contains(std::path::is_separator) && !name_or_path.starts_with('.') {
            let candidate = wdir.join(name_or_path);
            if candidate.is_dir() {
                candidate
            } else if Path::new(name_or_path).is_dir() {
                std::fs::canonicalize(name_or_path)
                    .with_context(|| format!("resolve path {}", name_or_path))?
            } else {
                bail!(
                    "worktree '{}' not found under {} and not a valid path",
                    name_or_path,
                    wdir.display()
                );
            }
        } else if Path::new(name_or_path).is_absolute() {
            PathBuf::from(name_or_path)
        } else {
            let resolved = cwd.join(name_or_path);
            if !resolved.is_dir() {
                bail!("worktree path does not exist: {}", resolved.display());
            }
            std::fs::canonicalize(&resolved)
                .with_context(|| format!("resolve path {}", resolved.display()))?
        };

    if !worktree_root.is_dir() {
        bail!("worktree path does not exist: {}", worktree_root.display());
    }

    // Verify it is actually a Git-linked worktree (has a .git file
    // pointing back to the parent repo).
    let git_file = worktree_root.join(".git");
    if !git_file.exists() {
        bail!(
            "directory {} does not look like a Git worktree (no .git file)",
            worktree_root.display()
        );
    }

    let relative_cwd = relative_cwd(&project_root, cwd)?;
    let session_cwd = worktree_root.join(relative_cwd);
    if !session_cwd.is_dir() {
        std::fs::create_dir_all(&session_cwd)
            .with_context(|| format!("create session cwd {}", session_cwd.display()))?;
    }

    prompt_to_ignore_worktrees(&worktree_root, input, output)?;

    Ok(CreatedWorktree {
        project_root,
        worktree_root,
        session_cwd,
        was_created: false,
    })
}

/// Ask the user whether to remove the worktree after the session ends.
/// Returns Ok(true) when the worktree was successfully removed.
pub fn prompt_remove_on_exit(
    worktree: &CreatedWorktree,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    let label = worktree
        .worktree_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| worktree.worktree_root.display().to_string());
    write!(output, "Remove worktree '{label}'? [y/N] ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if !is_yes(answer.trim()) {
        writeln!(
            output,
            "Keeping worktree: {}",
            worktree.worktree_root.display()
        )?;
        return Ok(false);
    }

    match remove_worktree(&worktree.project_root, &worktree.worktree_root) {
        Ok(()) => {
            writeln!(output, "Removed worktree '{label}'.")?;
            Ok(true)
        }
        Err(e) => {
            writeln!(
                output,
                "Failed to remove worktree '{label}': {e:#}\n\
                 Worktree kept at: {}",
                worktree.worktree_root.display()
            )?;
            Ok(false)
        }
    }
}

/// Remove a Git-linked worktree. Runs `git worktree remove` from the
/// project root; also cleans up the directory on disk if Git left it
/// behind (e.g. when the .git metadata is stale).
fn remove_worktree(project_root: &Path, worktree_root: &Path) -> Result<()> {
    run_git(
        project_root,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            worktree_root.as_os_str(),
        ],
    )
    .with_context(|| format!("git worktree remove {}", worktree_root.display()))?;

    // Belt-and-suspenders: if `git worktree remove --force` left the
    // directory behind (can happen with stale metadata), clean it up.
    if worktree_root.is_dir() {
        std::fs::remove_dir_all(worktree_root)
            .with_context(|| format!("remove worktree dir {}", worktree_root.display()))?;
    }

    // Prune stale worktree entries from the parent repo's metadata.
    let _ = run_git(project_root, [OsStr::new("worktree"), OsStr::new("prune")]);

    Ok(())
}

fn prompt_to_ignore_worktrees(
    checkout_root: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    if git_check_ignores_worktrees(checkout_root)? {
        return Ok(());
    }

    writeln!(
        output,
        "Mjolnir stores linked worktrees under {}.",
        WORKTREE_IGNORE_ENTRY
    )?;
    write!(output, "Add {WORKTREE_IGNORE_ENTRY} to .gitignore? [y/N] ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if is_yes(answer.trim()) {
        append_gitignore_entry(checkout_root)?;
        if !git_check_ignores_worktrees(checkout_root)? {
            bail!("added {WORKTREE_IGNORE_ENTRY} to .gitignore, but Git still does not ignore it");
        }
        writeln!(output, "Added {WORKTREE_IGNORE_ENTRY} to .gitignore.")?;
    } else {
        writeln!(
            output,
            "Leaving .gitignore unchanged; {} may appear as untracked content.",
            WORKTREE_IGNORE_ENTRY
        )?;
    }
    Ok(())
}

fn git_toplevel(cwd: &Path) -> Result<PathBuf> {
    paths::git_toplevel(cwd).map_err(|e| {
        anyhow!(
            "--worktree requires a Git worktree; git rev-parse failed in {}: {e:#}",
            cwd.display()
        )
    })
}

fn git_check_ignores_worktrees(project_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["check-ignore", "-q", "--no-index", "--"])
        .arg(WORKTREE_IGNORE_ENTRY)
        .output()
        .with_context(|| format!("run git check-ignore in {}", project_root.display()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git check-ignore failed in {}: {}",
            project_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn append_gitignore_entry(checkout_root: &Path) -> Result<()> {
    let path = checkout_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str("# Mjolnir linked worktrees\n");
    next.push_str(WORKTREE_IGNORE_ENTRY);
    next.push('\n');

    std::fs::write(&path, next).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
fn gitignore_text_contains_worktree_entry(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && matches!(
                line,
                ".mjolnir/"
                    | "/.mjolnir/"
                    | ".mjolnir"
                    | "/.mjolnir"
                    | ".mjolnir/worktrees/"
                    | "/.mjolnir/worktrees/"
                    | ".mjolnir/worktrees"
                    | "/.mjolnir/worktrees"
            )
    })
}

fn relative_cwd(project_root: &Path, cwd: &Path) -> Result<PathBuf> {
    let project_root = std::fs::canonicalize(project_root)
        .with_context(|| format!("canonicalize {}", project_root.display()))?;
    let cwd =
        std::fs::canonicalize(cwd).with_context(|| format!("canonicalize {}", cwd.display()))?;
    let relative = cwd.strip_prefix(&project_root).with_context(|| {
        format!(
            "cwd {} is not inside project {}",
            cwd.display(),
            project_root.display()
        )
    })?;
    Ok(relative.to_path_buf())
}

/// Pick a random adjective-noun combination for the worktree directory
/// name. Mixes timestamp and PID for extra entropy so rapid invocations
/// don't collide. Retries with a suffix on collision.
fn unique_worktree_path(worktrees_dir: &Path) -> Result<PathBuf> {
    let seed = mix_seed(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
    );

    for attempt in 0..100_u32 {
        let mixed = seed
            .wrapping_add(attempt as u64)
            .wrapping_mul(6364136223846793005);
        let adj = ADJECTIVES[(mixed as usize) % ADJECTIVES.len()];
        let noun = NOUNS[((mixed >> 16) as usize) % NOUNS.len()];
        let name = if attempt == 0 {
            format!("{adj}-{noun}")
        } else {
            format!("{adj}-{noun}-{attempt}")
        };
        let path = worktrees_dir.join(&name);
        if !path.exists() {
            return Ok(path);
        }
    }

    Err(anyhow!(
        "could not find an unused worktree name under {}",
        worktrees_dir.display()
    ))
}

/// Generate a random adjective-noun name (without collision checking).
/// Used for tests and potential future display needs.
#[cfg(test)]
fn random_worktree_name() -> String {
    let seed = mix_seed(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
    );
    let mixed = seed.wrapping_mul(6364136223846793005);
    let adj = ADJECTIVES[(mixed as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((mixed >> 16) as usize) % NOUNS.len()];
    format!("{adj}-{noun}")
}

/// Mix timestamp and PID into a u64 seed for random name generation.
/// Uses a simple hash mix to spread entropy across both values.
fn mix_seed(nanos: u128, pid: u32) -> u64 {
    let hi = (nanos >> 64) as u64;
    let lo = nanos as u64;
    lo.wrapping_add(hi)
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(pid as u64)
}

fn worktrees_dir(project_root: &Path) -> PathBuf {
    project_root.join(".mjolnir").join("worktrees")
}

fn run_git<'a>(project_root: &Path, args: impl IntoIterator<Item = &'a OsStr>) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", project_root.display()))?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "git failed in {}: {}",
        project_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes" | "yees")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn yes_prompt_accepts_common_answers_and_typo() {
        assert!(is_yes("y"));
        assert!(is_yes("yes"));
        assert!(is_yes("yees"));
        assert!(is_yes("YES"));
        assert!(!is_yes(""));
        assert!(!is_yes("no"));
    }

    #[test]
    fn gitignore_detection_recognizes_parent_and_worktree_entries() {
        assert!(gitignore_text_contains_worktree_entry(
            ".mjolnir/worktrees/\n"
        ));
        assert!(gitignore_text_contains_worktree_entry("/.mjolnir/\n"));
        assert!(gitignore_text_contains_worktree_entry(".mjolnir\n"));
        assert!(!gitignore_text_contains_worktree_entry(
            "# .mjolnir/worktrees/\n"
        ));
        assert!(!gitignore_text_contains_worktree_entry("target/\n"));
    }

    #[test]
    fn appending_gitignore_entry_preserves_existing_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gitignore = dir.path().join(".gitignore");
        std::fs::write(&gitignore, "target/\n").expect("write initial gitignore");

        append_gitignore_entry(dir.path()).expect("append entry");
        let text = std::fs::read_to_string(&gitignore).expect("read gitignore");

        assert!(text.contains("target/\n"));
        assert!(text.contains("# Mjolnir linked worktrees\n.mjolnir/worktrees/\n"));
    }

    #[test]
    fn prompt_appends_gitignore_when_user_says_yes() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        let mut input = Cursor::new(b"yees\n".to_vec());
        let mut output = Vec::new();

        prompt_to_ignore_worktrees(dir.path(), &mut input, &mut output).expect("prompt");

        let text = std::fs::read_to_string(dir.path().join(".gitignore")).expect("gitignore");
        assert!(text.contains(".mjolnir/worktrees/"));
        let output = String::from_utf8(output).expect("output utf8");
        assert!(output.contains("Added .mjolnir/worktrees/ to .gitignore."));
    }

    #[test]
    fn prompt_appends_final_ignore_after_existing_negation() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(
            dir.path().join(".gitignore"),
            ".mjolnir/worktrees/\n!.mjolnir/worktrees/\n",
        )
        .expect("write gitignore");
        assert!(!git_check_ignores_worktrees(dir.path()).expect("check initial ignore"));

        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut output = Vec::new();
        prompt_to_ignore_worktrees(dir.path(), &mut input, &mut output).expect("prompt");

        assert!(git_check_ignores_worktrees(dir.path()).expect("check final ignore"));
        let text = std::fs::read_to_string(dir.path().join(".gitignore")).expect("gitignore");
        assert!(text.ends_with("# Mjolnir linked worktrees\n.mjolnir/worktrees/\n"));
    }

    #[test]
    fn create_prompt_appends_gitignore_in_created_worktree_not_parent_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut output = Vec::new();
        let created =
            create_for_cwd_prompting(dir.path(), &mut input, &mut output).expect("create worktree");

        assert!(
            !dir.path().join(".gitignore").exists(),
            "parent checkout should not be dirtied by the ignore prompt"
        );
        let worktree_gitignore =
            std::fs::read_to_string(created.worktree_root.join(".gitignore")).expect("gitignore");
        assert!(worktree_gitignore.contains(".mjolnir/worktrees/"));
        assert!(git_check_ignores_worktrees(&created.worktree_root).expect("check worktree"));
        assert!(!git_check_ignores_worktrees(dir.path()).expect("check parent"));
    }

    #[test]
    fn create_worktree_preserves_relative_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::create_dir_all(dir.path().join("nested")).expect("create nested");
        std::fs::write(dir.path().join("nested/file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let created = create_for_project_cwd(dir.path(), &dir.path().join("nested"))
            .expect("create worktree");

        assert!(
            created
                .worktree_root
                .starts_with(dir.path().join(".mjolnir/worktrees"))
        );
        assert_eq!(created.session_cwd, created.worktree_root.join("nested"));
        assert!(created.session_cwd.join("file.txt").exists());
        assert!(created.was_created);
    }

    #[test]
    fn create_worktree_creates_untracked_relative_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new("tracked.txt")]).expect("git add");
        commit_all(dir.path());

        let untracked_dir = dir.path().join("scratch/empty");
        std::fs::create_dir_all(&untracked_dir).expect("create untracked dir");

        let created = create_for_project_cwd(dir.path(), &untracked_dir).expect("create worktree");

        assert_eq!(
            created.session_cwd,
            created.worktree_root.join("scratch/empty")
        );
        assert!(created.session_cwd.is_dir());
    }

    #[test]
    fn random_name_is_adjective_noun_format() {
        let name = random_worktree_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 2, "expected adjective-noun, got: {name}");
        assert!(
            ADJECTIVES.contains(&parts[0]),
            "first part '{}' not in ADJECTIVES",
            parts[0]
        );
        assert!(
            NOUNS.contains(&parts[1]),
            "second part '{}' not in NOUNS",
            parts[1]
        );
    }

    #[test]
    fn unique_worktree_path_produces_nonexistent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt_dir = dir.path().join(".mjolnir").join("worktrees");
        std::fs::create_dir_all(&wt_dir).expect("mkdir");

        let path = unique_worktree_path(&wt_dir).expect("unique path");
        assert!(path.starts_with(&wt_dir));
        assert!(!path.exists());

        // Create the first name so the next call is forced to retry.
        std::fs::create_dir_all(&path).expect("mkdir");
        let path2 = unique_worktree_path(&wt_dir).expect("unique path 2");
        assert!(path != path2, "second call should pick a different name");
    }

    #[test]
    fn open_existing_finds_worktree_by_short_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let created = create_for_project_cwd(dir.path(), dir.path()).expect("create worktree");
        let name = created
            .worktree_root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let reopened = open_existing_for_cwd_prompting(
            dir.path(),
            &name,
            &mut Cursor::new(b""),
            &mut Vec::new(),
        )
        .expect("open existing");

        assert_eq!(
            std::fs::canonicalize(&reopened.worktree_root).expect("canon"),
            std::fs::canonicalize(&created.worktree_root).expect("canon")
        );
        assert!(!reopened.was_created);
    }

    #[test]
    fn open_existing_rejects_nonexistent_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let result = open_existing_for_cwd_prompting(
            dir.path(),
            "no-such-worktree",
            &mut Cursor::new(b""),
            &mut Vec::new(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn prompt_remove_asks_and_removes_when_user_says_yes() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let created = create_for_project_cwd(dir.path(), dir.path()).expect("create worktree");
        assert!(created.worktree_root.is_dir());

        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut output = Vec::new();
        let removed = prompt_remove_on_exit(&created, &mut input, &mut output).expect("prompt");

        assert!(removed, "worktree should have been removed");
        assert!(!created.worktree_root.is_dir(), "directory should be gone");
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("Removed worktree"));
    }

    #[test]
    fn prompt_remove_keeps_worktree_when_user_says_no() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");
        run_git(dir.path(), [OsStr::new("add"), OsStr::new(".")]).expect("git add");
        commit_all(dir.path());

        let created = create_for_project_cwd(dir.path(), dir.path()).expect("create worktree");
        assert!(created.worktree_root.is_dir());

        let mut input = Cursor::new(b"no\n".to_vec());
        let mut output = Vec::new();
        let removed = prompt_remove_on_exit(&created, &mut input, &mut output).expect("prompt");

        assert!(!removed, "worktree should have been kept");
        assert!(
            created.worktree_root.is_dir(),
            "directory should still exist"
        );
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("Keeping worktree"));
    }

    fn init_git_repo(path: &Path) {
        let status = Command::new("git")
            .arg("init")
            .arg(path)
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed");
    }

    fn commit_all(path: &Path) {
        run_git(
            path,
            [
                OsStr::new("-c"),
                OsStr::new("user.name=Mjolnir Test"),
                OsStr::new("-c"),
                OsStr::new("user.email=mjolnir@example.invalid"),
                OsStr::new("-c"),
                OsStr::new("commit.gpgsign=false"),
                OsStr::new("commit"),
                OsStr::new("-am"),
                OsStr::new("initial"),
            ],
        )
        .expect("git commit");
    }
}
