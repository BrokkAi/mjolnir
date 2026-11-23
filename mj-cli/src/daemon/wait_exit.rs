//! Wait for one daemon process and record its abnormal exit.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use mj_core::daemon_exit::{
    DAEMON_EXIT_SCHEMA, DaemonExitRecord, STORE_OWNED_EXIT_CODE, write_daemon_exit_record,
};

#[derive(Debug, clap::Args)]
pub(crate) struct DaemonWaitExitArgs {
    #[arg(long)]
    exit_dir: PathBuf,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[cfg(unix)]
const WAITER_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

pub(crate) fn run(args: DaemonWaitExitArgs) -> Result<()> {
    let (program, child_args) = args
        .command
        .split_first()
        .context("missing daemon program")?;
    let mut command = Command::new(program);
    #[cfg(target_os = "linux")]
    {
        // systemd-run execs the live waiter's inode, which survives an upgrade.
        let executable = OsString::from(format!("/proc/{}/exe", std::process::id()));
        command.args(child_args.iter().map(|arg| {
            if program != "/proc/self/exe" && arg == "/proc/self/exe" {
                &executable
            } else {
                arg
            }
        }));
    }
    #[cfg(not(target_os = "linux"))]
    command.args(child_args);

    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::process::CommandExt;
        use std::path::Path;

        if (program == "/proc/self/exe" || Path::new(program).file_name() == Some(OsStr::new("mj")))
            && child_args.first().map(OsString::as_os_str) == Some(OsStr::new("daemon-run"))
        {
            command.arg0("mj");
        }

        // SAFETY: These are valid signals and the waiter is single-threaded.
        unsafe {
            for signal in WAITER_SIGNALS {
                libc::signal(signal, libc::SIG_IGN);
            }
            command.pre_exec(|| {
                // Ignored dispositions survive exec. Restore defaults in the
                // child so the daemon can install its own shutdown handlers.
                for signal in WAITER_SIGNALS {
                    if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    // Inherit stdio without creating any pipes. The waiter owns no daemon state.
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn daemon program {program:?}"))?;
    let child_pid = child.id();
    let status = child.wait().context("wait for daemon process")?;
    if matches!(status.code(), Some(0 | STORE_OWNED_EXIT_CODE)) {
        return Ok(());
    }

    #[cfg(unix)]
    let (signal, signal_name, core_dumped) = {
        use mj_core::daemon_exit::unix_signal_name;
        use std::os::unix::process::ExitStatusExt;
        let signal = status.signal();
        (
            signal,
            signal.and_then(unix_signal_name).map(str::to_owned),
            signal.map(|_| status.core_dumped()),
        )
    };
    #[cfg(not(unix))]
    let (signal, signal_name, core_dumped) = (None, None, None);

    let record = DaemonExitRecord {
        schema: DAEMON_EXIT_SCHEMA,
        pid: child_pid,
        waiter_pid: Some(std::process::id()),
        exited_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        exit_code: status.code(),
        signal,
        signal_name,
        core_dumped,
    };
    if let Err(error) = write_daemon_exit_record(&args.exit_dir, &record) {
        eprintln!("could not record exit of daemon {child_pid}: {error:#}");
    }
    Ok(())
}
