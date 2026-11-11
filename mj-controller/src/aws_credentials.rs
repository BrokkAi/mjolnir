//! Resolve the host AWS chain once, then provision only its selected identity.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use mj_core::aws_credentials::{AwsCredentialSource, AwsCredentials};
use mj_core::config::{AuthScheme, HarnessProfile};
use mj_core::targets::{CommandExecutor, CommandSpec, TargetLocator};

fn has_credential_source(environment: &BTreeMap<String, String>) -> bool {
    [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ]
    .iter()
    .any(|key| environment.get(*key).is_some_and(|value| !value.is_empty()))
        || environment
            .get("AWS_EC2_METADATA_DISABLED")
            .is_some_and(|value| value == "false")
}

/// Where a session runs, as far as AWS credentials care. Built from a
/// session's locator or from a configured target template; both matches are
/// exhaustive, so a new target kind must decide here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionHost {
    /// The controller's own machine and user: the session reads the host's
    /// AWS files itself.
    RawLocal,
    /// An EC2 instance, which uses its instance role.
    Ec2,
    /// A container or a remote machine, which has no AWS credentials unless
    /// the controller delivers them.
    Isolated,
}

impl From<&TargetLocator> for SessionHost {
    fn from(locator: &TargetLocator) -> Self {
        match locator {
            TargetLocator::LocalBare { .. } => Self::RawLocal,
            TargetLocator::AwsEc2 { .. } => Self::Ec2,
            TargetLocator::LocalPodman { .. }
            | TargetLocator::LocalDocker { .. }
            | TargetLocator::AppleContainer { .. }
            | TargetLocator::SshBare { .. }
            | TargetLocator::SshPodman { .. }
            | TargetLocator::SshDocker { .. }
            | TargetLocator::SshAppleContainer { .. }
            | TargetLocator::SshNitro { .. } => Self::Isolated,
        }
    }
}

impl From<&mj_core::config::TargetTemplate> for SessionHost {
    fn from(template: &mj_core::config::TargetTemplate) -> Self {
        use mj_core::config::TargetTemplate as T;
        match template {
            T::LocalBare => Self::RawLocal,
            T::AwsEc2 { .. } => Self::Ec2,
            T::LocalPodman { .. }
            | T::LocalDocker { .. }
            | T::AppleContainer { .. }
            | T::SshBare { .. }
            | T::SshPodman { .. }
            | T::SshDocker { .. }
            | T::SshAppleContainer { .. }
            | T::SshNitro { .. } => Self::Isolated,
        }
    }
}

/// Why a session on this profile and host does not receive AWS credentials
/// from the controller. One definition of the question, used by launch, the
/// sync and `mj doctor`: `None` means it does. Explicit credentials in the
/// target's or the profile's environment stay with their owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialsWithheld {
    /// The profile does not authenticate with the AWS credential chain.
    NotBedrock,
    RawLocal,
    Ec2,
    /// The target's environment names an AWS profile or credential source.
    TargetEnvironment,
    /// The profile's environment names an AWS credential source.
    ProfileEnvironment,
}

impl CredentialsWithheld {
    /// What to say about a `[profiles.<id>.aws]` table that therefore cannot
    /// apply, in terms of what the session uses instead.
    pub(crate) fn why_a_role_cannot_apply(self) -> &'static str {
        match self {
            Self::NotBedrock => {
                "the profile does not authenticate with the AWS credential chain (Amazon Bedrock)"
            }
            Self::RawLocal => {
                "raw local sessions read the host's own AWS files directly, so a role cannot limit them"
            }
            Self::Ec2 => "EC2 sessions use the instance's own role, which a role here cannot limit",
            Self::TargetEnvironment => {
                "the target's environment sets its own AWS profile or credentials, which take precedence"
            }
            Self::ProfileEnvironment => {
                "the profile's environment sets its own AWS credentials, which take precedence"
            }
        }
    }
}

