//! Writing the host's Claude login on macOS (#1491).
//!
//! The Keychain item `Claude Code-credentials` is the only host login for
//! Claude there ([`crate::setup::read_claude_keychain_login`] reads it). The
//! daemon is the one refresher, but a pre-upgrade container or remote session
//! (or an old worker during a rollout) can still hold a full copy with the
//! refresh token. When that session refreshes, Anthropic rotates the shared
//! single-use token and the newer one exists only in the session. The sync
//! pulls such a copy back into the Keychain, as it does into the profile's
//! file on other hosts, and hands the session a stripped copy again.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use mj_core::config::{CLAUDE_KEYCHAIN_SERVICE, HarnessKind};
use mj_core::credentials::{
    CredentialInstall, CredentialSnapshot, CredentialWriteGuard, canonical_login_is_replaced_by,
    validate_credential_payload,
};

use crate::setup::{KeychainLogin, claude_keychain_account, read_claude_keychain_login};
use crate::targets::{CommandExecutor, CommandSpec};

/// The longest command line `security -i` accepts.
const SECURITY_INTERACTIVE_LINE_LIMIT: usize = 4032;

/// Replace Claude Code's Keychain login, as Claude Code does: the command goes
/// to `security -i` on stdin, so the secret is never in an argument list
/// another process could read. The account is Claude Code's own, so this
/// updates its item instead of adding a second one. Reads the item back,
/// because an interactive `security` does not report a failed command in its
/// exit status.
pub(crate) fn write_claude_keychain_login(
    executor: &(impl CommandExecutor + ?Sized),
    login: &[u8],
) -> Result<()> {
    let line = format!(
        "add-generic-password -U -a {} -s \"{CLAUDE_KEYCHAIN_SERVICE}\" -X {}\n",
        claude_keychain_account(),
        mj_core::hex::lower_hex(login),
    );
    // `security -i` reads at most this many bytes per command line (Claude
    // Code 2.1.321 checks the same 4032 before using it). A longer line would
    // be cut short and could store a truncated login. Claude Code then passes
    // the login as an argument, where other processes can read it; Mjolnir
    // reports the failure instead (the sync step fails visibly).
    if line.len() > SECURITY_INTERACTIVE_LINE_LIMIT {
        bail!(
            "Claude Code's login is too large ({} bytes) to write to the macOS Keychain through `security -i`",
            login.len()
        );
    }
    let mut command = CommandSpec::new("security", ["-i"])
        .purpose("write Claude Code authentication to the macOS Keychain")
        .with_sensitive_stdin(line.into_bytes());
    command.sensitive_output = true;
    let output = executor
        .execute(&command)
        .map_err(|error| error.context("write Claude Code's login to the macOS Keychain"))?;
    if output.status != 0 {
        bail!(
            "the macOS Keychain refused Claude Code's login (security exited with status {})",
            output.status
        );
    }
    match read_claude_keychain_login(executor)? {
        KeychainLogin::Present(stored) if stored.trim_ascii_end() == login.trim_ascii_end() => {
            Ok(())
        }
        KeychainLogin::Present(_) => {
            bail!("the macOS Keychain does not hold the login that was just written")
        }
        KeychainLogin::Missing => bail!("the macOS Keychain has no login after the write"),
        KeychainLogin::Unreadable { status } => bail!(
            "the macOS Keychain could not be read back after the write (security exited with status {status})"
        ),
    }
}

/// Install a session's fresher login into the Keychain, only when it is
/// strictly newer and does not lose a refresh token the current one holds (the
/// rule of [`mj_core::credentials::install_credentials_if_newer`]). Holds the
/// locks Claude Code takes for `~/.claude` (`lock_marker` is its credential
/// file, [`mj_core::config::HarnessLoginStore::lock_marker`]), rereads the
/// Keychain under them, and writes under them, so Claude Code's own refresh and
/// this install cannot interleave. A Keychain that cannot be read or written
/// fails; it never falls back to a file.
pub(crate) async fn install_keychain_login_if_newer(
    lock_marker: &Path,
    security: Arc<dyn CommandExecutor + Send + Sync>,
    bytes: &[u8],
) -> Result<CredentialInstall> {
    let kind = HarnessKind::Claude;
    validate_credential_payload(kind, bytes)?;
    let guard = CredentialWriteGuard::acquire(kind, lock_marker).await?;
    let bytes = bytes.to_owned();
    tokio::task::spawn_blocking(move || {
        // Blocking writes cannot be cancelled: ownership follows the write.
        let _guard = guard;
        let (snapshot, current) = match read_claude_keychain_login(&*security)? {
            KeychainLogin::Present(current) => (CredentialSnapshot::of(kind, &current), current),
            KeychainLogin::Missing => (CredentialSnapshot::absent(), Vec::new()),
            KeychainLogin::Unreadable { status } => {
                bail!("the macOS Keychain could not be read (security exited with status {status})")
            }
        };
        let incoming = CredentialSnapshot::of(kind, &bytes);
        if !canonical_login_is_replaced_by(kind, (&snapshot, &current), (&incoming, &bytes)) {
            return Ok(CredentialInstall {
                snapshot,
                installed: false,
            });
        }
        write_claude_keychain_login(&*security, &bytes)?;
        Ok(CredentialInstall {
            snapshot: incoming,
            installed: true,
        })
    })
    .await
    .context("install the macOS Keychain login task stopped")?
}

