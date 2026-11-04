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

/// Explicit target credentials and EC2 instance roles stay target-owned.
pub(crate) fn source(
    profile: &HarnessProfile,
    backend: &TargetLocator,
    target_environment: &BTreeMap<String, String>,
) -> Result<Option<AwsCredentialSource>> {
    if profile.auth_scheme() != AuthScheme::AwsCredentialChain
        || matches!(
            backend,
            TargetLocator::LocalBare { .. } | TargetLocator::AwsEc2 { .. }
        )
        || has_credential_source(target_environment)
        || target_environment.contains_key("AWS_PROFILE")
        || target_environment.contains_key("AWS_DEFAULT_PROFILE")
    {
        return Ok(None);
    }
    let environment = profile.environment.resolved();
    if has_credential_source(environment) {
        return Ok(None);
    }
    let mut environment = environment.clone();
    let selected = environment
        .get("AWS_PROFILE")
        .or_else(|| environment.get("AWS_DEFAULT_PROFILE"))
        .cloned()
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_PROFILE").ok())
        .unwrap_or_else(|| "default".into());
    environment.insert("AWS_PROFILE".into(), selected);
    // Codex can select its AWS profile inside the provider table.
    if profile.kind == mj_core::config::HarnessKind::Codex
        && let Some(config) = mj_core::codex_provider::effective_config(
            &profile.home,
            profile.codex_profile.as_deref(),
        )?
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
            environment.insert("AWS_PROFILE".into(), selected.into());
        }
    }
    Ok(Some(AwsCredentialSource { environment }))
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
