//! Git worktree support for `mj --worktree`.
//!
//! A worktree session clones the current project checkout into a linked
//! Git worktree below `<project>/.mjolnir/worktrees/` and points the ACP
//! session at the corresponding directory there. The directory should be
//! ignored by the main checkout so the nested worktree does not show up
//! as untracked content.

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

const WORKTREE_IGNORE_ENTRY: &str = ".mjolnir/worktrees/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub project_root: PathBuf,
    pub worktree_root: PathBuf,
    pub session_cwd: PathBuf,
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
    prompt_to_ignore_worktrees(&project_root, input, output)?;
    create_for_project_cwd(&project_root, cwd)
}

fn create_for_project_cwd(project_root: &Path, cwd: &Path) -> Result<CreatedWorktree> {
    let relative_cwd = relative_cwd(project_root, cwd)?;
    let worktrees_dir = worktrees_dir(project_root);
    std::fs::create_dir_all(&worktrees_dir)
        .with_context(|| format!("create {}", worktrees_dir.display()))?;

    let worktree_root = unique_worktree_path(project_root, &worktrees_dir)?;
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
    })
}

fn prompt_to_ignore_worktrees(
    project_root: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    if git_check_ignores_worktrees(project_root)? {
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
        append_gitignore_entry(project_root)?;
        if !git_check_ignores_worktrees(project_root)? {
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
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("run git rev-parse in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "--worktree requires a Git worktree; git rev-parse failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("git rev-parse output was not UTF-8")?;
    let root = stdout.trim_end_matches(['\r', '\n']);
    if root.is_empty() {
        bail!("git rev-parse returned an empty project root");
    }
    Ok(PathBuf::from(root))
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

fn append_gitignore_entry(project_root: &Path) -> Result<()> {
    let path = project_root.join(".gitignore");
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

fn unique_worktree_path(project_root: &Path, worktrees_dir: &Path) -> Result<PathBuf> {
    let branch = current_branch_slug(project_root).unwrap_or_else(|| "detached".to_string());
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();

    for attempt in 0..100_u32 {
        let suffix = if attempt == 0 {
            format!("{millis}-{pid}")
        } else {
            format!("{millis}-{pid}-{attempt}")
        };
        let path = worktrees_dir.join(format!("{branch}-{suffix}"));
        if !path.exists() {
            return Ok(path);
        }
    }

    Err(anyhow!(
        "could not find an unused worktree path under {}",
        worktrees_dir.display()
    ))
}

fn current_branch_slug(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let slug = sanitize_path_component(branch.trim());
    (!slug.is_empty()).then_some(slug)
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches('-').chars().take(64).collect()
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
                OsStr::new("commit"),
                OsStr::new("-am"),
                OsStr::new("initial"),
            ],
        )
        .expect("git commit");
    }
}
