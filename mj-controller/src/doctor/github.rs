//! Whether a bare target's host can reach GitHub with nobody present.
//!
//! A bare target runs Git as the host's own account. Mjolnir's source fetch
//! and publication checks use that account's credentials, and an agent's push
//! goes through a generated Git config that includes the account's own
//! `~/.gitconfig`. So the host needs a GitHub SSH key, or HTTPS through a
//! credential helper together with `insteadOf` rules that send SSH GitHub
//! URLs over HTTPS. Doctor reports which one the host has; it changes nothing.
//! Its findings are warnings: a host that never uses GitHub needs neither.

use mj_core::remote_git::GITHUB_SSH_REWRITE_PREFIXES;

use super::*;

const SSH_PROBE: [&str; 9] = [
    "ssh",
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "ConnectTimeout=5",
    "git@github.com",
];

/// A remote in each spelling Git accepts for GitHub over SSH.
fn ssh_remotes() -> [String; 2] {
    GITHUB_SSH_REWRITE_PREFIXES.map(|prefix| format!("{prefix}octocat/hello-world.git"))
}

/// The command that makes Git send each of those spellings over HTTPS.
pub(super) fn rewrite_commands() -> [String; 2] {
    GITHUB_SSH_REWRITE_PREFIXES.map(|prefix| {
        format!("git config --global --add url.https://github.com/.insteadOf {prefix}")
    })
}

const FINGERPRINTS_URL: &str = "https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints";

/// Where a bare target's commands run.
#[derive(Clone, Copy)]
pub(super) enum BareHost<'a> {
    Local,
    Ssh(&'a RuntimeSshTarget),
}

impl BareHost<'_> {
    fn location(&self) -> &str {
        match self {
            Self::Local => "this machine",
            Self::Ssh(ssh) => &ssh.destination,
        }
    }

    /// `args` as a command on the host, with `env` set for it alone.
    fn command(&self, args: &[&str], env: &[(&str, &str)], purpose: &'static str) -> CommandSpec {
        match self {
            Self::Local => {
                let mut command =
                    CommandSpec::new(args[0], args[1..].iter().copied()).purpose(purpose);
                command
                    .env
                    .extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
                command
            }
            Self::Ssh(ssh) => {
                let remote = (!env.is_empty())
                    .then(|| "env".to_owned())
                    .into_iter()
                    .chain(env.iter().map(|(k, v)| format!("{k}={v}")))
                    .chain(args.iter().map(|arg| (*arg).to_owned()))
                    .collect();
                strict_ssh_validation_command(ssh, remote, purpose)
            }
        }
    }
}

/// Why `ssh -T git@github.com` did not authenticate.
struct SshRefusal {
    kind: SshFailure,
    /// What to show the user: the first line OpenSSH printed, or why the
    /// probe could not run.
    reported: String,
}

enum GithubSsh {
    Authenticated,
    Refused(SshRefusal),
}

fn probe_github_ssh(host: BareHost<'_>, executor: &impl CommandExecutor) -> GithubSsh {
    let command = host.command(&SSH_PROBE, &[], "check GitHub SSH access");
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            let kind = if error.downcast_ref::<CommandTimedOut>().is_some() {
                SshFailure::Unreachable
            } else {
                SshFailure::Unrecognized
            };
            return GithubSsh::Refused(SshRefusal {
                kind,
                reported: format!("{error:#}"),
            });
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // GitHub greets with exit status 1 even when the key is accepted.
    if stderr.contains("successfully authenticated")
        || stdout.contains("successfully authenticated")
    {
        return GithubSsh::Authenticated;
    }
    GithubSsh::Refused(SshRefusal {
        kind: classify_ssh_stderr(&stderr),
        reported: stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output")
            .to_owned(),
    })
}

enum HttpsCredentials {
    Supplied,
    /// No helper answered, or the probe did not finish. Holds only our own
    /// description: the probe's output can carry a token and is never kept.
    Unavailable(String),
}

