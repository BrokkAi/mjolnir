//! Opt-in fast-start preferences, separate from the main configuration schema.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigLock, atomic_write, config_path};
use crate::state::SessionResourceAllocation;
use crate::targets::AdditionalMount;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoRecipe {
    pub profile_id: String,
    pub target_id: String,
    pub bundle_id: Option<String>,
    pub project_directory: Option<PathBuf>,
    pub create_managed_worktree: Option<bool>,
    pub mjolnir_subagents: Option<bool>,
    pub additional_mounts: Vec<AdditionalMount>,
    pub resource_allocation: Option<SessionResourceAllocation>,
}

impl GoRecipe {
    /// Project paths and repository choices never leak into another folder.
    pub fn defaults(&self) -> Self {
        Self {
            bundle_id: None,
            project_directory: None,
            additional_mounts: Vec::new(),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectRecipe {
    directory: PathBuf,
    recipe: GoRecipe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoWorkspace {
    pub directory: PathBuf,
    pub workspace_id: String,
    pub last_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoPreferences {
    version: u32,
    pub default: Option<GoRecipe>,
    projects: Vec<ProjectRecipe>,
    #[serde(default)]
    workspaces: Vec<GoWorkspace>,
}

impl Default for GoPreferences {
    fn default() -> Self {
        Self {
            version: 2,
            default: None,
            projects: Vec::new(),
            workspaces: Vec::new(),
        }
    }
}

impl GoPreferences {
    pub fn workspace(&self, directory: &Path) -> Option<&GoWorkspace> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.directory == directory)
    }

    pub fn bind_workspace(path: &Path, directory: PathBuf, workspace_id: String) -> Result<()> {
        Self::update(path, |preferences| {
            if let Some(workspace) = preferences
                .workspaces
                .iter_mut()
                .find(|entry| entry.directory == directory)
            {
                if workspace.workspace_id != workspace_id {
                    workspace.workspace_id = workspace_id;
                    workspace.last_session_id = None;
                }
            } else {
                preferences.workspaces.push(GoWorkspace {
                    directory,
                    workspace_id,
                    last_session_id: None,
                });
            }
        })
    }

    pub fn remember_session(
        path: &Path,
        directory: &Path,
        workspace_id: &str,
        session_id: String,
    ) -> Result<()> {
        let _lock = ConfigLock::acquire(path)?;
        let mut preferences = Self::load(path)?;
        let workspace = preferences
            .workspaces
            .iter_mut()
            .find(|entry| entry.directory == directory && entry.workspace_id == workspace_id)
            .context("fast-start workspace binding changed; selection was not saved")?;
        workspace.last_session_id = Some(session_id);
        preferences.version = 2;
        atomic_write(path, &serde_json::to_vec_pretty(&preferences)?)
    }

    fn update(path: &Path, edit: impl FnOnce(&mut Self)) -> Result<()> {
        let _lock = ConfigLock::acquire(path)?;
        let mut preferences = Self::load(path)?;
        edit(&mut preferences);
        preferences.version = 2;
        atomic_write(path, &serde_json::to_vec_pretty(&preferences)?)
    }

    pub fn directory_label(directory: &Path) -> String {
        let name = directory
            .file_name()
            .unwrap_or(directory.as_os_str())
            .to_string_lossy();
        let name = name
            .chars()
            .filter(|c| !c.is_control())
            .take(48)
            .collect::<String>();
        if name.trim().is_empty() {
            "Project".into()
        } else {
            name
        }
    }

    /// Locate workspaces created by the first implementation without renaming unrelated workspaces.
    pub fn workspace_name(directory: &Path) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(directory.as_os_str().as_encoded_bytes());
        let identity = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let leaf = directory.file_name().unwrap_or_default().to_string_lossy();
        let label = leaf
            .chars()
            .filter(|c| !c.is_control())
            .take(24)
            .collect::<String>();
        format!("go: {label} {identity}")
    }

    pub fn path() -> PathBuf {
        config_path().with_file_name("go.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("read fast-start preferences"),
        };
        let preferences: Self =
            serde_json::from_slice(&bytes).context("read fast-start preferences")?;
        ensure!(
            matches!(preferences.version, 1 | 2),
            "unsupported fast-start preferences version {}",
            preferences.version
        );
        Ok(preferences)
    }

    pub fn recipe(&self, directory: &Path) -> Option<GoRecipe> {
        self.projects
            .iter()
            .find(|project| project.directory == directory)
            .map(|project| project.recipe.clone())
            .or_else(|| self.default.clone())
    }

    pub fn save_recipe(
        path: &Path,
        directory: PathBuf,
        recipe: GoRecipe,
        global: bool,
    ) -> Result<()> {
        Self::update(path, |preferences| {
            if global || preferences.default.is_none() {
                preferences.default = Some(recipe.defaults());
            }
            preferences
                .projects
                .retain(|project| project.directory != directory);
            preferences
                .projects
                .push(ProjectRecipe { directory, recipe });
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(target: &str) -> GoRecipe {
        GoRecipe {
            profile_id: "personal".into(),
            target_id: target.into(),
            bundle_id: Some("repo-a".into()),
            project_directory: Some("/repo-a".into()),
            create_managed_worktree: Some(false),
            mjolnir_subagents: None,
            additional_mounts: Vec::new(),
            resource_allocation: None,
        }
    }

    #[test]
    fn project_overrides_survive_default_changes_without_leaking_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("go.json");
        GoPreferences::save_recipe(&path, "/a".into(), recipe("docker"), false).unwrap();
        GoPreferences::save_recipe(&path, "/b".into(), recipe("remote"), true).unwrap();
        let prefs = GoPreferences::load(&path).unwrap();
        assert_eq!(prefs.recipe(Path::new("/a")).unwrap().target_id, "docker");
        let other = prefs.recipe(Path::new("/c")).unwrap();
        assert_eq!(other.target_id, "remote");
        assert!(other.project_directory.is_none());
        assert!(other.bundle_id.is_none());
    }

    #[test]
    fn long_and_same_named_folders_have_distinct_valid_workspace_names() {
        let a = GoPreferences::workspace_name(Path::new("/one/project"));
        let b = GoPreferences::workspace_name(Path::new("/two/project"));
        assert_ne!(a, b);
        let long = GoPreferences::workspace_name(Path::new(&format!("/{}", "folder".repeat(100))));
        assert!(crate::workspace::normalize_workspace_name(&long).is_ok());
        assert_eq!(a, GoPreferences::workspace_name(Path::new("/one/project")));
    }

    #[test]
    fn workspace_bindings_preserve_selection_and_keep_same_named_folders_separate() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("go.json");
        let first = Path::new("/one/project");
        GoPreferences::bind_workspace(&path, first.into(), "workspace-a".into()).unwrap();
        GoPreferences::remember_session(&path, first, "workspace-a", "conversation-a".into())
            .unwrap();
        GoPreferences::save_recipe(&path, first.into(), recipe("docker"), false).unwrap();
        GoPreferences::bind_workspace(&path, first.into(), "workspace-a".into()).unwrap();
        GoPreferences::bind_workspace(&path, "/two/project".into(), "workspace-b".into()).unwrap();
        let prefs = GoPreferences::load(&path).unwrap();
        assert_eq!(
            prefs.workspace(first).unwrap().last_session_id.as_deref(),
            Some("conversation-a")
        );
        assert_eq!(
            prefs
                .workspace(Path::new("/two/project"))
                .unwrap()
                .workspace_id,
            "workspace-b"
        );
        GoPreferences::bind_workspace(&path, first.into(), "replacement".into()).unwrap();
        assert!(
            GoPreferences::remember_session(&path, first, "workspace-a", "stale".into()).is_err()
        );
        assert!(
            GoPreferences::load(&path)
                .unwrap()
                .workspace(first)
                .unwrap()
                .last_session_id
                .is_none()
        );
    }

    #[test]
    fn previous_preferences_upgrade_without_losing_the_recipe() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("go.json");
        let old = serde_json::json!({"version": 1, "default": recipe("docker"), "projects": []});
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        GoPreferences::bind_workspace(&path, "/project".into(), "workspace".into()).unwrap();
        let prefs = GoPreferences::load(&path).unwrap();
        assert_eq!(prefs.version, 2);
        assert_eq!(
            prefs.recipe(Path::new("/project")).unwrap().target_id,
            "docker"
        );
    }

    #[test]
    fn broken_preferences_are_reported_without_overwriting_them() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("go.json");
        std::fs::write(&path, b"broken").unwrap();
        assert!(GoPreferences::save_recipe(&path, "/a".into(), recipe("docker"), false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    }
}
