use super::*;

#[cfg(unix)]
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The connectivity probe `mj doctor` runs against an SSH target.
///
/// It reuses the provisioning argument order so the probe fails exactly where
/// a real session would, with two deliberate overrides prepended. OpenSSH
/// honours the first occurrence of an option, so these win over the
/// provisioning defaults: `BatchMode=yes` never prompts for a password, and
/// `StrictHostKeyChecking=yes` never accepts an unknown host key. Doctor
/// diagnoses; the user decides whether to trust a key.
///
/// Those overrides are also why the probe joins a shared master but never
/// opens one (see [`CommandSpec::ssh_probe_session`]): as the master it would hold
/// them over every later session on that connection, and a plain `mj doctor`
/// would leave an `ssh` process behind for the whole `ControlPersist` window
/// even though the user asked only for a diagnosis.
pub fn ssh_connectivity_probe(ssh: &SshTarget) -> CommandSpec {
    let mut args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "StrictHostKeyChecking=yes".to_owned(),
    ];
    args.extend(ssh.ssh_args.iter().cloned());
    args.push(ssh.destination.clone());
    args.push(join_remote_command(&["true".to_owned()]));
    // The socket is named after the target as configured, not after these
    // probe-only overrides, so the probe finds the daemon's master.
    CommandSpec::new("ssh", args)
        .ssh_probe_session(ssh)
        .purpose("verify SSH connectivity")
}

pub fn ssh_command(
    ssh: &SshTarget,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> CommandSpec {
    ssh_command_owned(
        ssh,
        args.into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect(),
    )
}

/// Build an `ssh` command that runs as one session on a shared connection.
/// The executor leases the session and adds its options just before the
/// command is spawned; see [`CommandSpec::ssh_session`].
pub fn ssh_command_owned(ssh: &SshTarget, remote_args: Vec<String>) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.push(ssh.destination.clone());
    args.push(remote_command_line(&remote_args));
    CommandSpec::new("ssh", args).ssh_session(ssh)
}

/// Home-relative directory on an SSH host where files bound for a remote
/// container wait before the engine copies them in. Home-relative rather than
/// `~/`, because `ssh_command` quotes every argument while `scp` expands `~`.
pub const REMOTE_UPLOAD_STAGING: &str = ".cache/mjolnir/uploads";

/// Upload a local file or directory to the SSH host.
pub fn scp_upload(ssh: &SshTarget, source: &Path, remote: &str, recursive: bool) -> CommandSpec {
    let mut args = scp_args(ssh);
    if recursive {
        args.push("-r".into());
    }
    args.push(scp_local_path(source));
    args.push(format!("{}:{remote}", ssh.destination));
    scp_command(ssh, args)
}

/// Download a remote file from the SSH host.
pub fn scp_download(ssh: &SshTarget, remote: &str, local: &str) -> CommandSpec {
    let mut args = scp_args(ssh);
    args.push(format!("{}:{remote}", ssh.destination));
    args.push(scp_local_path(Path::new(local)));
    scp_command(ssh, args)
}

/// A controller path as `scp` must see it. `scp` reads an argument with a
/// colon before any separator as `host:path`, and Windows OpenSSH exempts only
/// a plain drive letter, so a verbatim path from `canonicalize`
/// (`\\?\C:\...`) would name the host `\\?\C`. Spell it without the
/// verbatim prefix; nothing else about the path changes.
fn scp_local_path(path: &Path) -> String {
    use std::path::{Component, Prefix};
    let text = path.to_string_lossy();
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) => text[r"\\?\".len()..].to_owned(),
            Prefix::VerbatimUNC(..) => format!(r"\\{}", &text[r"\\?\UNC\".len()..]),
            _ => text.into_owned(),
        },
        _ => text.into_owned(),
    }
}

/// The connection's `ssh` arguments rewritten for `scp`, which spells the port
/// option `-P`; to `scp`, `-p` means "preserve file times".
fn scp_args(ssh: &SshTarget) -> Vec<String> {
    ssh.ssh_args
        .iter()
        .map(|argument| {
            if argument == "-p" {
                "-P".to_owned()
            } else {
                argument.clone()
            }
        })
        .collect()
}

fn scp_command(ssh: &SshTarget, args: Vec<String>) -> CommandSpec {
    // `scp` runs `ssh` underneath, so it takes a session on a shared
    // connection and is retried the same way.
    CommandSpec::new("scp", args).ssh_session(ssh)
}

/// How long a shared master connection stays alive after its last channel
/// closes. The master is an `ssh` process that outlives the daemon by this
/// long, so it is kept short enough to be unsurprising and long enough to
/// cover a whole provision.
#[cfg(unix)]
const CONTROL_PERSIST: &str = "60";

/// The master's `ServerAliveInterval` seconds and `ServerAliveCountMax`: a
/// host silent for a minute is gone. Patient enough that a stall of a few
/// seconds does not drop the sessions on the connection.
#[cfg(unix)]
const MASTER_KEEPALIVE: (u32, u32) = (15, 4);

/// Environment override that turns connection sharing off. Any of `0`, `off`,
/// `false`, or `no` disables it.
pub const CONTROL_MASTER_ENV: &str = "MJ_SSH_CONTROL_MASTER";

/// Longest `ControlPath` that still fits in a Unix socket address. `sun_path`
/// holds 108 bytes on Linux and 104 on macOS, minus the terminating NUL.
#[cfg(unix)]
const MAX_CONTROL_PATH: usize = 103;

/// Hex digits of the connection hash in a socket name. 64 bits keeps the
/// names short while making a collision between two targets implausible.
#[cfg(unix)]
const CONNECTION_HASH_HEX: usize = 16;

/// Bytes reserved after the directory for a socket's name: a separator, the
/// connection hash, `-` and a shard index of up to four digits, and the
/// `.` plus 16 random characters `ssh` appends to the path while it binds a
/// new master.
#[cfg(unix)]
const CONTROL_SOCKET_NAME_RESERVE: usize = 1 + CONNECTION_HASH_HEX + 1 + 4 + 17;

#[cfg(unix)]
fn sharing_disabled(value: Option<&std::ffi::OsStr>) -> bool {
    let Some(value) = value else {
        return false;
    };
    matches!(
        value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// How a test pins connection sharing instead of letting it resolve from the
/// environment.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum SshSharingForTest {
    /// Behave as though the escape hatch were set.
    Disabled,
    /// Keep the control sockets in this directory.
    Directory(PathBuf),
}

static SHARING_OVERRIDE: Mutex<Option<SshSharingForTest>> = Mutex::new(None);

/// The connection-sharing override is process-wide, so the tests that set it
/// take turns.
#[cfg(all(test, unix))]
pub(crate) static SHARING_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Pin connection sharing for a test, or restore the real resolution with
/// `None`. Tests must not depend on the developer's `$XDG_RUNTIME_DIR` or home
/// directory, so every test that inspects `ssh` arguments pins it. Not part of
/// the daemon's behaviour.
#[doc(hidden)]
pub fn set_ssh_connection_sharing_for_test(setting: Option<SshSharingForTest>) {
    *SHARING_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = setting;
}

#[cfg(unix)]
fn sharing_override() -> Option<SshSharingForTest> {
    SHARING_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The directory holding this instance's control sockets, or `None` when
/// sharing is off.
///
/// `$XDG_RUNTIME_DIR/mjolnir/<identity>` is preferred because it is short,
/// per-user, and on tmpfs; the instance's data directory is the fallback.
/// Each instance gets its own directory because each daemon counts only its
/// own sessions: two daemons sharing masters would together exceed the
/// server's per-connection session limit. Neither location is
/// world-writable, and the directory is created 0700 because `ssh` will not
/// create it itself.
#[cfg(unix)]
fn control_socket_dir() -> Option<PathBuf> {
    match sharing_override() {
        Some(SshSharingForTest::Disabled) => return None,
        Some(SshSharingForTest::Directory(dir)) => return prepare_control_dir(dir),
        None => {}
    }
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if sharing_disabled(std::env::var_os(CONTROL_MASTER_ENV).as_deref()) {
            return None;
        }
        let data_dir_override = crate::config::env_override_os("DATA_DIR").map(PathBuf::from);
        let identity = control_dir_identity(
            crate::config::instance_name().as_deref(),
            data_dir_override.as_deref(),
        );
        prepare_control_dir(default_control_dir(
            std::env::var_os("XDG_RUNTIME_DIR"),
            &identity,
        ))
    })
    .clone()
}

/// Where an instance keeps its sockets when no test pins the directory.
#[cfg(unix)]
fn default_control_dir(runtime: Option<std::ffi::OsString>, identity: &str) -> PathBuf {
    match runtime {
        Some(runtime) if !runtime.is_empty() => {
            PathBuf::from(runtime).join("mjolnir").join(identity)
        }
        // The data directory is already specific to the instance.
        _ => crate::config::data_dir().join("ssh"),
    }
}

/// The name of this instance's directory under `$XDG_RUNTIME_DIR/mjolnir`:
/// the identity [`crate::config::instance_identity`] stamps on the
/// instance's workers. A named instance uses its name, and a data-directory
/// override (`MJ_DATA_DIR`, as every end-to-end lab sets) uses the
/// fingerprint of that directory, so neither shares the default instance's
/// masters or sweeps its lock files. The default instance keeps `default`,
/// where its sockets have always been.
#[cfg(unix)]
fn control_dir_identity(instance: Option<&str>, data_dir_override: Option<&Path>) -> String {
    if instance.is_none() && data_dir_override.is_none() {
        return "default".to_owned();
    }
    let data_dir = data_dir_override.map_or_else(crate::config::data_dir, Path::to_path_buf);
    crate::config::instance_identity_for(instance, &data_dir)
}

/// Create the socket directory 0700 and reject one whose sockets would not fit
/// in a Unix socket address. Failure means no sharing, never a failed command.
#[cfg(unix)]
fn prepare_control_dir(dir: PathBuf) -> Option<PathBuf> {
    if dir.as_os_str().len() + CONTROL_SOCKET_NAME_RESERVE > MAX_CONTROL_PATH {
        tracing::debug!(
            directory = %dir.display(),
            "skipping SSH connection sharing: control socket path would be too long"
        );
        return None;
    }
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::debug!(
            directory = %dir.display(),
            %error,
            "skipping SSH connection sharing: control directory is unavailable"
        );
        return None;
    }
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
        tracing::debug!(
            directory = %dir.display(),
            %error,
            "skipping SSH connection sharing: cannot restrict control directory"
        );
        return None;
    }
    Some(dir)
}

/// The identity of one configured connection: its destination and the
/// user's own `ssh` arguments, which can change the port, user, or route.
/// Two targets that differ in either get separate masters.
#[cfg(unix)]
fn connection_key(ssh: &SshTarget) -> String {
    let mut key = ssh.destination.clone();
    for argument in &ssh.ssh_args {
        key.push('\0');
        key.push_str(argument);
    }
    key
}

/// The socket file name for one shard of a connection: `<hash>-<shard>`.
///
/// Mjolnir names sockets itself rather than using `ssh`'s `%C` so that the
/// name follows exactly the key the daemon counts sessions under, and so the
/// daemon knows the concrete path when it must remove a stale socket.
#[cfg(unix)]
fn control_socket_name(ssh: &SshTarget, shard: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(connection_key(ssh).as_bytes());
    let mut name = String::with_capacity(CONNECTION_HASH_HEX + 5);
    for byte in digest.iter().take(CONNECTION_HASH_HEX / 2) {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(&format!("-{shard}"));
    name
}

/// The `-o` value that points `ssh` at `socket`.
///
/// An `ssh` option is one line of config, and `ssh` splits that line on
/// whitespace unless the value stands inside double quotes. A socket path with
/// a space in it -- macOS keeps its data in `~/Library/Application
/// Support/mjolnir` -- therefore ends the option early and `ssh` refuses the
/// whole command line with "keyword controlpath extra arguments at end of
/// line". `%` starts a token expansion in `ControlPath` (and in the other
/// filename options), so it is doubled. A path that needs neither is passed
/// exactly as it always was.
///
/// This is not unix-gated: [`push_session_args`] reaches it on every platform,
/// because a lease's options are compiled there too. Nothing else can produce
/// a socket on Windows, so the Windows path never runs.
fn control_path_option(socket: &Path) -> String {
    let path = socket.display().to_string().replace('%', "%%");
    let path = if path.contains(|character: char| {
        character.is_whitespace() || character == '"' || character == '\\'
    }) {
        // Inside the quotes OpenSSH unescapes only these two.
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path
    };
    format!("ControlPath={path}")
}

/// Whether the user's own `ssh` arguments already configure connection
/// sharing. OpenSSH keeps the first value it sees, so Mjolnir cannot add its
/// own sharing options without either overriding the user or being
/// overridden; a user who configures sharing owns it, and Mjolnir adds none.
#[cfg(unix)]
fn user_configures_sharing(ssh_args: &[String]) -> bool {
    ssh_args.iter().any(|argument| {
        if argument.starts_with("-S") {
            return true;
        }
        let option = argument.strip_prefix("-o").unwrap_or(argument).trim_start();
        let option = option.to_ascii_lowercase();
        ["controlmaster", "controlpath"].iter().any(|name| {
            option
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(['=', ' ', '\t']))
        })
    })
}

/// Append the options that let this command *reuse* a shared master without
/// ever becoming one.
///
/// Use this for commands that carry deliberately impatient options, such as
/// the short `ConnectTimeout` and one-miss `ServerAlive` keepalive of a
/// validation probe or a Tab completion. Those settings belong to the one
/// command that asked for them. If such a command opened the master, the
/// master would enforce them for its whole lifetime and drop every later
/// multiplexed session -- an upload, a `podman run`, the worker bootstrap --
/// on a stall of a couple of seconds. With `ControlMaster=no` the command
/// joins the connection's first master when one is up and otherwise opens its
/// own direct connection, keeping its fail-fast options to itself.
///
/// These commands run in processes without the daemon's session ledger
/// (`mj doctor`, completion) or only validate a target, so they are not
/// counted and are not bound to a master with `ProxyCommand=false`: for a
/// diagnosis, a direct connection is the stated behaviour.
pub fn push_connection_reuse_args(args: &mut Vec<String>, ssh: &SshTarget) {
    #[cfg(unix)]
    if !user_configures_sharing(&ssh.ssh_args)
        && let Some(dir) = control_socket_dir()
    {
        let socket = dir.join(control_socket_name(ssh, 0));
        args.extend([
            "-o".to_owned(),
            "ControlMaster=no".to_owned(),
            "-o".to_owned(),
            control_path_option(&socket),
        ]);
    }
    #[cfg(not(unix))]
    let _ = (args, ssh);
}

/// `args` quoted for a POSIX shell, joined into one line.
pub fn join_remote_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| posix_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command line an `ssh` invocation sends for `args`: each argument
/// quoted, run under [`with_macos_system_path`]. Every builder of an SSH
/// remote command uses this, so a Mac host finds the same programs whatever
/// the command is.
pub fn remote_command_line(args: &[String]) -> String {
    join_remote_command(&with_macos_system_path(args.to_vec()))
}