pub(crate) fn credentials_withheld(
    profile: &HarnessProfile,
    host: SessionHost,
    target_environment: &BTreeMap<String, String>,
) -> Option<CredentialsWithheld> {
    if !is_bedrock(profile) {
        return Some(CredentialsWithheld::NotBedrock);
    }
    match host {
        SessionHost::RawLocal => return Some(CredentialsWithheld::RawLocal),
        SessionHost::Ec2 => return Some(CredentialsWithheld::Ec2),
        SessionHost::Isolated => {}
    }
    if has_credential_source(target_environment)
        || target_environment.contains_key("AWS_PROFILE")
        || target_environment.contains_key("AWS_DEFAULT_PROFILE")
    {
        return Some(CredentialsWithheld::TargetEnvironment);
    }
    has_credential_source(profile.environment.resolved())
        .then_some(CredentialsWithheld::ProfileEnvironment)
}

/// Whether sessions on this profile authenticate with the AWS credential
/// chain: [`credentials_withheld`] (so launch, the unused-role notice and the
/// sync's decision to mint) and `mj doctor` ask it. Other code that reads
/// `auth_scheme()` for other reasons does not. A profile whose configuration
/// cannot be read is unknown, and with a role it is treated as Bedrock (see
/// [`unreadable_provider`]). It decides whether credentials are minted, never
/// whether a configured role withdraws the ones a session holds.
pub(crate) fn is_bedrock(profile: &HarnessProfile) -> bool {
    profile.auth_scheme() == AuthScheme::AwsCredentialChain
        || unreadable_provider(profile).is_some()
}

/// Why the profile's Codex configuration cannot be read, when a role is
/// configured and it cannot. Unreadable is unknown, not "not Bedrock": the
/// worker still runs the configuration it was staged with, so a profile with a
/// role is treated as Bedrock until its configuration can be read, and an
/// error is reported rather than the session's AWS credentials left unmanaged.
fn unreadable_provider(profile: &HarnessProfile) -> Option<String> {
    if profile.aws.is_none() || profile.kind != mj_core::config::HarnessKind::Codex {
        return None;
    }
    profile
        .codex_provider()
        .err()
        .map(|error| format!("{error:#}"))
}

pub(crate) fn receives_synced_credentials(
    profile: &HarnessProfile,
    host: SessionHost,
    target_environment: &BTreeMap<String, String>,
) -> bool {
    credentials_withheld(profile, host, target_environment).is_none()
}

/// What to tell the person when a profile has a `[profiles.<id>.aws]` table
/// that cannot apply to a session on this target, and why. The session runs
/// without it rather than being refused.
pub(crate) fn unused_role_notice(
    profile_id: &str,
    profile: &HarnessProfile,
    host: SessionHost,
    target_environment: &BTreeMap<String, String>,
) -> Option<String> {
    let withheld = credentials_withheld(profile, host, target_environment)?;
    profile.aws.as_ref()?;
    Some(format!(
        "[profiles.{profile_id}.aws] has no effect on this session: {}. Mjolnir mints no role-scoped credentials for it.",
        withheld.why_a_role_cannot_apply()
    ))
}

/// Explicit target credentials and EC2 instance roles stay target-owned.
pub(crate) fn source(
    profile: &HarnessProfile,
    backend: &TargetLocator,
    target_environment: &BTreeMap<String, String>,
) -> Result<Option<AwsCredentialSource>> {
    if !receives_synced_credentials(profile, SessionHost::from(backend), target_environment) {
        return Ok(None);
    }
    selected_source(profile).map(Some)
}

