use super::*;

pub(crate) struct GithubCliPaths {
    bin: PathBuf,
    wrapper: PathBuf,
    git_config: PathBuf,
}

impl GithubCliPaths {
    fn new(root: &std::path::Path) -> Self {
        let bin = root.join("bin");
        Self {
            wrapper: bin.join("gh"),
            bin,
            git_config: root.join("gitconfig"),
        }
    }
}

pub(crate) fn prepend_github_cli_path(
    bin: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let inherited_path = environment.get("PATH").cloned().unwrap_or_default();
    let mut entries = vec![bin.to_path_buf()];
    if !inherited_path.is_empty() {
        entries.extend(
            std::env::split_paths(std::ffi::OsStr::new(&inherited_path))
                .filter(|entry| entry != bin),
        );
    }
    let path = std::env::join_paths(entries).context("prepend session GitHub CLI to PATH")?;
    environment.insert(
        "PATH".into(),
        path.into_string()
            .map_err(|_| anyhow::anyhow!("session GitHub CLI PATH is not UTF-8"))?,
    );
    Ok(())
}

/// Attach an export to existing session authentication without rewriting it.
/// The wrapper reads the latest token only when Git invokes its helper.
pub fn attach_session_git_environment(
    root: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let root = std::path::absolute(root).context("resolve session worker root")?;
    let paths = GithubCliPaths::new(&root);
    let validate = || -> Result<()> {
        let metadata = std::fs::symlink_metadata(&paths.bin)?;
        anyhow::ensure!(
            metadata.is_dir(),
            "GitHub wrapper directory is not a directory"
        );
        for path in [&paths.wrapper, &paths.git_config] {
            let metadata = std::fs::symlink_metadata(path)
                .with_context(|| format!("inspect {}", path.display()))?;
            anyhow::ensure!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            std::fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
        }
        Ok(())
    };
    validate().with_context(|| {
        format!(
            "load session Git authentication from {}; resume the session to restore its setup",
            root.display()
        )
    })?;
    prepend_github_cli_path(&paths.bin, environment)?;
    environment.insert(
        "GIT_CONFIG_GLOBAL".into(),
        paths.git_config.to_string_lossy().into_owned(),
    );
    // Startup already migrated these entries into the generated configuration.
    // Reapplying them would outrank its absolute credential helper.
    environment.retain(|name, _| {
        name != "GIT_CONFIG_COUNT"
            && !is_indexed_git_config_name(name, "KEY")
            && !is_indexed_git_config_name(name, "VALUE")
            && name != "GH_TOKEN"
            && name != "GITHUB_TOKEN"
    });
    Ok(())
}

/// Install the session-owned GitHub wrapper and Git configuration at startup.
pub fn configure_github_cli(
    root: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const ORIGINAL_BASH_ENV: &str = "MJ_ORIGINAL_BASH_ENV";

    let paths = GithubCliPaths::new(root);
    let bin = &paths.bin;
    if std::fs::symlink_metadata(bin).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "GitHub CLI wrapper directory {} is a symbolic link",
            bin.display()
        );
    }
    std::fs::create_dir_all(bin)
        .with_context(|| format!("create GitHub CLI wrapper directory {}", bin.display()))?;
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o700))?;

    let wrapper = &paths.wrapper;
    if std::fs::symlink_metadata(wrapper).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "GitHub CLI wrapper {} is a symbolic link",
            wrapper.display()
        );
    }
    const WRAPPER: &str = r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
clean_path=
old_ifs=$IFS
IFS=:
for entry in $PATH; do
    if [ "$entry" != "$script_dir" ]; then
        if [ -z "$clean_path" ]; then clean_path=$entry; else clean_path=$clean_path:$entry; fi
    fi
done
IFS=$old_ifs
PATH=$clean_path
export PATH
token_file=$script_dir/../github-token
if [ -f "$token_file" ]; then
    IFS= read -r GH_TOKEN < "$token_file"
    export GH_TOKEN
    unset GITHUB_TOKEN
else
    unset GH_TOKEN GITHUB_TOKEN
fi
exec gh "$@"
"#;
    mj_core::config::atomic_write_existing(wrapper, WRAPPER.as_bytes())?;
    std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700))?;

    // Harnesses can start `bash -lc`, whose login profile may replace PATH
    // after the ACP bridge inherited it. BASH_ENV is read after that profile.
    let shell_environment = root.join("github-shell-env");
    if std::fs::symlink_metadata(&shell_environment)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "GitHub CLI shell environment {} is a symbolic link",
            shell_environment.display()
        );
    }
    const SHELL_ENVIRONMENT: &str = r#"if [ -n "${MJ_ORIGINAL_BASH_ENV:-}" ] && [ "${MJ_ORIGINAL_BASH_ENV}" != "${BASH_ENV:-}" ] && [ -r "${MJ_ORIGINAL_BASH_ENV}" ]; then
    . "${MJ_ORIGINAL_BASH_ENV}"
