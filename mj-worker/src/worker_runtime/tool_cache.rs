//! Configure the build tools already used by a checkout. All generated files
//! belong to this worker; shared directories contain only tool-owned caches.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use mj_core::config::atomic_write_existing;
use mj_core::hex::lower_hex;
use mj_core::targets::posix_quote;
use sha2::{Digest, Sha256};

pub(super) fn prepare(cwd: &Path, environment: &mut BTreeMap<String, String>) -> Result<()> {
    let Some(directory) = environment.get("MJ_TOOL_CACHE_DIR") else {
        return Ok(());
    };
    let shared = PathBuf::from(directory);
    ensure!(shared.is_absolute(), "native build cache must be absolute");
    let home = PathBuf::from(environment.get("HOME").context("build caches need HOME")?);
    let private = PathBuf::from(
        environment
            .get("MJ_TOOL_CACHE_HOME")
            .context("native cache placement needs a private directory")?,
    );
    let private = if private.is_absolute() {
        private
    } else {
        home.join(private)
    };
    environment.insert(
        "MJ_TOOL_CACHE_HOME".into(),
        private.to_string_lossy().into_owned(),
    );
    let private = private.join(lower_hex(Sha256::digest(
        cwd.as_os_str().as_encoded_bytes(),
    )));
    let project = environment
        .get("MJ_TOOL_CACHE_PROJECT")
        .context("native cache placement needs a project identity")?;
    let project_cache = shared
        .join("projects")
        .join(lower_hex(Sha256::digest(project.as_bytes())));
    let bin = private.join("bin");
    std::fs::create_dir_all(&bin).context("create private build tool launchers")?;
    let inherited_path = environment.get("PATH").cloned().unwrap_or_default();
    let previous_bin = environment.get("MJ_BUILD_TOOL_BIN").map(Path::new);
    let path = std::env::join_paths(
        std::env::split_paths(&inherited_path)
            .filter(|entry| entry != &bin && Some(entry.as_path()) != previous_bin),
    )?
    .into_string()
    .map_err(|_| anyhow::anyhow!("build tool PATH is not UTF-8"))?;
    environment.insert("PATH".into(), path.clone());

    if has_any(cwd, &["go.mod", "go.work"]) {
        preserve_go_settings(&home, environment)?;
        set_default(environment, "GOCACHE", shared.join("go"));
        // Go otherwise hashes the absolute package directory, preventing reuse
        // between worktrees. An explicit -trimpath=false remains authoritative.
        let flags = environment.entry("GOFLAGS".into()).or_default();
        if !flags
            .split_whitespace()
            .any(|flag| flag == "-trimpath" || flag.starts_with("-trimpath="))
        {
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str("-trimpath");
        }
    }
    if has_any(cwd, &["turbo.json", "turbo.jsonc"]) {
        set_default(environment, "TURBO_CACHE_DIR", project_cache.join("turbo"));
    }
    if has_any(
        cwd,
        &[
            "settings.gradle",
            "settings.gradle.kts",
            "build.gradle",
            "build.gradle.kts",
        ],
    ) {
        prepare_gradle(cwd, &home, &private, &project_cache, environment)?;
    }
    if cwd.join("nx.json").is_file() {
        // Nx 23.2+ shares artifacts and the cache DB under ~/.nx, while its
        // checkout-specific graph state stays private. The controller mounts
        // the host's directory there. Overriding either NX_* directory opts
        // out of Nx's coordinated placement and must remain the user's choice.
        if let Some(mounted) = environment.get("MJ_NX_HOME_DIR") {
            ensure!(
                home.join(".nx") == Path::new(mounted),
                "Nx cache mount does not match HOME; set the container HOME in its target environment"
            );
        }
    }

    if has_any(
        cwd,
        &[
            "MODULE.bazel",
            "WORKSPACE",
            "WORKSPACE.bazel",
            ".bazelversion",
        ],
    ) {
        let rc = private.join("bazelrc");
        let cache = project_cache.join("bazel");
        let cache = cache
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        atomic_write_existing(&rc, format!("build --disk_cache=\"{cache}\"\n").as_bytes())?;
        for tool in ["bazel", "bazelisk"] {
            if let Some(real) = super::harness_launch::find_command(Path::new(tool), environment)? {
                write_launcher(
                    &bin.join(tool),
                    &format!(
                        "{BAZEL_PATH}\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then exec {} \"$@\"; fi\nexec {} --bazelrc={} \"$@\"\n",
                        quote_path(&real),
                        quote_path(&real),
                        quote_path(&rc)
                    ),
                )?;
            }
        }
    }
    if has_any(
        cwd,
        &["CMakeLists.txt", "Makefile", "makefile", "GNUmakefile"],
    ) && let Some(mbx) = super::harness_launch::find_command(Path::new("mbx"), environment)?
        && supports_mbx_exec(&mbx, cwd, environment)?
    {
        for tool in ["cmake", "make", "gmake", "ninja"] {
            if let Some(real) = super::harness_launch::find_command(Path::new(tool), environment)? {
                write_launcher(
                    &bin.join(tool),
                    &format!(
                        "if [ -n \"${{MJ_MBX_EXEC_ACTIVE:-}}\" ]; then exec {} \"$@\"; fi\nexport MJ_MBX_EXEC_ACTIVE=1\nexec {} exec {} \"$@\"\n",
                        quote_path(&real),
                        quote_path(&mbx),
                        quote_path(&real)
                    ),
                )?;
            }
        }
    }
    let entries = std::iter::once(bin.clone()).chain(std::env::split_paths(&path));
    let path = std::env::join_paths(entries)?
        .into_string()
        .map_err(|_| anyhow::anyhow!("build tool PATH is not UTF-8"))?;
    environment.insert("PATH".into(), path);
    // The existing Git shell hook restores this after bash login files reset
    // PATH. Keep a single BASH_ENV owner, including its user-supplied hook.
    environment.insert(
        "MJ_BUILD_TOOL_BIN".into(),
        bin.to_string_lossy().into_owned(),
    );
    super::shell_environment::configure(&private.join("shell-env"), environment)?;
    Ok(())
}

