//! Exact harness versions used by Mjolnir-managed installations.
//!
//! Installation and process ownership live in `brokk-mj-worker`; this module
//! holds shared pin metadata and managed-install paths/manifests so the
//! controller and worker cannot silently disagree about an installation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;

pub const MANIFEST_FILE: &str = "mj-harness.json";
pub const LEASE_FILE: &str = ".lease";
pub const CODEX_CLI_ENTRYPOINT: &str = "node_modules/.bin/codex";
const MANAGED_RUNTIMES_DIR: &str = "mjolnir";
const MANAGED_HARNESSES_DIR: &str = "harnesses";
const MANAGED_NODE_DIR: &str = "node";

/// The exact Node.js release Mjolnir installs when the target has no
/// compatible runtime. Keep this in lockstep with `assets/node/runtime.json`.
pub const NODE_VERSION: &str = "24.22.1";

/// The install receipt written by the worker and checked by managed clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedHarnessManifest {
    schema: u32,
    harness: HarnessKind,
    install_id: String,
}

impl ManagedHarnessManifest {
    pub fn for_harness(harness: HarnessKind) -> Self {
        Self::for_install(harness, pin(harness).install_id)
    }

    pub fn for_install(harness: HarnessKind, install_id: &str) -> Self {
        Self {
            schema: 1,
            harness,
            install_id: install_id.to_owned(),
        }
    }
}

/// Resolve the shared managed-harness cache root from the target environment.
///
/// A non-empty `XDG_CACHE_HOME` takes precedence over `HOME/.cache`; the
/// resulting base must be absolute on every target.
pub fn managed_harness_cache_root(
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    Ok(managed_cache_base(xdg_cache_home, home)?.join(MANAGED_HARNESSES_DIR))
}

/// Resolve the cache directory that holds Mjolnir's pinned Node.js releases.
pub fn managed_node_cache_root(
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    Ok(managed_cache_base(xdg_cache_home, home)?.join(MANAGED_NODE_DIR))
}

fn managed_cache_base(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    let base = match xdg_cache_home.filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            home.filter(|path| !path.is_empty())
                .context("managed harness installation needs HOME or XDG_CACHE_HOME")?,
        )
        .join(".cache"),
    };
    if !base.is_absolute() {
        bail!(
            "managed harness cache root must be absolute: {}",
            base.display()
        );
    }
    Ok(base.join(MANAGED_RUNTIMES_DIR))
}

/// Resolve the pinned install directory under a managed-harness cache root.
pub fn managed_harness_install_dir(cache_root: &Path, harness: HarnessKind) -> PathBuf {
    cache_root.join(harness.id()).join(pin(harness).install_id)
}

/// Check that an installation carries the receipt for the current harness pin.
/// Missing and malformed receipts are incomplete installs; other read failures
/// are returned with their path for diagnostics.
pub fn managed_harness_manifest_matches(
    path: &Path,
    harness: HarnessKind,
    install_id: &str,
) -> Result<bool> {
    let manifest_path = path.join(MANIFEST_FILE);
    let body = match std::fs::read(&manifest_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read managed harness manifest {}", manifest_path.display())
            });
        }
    };
    let manifest: ManagedHarnessManifest = match serde_json::from_slice(&body) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    Ok(manifest == ManagedHarnessManifest::for_install(harness, install_id))
}

pub const CODEX_ACP_PACKAGE: &str = "@brokkai/codex-acp";
pub const CODEX_ACP_VERSION: &str = "1.15.0";
pub const CODEX_CLI_VERSION: &str = "0.164.0";
pub const CLAUDE_ACP_VERSION: &str = "0.97.0";
pub const CLAUDE_CLI_VERSION: &str = "2.1.308";
pub const KIMI_VERSION: &str = "2.3.2";
pub const GROK_VERSION: &str = "1.0.40";
pub const MUSE_ACP_PACKAGE: &str = "@brokkai/muse-acp";
pub const MUSE_ACP_VERSION: &str = "0.18.0";
pub const MUSE_VERSION: &str = "1.4.13-R6996.1";
pub const OPENCODE_VERSION: &str = "2.1.2";
pub const MIMO_CLI_VERSION: &str = "0.1.15";
pub const DSH_ACP_VERSION: &str = "0.5.0";
pub const DSH_ACP_PACKAGE: &str = "@brokkai/dsh-acp";