/// Run `args` with, on a Mac, the directories macOS lists in /etc/paths and
/// /etc/paths.d and Homebrew's Apple-silicon prefix after the session's own
/// PATH, so a directory the account already chose still comes first. Another
/// host, which has no `path_helper`, runs `args` unchanged.
///
/// An OpenSSH command session on macOS gets /usr/bin:/bin:/usr/sbin:/sbin
/// only: macOS adds /etc/paths in login shells, and Homebrew's installer adds
/// /opt/homebrew/bin through the login shell's profile. Apple's `container`
/// (/usr/local/bin) and Homebrew's `docker`, `podman` and `colima` would
/// otherwise not be found.
pub fn with_macos_system_path(args: Vec<String>) -> Vec<String> {
    [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        r#"if [ -x /usr/libexec/path_helper ]; then session=$PATH; eval "$(PATH= /usr/libexec/path_helper -s)"; PATH=${session:+$session:}$PATH:/opt/homebrew/bin:/opt/homebrew/sbin; fi; exec "$@""#
            .to_owned(),
        "sh".to_owned(),
    ]
    .into_iter()
    .chain(args)
    .collect()
}

/// Check whether a directory exists on the configured SSH host.
pub fn ssh_directory_exists(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let command = ssh_validation_command(
        ssh,
        vec![
            "test".into(),
            "-d".into(),
            path.to_string_lossy().into_owned(),
        ],
        "validate remote directory",
    );
    let output = executor.execute(&command)?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        status => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error = anyhow::anyhow!(
                "remote directory check failed with status {status}: {}",
                stderr.trim()
            );
            Err(match host_key_refusal(&stderr, &ssh.ssh_args) {
                Some(refusal) => error.context(refusal),
                None => error,
            })
        }
    }
}

/// What the caller is told when `ssh` refused the host's key, so it does not
/// get only a daemon log reference (launch finding R3-6).
///
/// OpenSSH's wording is the only signal: "Host key verification failed." ends
/// both an unknown key under strict checking and a key that changed. The
/// sentence quotes that line and names no host, so it may reach any client;
/// the full ssh text stays on the error chain for the daemon log. It names
/// the known_hosts file the machine's ssh options name (launch finding R6-5).
fn host_key_refusal(stderr: &str, ssh_args: &[String]) -> Option<crate::refusal::Refusal> {
    if !stderr.contains("Host key verification failed") {
        return None;
    }
    let known_hosts = KnownHostsFile::from_ssh_args(ssh_args);
    let file = known_hosts.phrase();
    Some(crate::refusal::Refusal::precondition(
        if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
            let keygen = match known_hosts.first_named() {
                Some(path) => format!("`ssh-keygen -f {path} -R`"),
                None => "`ssh-keygen -R`".to_owned(),
            };
            format!(
                "ssh reported \"Host key verification failed\": the machine's host key is not the one saved in {file}. If you expected the change, remove the old entry with {keygen} and the host name, add the new key, and try again."
            )
        } else {
            format!(
                "ssh reported \"Host key verification failed\": the machine's host key is not in {file}, and its ssh options require a known key. Add the host key (for example with `ssh-keyscan`, after checking the fingerprint), or put `-o StrictHostKeyChecking=accept-new` in the machine's extra_args, and try again."
            )
        },
    ))
}

/// The known_hosts file ssh checks a host key against, as far as the options
/// on its command line tell.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KnownHostsFile {
    /// No option names one, so ssh uses its default.
    Default,
    /// `-o UserKnownHostsFile=` names this file, or these files separated
    /// by spaces.
    Named(String),
    /// A config file given with `-F` may name any file, or the option names
    /// no usable file.
    Unknown,
}

impl KnownHostsFile {
    /// Read `ssh`'s arguments the way OpenSSH does: the first
    /// `UserKnownHostsFile` given with `-o` wins, the keyword is matched
    /// without regard to case, and its value follows `=` or a space.
    fn from_ssh_args(args: &[String]) -> Self {
        let mut config_file = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let option = match arg.as_str() {
                "-o" => args.next().map(String::as_str),
                other => other.strip_prefix("-o"),
            };
            if arg.starts_with("-F") {
                config_file = true;
            }
            let Some(value) = option.and_then(|option| option_value(option, "UserKnownHostsFile"))
            else {
                continue;
            };
            let value = value.trim_matches('"').trim();
            return if value.is_empty() || value.eq_ignore_ascii_case("none") {
                Self::Unknown
            } else {
                Self::Named(value.to_owned())
            };
        }
        if config_file {
            Self::Unknown
        } else {
            Self::Default
        }
    }

    /// The file as a refusal names it.
    fn phrase(&self) -> String {
        match self {
            Self::Default => "~/.ssh/known_hosts".to_owned(),
            Self::Named(paths) => paths.split_whitespace().collect::<Vec<_>>().join(" or "),
            Self::Unknown => "the known_hosts file ssh uses".to_owned(),
        }
    }

    /// The first file an option names, which is the one ssh adds keys to.
    fn first_named(&self) -> Option<&str> {
        match self {
            Self::Named(paths) => paths.split_whitespace().next(),
            Self::Default | Self::Unknown => None,
        }
    }
}

/// The value of `keyword` in one ssh option (`Keyword=value`,
/// `Keyword value`, or `Keyword = value`), or `None` for another keyword.
fn option_value<'a>(option: &'a str, keyword: &str) -> Option<&'a str> {
    let option = option.trim_start();
    let end = option
        .find(|character: char| character == '=' || character.is_whitespace())
        .unwrap_or(option.len());
    let (name, rest) = option.split_at(end);
    if !name.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = rest.trim_start();
    Some(rest.strip_prefix('=').unwrap_or(rest).trim())
}

/// Verify that a bare-SSH project path exists and has a committed Git HEAD.
pub fn validate_bare_project_directory(
    ssh: &SshTarget,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_bare_project_path(path)?;
    if !ssh_directory_exists(ssh, path, executor)? {
        return Err(anyhow::Error::new(crate::refusal::Refusal::unusable(
            format!(
                "remote project directory {} does not exist or is not a directory on {}",
                path.display(),
                ssh.destination
            ),
        )));
    }
    let output = executor.execute(&ssh_validation_command(
        ssh,
        vec![
            "git".into(),
            "-C".into(),
            path.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--verify".into(),
            "HEAD".into(),
        ],
        "validate bare SSH Git project",
    ))?;
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!(
                "remote project directory {} has no valid Git HEAD",
                path.display()
            );
        }
        bail!(
            "remote project directory {} has no valid Git HEAD: {detail}",
            path.display()
        );
    }
    Ok(())
}

pub fn validate_bare_project_path(path: &Path) -> Result<()> {
    if !crate::target_path::is_absolute(path)
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        bail!("bare project directory must be an absolute safe path");
    }
    Ok(())
}

pub fn ssh_validation_command(
    ssh: &SshTarget,
    remote_args: Vec<String>,
    purpose: &'static str,
) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ServerAliveInterval=2".into(),
        "-o".into(),
        "ServerAliveCountMax=1".into(),
    ]);
    args.extend([ssh.destination.clone(), remote_command_line(&remote_args)]);
    CommandSpec::new("ssh", args)
        .ssh_probe_session(ssh)
        .purpose(purpose)
}

/// Wrap a value so a POSIX shell reads it as one literal argument. Used at the
/// SSH boundary here and when Hel rebuilds an agent's terminal command line
/// (`terminal::shell_line`).
pub fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn verify_locator(locator: &TargetLocator, session_id: &str) -> Result<()> {
    validate_session_id(session_id)?;
    match locator {
        TargetLocator::LocalBare { worker_root } => {
            let path = Path::new(worker_root);
            if !crate::target_path::is_absolute(path)
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
                || !path.ends_with(session_id)
            {
                bail!("refusing cleanup: invalid local bare worker root");
            }
        }
        TargetLocator::LocalPodman {
            container_id,
            borrowed_from,
            ..
        }
        | TargetLocator::LocalDocker {
            container_id,
            borrowed_from,
        }
        | TargetLocator::AppleContainer {
            container_id,
            borrowed_from,
        }
        | TargetLocator::SshPodman {
            container_id,
            borrowed_from,
            ..
        }
        | TargetLocator::SshDocker {
            container_id,
            borrowed_from,
            ..
        }
        | TargetLocator::SshAppleContainer {
            container_id,
            borrowed_from,
            ..
        } => match borrowed_from {
            Some(owner) => {
                validate_session_id(owner)?;
                if owner == session_id {
                    bail!(
                        "refusing cleanup: a borrowed container cannot be owned by the borrowing session"
                    );
                }
                if !resource_name_belongs_to(container_id, owner)?
                    && !is_runtime_container_id(container_id)
                {
                    bail!(
                        "refusing cleanup: borrowed container locator is neither the owning session's generated name nor an immutable runtime ID"
                    );
                }
            }
            None => {
                if !resource_name_belongs_to(container_id, session_id)?
                    && !is_runtime_container_id(container_id)
                {
                    bail!(
                        "refusing cleanup: container locator is neither the generated name nor an immutable runtime ID"
                    );
                }
            }
        },
        TargetLocator::AwsEc2 {
            instance_id,
            workspace,
            ..
        } => {
            if !valid_ec2_instance_id(instance_id) {
                bail!("refusing cleanup: invalid EC2 instance ID");
            }
            verify_session_workspace(workspace, session_id)?;
        }
        TargetLocator::SshBare {
            workspace,
            worker_id,
            ..
        } => match worker_id {
            Some(worker_id) => {
                validate_session_id(worker_id)?;
                if worker_id != session_id {
                    bail!("refusing cleanup: SSH worker identity does not match session ID");
                }
                validate_workspace_prefix(workspace)?;
            }
            None => verify_session_workspace(workspace, session_id)?,
        },
    }
    Ok(())
}

/// Whether this locator names a target another session owns: a sub-agent
/// child either borrowing its parent's container or running as its own worker
/// inside the parent's SSH workspace.
pub fn is_borrowed(locator: &TargetLocator) -> bool {
    match locator {
        TargetLocator::LocalPodman { borrowed_from, .. }
        | TargetLocator::LocalDocker { borrowed_from, .. }
        | TargetLocator::AppleContainer { borrowed_from, .. }
        | TargetLocator::SshPodman { borrowed_from, .. }
        | TargetLocator::SshDocker { borrowed_from, .. }
        | TargetLocator::SshAppleContainer { borrowed_from, .. } => borrowed_from.is_some(),
        TargetLocator::SshBare { worker_id, .. } => worker_id.is_some(),
        TargetLocator::LocalBare { .. } | TargetLocator::AwsEc2 { .. } => false,
    }
}

pub fn verify_session_workspace(workspace: &str, session_id: &str) -> Result<()> {
    validate_workspace_prefix(workspace)?;
    let final_component = workspace.trim_end_matches('/').rsplit('/').next();
    if final_component != Some(session_id) {
        bail!("refusing cleanup: workspace does not end in the exact session ID");
    }
    Ok(())
}

pub fn validate_session_id(value: &str) -> Result<()> {
    if value.len() < 8
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("session ID must be 8-128 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

pub fn validate_relative_path(value: &str) -> Result<()> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!("unsafe relative bundle path {value:?}");
    }
    Ok(())
}

pub fn validate_workspace_prefix(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "/"
        || value == "~"
        || value == "~/"
        || value.contains('\0')
        || value.split('/').any(|part| part == "..")
    {
        bail!("unsafe workspace path");
    }
    Ok(())
}

pub fn validate_container_template(template: &ContainerTemplate) -> Result<()> {
    if template.image.trim().is_empty() || template.image.starts_with('-') {
        bail!("invalid container image");
    }
    if template
        .extra_run_args
        .iter()
        .any(|arg| arg == "--name" || arg.starts_with("--name="))
    {
        bail!("container template may not override the generated name");
    }
    if template.extra_run_args.iter().any(|arg| {
        arg == "--label"
            || [SESSION_LABEL, MANAGED_LABEL, INSTANCE_LABEL]
                .iter()
                .any(|label| arg.starts_with(&format!("--label={label}=")))
    }) {
        bail!("container template may not override Mjolnir ownership labels");
    }
    Ok(())
}

pub fn validate_ssh(ssh: &SshTarget) -> Result<()> {
    if ssh.destination.trim().is_empty()
        || ssh.destination.starts_with('-')
        || ssh.destination.chars().any(char::is_whitespace)
    {
        bail!("invalid SSH destination");
    }
    Ok(())
}

pub fn validate_aws(aws: &AwsTemplate) -> Result<()> {
    validate_ssh(&aws.ssh)?;
    for (name, value) in [
        ("AWS profile", &aws.profile),
        ("AWS region", &aws.region),
        ("launch template", &aws.launch_template),
    ] {
        if value.is_empty()
            || value.starts_with('-')
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        {
            bail!("invalid {name}");
        }
    }
    Ok(())
}

pub fn validate_executable(value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        bail!("invalid executable name");
    }
    Ok(())
}

pub fn valid_ec2_instance_id(value: &str) -> bool {
    value
        .strip_prefix("i-")
        .is_some_and(|rest| rest.len() >= 8 && rest.chars().all(|c| c.is_ascii_hexdigit()))
}

pub fn is_runtime_container_id(value: &str) -> bool {
    value.len() >= 12 && value.len() <= 128 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// `ssh` reserves exit status 255 for its own transport failures; a remote
/// command never produces it, so the remote side provably never ran.
pub const SSH_TRANSPORT_EXIT_STATUS: i32 = 255;

/// Stderr fragments OpenSSH prints when the server hangs up before
/// authentication. `sshd`'s `MaxStartups` produces exactly these when it drops
/// an unauthenticated connection, and so does a server that is still starting.
const TRANSPORT_REJECTION_MARKERS: [&str; 4] = [
    "Connection closed by",
    "Connection reset by",
    "kex_exchange_identification",
    "Connection timed out during banner exchange",
];

/// Whether a finished `ssh` process was turned away by the transport rather
/// than by the remote command.
///
/// The remote command never started in this case, so the caller may retry the
/// whole invocation without worrying about repeating a side effect.
pub fn is_transport_rejection(status: i32, stderr: &str) -> bool {
    ssh_refusal(status, stderr).is_some()
}

/// What `ssh` prints when the server refuses a new session on an existing,
/// authenticated shared connection. `sshd` does this once the connection
/// carries `MaxSessions` sessions.
const SESSION_REFUSAL_MARKER: &str = "Session open refused by peer";

/// Why the SSH server turned an `ssh` invocation away before its remote
/// command started. Either way the command never ran, so it may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshRefusal {
    /// The server dropped a new connection before authentication, which is
    /// what `MaxStartups` does, and a dead shared master looks the same to a
    /// session bound to it.
    BeforeAuthentication,
    /// The shared connection is up and authenticated, but the server refused
    /// one more session on it (`MaxSessions`).
    SessionLimit,
}

