//! Git author identity and portable session settings from the controller host.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use mj_client::daemon::GitIdentity;
use mj_targets::command_checks::is_missing_command;

const INHERITED_GIT_SETTINGS: &[&str] = &[
    "diff.algorithm",
    "fetch.prune",
    "fetch.prunetags",
    "init.defaultbranch",
    "merge.conflictstyle",
    "pull.ff",
    "pull.rebase",
    "push.autosetupremote",
    "push.default",
    "rebase.autostash",
    "rerere.autoupdate",
    "rerere.enabled",
    "user.email",
    "user.name",
];

pub fn controller_git_settings() -> Result<BTreeMap<String, String>> {
    let Some(config) = read_host_git_config(&|| Command::new("git"))? else {
        return Ok(BTreeMap::new());
    };
    Ok(inherited_git_settings(config.merged()))
}

fn inherited_git_settings(entries: GitConfigEntries) -> BTreeMap<String, String> {
    entries
        .into_iter()
        .filter(|(key, _)| INHERITED_GIT_SETTINGS.contains(&key.as_str()))
        // A key with no value is Git's spelling of boolean true.
        .map(|(key, value)| (key, value.unwrap_or_else(|| "true".to_owned())))
        .collect()
}

/// One Git configuration scope: every key with its last value, `None` for a
/// key written without a value.
type GitConfigEntries = BTreeMap<String, Option<String>>;

/// The host configuration a session sees outside any repository: the system
/// file, then the global file, which wins.
struct HostGitConfig {
    system: GitConfigEntries,
    global: GitConfigEntries,
}

impl HostGitConfig {
    fn merged(&self) -> GitConfigEntries {
        let mut merged = self.system.clone();
        merged.extend(self.global.clone());
        merged
    }
}

/// Reads the host's system and global Git configuration. `None` means Git
/// itself is not installed.
fn read_host_git_config(git: &dyn Fn() -> Command) -> Result<Option<HostGitConfig>> {
    let Some(system) = read_git_config_scope(git(), "--system")? else {
        return Ok(None);
    };
    let Some(global) = read_git_config_scope(git(), "--global")? else {
        return Ok(None);
    };
    Ok(Some(HostGitConfig { system, global }))
}

