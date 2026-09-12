//! Target-account login exports, isolated from the process that launched Mjolnir.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

pub type Environment = BTreeMap<String, String>;

static ENVIRONMENT: tokio::sync::OnceCell<Environment> = tokio::sync::OnceCell::const_new();

/// The worker's internal re-exec passes a resolved environment explicitly.
/// Seed the cache before starting its runtime, without another shell startup.
pub fn initialize_from_parent() -> Result<()> {
    let environment = std::env::vars_os()
        .map(|(name, value)| {
            Ok((
                name.into_string()
                    .map_err(|_| anyhow::anyhow!("login environment name is not UTF-8"))?,
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("login environment value is not UTF-8"))?,
            ))
        })
        .collect::<Result<Environment>>()?;
    ENVIRONMENT
        .set(environment)
        .map_err(|_| anyhow::anyhow!("login environment already initialized"))
}

/// Successful discovery is shared by session launches in this process. Failed
/// startup is reported and may be retried; it never authorizes ambient inheritance.
pub async fn resolve() -> Result<&'static Environment> {
    ENVIRONMENT
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(discover)
                .await
                .context("target login environment task failed")?
        })
        .await
}

pub async fn with_overrides(overrides: &Environment) -> Result<Environment> {
    let mut environment = resolve().await?.clone();
    environment.extend(overrides.clone());
    Ok(environment)
}

/// Minimal account identity for recovery that must not depend on shell startup.
#[cfg(unix)]
pub fn bootstrap() -> Result<Environment> {
    Account::current()?.environment()
}

#[cfg(not(unix))]
pub fn bootstrap() -> Result<Environment> {
    anyhow::bail!("target login environment requires a Unix account")
}

#[cfg(not(unix))]
pub fn discover() -> Result<Environment> {
    anyhow::bail!("target login environment requires a Unix account")
}

#[cfg(unix)]
pub fn discover() -> Result<Environment> {
    discover_account(Account::current()?, std::time::Duration::from_secs(5))
}

#[cfg(unix)]
struct Account {
    user: String,
    home: std::path::PathBuf,
    shell: std::path::PathBuf,
}

#[cfg(unix)]
impl Account {
    fn environment(&self) -> Result<Environment> {
        let shell = self
            .shell
            .to_str()
            .context("target shell path is not UTF-8")?;
        let home = self
            .home
            .to_str()
            .context("target home path is not UTF-8")?;
        Ok(Environment::from([
            ("HOME".into(), home.into()),
            ("USER".into(), self.user.clone()),
            ("LOGNAME".into(), self.user.clone()),
            ("SHELL".into(), shell.into()),
            (
                "PATH".into(),
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
            ),
        ]))
    }