/// The shell commands that start OpenCode's ACP server with the executable
/// named by `$opencode`, for the managed install and the container fallback
/// alike.
///
/// OpenCode 2 keeps its logins in its database, which a session home starts
/// without, so the commands first import the logins staging wrote into the
/// home ([`HarnessKind::credential_file_name`]). `auth import` adds only the
/// entries the database lacks, so a relaunch imports nothing again, and
/// `--standalone` keeps it off any OpenCode background service. Its report
/// goes to standard error because standard output is the ACP stream, and a
/// failed import fails the launch rather than starting a session without
/// the logins.
pub fn opencode_acp_script() -> String {
    let kind = HarnessKind::OpenCode;
    format!(
        "logins=\"${{{config}:?}}/{id}/{logins}\"; if [ -f \"$logins\" ]; then \"$opencode\" auth import --standalone \"$logins\" >&2 || exit; fi; exec \"$opencode\" {acp}",
        config = kind.home_env(),
        id = kind.id(),
        logins = kind.credential_file_name(),
        acp = kind
            .bridge_args(crate::config::ExecutionPolicy::ConfiguredApprovals)
            .join(" "),
    )
}

/// The most bytes dsh-acp reads from one instruction file.
pub const DSH_ACP_INSTRUCTIONS_LIMIT: usize = 1024 * 1024;

/// Why dsh-acp cannot load an instruction file.
/// The controller refuses a session for any of these before it launches anything
/// ([`crate::config::HarnessProfile::ensure_ready`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndeliverableDshInstructions {
    /// The path exists but is not a regular file.
    NotRegularFile,
    /// The file has more bytes than [`DSH_ACP_INSTRUCTIONS_LIMIT`].
    TooLong { bytes: u64 },
    /// The file is not UTF-8, and dsh-acp refuses instruction files that are
    /// not. `line` is the line with the first byte that is not.
    NotUtf8 { line: usize },
}

/// Whether dsh-acp can load the instruction file at `path`. Like dsh-acp,
/// this reads only a regular file.
pub fn undeliverable_dsh_instructions(
    path: &Path,
) -> std::io::Result<Option<UndeliverableDshInstructions>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(Some(UndeliverableDshInstructions::NotRegularFile)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let too_long = |bytes: u64| {
        (bytes > DSH_ACP_INSTRUCTIONS_LIMIT as u64)
            .then_some(UndeliverableDshInstructions::TooLong { bytes })
    };
    if let Some(problem) = too_long(metadata.len()) {
        return Ok(Some(problem));
    }
    let contents = std::fs::read(path)?;
    if let Some(problem) = too_long(contents.len() as u64) {
        return Ok(Some(problem));
    }
    Ok(std::str::from_utf8(&contents).err().map(|error| {
        let before = &contents[..error.valid_up_to()];
        UndeliverableDshInstructions::NotUtf8 {
            line: before.iter().filter(|&&byte| byte == b'\n').count() + 1,
        }
    }))
}

/// The shell commands that start dsh-acp, which the positional arguments
/// name (`"$@"`), for bare and container targets alike.
///
/// dsh-acp reads [`HarnessKind::agent_instructions_file`] from its data
/// directory, the session working directory and each additional workspace
/// root, rebuilding that prompt whenever a session is opened. The command
/// clears `DSH_ACP_SYSTEM_PROMPT` so a value from the target's login shell
/// cannot add a second prompt beside the staged file. Without
/// `DSH_ACP_DATA_DIR` dsh-acp would keep its sessions in a `.dsh-acp`
/// directory inside the repository, so the command refuses to start.
pub fn dsh_acp_script() -> String {
    let kind = HarnessKind::Dsh;
    format!(
        "if [ -z \"${{{data}:-}}\" ]; then echo 'Mjolnir requires {data} to keep dsh-acp sessions outside the workspace' >&2; exit 1; fi; DSH_ACP_SYSTEM_PROMPT=; export DSH_ACP_SYSTEM_PROMPT; exec \"$@\"",
        data = kind.home_env(),
    )
}

