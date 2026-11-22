//! Host tools and system facts the engine uses.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Context, Result, anyhow, bail};
use mj_core::subprocess::run_with_input;

/// Directories searched before `PATH`: where the host bootstrap installs
/// Firecracker and crane.
const TOOL_DIRECTORIES: &[&str] = &[
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// Locate an installed tool. The engine runs under `sudo`, whose `PATH` is not the caller's.
pub fn find_tool(name: &str) -> Result<PathBuf> {
    for directory in TOOL_DIRECTORIES {
        let candidate = PathBuf::from(directory).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "{name} is not installed on this host (searched {})",
        TOOL_DIRECTORIES.join(", ")
    ))
}

/// Run `program` and capture its output, whatever its exit status.
pub fn run_unchecked<S: AsRef<OsStr>>(program: &std::path::Path, args: &[S]) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    run_with_input(&mut command, &[]).with_context(|| format!("run {}", program.display()))
}

/// Run `program`, failing with its error output unless it exits zero.
pub fn run_checked<S: AsRef<OsStr>>(program: &std::path::Path, args: &[S]) -> Result<Output> {
    let output = run_unchecked(program, args)?;
    if !output.status.success() {
        bail!(
            "{} failed ({}): {}",
            program.display(),
            output.status,
            tail(&String::from_utf8_lossy(&output.stderr), 20)
        );
    }
    Ok(output)
}

/// Run a tool found by name.
pub fn run_tool<S: AsRef<OsStr>>(name: &str, args: &[S]) -> Result<Output> {
    run_checked(&find_tool(name)?, args)
}

/// The last `lines` lines of `text`.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn kernel_release() -> Result<String> {
    // SAFETY: utsname is plain old data and uname fills it.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid utsname.
    if unsafe { libc::uname(&mut name) } != 0 {
        return Err(std::io::Error::last_os_error()).context("uname");
    }
    // SAFETY: uname NUL-terminates `release`.
    Ok(unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The container platform architecture name for this host.
pub fn host_architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        other => bail!("unsupported host architecture {other}"),
    }
}

/// The user that ran `sudo`, if any: files handed back to the caller belong to them.
pub fn sudo_owner() -> Option<(u32, u32)> {
    let uid = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    Some((uid, gid))
}

/// The current UTC time in RFC 3339.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Seconds-since-epoch free-form unique suffix for scratch names.
pub fn scratch_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    )
}

/// Set by the termination signal handler during a launch, which then rolls back.
pub static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn note_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Turn SIGINT, SIGTERM and SIGHUP into a flag the launch loops check, so an
/// interrupted `run` removes what it created instead of dying half-built.
pub fn install_interrupt_handlers() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(signal, note_interrupt as *const () as libc::sighandler_t);
        }
    }
}

pub fn check_interrupted() -> Result<()> {
    if INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        bail!("interrupted");
    }
    Ok(())
}

/// Total bytes of the filesystem that holds `path`.
pub fn filesystem_bytes(path: &std::path::Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain old data and is filled by the call below.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and `stat` is a valid statvfs.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", path.display()));
    }
    Ok(stat.f_blocks as u64 * stat.f_frsize as u64)
}
