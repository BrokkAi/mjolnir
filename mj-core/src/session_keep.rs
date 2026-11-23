//! Per-session scratch directory `.mj/keep/<session-id>/`.
//!
//! The directory is created without following a symlink at `.mj`, `.mj/keep`,
//! or the session directory, the same refusal the report-root cleanup uses for
//! `.mj` and `.mj/agents` (`mj-controller` `REMOVE_REPORT_DIRS_SCRIPT`).
//! Ancestors above `.mj` may be symlinks. `/.mj/keep/` is listed in
//! `.git/info/exclude` with the same append rules as `/.mj/agents/`
//! (`PREPARE_REPORT_DIR_SCRIPT`): one exact line, a newline before it when the
//! file does not already end in one, and no write when the checkout is not a
//! Git repository.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::worker_launch::SESSION_KEEP_DIR_ENV;

/// Staged into every harness's instruction file.
pub const SESSION_STATE_INSTRUCTIONS: &str = include_str!("../assets/session-state.md");

/// Git exclude line for the keep directory. Anchored at the work tree root,
/// matching `/.mj/agents/`.
pub const KEEP_EXCLUDE_ENTRY: &str = "/.mj/keep/";

const KEEP_PARENT: &str = ".mj";
const KEEP_NAME: &str = "keep";

/// Whether `session_id` is a single directory name, so joining it under
/// `.mj/keep/` cannot leave the checkout or name another session's directory.
pub fn is_session_directory_name(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn validate_session_directory_name(session_id: &str) -> Result<()> {
    ensure!(
        is_session_directory_name(session_id),
        "session id {session_id:?} is not a single directory name"
    );
    Ok(())
}

/// `<checkout>/.mj/keep` when `session_id` is absent, otherwise
/// `<checkout>/.mj/keep/<session-id>`.
pub fn snapshot_keep_dir(checkout: &Path, session_id: Option<&str>) -> Result<PathBuf> {
    let mut path = checkout.join(KEEP_PARENT).join(KEEP_NAME);
    if let Some(session_id) = session_id {
        validate_session_directory_name(session_id)?;
        path.push(session_id);
    }
    Ok(path)
}

/// The source directory and path exclusions for a cross-target Move.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MoveKeep {
    directory: PathBuf,
    skipped: BTreeSet<PathBuf>,
}

impl MoveKeep {
    pub fn new(directory: PathBuf, skip: &[PathBuf]) -> Result<Self> {
        Ok(Self {
            directory,
            skipped: skip
                .iter()
                .map(|path| normalize_keep_path(path))
                .collect::<Result<_>>()?,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn skipped_paths(&self) -> &BTreeSet<PathBuf> {
        &self.skipped
    }

    /// Match a path under the checkout-wide keep directory against the one
    /// session directory the transport sends.
    pub fn relative_path<'a>(&self, keep_root: &Path, path: &'a Path) -> Option<&'a Path> {
        let prefix = self.directory.strip_prefix(keep_root).ok()?;
        path.strip_prefix(prefix).ok()
    }

    pub fn carries(&self, keep_root: &Path, path: &Path) -> bool {
        self.relative_path(keep_root, path)
            .is_some_and(|relative| !self.skips(relative))
    }

    pub fn skips(&self, relative: &Path) -> bool {
        self.skipped.iter().any(|skip| relative.starts_with(skip))
    }
}

pub fn normalize_keep_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("invalid relative path in keep stream: {}", path.display());
            }
        }
    }
    ensure!(
        !out.as_os_str().is_empty(),
        "empty relative path in keep stream"
    );
    Ok(out)
}

/// Create the session keep directory and export [`SESSION_KEEP_DIR_ENV`].
///
/// `cwd` may be anywhere inside the checkout. The directory is created at the
/// Git top level when `cwd` is in a repository, and at `cwd` otherwise.
/// A read-only checkout returns the filesystem error; the caller warns and
/// continues without the variable.
pub fn install_session_keep_dir(
    cwd: &Path,
    session_id: &str,
    environment: &mut BTreeMap<String, String>,
) -> Result<PathBuf> {
    validate_session_directory_name(session_id)?;
    let (checkout, repository) = checkout_top_level(cwd)?;
    let path = create_keep_directory(&checkout, session_id)?;
    if repository {
        append_git_info_exclude(&checkout, KEEP_EXCLUDE_ENTRY)?;
    }
    let text = path
        .to_str()
        .with_context(|| format!("session keep directory {} is not Unicode", path.display()))?;
    environment.insert(SESSION_KEEP_DIR_ENV.to_owned(), text.to_owned());
    Ok(path)
}