fn dsh_acp_required_major_minor() -> (u64, u64) {
    DSH_ACP_VERSION
        .split_once('.')
        .and_then(|(major, minor)| {
            Some((
                major.parse::<u64>().ok()?,
                minor.split('.').next()?.parse::<u64>().ok()?,
            ))
        })
        .expect("DSH_ACP_VERSION starts with numeric major and minor versions")
}

/// Whether `dsh-acp --version` output names a release that loads staged
/// instructions from its data directory.
pub fn dsh_acp_version_supports_native_instructions(version_output: &str) -> bool {
    let Some(version) = version_output.strip_prefix("dsh-acp ") else {
        return false;
    };
    let mut version = version.trim().split('.');
    let Some(Ok(major)) = version.next().map(str::parse::<u64>) else {
        return false;
    };
    let Some(Ok(minor)) = version.next().map(str::parse::<u64>) else {
        return false;
    };
    let (required_major, required_minor) = dsh_acp_required_major_minor();
    major > required_major || (major == required_major && minor >= required_minor)
}

/// A POSIX shell command that succeeds when the `dsh-acp` on `PATH` can load
/// staged instructions from its data directory. The caller checks that the
/// command exists first.
pub fn dsh_acp_version_supported_script() -> String {
    let (required_major, required_minor) = dsh_acp_required_major_minor();
    format!(
        "{{ reported=\"$(dsh-acp --version 2>/dev/null)\" || reported=; case \"$reported\" in 'dsh-acp '*) reported=\"${{reported#dsh-acp }}\" ;; *) reported= ;; esac; major=\"${{reported%%.*}}\"; rest=\"${{reported#*.}}\"; minor=\"${{rest%%.*}}\"; case \"$major\" in ''|*[!0-9]*) major= ;; esac; case \"$minor\" in ''|*[!0-9]*) minor= ;; esac; [ -n \"$major\" ] && [ -n \"$minor\" ] && {{ [ \"$major\" -gt {required_major} ] || {{ [ \"$major\" -eq {required_major} ] && [ \"$minor\" -ge {required_minor} ]; }}; }}; }}",
        required_major = required_major,
        required_minor = required_minor,
    )
}

/// The shell commands that start dsh-acp on any target: the target's own
/// `dsh-acp` when its version supports native instruction loading, otherwise
/// the pinned npm package through `npx`, running [`dsh_acp_script`] with the
/// resolved command as its positional arguments. DSH has no managed
/// installation — the npm package bundles the same release binaries — so this
/// one script is the whole launch path for bare and container targets alike,
/// and for the model discovery probe.
pub fn dsh_acp_launch_script() -> String {
    format!(
        concat!(
            "if command -v dsh-acp >/dev/null 2>&1; then if {version_supported}; then ",
            "set -- dsh-acp; ",
            "elif command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1; then ",
            "set -- npx -y {package}@{version}; ",
            "else echo 'Mjolnir requires dsh-acp {version} or newer to load AGENTS.md; upgrade it or install Node.js 22+, npm, and npx to use the pinned release' >&2; exit 127; fi; ",
            "elif command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1; then ",
            "set -- npx -y {package}@{version}; ",
            "else echo 'Mjolnir needs dsh-acp or Node.js, npm, and npx on PATH; install one in the target environment' >&2; exit 127; fi; ",
            "{script}"
        ),
        version_supported = dsh_acp_version_supported_script(),
        package = DSH_ACP_PACKAGE,
        version = DSH_ACP_VERSION,
        script = dsh_acp_script(),
    )
}