impl SshRefusal {
    /// A log message that names this refusal.
    fn retry_message(self) -> &'static str {
        match self {
            Self::BeforeAuthentication => {
                "the SSH server closed the connection before authentication; retrying"
            }
            Self::SessionLimit => {
                "the SSH server refused another session on a shared connection (MaxSessions); retrying"
            }
        }
    }

    /// Log one retry of a command this refusal turned away.
    ///
    /// A refused session on a live shared connection is routine while many
    /// sessions start at once, and the retry nearly always gets in (launch
    /// finding R3-5 counted 98 in one dashboard log, all answered), so it is
    /// logged at debug level. A connection closed before authentication can
    /// mean a master died, so it stays a warning.
    fn log_retry(
        self,
        destination: &str,
        purpose: &str,
        attempt: usize,
        delay: Duration,
        stderr: &str,
    ) {
        let delay_ms = delay.as_millis() as u64;
        match self {
            Self::SessionLimit => tracing::debug!(
                destination,
                purpose,
                attempt,
                delay_ms,
                stderr,
                "{}",
                self.retry_message()
            ),
            Self::BeforeAuthentication => tracing::warn!(
                destination,
                purpose,
                attempt,
                delay_ms,
                stderr,
                "{}",
                self.retry_message()
            ),
        }
    }

    /// Log a command the server still refused when its retries ran out.
    fn log_exhausted(self, destination: &str, purpose: &str, attempts: usize, stderr: &str) {
        tracing::warn!(
            destination,
            purpose,
            attempts,
            stderr,
            "{}",
            match self {
                Self::BeforeAuthentication =>
                    "the SSH server closed the connection before authentication on every attempt",
                Self::SessionLimit =>
                    "the SSH server refused another session on a shared connection (MaxSessions) on every attempt",
            }
        );
    }
}

/// How many times a refused command is tried in total when waiting cannot
/// help or cannot be ended from outside: a session refused on a connection
/// with no shared master to walk to (the user's own `ssh_args` configure
/// sharing), and any pre-authentication refusal for a caller with no
/// cancellation or deadline to stop the retries.
pub const SSH_BOUNDED_RETRY_ATTEMPTS: usize = 3;

/// A refusal before authentication that has lasted this long is reported as
/// a warning once, so a server that keeps dropping connections is visible
/// while Mjolnir still retries.
const SSH_PERSISTENT_REFUSAL: Duration = Duration::from_secs(60);

/// What follows each refusal of one `ssh` command: the single owner of the
/// retry decision, shared by the executors, the relay connect and the
/// resource poller.
///
/// A connection closed before authentication means `sshd` is up and asking
/// the client to come back (`MaxStartups`), and the command never ran, so a
/// caller that can be cancelled retries until it succeeds or is cancelled.
/// A refused session on a shared master retries at once: the command's
/// [`SshWalk`], which this value owns, sends the next lease to another master.
/// Everything else is bounded by [`SSH_BOUNDED_RETRY_ATTEMPTS`].
pub struct SshRefusalRetry {
    walk: SshWalk,
    cancellable: bool,
    started: Instant,
    /// Refusals that waited; indexes the backoff.
    waits: usize,
    session_limit_refusals: usize,
    warned_persistent: bool,
}

impl SshRefusalRetry {
    /// `cancellable` is whether the caller's cancellation or deadline can end
    /// the retries from outside.
    pub fn new(cancellable: bool) -> Self {
        Self {
            walk: SshWalk::default(),
            cancellable,
            started: Instant::now(),
            waits: 0,
            session_limit_refusals: 0,
            warned_persistent: false,
        }
    }

    /// The command's walk across the shards of its connection. Pass it to
    /// every lease the command takes.
    pub fn walk(&mut self) -> &mut SshWalk {
        &mut self.walk
    }

    /// The wait before the next attempt, or `None` when the caller should
    /// stop retrying and report the refusal. `walks_on` is what
    /// [`SshSessionLease::note_refusal`] returned: whether the refusal is a
    /// step of the walk to another master. This logs the retry or the
    /// exhaustion.
    pub fn next(
        &mut self,
        refusal: SshRefusal,
        walks_on: bool,
        destination: &str,
        purpose: &str,
        stderr: &str,
    ) -> Option<Duration> {
        let stderr = stderr.trim();
        if refusal == SshRefusal::BeforeAuthentication {
            self.walk.retry_last_shard();
        }
        if walks_on {
            // The walk sends the next lease to a shard this command has not
            // tried, so this refusal cannot repeat: no wait, no attempt spent.
            refusal.log_retry(destination, purpose, self.waits, Duration::ZERO, stderr);
            return Some(Duration::ZERO);
        }
        self.waits += 1;
        let bounded = match refusal {
            SshRefusal::SessionLimit => {
                self.session_limit_refusals += 1;
                self.session_limit_refusals >= SSH_BOUNDED_RETRY_ATTEMPTS
            }
            SshRefusal::BeforeAuthentication => {
                !self.cancellable && self.waits >= SSH_BOUNDED_RETRY_ATTEMPTS
            }
        };
        if bounded {
            refusal.log_exhausted(destination, purpose, self.waits, stderr);
            return None;
        }
        let delay = ssh_retry_delay(self.waits);
        refusal.log_retry(destination, purpose, self.waits, delay, stderr);
        if refusal == SshRefusal::BeforeAuthentication
            && !self.warned_persistent
            && self.started.elapsed() >= SSH_PERSISTENT_REFUSAL
        {
            self.warned_persistent = true;
            tracing::warn!(
                destination,
                purpose,
                attempts = self.waits,
                elapsed_s = self.started.elapsed().as_secs(),
                stderr,
                "the SSH server has refused connections before authentication for over a minute; still retrying"
            );
        }
        Some(delay)
    }
}

/// Classify a finished `ssh` process that the server turned away. A refused
/// session is checked first: a session bound to its master with
/// `ProxyCommand=false` also reports a closed connection after the refusal.
pub fn ssh_refusal(status: i32, stderr: &str) -> Option<SshRefusal> {
    if status != SSH_TRANSPORT_EXIT_STATUS {
        return None;
    }
    ssh_refusal_in_stderr(stderr)
}

/// [`ssh_refusal`] for a command that runs `ssh` as its transport and exits
/// with a status of its own (`rsync` exits 12 when its transport closes), so
/// only the `ssh` messages it relays on stderr can tell a refusal.
pub fn ssh_refusal_in_stderr(stderr: &str) -> Option<SshRefusal> {
    if stderr.contains(SESSION_REFUSAL_MARKER) {
        return Some(SshRefusal::SessionLimit);
    }
    TRANSPORT_REJECTION_MARKERS
        .iter()
        .any(|marker| stderr.contains(marker))
        .then_some(SshRefusal::BeforeAuthentication)
}

/// The first line every remote command mj sends over `ssh` prints on stdout,
/// before the command itself runs (see [`with_start_marker`]).
///
/// Stdout is empty exactly when the session never got as far as running the
/// command's shell: a server that refuses a session or drops a connection
/// before authentication prints nothing there. So a session that ended with
/// no stdout byte at all provably never started and is always safe to
/// retry, and one that printed even a byte is a real result, whatever its
/// exit status. The ledger also counts a session as open only from this byte.
pub const SSH_START_MARKER: &str = "MJOLNIR-SSH-SESSION-STARTED\n";

/// The remote command line with the start marker printed first. `;` joins them
/// because callers prepend compound statements to the line (`read`, `export`,
/// `exec`), and it is valid in sh, bash, zsh, fish and csh alike.
pub(crate) fn with_start_marker(remote_command: &str) -> String {
    format!(
        "printf '%s\\n' '{}'; {remote_command}",
        SSH_START_MARKER.trim_end()
    )
}

/// Removes [`SSH_START_MARKER`] from the front of a session's stdout and tells
/// when the session has started.
///
/// The first stdout byte of any kind proves the session started. A stream
/// that begins with the marker has it removed. A stream that does not (a
/// login shell's rc file printing first, or a forced command) passes through
/// unchanged and still counts as started. A marker split across reads is held
/// until the next chunk decides which of the two it is.
pub struct StartScan {
    /// The stream so far: still a prefix of the marker, or decided.
    state: ScanState,
    /// Bytes held while they could still be the start of the marker.
    held: Vec<u8>,
    started: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Matching,
    Passing,
}

impl Default for StartScan {
    fn default() -> Self {
        Self::new()
    }
}

impl StartScan {
    pub fn new() -> Self {
        Self {
            state: ScanState::Matching,
            held: Vec::new(),
            started: false,
        }
    }

    /// Feed one chunk. The bytes to pass on are appended to `out`. Returns
    /// `true` the first time a byte arrives, which is the session's start.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        if chunk.is_empty() {
            return false;
        }
        let first = !std::mem::replace(&mut self.started, true);
        match self.state {
            ScanState::Passing => out.extend_from_slice(chunk),
            ScanState::Matching => {
                self.held.extend_from_slice(chunk);
                let marker = SSH_START_MARKER.as_bytes();
                let compared = self.held.len().min(marker.len());
                if self.held[..compared] != marker[..compared] {
                    self.state = ScanState::Passing;
                    out.append(&mut self.held);
                } else if self.held.len() >= marker.len() {
                    self.state = ScanState::Passing;
                    out.extend_from_slice(&self.held[marker.len()..]);
                    self.held.clear();
                }
            }
        }
        first
    }

    /// The stream ended. Bytes held as a possible marker were ordinary output.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
        self.state = ScanState::Passing;
    }
}

/// Where one SSH session stands: Waiting until its stdout shows a byte,
/// Started after that, Ended when stdout closed without any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPhase {
    Waiting,
    Started,
    Ended,
}

/// How a blocking wait on a [`StartGate`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartWait {
    Started,
    Ended,
    Cancelled,
}

/// The owner of the fact "this SSH session started its command", one per
/// lease. The reader of the session's stdout reports into it; the writer of its
/// stdin, the retry decision and the ledger ask it.
pub struct StartGate {
    phase: Mutex<StartPhase>,
    changed: std::sync::Condvar,
    watch: tokio::sync::watch::Sender<StartPhase>,
    confirm: Option<SessionConfirm>,
}

impl std::fmt::Debug for StartGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartGate")
            .field("phase", &self.phase())
            .finish()
    }
}

impl StartGate {
    /// A gate no lease owns: the start is known, but no ledger counts it.
    pub fn standalone() -> Arc<Self> {
        Self::new(None)
    }

    pub(crate) fn new(confirm: Option<SessionConfirm>) -> Arc<Self> {
        Arc::new(Self {
            phase: Mutex::new(StartPhase::Waiting),
            changed: std::sync::Condvar::new(),
            watch: tokio::sync::watch::channel(StartPhase::Waiting).0,
            confirm,
        })
    }