/// Interpret the result of `git -C <cwd> rev-parse --show-toplevel` when
/// locating a session's keep directory: returns `(top_level, true)` inside a
/// Git repository, `(cwd, false)` for a plain directory (`exit 128` with
/// `"not a git repository"`), and an error for any other Git failure.
pub fn interpret_checkout_top_level(
    cwd: &Path,
    status: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(PathBuf, bool)> {
    if status == Some(0) {
        let top = std::str::from_utf8(stdout)
            .context("decode Git checkout top level")?
            .trim();
        ensure!(!top.is_empty(), "Git returned an empty checkout top level");
        return Ok((PathBuf::from(top), true));
    }
    let stderr = String::from_utf8_lossy(stderr);
    if status == Some(128) && stderr.contains("not a git repository") {
        return Ok((cwd.to_path_buf(), false));
    }
    bail!("resolve checkout {}: {}", cwd.display(), stderr.trim())
}

fn checkout_top_level(cwd: &Path) -> Result<(PathBuf, bool)> {
    let cwd = std::path::absolute(cwd).with_context(|| format!("resolve {}", cwd.display()))?;
    let output = git(
        &cwd,
        &["rev-parse", "--show-toplevel"],
        "resolve checkout top level",
    )?;
    let (top, repository) =
        interpret_checkout_top_level(&cwd, output.status.code(), &output.stdout, &output.stderr)?;
    let top = std::fs::canonicalize(&top)
        .with_context(|| format!("resolve checkout top level {}", top.display()))?;
    Ok((top, repository))
}

fn create_keep_directory(checkout: &Path, session_id: &str) -> Result<PathBuf> {
    refuse_symlinked_keep_path(checkout, session_id)?;
    let relative = PathBuf::from(KEEP_PARENT).join(KEEP_NAME).join(session_id);
    let root = crate::confined_path::ConfinedDir::open_root(checkout)
        .with_context(|| format!("open checkout {}", checkout.display()))?
        .with_context(|| format!("checkout {} is missing", checkout.display()))?;
    let created = root
        .create_dir_all(&relative)
        .with_context(|| format!("create session keep directory {}", checkout.display()))?;
    let resolved = std::fs::canonicalize(created.path()).with_context(|| {
        format!(
            "resolve session keep directory {}",
            created.path().display()
        )
    })?;
    let relative_resolved = resolved.strip_prefix(checkout).with_context(|| {
        format!(
            "session keep directory {} is outside {}",
            resolved.display(),
            checkout.display()
        )
    })?;
    ensure!(
        relative_resolved == relative,
        "session keep directory {} escaped {}",
        resolved.display(),
        checkout.display()
    );
    Ok(resolved)
}

/// Refuse `.mj`, `.mj/keep`, or `.mj/keep/<session-id>` when that path is a
/// symlink. A missing component stops the walk: nothing below it exists yet.
/// The checkout itself is already canonical, so a symlink above `.mj` is not
/// a component of this walk.
fn refuse_symlinked_keep_path(checkout: &Path, session_id: &str) -> Result<()> {
    let mut current = checkout.to_path_buf();
    for name in [KEEP_PARENT, KEEP_NAME, session_id] {
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing a symlinked session keep path {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

/// Append `line` to the repository's `info/exclude` when it is not already an
/// exact line. A checkout that is not a Git repository is left unchanged.
pub fn append_git_info_exclude(repository: &Path, line: &str) -> Result<()> {
    ensure!(
        !line.is_empty() && !line.contains('\n') && !line.contains('\r'),
        "git exclude entry must be one line"
    );
    let output = git(
        repository,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
        "resolve repository-local exclude file",
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(128) && stderr.contains("not a git repository") {
            return Ok(());
        }
        bail!(
            "resolve info/exclude in {}: {}",
            repository.display(),
            stderr.trim()
        );
    }
    let exclude = crate::local_git::resolve_git_path(
        repository,
        std::str::from_utf8(&output.stdout).context("decode Git exclude path")?,
    )?;
    let existing = match std::fs::read_to_string(&exclude) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", exclude.display()));
        }
    };
    if existing.lines().any(|entry| entry == line) {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .with_context(|| format!("open {}", exclude.display()))?;
    use std::io::Write;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{line}")?;
    Ok(())
}

fn git(cwd: &Path, args: &[&str], purpose: &str) -> Result<std::process::Output> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    command.env("LC_ALL", "C");
    // The checkout named by `-C` is the one this session works in. An ambient
    // Git directory would point the top level and the exclude file elsewhere.
    command.env_remove("GIT_DIR");
    command.env_remove("GIT_WORK_TREE");
    command.env_remove("GIT_COMMON_DIR");
    crate::subprocess::run_with_input(&mut command, &[])
        .with_context(|| format!("start git to {purpose} in {}", cwd.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn git_ok(cwd: &Path, args: &[&str]) -> String {
        let output = git(cwd, args, "test").unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git_ok(path, &["init", "-q"]);
    }

    fn install(cwd: &Path, session_id: &str) -> (PathBuf, BTreeMap<String, String>) {
        let mut environment = BTreeMap::new();
        let path = install_session_keep_dir(cwd, session_id, &mut environment).unwrap();
        (path, environment)
    }

    /// A symlink at `.mj`, `.mj/keep`, or the session directory must not
    /// receive the scratch directory, including when it points at another
    /// session. A clean tree creates the directory, lists `/.mj/keep/` once,
    /// and exports that path.
    #[test]
    fn session_keep_dir_refuses_symlink_escape_and_exports_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let session = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let repo = temp.path().join("repo");
        init_repo(&repo);
        std::fs::write(repo.join(".git/info/exclude"), "*.tmp\n/.mj/agents/").unwrap();

        let (path, environment) = install(&repo, session);
        let again = install(&repo, session);
        assert_eq!(again.0, path);
        assert_eq!(
            environment.get(SESSION_KEEP_DIR_ENV).map(String::as_str),
            Some(path.to_str().unwrap())
        );
        assert_eq!(
            again.1.get(SESSION_KEEP_DIR_ENV),
            environment.get(SESSION_KEEP_DIR_ENV)
        );
        assert!(path.is_dir());
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            path.strip_prefix(repo.canonicalize().unwrap()).unwrap(),
            Path::new(".mj").join("keep").join(session)
        );
        std::fs::write(path.join("receipt.json"), "{}\n").unwrap();
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, "*.tmp\n/.mj/agents/\n/.mj/keep/\n");
        assert_eq!(exclude.matches("/.mj/keep/\n").count(), 1);
        assert_eq!(
            git_ok(&repo, &["status", "--porcelain", "--ignored=no"]),
            ""
        );

        let nested = repo.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let (nested_path, _) = install(&nested, session);
        assert_eq!(nested_path, path);
        assert!(!repo.join("nested/.mj").exists());

        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "keep").unwrap();

        let linked_mj = temp.path().join("linked-mj");
        init_repo(&linked_mj);
        std::os::unix::fs::symlink(&outside, linked_mj.join(".mj")).unwrap();
        let mut environment = BTreeMap::new();
        let error = install_session_keep_dir(&linked_mj, session, &mut environment).unwrap_err();
        assert!(error.to_string().contains("symlinked"), "{error:#}");
        assert!(environment.is_empty());
        assert!(!outside.join("keep").exists());
        assert!(outside.join("secret").is_file());

        let linked_keep = temp.path().join("linked-keep");
        init_repo(&linked_keep);
        std::fs::create_dir(linked_keep.join(".mj")).unwrap();
        std::os::unix::fs::symlink(&outside, linked_keep.join(".mj/keep")).unwrap();
        assert!(install_session_keep_dir(&linked_keep, session, &mut BTreeMap::new()).is_err());
        assert!(!outside.join(session).exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "keep"
        );

        let linked_session = temp.path().join("linked-session");
        init_repo(&linked_session);
        let other_dir = linked_session.join(".mj/keep").join(other);
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(other_dir.join("secret"), "other session").unwrap();
        std::os::unix::fs::symlink(&other_dir, linked_session.join(".mj/keep").join(session))
            .unwrap();
        assert!(install_session_keep_dir(&linked_session, session, &mut BTreeMap::new()).is_err());
        assert_eq!(
            std::fs::read_to_string(other_dir.join("secret")).unwrap(),
            "other session"
        );
        assert!(
            std::fs::symlink_metadata(linked_session.join(".mj/keep").join(session))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let plain = temp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let (plain_path, plain_env) = install(&plain, session);
        assert!(plain_path.is_dir());
        assert!(!plain.join(".git").exists());
        assert_eq!(
            plain_env.get(SESSION_KEEP_DIR_ENV).map(String::as_str),
            Some(plain_path.to_str().unwrap())
        );

        let real = temp.path().join("real");
        init_repo(&real);
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let (aliased, _) = install(&alias, session);
        assert_eq!(
            aliased,
            real.canonicalize()
                .unwrap()
                .join(".mj")
                .join("keep")
                .join(session)
        );
        assert!(aliased.is_dir());

        let escape = temp.path().join("escape");
        init_repo(&escape);
        assert!(install_session_keep_dir(&escape, "../other", &mut BTreeMap::new()).is_err());
        assert!(!temp.path().join("other").exists());
        assert!(!escape.join(".mj").exists());
    }
}