/// A scope with no configuration file at all is the same as one that sets
/// nothing: `--get-regexp` exits 1 for both, while `--list` fails on the
/// missing file.
fn read_git_config_scope(mut git: Command, scope: &str) -> Result<Option<GitConfigEntries>> {
    git.args(["config", scope, "--includes", "--null", "--get-regexp", "."]);
    let output = match mj_core::subprocess::run_with_input(&mut git, b"") {
        Ok(output) => output,
        Err(error) if is_missing_command(&error) => return Ok(None),
        Err(error) => return Err(error).context("read controller Git configuration"),
    };
    match output.status.code() {
        Some(0) => parse_git_config_entries(&output.stdout).map(Some),
        Some(1) => Ok(Some(GitConfigEntries::new())),
        _ => bail!(
            "read controller Git configuration ({scope}) failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Parses `git config --null --get-regexp` output: `key\nvalue\0` per
/// entry, or `key\0` for a key written without a value.
fn parse_git_config_entries(output: &[u8]) -> Result<GitConfigEntries> {
    let mut entries = GitConfigEntries::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry).context("decode controller Git configuration")?;
        let (key, value) = match entry.split_once('\n') {
            Some((key, value)) => (key, Some(value.to_owned())),
            None => (entry, None),
        };
        entries.insert(key.to_ascii_lowercase(), value);
    }
    Ok(entries)
}

/// Reads the controller host's Git author identity. Blocking: run it from
/// supervised background work, never on a UI loop.
pub fn controller_git_identity() -> Result<GitIdentity> {
    host_git_identity(&|| Command::new("git"))
}

/// Writes the author identity into the controller host's global Git
/// configuration, creating the file when there is none, and returns the
/// identity Git now reports. Blocking, like [`controller_git_identity`].
pub fn set_controller_git_identity(name: &str, email: &str) -> Result<GitIdentity> {
    write_global_git_identity(&|| Command::new("git"), name, email)
}

fn host_git_identity(git: &dyn Fn() -> Command) -> Result<GitIdentity> {
    // Without Git there is no identity to set up, and asking for one would
    // only fail on save.
    let config = read_host_git_config(git)?
        .context("Git is not installed on this host, so sessions have no commit author")?;
    Ok(git_identity_from(&config))
}

fn git_identity_from(config: &HostGitConfig) -> GitIdentity {
    let merged = config.merged();
    let part = |key: &str| {
        merged
            .get(key)
            .cloned()
            .flatten()
            .filter(|value| !value.trim().is_empty())
    };
    GitIdentity {
        name: part("user.name"),
        email: part("user.email"),
        per_directory: config
            .global
            .keys()
            .any(|key| key.starts_with("includeif.")),
    }
}

fn write_global_git_identity(
    git: &dyn Fn() -> Command,
    name: &str,
    email: &str,
) -> Result<GitIdentity> {
    let name = validated_identity_part("name", name)?;
    let email = validated_identity_part("email", email)?;
    ensure!(email.contains('@'), "Git email {email:?} needs an @");
    let previous = read_host_git_config(git)?
        .context("Git is not installed on this host")?
        .global;
    write_global_git_setting(git, "user.name", Some(name))?;
    if let Err(error) = write_global_git_setting(git, "user.email", Some(email)) {
        // Put the name back so a failed save never leaves a new name
        // beside the old email.
        let restored = previous.get("user.name").cloned().flatten();
        return match write_global_git_setting(git, "user.name", restored.as_deref()) {
            Ok(()) => Err(error),
            Err(restore) => Err(error.context(format!(
                "and restoring the previous Git user.name failed: {restore:#}"
            ))),
        };
    }
    let identity = host_git_identity(git)?;
    ensure!(
        identity.name.as_deref() == Some(name) && identity.email.as_deref() == Some(email),
        "Git saved the identity but reports {} <{}>; a system or included setting overrides it",
        identity.name.as_deref().unwrap_or("no name"),
        identity.email.as_deref().unwrap_or("no email"),
    );
    Ok(identity)
}

/// Sets a global key, or removes it when `value` is `None`.
fn write_global_git_setting(
    git: &dyn Fn() -> Command,
    key: &str,
    value: Option<&str>,
) -> Result<()> {
    let mut command = git();
    command.args(["config", "--global"]);
    match value {
        Some(value) => command.args(["--replace-all", "--", key, value]),
        None => command.args(["--unset-all", "--", key]),
    };
    let output = mj_core::subprocess::run_with_input(&mut command, b"")
        .with_context(|| format!("set Git {key}"))?;
    // Exit 5 from --unset-all means the key was already absent.
    let unset_absent = value.is_none() && output.status.code() == Some(5);
    ensure!(
        output.status.success() || unset_absent,
        "set Git {key} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn validated_identity_part<'a>(label: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    ensure!(!value.is_empty(), "Git {label} cannot be empty");
    // Git strips angle brackets and line breaks from author fields, so a
    // value containing them would not be the identity the user typed.
    ensure!(
        !value.contains(['<', '>']) && !value.chars().any(char::is_control),
        "Git {label} cannot contain angle brackets or control characters"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_git_settings_allow_only_portable_non_executable_values() {
        let settings = inherited_git_settings(parse_git_config_entries(
                b"user.name\nAgent User\0USER.EMAIL\nagent@example.test\0pull.rebase\ntrue\0alias.deploy\n!ship\0credential.helper\nstore\0core.editor\nvim\0include.path\n/host/config\0user.name\nFinal User\0",
            )
            .unwrap());

        assert_eq!(
            settings,
            BTreeMap::from([
                ("pull.rebase".into(), "true".into()),
                ("user.email".into(), "agent@example.test".into()),
                ("user.name".into(), "Final User".into()),
            ])
        );
    }

    // Hard-won: #1368: one valueless key such as `[core] bare` made the whole config unreadable.
    #[test]
    fn inherited_git_settings_read_valueless_keys_as_true() {
        let settings = inherited_git_settings(
            parse_git_config_entries(b"core.bare\0rerere.enabled\0user.name\nAgent User\0")
                .unwrap(),
        );

        assert_eq!(
            settings,
            BTreeMap::from([
                ("rerere.enabled".into(), "true".into()),
                ("user.name".into(), "Agent User".into()),
            ])
        );
    }

    #[test]
    fn inherited_git_settings_reject_non_utf8_output() {
        assert!(parse_git_config_entries(b"user.name\n\xff\0").is_err());
    }

    // Hard-won: dbe0e48f4: a controller with no ~/.gitconfig failed every container launch.
    #[test]
    fn host_git_identity_reads_missing_files_as_unset_and_saves_into_them() {
        let home = tempfile::tempdir().unwrap();
        let system = home.path().join("system-gitconfig");
        let git = || {
            let mut command = Command::new("git");
            command
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_CONFIG_NOSYSTEM")
                .env("GIT_CONFIG_SYSTEM", &system)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join("xdg"));
            command
        };

        let config = read_host_git_config(&git).unwrap().unwrap();
        assert!(config.merged().is_empty());
        assert!(host_git_identity(&git).unwrap().needs_setup());

        // A system-wide name applies to host sessions, so it counts; the
        // missing email is still worth asking for.
        std::fs::write(&system, "[user]\n\tname = System Name\n").unwrap();
        assert_eq!(
            host_git_identity(&git).unwrap(),
            GitIdentity {
                name: Some("System Name".to_owned()),
                email: None,
                per_directory: false,
            }
        );

        // The prompt's answer must create the missing global file, override the
        // system name, and read back whole.
        let identity =
            write_global_git_identity(&git, " Ada Lovelace ", "ada@example.test").unwrap();
        assert_eq!(
            identity,
            GitIdentity {
                name: Some("Ada Lovelace".to_owned()),
                email: Some("ada@example.test".to_owned()),
                per_directory: false,
            }
        );
        assert!(home.path().join(".gitconfig").is_file());
    }

    // Hard-won: #1368: Save wrote a plain identity over the user's per-directory includeIf identities.
    #[test]
    fn host_git_identity_does_not_offer_to_override_per_directory_identities() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[includeIf \"gitdir:~/work/\"]\n\tpath = work.gitconfig\n",
        )
        .unwrap();
        let git = || {
            let mut command = Command::new("git");
            command
                .env_remove("GIT_CONFIG_GLOBAL")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join("xdg"));
            command
        };

        let identity = host_git_identity(&git).unwrap();
        assert!(identity.per_directory && !identity.is_complete());
        assert!(!identity.needs_setup());
    }
}