    pub fn phase(&self) -> StartPhase {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn settle(&self, next: StartPhase) -> bool {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *phase != StartPhase::Waiting {
            return false;
        }
        *phase = next;
        self.watch.send_replace(next);
        self.changed.notify_all();
        true
    }

    /// The session's stdout produced its first byte: the command's shell ran.
    /// The lease becomes an open session in the ledger.
    pub fn mark_started(&self) {
        if self.settle(StartPhase::Started)
            && let Some(confirm) = &self.confirm
        {
            confirm.confirm();
        }
    }

    /// The session's stdout closed. Without an earlier byte, the session never
    /// started.
    pub fn mark_ended(&self) {
        self.settle(StartPhase::Ended);
    }

    /// Wait until the session has started or ended, polling `cancelled`.
    pub fn wait_blocking(&self, cancelled: &dyn Fn() -> bool) -> StartWait {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match *phase {
                StartPhase::Started => return StartWait::Started,
                StartPhase::Ended => return StartWait::Ended,
                StartPhase::Waiting => {}
            }
            if cancelled() {
                return StartWait::Cancelled;
            }
            phase = self
                .changed
                .wait_timeout(phase, Duration::from_millis(25))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Wait until the session has started (`true`) or ended without starting.
    pub async fn wait(&self) -> bool {
        let mut watch = self.watch.subscribe();
        match watch.wait_for(|phase| *phase != StartPhase::Waiting).await {
            Ok(phase) => *phase == StartPhase::Started,
            Err(_) => false,
        }
    }

    /// Whether the server turned the session away. A session that printed a
    /// byte started, so its exit status is its own whatever stderr says. One
    /// that printed nothing, including one that never carried the marker
    /// (`scp`), is judged by the exit status and stderr alone.
    pub fn refusal(&self, status: i32, stderr: &str) -> Option<SshRefusal> {
        if self.phase() == StartPhase::Started {
            return None;
        }
        ssh_refusal(status, stderr)
    }
}

/// How long a successful `ssh -O check` of a master is trusted before the
/// next lease on that shard checks again.
#[cfg(unix)]
const MASTER_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Per-command deadline for checking or opening a master from code that has
/// no executor of its own, such as the relay and the resource pollers.
pub const SSH_MASTER_OPEN_TIMEOUT: Duration = Duration::from_secs(60);

/// One command's walk across the shards of a connection.
///
/// The server decides how many sessions a master accepts, and other
/// processes share the same control sockets, so no count kept here can say
/// whether a shard has room. A command instead tries shards in order: when
/// the server refuses its session on one (`MaxSessions`), the command's next
/// lease takes the next shard it has not tried, and only when it has tried
/// every shard that exists does it open a new one. The walk is the state
/// change that ends the retry loop, so a refusal needs no wait and spends no
/// attempt.
///
/// One command owns one walk (see [`SshRefusalRetry::walk`]) and passes it to
/// each lease it takes.
#[derive(Debug, Default)]
pub struct SshWalk {
    /// Where the walk started: the shard that most recently started a session
    /// for the connection. Only a starting point; a stale one costs extra
    /// refusals and never a failure.
    #[cfg(unix)]
    start: Option<usize>,
    /// The shards this command has already leased, whether or not they
    /// accepted it.
    #[cfg(unix)]
    visited: Vec<usize>,
}

impl SshWalk {
    /// The command's last lease did not say anything about its shard being
    /// full (a connection dropped before authentication usually means the
    /// master died), so the retry takes that shard again; its lease checks the
    /// master and reopens it.
    fn retry_last_shard(&mut self) {
        #[cfg(unix)]
        self.visited.pop();
    }
}

/// One master connection this process knows about: when its master was last
/// found alive. How many sessions it carries is the server's to say.
#[cfg(unix)]
struct Shard {
    /// When `ssh -O check` last found this shard's master running.
    verified_at: Option<Instant>,
    /// Serializes checking and opening this shard's master, so concurrent
    /// leases never start two openers for one socket.
    opening: Arc<Mutex<()>>,
}

#[cfg(unix)]
impl Shard {
    fn new() -> Self {
        Self {
            verified_at: None,
            opening: Arc::new(Mutex::new(())),
        }
    }
}

/// The shards of one connection and where walks start.
#[cfg(unix)]
#[derive(Default)]
struct Connection {
    shards: Vec<Shard>,
    /// The shard that most recently started a session.
    hint: usize,
}

/// A shard a lease was placed on.
#[cfg(unix)]
struct Reservation {
    shard: usize,
    opening: Arc<Mutex<()>>,
}

/// This process's view of the shards of each connection, keyed by
/// connection. It records which shards exist and whether their masters are
/// known to be alive, nothing about how full they are.
#[cfg(unix)]
struct SessionLedger {
    connections: Mutex<BTreeMap<String, Connection>>,
}

#[cfg(unix)]
impl SessionLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(BTreeMap::new()),
        })
    }

    fn global() -> Arc<Self> {
        static LEDGER: OnceLock<Arc<SessionLedger>> = OnceLock::new();
        Arc::clone(LEDGER.get_or_init(Self::new))
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Choose the shard for `walk`'s next lease: the first shard the walk has
    /// not tried, in order from the walk's start and wrapping, or a new shard
    /// when it has tried them all. This is the only placement rule, and the
    /// ledger's lock makes it one decision.
    fn reserve(&self, key: &str, walk: &mut SshWalk) -> Reservation {
        let mut connections = self.connections();
        let connection = connections.entry(key.to_owned()).or_default();
        let count = connection.shards.len();
        let start = *walk.start.get_or_insert(connection.hint);
        let index = (0..count)
            .map(|offset| (start + offset) % count)
            .find(|shard| !walk.visited.contains(shard))
            .unwrap_or_else(|| {
                connection.shards.push(Shard::new());
                count
            });
        walk.visited.push(index);
        Reservation {
            shard: index,
            opening: Arc::clone(&connection.shards[index].opening),
        }
    }

    fn slot(
        self: &Arc<Self>,
        key: String,
        reservation: &Reservation,
        socket: PathBuf,
    ) -> LeasedSlot {
        LeasedSlot {
            ledger: Arc::clone(self),
            key,
            shard: reservation.shard,
            socket,
        }
    }

    /// Lease a session on `walk`'s next shard, opening that shard's master
    /// first when it is not known to be running.
    fn lease(
        self: &Arc<Self>,
        ssh: &SshTarget,
        dir: &Path,
        executor: &dyn CommandExecutor,
        walk: &mut SshWalk,
    ) -> Result<SshSessionLease> {
        let key = connection_key(ssh);
        let reservation = self.reserve(&key, walk);
        let slot = self.slot(
            key,
            &reservation,
            dir.join(control_socket_name(ssh, reservation.shard)),
        );
        if slot.needs_check() {
            let _opening = loop {
                ensure!(
                    !executor.cancellation_requested(),
                    "operation cancelled while waiting for SSH master"
                );
                match reservation.opening.try_lock() {
                    Ok(guard) => break guard,
                    Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            };
            // Another lease may have checked or opened the master while this
            // one waited for the lock.
            if slot.needs_check() {
                ensure_master(ssh, &slot.socket, executor)?;
                slot.set_verified(Some(Instant::now()));
            }
        }
        Ok(SshSessionLease::new(Some(slot), false))
    }

    /// Place a fail-fast probe on `walk`'s next shard, without checking or
    /// opening that shard's master.
    ///
    /// The probe joins the master when it is up and otherwise connects on
    /// its own, so its short timeouts never become a master's settings. It
    /// uses one of the master's sessions while it runs, so the server may
    /// refuse it like any other session; the walk then moves it on. Past the
    /// last known shard it takes the next index, which joins that shard's
    /// master if some process opened it and otherwise connects directly.
    fn lease_probe(
        self: &Arc<Self>,
        ssh: &SshTarget,
        dir: &Path,
        walk: &mut SshWalk,
    ) -> SshSessionLease {
        let key = connection_key(ssh);
        let reservation = self.reserve(&key, walk);
        let socket = dir.join(control_socket_name(ssh, reservation.shard));
        SshSessionLease::new(Some(self.slot(key, &reservation, socket)), true)
    }
}

/// Make sure a master is listening on `socket`, opening one if needed.
///
/// The master is opened explicitly, with `ControlMaster=yes`, and then
/// checked again. Nothing else is attempted when that fails: the caller gets
/// an error naming the destination instead of a direct connection. A master
/// opened here must also accept a session before anyone relies on it (see
/// [`check_first_session`]).
#[cfg(unix)]
fn ensure_master(ssh: &SshTarget, socket: &Path, executor: &dyn CommandExecutor) -> Result<()> {
    // Another process of this instance (the old daemon during a restart)
    // may be checking and opening the same socket. Without this lock both
    // find no master and both open one; the second finds the socket bound,
    // prints "already exists, disabling multiplexing", and keeps a plain
    // background connection that no ControlPersist ever closes (J-18). The
    // lock also keeps one process from removing, as stale, a socket the
    // other has just bound.
    let _opening = lock_master_opening_unless(socket, &|| executor.cancellation_requested())?;
    if master_running(ssh, socket, executor)? {
        return Ok(());
    }
    // A master that died without cleaning up leaves its socket behind, and
    // `ssh` will not bind over it: the opener would print "already exists,
    // disabling multiplexing" and hold a plain connection instead.
    match fs::remove_file(socket) {
        Ok(()) => tracing::debug!(
            socket = %socket.display(),
            "removed a stale SSH control socket"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove stale SSH control socket {}", socket.display()));
        }
    }
    let opened = executor.execute(&master_open_command(ssh, socket))?;
    if master_running(ssh, socket, executor)? {
        tracing::info!(
            destination = ssh.destination.as_str(),
            socket = %socket.display(),
            "opened a shared SSH connection"
        );
        return check_first_session(ssh, socket, executor);
    }
    let stderr = String::from_utf8_lossy(&opened.stderr);
    let detail = match stderr.trim() {
        "" => format!("ssh exited with status {}", opened.status),
        stderr => stderr.to_owned(),
    };
    bail!(
        "could not open a shared SSH connection to {}: {detail}",
        ssh.destination
    )
}

/// Run one session on the master this process has just opened, before any
/// other lease can use it.
///
/// A server that does not allow shared sessions (`MaxSessions 0`) refuses
/// every session on every master, so walking the shards would open masters
/// forever. A refusal of the first session tells that case apart from a
/// master that other sessions have filled: nothing else can be running on a
/// master nobody has been able to use yet, because the callers of this
/// function hold the master's opening locks. (A refusal of a session among
/// many proves nothing: in a burst the refusals arrive before the start
/// markers of the sessions the server accepted.) The master is closed again,
/// so the next lease checks it afresh and fails the same way, and the error
/// names the host and the setting. Any other outcome is left to the
/// command that follows.
#[cfg(unix)]
fn check_first_session(
    ssh: &SshTarget,
    socket: &Path,
    executor: &dyn CommandExecutor,
) -> Result<()> {
    let mut command = ssh_command_owned(ssh, vec!["true".to_owned()]);
    command.args = session_command_args_on(&command.program, &command.args, ssh, socket, false);
    command.ssh_session = None;
    command.ssh_destination = None;
    command.purpose = "check that a new shared SSH connection accepts sessions".to_owned();
    let output = executor.execute(&command)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if ssh_refusal(output.status, &stderr) != Some(SshRefusal::SessionLimit) {
        return Ok(());
    }
    // Best effort: a master that cannot be closed still exits on its own once
    // ControlPersist runs out.
    if let Err(error) = executor.execute(&master_exit_command(ssh, socket)) {
        tracing::debug!(%error, socket = %socket.display(), "cannot close the unusable SSH master");
    }
    bail!(
        "the SSH server at {destination} refused the first session on a shared connection \
         that Mjolnir had just opened, so it does not allow shared sessions \
         (MaxSessions 0 in its sshd_config): {detail}. Set MaxSessions to 1 or more in \
         sshd_config on that host, or set {CONTROL_MASTER_ENV}=off to give every command \
         its own connection",
        destination = ssh.destination,
        detail = stderr.trim(),
    )
}

/// Take the file lock that serializes checking and opening the master on
/// `socket` across processes. It is held until the returned file is dropped.
/// The lock file sits beside the socket as `<socket>.lock`; `ssh` binds a
/// new master at `<socket>.<16 random characters>`, so the names never meet.
#[cfg(all(unix, test))]
fn lock_master_opening(socket: &Path) -> Result<crate::file_lock::FileLock> {
    lock_master_opening_unless(socket, &|| false)
}

#[cfg(unix)]
fn lock_master_opening_unless(
    socket: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<crate::file_lock::FileLock> {
    let mut path = socket.as_os_str().to_owned();
    path.push(".lock");
    let path = PathBuf::from(path);
    loop {
        ensure!(
            !cancelled(),
            "operation cancelled while waiting for SSH master lock"
        );
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open SSH master lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("lock SSH master lock {}", path.display()));
            }
        }
        let file = crate::file_lock::FileLock::new(file);
        // The daemon removes stale locks when it starts. A lock taken on a
        // file removed meanwhile serializes nothing, so take the one at the
        // path now.
        if is_file_at(file.file(), &path) {
            return Ok(file);
        }
    }
}

/// Remove the master lock files in `dir` whose master is gone.
///
/// A lock outlives the master it guarded: ssh removes its socket when the
/// master exits, but nothing removed `<socket>.lock` (launch finding R3-11).
/// A lock is removed only while its socket is absent and no other process
/// holds it, and only if the file locked is still the one at its path.
/// [`lock_master_opening`] checks the same after it locks, so an opener that
/// opened the file just before it was removed takes a fresh one instead.
#[cfg(unix)]
fn remove_stale_master_locks_in(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(directory = %dir.display(), %error, "cannot list SSH master locks");
            return;
        }
    };
    for entry in entries.flatten() {
        let lock = entry.path();
        let Some(socket) = lock
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|name| name.strip_suffix(".lock"))
            .map(|name| dir.join(name))
        else {
            continue;
        };
        if fs::symlink_metadata(&socket).is_ok() {
            continue;
        }
        let Ok(file) = fs::OpenOptions::new().write(true).open(&lock) else {
            continue;
        };
        // Held: another process is checking or opening this master now.
        if file.try_lock().is_err() {
            continue;
        }
        let file = crate::file_lock::FileLock::new(file);
        if fs::symlink_metadata(&socket).is_ok() || !is_file_at(file.file(), &lock) {
            continue;
        }
        match fs::remove_file(&lock) {
            Ok(()) => tracing::debug!(lock = %lock.display(), "removed a stale SSH master lock"),
            Err(error) => {
                tracing::debug!(lock = %lock.display(), %error, "cannot remove a stale SSH master lock")
            }
        }
    }
}

/// Whether `file` is the file now at `path`, rather than one removed from it.
#[cfg(unix)]
fn is_file_at(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
        _ => false,
    }
}

#[cfg(unix)]
fn master_running(ssh: &SshTarget, socket: &Path, executor: &dyn CommandExecutor) -> Result<bool> {
    Ok(executor.execute(&master_check_command(ssh, socket))?.status == 0)
}

/// `ssh -O check` asks the master on `socket` whether it is alive. It opens
/// no network connection, so a server cannot refuse it like one.
#[cfg(unix)]
fn master_check_command(ssh: &SshTarget, socket: &Path) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".to_owned(),
        control_path_option(socket),
        "-O".to_owned(),
        "check".to_owned(),
        ssh.destination.clone(),
    ]);
    CommandSpec::new("ssh", args).purpose("check a shared SSH connection")
}

/// `ssh -O exit` asks the master on `socket` to close.
#[cfg(unix)]
fn master_exit_command(ssh: &SshTarget, socket: &Path) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".to_owned(),
        control_path_option(socket),
        "-O".to_owned(),
        "exit".to_owned(),
        ssh.destination.clone(),
    ]);
    CommandSpec::new("ssh", args).purpose("close an unusable shared SSH connection")
}

/// Open a master on `socket` and return once it is authenticated.
///
/// `-f -N` backgrounds the master after authentication without keeping the
/// caller's output pipes open, and `ControlPersist` stops it on its own once
/// its last session has been gone that long. `BatchMode=yes` keeps the daemon
/// from ever waiting on a password prompt. This is a real connection, so a
/// server can refuse it before authentication and it is retried like one.
///
/// Without a keepalive the master never notices a host that dropped off the
/// network: every multiplexed session on it, a destroy's container removal
/// included, waits until the host comes back and resets the connection.
/// [`MASTER_KEEPALIVE`] makes the master give up on a silent host within
/// about a minute, which fails each of its sessions with status 255. The
/// user's own `ssh_args` come first and so still win.
#[cfg(unix)]
fn master_open_command(ssh: &SshTarget, socket: &Path) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    args.extend([
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "ConnectTimeout=10".to_owned(),
        "-o".to_owned(),
        format!("ServerAliveInterval={}", MASTER_KEEPALIVE.0),
        "-o".to_owned(),
        format!("ServerAliveCountMax={}", MASTER_KEEPALIVE.1),
        "-f".to_owned(),
        "-N".to_owned(),
        "-o".to_owned(),
        "ControlMaster=yes".to_owned(),
        "-o".to_owned(),
        control_path_option(socket),
        "-o".to_owned(),
        format!("ControlPersist={CONTROL_PERSIST}"),
        ssh.destination.clone(),
    ]);
    CommandSpec::new("ssh", args)
        .ssh_destination(ssh.destination.clone())
        .purpose("open a shared SSH connection")
}

/// The shard a lease was placed on.
#[cfg(unix)]
struct LeasedSlot {
    ledger: Arc<SessionLedger>,
    key: String,
    shard: usize,
    socket: PathBuf,
}

/// Lets the reader of a session's stdout tell the ledger that the session
/// started, from any thread and after the lease itself has moved or gone.
#[cfg(unix)]
pub(crate) struct SessionConfirm {
    ledger: Arc<SessionLedger>,
    key: String,
    shard: usize,
}

/// Stands in on platforms without connection sharing, where no lease counts.
#[cfg(not(unix))]
pub(crate) struct SessionConfirm;

#[cfg(unix)]
impl SessionConfirm {
    /// The session started, so its shard accepted it: the next walk for this
    /// connection starts there.
    pub(crate) fn confirm(&self) {
        if let Some(connection) = self.ledger.connections().get_mut(&self.key) {
            connection.hint = self.shard;
        }
    }
}

#[cfg(not(unix))]
impl SessionConfirm {
    pub(crate) fn confirm(&self) {}
}

#[cfg(unix)]
impl LeasedSlot {
    fn needs_check(&self) -> bool {
        let connections = self.ledger.connections();
        connections
            .get(&self.key)
            .and_then(|connection| connection.shards.get(self.shard))
            .is_none_or(|shard| {
                shard
                    .verified_at
                    .is_none_or(|verified| verified.elapsed() >= MASTER_CHECK_INTERVAL)
            })
    }

    fn set_verified(&self, verified_at: Option<Instant>) {
        let mut connections = self.ledger.connections();
        if let Some(shard) = connections
            .get_mut(&self.key)
            .and_then(|connection| connection.shards.get_mut(self.shard))
        {
            shard.verified_at = verified_at;
        }
    }

    fn confirm_handle(&self) -> SessionConfirm {
        SessionConfirm {
            ledger: Arc::clone(&self.ledger),
            key: self.key.clone(),
            shard: self.shard,
        }
    }
}

/// A lease of one SSH session on one shard of a shared connection.
///
/// A lease without a socket stands for a command that runs on its own
/// connection: sharing is switched off with `MJ_SSH_CONTROL_MASTER`, the
/// user's `ssh_args` configure sharing themselves, or the platform has no
/// connection sharing.
pub struct SshSessionLease {
    #[cfg(unix)]
    slot: Option<LeasedSlot>,
    /// A counted fail-fast probe: it joins the master when one is up and
    /// otherwise connects directly, so it carries no `ProxyCommand=false`.
    #[cfg(unix)]
    probe: bool,
    gate: Arc<StartGate>,
}

impl std::fmt::Debug for SshSessionLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshSessionLease")
            .field("control_path", &self.control_path())
            .finish()
    }
}

