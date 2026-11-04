//! Session shell hooks and safely published executable wrappers.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use mj_core::targets::posix_quote;

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

pub(super) fn write_executable(path: &Path, contents: &str) -> Result<()> {
    let staging = tempfile::tempdir_in(path.parent().context("launcher has no parent")?)?;
    let temporary = staging.path().join("launcher");
    // Never open an executable for writing in this multithreaded process.
    // An unrelated fork can inherit even a CLOEXEC writer until it execs,
    // making the published launcher fail with ETXTBSY after our own close.
    // This single-threaded shell uses only builtins, so it cannot pass the
    // writer to another process. Reaping it ends every writable description
    // before we publish the inode. Keep the body on stdin, without ARG_MAX.
    let script = format!(
        "umask 077\nprintf '%s' {} > \"$1\"\n",
        posix_quote(contents)
    );
    let output = mj_core::subprocess::run_with_input(
        std::process::Command::new("/bin/sh")
            .args(["-s", "--"])
            .arg(&temporary),
        script.as_bytes(),
    )
    .with_context(|| format!("write launcher {}", path.display()))?;
    ensure!(
        output.status.success(),
        "write launcher {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("publish launcher {}", path.display()))?;
    Ok(())
}