/// Cache storage is writable to the harness but is not a repository root for
/// checkpointing or review discovery. Use the prepared environment so raw SSH
/// home-relative placement has already become absolute.
pub(super) fn extend_writable_directories(
    harness: mj_core::config::HarnessKind,
    environment: &BTreeMap<String, String>,
    directories: &mut Vec<PathBuf>,
) {
    if harness == mj_core::config::HarnessKind::Muse {
        return;
    }
    for name in ["MJ_TOOL_CACHE_DIR", "MJ_NX_HOME_DIR", "MJ_TOOL_CACHE_HOME"] {
        if let Some(path) = environment.get(name).map(PathBuf::from)
            && path.is_absolute()
            && !directories.contains(&path)
        {
            directories.push(path);
        }
    }
}

fn has_any(root: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| root.join(name).is_file())
}

// Bazel hashes PATH into action keys. Remove only Mjolnir's private launcher
// directories; all user toolchain entries, including empty entries, survive.
const BAZEL_PATH: &str = r#"mj_path=
mj_path_set=
mj_rest=${PATH:-}
while :; do
    case "$mj_rest" in
        *:*) mj_entry=${mj_rest%%:*}; mj_rest=${mj_rest#*:}; mj_more=yes ;;
        *) mj_entry=$mj_rest; mj_more= ;;
    esac
    if { [ -z "${MJ_BUILD_TOOL_BIN:-}" ] || [ "$mj_entry" != "$MJ_BUILD_TOOL_BIN" ]; } &&
       { [ -z "${MJ_GITHUB_CLI_BIN:-}" ] || [ "$mj_entry" != "$MJ_GITHUB_CLI_BIN" ]; }; then
        if [ -n "$mj_path_set" ]; then mj_path=$mj_path:$mj_entry; else mj_path=$mj_entry; fi
        mj_path_set=yes
    fi
    [ -n "$mj_more" ] || break
done
PATH=$mj_path
export PATH
unset mj_path mj_path_set mj_rest mj_entry mj_more
"#;