/// A hand-written macOS Keychain for tests: `security` as Claude Code's item
/// answers it and as `security -i` writes it.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::targets::CommandOutput;

    #[derive(Default)]
    pub(crate) struct FakeKeychain {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        login: Option<Vec<u8>>,
        /// Every call fails as `security` does on a locked Keychain.
        locked: bool,
        /// Writes fail while reads work, as when the Keychain refuses to add.
        refuse_writes: bool,
        /// A directory whose Claude Code refresh lock a write must hold.
        lock_dir: Option<std::path::PathBuf>,
        /// Every call: its arguments, and what it was given on stdin.
        calls: Vec<(Vec<String>, Option<Vec<u8>>)>,
    }

    impl FakeKeychain {
        pub(crate) fn holding(login: &[u8]) -> Self {
            let keychain = Self::default();
            keychain.state.lock().unwrap().login = Some(login.to_vec());
            keychain
        }

        pub(crate) fn locked(self) -> Self {
            self.state.lock().unwrap().locked = true;
            self
        }

        /// Fail any write made without Claude Code's refresh lock in `dir`.
        #[cfg(unix)]
        pub(crate) fn requiring_lock_in(self, dir: &std::path::Path) -> Self {
            self.state.lock().unwrap().lock_dir = Some(dir.canonicalize().unwrap());
            self
        }

        #[cfg(unix)]
        pub(crate) fn refuse_writes(&self) {
            self.state.lock().unwrap().refuse_writes = true;
        }

        #[cfg(unix)]
        pub(crate) fn login(&self) -> Option<Vec<u8>> {
            self.state.lock().unwrap().login.clone()
        }

        pub(crate) fn calls(&self) -> Vec<(Vec<String>, Option<Vec<u8>>)> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl CommandExecutor for FakeKeychain {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            assert_eq!(command.program, "security");
            assert!(command.sensitive_output, "a login passes through here");
            let stdin = command.to_node_run(false).sensitive_stdin;
            let mut state = self.state.lock().unwrap();
            state.calls.push((command.args.clone(), stdin.clone()));
            let status = |status| {
                Ok(CommandOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            };
            if state.locked {
                return status(36);
            }
            if command.args.first().map(String::as_str) == Some("-i") {
                assert_eq!(command.args, ["-i"], "the secret is never an argument");
                if state.refuse_writes {
                    return status(36);
                }
                if let Some(dir) = &state.lock_dir {
                    assert!(
                        dir.join(".oauth_refresh.lock").is_dir(),
                        "the Keychain is written under Claude Code's refresh lock"
                    );
                }
                let line = String::from_utf8(stdin.expect("the command comes on stdin")).unwrap();
                let words = line.split_whitespace().collect::<Vec<_>>();
                assert_eq!(
                    words[..6],
                    [
                        "add-generic-password",
                        "-U",
                        "-a",
                        &claude_keychain_account(),
                        "-s",
                        "\"Claude"
                    ]
                );
                assert_eq!(words[6], "Code-credentials\"");
                assert_eq!(words[7], "-X");
                let hex = words[8];
                state.login = Some(
                    (0..hex.len())
                        .step_by(2)
                        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
                        .collect(),
                );
                return status(0);
            }
            assert_eq!(
                command.args,
                [
                    "find-generic-password",
                    "-a",
                    &claude_keychain_account(),
                    "-s",
                    CLAUDE_KEYCHAIN_SERVICE,
                    "-w"
                ]
            );
            match &state.login {
                None => status(44),
                Some(login) => Ok(CommandOutput {
                    status: 0,
                    stdout: login.clone(),
                    stderr: Vec::new(),
                }),
            }
        }
    }
}
