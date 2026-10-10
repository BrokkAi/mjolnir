//! One owner for BASH_ENV, including the user's original startup hook.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Result, ensure};

pub(super) fn configure(path: &Path, environment: &mut BTreeMap<String, String>) -> Result<()> {
    const ORIGINAL: &str = "MJ_ORIGINAL_BASH_ENV";
    const OWNED: &str = "MJ_SESSION_BASH_ENV";
    const REFERENCE: &str = "${MJ_SESSION_BASH_ENV}";
    const SCRIPT: &str = r#"if [ -n "${MJ_ORIGINAL_BASH_ENV:-}" ] && [ "${MJ_ORIGINAL_BASH_ENV}" != "${BASH_ENV:-}" ] && [ -r "${MJ_ORIGINAL_BASH_ENV}" ]; then
    . "${MJ_ORIGINAL_BASH_ENV}"
fi
if [ -n "${MJ_GITHUB_CLI_BIN:-}" ]; then
    case ":${PATH:-}:" in
        *:"${MJ_GITHUB_CLI_BIN}":*) ;;
        *) PATH="${MJ_GITHUB_CLI_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
if [ -n "${MJ_BUILD_TOOL_BIN:-}" ]; then
    case ":${PATH:-}:" in
        *:"${MJ_BUILD_TOOL_BIN}":*) ;;
        *) PATH="${MJ_BUILD_TOOL_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
"#;
    ensure!(
        !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()),
        "session shell environment {} is a symbolic link",
        path.display()
    );
    mj_core::config::atomic_write_existing(path, SCRIPT.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let text = path.to_string_lossy().into_owned();
    // Git preparation and cache preparation can both install this hook. Never
    // chain one generated hook to another: a restart could make a cycle.
    let original = environment
        .get("BASH_ENV")
        .filter(|configured| {
            configured.as_str() != REFERENCE
                && configured.as_str() != text
                && environment.get(OWNED) != Some(*configured)
        })
        .cloned()
        .or_else(|| environment.get(ORIGINAL).cloned());
    if let Some(original) = original {
        environment.insert(ORIGINAL.into(), original);
    } else {
        environment.remove(ORIGINAL);
    }
    environment.insert(OWNED.into(), text.clone());
    // Bash expands BASH_ENV before opening it. Indirection prevents dollar
    // signs or command substitutions in a directory name from being expanded.
    environment.insert("BASH_ENV".into(), REFERENCE.into());
    Ok(())
}