fn preserve_go_settings(home: &Path, environment: &mut BTreeMap<String, String>) -> Result<()> {
    let Some(go) = super::harness_launch::find_command(Path::new("go"), environment)? else {
        return Ok(());
    };
    // Query outside the checkout and forbid toolchain downloads during worker
    // preparation. GOENV includes `go env -w` settings, not just shell exports.
    let mut command = std::process::Command::new(go);
    command
        .current_dir(home)
        .env_clear()
        .envs(&*environment)
        .env("GOTOOLCHAIN", "local")
        .args(["env", "GOENV"]);
    let output = mj_core::subprocess::run_capturing_stdout(&mut command)?;
    ensure!(
        output.status.success(),
        "could not read Go's persistent environment: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = std::str::from_utf8(&output.stdout)?.trim();
    if path.is_empty() || path == "off" {
        return Ok(());
    }
    for line in read_optional(Path::new(path))?.lines() {
        if let Some((name @ ("GOFLAGS" | "GOCACHE"), value)) = line.split_once('=') {
            environment
                .entry(name.into())
                .or_insert_with(|| value.into());
        }
    }
    Ok(())
}

fn supports_mbx_exec(
    mbx: &Path,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<bool> {
    let mut command = std::process::Command::new(mbx);
    command
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .args(["exec", "--help"]);
    let output = mj_core::subprocess::run_capturing_stdout(&mut command)?;
    if !output.status.success() {
        tracing::warn!(
            "C/C++ cache launchers need mbx with standalone exec support; upgrade the host mbx installation"
        );
    }
    Ok(output.status.success())
}

fn set_default(environment: &mut BTreeMap<String, String>, name: &str, path: PathBuf) {
    environment
        .entry(name.into())
        .or_insert_with(|| path.to_string_lossy().into_owned());
}

fn quote_path(path: &Path) -> String {
    posix_quote(&path.to_string_lossy())
}

fn write_launcher(path: &Path, body: &str) -> Result<()> {
    atomic_write_existing(path, format!("#!/bin/sh\nset -eu\n{body}").as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn prepare_gradle(
    cwd: &Path,
    home: &Path,
    private: &Path,
    shared: &Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    // Only task outputs are shared. Dependency locks and daemons in one user
    // home cannot coordinate across separate container network namespaces.
    let destination = private.join("gradle");
    let source = environment
        .get("MJ_ORIGINAL_GRADLE_USER_HOME")
        .or_else(|| environment.get("GRADLE_USER_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".gradle"));
    let source = if source.is_absolute() {
        source
    } else {
        cwd.join(source)
    };
    ensure!(
        source != destination,
        "Gradle cache preparation lost its original user home"
    );
    let init = destination.join("init.d");
    std::fs::create_dir_all(&init)?;
    let user_properties = read_optional(&source.join("gradle.properties"))?;
    let project_properties = read_optional(&cwd.join("gradle.properties"))?;
    let mut properties = user_properties.clone();
    if !has_gradle_cache_property(&user_properties)
        && !has_gradle_cache_property(&project_properties)
    {
        properties.push_str("\norg.gradle.caching=true\n");
    }
    atomic_write_existing(
        &destination.join("gradle.properties"),
        properties.as_bytes(),
    )?;
    // Preserve user initialization, including credentials and repositories,
    // without copying mutable dependency caches or daemon registries.
    for name in ["init.gradle", "init.gradle.kts"] {
        let path = source.join(name);
        if path.is_file() {
            link_configuration(&path, &destination.join(name))?;
        }
    }
    let source_init = source.join("init.d");
    if source_init.is_dir() {
        for entry in std::fs::read_dir(&source_init)? {
            let entry = entry?;
            link_configuration(&entry.path(), &init.join(entry.file_name()))?;
        }
    }
    // Settings scripts run first: a repository's explicitly chosen local
    // directory takes precedence over the automatically shared default.
    let cache_path =
        serde_json::to_string(&shared.join("gradle").to_string_lossy())?.replace('$', "\\$");
    let script = format!(
        "gradle.settingsEvaluated {{ settings ->\n    if (settings.buildCache.local.directory == null) {{\n        settings.buildCache.local.directory = new File({cache_path})\n    }}\n}}\n"
    );
    let generated = init.join("zz-mjolnir-build-cache.gradle");
    ensure!(
        !source_init.join("zz-mjolnir-build-cache.gradle").exists(),
        "user Gradle initialization uses Mjolnir's reserved init filename"
    );
    atomic_write_existing(&generated, script.as_bytes())?;
    environment.insert(
        "MJ_ORIGINAL_GRADLE_USER_HOME".into(),
        source.to_string_lossy().into_owned(),
    );
    environment.insert(
        "GRADLE_USER_HOME".into(),
        destination.to_string_lossy().into_owned(),
    );
    Ok(())
}

fn read_optional(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn has_gradle_cache_property(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with("org.gradle.caching"))
}

fn link_configuration(source: &Path, destination: &Path) -> Result<()> {
    if std::fs::read_link(destination).ok().as_deref() == Some(source) {
        return Ok(());
    }
    let staging = tempfile::tempdir_in(
        destination
            .parent()
            .context("Gradle configuration has no parent")?,
    )?;
    let link = staging.path().join("config");
    std::os::unix::fs::symlink(source, &link)?;
    std::fs::rename(link, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests;
