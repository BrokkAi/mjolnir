//! Shared helpers for deriving human-readable labels from filesystem paths.

use std::path::{Path, PathBuf};

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
}