impl SshSessionLease {
    #[cfg(unix)]
    fn new(slot: Option<LeasedSlot>, probe: bool) -> Self {
        let confirm = slot.as_ref().map(LeasedSlot::confirm_handle);
        Self {
            slot,
            probe,
            gate: StartGate::new(confirm),
        }
    }

    fn unshared() -> Self {
        Self {
            #[cfg(unix)]
            slot: None,
            #[cfg(unix)]
            probe: false,
            gate: StartGate::new(None),
        }
    }

    /// Where this session stands: the owner of whether its command started.
    /// The reader of the session's stdout feeds it, and everything that asks
    /// whether the session was refused or has started asks it.
    pub fn gate(&self) -> &Arc<StartGate> {
        &self.gate
    }

    /// The control socket this session must use, or `None` when the command
    /// runs on its own connection.
    pub fn control_path(&self) -> Option<&Path> {
        #[cfg(unix)]
        return self.slot.as_ref().map(|slot| slot.socket.as_path());
        #[cfg(not(unix))]
        None
    }

    /// Forget that this lease's master was verified, so the next lease on the
    /// shard checks it and reopens it if needed. Call this when a session
    /// failed in a way that suggests the master is gone.
    pub fn invalidate(&self) {
        #[cfg(unix)]
        if let Some(slot) = &self.slot {
            slot.set_verified(None);
        }
    }

    /// Tell the ledger what the server's refusal of this session means for
    /// its master. This is the one place that decides it.
    ///
    /// A connection closed before authentication suggests the master is gone,
    /// so the next lease checks it. A refused session found its master alive
    /// and the server unwilling to carry another session on it. The ledger
    /// keeps nothing about that, because the server owns that fact: the
    /// command's [`SshWalk`] already lists this shard, so its next lease lands
    /// on another one.
    ///
    /// Returns `true` when retrying at once is a step of the walk (a refused
    /// session on a shared master). A connection dropped before
    /// authentication, or a refusal for a lease with no shared master, returns
    /// `false`: the retry has to wait and count as an attempt.
    pub fn note_refusal(&self, refusal: SshRefusal) -> bool {
        match refusal {
            SshRefusal::BeforeAuthentication => {
                self.invalidate();
                false
            }
            SshRefusal::SessionLimit => self.control_path().is_some(),
        }
    }
}

/// Process-wide placement of `ssh` sessions on shared connections.
///
/// An `sshd` limits the sessions on one connection (`MaxSessions`, 10 by
/// default). The daemon therefore spreads its sessions for one connection
/// across several masters (shards), each opened explicitly, and every other
/// command joins one of them with options that make a direct connection
/// impossible. The limit is the server's: a command that is refused walks to
/// the next shard (see [`SshWalk`]).
pub struct SshSessions;

impl SshSessions {
    /// Lease a session for `ssh` on the next shard of `walk`, opening that
    /// shard's master if it is not running. Uses a blocking lock, so it is
    /// callable from plain threads and `spawn_blocking`. Returns an error only
    /// when a master could not be opened or verified, or when the server does
    /// not allow shared sessions at all (see [`check_first_session`]).
    pub fn lease(
        ssh: &SshTarget,
        executor: &dyn CommandExecutor,
        walk: &mut SshWalk,
    ) -> Result<SshSessionLease> {
        #[cfg(unix)]
        {
            if user_configures_sharing(&ssh.ssh_args) {
                return Ok(SshSessionLease::unshared());
            }
            let Some(dir) = control_socket_dir() else {
                return Ok(SshSessionLease::unshared());
            };
            SessionLedger::global().lease(ssh, &dir, executor, walk)
        }
        #[cfg(not(unix))]
        {
            let _ = (ssh, executor, walk);
            Ok(SshSessionLease::unshared())
        }
    }

    /// Remove this instance's master lock files whose master has exited.
    /// The daemon calls this when it starts; failures are only logged.
    pub fn remove_stale_master_locks() {
        #[cfg(unix)]
        if let Some(dir) = control_socket_dir() {
            remove_stale_master_locks_in(&dir);
        }
    }

    /// Lease a fail-fast probe, such as a target validation or the
    /// connectivity check, on the next shard of `walk` without opening a
    /// master. See [`CommandSpec::ssh_probe_session`].
    ///
    /// A process other than the daemon (`mj doctor`) has an empty ledger, so
    /// its probe starts at shard 0 and finds the daemon's master by its
    /// socket name. If that master is full, the server refuses the probe and
    /// the walk moves it to the next shard's socket; where no master exists
    /// the probe connects on its own, as probes always do.
    pub fn lease_probe(ssh: &SshTarget, walk: &mut SshWalk) -> SshSessionLease {
        #[cfg(unix)]
        {
            if user_configures_sharing(&ssh.ssh_args) {
                return SshSessionLease::unshared();
            }
            let Some(dir) = control_socket_dir() else {
                return SshSessionLease::unshared();
            };
            SessionLedger::global().lease_probe(ssh, &dir, walk)
        }
        #[cfg(not(unix))]
        {
            let _ = (ssh, walk);
            SshSessionLease::unshared()
        }
    }
}

/// Options for a command that runs as one session on an already open
/// master. `ProxyCommand=false` makes it impossible for `ssh` to open a
/// direct connection: a multiplexed client never runs the proxy command, and
/// a client that fails to reach the master exits 255 instead of connecting on
/// its own. Appends nothing for a lease without a socket.
///
/// A probe lease gets only `ControlMaster=no` and the `ControlPath`: it joins
/// the master when one is up and otherwise connects on its own.
pub fn push_session_args(args: &mut Vec<String>, lease: &SshSessionLease) {
    if let Some(socket) = lease.control_path() {
        #[cfg(unix)]
        let probe = lease.probe;
        #[cfg(not(unix))]
        let probe = false;
        push_session_options(args, socket, probe);
    }
}

fn push_session_options(args: &mut Vec<String>, socket: &Path, probe: bool) {
    args.extend([
        "-o".to_owned(),
        "ControlMaster=no".to_owned(),
        "-o".to_owned(),
        control_path_option(socket),
    ]);
    if !probe {
        args.extend(["-o".to_owned(), "ProxyCommand=false".to_owned()]);
    }
}

/// The argument list for `program` (`ssh` or `scp`) running as one session
/// on `lease`'s master: the session options first, then the command's own
/// arguments. See [`session_command_args_on`].
pub fn session_command_args(
    program: &str,
    args: &[String],
    ssh: &SshTarget,
    lease: &SshSessionLease,
) -> Vec<String> {
    let mut session = Vec::with_capacity(args.len() + 6);
    push_session_args(&mut session, lease);
    if session.is_empty() {
        return args.to_vec();
    }
    append_session_command(&mut session, program, args, ssh);
    session
}

/// [`session_command_args`] for a session on the master at `socket` that is
/// not a lease.
#[cfg(unix)]
fn session_command_args_on(
    program: &str,
    args: &[String],
    ssh: &SshTarget,
    socket: &Path,
    probe: bool,
) -> Vec<String> {
    let mut session = Vec::with_capacity(args.len() + 6);
    push_session_options(&mut session, socket, probe);
    append_session_command(&mut session, program, args, ssh);
    session
}

/// Append the command's own arguments after the session options.
///
/// OpenSSH keeps the first value it sees for an option, so leading with the
/// session options makes them win over the user's `ssh_args` and
/// `ssh_config`, including any `ProxyCommand` or `ProxyJump`. `ssh` refuses
/// the `-J` flag after a `ProxyCommand` outright, so a `-J` in the user's own
/// arguments is rewritten to the equivalent `-o ProxyJump=`, which the
/// session's `ProxyCommand=false` then overrides; a session never connects
/// by itself, and the master was opened with the user's jump host. `scp`
/// already passes its `-J` on as `-oProxyJump=`.
fn append_session_command(
    session: &mut Vec<String>,
    program: &str,
    args: &[String],
    ssh: &SshTarget,
) {
    if program == "ssh" && args.starts_with(&ssh.ssh_args) {
        let (user, rest) = args.split_at(ssh.ssh_args.len());
        let mut user = user.iter();
        while let Some(argument) = user.next() {
            match argument.strip_prefix("-J") {
                Some("") => match user.next() {
                    Some(jump) => session.extend(["-o".to_owned(), format!("ProxyJump={jump}")]),
                    None => session.push(argument.clone()),
                },
                Some(jump) => session.extend(["-o".to_owned(), format!("ProxyJump={jump}")]),
                None => session.push(argument.clone()),
            }
        }
        session.extend(rest.iter().cloned());
    } else {
        session.extend(args.iter().cloned());
    }
}

/// Inclusive millisecond bounds the jittered wait is drawn from, indexed by
/// the number of refusals already waited out; the last row repeats for every
/// later refusal. This is the shape of the backoff, not a limit on attempts.
/// `sshd` sheds load for as long as its pre-auth queue stays full, so the
/// second wait is a multiple of the first.
const SSH_RETRY_BACKOFF_MS: [(u64, u64); 2] = [(500, 2_000), (2_000, 4_000)];

/// Test override collapsing every retry delay to this many milliseconds.
/// `u64::MAX` means "no override".
static SSH_RETRY_BACKOFF_OVERRIDE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Shorten the retry backoff so tests can drive the retry path without
/// sleeping for seconds. Not part of the daemon's behaviour.
#[doc(hidden)]
pub fn set_ssh_retry_backoff_for_test(delay: Option<Duration>) {
    SSH_RETRY_BACKOFF_OVERRIDE_MS.store(
        delay.map_or(u64::MAX, |delay| delay.as_millis() as u64),
        Ordering::Relaxed,
    );
}