    fn current() -> Result<Self> {
        use std::ffi::CStr;
        let mut buffer = vec![0u8; 16384];
        loop {
            let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut result = std::ptr::null_mut();
            // SAFETY: getpwuid_r writes into live, correctly sized storage; all
            // returned strings are copied before the backing buffer is dropped.
            let status = unsafe {
                libc::getpwuid_r(
                    libc::geteuid(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
                buffer.resize(buffer.len() * 2, 0);
                continue;
            }
            anyhow::ensure!(
                status == 0,
                "read target account: {}",
                std::io::Error::from_raw_os_error(status)
            );
            anyhow::ensure!(
                !result.is_null(),
                "target account is absent from the account database"
            );
            // SAFETY: a successful lookup initialized entry and its string pointers.
            let (user, home, shell) = unsafe {
                let entry = entry.assume_init();
                (
                    CStr::from_ptr(entry.pw_name).to_str()?.to_owned(),
                    CStr::from_ptr(entry.pw_dir).to_str()?.to_owned(),
                    CStr::from_ptr(entry.pw_shell).to_str()?.to_owned(),
                )
            };
            return Ok(Self {
                user,
                home: home.into(),
                shell: if shell.is_empty() {
                    "/bin/sh".into()
                } else {
                    shell.into()
                },
            });
        }
    }
}

#[cfg(unix)]
fn discover_account(account: Account, timeout: std::time::Duration) -> Result<Environment> {
    use crate::hel_targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("create login capture marker: {error}"))?;
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let marker = format!("\0MJ_LOGIN_{nonce}\0");
    // Absolute env prevents a profile alias or PATH change from replacing the
    // exporter. exec avoids logout hooks being appended to the captured records.
    let script = format!("printf '\\000MJ_LOGIN_{nonce}\\000'; exec /usr/bin/env -0");
    let shell = account
        .shell
        .to_str()
        .context("target shell path is not UTF-8")?;
    let mut command = CommandSpec::new(shell, ["-l", "-c", &script])
        .purpose("initialize target login environment");
    command.clear_env = true;
    command.cwd = Some(account.home.clone());
    command.env = account.environment()?;
    let output = BoundedProcessExecutor::new(timeout).execute(&command)?;
    // Startup output can contain secrets. Never include it in errors or logs.
    anyhow::ensure!(
        output.status == 0,
        "target login shell exited with status {}",
        output.status
    );
    parse(&output.stdout, marker.as_bytes())
}

#[cfg(unix)]
fn parse(output: &[u8], marker: &[u8]) -> Result<Environment> {
    let start = output
        .windows(marker.len())
        .position(|part| part == marker)
        .context("target login shell did not produce an environment frame")?
        + marker.len();
    let records = &output[start..];
    anyhow::ensure!(
        records.ends_with(&[0]),
        "target login environment was truncated"
    );
    let mut environment = Environment::new();
    for record in records[..records.len() - 1].split(|byte| *byte == 0) {
        let record =
            std::str::from_utf8(record).context("target login environment is not UTF-8")?;
        let (name, value) = record
            .split_once('=')
            .context("invalid target login environment record")?;
        anyhow::ensure!(!name.is_empty(), "empty target login environment name");
        environment.insert(name.into(), value.into());
    }
    anyhow::ensure!(
        environment
            .get("PATH")
            .is_some_and(|value| !value.is_empty()),
        "target login environment has no PATH"
    );
    Ok(environment)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hel_targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    fn account(home: &std::path::Path) -> Account {
        Account {
            user: "fixture-user".into(),
            home: home.into(),
            shell: home.join("login shell"),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        // Model account startup independently of the machine's /etc/profile.
        // Verify noninteractive login flags and then source the fixture profile.
        let shell = home.path().join("login shell");
        std::fs::write(&shell, "#!/bin/sh\n[ \"$1\" = -l ] && [ \"$2\" = -c ] || exit 80\n. \"$HOME/.profile\"\nexec /bin/sh -c \"$3\"\n").unwrap();
        std::fs::set_permissions(shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            home.path().join(".profile"),
            "export PROFILE_SETTING='profile value'\n",
        )
        .unwrap();
        home
    }

    #[test]
    fn login_capture_ignores_parent_exports_and_preserves_profile_exports() {
        const HOME_KEY: &str = "MJ_TEST_LOGIN_HOME";
        let pollution = [
            "CARGO_TARGET_DIR",
            "RUSTFLAGS",
            "LD_LIBRARY_PATH",
            "PYTHONPATH",
            "VIRTUAL_ENV",
            "NODE_OPTIONS",
            "GIT_DIR",
            "CODEX_HOME",
            "MJ_DEV_RESTART_STALE_DAEMON",
            "ARBITRARY_PARENT_EXPORT",
        ];
        if let Some(home) = std::env::var_os(HOME_KEY) {
            let environment =
                discover_account(account(std::path::Path::new(&home)), Duration::from_secs(5))
                    .unwrap();
            for key in pollution {
                assert!(
                    !environment.contains_key(key),
                    "parent export survived: {key}"
                );
            }
            assert_eq!(environment["PROFILE_SETTING"], "profile value");
            assert_eq!(environment["MULTILINE"], "first\nsecond=third");
            assert_eq!(environment["LARGE"].len(), 80_000);
            assert_eq!(environment["USER"], "fixture-user");
            return;
        }
        let home = fixture();
        std::fs::write(home.path().join(".profile"), format!("printf 'startup chatter\\n'\nexport PROFILE_SETTING='profile value'\nexport MULTILINE='first\nsecond=third'\nexport LARGE='{}'\n", "x".repeat(80_000))).unwrap();
        let mut command = CommandSpec::new(
            std::env::current_exe().unwrap().to_str().unwrap(),
            [
                "--exact",
                "hel_login_environment::tests::login_capture_ignores_parent_exports_and_preserves_profile_exports",
                "--nocapture",
            ],
        );
        command
            .env
            .insert(HOME_KEY.into(), home.path().to_str().unwrap().into());
        for key in pollution {
            command.env.insert(key.into(), "parent-only-value".into());
        }
        let output = BoundedProcessExecutor::new(Duration::from_secs(15))
            .execute(&command)
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn capture_rejects_missing_or_truncated_frames_without_echoing_values() {
        let marker = b"\0marker\0";
        for output in [
            b"secret".as_slice(),
            b"\0marker\0PATH=secret",
            b"\0marker\0secret\0",
            b"\0marker\0=secret\0",
            b"\0marker\0OTHER=secret\0",
        ] {
            let error = parse(output, marker).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        let environment = parse(
            b"greeting\n\0marker\0PATH=/bin\0EMPTY=\0VALUE=a=b\nline\0",
            marker,
        )
        .unwrap();
        assert_eq!(environment["EMPTY"], "");
        assert_eq!(environment["VALUE"], "a=b\nline");
    }

    #[test]
    fn failed_login_never_returns_an_ambient_environment() {
        let home = fixture();
        std::fs::write(home.path().join(".profile"), "echo secret >&2\nexit 42\n").unwrap();
        let error = discover_account(account(home.path()), Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        assert!(error.contains("42"));
        assert!(!error.contains("secret"));
    }

    #[test]
    fn capture_deadline_includes_descendants_holding_output_pipes() {
        let home = fixture();
        std::fs::write(
            home.path().join(".profile"),
            "sleep 60 &\nprintf '%s' \"$!\" > \"$HOME/child.pid\"\nexit 0\n",
        )
        .unwrap();
        let started = Instant::now();
        let error = discover_account(account(home.path()), Duration::from_millis(250)).unwrap_err();
        assert!(error.to_string().contains("did not answer"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn explicit_session_settings_override_the_login_baseline() {
        let environment = with_overrides(&Environment::from([
            ("PATH".into(), "/explicit/bin".into()),
            ("SESSION_SETTING".into(), "explicit".into()),
        ]))
        .await
        .unwrap();
        assert_eq!(environment["PATH"], "/explicit/bin");
        assert_eq!(environment["SESSION_SETTING"], "explicit");
        assert!(environment.contains_key("HOME"));
    }
}
