//! Where the daemon process runs: `[daemon]` in config.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The `[daemon]` table selects where the daemon runs.
///
/// ```toml
/// [daemon]
/// runtime = "podman"          # "host" (default), "podman", "docker" or "ssh"
/// image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
/// mounts = ["/srv/projects"]  # optional host paths, mounted explicitly
/// ```
///
/// `image` and `mounts` apply only to a container runtime. A container
/// otherwise has no host filesystem access; its data lives in a named volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    #[serde(skip_serializing_if = "DaemonRuntime::is_default")]
    pub runtime: DaemonRuntime,
    /// SSH destination, using the user's OpenSSH configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Remote instance; omitted means the client's selected instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ssh_args: Vec<String>,
    /// Remote executable; omitted means `mj` on the login shell PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_command: Option<String>,
    /// The daemon container's image. `None` is this build's agent-dev image,
    /// which carries the harnesses local sessions run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Host directories explicitly mounted into the daemon container at the
    /// same path. Nothing is mounted by default; the daemon's own data lives
    /// in a named volume.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<PathBuf>,
}

impl DaemonConfig {
    /// Read only client-owned placement. Other tables are deliberately not
    /// deserialized: profiles and credentials belong to the daemon machine.
    pub fn load_placement(path: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Placement {
            daemon: DaemonConfig,
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read placement {}", path.display()));
            }
        };
        // The other tables belong to the daemon, so only `[daemon]` keys are
        // this reader's to refuse; an SSH client reads `[daemon]` nowhere else.
        let mut unread = Vec::new();
        let placement: Placement =
            serde_ignored::deserialize(toml::Deserializer::new(&contents), |key| {
                let key = super::setting_path(&key);
                if key.len() > 1 && key[0] == "daemon" {
                    unread.push(key.join("."));
                }
            })
            .with_context(|| format!("parse placement {}", path.display()))?;
        super::refuse_unread_keys(path, &unread)?;
        Ok(placement.daemon)
    }

    /// Whether the file has a `[daemon]` table at all. A placement written as
    /// `runtime = "host"` equals the default once loaded, so only the file can
    /// tell an explicit choice from none.
    pub fn is_set_in(path: &std::path::Path) -> anyhow::Result<bool> {
        use anyhow::Context;
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(contents
                .parse::<toml::Table>()
                .with_context(|| format!("parse placement {}", path.display()))?
                .contains_key("daemon")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("read placement {}", path.display())),
        }
    }

    /// Write this placement as the file's `[daemon]` table, provided the file
    /// still holds `expected`: `None` for no `[daemon]` table at all (an
    /// explicit `runtime = "host"` is a table, though it loads as the default),
    /// else the placement it loads as. The check and the write happen under
    /// one hold of the config lock, so two launches that each decided from the
    /// same file cannot both win: the second is refused instead of silently
    /// replacing the first. Every other table, comment and the order of the
    /// file stay as they are, and the rest of the file is not loaded: an SSH
    /// client's config belongs to the daemon machine and may hold tables this
    /// build must not reinterpret.
    pub fn replace_placement(
        &self,
        path: &std::path::Path,
        expected: Option<&Self>,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let _lock = super::ConfigLock::acquire(path)?;
        super::Config::ensure_writable(path)?;
        let current = if Self::is_set_in(path)? {
            Some(Self::load_placement(path)?)
        } else {
            None
        };
        anyhow::ensure!(
            current.as_ref() == expected,
            "the daemon placement in {} changed while this was running: {}",
            path.display(),
            match current.as_ref().map(|current| current.host.as_deref()) {
                Some(Some(host)) => format!("it now names {host}"),
                Some(None) => "it is now set".to_owned(),
                None => "it is no longer set".to_owned(),
            }
        );
        let existing = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("read placement {}", path.display()));
            }
        };
        let mut document = existing
            .parse::<toml_edit::Document>()
            .with_context(|| format!("parse placement {}", path.display()))?;
        // Only the non-default keys serialize, so the default placement
        // removes the table instead of leaving an empty one behind.
        let keys = toml::to_string(self).context("serialize daemon placement")?;
        let keys = keys
            .parse::<toml_edit::Document>()
            .context("reparse daemon placement")?;
        if keys.as_table().is_empty() {
            document.remove("daemon");
        } else {
            // A file made here starts the way every file Mjolnir writes does.
            // Without the line, switching `runtime` back to `host` by hand
            // would leave a file the local daemon refuses to load.
            if existing.trim().is_empty() {
                document["version"] = toml_edit::value(i64::from(super::CONFIG_VERSION));
            }
            document["daemon"] = toml_edit::Item::Table(keys.as_table().clone());
        }
        super::atomic_write(path, document.to_string().as_bytes())
    }

    /// The instance the remote `mj` runs as: the configured one, else the
    /// client's own. `None` is the remote's unnamed instance. It must not
    /// become `--instance default`, which selects `instances/default`, a
    /// different store from the one a plain `mj` on that machine uses.
    #[must_use]
    pub fn remote_instance(&self) -> Option<String> {
        self.instance.clone().or_else(super::instance_name)
    }

    pub fn remote_command(&self) -> &str {
        self.remote_command.as_deref().unwrap_or("mj")
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where `mj daemon-run` runs. `Podman` and `Docker` run it in a long-lived
/// per-instance container of that engine. The container is its own machine:
/// its data is a named volume, its network is private, and the only host
/// directories it sees are the ones named in [`DaemonConfig::mounts`]. Local
/// sessions' workers run inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonRuntime {
    /// A detached process on this computer.
    #[default]
    Host,
    Podman,
    Docker,
    Ssh,
}