/// The jittered wait before retry number `attempts_made + 1`.
///
/// Jitter matters more than the mean here: every session's reconnect fails at
/// the same instant, so an unjittered schedule would simply re-send the whole
/// burst into the same full queue.
pub fn ssh_retry_delay(attempts_made: usize) -> Duration {
    let override_ms = SSH_RETRY_BACKOFF_OVERRIDE_MS.load(Ordering::Relaxed);
    if override_ms != u64::MAX {
        return Duration::from_millis(override_ms);
    }
    let (low, high) = SSH_RETRY_BACKOFF_MS
        .get(attempts_made.saturating_sub(1))
        .copied()
        .unwrap_or(*SSH_RETRY_BACKOFF_MS.last().expect("non-empty schedule"));
    let mut bytes = [0_u8; 8];
    // A failed draw only costs jitter, so fall back to the lower bound.
    let spread = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes) % (high - low + 1)
    } else {
        0
    };
    Duration::from_millis(low + spread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_podman_exec_keeps_the_container_configured_user() {
        let command = ssh_command_owned(
            &plain_target("podman-root-test"),
            vec![
                "podman".into(),
                "exec".into(),
                "--detach".into(),
                "session".into(),
                "true".into(),
            ],
        );
        assert!(
            command
                .args
                .last()
                .unwrap()
                .contains("'podman' 'exec' '--detach' 'session' 'true'"),
            "{:?}",
            command.args
        );
    }

    /// An OpenSSH command session on macOS starts with only the base system
    /// directories on PATH, so every remote line must bring in /usr/local/bin
    /// (Apple's `container`, Docker Desktop) and /opt/homebrew/bin (Homebrew's
    /// `docker` and `podman`) by itself, after the directories the session
    /// already had. Another host runs the command with its PATH unchanged.
    #[cfg(unix)]
    // Hard-won: f6b68bcc9: SSH Apple container probes ran `container` without /usr/local/bin and exited 127.
    #[test]
    fn remote_lines_run_under_the_macos_system_path() {
        let session = "/usr/bin:/bin:/usr/sbin:/sbin";
        let probe = remote_command_line(&[
            "sh".into(),
            "-c".into(),
            "printf '%s|%s' \"$*\" \"$PATH\"".into(),
            "probe".into(),
            "two words".into(),
        ]);
        let mut command = CommandSpec::new("/bin/sh", ["-c", probe.as_str()]);
        command.clear_env = true;
        command.env.insert("PATH".into(), session.into());
        let output = ProcessExecutor.execute(&command).unwrap();
        assert_eq!(output.status, 0, "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let (args, path) = stdout.split_once('|').unwrap();
        assert_eq!(args, "two words");
        if cfg!(target_os = "macos") {
            assert!(path.starts_with(&format!("{session}:")), "{path}");
            for directory in ["/usr/local/bin", "/opt/homebrew/bin"] {
                assert!(path.split(':').any(|entry| entry == directory), "{path}");
            }
        } else {
            assert_eq!(path, session);
        }
    }

    /// Launch finding R3-11: `*.lock` files stayed in the instance's socket
    /// directory after their masters exited. The daemon clears, when it
    /// starts, each lock whose master's socket is gone, and leaves a lock that
    /// guards a live socket or that another process holds.
    #[cfg(unix)]
    // Hard-won: 761279ef: an exited master left lock files that blocked later opens.
    #[test]
    fn stale_master_locks_are_removed_but_live_or_held_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("aaaaaaaaaaaaaaaa-0.lock");
        fs::write(&stale, b"").unwrap();
        fs::write(dir.path().join("bbbbbbbbbbbbbbbb-0"), b"").unwrap();
        let live = dir.path().join("bbbbbbbbbbbbbbbb-0.lock");
        fs::write(&live, b"").unwrap();
        let held = dir.path().join("cccccccccccccccc-0.lock");
        let holder = lock_master_opening(&dir.path().join("cccccccccccccccc-0")).unwrap();

        remove_stale_master_locks_in(dir.path());
        assert!(!stale.exists(), "a lock whose master is gone is removed");
        assert!(live.exists(), "a lock beside a live socket stays");
        assert!(held.exists(), "a lock another opener holds stays");

        drop(holder);
        // A process another test forks while the holder is open shares its
        // lock until that child execs, so the sweep may find the lock still
        // held for a few milliseconds; the daemon would try again at its next
        // start.
        for _ in 0..200 {
            remove_stale_master_locks_in(dir.path());
            if !held.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!held.exists(), "a lock nobody holds any more is removed");
    }

    const BORROW_PARENT: &str = "0123456789abcdef0123456789abcdef";
    const BORROW_CHILD: &str = "fedcba9876543210fedcba9876543210";

    fn borrowed_podman(owner: &str) -> TargetLocator {
        TargetLocator::LocalPodman {
            container_id: crate::targets::resource_name(owner).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::default(),
            borrowed_from: Some(owner.to_owned()),
        }
    }

    // Hard-won: 5cea6d11: a child inherited its parent locator and was refused before starting.
    #[test]
    fn verify_locator_accepts_a_container_borrowed_from_its_owner() {
        verify_locator(&borrowed_podman(BORROW_PARENT), BORROW_CHILD)
            .expect("a child may borrow its parent's container");
    }

    #[test]
    fn verify_locator_rejects_a_container_borrowed_from_the_checking_session() {
        let error = verify_locator(&borrowed_podman(BORROW_PARENT), BORROW_PARENT)
            .expect_err("a session cannot borrow from itself");
        assert!(
            format!("{error:#}").contains("cannot be owned by the borrowing session"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn verify_locator_rejects_a_borrowed_container_naming_another_session() {
        let locator = TargetLocator::LocalPodman {
            container_id: crate::targets::resource_name(BORROW_CHILD).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::default(),
            borrowed_from: Some(BORROW_PARENT.to_owned()),
        };
        let error = verify_locator(&locator, BORROW_CHILD)
            .expect_err("the container must belong to the recorded owner");
        assert!(
            format!("{error:#}").contains("borrowed container locator"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn worker_root_of_a_borrowed_container_is_the_childs_own_directory() {
        assert_eq!(
            crate::targets::worker_root(&borrowed_podman(BORROW_PARENT), BORROW_CHILD).unwrap(),
            format!("/var/lib/hel/workers/{BORROW_CHILD}")
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records the commands it is handed and reports an empty success.
    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingExecutor {
        seen: std::cell::RefCell<Vec<CommandSpec>>,
    }

    #[cfg(unix)]
    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.seen.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[cfg(unix)]
    fn sharing_socket_dir() -> tempfile::TempDir {
        // macOS's default temporary path leaves too little room for SSH's hash.
        tempfile::tempdir_in("/tmp").expect("short control socket directory")
    }

    /// Lease a session for `command` through the process-wide ledger with
    /// the sockets pinned to `dir`, and return the arguments it would be
    /// spawned with.
    #[cfg(unix)]
    fn spawned_args(
        command: &CommandSpec,
        dir: Option<&Path>,
        masters: &FakeMasters,
    ) -> Vec<String> {
        set_ssh_connection_sharing_for_test(Some(match dir {
            Some(dir) => SshSharingForTest::Directory(dir.to_path_buf()),
            None => SshSharingForTest::Disabled,
        }));
        let session = command.open_ssh_session(masters, &mut SshWalk::default());
        set_ssh_connection_sharing_for_test(None);
        session.expect("session").command().args.clone()
    }

    /// A built command carries a session request instead of sharing
    /// options; at spawn time the session options go in front of everything
    /// the user configured, so OpenSSH honours them over any proxy setting.
    #[test]
    #[cfg(unix)]
    fn session_options_lead_the_spawned_command() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        let ssh = SshTarget {
            destination: "session-options-host".to_owned(),
            ssh_args: vec![
                "-p".to_owned(),
                "2222".to_owned(),
                "-o".to_owned(),
                "ProxyCommand=nc %h %p".to_owned(),
            ],
        };
        let command = ssh_command(&ssh, ["true"]);
        let remote = remote_command_line(&["true".to_owned()]);
        assert_eq!(
            command.args,
            [
                "-p",
                "2222",
                "-o",
                "ProxyCommand=nc %h %p",
                "session-options-host",
                remote.as_str()
            ],
            "stored arguments never contain sharing options"
        );
        assert_eq!(command.ssh_session.as_ref(), Some(&ssh));
        assert_eq!(
            command.ssh_destination.as_deref(),
            Some("session-options-host")
        );

        let masters = FakeMasters::default();
        let args = spawned_args(&command, Some(socket_dir.path()), &masters);
        let socket = socket_dir.path().join(control_socket_name(&ssh, 0));
        assert_eq!(
            args,
            [
                "-o".to_owned(),
                "ControlMaster=no".to_owned(),
                "-o".to_owned(),
                format!("ControlPath={}", socket.display()),
                "-o".to_owned(),
                "ProxyCommand=false".to_owned(),
                "-p".to_owned(),
                "2222".to_owned(),
                "-o".to_owned(),
                "ProxyCommand=nc %h %p".to_owned(),
                "session-options-host".to_owned(),
                with_start_marker(&remote),
            ]
        );
        assert_eq!(masters.openers(), 1);
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(
                &fs::metadata(socket_dir.path()).expect("socket directory")
            ) & 0o777,
            0o700
        );
    }

    /// A control socket is the value of an `ssh` option, and `ssh` splits an
    /// unquoted value on whitespace. On macOS the sockets sit below
    /// `~/Library/Application Support/mjolnir`, so every option line Mjolnir
    /// wrote was refused with "keyword controlpath extra arguments at end of
    /// line" and no Mac target answered at all. `ssh -G` resolves options
    /// without connecting, so this holds the value we emit against ssh's own
    /// parser. `%` is expanded even inside the quotes, so it is doubled.
    #[test]
    #[cfg(unix)]
    fn a_control_path_with_a_space_reaches_ssh_intact() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        let dir = socket_dir.path().join("Application Support/%h");
        let ssh = SshTarget {
            destination: "socket-spaces.invalid".to_owned(),
            ssh_args: Vec::new(),
        };
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(dir.clone())));
        let mut args = Vec::new();
        push_connection_reuse_args(&mut args, &ssh);
        set_ssh_connection_sharing_for_test(None);

        let Ok(resolved) = std::process::Command::new("ssh")
            .args(["-F", "/dev/null", "-G"])
            .args(&args)
            .arg(&ssh.destination)
            .output()
        else {
            return; // no ssh client on this machine
        };
        let stderr = String::from_utf8_lossy(&resolved.stderr);
        assert!(
            resolved.status.success(),
            "ssh refused the options we built: {stderr}\n{args:?}"
        );
        let expected = format!(
            "controlpath {}",
            dir.join(control_socket_name(&ssh, 0)).display()
        );
        let stdout = String::from_utf8_lossy(&resolved.stdout);
        assert!(
            stdout.lines().any(|line| line == expected),
            "expected {expected:?}:\n{stdout}"
        );
    }

    /// Each instance keeps its own sockets, because each daemon counts only
    /// its own sessions against a master.
    #[test]
    #[cfg(unix)]
    fn control_sockets_live_in_a_directory_per_instance() {
        let runtime = Some(std::ffi::OsString::from("/run/user/1000"));
        assert_eq!(
            default_control_dir(runtime.clone(), &control_dir_identity(Some("hel2"), None)),
            PathBuf::from("/run/user/1000/mjolnir/hel2")
        );
        assert_eq!(
            default_control_dir(runtime, &control_dir_identity(None, None)),
            PathBuf::from("/run/user/1000/mjolnir/default")
        );
    }

    /// A daemon isolated only by `MJ_DATA_DIR` (every luna lab) is another
    /// instance, so it must not share the default instance's masters or
    /// sweep its lock files (launch finding R5-1). Its directory is named by
    /// the same fingerprint `instance_identity` stamps on its workers.
    // Hard-won: 5cd9c555: MJ_DATA_DIR-only daemons reused default-instance sockets and swept each other’s locks.
    #[test]
    #[cfg(unix)]
    fn a_data_directory_override_gets_its_own_socket_directory() {
        let runtime = Some(std::ffi::OsString::from("/run/user/1000"));
        let lab = Path::new("/tmp/lab-a/data");
        let other_lab = Path::new("/tmp/lab-b/data");
        let lab_dir = default_control_dir(runtime.clone(), &control_dir_identity(None, Some(lab)));
        assert_ne!(lab_dir, PathBuf::from("/run/user/1000/mjolnir/default"));
        assert_eq!(
            lab_dir,
            PathBuf::from("/run/user/1000/mjolnir")
                .join(crate::config::instance_identity_for(None, lab))
        );
        assert_ne!(
            lab_dir,
            default_control_dir(
                runtime.clone(),
                &control_dir_identity(None, Some(other_lab))
            )
        );
        // A named instance keeps its name whatever its data directory, as
        // `instance_identity` does.
        assert_eq!(
            default_control_dir(runtime, &control_dir_identity(Some("hel2"), Some(lab))),
            PathBuf::from("/run/user/1000/mjolnir/hel2")
        );
    }

    /// Sockets are named `<hash>-<shard>`, and the hash follows the whole
    /// configured connection, not only the destination.
    #[test]
    #[cfg(unix)]
    fn socket_names_identify_the_connection_and_the_shard() {
        let plain = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let other_port = SshTarget {
            destination: "host".to_owned(),
            ssh_args: vec!["-p".to_owned(), "2222".to_owned()],
        };
        let first = control_socket_name(&plain, 0);
        let second = control_socket_name(&plain, 1);
        assert_eq!(first.len(), CONNECTION_HASH_HEX + 2, "{first}");
        assert!(first.ends_with("-0") && second.ends_with("-1"));
        assert_eq!(first[..CONNECTION_HASH_HEX], second[..CONNECTION_HASH_HEX]);
        assert_ne!(
            first[..CONNECTION_HASH_HEX],
            control_socket_name(&other_port, 0)[..CONNECTION_HASH_HEX]
        );
    }

    /// A command with a two-second keepalive must join a master, never open
    /// one: as the master it would impose that keepalive on every later
    /// session sharing the connection.
    #[test]
    #[cfg(unix)]
    fn fail_fast_commands_reuse_a_master_without_becoming_one() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let masters = FakeMasters::default();
        let validation = spawned_args(
            &ssh_validation_command(&ssh, vec!["true".to_owned()], "test"),
            Some(socket_dir.path()),
            &masters,
        );
        assert_eq!(
            masters.commands(),
            0,
            "a probe never checks or opens a master"
        );
        assert!(!validation.contains(&"ProxyCommand=false".to_owned()));
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let executor = RecordingExecutor::default();
        crate::path_completion::ssh_completions(
            &ssh,
            "/srv/pr",
            crate::path_completion::CompletionKind::Directories,
            &executor,
        )
        .expect("completion runs");
        let completion = executor.seen.borrow()[0].args.clone();
        set_ssh_connection_sharing_for_test(None);

        let control_path = format!(
            "ControlPath={}/{}",
            socket_dir.path().display(),
            control_socket_name(&ssh, 0)
        );
        for args in [&validation, &completion] {
            assert!(args.contains(&"ControlMaster=no".to_owned()), "{args:?}");
            assert!(args.contains(&control_path), "{args:?}");
            assert!(
                !args.iter().any(|arg| arg.starts_with("ControlPersist")),
                "a fail-fast command must not set how long a master lingers: {args:?}"
            );
            let master = args
                .iter()
                .position(|arg| arg == "ControlMaster=no")
                .expect("sharing options");
            assert!(
                args.contains(&"ServerAliveCountMax=1".to_owned()),
                "its own keepalive: {args:?}"
            );
            assert!(
                master
                    < args
                        .iter()
                        .position(|arg| arg == "host")
                        .expect("destination"),
                "{args:?}"
            );
        }
    }

    /// `mj doctor` diagnoses and exits. Its connectivity probe must join a
    /// master when one is up and otherwise open a plain connection, so a
    /// doctor run never leaves a `ControlPersist` master behind, and the
    /// probe's own strict overrides never bind a shared connection.
    #[test]
    #[cfg(unix)]
    fn connectivity_probe_joins_a_master_without_becoming_one() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let ssh = SshTarget {
            destination: "host".to_owned(),
            ssh_args: Vec::new(),
        };
        let masters = FakeMasters::default();
        let args = spawned_args(
            &ssh_connectivity_probe(&ssh),
            Some(socket_dir.path()),
            &masters,
        );

        assert_eq!(masters.commands(), 0, "a probe never opens a master");
        assert!(args.contains(&"ControlMaster=no".to_owned()), "{args:?}");
        assert!(
            args.contains(&format!(
                "ControlPath={}/{}",
                socket_dir.path().display(),
                control_socket_name(&ssh, 0)
            )),
            "the probe must still join an existing master: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("ControlPersist")),
            "a doctor probe must not set how long a master lingers: {args:?}"
        );
        let master = args
            .iter()
            .position(|arg| arg == "ControlMaster=no")
            .expect("sharing options");
        let strict = args
            .iter()
            .position(|arg| arg == "StrictHostKeyChecking=yes")
            .expect("its own host key policy");
        assert!(master < strict, "{args:?}");
    }

    #[test]
    #[cfg(unix)]
    fn a_control_path_that_cannot_fit_a_socket_address_is_skipped() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().expect("temp dir");
        let long = root.path().join("a".repeat(MAX_CONTROL_PATH));
        let ssh = SshTarget {
            destination: "long-path-host".to_owned(),
            ssh_args: Vec::new(),
        };
        let masters = FakeMasters::default();
        let args = spawned_args(&ssh_command(&ssh, ["true"]), Some(&long), &masters);
        assert_eq!(
            args,
            [
                "long-path-host".to_owned(),
                with_start_marker(&remote_command_line(&["true".to_owned()]))
            ]
        );
        assert_eq!(masters.commands(), 0);
        assert!(!long.exists(), "an unusable directory must not be created");
    }

    /// Against a real host: leasing a session opens a master that
    /// `ssh -O check` finds, and a command runs through it. Set
    /// `MJ_E2E_SSH_HOST` to a reachable destination to run it.
    #[test]
    #[cfg(unix)]
    fn a_leased_session_runs_through_an_opened_master_on_a_real_host() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(host) = std::env::var_os("MJ_E2E_SSH_HOST") else {
            return;
        };
        let host = host.to_string_lossy().into_owned();
        let socket_dir = sharing_socket_dir();
        let ssh = SshTarget {
            destination: host.clone(),
            ssh_args: vec!["-o".to_owned(), "BatchMode=yes".to_owned()],
        };
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        let output = ProcessExecutor.execute(&ssh_command(&ssh, ["true"]));
        set_ssh_connection_sharing_for_test(None);
        let socket = socket_dir.path().join(control_socket_name(&ssh, 0));
        let check = ProcessExecutor
            .execute(&master_check_command(&ssh, &socket))
            .expect("ssh -O check must run");
        let exit = std::process::Command::new("ssh")
            .args(["-O", "exit", "-o", &control_path_option(&socket), &host])
            .output();
        let output = output.expect("ssh must run");
        assert_eq!(
            output.status,
            0,
            "ssh {host} true failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            check.status,
            0,
            "no master is running: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        drop(exit);
    }

    /// A throwaway `sshd` on 127.0.0.1 run by the current user, with
    /// `MaxSessions` set, so a test meets the refusal a real server gives.
    /// Dropping it stops the server and exits the masters it carried.
    #[cfg(unix)]
    struct LocalSshd {
        child: std::process::Child,
        ssh: SshTarget,
        _directory: tempfile::TempDir,
        sockets: PathBuf,
    }

    #[cfg(unix)]
    impl LocalSshd {
        /// `None` when this machine has no `sshd` or `ssh-keygen` to run.
        fn start(max_sessions: usize, sockets: &Path) -> Option<Self> {
            let sshd = ["/usr/sbin/sshd", "/usr/local/sbin/sshd"]
                .into_iter()
                .map(Path::new)
                .find(|path| path.exists())?;
            let directory = tempfile::tempdir_in("/tmp").expect("sshd directory");
            let root = directory.path();
            for name in ["hostkey", "client"] {
                let status = std::process::Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", ""])
                    .arg("-f")
                    .arg(root.join(name))
                    .status()
                    .ok()?;
                assert!(status.success(), "ssh-keygen {name}");
            }
            fs::copy(root.join("client.pub"), root.join("authorized_keys")).unwrap();
            // The kernel hands out a free port; the window before sshd binds it
            // is the usual one for test servers.
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let config = root.join("sshd_config");
            fs::write(
                &config,
                format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/hostkey\n\
                     PidFile {root}/sshd.pid\nAuthorizedKeysFile {root}/authorized_keys\n\
                     StrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                     MaxSessions {max_sessions}\n",
                    root = root.display()
                ),
            )
            .unwrap();
            let log = fs::File::create(root.join("sshd.log")).unwrap();
            let child = std::process::Command::new(sshd)
                .args(["-D", "-e", "-f"])
                .arg(&config)
                .stderr(log)
                .spawn()
                .expect("start sshd");
            let user = std::process::Command::new("id")
                .arg("-un")
                .output()
                .expect("id -un");
            let user = String::from_utf8_lossy(&user.stdout).trim().to_owned();
            let ssh = SshTarget {
                destination: format!("{user}@127.0.0.1"),
                ssh_args: [
                    "-F",
                    "/dev/null",
                    "-p",
                    &port.to_string(),
                    "-i",
                    &root.join("client").display().to_string(),
                    "-o",
                    "IdentitiesOnly=yes",
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "-o",
                    "BatchMode=yes",
                ]
                .map(str::to_owned)
                .to_vec(),
            };
            let server = Self {
                child,
                ssh,
                sockets: sockets.to_path_buf(),
                _directory: directory,
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "sshd did not listen: {}",
                    fs::read_to_string(server._directory.path().join("sshd.log"))
                        .unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Some(server)
        }
    }

    #[cfg(unix)]
    impl Drop for LocalSshd {
        fn drop(&mut self) {
            // Masters persist for a minute after their last session unless told
            // to exit; they carry no sessions the test still needs.
            for shard in 0..32 {
                let socket = self.sockets.join(control_socket_name(&self.ssh, shard));
                if socket.exists() {
                    let _ = std::process::Command::new("ssh")
                        .args(["-O", "exit", "-o", &control_path_option(&socket)])
                        .args(&self.ssh.ssh_args)
                        .arg(&self.ssh.destination)
                        .output();
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Against a real `sshd` that allows two sessions per connection, with
    /// another process (a second ledger on the same control sockets: an
    /// overlapping daemon, `mj doctor`, Tab completion) holding the first
    /// master full: more concurrent commands than one connection can carry
    /// all succeed, because a command the server refuses never started and
    /// walks to the next master. That includes commands that stream more than
    /// a pipe's worth of stdin: nothing of their input is read until the
    /// session has started, so a refusal never loses any of it. A guarded
    /// session with no master still fails instead of connecting on its own.
    // Hard-won: 75da0c2b0: another process filled a master the ledger counted as free, and every refusal at its "known capacity" spent an attempt until the command failed.
    #[test]
    #[cfg(unix)]
    fn commands_walk_past_masters_that_another_process_filled_on_a_real_sshd() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        let Some(server) = LocalSshd::start(2, socket_dir.path()) else {
            eprintln!("skipped: this machine has no sshd");
            return;
        };
        let ssh = server.ssh.clone();
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Directory(
            socket_dir.path().to_path_buf(),
        )));
        // The other process opens the first master and fills both of its
        // sessions with sleepers that outlast the whole test.
        let started = tempfile::tempdir().unwrap();
        let other = SessionLedger::new();
        let mut held: Vec<std::process::Child> = (0..2)
            .map(|index| {
                let lease = other
                    .lease(
                        &ssh,
                        socket_dir.path(),
                        &ProcessExecutor,
                        &mut SshWalk::default(),
                    )
                    .expect("the other process leases a session");
                let marker = started.path().join(format!("held-{index}"));
                let command = ssh_command(
                    &ssh,
                    [
                        "sh".to_owned(),
                        "-c".to_owned(),
                        "touch \"$0\"; sleep 20".to_owned(),
                        marker.display().to_string(),
                    ],
                );
                std::process::Command::new("ssh")
                    .args(session_command_args("ssh", &command.args, &ssh, &lease))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn a held session")
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(0..2).all(|index| started.path().join(format!("held-{index}")).exists()) {
            assert!(Instant::now() < deadline, "the held sessions never started");
            std::thread::sleep(Duration::from_millis(20));
        }

        let sleeper = ssh_command(&ssh, ["sleep", "3"]);
        let counter = ssh_command(&ssh, ["wc", "-c"]);
        let payload = vec![b'x'; 192 * 1024];
        let outputs: Vec<Result<CommandOutput>> = std::thread::scope(|scope| {
            let mut handles: Vec<_> = (0..3)
                .map(|_| scope.spawn(|| ProcessExecutor.execute(&sleeper)))
                .collect();
            handles.extend((0..2).map(|_| {
                scope.spawn(|| {
                    ProcessExecutor
                        .execute_with_stdin(&counter, &mut std::io::Cursor::new(payload.clone()))
                })
            }));
            handles
                .into_iter()
                .map(|handle| handle.join().expect("session thread"))
                .collect()
        });
        let shards = SessionLedger::global()
            .connections()
            .get(&connection_key(&ssh))
            .map_or(0, |connection| connection.shards.len());
        let ledger = SessionLedger::new();
        let orphan_reservation = Reservation {
            shard: 99,
            opening: Arc::new(Mutex::new(())),
        };
        let orphan_lease = SshSessionLease::new(
            Some(ledger.slot(
                connection_key(&ssh),
                &orphan_reservation,
                socket_dir.path().join(control_socket_name(&ssh, 99)),
            )),
            false,
        );
        let orphan = ProcessExecutor.execute(
            &CommandSpec::new(
                "ssh",
                session_command_args("ssh", &sleeper.args, &ssh, &orphan_lease),
            )
            .purpose("run a guarded session with no master"),
        );
        set_ssh_connection_sharing_for_test(None);
        for child in &mut held {
            let _ = child.kill();
            let _ = child.wait();
        }

        for (index, output) in outputs.iter().enumerate() {
            let output = output.as_ref().expect("ssh must run");
            assert_eq!(
                output.status,
                0,
                "session {index} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if index >= 3 {
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout).trim(),
                    payload.len().to_string(),
                    "streamed session {index} received its whole input exactly once"
                );
            } else {
                assert_eq!(output.stdout, b"", "the start marker is stripped");
            }
        }
        assert!(
            shards >= 2,
            "the first master was full, so the commands walked on: {shards} shards"
        );
        let orphan = orphan.expect("ssh must run");
        assert_eq!(
            orphan.status,
            255,
            "a guarded session with no master must not connect: {}",
            String::from_utf8_lossy(&orphan.stderr)
        );
        drop(server);
    }

    /// A server that allows no sessions on a connection (`MaxSessions 0`)
    /// refuses the first session on every master, so walking would open
    /// masters forever. The lease fails visibly instead, naming the host and
    /// the setting, closes the master it opened, and the next lease fails the
    /// same way without having walked to a second shard.
    #[test]
    #[cfg(unix)]
    fn a_server_that_allows_no_sessions_fails_the_lease_instead_of_opening_masters() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let socket_dir = sharing_socket_dir();
        let Some(server) = LocalSshd::start(0, socket_dir.path()) else {
            eprintln!("skipped: this machine has no sshd");
            return;
        };
        let ssh = server.ssh.clone();
        let ledger = SessionLedger::new();
        for _ in 0..3 {
            let error = ledger
                .lease(
                    &ssh,
                    socket_dir.path(),
                    &ProcessExecutor,
                    &mut SshWalk::default(),
                )
                .expect_err("a server without sessions cannot carry a command");
            let message = format!("{error:#}");
            assert!(message.contains(&ssh.destination), "{message}");
            assert!(message.contains("MaxSessions"), "{message}");
        }
        let shards = ledger
            .connections()
            .get(&connection_key(&ssh))
            .map_or(0, |connection| connection.shards.len());
        assert_eq!(shards, 1, "the failed lease did not walk to another shard");
        let socket = socket_dir.path().join(control_socket_name(&ssh, 0));
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket.exists() {
            assert!(
                Instant::now() < deadline,
                "the unusable master was left running"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(server);
    }

    /// A hand-written stand-in for `ssh` that models masters: `-O check`
    /// succeeds only for a socket whose master it opened, and an opener
    /// starts one unless told to refuse. It records every command.
    #[cfg(unix)]
    #[derive(Default)]
    struct FakeMasters {
        running: std::cell::RefCell<BTreeSet<String>>,
        refuse_open: std::cell::Cell<Option<&'static str>>,
        /// The server refuses every session, as `MaxSessions 0` does.
        refuse_sessions: std::cell::Cell<bool>,
        socket_existed_at_open: std::cell::RefCell<Vec<bool>>,
        seen: std::cell::RefCell<Vec<CommandSpec>>,
    }

    #[cfg(unix)]
    impl FakeMasters {
        fn socket(command: &CommandSpec) -> String {
            command
                .args
                .iter()
                .find_map(|arg| arg.strip_prefix("ControlPath="))
                .expect("every master command names its socket")
                .to_owned()
        }

        fn kill(&self, socket: &Path) {
            self.running
                .borrow_mut()
                .remove(&socket.display().to_string());
        }

        fn commands(&self) -> usize {
            self.seen.borrow().len()
        }

        fn openers(&self) -> usize {
            self.seen
                .borrow()
                .iter()
                .filter(|command| command.args.contains(&"ControlMaster=yes".to_owned()))
                .count()
        }
    }

    #[cfg(unix)]
    impl CommandExecutor for FakeMasters {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.seen.borrow_mut().push(command.clone());
            assert_eq!(command.program, "ssh");
            let socket = Self::socket(command);
            let (status, stderr) = if command.args.windows(2).any(|pair| pair == ["-O", "check"]) {
                if self.running.borrow().contains(&socket) {
                    (0, "")
                } else {
                    (255, "Control socket connect: No such file or directory")
                }
            } else if command.args.windows(2).any(|pair| pair == ["-O", "exit"]) {
                self.running.borrow_mut().remove(&socket);
                (0, "")
            } else if command.args.contains(&"ProxyCommand=false".to_owned()) {
                // The first session on a master the ledger just opened.
                if self.refuse_sessions.get() {
                    (
                        255,
                        "mux_client_request_session: session request failed: Session open refused by peer",
                    )
                } else {
                    (0, "")
                }
            } else if command.args.contains(&"ControlMaster=yes".to_owned()) {
                self.socket_existed_at_open
                    .borrow_mut()
                    .push(Path::new(&socket).exists());
                match self.refuse_open.get() {
                    Some(stderr) => (255, stderr),
                    None => {
                        self.running.borrow_mut().insert(socket);
                        (0, "")
                    }
                }
            } else {
                panic!("the ledger ran an unexpected command: {command:?}");
            };
            Ok(CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            })
        }
    }

    #[cfg(unix)]
    fn shard_of(lease: &SshSessionLease) -> String {
        let path = lease.control_path().expect("a shared lease has a socket");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        name.rsplit('-').next().unwrap().to_owned()
    }

    /// The marker can arrive in any split of reads, and a stream that does not
    /// begin with it passes through whole: the first byte of anything proves
    /// the session started.
    #[test]
    fn the_start_marker_is_stripped_however_it_is_split_across_reads() {
        let stream = format!("{SSH_START_MARKER}payload\nmore");
        for split in 1..stream.len() {
            let mut scan = StartScan::new();
            let mut out = Vec::new();
            let (head, tail) = stream.as_bytes().split_at(split);
            assert!(
                scan.push(head, &mut out),
                "the first byte starts the session"
            );
            assert!(!scan.push(tail, &mut out));
            scan.finish(&mut out);
            assert_eq!(out, b"payload\nmore", "split at {split}");
        }
        // One byte at a time, as a slow pipe delivers it.
        let mut scan = StartScan::new();
        let mut out = Vec::new();
        for byte in stream.as_bytes() {
            scan.push(&[*byte], &mut out);
        }
        assert_eq!(out, b"payload\nmore");

        // A stream that diverges from the marker, and one that ends inside it.
        let mut scan = StartScan::new();
        let mut out = Vec::new();
        assert!(scan.push(b"MJOLNIR-x", &mut out));
        assert_eq!(out, b"MJOLNIR-x");
        let mut scan = StartScan::new();
        let mut out = Vec::new();
        assert!(scan.push(b"MJOLN", &mut out));
        assert!(out.is_empty(), "held until the next chunk decides");
        scan.finish(&mut out);
        assert_eq!(out, b"MJOLN");
        let mut scan = StartScan::new();
        assert!(!scan.push(b"", &mut Vec::new()), "no byte, no start");
    }

    fn plain_target(destination: &str) -> SshTarget {
        SshTarget {
            destination: destination.to_owned(),
            ssh_args: Vec::new(),
        }
    }

    /// `scp` spells the port `-P`; passing an `ssh` `-p` through would ask it
    /// to preserve file times and read the port as a file name. Every `scp`
    /// also opens a connection, so it is retried like `ssh`.
    // Hard-won: a1b93a2e: controller uploads used SSH -p where scp requires -P.
    #[test]
    #[cfg(unix)]
    fn scp_translates_the_ssh_port_option_and_is_tagged_with_its_destination() {
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let ssh = SshTarget {
            destination: "build@10.0.0.1".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        };

        let upload = scp_upload(&ssh, Path::new("/tmp/local"), "remote/path", true);
        let download = scp_download(&ssh, "remote/archive.zip", "/tmp/local.zip");
        set_ssh_connection_sharing_for_test(None);

        assert_eq!(
            upload.args,
            [
                "-P",
                "2222",
                "-r",
                "/tmp/local",
                "build@10.0.0.1:remote/path"
            ]
        );
        assert_eq!(
            download.args,
            [
                "-P",
                "2222",
                "build@10.0.0.1:remote/archive.zip",
                "/tmp/local.zip"
            ]
        );
        for command in [upload, download] {
            assert_eq!(command.program, "scp");
            assert_eq!(command.ssh_destination.as_deref(), Some("build@10.0.0.1"));
        }
    }

    /// Take the next lease of `walk` on `ledger`.
    #[cfg(unix)]
    fn next_lease(
        ledger: &Arc<SessionLedger>,
        ssh: &SshTarget,
        dir: &Path,
        masters: &FakeMasters,
        walk: &mut SshWalk,
    ) -> SshSessionLease {
        ledger.lease(ssh, dir, masters, walk).expect("lease")
    }

    /// A command's walk tries each shard once, in order from where the last
    /// started session was, wrapping around, and opens a new shard only after
    /// it has tried every one. A stale hint (here, a shard that no longer
    /// exists in the order the walk expects) changes the order and never
    /// fails a lease. A probe walks the same way and opens no master.
    #[test]
    #[cfg(unix)]
    fn a_walk_tries_every_shard_once_before_it_opens_a_new_one() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("walk-host");
        let masters = FakeMasters::default();

        let mut first = SshWalk::default();
        let leases: Vec<_> = (0..3)
            .map(|_| next_lease(&ledger, &ssh, dir.path(), &masters, &mut first))
            .collect();
        assert_eq!(
            leases.iter().map(shard_of).collect::<Vec<_>>(),
            ["0", "1", "2"],
            "each refusal opens the next shard"
        );
        assert_eq!(masters.openers(), 3);

        // A session started on shard 2: the next walk starts there, wraps
        // through 0 and 1, and only then opens shard 3.
        leases[2].gate().mark_started();
        let mut second = SshWalk::default();
        let walked: Vec<_> = (0..5)
            .map(|_| {
                shard_of(&next_lease(
                    &ledger,
                    &ssh,
                    dir.path(),
                    &masters,
                    &mut second,
                ))
            })
            .collect();
        assert_eq!(walked, ["2", "0", "1", "3", "4"]);

        // A probe never opens a master, even past the last shard.
        let openers = masters.openers();
        let mut probing = SshWalk::default();
        let probes: Vec<_> = (0..7)
            .map(|_| shard_of(&ledger.lease_probe(&ssh, dir.path(), &mut probing)))
            .collect();
        assert_eq!(probes, ["2", "3", "4", "0", "1", "5", "6"]);
        assert_eq!(masters.openers(), openers);
    }

    /// Commands that are refused at the same moment share the shard one of
    /// them opened instead of each opening its own.
    #[test]
    #[cfg(unix)]
    fn walks_past_the_last_shard_share_the_shard_that_was_just_added() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("shared-walk-host");
        let masters = FakeMasters::default();
        let mut walks = [SshWalk::default(), SshWalk::default(), SshWalk::default()];
        for walk in &mut walks {
            drop(next_lease(&ledger, &ssh, dir.path(), &masters, walk));
        }
        // All three were refused on shard 0 and now ask for the next one.
        let next: Vec<_> = walks
            .iter_mut()
            .map(|walk| shard_of(&next_lease(&ledger, &ssh, dir.path(), &masters, walk)))
            .collect();
        assert_eq!(next, ["1", "1", "1"]);
        assert_eq!(masters.openers(), 2, "one master for the shard they share");
    }

    /// A refused session on a shared master walks on at once. A refusal with
    /// no shared master to walk on (user-configured sharing) does not, or the
    /// zero-delay retry would never end, and neither does a connection dropped
    /// before authentication.
    #[test]
    #[cfg(unix)]
    fn only_a_refused_session_on_a_shared_master_walks_on() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("refused-host");
        let masters = FakeMasters::default();
        let shared = next_lease(&ledger, &ssh, dir.path(), &masters, &mut SshWalk::default());
        assert!(shared.note_refusal(SshRefusal::SessionLimit));
        assert!(!shared.note_refusal(SshRefusal::BeforeAuthentication));
        assert!(!SshSessionLease::unshared().note_refusal(SshRefusal::SessionLimit));
    }

    /// Target validation and the connectivity probe walk the shards like any
    /// command, but open no master, and carry no `ProxyCommand=false`: where
    /// no master exists they connect directly, as probes always did.
    #[test]
    #[cfg(unix)]
    fn probes_join_a_shard_without_opening_it() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("probe-host");
        let masters = FakeMasters::default();

        let session = next_lease(&ledger, &ssh, dir.path(), &masters, &mut SshWalk::default());
        let probe = ledger.lease_probe(&ssh, dir.path(), &mut SshWalk::default());
        assert_eq!(
            [&session, &probe].map(shard_of),
            ["0", "0"].map(str::to_owned)
        );
        assert_eq!(masters.openers(), 1, "a probe never opens a master");

        let mut args = Vec::new();
        push_session_args(&mut args, &probe);
        assert!(
            !args.contains(&"ProxyCommand=false".to_owned()),
            "a probe may connect directly when its master is down: {args:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn separate_connections_are_counted_separately() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let masters = FakeMasters::default();
        let first = ledger
            .lease(
                &plain_target("one"),
                dir.path(),
                &masters,
                &mut SshWalk::default(),
            )
            .expect("one");
        let second = ledger
            .lease(
                &plain_target("two"),
                dir.path(),
                &masters,
                &mut SshWalk::default(),
            )
            .expect("two");
        assert_eq!(
            [&first, &second].map(shard_of),
            ["0", "0"].map(str::to_owned)
        );
        assert_ne!(first.control_path(), second.control_path());
    }

    #[test]
    #[cfg(unix)]
    fn an_invalidated_lease_makes_the_next_lease_reopen_a_dead_master() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("host");
        let masters = FakeMasters::default();
        let first = ledger
            .lease(&ssh, dir.path(), &masters, &mut SshWalk::default())
            .expect("first");
        masters.kill(first.control_path().unwrap());

        // Without a failure report the recent check is still trusted.
        drop(next_lease(
            &ledger,
            &ssh,
            dir.path(),
            &masters,
            &mut SshWalk::default(),
        ));
        assert_eq!(masters.openers(), 1);

        first.invalidate();
        let second = next_lease(&ledger, &ssh, dir.path(), &masters, &mut SshWalk::default());
        assert_eq!(masters.openers(), 2);
        assert_eq!(first.control_path(), second.control_path());
    }

    #[test]
    #[cfg(unix)]
    fn a_master_that_cannot_be_opened_is_an_error_naming_the_destination() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("build@10.0.0.1");
        let masters = FakeMasters::default();
        masters
            .refuse_open
            .set(Some("Permission denied (publickey)."));

        let error = ledger
            .lease(&ssh, dir.path(), &masters, &mut SshWalk::default())
            .expect_err("no master means no session");
        let message = format!("{error:#}");
        assert!(message.contains("build@10.0.0.1"), "{message}");
        assert!(message.contains("Permission denied"), "{message}");
        assert_eq!(
            masters.openers(),
            1,
            "the opener is not retried by the ledger"
        );

        masters.refuse_open.set(None);
        let lease = next_lease(&ledger, &ssh, dir.path(), &masters, &mut SshWalk::default());
        assert_eq!(shard_of(&lease), "0");
    }

    /// A stand-in for `ssh` shared by two threads that play two daemon
    /// processes (the old and new daemon during a restart). An opener takes a
    /// while to authenticate; if the socket is bound when it finishes, real
    /// `ssh` prints "already exists, disabling multiplexing" and keeps a plain
    /// background connection that nothing will ever close.
    #[cfg(unix)]
    #[derive(Default)]
    struct RacingMasters {
        bound: Mutex<BTreeSet<String>>,
        masters: AtomicUsize,
        orphans: AtomicUsize,
    }

    #[cfg(unix)]
    impl CommandExecutor for RacingMasters {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let socket = FakeMasters::socket(command);
            let status = if command.args.windows(2).any(|pair| pair == ["-O", "check"]) {
                if self.bound.lock().unwrap().contains(&socket) {
                    0
                } else {
                    255
                }
            } else if !command.args.contains(&"ControlMaster=yes".to_owned()) {
                // The first session on the master.
                0
            } else {
                std::thread::sleep(Duration::from_millis(100));
                if self.bound.lock().unwrap().insert(socket) {
                    self.masters.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.orphans.fetch_add(1, Ordering::SeqCst);
                }
                0
            };
            Ok(CommandOutput {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// Two daemon processes that lease on the same instance's sockets at
    /// once (J-18) must open one master between them, not one master and one
    /// orphaned plain connection.
    // Hard-won: 07a04c5d: two daemons could leave a second orphan SSH connection for one socket.
    #[test]
    #[cfg(unix)]
    fn two_processes_opening_one_socket_open_one_master() {
        let dir = sharing_socket_dir();
        let ssh = plain_target("racing-host");
        let fake = RacingMasters::default();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    // Each process has its own ledger.
                    let ledger = SessionLedger::new();
                    ledger
                        .lease(&ssh, dir.path(), &fake, &mut SshWalk::default())
                        .expect("lease");
                });
            }
        });
        assert_eq!(fake.masters.load(Ordering::SeqCst), 1);
        assert_eq!(fake.orphans.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[cfg(unix)]
    fn a_stale_socket_is_removed_before_the_master_is_opened() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let ssh = plain_target("host");
        let socket = dir.path().join(control_socket_name(&ssh, 0));
        fs::write(&socket, b"").expect("stale socket stand-in");
        let masters = FakeMasters::default();

        ledger
            .lease(&ssh, dir.path(), &masters, &mut SshWalk::default())
            .expect("opens");

        assert_eq!(*masters.socket_existed_at_open.borrow(), [false]);
    }

    #[test]
    #[cfg(unix)]
    fn a_master_open_times_out_during_handshake_and_honors_the_users_shorter_budget() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (finish, stopping) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((connection, _)) => {
                        // Accept TCP but never send an SSH banner. ConnectTimeout
                        // must cover the handshake, not just the TCP connect.
                        let _ = stopping.recv_timeout(Duration::from_secs(5));
                        drop(connection);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stopping.try_recv().is_ok() || Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept stalled SSH handshake: {error}"),
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let ssh = SshTarget {
            destination: "127.0.0.1".into(),
            ssh_args: vec![
                "-F".into(),
                "/dev/null".into(),
                "-p".into(),
                port.to_string(),
                "-o".into(),
                "ConnectTimeout=1".into(),
            ],
        };
        let started = Instant::now();
        // Exercise one open: the retry loop around it separately retries
        // pre-authentication timeouts with backoff.
        let result = CancellableProcessExecutor::with_timeout(Duration::from_secs(3)).run_once(
            &master_open_command(&ssh, &directory.path().join("master")),
            None,
        );
        let _ = finish.send(());
        server.join().unwrap();
        let output = result.expect("SSH's handshake timeout must beat the executor deadline");
        assert_eq!(output.status, 255);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("timed out"), "{stderr}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[cfg(unix)]
    fn session_args_forbid_a_direct_connection() {
        let dir = sharing_socket_dir();
        let ledger = SessionLedger::new();
        let masters = FakeMasters::default();
        let lease = ledger
            .lease(
                &plain_target("host"),
                dir.path(),
                &masters,
                &mut SshWalk::default(),
            )
            .expect("lease");
        let mut args = Vec::new();
        push_session_args(&mut args, &lease);
        assert_eq!(
            args,
            [
                "-o".to_owned(),
                "ControlMaster=no".to_owned(),
                "-o".to_owned(),
                format!("ControlPath={}", lease.control_path().unwrap().display()),
                "-o".to_owned(),
                "ProxyCommand=false".to_owned(),
            ]
        );
    }

    /// A session refused on a live shared connection is told apart from a
    /// connection dropped before authentication, even though a session bound
    /// with `ProxyCommand=false` prints a closed connection after the refusal.
    // Hard-won: 79fcdde4: an authenticated session refusal was mislabeled as a pre-authentication hangup.
    #[test]
    fn a_refused_session_is_named_apart_from_a_pre_authentication_hangup() {
        assert_eq!(
            ssh_refusal(
                255,
                "mux_client_request_session: session request failed: Session open refused by peer\n\
                 kex_exchange_identification: Connection closed by remote host\n\
                 Connection closed by UNKNOWN port 65535"
            ),
            Some(SshRefusal::SessionLimit)
        );
        assert_eq!(
            ssh_refusal(255, "Connection closed by 192.168.1.77 port 22"),
            Some(SshRefusal::BeforeAuthentication)
        );
        assert_eq!(ssh_refusal(1, "Session open refused by peer"), None);
        assert!(
            !SshRefusal::SessionLimit
                .retry_message()
                .contains("before authentication")
        );
    }

    #[test]
    fn transport_rejection_matches_only_sshd_hangups() {
        let cases: [(i32, &str, bool); 7] = [
            (255, "Connection closed by 192.168.1.77 port 22", true),
            (
                255,
                "kex_exchange_identification: read: Connection reset by peer",
                true,
            ),
            (255, "ssh: Connection reset by 10.0.0.1 port 22", true),
            (255, "Connection timed out during banner exchange", true),
            (255, "Permission denied (publickey).", false),
            (
                255,
                "ssh: connect to host h port 22: Connection refused",
                false,
            ),
            (1, "Connection closed by 192.168.1.77 port 22", false),
        ];
        for (status, stderr, expected) in cases {
            assert_eq!(
                is_transport_rejection(status, stderr),
                expected,
                "status {status} stderr {stderr:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn master_file_and_thread_admission_honor_the_executor_deadline() {
        let dir = sharing_socket_dir();
        let socket = dir.path().join("held-master");
        let _held = lock_master_opening(&socket).unwrap();
        let executor = CancellableProcessExecutor::with_timeout(Duration::from_millis(50));
        assert!(lock_master_opening_unless(&socket, &|| executor.is_cancelled()).is_err());

        let ledger = SessionLedger::new();
        let ssh = plain_target("cancelled-master-host");
        let reservation = ledger.reserve(&connection_key(&ssh), &mut SshWalk::default());
        let _opening = reservation.opening.lock().unwrap();
        let executor = CancellableProcessExecutor::with_timeout(Duration::from_millis(50));
        assert!(
            ledger
                .lease(&ssh, dir.path(), &executor, &mut SshWalk::default())
                .is_err()
        );
    }

    /// Fails the test if a lease runs any command, such as opening a master.
    struct NoMaster;

    impl CommandExecutor for NoMaster {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            panic!(
                "an unshared lease ran {} {:?}",
                command.program, command.args
            )
        }
    }

    /// Every `ssh` and `scp` on a native Windows controller, and on Unix with
    /// `MJ_SSH_CONTROL_MASTER=off`, takes an unshared lease: it opens no master
    /// and the command spawns exactly as built, on its own connection.
    #[test]
    fn unshared_leases_open_no_master_and_spawn_the_built_command() {
        #[cfg(unix)]
        let _guard = SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(unix)]
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let ssh = SshTarget {
            destination: "dev@203.0.113.7".to_owned(),
            ssh_args: vec!["-p".to_owned(), "2238".to_owned()],
        };
        let commands = [
            ssh_command(&ssh, ["git", "status"]),
            ssh_validation_command(&ssh, vec!["true".to_owned()], "validate"),
            ssh_connectivity_probe(&ssh),
            scp_upload(&ssh, Path::new("archive.zip"), "remote/archive.zip", false),
            scp_download(&ssh, "remote/archive.zip", "archive.zip"),
        ];
        for command in &commands {
            let session = command
                .open_ssh_session(&NoMaster, &mut SshWalk::default())
                .unwrap();
            let lease = session.lease().expect("an SSH command takes a lease");
            assert_eq!(lease.control_path(), None);
            lease.invalidate();
            // Only the remote line of an `ssh` command changes: it starts by
            // printing the start marker. `scp` has no remote line.
            let mut expected = command.args.clone();
            if command.program == "ssh" {
                let line = expected.last_mut().unwrap();
                *line = with_start_marker(line);
            }
            assert_eq!(session.command().args, expected, "{}", command.purpose);
        }
        #[cfg(unix)]
        set_ssh_connection_sharing_for_test(None);
    }

    // Hard-won: #1238: a canonicalized `\\?\C:` checkpoint path reached Windows scp as a host name.
    #[cfg(windows)]
    #[test]
    fn scp_names_verbatim_controller_paths_without_their_prefix() {
        let ssh = plain_target("dev@203.0.113.7");
        for (local, expected) in [
            (r"\\?\C:\mj\a.hel.zip", r"C:\mj\a.hel.zip"),
            (
                r"\\?\UNC\server\share\a.hel.zip",
                r"\\server\share\a.hel.zip",
            ),
            (r"C:\mj\a.hel.zip", r"C:\mj\a.hel.zip"),
        ] {
            let upload = scp_upload(&ssh, Path::new(local), "a.zip", false);
            assert_eq!(upload.args, [expected, "dev@203.0.113.7:a.zip"]);
            let download = scp_download(&ssh, "a.zip", local);
            assert_eq!(download.args, ["dev@203.0.113.7:a.zip", expected]);
        }
    }
}
