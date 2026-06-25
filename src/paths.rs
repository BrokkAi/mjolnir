//! Shared helpers for deriving human-readable labels and normalized workspace
//! roots from filesystem paths.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Canonical workspace scope for an ACP session.
///
/// `additional` excludes the primary root even when the user supplies it
/// directly or through a symlink, so capability checks, ACP payloads, and UI
/// labels all agree on whether there are real extra roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRoots {
    primary: PathBuf,
    additional: Vec<PathBuf>,
}

impl WorkspaceRoots {
    pub(crate) fn new(primary: &Path, additional: &[PathBuf]) -> Result<Self> {
        let primary = canonical_existing_directory("workspace root", primary)?;
        let mut canonical_additional = Vec::new();
        for path in additional {
            let canonical = canonical_existing_directory("additional workspace directory", path)?;
            if canonical != primary && !canonical_additional.iter().any(|root| root == &canonical) {
                canonical_additional.push(canonical);
            }
        }
        Ok(Self {
            primary,
            additional: canonical_additional,
        })
    }

    pub(crate) fn additional_directories(&self) -> &[PathBuf] {
        &self.additional
    }

    pub(crate) fn active_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(1 + self.additional.len());
        roots.push(self.primary.clone());
        roots.extend(self.additional.iter().cloned());
        roots
    }
}

fn canonical_existing_directory(label: &str, path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("{label} must be absolute: {}", path.display());
    }
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("resolve {label} {}", path.display()))?;
    let metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("inspect {label} {}", canonical.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("{label} is not a directory: {}", path.display());
    }
    Ok(canonical)
}

pub(crate) fn path_is_under_any_root(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Last path component as a display string, falling back to the full path when
/// the path has no file name (e.g. the filesystem root).
pub fn folder_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Expand a limited set of home-directory shortcuts used in user-entered
/// command/path fields. Supports `~`, `~/...`, `$HOME`, and `$HOME/...`.
pub fn expand_home_shortcut(input: &str) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(input);
    };
    if input == "~" || input == "$HOME" {
        return home;
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = input.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    PathBuf::from(input)
}

/// Normalize commands that are commonly entered by their shell name but need a
/// concrete launcher when spawned directly.
pub fn normalize_spawn_program(program: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if program.extension().is_none()
            && program
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("npx"))
        {
            let mut normalized = program;
            normalized.set_extension("cmd");
            return normalized;
        }
    }
    program
}

/// Render a path for the UI, replacing the user's home directory prefix with
/// `~` when possible so long paths stay a bit shorter.
pub fn display_path_with_tilde(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The directory containing a `.mjolnir` marker dir on the way up from `path`,
/// if any. Used to label a session by its enclosing project rather than the
/// internal worktree/checkout directory.
pub fn parent_above_mjolnir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".mjolnir"))
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Project label for a working directory with no worktree context: the parent
/// above `.mjolnir` when present, otherwise the directory itself.
pub fn project_label_from_cwd(cwd: &Path) -> String {
    if let Some(parent) = parent_above_mjolnir(cwd) {
        return folder_label(&parent);
    }
    folder_label(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_label_uses_last_component() {
        assert_eq!(folder_label(Path::new("/home/me/project")), "project");
    }

    #[test]
    fn expand_home_shortcut_expands_tilde_and_home_env_forms() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(expand_home_shortcut("~"), home);
        assert_eq!(expand_home_shortcut("$HOME"), home);
        assert_eq!(
            expand_home_shortcut("~/project/src"),
            home.join("project/src")
        );
        assert_eq!(
            expand_home_shortcut("$HOME/project/src"),
            home.join("project/src")
        );
    }

    #[test]
    fn expand_home_shortcut_leaves_other_inputs_unchanged() {
        assert_eq!(
            expand_home_shortcut("/tmp/project"),
            PathBuf::from("/tmp/project")
        );
        assert_eq!(
            expand_home_shortcut("${HOME}/project"),
            PathBuf::from("${HOME}/project")
        );
    }

    #[test]
    fn normalize_spawn_program_uses_cmd_shim_for_windows_npx() {
        let normalized = normalize_spawn_program(PathBuf::from("npx"));
        if cfg!(windows) {
            assert_eq!(normalized, PathBuf::from("npx.cmd"));
        } else {
            assert_eq!(normalized, PathBuf::from("npx"));
        }
    }

    #[test]
    fn normalize_spawn_program_keeps_explicit_extensions() {
        assert_eq!(
            normalize_spawn_program(PathBuf::from("npx.cmd")),
            PathBuf::from("npx.cmd")
        );
        assert_eq!(
            normalize_spawn_program(PathBuf::from("npx.ps1")),
            PathBuf::from("npx.ps1")
        );
    }

    #[test]
    fn display_path_with_tilde_shortens_home_prefix() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(
            display_path_with_tilde(&home.join("project/src")),
            "~/project/src"
        );
    }

    #[test]
    fn parent_above_mjolnir_finds_enclosing_project() {
        let path = Path::new("/home/me/project/.mjolnir/worktrees/abc");
        assert_eq!(
            parent_above_mjolnir(path),
            Some(PathBuf::from("/home/me/project"))
        );
    }

    #[test]
    fn project_label_prefers_parent_above_mjolnir() {
        let path = Path::new("/home/me/project/.mjolnir/worktrees/abc");
        assert_eq!(project_label_from_cwd(path), "project");
    }

    #[test]
    fn project_label_falls_back_to_cwd() {
        assert_eq!(project_label_from_cwd(Path::new("/home/me/plain")), "plain");
    }

    #[test]
    fn workspace_roots_deduplicate_additional_directories_against_primary() {
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let roots = WorkspaceRoots::new(
            primary.path(),
            &[
                primary.path().to_path_buf(),
                additional.path().to_path_buf(),
                std::fs::canonicalize(additional.path()).expect("canonical additional"),
            ],
        )
        .expect("workspace roots");

        let active_roots = roots.active_roots();
        assert_eq!(
            active_roots[0],
            std::fs::canonicalize(primary.path()).expect("canonical primary")
        );
        assert_eq!(
            roots.additional_directories(),
            &[std::fs::canonicalize(additional.path()).expect("canonical additional")]
        );
        assert_eq!(roots.additional_directories().len(), 1);
        assert_eq!(active_roots.len(), 2);
    }

    #[test]
    fn path_is_under_any_workspace_root() {
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let outside = tempfile::tempdir().expect("outside");
        let roots = WorkspaceRoots::new(primary.path(), &[additional.path().to_path_buf()])
            .expect("workspace roots")
            .active_roots();
        let primary = std::fs::canonicalize(primary.path()).expect("canonical primary");
        let additional = std::fs::canonicalize(additional.path()).expect("canonical additional");
        let outside = std::fs::canonicalize(outside.path()).expect("canonical outside");

        assert!(path_is_under_any_root(&roots, &primary.join("file.txt")));
        assert!(path_is_under_any_root(&roots, &additional.join("file.txt")));
        assert!(!path_is_under_any_root(&roots, &outside.join("file.txt")));
    }
}