/// Ask Git's credential machinery for github.com without ever prompting.
///
/// Only the presence of a `password=` line is read from the output. The output
/// is a real token, so it is marked sensitive and nothing derived from it is
/// kept.
fn probe_https_credentials(
    host: BareHost<'_>,
    executor: &impl CommandExecutor,
) -> HttpsCredentials {
    let mut command = host
        .command(
            &["git", "credential", "fill"],
            &[
                ("GIT_TERMINAL_PROMPT", "0"),
                // A failing askpass makes Git give up instead of prompting.
                ("GIT_ASKPASS", "false"),
                ("GCM_INTERACTIVE", "never"),
            ],
            "check GitHub HTTPS credentials",
        )
        .with_sensitive_stdin(b"protocol=https\nhost=github.com\n\n".to_vec());
    command.sensitive_output = true;
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => {
            let supplied = output
                .stdout
                .split(|byte| *byte == b'\n')
                .any(|line| line.strip_prefix(b"password=").is_some_and(|rest| !rest.is_empty()));
            if supplied {
                HttpsCredentials::Supplied
            } else {
                HttpsCredentials::Unavailable(
                    "no Git credential helper supplies a password for github.com".to_owned(),
                )
            }
        }
        Ok(_) => HttpsCredentials::Unavailable(
            "no Git credential helper supplies a password for github.com".to_owned(),
        ),
        Err(error) if error.downcast_ref::<CommandTimedOut>().is_some() => {
            HttpsCredentials::Unavailable(
                "`git credential fill` did not answer in time; a credential helper may be waiting for input"
                    .to_owned(),
            )
        }
        Err(error) => HttpsCredentials::Unavailable(format!(
            "could not run `git credential fill`: {error:#}"
        )),
    }
}

/// Whether Git on the host sends `remote` over HTTPS, with or without a
/// username in the rewritten URL. Local: no network.
fn rewrites_to_https(host: BareHost<'_>, remote: &str, executor: &impl CommandExecutor) -> bool {
    let command = host.command(
        &["git", "ls-remote", "--get-url", remote],
        &[],
        "check GitHub URL rewriting",
    );
    matches!(
        executor.execute(&command),
        Ok(output) if output.status == 0
            && String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with("https://")
    )
}

