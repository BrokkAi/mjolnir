//! Interpretation of explicitly entered paths, without shell evaluation or I/O.
use anyhow::{Context, Result, bail, ensure};
use std::path::{Component, Path, PathBuf};

/// Whether an input requests home expansion. Named users are not supported.
pub fn needs_home(path: &Path) -> Result<bool> {
    let first = path.components().next();
    if first.is_some_and(|part| part.as_os_str() == "~") {
        return Ok(true);
    }
    if let Some(Component::Normal(first)) = first
        && first.to_string_lossy().starts_with('~')
    {
        bail!("Named-user paths are unsupported; use ~/ or an explicit path.");
    }
    Ok(false)
}

/// Expand only a leading home component; leave all other components intact.
pub fn expand_home(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if !needs_home(path)? {
        return Ok(path.to_path_buf());
    }
    let home = home.context("Cannot expand ~: the home directory is unavailable.")?;
    ensure!(
        home.is_absolute(),
        "Cannot expand ~: the home directory must be absolute."
    );
    Ok(home.join(path.components().skip(1).collect::<PathBuf>()))
}

pub fn expand_local(path: &Path) -> Result<PathBuf> {
    expand_home(path, dirs::home_dir().as_deref())
}

/// Keep target-specific validation after resolution, but reject unsafe drafts early.
pub fn validate_absolute_input(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() || needs_home(path)?,
        "Enter an absolute path or a path starting with ~/."
    );
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "Path must not contain '..'."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expands_only_the_leading_home_component() {
        let home = Path::new("/home/test user");
        for (input, expected) in [
            ("~", "/home/test user"),
            ("~/資料/a b", "/home/test user/資料/a b"),
            ("relative/~", "relative/~"),
            ("/absolute/~", "/absolute/~"),
            ("./~", "./~"),
            ("$HOME/file", "$HOME/file"),
            ("~/$(touch nope)", "/home/test user/$(touch nope)"),
        ] {
            assert_eq!(
                expand_home(Path::new(input), Some(home)).unwrap(),
                Path::new(expected)
            );
        }
        assert!(expand_home(Path::new("~/file"), None).is_err());
        assert!(expand_home(Path::new("~someone/file"), Some(home)).is_err());
        assert!(expand_home(Path::new("~/file"), Some(Path::new("relative"))).is_err());
    }
}