/// The shell launcher used by container targets when Mjolnir's worker has not
/// replaced it with a managed installation. Workers also use this value to
/// distinguish the built-in fallback from a custom launch command.
pub fn fallback_launch(
    harness: HarnessKind,
    policy: crate::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    match harness {
        HarnessKind::Codex | HarnessKind::Claude | HarnessKind::Muse => {
            let bridge = npm_bridge(harness).expect("npm harness has a bridge");
            (
                "sh".into(),
                vec!["-c".into(), bridge.bootstrap_script()],
            )
        }
        HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code on PATH or a Mjolnir-managed installation' >&2; exit 127; fi".into(),
            ],
        ),
        HarnessKind::Grok => {
            let acp = harness.bridge_args(policy).join(" ");
            (
                "sh".into(),
                vec![
                    "-c".into(),
                    format!(
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build on PATH or a Mjolnir-managed installation' >&2; exit 127; fi"
                    ),
                ],
            )
        }
        HarnessKind::OpenCode => (
            "sh".into(),
            vec![
                "-c".into(),
                format!(
                    "if command -v opencode >/dev/null 2>&1; then opencode=opencode; elif [ -x \"$HOME/.opencode/bin/opencode\" ]; then opencode=\"$HOME/.opencode/bin/opencode\"; else echo 'Mjolnir needs compatible OpenCode on PATH or a Mjolnir-managed installation' >&2; exit 127; fi; {}",
                    opencode_acp_script()
                ),
            ],
        ),
        HarnessKind::Mimo => (
            "sh".into(),
            vec![
                "-c".into(),
                "if command -v mimo >/dev/null 2>&1; then exec mimo acp; else echo 'Mjolnir needs compatible MiMo Code on PATH or a Mjolnir-managed installation' >&2; exit 127; fi".into(),
            ],
        ),
        HarnessKind::Dsh => (
            "sh".into(),
            vec!["-c".into(), dsh_acp_launch_script()],
        ),
    }
}

/// Whether a persisted launch description is the one Mjolnir generated for
/// this harness, rather than a command supplied by a caller.
pub fn is_fallback_launch(
    harness: HarnessKind,
    policy: crate::config::ExecutionPolicy,
    command: &Path,
    args: &[String],
) -> bool {
    let (expected_command, expected_args) = fallback_launch(harness, policy);
    command == Path::new(&expected_command) && args == expected_args
}

/// The built-in npm launcher, selected on the worker before ACP startup.
#[derive(Clone, Copy)]
pub struct NpmBridge {
    pub command: &'static str,
    pub package: &'static str,
    pub version: &'static str,
}

pub const fn npm_bridge(kind: HarnessKind) -> Option<NpmBridge> {
    match kind {
        HarnessKind::Codex => Some(NpmBridge {
            command: "codex-acp",
            package: CODEX_ACP_PACKAGE,
            version: CODEX_ACP_VERSION,
        }),
        HarnessKind::Claude => Some(NpmBridge {
            command: "claude-agent-acp",
            package: "@agentclientprotocol/claude-agent-acp",
            version: CLAUDE_ACP_VERSION,
        }),
        // The npm package carries the adapter only; the native Muse server
        // keeps its own verified download.
        HarnessKind::Muse => Some(NpmBridge {
            command: "muse-acp",
            package: MUSE_ACP_PACKAGE,
            version: MUSE_ACP_VERSION,
        }),
        _ => None,
    }
}

/// Whether preparing this harness may need Node.js, npm, and npx.
///
/// Kimi, Grok, and OpenCode install and launch native executables. Codex,
/// Claude, Muse, and MiMo install npm packages or adapters. DSH itself is not
/// managed, but its fallback fetches the pinned package through npx.
pub const fn harness_uses_node_runtime(kind: HarnessKind) -> bool {
    matches!(
        kind,
        HarnessKind::Codex
            | HarnessKind::Claude
            | HarnessKind::Muse
            | HarnessKind::Mimo
            | HarnessKind::Dsh
    )
}