impl DaemonRuntime {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::Host
    }

    /// The container engine program, or `None` for host and SSH placement.
    #[must_use]
    pub fn engine(self) -> Option<&'static str> {
        match self {
            Self::Host | Self::Ssh => None,
            Self::Podman => Some("podman"),
            Self::Docker => Some("docker"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placement_loading_does_not_resolve_daemon_profiles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        // Profiles intentionally have the wrong type for Config: a client
        // must not interpret daemon-owned settings before connecting.
        std::fs::write(
            &path,
            r#"
profiles = "owned by remote"
[daemon]
runtime = "ssh"
host = "test-box"
ssh_args = ["-p", "2222"]
"#,
        )
        .unwrap();
        let placement = super::DaemonConfig::load_placement(&path).unwrap();
        assert_eq!(placement.runtime, super::DaemonRuntime::Ssh);
        assert_eq!(placement.host.as_deref(), Some("test-box"));
        assert_eq!(placement.ssh_args, ["-p", "2222"]);
    }

    /// An SSH client reads `[daemon]` only here, so a misspelled key would
    /// otherwise attach it to another instance without a word. Keys outside
    /// `[daemon]` belong to the daemon and are not this reader's to judge.
    // Hard-won: #1557: once configuration types read past unknown keys, a `[daemon]` typo went unrefused
    #[test]
    fn placement_refuses_a_daemon_key_it_does_not_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "a_later_section = 1\n[daemon]\nruntime = \"ssh\"\nhost = \"box\"\nisntance = \"work\"\n",
        )
        .unwrap();
        let refusal = format!(
            "{:#}",
            super::DaemonConfig::load_placement(&path).unwrap_err()
        );
        assert!(refusal.contains("`daemon.isntance`"), "{refusal}");
        assert!(!refusal.contains("a_later_section"), "{refusal}");
    }

    // Hard-won: 916f27ded: the first-launch save wrote a file with only
    // `[daemon]`, which the local daemon refuses to load once `runtime` is
    // switched back to `host`; and an explicit `runtime = "host"` looked unset.
    #[test]
    fn a_placement_file_made_here_loads_as_a_config_and_explicit_host_counts_as_set() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert!(!super::DaemonConfig::is_set_in(&path).unwrap());
        let ssh = super::DaemonConfig {
            runtime: super::DaemonRuntime::Ssh,
            host: Some("test-box".into()),
            ..super::DaemonConfig::default()
        };
        ssh.replace_placement(&path, None).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("version = "), "{written}");
        crate::config::Config::load_from(&path).unwrap();
        std::fs::write(&path, "version = 23\n[daemon]\nruntime = \"host\"\n").unwrap();
        assert!(super::DaemonConfig::is_set_in(&path).unwrap());
        assert!(
            super::DaemonConfig::load_placement(&path)
                .unwrap()
                .is_default()
        );
        // A launch that saw no table must not overwrite one added since.
        let refused = ssh.replace_placement(&path, None).unwrap_err();
        assert!(
            format!("{refused:#}").contains("it is now set"),
            "{refused:#}"
        );
        assert!(
            super::DaemonConfig::load_placement(&path)
                .unwrap()
                .is_default()
        );
    }

    /// The placement is written into a file the daemon machine owns, so every
    /// other table and comment must survive, and choosing the default
    /// placement again must leave no empty `[daemon]` table behind.
    #[test]
    fn saving_a_placement_keeps_the_rest_of_the_file_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let others = "# kept comment\nprofiles = \"owned by remote\"\n\n[phone]\nenabled = false\n";
        std::fs::write(&path, others).unwrap();
        let ssh = super::DaemonConfig {
            runtime: super::DaemonRuntime::Ssh,
            host: Some("test-box".into()),
            ..super::DaemonConfig::default()
        };
        ssh.replace_placement(&path, None).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(others), "{written}");
        assert_eq!(super::DaemonConfig::load_placement(&path).unwrap(), ssh);

        // A launch that decided from an older file is refused, not applied.
        let stale = super::DaemonConfig::default()
            .replace_placement(&path, None)
            .unwrap_err();
        assert!(
            format!("{stale:#}").contains("it now names test-box"),
            "{stale:#}"
        );
        super::DaemonConfig::default()
            .replace_placement(&path, Some(&ssh))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim_end(),
            others.trim_end()
        );
    }
}