/// The one check for a bare target's GitHub access.
///
/// Git picks the transport per URL spelling: a spelling an `insteadOf` rule
/// rewrites goes over HTTPS whatever SSH could do, so the rewrite decides
/// which credentials have to work. Every outcome but ready is a warning: a
/// target that never touches GitHub works without any of this.
pub(super) fn github_access_check(
    check_id: String,
    title: String,
    host: BareHost<'_>,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let location = host.location();
    let ssh_remotes = ssh_remotes();
    let (over_https, over_ssh): (Vec<usize>, Vec<usize>) = (0..ssh_remotes.len())
        .partition(|&index| rewrites_to_https(host, &ssh_remotes[index], executor));
    let refusal = if over_ssh.is_empty() {
        None
    } else {
        match probe_github_ssh(host, executor) {
            GithubSsh::Authenticated => None,
            GithubSsh::Refused(refusal) => Some(refusal),
        }
    };
    // HTTPS matters for rewritten spellings, and for a host whose SSH fails
    // it tells whether adding the rewrite would be enough.
    let https = (!over_https.is_empty() || refusal.is_some())
        .then(|| probe_https_credentials(host, executor));
    let https_gap = match &https {
        Some(HttpsCredentials::Unavailable(gap)) => Some(gap.as_str()),
        _ => None,
    };
    let remotes = |indexes: &[usize]| {
        indexes
            .iter()
            .map(|&index| ssh_remotes[index].as_str())
            .collect::<Vec<_>>()
            .join(" and ")
    };
    match (&refusal, https_gap) {
        (None, None) => {
            let route = match (over_ssh.is_empty(), over_https.is_empty()) {
                (true, _) => format!(
                    "GitHub over HTTPS: Git on {location} rewrites SSH GitHub URLs to HTTPS, and a Git credential helper there supplies github.com credentials."
                ),
                (_, true) => format!(
                    "GitHub over SSH: `ssh -T git@github.com` authenticates noninteractively on {location}."
                ),
                _ => format!(
                    "GitHub over SSH for {} and over HTTPS for {}: on {location}, `ssh -T git@github.com` authenticates and a Git credential helper supplies github.com credentials.",
                    remotes(&over_ssh),
                    remotes(&over_https)
                ),
            };
            DoctorCheck::ready(check_id, title, route)
        }
        (None, Some(gap)) => DoctorCheck::warning(
            check_id,
            title,
            format!(
                "Git on {location} rewrites {} to HTTPS, but {gap}. Mjolnir's source fetch and publication checks there will fail for those GitHub remotes; agents' pushes in a session use the session's synced GitHub token instead.",
                remotes(&over_https)
            ),
            format!(
                "On {location}, run `gh auth login` then `gh auth setup-git` (it records gh's absolute path, which non-interactive SSH needs when gh is not on PATH). Or, if that host uses a GitHub SSH key, remove the rewrite with `git config --global --unset-all url.https://github.com/.insteadOf`. Rerun `mj doctor` afterwards."
            ),
        ),
        (Some(refusal), None) => DoctorCheck::warning(
            check_id,
            title,
            format!(
                "A Git credential helper on {location} supplies github.com credentials, but Git there does not rewrite {} to HTTPS, and SSH fails: {}. Agents' pushes to SSH GitHub remotes will fail.",
                remotes(&over_ssh),
                ssh_gap(refusal)
            ),
            format!(
                "On {location}, run {}, then rerun `mj doctor`.",
                over_ssh
                    .iter()
                    .map(|&index| format!("`{}`", rewrite_commands()[index]))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        ),
        (Some(refusal), Some(gap)) => DoctorCheck::warning(
            check_id,
            title,
            format!(
                "{location} cannot reach GitHub noninteractively. SSH: {}. HTTPS: {gap}.",
                ssh_gap(refusal)
            ),
            no_access_remediation(location, refusal.kind, !over_https.is_empty()),
        ),
    }
}

fn ssh_gap(refusal: &SshRefusal) -> String {
    match refusal.kind {
        SshFailure::UntrustedHostKey => format!(
            "GitHub's host key is unknown or has changed ({})",
            refusal.reported
        ),
        SshFailure::Unauthenticated => format!("GitHub accepted no SSH key ({})", refusal.reported),
        SshFailure::Unreachable => format!("github.com did not answer ({})", refusal.reported),
        SshFailure::ClientMissing | SshFailure::Unrecognized => {
            format!("`ssh -T git@github.com` failed ({})", refusal.reported)
        }
    }
}

/// `rewritten`: Git already sends some SSH spelling over HTTPS, so an SSH key
/// alone would not fix it.
fn no_access_remediation(location: &str, ssh: SshFailure, rewritten: bool) -> String {
    let ssh_fix = match ssh {
        SshFailure::UntrustedHostKey => format!(
            "Trust GitHub's SSH host key on {location}: run `ssh -T git@github.com` there by hand, accept the key only if its fingerprint matches {FINGERPRINTS_URL}, and if the key changed run `ssh-keygen -R github.com` first"
        ),
        SshFailure::Unreachable => format!(
            "Make sure {location} can reach github.com on port 22, then add a GitHub SSH key there"
        ),
        SshFailure::Unauthenticated | SshFailure::ClientMissing | SshFailure::Unrecognized => {
            format!(
                "Add a GitHub SSH key to {location} (`ssh-keygen -t ed25519`, then add the public key at https://github.com/settings/keys)"
            )
        }
    };
    let ssh_fix = if rewritten {
        format!(
            "{ssh_fix}, and remove the HTTPS rewrite with `git config --global --unset-all url.https://github.com/.insteadOf`"
        )
    } else {
        ssh_fix
    };
    let [scp_rewrite, ssh_rewrite] = rewrite_commands();
    format!(
        "{ssh_fix}. Or use HTTPS: on {location}, run `gh auth login` then `gh auth setup-git` (it records gh's absolute path, which non-interactive SSH needs when gh is not on PATH), then run `{scp_rewrite}` and `{ssh_rewrite}`. Rerun `mj doctor` afterwards."
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::targets::CommandOutput;

    const TOKEN: &str = "SECRET-TOKEN";

    /// Answers each command in order and records what it was asked to run.
    struct Script {
        commands: RefCell<Vec<CommandSpec>>,
        responses: RefCell<Vec<Result<CommandOutput>>>,
    }

    impl Script {
        fn new(responses: impl IntoIterator<Item = Result<CommandOutput>>) -> Self {
            Self {
                commands: RefCell::default(),
                responses: RefCell::new(responses.into_iter().collect()),
            }
        }
    }

    impl CommandExecutor for Script {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            self.responses.borrow_mut().remove(0)
        }
    }

    fn exit(status: i32, stdout: &str, stderr: &str) -> Result<CommandOutput> {
        Ok(CommandOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    fn greeting() -> Result<CommandOutput> {
        // GitHub exits 1 even when it accepts the key.
        exit(
            1,
            "",
            "Hi octocat! You've successfully authenticated, but GitHub does not provide shell access.\n",
        )
    }

    fn denied() -> Result<CommandOutput> {
        exit(255, "", "git@github.com: Permission denied (publickey).\n")
    }

    fn credential() -> Result<CommandOutput> {
        exit(
            0,
            &format!("protocol=https\nhost=github.com\nusername=octocat\npassword={TOKEN}\n"),
            "",
        )
    }

    fn rewritten() -> Result<CommandOutput> {
        exit(0, "https://github.com/octocat/hello-world.git\n", "")
    }

    fn unrewritten() -> Result<CommandOutput> {
        exit(0, "git@github.com:octocat/hello-world.git\n", "")
    }

    fn run(responses: impl IntoIterator<Item = Result<CommandOutput>>) -> (DoctorCheck, Script) {
        let script = Script::new(responses);
        let check = github_access_check(
            "github.test".into(),
            "GitHub".into(),
            BareHost::Local,
            &script,
        );
        (check, script)
    }

    /// Each outcome tells the user which of SSH and HTTPS works and what to
    /// run next, and the credential probe's token reaches no check or
    /// recorded command line.
    #[test]
    fn github_access_outcomes_name_the_working_route_and_never_carry_the_token() {
        let no_password = || {
            exit(
                128,
                "",
                "fatal: could not read Username for 'https://github.com'\n",
            )
        };
        let cases = [
            // Neither spelling rewritten and SSH works; HTTPS is not asked.
            (
                vec![unrewritten(), unrewritten(), greeting()],
                CheckStatus::Ready,
                "GitHub over SSH",
            ),
            // Both spellings rewritten and HTTPS answers; SSH is not asked.
            (
                vec![rewritten(), rewritten(), credential()],
                CheckStatus::Ready,
                "GitHub over HTTPS",
            ),
            // One spelling each way, and both routes work.
            (
                vec![rewritten(), unrewritten(), greeting(), credential()],
                CheckStatus::Ready,
                "over SSH for ssh://git@github.com/octocat/hello-world.git",
            ),
            // HTTPS answers, SSH fails, and one spelling is not rewritten.
            (
                vec![rewritten(), unrewritten(), denied(), credential()],
                CheckStatus::Warning,
                "url.https://github.com/.insteadOf ssh://git@github.com/",
            ),
            // Unknown GitHub host key and no credentials.
            (
                vec![
                    unrewritten(),
                    unrewritten(),
                    exit(
                        255,
                        "",
                        "No ED25519 host key is known for github.com\nHost key verification failed.\n",
                    ),
                    no_password(),
                ],
                CheckStatus::Warning,
                "host key is unknown or has changed",
            ),
            // A key GitHub does not know and a credential helper that answers
            // without a password.
            (
                vec![
                    unrewritten(),
                    unrewritten(),
                    denied(),
                    exit(0, "username=octocat\npassword=\n", ""),
                ],
                CheckStatus::Warning,
                "GitHub accepted no SSH key",
            ),
            // Hard-won: f37af2cdd: the SSH remedy left a rewrite sending GitHub over HTTPS without credentials.
            (
                vec![rewritten(), unrewritten(), denied(), no_password()],
                CheckStatus::Warning,
                "remove the HTTPS rewrite with `git config --global --unset-all url.https://github.com/.insteadOf`",
            ),
            // Hard-won: e32a6aeef: a working SSH key reported ready although insteadOf sent GitHub over HTTPS without credentials.
            (
                vec![rewritten(), rewritten(), no_password()],
                CheckStatus::Warning,
                "rewrites git@github.com:octocat/hello-world.git and ssh://git@github.com/octocat/hello-world.git to HTTPS",
            ),
        ];
        for (responses, status, expected) in cases {
            let (check, script) = run(responses);
            assert_eq!(check.status, status, "{check:?}");
            let text = format!(
                "{} {}",
                check.detail,
                check.remediation.clone().unwrap_or_default()
            );
            assert!(text.contains(expected), "{expected}: {check:?}");
            assert!(!serde_json::to_string(&check).unwrap().contains(TOKEN));
            for command in script.commands.borrow().iter() {
                assert!(!format!("{command:?}").contains(TOKEN));
                if command.args.first().map(String::as_str) == Some("credential") {
                    assert!(command.sensitive_output, "the credential output is a token");
                }
            }
        }

        let (check, _) = run([
            unrewritten(),
            unrewritten(),
            denied(),
            exit(255, "", "could not run"),
        ]);
        let fix = check.remediation.unwrap();
        for needed in [
            "gh auth login",
            "gh auth setup-git",
            "git config --global --add url.https://github.com/.insteadOf git@github.com:",
            "git config --global --add url.https://github.com/.insteadOf ssh://git@github.com/",
            "GitHub SSH key",
        ] {
            assert!(fix.contains(needed), "{needed}: {fix}");
        }
    }

    #[cfg(unix)]
    mod real_git {
        use super::*;

        /// Runs commands against a throwaway home so the host's own Git
        /// configuration and credentials cannot decide the result.
        struct IsolatedGit {
            home: tempfile::TempDir,
        }

        impl IsolatedGit {
            fn new(config: &str) -> Self {
                let home = tempfile::tempdir().unwrap();
                std::fs::write(home.path().join(".gitconfig"), config).unwrap();
                Self { home }
            }
        }

        impl CommandExecutor for IsolatedGit {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let mut command = command.clone();
                command.env.extend([
                    ("HOME".into(), self.home.path().display().to_string()),
                    (
                        "XDG_CONFIG_HOME".into(),
                        self.home.path().join(".config").display().to_string(),
                    ),
                    ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                    // A session's generated Git config is selected this way.
                    (
                        "GIT_CONFIG_GLOBAL".into(),
                        self.home.path().join(".gitconfig").display().to_string(),
                    ),
                ]);
                command
                    .env_remove
                    .extend(["GIT_CONFIG_COUNT".into(), "GIT_CONFIG_PARAMETERS".into()]);
                // A hung probe is a failure of the check's no-prompt promise.
                BoundedProcessExecutor::new(Duration::from_secs(30)).execute(&command)
            }
        }

        const HELPER: &str = "[credential]\n\thelper = \"!f() { echo username=octocat; echo password=SECRET-TOKEN; }; f\"\n";
        const EMPTY_HELPER: &str =
            "[credential]\n\thelper = \"!f() { echo username=octocat; echo password=; }; f\"\n";
        const REWRITES: &str = "[url \"https://github.com/\"]\n\tinsteadOf = git@github.com:\n\tinsteadOf = ssh://git@github.com/\n";
        // Hard-won: 46cceae5a: a rewrite carrying a username was taken for SSH.
        const REWRITES_WITH_USER: &str = "[url \"https://alice@github.com/\"]\n\tinsteadOf = git@github.com:\n\tinsteadOf = ssh://git@github.com/\n";
        const SCP_REWRITE_ONLY: &str =
            "[url \"https://github.com/\"]\n\tinsteadOf = git@github.com:\n";

        /// Pins what the check assumes of Git: a helper's password is seen,
        /// no helper or an empty password is not (and never prompts), and
        /// `ls-remote --get-url` applies each `insteadOf` rule.
        #[test]
        fn git_credential_fill_and_url_rewriting_behave_as_the_check_assumes() {
            let seen = |config: &str| {
                matches!(
                    probe_https_credentials(BareHost::Local, &IsolatedGit::new(config)),
                    HttpsCredentials::Supplied
                )
            };
            assert!(seen(HELPER));
            assert!(!seen(""));
            assert!(!seen(EMPTY_HELPER));

            let rewrites = |config: &str, remote: &str| {
                rewrites_to_https(BareHost::Local, remote, &IsolatedGit::new(config))
            };
            let ssh_remotes = ssh_remotes();
            for remote in &ssh_remotes {
                assert!(rewrites(REWRITES, remote), "{remote}");
                assert!(rewrites(REWRITES_WITH_USER, remote), "{remote}");
                assert!(!rewrites("", remote), "{remote}");
            }
            assert!(rewrites(SCP_REWRITE_ONLY, &ssh_remotes[0]));
            assert!(!rewrites(SCP_REWRITE_ONLY, &ssh_remotes[1]));
        }
    }
}