/// [`source`] for the credential sync.
///
/// A profile without a role is the unscoped path, unchanged from before role
/// scoping: the same source launch builds, resolved when the targets are built.
///
/// A profile with a role is the role-scoped path. Every session of the profile
/// that Mjolnir may have staged credentials into is managed, whether or not it
/// still receives any: a role configured for a profile means no session of it
/// keeps host credentials, so the sync withdraws what does not match the role's
/// scope, and `mints` says whether it also mints. The source carries a pointer
/// to the harness configuration that may select the host's AWS profile; the
/// mint reads it when it runs ([`resolve`]), because an edit to that file does
/// not rebuild the targets, and a file that cannot be read fails that cycle
/// closed. Raw local and EC2 sessions have no credentials of Mjolnir's to
/// withdraw.
pub(crate) fn sync_source(
    profile: &HarnessProfile,
    backend: &TargetLocator,
    target_environment: &BTreeMap<String, String>,
) -> Result<Option<AwsCredentialSource>> {
    if profile.aws.is_none() {
        return source(profile, backend, target_environment);
    }
    let host = SessionHost::from(backend);
    if host != SessionHost::Isolated {
        return Ok(None);
    }
    let mut source = base_source(profile);
    source.mints = credentials_withheld(profile, host, target_environment).is_none();
    Ok(Some(source))
}

/// The source with the AWS profile the harness's configuration selects, read
/// now. The role-scoped mint calls this each time. An error means the
/// configuration cannot be read: nothing can be minted for that attempt.
pub(crate) fn resolve(source: &AwsCredentialSource) -> Result<AwsCredentialSource> {
    let mut source = source.clone();
    // Codex can select its AWS profile inside the provider table.
    if let Some(selection) = &source.selection
        && let Some(config) = mj_core::codex_provider::ConfigFiles::read_with(
            &selection.home,
            selection.overlay.as_deref(),
            crate::host_fs::read_optional_sync,
        )?
        .layer(&selection.home)?
    {
        let config: toml::Value =
            toml::from_str(&config).context("read Codex AWS profile selection")?;
        if let Some(provider) = config.get("model_provider").and_then(toml::Value::as_str)
            && let Some(aws) = config
                .get("model_providers")
                .and_then(|providers| providers.get(provider))
                .and_then(|provider| provider.get("aws"))
            && let Some(selected) = aws.get("profile").and_then(toml::Value::as_str)
        {
            source
                .environment
                .insert("AWS_PROFILE".into(), selected.into());
        }
    }
    Ok(source)
}

/// The environment the AWS CLI runs in to resolve this profile's identity on
/// the controller host: the profile's own environment with the AWS profile
/// it selects, read now. Callers have already decided that the profile
/// receives synced credentials.
pub(crate) fn selected_source(profile: &HarnessProfile) -> Result<AwsCredentialSource> {
    let mut source = resolve(&base_source(profile))?;
    // Resolved, so nothing is left to read later.
    source.selection = None;
    Ok(source)
}

/// The profile's own environment with the AWS profile it names, before the
/// harness's configuration is consulted.
fn base_source(profile: &HarnessProfile) -> AwsCredentialSource {
    let mut environment = profile.environment.resolved().clone();
    let selected = environment
        .get("AWS_PROFILE")
        .or_else(|| environment.get("AWS_DEFAULT_PROFILE"))
        .cloned()
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_PROFILE").ok())
        .unwrap_or_else(|| "default".into());
    environment.insert("AWS_PROFILE".into(), selected);
    AwsCredentialSource {
        environment,
        role: profile.aws.clone(),
        mints: true,
        selection: (profile.kind == mj_core::config::HarnessKind::Codex).then(|| {
            mj_core::aws_credentials::AwsProfileSelection {
                home: profile.home.clone(),
                overlay: profile.codex_profile.clone(),
            }
        }),
    }
}