fi
if [ -n "${MJ_GITHUB_CLI_BIN:-}" ]; then
    case ":${PATH:-}:" in
        *:"${MJ_GITHUB_CLI_BIN}":*) ;;
        *) PATH="${MJ_GITHUB_CLI_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
"#;
    mj_core::config::atomic_write_existing(&shell_environment, SHELL_ENVIRONMENT.as_bytes())?;
    std::fs::set_permissions(&shell_environment, std::fs::Permissions::from_mode(0o600))?;

    prepend_github_cli_path(bin, environment)?;
    environment.insert(
        crate::worker_runtime::GITHUB_CLI_BIN_ENV.into(),
        bin.to_string_lossy().into_owned(),
    );
    let shell_environment_text = shell_environment.to_string_lossy().into_owned();
    let original_bash_env = environment
        .get("BASH_ENV")
        .filter(|configured| configured.as_str() != shell_environment_text)
        .cloned()
        .or_else(|| environment.get(ORIGINAL_BASH_ENV).cloned());
    match original_bash_env {
        Some(original) => {
            environment.insert(ORIGINAL_BASH_ENV.into(), original);
        }
        None => {
            environment.remove(ORIGINAL_BASH_ENV);
        }
    }
    environment.insert("BASH_ENV".into(), shell_environment_text);
    configure_git_config_file(root, environment, &paths)?;

    let inherited_token = std::env::var("GH_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());
    if let Some(token) = inherited_token
        && mj_core::credentials::validate_github_token(token.as_bytes()).is_ok()
    {
        mj_core::credentials::write_github_token(&root.join("github-token"), token.as_bytes())?;
    }
    Ok(())
}

/// Give the harness process tree its Git settings through a worker-owned
/// global configuration file named by `GIT_CONFIG_GLOBAL`.
///
/// The environment form those settings used to take cannot survive a harness
/// that drops credential-shaped variable names. A harness that spawns every
/// tool with a parent environment scrubbed of names matching
/// `KEY|PASSWORD|SECRET|TOKEN` removes `GIT_CONFIG_KEY_*` while keeping
/// `GIT_CONFIG_COUNT`, so each git command in the session fails with
/// "missing config key GIT_CONFIG_KEY_0". One path variable carries no
/// credential-shaped name, and a file cannot be partly delivered.
pub(crate) fn configure_git_config_file(
    root: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
    paths: &GithubCliPaths,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const ORIGINAL_GIT_CONFIG_GLOBAL: &str = "MJ_ORIGINAL_GIT_CONFIG_GLOBAL";

    let path = &paths.git_config;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("Git configuration {} is a symbolic link", path.display());
    }
    let path_text = path.to_string_lossy().into_owned();

    // This file replaces the global configuration Git would otherwise read, so
    // it includes what it replaces. Remember a caller's own choice separately:
    // after the first call `GIT_CONFIG_GLOBAL` names this file, and the
    // original would otherwise be lost on the next call.
    let original_global = environment
        .get("GIT_CONFIG_GLOBAL")
        .filter(|configured| configured.as_str() != path_text)
        .cloned()
        .or_else(|| environment.get(ORIGINAL_GIT_CONFIG_GLOBAL).cloned());
    let mut entries = match &original_global {
        Some(original) => {
            environment.insert(ORIGINAL_GIT_CONFIG_GLOBAL.into(), original.clone());
            vec![("include.path".to_owned(), original.clone())]
        }
        None => {
            environment.remove(ORIGINAL_GIT_CONFIG_GLOBAL);
            // Git's own order for the files this replaces: the XDG file first,
            // then `~/.gitconfig`, which therefore wins. A missing include
            // path is ignored, so both can be named unconditionally.
            let xdg = match environment.get("XDG_CONFIG_HOME") {
                Some(home) if !home.is_empty() => format!("{home}/git/config"),
                _ => "~/.config/git/config".to_owned(),
            };
            vec![
                ("include.path".to_owned(), xdg),
                ("include.path".to_owned(), "~/.gitconfig".to_owned()),
            ]
        }
    };
    // Inherited entries get their own included file. Folding them into this
    // one would lose them the next time it is written, because by then they
    // are gone from the environment. A missing include is ignored, so the
    // include is unconditional.
    let inherited_path = root.join("gitconfig-inherited");
    if std::fs::symlink_metadata(&inherited_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "Git configuration {} is a symbolic link",
            inherited_path.display()
        );
    }
    let inherited = take_environment_git_config(environment);
    if !inherited.is_empty() {
        mj_core::config::atomic_write_existing(
            &inherited_path,
            render_git_config(&inherited)?.as_bytes(),
        )?;
        std::fs::set_permissions(&inherited_path, std::fs::Permissions::from_mode(0o600))?;
    }
    entries.push((
        "include.path".to_owned(),
        inherited_path.to_string_lossy().into_owned(),
    ));

    // The empty value clears image and user helpers before the absolute Hel
    // helper is added. Settings later in the file win, so these outrank every
    // included file.
    let helper = format!(
        "!{} auth git-credential",
        mj_core::targets::posix_quote(&paths.wrapper.to_string_lossy())
    );
    for host in ["github.com", "gist.github.com"] {
        let key = format!("credential.https://{host}.helper");
        entries.push((key.clone(), String::new()));
        entries.push((key, helper.clone()));
    }

    mj_core::config::atomic_write_existing(path, render_git_config(&entries)?.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    environment.insert("GIT_CONFIG_GLOBAL".into(), path_text);
    Ok(())
}