impl NpmBridge {
    pub fn matches_launcher(&self, command: &std::path::Path, args: &[String]) -> bool {
        (command == std::path::Path::new(self.command) && args.is_empty())
            || self.is_legacy_launcher(command, args)
    }

    /// Keep launch descriptions usable by older workers, including reviewers on
    /// a busy worker that cannot upgrade yet. New workers resolve this before ACP.
    pub fn bootstrap_script(&self) -> String {
        self.script_for_version(self.version)
    }

    /// Recognize persisted launch descriptions from before worker-side selection.
    /// Match the entire generated script; arbitrary shell wrappers stay custom.
    pub fn is_legacy_launcher(&self, command: &std::path::Path, args: &[String]) -> bool {
        if command != std::path::Path::new("sh") || args.len() != 2 || args[0] != "-c" {
            return false;
        }
        let Some((_, version)) = args[1].rsplit_once(&format!("exec npx -y {}@", self.package))
        else {
            return false;
        };
        if version.is_empty()
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
        {
            return false;
        }
        args[1] == self.script_for_version(version)
    }

    fn script_for_version(&self, version: &str) -> String {
        let check = if self.package == CODEX_ACP_PACKAGE {
            format!(
                " && [ \"$(codex-acp --version 2>/dev/null)\" = \"{} {version}\" ]",
                self.package
            )
        } else {
            String::new()
        };
        let node_check = "if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v npx >/dev/null 2>&1; then echo 'Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment' >&2; exit 127; fi";
        format!(
            "if command -v {command} >/dev/null 2>&1{check}; then exec {command}; fi; {node_check}; exec npx -y {package}@{version}",
            command = self.command,
            package = self.package,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPin {
    /// Identity of the installed content, and the directory name under the
    /// harness cache. Change it whenever the installed bytes change, including
    /// a change to the harness asset `package.json`, so caches written by an
    /// earlier packaging are replaced instead of reused.
    pub install_id: &'static str,
    pub display_version: &'static str,
    pub entrypoint: &'static str,
    pub cli_entrypoint: &'static str,
}

pub const fn pin(kind: HarnessKind) -> HarnessPin {
    match kind {
        HarnessKind::Muse => HarnessPin {
            install_id: "muse-acp-0.18.0_muse-1.4.13-R6996.1",
            display_version: "muse-acp 0.18.0 + Muse Code 1.4.13-R6996.1",
            entrypoint: "bin/muse",
            cli_entrypoint: "bin/muse",
        },
        HarnessKind::Codex => HarnessPin {
            install_id: "brokkai-codex-acp-1.15.0_codex-0.164.0",
            display_version: "@brokkai/codex-acp 1.15.0 + codex 0.164.0",
            entrypoint: "node_modules/.bin/codex-acp",
            cli_entrypoint: CODEX_CLI_ENTRYPOINT,
        },
        HarnessKind::Claude => HarnessPin {
            // pkg2: the asset lists @anthropic-ai/claude-code in allowScripts.
            // Installs made before that change kept the package's native-binary
            // stub, so their identity must not match this pin.
            install_id: "claude-agent-acp-0.97.0_claude-2.1.308_pkg2",
            display_version: "claude-agent-acp 0.97.0 + Claude Code 2.1.308",
            entrypoint: "node_modules/.bin/claude-agent-acp",
            cli_entrypoint: "node_modules/.bin/claude",
        },
        HarnessKind::Kimi => HarnessPin {
            install_id: "kimi-2.3.2",
            display_version: "Kimi Code 2.3.2",
            entrypoint: "bin/kimi",
            cli_entrypoint: "bin/kimi",
        },
        HarnessKind::Grok => HarnessPin {
            install_id: "grok-1.0.40",
            display_version: "Grok 1.0.40",
            entrypoint: "bin/grok",
            cli_entrypoint: "bin/grok",
        },
        HarnessKind::OpenCode => HarnessPin {
            install_id: "opencode-2.1.2",
            display_version: "OpenCode 2.1.2",
            entrypoint: "opencode",
            cli_entrypoint: "opencode",
        },
        HarnessKind::Mimo => HarnessPin {
            install_id: "mimo-cli-0.1.15",
            display_version: "MiMo Code 0.1.15",
            entrypoint: "node_modules/.bin/mimo",
            cli_entrypoint: "node_modules/.bin/mimo",
        },
        HarnessKind::Dsh => HarnessPin {
            install_id: "dsh-acp-0.5.0",
            display_version: "dsh-acp 0.5.0",
            entrypoint: "dsh-acp",
            cli_entrypoint: "dsh-acp",
        },
    }
}
/// Legacy receipt shapes retained to decode journals and events from shipped releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProvenance {
    ManagedInstallation,
    TargetInstallation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeComponent {
    pub name: String,
    pub version: Option<String>,
    pub sha256: Option<String>,
}

/// Historical runtime identity; new workers no longer inspect installations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub id: Option<String>,
    pub harness: HarnessKind,
    pub platform: String,
    pub provenance: RuntimeProvenance,
    pub components: Vec<RuntimeComponent>,
    pub unavailable_reason: Option<String>,
}

/// One immutable initialization, identified by its position in the worker journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReceipt {
    #[serde(flatten)]
    pub identity: RuntimeIdentity,
    pub event_ordinal: u64,
    pub observed_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_cache_root_prefers_xdg_and_falls_back_when_it_is_empty() {
        // `temp_dir` is absolute on every target; a hardcoded Unix-style
        // `/custom-cache` is relative on Windows and trips the absolute-path
        // guard the function promises on all platforms.
        let xdg = std::env::temp_dir().join("mj-xdg-cache");
        let home = std::env::temp_dir().join("mj-home");
        let xdg_root = managed_harness_cache_root(Some(xdg.as_os_str()), Some(home.as_os_str()))
            .expect("absolute XDG cache root");
        assert_eq!(
            xdg_root,
            xdg.join(MANAGED_RUNTIMES_DIR).join(MANAGED_HARNESSES_DIR)
        );

        let home_root = managed_harness_cache_root(Some(OsStr::new("")), Some(home.as_os_str()))
            .expect("HOME fallback");
        assert_eq!(
            home_root,
            home.join(".cache")
                .join(MANAGED_RUNTIMES_DIR)
                .join(MANAGED_HARNESSES_DIR)
        );
        let node_root = managed_node_cache_root(Some(xdg.as_os_str()), Some(home.as_os_str()))
            .expect("managed Node cache root");
        assert_eq!(
            node_root,
            xdg.join(MANAGED_RUNTIMES_DIR).join(MANAGED_NODE_DIR)
        );

        assert!(
            managed_harness_cache_root(Some(OsStr::new("relative-cache")), Some(home.as_os_str()))
                .is_err(),
            "a relative XDG cache root stays rejected on every target"
        );
    }

    #[test]
    fn managed_manifest_rejects_a_different_install_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install = managed_harness_install_dir(temp.path(), HarnessKind::Codex);
        std::fs::create_dir_all(&install).expect("create install");
        assert!(
            !managed_harness_manifest_matches(
                &install,
                HarnessKind::Codex,
                pin(HarnessKind::Codex).install_id
            )
            .expect("missing manifest")
        );

        let manifest = ManagedHarnessManifest::for_harness(HarnessKind::Codex);
        std::fs::write(
            install.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        assert!(
            managed_harness_manifest_matches(
                &install,
                HarnessKind::Codex,
                pin(HarnessKind::Codex).install_id
            )
            .expect("matching manifest")
        );
        assert!(
            !managed_harness_manifest_matches(&install, HarnessKind::Codex, "other-install")
                .expect("different install id")
        );
    }
}