pub(crate) fn export(
    source: &AwsCredentialSource,
    executor: &impl CommandExecutor,
) -> Result<AwsCredentials> {
    let mut command = CommandSpec::new(
        "aws",
        ["configure", "export-credentials", "--format", "process"],
    )
    .purpose("resolve selected AWS credentials for isolated worker");
    command.sensitive_output = true;
    let selected = source
        .environment
        .get("AWS_PROFILE")
        .map(String::as_str)
        .unwrap_or("default");
    command.args.extend(["--profile".into(), selected.into()]);
    command.env = source.environment.clone();
    command
        .env
        .insert("AWS_CLI_AUTO_PROMPT".into(), "off".into());
    let advice = "Could not resolve AWS credentials for the isolated worker; install AWS CLI v2 and configure or renew the selected AWS profile on the controller host";
    let output = executor.execute(&command).with_context(|| advice)?;
    // A custom credential_process can put secrets in either stream. Never
    // include CLI output in diagnostics, including on failure.
    ensure!(
        output.status == 0,
        "{advice} (AWS CLI exit {})",
        output.status
    );
    AwsCredentials::from_export(selected, &output.stdout).with_context(|| advice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::targets::CommandOutput;
    #[cfg(unix)]
    use mj_core::targets::ProcessExecutor;

    #[cfg(unix)]
    struct IsolatedAws;

    #[cfg(unix)]
    impl CommandExecutor for IsolatedAws {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut command = command.clone();
            command.clear_env = true;
            command
                .env
                .insert("PATH".into(), std::env::var("PATH").unwrap());
            ProcessExecutor.execute(&command)
        }
    }

    // An unreadable provider configuration is unknown, not "not Bedrock": a
    // profile with a role stays managed by the sync, whose mint reads the file
    // when it runs and fails closed on an error, while launch (which resolves
    // it at once) fails. Nothing is captured in the sync's source.
    #[test]
    fn an_unreadable_codex_configuration_fails_launch_and_the_mint_but_leaves_the_sync_managed() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let role = mj_core::config::AwsRoleConfig {
            role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
            session_policy: None,
            session_policy_arns: Vec::new(),
            duration_seconds: None,
            source_identity: false,
        };
        let mut profile = HarnessProfile::new(mj_core::config::HarnessKind::Codex, home.path());
        profile.aws = Some(role);
        let container = TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "c".repeat(64),
            workspace_storage: Default::default(),
        };
        let none = BTreeMap::new();

        std::fs::write(&config, "model_provider = [unterminated").unwrap();
        assert!(super::source(&profile, &container, &none).is_err());
        let synced = sync_source(&profile, &container, &none)
            .unwrap()
            .expect("still managed");
        assert!(synced.role.is_some() && synced.mints);
        let why = format!("{:#}", resolve(&synced).unwrap_err());
        assert!(why.contains("TOML parse error"), "{why}");

        // Repaired, and it selects an AWS profile: the same source, with
        // nothing rebuilt, resolves to it at the next mint.
        std::fs::write(
            &config,
            "model_provider = \"amazon-bedrock-runtime\"\n[model_providers.amazon-bedrock-runtime.aws]\nprofile = \"team\"\n",
        )
        .unwrap();
        assert_eq!(
            sync_source(&profile, &container, &none).unwrap().as_ref(),
            Some(&synced)
        );
        assert_eq!(resolve(&synced).unwrap().environment["AWS_PROFILE"], "team");
        assert_eq!(
            selected_source(&profile).unwrap().environment["AWS_PROFILE"],
            "team"
        );

        // A raw local session has no credentials of Mjolnir's to manage.
        let raw = TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/workers/s".into(),
        };
        assert_eq!(sync_source(&profile, &raw, &none).unwrap(), None);
        // Without a role it is the unscoped path, as before role scoping.
        profile.aws = None;
        std::fs::write(&config, "model_provider = [unterminated").unwrap();
        assert_eq!(sync_source(&profile, &container, &none).unwrap(), None);
    }

    // A role configured for a profile that no longer authenticates with the AWS
    // credential chain still manages its sessions, so the sync withdraws the
    // credentials they hold; it only does not mint.
    #[test]
    fn a_role_on_a_profile_that_is_not_bedrock_manages_its_sessions_without_minting() {
        let home = tempfile::tempdir().unwrap();
        let mut profile = HarnessProfile::new(mj_core::config::HarnessKind::Claude, home.path());
        profile.aws = Some(mj_core::config::AwsRoleConfig {
            role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
            session_policy: None,
            session_policy_arns: Vec::new(),
            duration_seconds: None,
            source_identity: false,
        });
        let container = TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "c".repeat(64),
            workspace_storage: Default::default(),
        };
        let none = BTreeMap::new();

        let source = sync_source(&profile, &container, &none)
            .unwrap()
            .expect("a configured role manages the profile's sessions");
        assert!(
            !source.mints,
            "nothing is minted for a profile not on Bedrock"
        );
        // Bedrock again: minted.
        profile
            .environment
            .insert("CLAUDE_CODE_USE_BEDROCK".into(), "1".into());
        assert!(
            sync_source(&profile, &container, &none)
                .unwrap()
                .unwrap()
                .mints
        );
    }

    // Hard-won: #1385: copying profile selection left isolated Bedrock workers without credentials.
    // Hard-won: #1390: background installation omitted the native process-provider configuration.
    #[cfg(unix)]
    #[test]
    #[ignore = "requires AWS CLI v2; uses isolated fixture credentials without network access"]
    fn aws_cli_resolves_staged_credentials_and_their_renewal() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile ' λ");
        std::fs::create_dir(&home).unwrap();
        let empty = root.path().join("empty");
        std::fs::write(&empty, "").unwrap();
        let source_file = root.path().join("selected.json");
        let config = root.path().join("config");
        std::fs::write(
            &config,
            format!(
                "[profile selected]\ncredential_process = cat {}\n",
                mj_core::targets::posix_quote(&source_file.to_string_lossy())
            ),
        )
        .unwrap();
        let mut source = AwsCredentialSource {
            environment: BTreeMap::from([
                ("AWS_PROFILE".into(), "selected".into()),
                (
                    "AWS_CONFIG_FILE".into(),
                    config.to_string_lossy().into_owned(),
                ),
                (
                    "AWS_SHARED_CREDENTIALS_FILE".into(),
                    empty.to_string_lossy().into_owned(),
                ),
                ("AWS_EC2_METADATA_DISABLED".into(), "true".into()),
            ]),
            role: None,
            mints: true,
            selection: None,
        };
        for generation in ["original", "renewed"] {
            std::fs::write(
                &source_file,
                serde_json::to_vec(&serde_json::json!({
                    "Version": 1, "AccessKeyId": format!("key-{generation}"),
                    "SecretAccessKey": format!("secret-{generation}"),
                    "SessionToken": format!("token-{generation}"),
                    "Expiration": "2099-01-01T00:00:00Z",
                }))
                .unwrap(),
            )
            .unwrap();
            let exported = export(&source, &IsolatedAws).unwrap();
            exported.install(&home, &home).unwrap();
            let mut worker_source = source.clone();
            worker_source.environment.insert(
                "AWS_CONFIG_FILE".into(),
                home.join("aws-config").to_string_lossy().into_owned(),
            );
            let worker = export(&worker_source, &IsolatedAws).unwrap();
            assert_eq!(worker.encode().unwrap(), exported.encode().unwrap());
            // SSH starts relative profile homes beneath HOME, while the
            // credential process may run from an unrelated working directory.
            std::fs::write(
                home.join("aws-config"),
                exported
                    .configuration(std::path::Path::new("profile ' λ"))
                    .unwrap(),
            )
            .unwrap();
            worker_source
                .environment
                .insert("HOME".into(), root.path().to_string_lossy().into_owned());
            let worker = export(&worker_source, &IsolatedAws).unwrap();
            assert_eq!(worker.encode().unwrap(), exported.encode().unwrap());
        }
        // Missing credentials fail with an authentication explanation before
        // starting a harness, without falling through to instance metadata.
        source
            .environment
            .insert("AWS_PROFILE".into(), "missing".into());
        let error = export(&source, &IsolatedAws).err().unwrap();
        assert!(format!("{error:#}").contains("Could not resolve AWS credentials"));
        assert!(!format!("{error:#}").contains("secret-renewed"));
    }

    // Hard-won: #1385: AWS helpers can return secrets in either output stream on failure.
    #[test]
    fn failed_aws_export_diagnostics_never_include_helper_output() {
        struct FailedExport;
        impl CommandExecutor for FailedExport {
            fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 1,
                    stdout: b"private-key".to_vec(),
                    stderr: b"private-secret".to_vec(),
                })
            }
        }
        let source = AwsCredentialSource {
            environment: BTreeMap::new(),
            role: None,
            mints: true,
            selection: None,
        };
        let error = export(&source, &FailedExport).err().unwrap();
        assert!(!format!("{error:#}").contains("private-"));
        #[cfg(unix)]
        {
            // The shared executor normally logs stderr on a disk-full
            // failure. AWS helper output must bypass that observer too.
            struct FailedProcess;
            impl CommandExecutor for FailedProcess {
                fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                    let mut command = command.clone();
                    command.program = "sh".into();
                    command.args = vec![
                        "-c".into(),
                        "echo 'No space left on device: private-secret' >&2; exit 1".into(),
                    ];
                    ProcessExecutor.execute(&command)
                }
            }
            let log = crate::test_log::CapturedLog::default();
            let _guard = tracing::subscriber::set_default(log.clone());
            let error = export(&source, &FailedProcess).err().unwrap();
            assert!(!format!("{error:#}").contains("private-secret"));
            assert!(
                log.events()
                    .iter()
                    .all(|(_, event)| !event.contains("private-secret"))
            );
        }
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    fn role() -> mj_core::config::AwsRoleConfig {
        mj_core::config::AwsRoleConfig {
            role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
            session_policy: None,
            session_policy_arns: Vec::new(),
            duration_seconds: None,
            source_identity: false,
        }
    }

    fn bedrock(home: &std::path::Path) -> HarnessProfile {
        let mut profile = HarnessProfile::new(mj_core::config::HarnessKind::Claude, home);
        profile
            .environment
            .insert("CLAUDE_CODE_USE_BEDROCK".to_owned(), "1".to_owned());
        profile.aws = Some(role());
        profile
    }

    #[test]
    fn a_role_that_cannot_apply_is_a_notice_that_names_why_not_a_refusal() {
        let home = tempfile::tempdir().unwrap();
        let none = BTreeMap::new();
        let mut plain = HarnessProfile::new(mj_core::config::HarnessKind::Claude, home.path());
        assert_eq!(
            unused_role_notice("work", &plain, SessionHost::Isolated, &none),
            None
        );
        plain.aws = Some(role());
        let notice = unused_role_notice("work", &plain, SessionHost::Isolated, &none).unwrap();
        assert!(
            notice.contains("[profiles.work.aws] has no effect"),
            "{notice}"
        );
        assert!(notice.contains("Amazon Bedrock"), "{notice}");

        // A Bedrock profile: the reason depends on where the session runs.
        let profile = bedrock(home.path());
        assert_eq!(
            unused_role_notice("work", &profile, SessionHost::Isolated, &none),
            None
        );
        let reason = |host, environment: &BTreeMap<String, String>| {
            unused_role_notice("work", &profile, host, environment).unwrap()
        };
        assert!(
            reason(SessionHost::RawLocal, &none)
                .contains("raw local sessions read the host's own AWS files")
        );
        assert!(reason(SessionHost::Ec2, &none).contains("instance's own role"));
        let own_profile = BTreeMap::from([("AWS_PROFILE".to_owned(), "team".to_owned())]);
        assert!(
            reason(SessionHost::Isolated, &own_profile)
                .contains("target's environment sets its own AWS")
        );
        let own_keys = BTreeMap::from([("AWS_ACCESS_KEY_ID".to_owned(), "AKIA".to_owned())]);
        assert!(
            reason(SessionHost::Isolated, &own_keys)
                .contains("target's environment sets its own AWS")
        );
        // Keys in the profile's own environment take precedence too.
        let mut with_keys = bedrock(home.path());
        with_keys
            .environment
            .insert("AWS_ACCESS_KEY_ID".to_owned(), "AKIA".to_owned());
        assert!(
            unused_role_notice("work", &with_keys, SessionHost::Isolated, &none)
                .unwrap()
                .contains("profile's environment sets its own AWS credentials")
        );
    }
}
