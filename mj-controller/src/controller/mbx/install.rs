//! Install the pinned native mbx binary on a local or SSH container host.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::super::cache_host::{CacheHost, HostPlatform};
use super::{MBX_VERSION, NativeMbx, probe_native_version, version_at_least};
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
use mj_core::config::Machine;
use mj_core::state::{BuildCacheOff, BuildCachePreview};

const MBX_X86_64_SHA256: &str = "b523a9324d50fd434e355fc7a5af5f7155d62854dd6a5a74173d52655f168d45";
const MBX_AARCH64_SHA256: &str = "900844b980f9e3daf778d38a19046281cc35e07108a9c821befef6130846df1f";

const PROFILE_SCRIPT: &str = r#"set -eu
login_shell=
if command -v getent >/dev/null 2>&1 && command -v id >/dev/null 2>&1; then
    login_name=$(id -un 2>/dev/null || true)
    if [ -n "$login_name" ]; then
        login_shell=$(getent passwd "$login_name" 2>/dev/null | awk -F: 'NR == 1 { print $7; exit }' || true)
    fi
fi
[ -n "$login_shell" ] || login_shell=${SHELL:-sh}
shell_name=${login_shell##*/}
[ -n "$shell_name" ] || shell_name=sh
case "$shell_name" in
    zsh) profile=$HOME/.zprofile; shown='~/.zprofile'; shell_kind=posix ;;
    bash)
        if [ -e "$HOME/.bash_profile" ]; then
            profile=$HOME/.bash_profile; shown='~/.bash_profile'
        elif [ -e "$HOME/.bash_login" ]; then
            profile=$HOME/.bash_login; shown='~/.bash_login'
        else
            profile=$HOME/.profile; shown='~/.profile'
        fi
        shell_kind=posix
        ;;
    fish)
        config_home=${XDG_CONFIG_HOME:-$HOME/.config}
        case "$config_home" in /*) ;; *) config_home=$HOME/.config ;; esac
        config_home=${config_home%/}
        [ -n "$config_home" ] || config_home=/
        profile=$config_home/fish/conf.d/mbx.fish
        case "$profile" in
            "$HOME"/*) shown="~/${profile#"$HOME"/}" ;;
            *) shown=$profile ;;
        esac
        shell_kind=fish
        ;;
    *) profile=$HOME/.profile; shown='~/.profile'; shell_kind=unknown ;;
esac
binary_dir=$2
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
case "$data_home" in /*) ;; *) data_home=$HOME/.local/share ;; esac
data_home=${data_home%/}
[ -n "$data_home" ] || data_home=/
shim_dir=$data_home/mbx/bin
action=$1
if [ "$action" = preview ]; then
    printf 'preview\n%s\n%s\n%s' "$shown" "$shell_kind" "$shim_dir"
    exit 0
fi
[ "$action" = update ] || { echo 'unknown profile action' >&2; exit 2; }
posix_block=$3
fish_block=$4
start='# >>> mbx (added by Mjolnir) >>>'
if grep -Fq "$start" "$profile" 2>/dev/null; then
    status=unchanged
else
    profile_dir=${profile%/*}
    mkdir -p -- "$profile_dir"
    : >> "$profile"
    if [ -s "$profile" ]; then
        printf '\n' >> "$profile"
    fi
    if [ "$shell_kind" = fish ]; then
        printf '%s\n' "$fish_block" >> "$profile"
    else
        printf '%s\n' "$posix_block" >> "$profile"
    fi
    status=changed
fi
printf '%s\n%s\n%s\n%s' "$status" "$shown" "$shell_kind" "$shim_dir"
"#;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Whether machine settings should offer an install or upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbxInstallKind {
    Install,
    Upgrade,
}

/// The verified result of an mbx installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbxInstallResult {
    pub program: PathBuf,
    pub version: String,
    pub profile_changed: bool,
    pub profile_file: String,
    pub profile_warning: Option<String>,
    pub manual_path_line: Option<String>,
    pub kind: MbxInstallKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallDestination {
    Supported {
        program: PathBuf,
        kind: MbxInstallKind,
    },
    Unsupported {
        program: PathBuf,
    },
}

/// The login-profile change shown before and after installing mbx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MbxProfileDetails {
    pub file: String,
    pub warning: Option<String>,
    pub manual_path_line: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileSelection {
    file: String,
    shell_kind: String,
    shim_dir: PathBuf,
}

/// The install action to show for the current read-only cache preview.
pub fn install_kind(preview: &BuildCachePreview) -> Option<MbxInstallKind> {
    // A profile path is populated only when destination selection supports
    // installing or upgrading this host's mbx.
    preview.mbx_profile_file.as_ref()?;
    install_candidate_kind(preview)
}

pub(super) fn install_candidate_kind(preview: &BuildCachePreview) -> Option<MbxInstallKind> {
    // The pinned release is a static Linux executable.
    if matches!(
        &preview.off_reason,
        Some(BuildCacheOff::UnsupportedPlatform(_))
    ) {
        return None;
    }
    match preview.native_mbx.as_deref() {
        None => Some(MbxInstallKind::Install),
        Some(version) if !version_at_least(version, MBX_VERSION) => Some(MbxInstallKind::Upgrade),
        Some(_) => None,
    }
}

/// A fully staged install. Dropping it removes unpublished files after executor teardown.
pub struct PreparedMbx {
    host: CacheHost,
    program: PathBuf,
    kind: MbxInstallKind,
    staged: Option<PathBuf>,
    _download: Option<tempfile::TempDir>,
}

impl Drop for PreparedMbx {
    fn drop(&mut self) {
        if let Some(path) = &self.staged {
            if self.host.ssh().is_none() {
                if let Err(error) = std::fs::remove_file(path) {
                    tracing::warn!(%error, path = %path.display(), "remove staged mbx binary");
                }
            } else {
                cleanup_remote_staging(&self.host, path);
            }
        }
    }
}

/// Probe, fetch, verify and upload without publishing or holding handoff admission.
pub fn prepare_mbx(machine: &Machine, executor: &impl CommandExecutor) -> Result<PreparedMbx> {
    let directory = tempfile::tempdir().context("create mbx download staging directory")?;
    let mut prepared = prepare_mbx_with(machine, executor, None, |triple| {
        download(triple, directory.path(), executor)
    })?;
    prepared._download = Some(directory);
    Ok(prepared)
}

/// Install from a standalone CLI operation without a daemon handoff owner.
pub fn install_mbx(machine: &Machine, executor: &impl CommandExecutor) -> Result<MbxInstallResult> {
    let prepared = prepare_mbx(machine, executor)?;
    let publishing =
        targets::CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(240));
    prepared.publish(&publishing)
}

#[cfg(test)]
fn install_mbx_with(
    machine: &Machine,
    executor: &impl CommandExecutor,
    home_override: Option<&Path>,
    get_binary: impl FnOnce(&str) -> Result<PathBuf>,
) -> Result<MbxInstallResult> {
    prepare_mbx_with(machine, executor, home_override, get_binary)?.publish(executor)
}

fn prepare_mbx_with(
    machine: &Machine,
    executor: &impl CommandExecutor,
    home_override: Option<&Path>,
    get_binary: impl FnOnce(&str) -> Result<PathBuf>,
) -> Result<PreparedMbx> {
    let host = CacheHost::for_machine(machine)
        .context("this machine has no persistent container host where mbx can be installed")?;
    ensure!(
        host.platform(executor)? == HostPlatform::Linux,
        "mbx can only be installed on a Linux container host"
    );
    let triple = host_architecture(&host, executor)?;
    let home = match home_override {
        Some(home) => home.to_path_buf(),
        None => canonical_home(&host, executor)?,
    };

    check_cancelled(executor)?;
    let existing = probe_native_version(&host, executor)?;
    let (program, kind) = match destination(&home, existing.as_ref())? {
        InstallDestination::Supported { program, kind } => (program, kind),
        InstallDestination::Unsupported { program } => bail!(
            "mbx at {} is managed outside Mjolnir's install locations. Upgrade it with its package manager (for mise, run `mise upgrade`).",
            program.display()
        ),
    };
    ensure!(
        program.is_absolute(),
        "the selected mbx destination is not absolute"
    );

    check_cancelled(executor)?;
    let binary = get_binary(triple)?;
    ensure!(
        binary.is_file(),
        "the verified mbx download is not a regular file"
    );
    check_cancelled(executor)?;
    let staged = stage_binary(&host, &home, &binary, &program, executor)?;
    Ok(PreparedMbx {
        host,
        program,
        kind,
        staged: Some(staged),
        _download: None,
    })
}

impl PreparedMbx {
    /// Publish only after admission. The caller supplies an executor independent of request cancellation.
    pub fn publish(mut self, executor: &impl CommandExecutor) -> Result<MbxInstallResult> {
        let host = self.host.clone();
        let program = self.program.clone();
        let kind = self.kind;
        let staged = self
            .staged
            .as_ref()
            .context("mbx install has no staged binary")?;
        let command = host.shell_command(
            r#"chmod 755 -- "$1" && mv -f -- "$1" "$2""#,
            "hel-mbx-install-publish",
            [
                staged.to_string_lossy().into_owned(),
                program.to_string_lossy().into_owned(),
            ],
            "publish the mbx binary atomically on the SSH host",
        );
        if host.ssh().is_none() {
            set_executable(staged)?;
            std::fs::rename(staged, &program).context("publish staged mbx binary")?;
        } else {
            checked(executor.execute(&command)?, &command)?;
        }
        self.staged = None;
        check_cancelled(executor)?;
        run_setup(&host, &program, executor)?;
        let (profile_changed, profile) = update_profile(&host, &program, executor)?;

        check_cancelled(executor)?;
        let verified = probe_native_version(&host, executor)?
            .context("mbx was not found after installation")?;
        ensure!(
            verified.program == program,
            "mbx verification found {} instead of the installed path {}",
            verified.program.display(),
            program.display()
        );
        ensure!(
            verified.version == MBX_VERSION,
            "installed mbx reported version {}, expected {MBX_VERSION}",
            verified.version
        );
        Ok(MbxInstallResult {
            program: program.clone(),
            version: verified.version,
            profile_changed,
            profile_file: profile.file,
            profile_warning: profile.warning,
            manual_path_line: profile.manual_path_line,
            kind,
        })
    }
}

fn canonical_home(host: &CacheHost, executor: &impl CommandExecutor) -> Result<PathBuf> {
    let home = host.home(executor)?;
    let command = host.shell_command(
        r#"cd -- "$1" && pwd -P"#,
        "hel-mbx-home",
        [home.to_string_lossy().into_owned()],
        "resolve the container host's physical home directory",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let text = String::from_utf8(output.stdout).context("host home directory is not UTF-8")?;
    let home = PathBuf::from(text.trim());
    ensure!(
        home.is_absolute(),
        "container host home is not an absolute path"
    );
    Ok(home)
}

fn host_architecture(host: &CacheHost, executor: &impl CommandExecutor) -> Result<&'static str> {
    let command = host.command(
        vec!["uname".into(), "-m".into()],
        "detect mbx host architecture",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let architecture = std::str::from_utf8(&output.stdout)
        .context("decode mbx host architecture")?
        .trim();
    match architecture {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        _ => bail!("no pinned static mbx release for Linux architecture {architecture:?}"),
    }
}

fn destination(home: &Path, existing: Option<&NativeMbx>) -> Result<InstallDestination> {
    let Some(existing) = existing else {
        return Ok(InstallDestination::Supported {
            program: home.join(".local/bin/mbx"),
            kind: MbxInstallKind::Install,
        });
    };
    ensure!(
        existing.program.is_absolute(),
        "native mbx probe returned a non-absolute path"
    );
    let allowed = [home.join(".local/bin"), home.join(".cargo/bin")];
    if allowed
        .iter()
        .any(|directory| existing.program.strip_prefix(directory).is_ok())
    {
        return Ok(InstallDestination::Supported {
            program: existing.program.clone(),
            kind: MbxInstallKind::Upgrade,
        });
    }
    Ok(InstallDestination::Unsupported {
        program: existing.program.clone(),
    })
}

fn stage_binary(
    host: &CacheHost,
    home: &Path,
    binary: &Path,
    program: &Path,
    executor: &impl CommandExecutor,
) -> Result<PathBuf> {
    let parent = program
        .parent()
        .context("the selected mbx path has no parent directory")?;
    if host.ssh().is_none() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create mbx install directory {}", parent.display()))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary mbx binary in {}", parent.display()))?;
        let mut source = std::fs::File::open(binary)
            .with_context(|| format!("open downloaded mbx binary {}", binary.display()))?;
        std::io::copy(&mut source, temporary.as_file_mut())
            .context("copy downloaded mbx binary into the install directory")?;
        check_cancelled(executor)?;
        temporary.as_file_mut().sync_all()?;
        set_executable(temporary.path())?;
        return temporary
            .keep()
            .map(|(_, path)| path)
            .map_err(|error| error.error)
            .context("retain staged mbx binary");
    }

    let command = host.command(
        vec![
            "mkdir".into(),
            "-p".into(),
            "--".into(),
            parent.to_string_lossy().into_owned(),
        ],
        "create the mbx install directory",
    );
    checked(executor.execute(&command)?, &command)?;

    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".mbx.mjolnir-{}-{sequence}.tmp",
        std::process::id()
    ));
    let remote_relative = temporary
        .strip_prefix(home)
        .context("mbx install directory is outside the SSH host home")?
        .to_string_lossy()
        .into_owned();
    let ssh = host.ssh().expect("the SSH host was checked");
    let upload = targets::scp_upload(ssh, binary, &remote_relative, false)
        .purpose("upload the pinned mbx binary to the SSH host");
    if let Err(error) = executor
        .execute(&upload)
        .and_then(|output| checked(output, &upload))
    {
        cleanup_remote_staging(host, &temporary);
        return Err(error);
    }
    if let Err(error) = check_cancelled(executor) {
        cleanup_remote_staging(host, &temporary);
        return Err(error);
    }
    Ok(temporary)
}

fn cleanup_remote_staging(host: &CacheHost, path: &Path) {
    // Use the executor's existing 15-second cleanup bound, independent of the cancelled request.
    let cleanup = targets::CancellableProcessExecutor::new(std::sync::Arc::new(
        std::sync::atomic::AtomicBool::new(false),
    ));
    remove_remote_temporary(host, path, &cleanup);
}

fn remove_remote_temporary(host: &CacheHost, path: &Path, executor: &impl CommandExecutor) {
    let command = host.command(
        vec![
            "rm".into(),
            "-f".into(),
            "--".into(),
            path.to_string_lossy().into_owned(),
        ],
        "remove partial mbx install upload",
    );
    match executor.execute_cleanup(&command) {
        Ok(output) if output.status == 0 => {}
        Ok(output) => tracing::warn!(
            status = output.status,
            "could not remove a partial mbx installation upload: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => {
            tracing::warn!("could not remove a partial mbx installation upload: {error:#}")
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .context("make downloaded mbx executable")
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn run_setup(host: &CacheHost, program: &Path, executor: &impl CommandExecutor) -> Result<()> {
    let command = host.command(
        vec![
            program.to_string_lossy().into_owned(),
            "setup".into(),
            "--yes".into(),
        ],
        "run mbx setup on the container host",
    );
    checked(executor.execute(&command)?, &command).map(|_| ())
}

pub(super) fn login_profile_details(
    host: &CacheHost,
    executor: &impl CommandExecutor,
) -> Result<Option<MbxProfileDetails>> {
    let home = canonical_home(host, executor)?;
    let existing = probe_native_version(host, executor)?;
    let program = match destination(&home, existing.as_ref())? {
        InstallDestination::Supported { program, .. } => program,
        InstallDestination::Unsupported { .. } => return Ok(None),
    };
    let binary_dir = program
        .parent()
        .context("the selected mbx path has no parent directory")?;
    let selection = inspect_profile_selection(host, binary_dir, executor)?;
    Ok(Some(profile_details(&selection, binary_dir)))
}

fn inspect_profile_selection(
    host: &CacheHost,
    binary_dir: &Path,
    executor: &impl CommandExecutor,
) -> Result<ProfileSelection> {
    let command = host.shell_command(
        PROFILE_SCRIPT,
        "hel-mbx-profile",
        ["preview".into(), binary_dir.to_string_lossy().into_owned()],
        "resolve the host login profile for the mbx PATH block",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    parse_profile_selection(&output.stdout)
}

fn update_profile(
    host: &CacheHost,
    program: &Path,
    executor: &impl CommandExecutor,
) -> Result<(bool, MbxProfileDetails)> {
    let binary_dir = program
        .parent()
        .context("the selected mbx path has no parent directory")?;
    let selection = inspect_profile_selection(host, binary_dir, executor)?;
    let posix_block = posix_profile_block(binary_dir, &selection.shim_dir);
    let fish_block = fish_profile_block(binary_dir, &selection.shim_dir);
    let command = host.shell_command(
        PROFILE_SCRIPT,
        "hel-mbx-profile",
        [
            "update".into(),
            binary_dir.to_string_lossy().into_owned(),
            posix_block,
            fish_block,
        ],
        "add the mbx PATH block to the container host login profile",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let text = String::from_utf8(output.stdout).context("profile update result is not UTF-8")?;
    let mut lines = text.lines().map(str::to_owned);
    let status = lines.next().context("profile update returned no result")?;
    let updated_selection = selection_from_lines(&mut lines)?;
    let changed = match status.as_str() {
        "changed" => true,
        "unchanged" => false,
        _ => bail!("profile update returned an unexpected result: {status:?}"),
    };
    Ok((changed, profile_details(&updated_selection, binary_dir)))
}

fn parse_profile_selection(output: &[u8]) -> Result<ProfileSelection> {
    let text = std::str::from_utf8(output).context("host profile selection is not UTF-8")?;
    let mut lines = text.lines().map(str::to_owned);
    ensure!(
        lines.next().as_deref() == Some("preview"),
        "host profile selection returned an unexpected result"
    );
    selection_from_lines(&mut lines)
}

fn selection_from_lines(lines: &mut impl Iterator<Item = String>) -> Result<ProfileSelection> {
    let file = lines.next().context("profile selection returned no path")?;
    validate_profile_file(&file)?;
    let shell_kind = lines
        .next()
        .context("profile selection returned no shell kind")?;
    ensure!(
        matches!(shell_kind.as_str(), "posix" | "fish" | "unknown"),
        "host selected an unexpected login shell kind: {shell_kind:?}"
    );
    let shim_dir = lines
        .next()
        .context("profile selection returned no mbx shim directory")?;
    ensure!(
        Path::new(&shim_dir).is_absolute(),
        "host selected a non-absolute mbx shim directory: {shim_dir:?}"
    );
    ensure!(
        lines.next().is_none(),
        "profile selection returned unexpected extra fields"
    );
    Ok(ProfileSelection {
        file,
        shell_kind,
        shim_dir: PathBuf::from(shim_dir),
    })
}

fn validate_profile_file(file: &str) -> Result<()> {
    ensure!(
        file.starts_with("~/") || Path::new(file).is_absolute(),
        "host selected a non-absolute login profile path: {file:?}"
    );
    Ok(())
}

fn profile_details(selection: &ProfileSelection, binary_dir: &Path) -> MbxProfileDetails {
    let manual_path_line = (selection.shell_kind == "unknown").then(|| {
        let directories = format!("{}:{}", selection.shim_dir.display(), binary_dir.display());
        format!(
            "export PATH={}{}",
            shell_quote(&directories),
            "${PATH:+:$PATH}"
        )
    });
    let warning = manual_path_line.as_ref().map(|_| {
        "Your login shell may not read ~/.profile. Add this PATH line to a startup file it reads:"
            .to_owned()
    });
    MbxProfileDetails {
        file: selection.file.clone(),
        warning,
        manual_path_line,
    }
}

fn posix_profile_block(binary_dir: &Path, shim_dir: &Path) -> String {
    let binary_dir = shell_quote(&binary_dir.to_string_lossy());
    let shim_dir = shell_quote(&shim_dir.to_string_lossy());
    format!(
        r#"# >>> mbx (added by Mjolnir) >>>
_mjolnir_mbx_prepend_path() {{
    _mjolnir_mbx_dir=$1
    _mjolnir_mbx_rest=${{PATH-}}
    _mjolnir_mbx_new=
    _mjolnir_mbx_first=yes
    if [ -n "$_mjolnir_mbx_rest" ]; then
        while :; do
            case $_mjolnir_mbx_rest in
                *:*)
                    _mjolnir_mbx_entry=${{_mjolnir_mbx_rest%%:*}}
                    _mjolnir_mbx_rest=${{_mjolnir_mbx_rest#*:}}
                    _mjolnir_mbx_more=yes
                    ;;
                *)
                    _mjolnir_mbx_entry=$_mjolnir_mbx_rest
                    _mjolnir_mbx_more=no
                    ;;
            esac
            if [ "$_mjolnir_mbx_entry" != "$_mjolnir_mbx_dir" ]; then
                if [ "$_mjolnir_mbx_first" = yes ]; then
                    _mjolnir_mbx_new=$_mjolnir_mbx_entry
                    _mjolnir_mbx_first=no
                else
                    _mjolnir_mbx_new=$_mjolnir_mbx_new:$_mjolnir_mbx_entry
                fi
            fi
            [ "$_mjolnir_mbx_more" = yes ] || break
        done
    fi
    if [ "$_mjolnir_mbx_first" = yes ]; then
        PATH=$_mjolnir_mbx_dir
    else
        PATH=$_mjolnir_mbx_dir:$_mjolnir_mbx_new
    fi
}}
_mjolnir_mbx_prepend_path {binary_dir}
_mjolnir_mbx_prepend_path {shim_dir}
export PATH
# <<< mbx <<<"#
    )
}

fn fish_profile_block(binary_dir: &Path, shim_dir: &Path) -> String {
    let binary_dir = fish_quote(&binary_dir.to_string_lossy());
    let shim_dir = fish_quote(&shim_dir.to_string_lossy());
    format!(
        "# >>> mbx (added by Mjolnir) >>>\nfish_add_path --path --prepend --move -- {binary_dir}\nfish_add_path --path --prepend --move -- {shim_dir}\n# <<< mbx <<<"
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn check_cancelled(executor: &impl CommandExecutor) -> Result<()> {
    ensure!(
        !executor.cancellation_requested(),
        "mbx installation cancelled"
    );
    Ok(())
}

fn checked(output: CommandOutput, command: &CommandSpec) -> Result<CommandOutput> {
    ensure!(
        output.status == 0,
        "{} failed with status {}: {}",
        command.purpose,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn release_url(triple: &str) -> String {
    format!(
        "https://github.com/jdx/mr-boxington/releases/download/v{MBX_VERSION}/mbx-{triple}-unknown-linux-musl.tar.gz"
    )
}

fn expected_digest(triple: &str) -> Result<&'static str> {
    match triple {
        "x86_64" => Ok(MBX_X86_64_SHA256),
        "aarch64" => Ok(MBX_AARCH64_SHA256),
        _ => bail!("no pinned mbx release for {triple}"),
    }
}

/// Fetch with the same cancellable subprocess owner as host probes and uploads.
fn download(triple: &str, directory: &Path, executor: &impl CommandExecutor) -> Result<PathBuf> {
    let expected = expected_digest(triple)?;
    let archive_path = directory.join("release.tar.gz");
    fetch_archive(&release_url(triple), &archive_path, executor)?;
    check_cancelled(executor)?;
    let archive = std::fs::read(&archive_path).context("read staged mbx archive")?;
    let actual = mj_core::hex::lower_hex(Sha256::digest(&archive));
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "downloaded mbx checksum mismatch: expected {expected}, got {actual}"
    );
    let binary = extract_binary(&archive)?;
    check_cancelled(executor)?;
    let destination = directory.join("mbx");
    std::fs::write(&destination, binary).context("write verified staged mbx binary")?;
    set_executable(&destination)?;
    Ok(destination)
}

fn fetch_archive(url: &str, archive: &Path, executor: &impl CommandExecutor) -> Result<()> {
    let command = CommandSpec::new(
        "curl",
        [
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
            &archive.to_string_lossy(),
            url,
        ],
    )
    .purpose("download the pinned mbx release");
    checked(executor.execute(&command)?, &command)?;
    Ok(())
}

/// Extract the archive's single binary; its licence texts are not installed.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in reader.entries().context("read the mbx release archive")? {
        let mut entry = entry.context("read the mbx release archive")?;
        if entry.path().context("read an mbx archive path")?.as_ref() != Path::new("mbx") {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .context("read the mbx binary from its release archive")?;
        return Ok(bytes);
    }
    bail!("the mbx release archive contains no mbx binary")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use mj_core::config::{SshConnection, TargetBuildCache};

    #[derive(Default)]
    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        probe_answers: Mutex<VecDeque<CommandOutput>>,
        setup_error: Option<String>,
    }

    impl FakeExecutor {
        fn with_probes(probes: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                probe_answers: Mutex::new(probes.into_iter().collect()),
                setup_error: None,
            }
        }

        fn failing_setup(probes: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                setup_error: Some("mbx setup failed".into()),
                ..Self::with_probes(probes)
            }
        }

        fn commands(&self) -> Vec<CommandSpec> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.clone());
            let output = match command.purpose.as_str() {
                "detect build cache host platform" => output(0, "Linux x86_64\n", ""),
                "detect mbx host architecture" => output(0, "x86_64\n", ""),
                "read the container host mbx version" => self
                    .probe_answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("a probe answer was configured"),
                "run mbx setup on the container host" => self
                    .setup_error
                    .as_ref()
                    .map_or_else(|| output(0, "", ""), |error| output(7, "", error)),
                "resolve the host login profile for the mbx PATH block" => output(
                    0,
                    "preview\n~/.bash_profile\nposix\n/home/build/.local/share/mbx/bin",
                    "",
                ),
                "add the mbx PATH block to the container host login profile" => output(
                    0,
                    "changed\n~/.bash_profile\nposix\n/home/build/.local/share/mbx/bin",
                    "",
                ),
                _ => output(0, "", ""),
            };
            Ok(output)
        }
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn local_machine() -> Machine {
        Machine::Local { build_cache: None }
    }

    fn ssh_machine() -> Machine {
        Machine::Ssh {
            ssh: SshConnection {
                host: "builder.example.test".into(),
                user: Some("build".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            workspace_prefix: PathBuf::from(".local/share/mjolnir/workspaces"),
            build_cache: Some(TargetBuildCache::default()),
        }
    }

    fn absent_then(path: &Path, version: &str) -> [CommandOutput; 2] {
        [
            output(1, "", ""),
            output(0, &format!("{}\nmbx {version}\n", path.display()), ""),
        ]
    }

    fn cache_preview(
        native_mbx: Option<&str>,
        off_reason: Option<BuildCacheOff>,
    ) -> BuildCachePreview {
        BuildCachePreview {
            native_mbx: native_mbx.map(str::to_owned),
            mbx_profile_file: Some("~/.profile".into()),
            mbx_profile_warning: None,
            mbx_manual_path_line: None,
            directory: None,
            max_total_size: None,
            user_managed: false,
            application: mj_core::state::BuildCacheApplication::Pending,
            budget_note: None,
            stats: None,
            off_reason,
        }
    }

    #[test]
    fn install_offer_tracks_absent_old_compatible_and_unsupported_hosts() {
        assert_eq!(
            install_kind(&cache_preview(None, None)),
            Some(MbxInstallKind::Install)
        );
        assert_eq!(
            install_kind(&cache_preview(Some("1.21.0"), None)),
            Some(MbxInstallKind::Upgrade)
        );
        assert_eq!(
            install_kind(&cache_preview(Some("1.22.0"), None)),
            Some(MbxInstallKind::Upgrade)
        );
        assert_eq!(install_kind(&cache_preview(Some(MBX_VERSION), None)), None);
        let mut external = cache_preview(Some("1.22.0"), None);
        external.mbx_profile_file = None;
        assert_eq!(install_kind(&external), None);
        // A Mac reports its own older mbx, which the Linux release cannot
        // replace, as well as having none.
        for native in [None, Some("1.16.0")] {
            assert_eq!(
                install_kind(&cache_preview(
                    native,
                    Some(BuildCacheOff::UnsupportedPlatform("macOS".into()))
                )),
                None
            );
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn destination_installs_absent_mbx_and_replaces_only_managed_paths() {
        let home = Path::new("/home/build");
        assert_eq!(
            destination(home, None).unwrap(),
            InstallDestination::Supported {
                program: PathBuf::from("/home/build/.local/bin/mbx"),
                kind: MbxInstallKind::Install,
            }
        );
        for path in ["/home/build/.local/bin/mbx", "/home/build/.cargo/bin/mbx"] {
            assert_eq!(
                destination(
                    home,
                    Some(&NativeMbx {
                        program: PathBuf::from(path),
                        version: "1.21.0".into(),
                    })
                )
                .unwrap(),
                InstallDestination::Supported {
                    program: PathBuf::from(path),
                    kind: MbxInstallKind::Upgrade,
                }
            );
        }
        let unsupported = destination(
            home,
            Some(&NativeMbx {
                program: PathBuf::from("/home/build/.local/share/mise/installs/mbx/1.21/bin/mbx"),
                version: "1.21.0".into(),
            }),
        )
        .unwrap();
        assert_eq!(
            unsupported,
            InstallDestination::Unsupported {
                program: PathBuf::from("/home/build/.local/share/mise/installs/mbx/1.21/bin/mbx")
            }
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn local_install_copies_atomically_runs_setup_and_verifies_the_version() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home with ' quotes");
        std::fs::create_dir_all(&home).unwrap();
        let binary = temp.path().join("downloaded-mbx");
        std::fs::write(&binary, b"recorded mbx executable").unwrap();
        let program = home.join(".local/bin/mbx");
        let executor = FakeExecutor::with_probes(absent_then(&program, MBX_VERSION));

        let result = install_mbx_with(&local_machine(), &executor, Some(&home), |triple| {
            assert_eq!(triple, "x86_64");
            Ok(binary.clone())
        })
        .unwrap();

        assert_eq!(result.program, program);
        assert_eq!(result.version, MBX_VERSION);
        assert_eq!(result.kind, MbxInstallKind::Install);
        assert!(result.profile_changed);
        assert_eq!(result.profile_file, "~/.bash_profile");
        assert_eq!(std::fs::read(&program).unwrap(), b"recorded mbx executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&program).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let commands = executor.commands();
        let setup = commands
            .iter()
            .find(|command| command.purpose == "run mbx setup on the container host")
            .unwrap();
        assert_eq!(setup.program, program.to_string_lossy());
        assert_eq!(setup.args, ["setup", "--yes"]);
        assert!(commands.iter().any(|command| {
            command.purpose == "add the mbx PATH block to the container host login profile"
        }));
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn ssh_install_uses_scp_to_a_home_relative_sibling_then_renames_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("downloaded-mbx");
        std::fs::write(&binary, b"recorded mbx executable").unwrap();
        let home = PathBuf::from("/remote home/O'Neil");
        let program = home.join(".local/bin/mbx");
        let executor = FakeExecutor::with_probes(absent_then(&program, MBX_VERSION));

        let result =
            install_mbx_with(&ssh_machine(), &executor, Some(&home), |_| Ok(binary)).unwrap();

        assert_eq!(result.program, program);
        let commands = executor.commands();
        let upload = commands
            .iter()
            .find(|command| command.purpose == "upload the pinned mbx binary to the SSH host")
            .unwrap();
        assert_eq!(upload.program, "scp");
        assert!(upload.ssh_session.is_some());
        assert!(upload.args.iter().any(|arg| {
            arg.starts_with("build@builder.example.test:.local/bin/.mbx.mjolnir-")
                && arg.ends_with(".tmp")
        }));
        let publish = commands
            .iter()
            .find(|command| command.purpose == "publish the mbx binary atomically on the SSH host")
            .unwrap();
        assert!(publish.args.last().unwrap().contains("chmod 755"));
        assert!(publish.args.last().unwrap().contains("mv -f"));
        assert!(
            publish
                .args
                .last()
                .unwrap()
                .contains("'/remote home/O'\\''Neil/.local/bin/mbx'")
        );
        assert!(commands.iter().any(|command| {
            command.purpose == "run mbx setup on the container host"
                && command
                    .args
                    .last()
                    .unwrap()
                    .contains("'/remote home/O'\\''Neil/.local/bin/mbx'")
        }));
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn setup_failure_surfaces_stderr_and_stops_before_profile_changes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let binary = temp.path().join("downloaded-mbx");
        std::fs::write(&binary, b"recorded mbx executable").unwrap();
        let program = home.join(".local/bin/mbx");
        let executor = FakeExecutor::failing_setup([
            output(1, "", ""),
            output(0, &format!("{}\nmbx 1.21.0\n", program.display()), ""),
        ]);

        let error =
            install_mbx_with(&local_machine(), &executor, Some(&home), |_| Ok(binary)).unwrap_err();

        assert!(format!("{error:#}").contains("mbx setup failed"));
        assert!(!executor.commands().iter().any(|command| {
            command.purpose == "add the mbx PATH block to the container host login profile"
        }));
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn mise_install_refusal_happens_before_download_or_host_mutation() {
        let home = Path::new("/home/build");
        let executor = FakeExecutor::with_probes([output(
            0,
            "/home/build/.local/share/mise/installs/mbx/1.21/bin/mbx\nmbx 1.21.0\n",
            "",
        )]);
        let mut downloaded = false;

        let error = install_mbx_with(&local_machine(), &executor, Some(home), |_| {
            downloaded = true;
            bail!("download should not run")
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("mise upgrade"));
        assert!(!downloaded);
        assert!(executor.commands().iter().all(|command| {
            !command.purpose.starts_with("run mbx setup")
                && !command.purpose.starts_with("add the mbx PATH")
        }));
    }

    #[cfg(unix)]
    // Hard-won: 7f871fd03: inherited fixture writers made the shell probe silently select the wrong profile.
    #[test]
    fn profile_block_selects_and_updates_the_login_shell_file_idempotently() {
        struct Case {
            name: &'static str,
            login_shell: &'static str,
            existing: &'static [&'static str],
            selected: Option<&'static str>,
            shell_kind: &'static str,
        }
        let cases = [
            Case {
                name: "zsh",
                login_shell: "/bin/zsh",
                existing: &[".bash_profile"],
                selected: Some("~/.zprofile"),
                shell_kind: "posix",
            },
            Case {
                name: "bash-profile",
                login_shell: "/bin/bash",
                existing: &[".bash_profile", ".bash_login"],
                selected: Some("~/.bash_profile"),
                shell_kind: "posix",
            },
            Case {
                name: "bash-login",
                login_shell: "/usr/bin/bash",
                existing: &[".bash_login"],
                selected: Some("~/.bash_login"),
                shell_kind: "posix",
            },
            Case {
                name: "bash-default",
                login_shell: "/bin/bash",
                existing: &[],
                selected: Some("~/.profile"),
                shell_kind: "posix",
            },
            Case {
                name: "unknown",
                login_shell: "/usr/bin/nu",
                existing: &[".zprofile", ".bash_profile", ".bash_login"],
                selected: Some("~/.profile"),
                shell_kind: "unknown",
            },
            Case {
                name: "fish",
                login_shell: "/usr/bin/fish",
                existing: &[],
                selected: None,
                shell_kind: "fish",
            },
        ];

        for case in cases {
            let temp = tempfile::tempdir().unwrap();
            let home = temp
                .path()
                .join(format!("home {} with ' quotes", case.name));
            let tools = temp.path().join("tools");
            let xdg_config = temp.path().join("xdg config");
            let xdg_data = temp.path().join("xdg data with ' quote");
            let binary_dir = home.join(".local/bin");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&tools).unwrap();
            mj_core::test_hooks::install_fake_command(
                &tools,
                "id",
                "#!/bin/sh\nprintf 'profile-test-user\\n'\n",
            );
            mj_core::test_hooks::install_fake_command(
                &tools,
                "getent",
                "#!/bin/sh\nprintf 'profile-test-user:x:1000:1000::/tmp:%s\\n' \"$PROFILE_TEST_LOGIN_SHELL\"\n",
            );
            // A concurrent fork can keep fixture writers open past setup.
            // Only the dispatcher is executed; these files are read by sh.
            let _writers: Vec<_> = ["id", "getent"]
                .into_iter()
                .map(|name| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(tools.join(format!("{name}.script")))
                        .unwrap()
                })
                .collect();
            for name in case.existing {
                std::fs::write(home.join(name), "export EXISTING_PROFILE=yes").unwrap();
            }
            let executor =
                targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(5));
            let mut command = CommandSpec::new(
                "/bin/sh",
                [
                    "-c",
                    PROFILE_SCRIPT,
                    "mj-profile",
                    "preview",
                    binary_dir.to_str().unwrap(),
                ],
            );
            command
                .env
                .insert("HOME".into(), home.to_string_lossy().into_owned());
            command
                .env
                .insert("PATH".into(), format!("{}:/usr/bin:/bin", tools.display()));
            command
                .env
                .insert("PROFILE_TEST_LOGIN_SHELL".into(), case.login_shell.into());
            command.env.insert("SHELL".into(), "/bin/sh".into());
            command.env.insert(
                "XDG_CONFIG_HOME".into(),
                xdg_config.to_string_lossy().into_owned(),
            );
            command.env.insert(
                "XDG_DATA_HOME".into(),
                xdg_data.to_string_lossy().into_owned(),
            );
            let preview = executor.execute(&command).unwrap();
            let selection = parse_profile_selection(&preview.stdout).unwrap();
            let selected = case.selected.map(str::to_owned).unwrap_or_else(|| {
                xdg_config
                    .join("fish/conf.d/mbx.fish")
                    .to_string_lossy()
                    .into_owned()
            });
            assert_eq!(selection.file, selected);
            assert_eq!(selection.shell_kind, case.shell_kind);
            assert_eq!(selection.shim_dir, xdg_data.join("mbx/bin"));

            let posix_block = posix_profile_block(&binary_dir, &selection.shim_dir);
            let fish_block = fish_profile_block(&binary_dir, &selection.shim_dir);
            let update = CommandSpec::new(
                "/bin/sh",
                [
                    "-c",
                    PROFILE_SCRIPT,
                    "mj-profile",
                    "update",
                    binary_dir.to_str().unwrap(),
                    &posix_block,
                    &fish_block,
                ],
            );
            let mut update = update;
            update.env.clone_from(&command.env);
            let first_update = executor.execute(&update).unwrap();
            assert_eq!(
                std::str::from_utf8(&first_update.stdout).unwrap(),
                format!(
                    "changed\n{selected}\n{}\n{}",
                    case.shell_kind,
                    selection.shim_dir.display()
                )
            );
            let second_update = executor.execute(&update).unwrap();
            assert_eq!(
                std::str::from_utf8(&second_update.stdout).unwrap(),
                format!(
                    "unchanged\n{selected}\n{}\n{}",
                    case.shell_kind,
                    selection.shim_dir.display()
                )
            );
            let profile_path = if let Some(relative) = selected.strip_prefix("~/") {
                home.join(relative)
            } else {
                PathBuf::from(&selected)
            };
            let profile = std::fs::read_to_string(profile_path).unwrap();
            if case
                .existing
                .iter()
                .any(|name| selected == format!("~/{}", name))
            {
                assert!(profile.starts_with("export EXISTING_PROFILE=yes\n# >>> mbx"));
            }
            assert_eq!(
                profile.matches("# >>> mbx (added by Mjolnir) >>>").count(),
                1
            );
            assert_eq!(profile.matches("# <<< mbx <<<").count(), 1);
            for name in case.existing {
                if selected != format!("~/{}", name) {
                    assert!(
                        !std::fs::read_to_string(home.join(name))
                            .unwrap()
                            .contains("# >>> mbx")
                    );
                }
            }
            if case.shell_kind == "unknown" {
                let details = profile_details(&selection, &binary_dir);
                assert!(details.warning.unwrap().contains("may not read ~/.profile"));
                assert!(
                    details
                        .manual_path_line
                        .unwrap()
                        .starts_with("export PATH='")
                );
            }
            if case.shell_kind == "posix" {
                assert!(profile.contains(&shell_quote(&selection.shim_dir.to_string_lossy())));
                assert!(profile.contains(&shell_quote(&binary_dir.to_string_lossy())));
                assert!(!profile.contains("$HOME/.local/share/mbx/bin"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_selection_falls_back_to_shell_then_sh() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let tools = temp.path().join("tools");
        let xdg_config = temp.path().join("xdg config");
        let xdg_data = temp.path().join("xdg data");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("id"), "#!/bin/sh\nprintf 'test-user\\n'\n").unwrap();
        std::fs::write(tools.join("getent"), "#!/bin/sh\nexit 2\n").unwrap();
        for name in ["id", "getent"] {
            std::fs::set_permissions(tools.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let executor = targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(5));
        let binary_dir = home.join(".local/bin");
        let mut command = CommandSpec::new(
            "/bin/sh",
            [
                "-c",
                PROFILE_SCRIPT,
                "mj-profile",
                "preview",
                binary_dir.to_str().unwrap(),
            ],
        );
        command
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        command
            .env
            .insert("PATH".into(), format!("{}:/usr/bin:/bin", tools.display()));
        command.env.insert("SHELL".into(), "/usr/bin/zsh".into());
        command.env.insert(
            "XDG_CONFIG_HOME".into(),
            xdg_config.to_string_lossy().into_owned(),
        );
        command.env.insert(
            "XDG_DATA_HOME".into(),
            xdg_data.to_string_lossy().into_owned(),
        );
        assert_eq!(
            parse_profile_selection(&executor.execute(&command).unwrap().stdout)
                .unwrap()
                .file,
            "~/.zprofile"
        );
        command.env.insert("SHELL".into(), "".into());
        assert_eq!(
            parse_profile_selection(&executor.execute(&command).unwrap().stdout)
                .unwrap()
                .file,
            "~/.profile"
        );
    }

    #[test]
    fn profile_preview_and_install_use_one_shared_rule_script() {
        assert!(PROFILE_SCRIPT.contains("getent passwd"));
        assert!(PROFILE_SCRIPT.contains("${SHELL:-sh}"));
        assert!(PROFILE_SCRIPT.contains("grep -Fq \"$start\" \"$profile\""));
        assert!(PROFILE_SCRIPT.contains("fish/conf.d/mbx.fish"));
        assert!(PROFILE_SCRIPT.contains("XDG_DATA_HOME"));
        let directory = Path::new("/home/example/.local/bin");
        assert!(
            posix_profile_block(directory, Path::new("/home/example/.local/share/mbx/bin"))
                .contains("# >>> mbx (added by Mjolnir) >>>")
        );
        assert!(
            fish_profile_block(directory, Path::new("/home/example/.local/share/mbx/bin"))
                .contains("# <<< mbx <<<")
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1293: mbx cache host paths and Linux-host checks assume a Unix controller"
    )]
    fn posix_profile_moves_shim_and_binary_directories_ahead_of_existing_path_entries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let shim_dir = temp.path().join("xdg data's/mbx/bin");
        let binary_dir = temp.path().join("local bin's");
        std::fs::create_dir_all(&home).unwrap();
        let block = posix_profile_block(&binary_dir, &shim_dir);
        std::fs::write(home.join(".profile"), block).unwrap();
        let initial = format!(
            "/usr/bin:{}:/bin:{}:{}",
            binary_dir.display(),
            shim_dir.display(),
            binary_dir.display()
        );
        let mut command = CommandSpec::new(
            "/bin/sh",
            [
                "-c",
                ". \"$HOME/.profile\"; . \"$HOME/.profile\"; printf '%s' \"$PATH\"",
            ],
        );
        command
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        command.env.insert("PATH".into(), initial);
        let output = targets::ProcessExecutor.execute(&command).unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{}:{}:/usr/bin:/bin",
                shim_dir.display(),
                binary_dir.display()
            )
        );
    }

    #[test]
    fn fish_profile_is_idempotent_when_fish_is_installed() {
        let probe = targets::ProcessExecutor.execute(&CommandSpec::new("fish", ["--version"]));
        let Ok(probe) = probe else {
            return;
        };
        if probe.status != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = temp.path().join("xdg config");
        let data = temp.path().join("xdg data");
        let binary_dir = temp.path().join("binary");
        let shim_dir = temp.path().join("shim");
        std::fs::create_dir_all(config.join("fish/conf.d")).unwrap();
        std::fs::create_dir_all(&binary_dir).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();
        let file = config.join("fish/conf.d/mbx.fish");
        std::fs::write(&file, fish_profile_block(&binary_dir, &shim_dir)).unwrap();
        let path = format!(
            "/usr/bin:{}:/bin:{}:{}",
            binary_dir.display(),
            shim_dir.display(),
            binary_dir.display()
        );
        let fish_script = format!(
            "source '{}'; source '{}'; string join ':' $PATH",
            file.display(),
            file.display()
        );
        let mut command = CommandSpec::new("fish", ["-c", fish_script.as_str()]);
        command
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        command.env.insert(
            "XDG_CONFIG_HOME".into(),
            config.to_string_lossy().into_owned(),
        );
        command
            .env
            .insert("XDG_DATA_HOME".into(), data.to_string_lossy().into_owned());
        command.env.insert("PATH".into(), path);
        let output = targets::ProcessExecutor.execute(&command).unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!(
                "{}:{}:/usr/bin:/bin",
                shim_dir.display(),
                binary_dir.display()
            )
        );
    }

    #[test]
    fn installer_scripts_cover_absolute_candidates_and_marked_profile_paths() {
        for expected in [
            "command -v mbx",
            "$HOME/.local/bin/mbx",
            "$HOME/.cargo/bin/mbx",
            "cd -P",
            "case \"$resolved\" in /*)",
        ] {
            assert!(
                super::super::NATIVE_VERSION_SCRIPT.contains(expected),
                "missing {expected:?}"
            );
        }
    }
}