/// Move inherited `GIT_CONFIG_COUNT` entries out of the environment and into
/// configuration entries. They must not stay behind: a harness that drops only
/// some of those variables would leave Git a count it cannot satisfy, which
/// fails every git command rather than only losing a setting.
pub(crate) fn take_environment_git_config(
    environment: &mut BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let count = match environment.remove("GIT_CONFIG_COUNT") {
        Some(count) => match count.parse::<usize>() {
            Ok(count) => count,
            Err(error) => {
                tracing::warn!(
                    count,
                    %error,
                    "ignoring inherited Git configuration with an unparsable count"
                );
                0
            }
        },
        None => 0,
    };
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let key_name = format!("GIT_CONFIG_KEY_{index}");
        let value_name = format!("GIT_CONFIG_VALUE_{index}");
        match (
            environment.remove(&key_name),
            environment.remove(&value_name),
        ) {
            (Some(key), Some(value)) => entries.push((key, value)),
            // An incomplete pair cannot be recovered, and the session is more
            // useful without it than refusing to start.
            _ => tracing::warn!(
                %key_name,
                %value_name,
                "ignoring an incomplete inherited Git configuration entry"
            ),
        }
    }
    environment.retain(|name, _| {
        !is_indexed_git_config_name(name, "KEY") && !is_indexed_git_config_name(name, "VALUE")
    });
    entries
}

pub(crate) fn is_indexed_git_config_name(name: &str, kind: &str) -> bool {
    name.strip_prefix("GIT_CONFIG_")
        .and_then(|rest| rest.strip_prefix(kind))
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|index| !index.is_empty() && index.chars().all(|digit| digit.is_ascii_digit()))
}

/// Render entries as a Git configuration file. Keys arrive in the
/// `section.subsection.key` form `GIT_CONFIG_KEY_*` and `git -c` accept.
pub(crate) fn render_git_config(entries: &[(String, String)]) -> Result<String> {
    let mut text = String::from("# Written by Mjolnir for this session; edits are overwritten.\n");
    let mut open_section: Option<String> = None;
    for (key, value) in entries {
        let (section, name) = split_git_config_key(key)?;
        if open_section.as_deref() != Some(section.as_str()) {
            text.push_str(&section);
            text.push('\n');
            open_section = Some(section);
        }
        text.push('\t');
        text.push_str(&name);
        text.push_str(" = ");
        text.push_str(&quote_git_config_value(value)?);
        text.push('\n');
    }
    Ok(text)
}

/// Split a configuration key into its rendered section header and its name.
pub(crate) fn split_git_config_key(key: &str) -> Result<(String, String)> {
    let (section, rest) = key
        .split_once('.')
        .with_context(|| format!("Git configuration key {key:?} names no section"))?;
    let (subsection, name) = match rest.rsplit_once('.') {
        Some((subsection, name)) => (Some(subsection), name),
        None => (None, rest),
    };
    for (part, label) in [(section, "section"), (name, "name")] {
        if part.is_empty()
            || !part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            bail!("Git configuration key {key:?} has an unusable {label}");
        }
    }
    let header = match subsection {
        // A subsection name keeps its own quoting rules: only a backslash and
        // a double quote are escapes, so no other character may be encoded.
        Some(subsection) => {
            if let Some(control) = subsection.chars().find(|character| character.is_control()) {
                bail!(
                    "Git configuration key {key:?} has a control character {control:?} in its subsection"
                );
            }
            format!("[{section} \"{}\"]", escape_git_config_text(subsection))
        }
        None => format!("[{section}]"),
    };
    Ok((header, name.to_owned()))
}

pub(crate) fn quote_git_config_value(value: &str) -> Result<String> {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\u{8}' => quoted.push_str("\\b"),
            // Git has no escape for any other control character.
            control if control.is_control() => {
                bail!("Git configuration value {value:?} has a control character {control:?}")
            }
            plain => quoted.push(plain),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

pub(crate) fn escape_git_config_text(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
