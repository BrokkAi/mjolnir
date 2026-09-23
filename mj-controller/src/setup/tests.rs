use std::cell::RefCell;
use std::fs;

use super::*;
use crate::targets::CommandOutput;

#[test]
fn an_api_key_codex_profile_is_authenticated_by_its_configuration_file() {
    let home = tempfile::tempdir().unwrap();
    let executor = FakeExecutor::succeeds();
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: [("ZAI_API_KEY".to_owned(), "key".to_owned())]
            .into_iter()
            .collect(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    assert!(
        !harness_is_authenticated_with_executor(&profile, &executor),
        "an empty home is not set up"
    );
    fs::write(
        home.path().join("config.toml"),
        "model = \"glm-5.3\"\n\
         model_provider = \"zai\"\n\
         [model_providers.zai]\n\
         base_url = \"https://api.z.ai/api/v1\"\n\
         env_key = \"ZAI_API_KEY\"\n\
         wire_api = \"responses\"\n",
    )
    .unwrap();
    assert!(
        harness_is_authenticated_with_executor(&profile, &executor),
        "the key lives in the profile environment, so the configuration is the proof"
    );
    assert!(
        !home.path().join("auth.json").exists(),
        "no ChatGPT login is involved"
    );
}

#[cfg(unix)]
#[test]
fn first_terminal_launch_writes_a_local_codex_config_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    initialize_local_startup_config(&path).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.profiles["codex"].kind, HarnessKind::Codex);
    assert!(config.profiles["codex"].home.is_absolute());
    assert!(matches!(
        config.targets["localhost"],
        TargetTemplate::LocalBare
    ));
    assert!(config.bundles.is_empty());
    let written = fs::read(&path).unwrap();
    initialize_local_startup_config(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), written);
}

#[cfg(unix)]
#[test]
fn local_startup_preserves_existing_settings_and_ignores_disabled_startup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = Config::default();
    config.phone.enabled = false;
    config.save_to(&path).unwrap();
    initialize_local_startup_config(&path).unwrap();
    assert!(!Config::load_from(&path).unwrap().phone.enabled);

    // Even a partially configured installation belongs to the user.
    let configured = "version = 2\n# keep this comment\n[targets.custom]\nkind = 'local-bare'\n";
    fs::write(&path, configured).unwrap();
    initialize_local_startup_config(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), configured);

    let disabled = "version = 2\n[startup]\nenabled = false\n";
    fs::write(&path, disabled).unwrap();
    initialize_local_startup_config(&path).unwrap();
    let bootstrapped = Config::load_from(&path).unwrap();
    assert!(
        serde_json::to_value(&bootstrapped)
            .unwrap()
            .get("startup")
            .is_none()
    );
    assert_eq!(bootstrapped.profiles["codex"].kind, HarnessKind::Codex);
    assert!(matches!(
        bootstrapped.targets["localhost"],
        TargetTemplate::LocalBare
    ));

    let newer = "version = 999\nfuture_field = true\n";
    fs::write(&path, newer).unwrap();
    assert!(initialize_local_startup_config(&path).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), newer);
}

struct FakeExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    statuses: Vec<i32>,
}

impl FakeExecutor {
    fn succeeds() -> Self {
        Self {
            commands: RefCell::new(vec![]),
            statuses: vec![0, 0, 0],
        }
    }
}

impl CommandExecutor for FakeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let index = self.commands.borrow().len();
        self.commands.borrow_mut().push(command.clone());
        Ok(CommandOutput {
            status: self.statuses.get(index).copied().unwrap_or(0),
            stdout: b"available".to_vec(),
            stderr: b"failed".to_vec(),
        })
    }
}

struct RuntimeProbeExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    outputs: RefCell<Vec<CommandOutput>>,
}

impl RuntimeProbeExecutor {
    fn new(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            commands: RefCell::new(vec![]),
            outputs: RefCell::new(outputs.into_iter().collect()),
        }
    }
}

impl CommandExecutor for RuntimeProbeExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        if self.outputs.borrow().is_empty() {
            anyhow::bail!("no canned output for {}", command.program);
        }
        Ok(self.outputs.borrow_mut().remove(0))
    }
}

fn ok(stdout: &[u8]) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.to_vec(),
        stderr: vec![],
    }
}

fn failed(stderr: &[u8]) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: stderr.to_vec(),
    }
}

const CALLER_IDENTITY: &[u8] =
    br#"{"UserId":"AIDA","Account":"123456789012","Arn":"arn:aws:iam::123456789012:user/dev"}"#;

fn discovery_without_runtimes() -> SetupDiscovery {
    SetupDiscovery {
        homes: vec![],
        repository: None,
        runtimes: vec![],
        aws: None,
        ssh_hosts: vec![],
    }
}

#[test]
fn newly_installed_harness_is_discovered_before_its_first_login() {
    struct InstalledMuse;
    impl CommandExecutor for InstalledMuse {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            assert_eq!(command.args, ["--version"]);
            Ok(CommandOutput {
                status: if command.program == "muse" { 0 } else { 127 },
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("custom-muse-home");
    let overrides = BTreeMap::from([(HarnessKind::Muse, path.clone())]);
    let mut homes = Vec::new();
    for _ in 0..2 {
        discover_installed_harnesses(
            Some(directory.path()),
            &overrides,
            &mut homes,
            &InstalledMuse,
        );
        assert_eq!(
            homes,
            vec![DiscoveredHome {
                kind: HarnessKind::Muse,
                path: path.clone(),
                authenticated: false
            }]
        );
        assert!(!path.exists(), "discovery must not create a profile");
    }
}

#[test]
fn discovers_default_and_overridden_homes_with_authentication_markers() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let codex = home.join(".codex");
    let kimi = home.join(".kimi-code");
    let grok = home.join(".grok");
    let claude = directory.path().join("claude-override");
    fs::create_dir_all(&codex).unwrap();
    fs::create_dir_all(kimi.join("credentials")).unwrap();
    fs::create_dir_all(&grok).unwrap();
    fs::create_dir_all(&claude).unwrap();
    fs::write(codex.join("auth.json"), "{}").unwrap();
    fs::write(kimi.join("credentials/kimi-code.json"), "{}").unwrap();
    fs::write(grok.join("auth.json"), "{}").unwrap();
    fs::write(claude.join(".credentials.json"), "{}").unwrap();

    let executor = FakeExecutor::succeeds();
    let homes = discover_harness_homes_with_executor(
        Some(&home),
        [(HarnessKind::Claude, claude.clone())],
        &executor,
    );

    assert_eq!(homes.len(), 4);
    assert!(homes.iter().all(|home| home.authenticated));
    assert!(homes.iter().any(|home| home.path == codex));
    assert!(homes.iter().any(|home| home.path == claude));
    assert!(homes.iter().any(|home| home.path == kimi));
    assert!(
        homes
            .iter()
            .any(|home| home.path == grok && home.kind == HarnessKind::Grok)
    );
}

#[test]
fn every_harness_has_a_discoverable_default_home() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().to_path_buf();
    for kind in HarnessKind::ALL {
        fs::create_dir_all(home.join(kind.default_home_leaf())).unwrap();
    }

    let executor = FakeExecutor::succeeds();
    let homes = discover_harness_homes_with_executor(Some(&home), [], &executor);

    assert_eq!(homes.len(), HarnessKind::ALL.len());
    for kind in HarnessKind::ALL {
        assert!(
            homes
                .iter()
                .any(|home| home.kind == kind && !home.authenticated),
            "{kind:?} default home"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn claude_keychain_marks_the_default_home_authenticated_without_a_marker() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let claude = home.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let executor = RuntimeProbeExecutor::new([ok(
        br#"{"claudeAiOauth":{"accessToken":"access","refreshToken":"refresh"}}"#,
    )]);

    let homes = discover_harness_homes_with_executor(Some(&home), [], &executor);

    assert_eq!(
        homes,
        vec![DiscoveredHome {
            kind: HarnessKind::Claude,
            path: claude.clone(),
            authenticated: true,
        }]
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "security");
    assert_eq!(
        commands[0].args,
        [
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w"
        ]
    );
    assert!(commands[0].env.is_empty());
}

#[test]
fn claude_status_checks_a_custom_home_without_a_marker() {
    let directory = tempfile::tempdir().unwrap();
    let claude = directory.path().join("claude-custom");
    fs::create_dir_all(&claude).unwrap();
    // Where CLAUDE_CONFIG_DIR scopes a home, the CLI answers for that home.
    // On macOS every home shares one Keychain item, so the Keychain is the
    // only thing worth asking and the CLI would only report the default
    // profile back.
    let on_macos = cfg!(target_os = "macos");
    let executor = RuntimeProbeExecutor::new([if on_macos {
        ok(br#"{"claudeAiOauth":{"accessToken":"access","refreshToken":"refresh"}}"#)
    } else {
        ok(br#"{"loggedIn":true,"authMethod":"claude.ai"}"#)
    }]);

    let homes = discover_harness_homes_with_executor(
        None,
        [(HarnessKind::Claude, claude.clone())],
        &executor,
    );

    assert_eq!(
        homes,
        vec![DiscoveredHome {
            kind: HarnessKind::Claude,
            path: claude.clone(),
            authenticated: true,
        }]
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    if on_macos {
        assert_eq!(commands[0].program, "security");
        assert!(commands[0].env.is_empty(), "{:?}", commands[0].env);
    } else {
        assert_eq!(commands[0].program, "claude");
        assert_eq!(commands[0].args, ["auth", "status", "--json"]);
        assert_eq!(
            commands[0].env.get("CLAUDE_CONFIG_DIR"),
            Some(&claude.to_string_lossy().into_owned())
        );
    }
}

#[test]
fn claude_credential_evidence_requires_a_nonempty_login_secret() {
    assert!(claude_credentials_contain_login(
        br#"{"claudeAiOauth":{"refreshToken":"refresh"}}"#
    ));
    assert!(!claude_credentials_contain_login(
        br#"{"claudeAiOauth":{"refreshToken":"  "}}"#
    ));
    assert!(!claude_credentials_contain_login(b"not json"));
}

#[test]
fn github_origin_parser_accepts_standard_https_and_ssh_forms() {
    for origin in [
        "https://github.com/BrokkAi/hel.git",
        "git@github.com:BrokkAi/hel.git",
        "ssh://git@github.com/BrokkAi/hel.git",
    ] {
        assert_eq!(
            github_repository_from_origin(origin),
            Some(GithubRepository {
                owner: "BrokkAi".into(),
                repository: "hel".into(),
            })
        );
    }
    assert_eq!(
        github_repository_from_origin("https://example.com/hel"),
        None
    );
}

#[test]
fn config_contains_discovered_profiles_current_repository_and_selected_target() {
    let homes = vec![
        DiscoveredHome {
            kind: HarnessKind::Codex,
            path: PathBuf::from("/profiles/codex"),
            authenticated: true,
        },
        DiscoveredHome {
            kind: HarnessKind::Codex,
            path: PathBuf::from("/profiles/codex-two"),
            authenticated: false,
        },
    ];
    let repository = GithubRepository {
        owner: "BrokkAi".into(),
        repository: "hel".into(),
    };

    let config = build_config(
        &homes,
        Some(&repository),
        RuntimeKind::Podman,
        "ubuntu:24.04",
    );

    config.validate().unwrap();
    assert!(config.profiles.contains_key("codex"));
    assert!(config.profiles.contains_key("codex-2"));
    assert_eq!(
        config.bundles["current-repository"].repositories[0]
            .github
            .as_deref(),
        Some("BrokkAi/hel")
    );
    assert!(matches!(
        config.targets["podman"],
        TargetTemplate::LocalPodman { .. }
    ));
    assert!(matches!(
        config.targets["localhost"],
        TargetTemplate::LocalBare
    ));

    let docker = build_config(
        &homes,
        Some(&repository),
        RuntimeKind::Docker,
        "ubuntu:24.04",
    );
    assert!(matches!(
        docker.targets["docker"],
        TargetTemplate::LocalDocker { .. }
    ));
}

#[test]
fn runtime_probe_requires_podman_rootless_preflight_and_checks_apple_on_macos() {
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 5.4.2\n"),
        ok(b"true\n"),
        ok(b"0 1000 1\n1 100000 65536\n"),
        ok(b"29.0.1 linux\n"),
        ok(b"container version 1\n"),
        ok(b"running\n"),
    ]);
    let runtimes = probe_local_runtimes(&executor, true);

    assert_eq!(runtimes.len(), 3);
    assert_eq!(executor.commands.borrow()[0].program, "podman");
    assert_eq!(executor.commands.borrow()[0].args, ["--version"]);
    assert_eq!(
        executor.commands.borrow()[1].args,
        ["info", "--format", "{{.Host.Security.Rootless}}"]
    );
    assert_eq!(
        executor.commands.borrow()[2].args,
        ["unshare", "cat", "/proc/self/uid_map"]
    );
    assert_eq!(executor.commands.borrow()[3].program, "docker");
    assert_eq!(executor.commands.borrow()[4].program, "container");
    assert!(runtimes.iter().all(|runtime| runtime.usable));
}

#[test]
fn unusable_podman_carries_the_doctor_remediation_into_the_runtime_list() {
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 3.4.7\n"),
        failed(b"docker is unavailable"),
    ]);

    let runtimes = probe_local_runtimes(&executor, false);

    assert_eq!(runtimes.len(), 2);
    assert!(!runtimes[0].usable);
    let remediation = runtimes[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("Install or upgrade Podman"),
        "{remediation}"
    );

    let mut output = Vec::new();
    write_runtimes(&mut output, &runtimes).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Podman: unavailable"), "{output}");
    assert!(output.contains("Docker: unavailable"), "{output}");
    assert!(
        output.contains("remediation: Install or upgrade Podman"),
        "{output}"
    );
}

#[test]
fn aws_is_detected_only_when_the_caller_identity_call_succeeds() {
    let missing = RuntimeProbeExecutor::new([]);
    assert_eq!(detect_aws(&missing), None);

    let denied = RuntimeProbeExecutor::new([failed(b"ExpiredToken")]);
    assert_eq!(detect_aws(&denied), None);

    let working = RuntimeProbeExecutor::new([ok(CALLER_IDENTITY), ok(b"us-east-1\n")]);
    assert_eq!(
        detect_aws(&working),
        Some(AwsAccount {
            account: "123456789012".into(),
            arn: "arn:aws:iam::123456789012:user/dev".into(),
            region: Some("us-east-1".into()),
        })
    );
    assert_eq!(working.commands.borrow()[0].args[0], "sts");
    assert_eq!(
        working.commands.borrow()[1].args,
        ["configure", "get", "region"]
    );
}

#[test]
fn aws_detection_without_a_configured_region_leaves_the_region_unset() {
    let executor = RuntimeProbeExecutor::new([ok(CALLER_IDENTITY), failed(b"")]);

    assert_eq!(detect_aws(&executor).unwrap().region, None);
}

#[test]
fn the_aws_step_asks_nothing_when_no_aws_credentials_were_detected() {
    let mut input = b"".as_slice();
    let mut output = Vec::new();

    let aws = prompt_aws_target(&mut input, &mut output, None).unwrap();

    assert_eq!(aws, None);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("skipping the AWS target"), "{output}");
    assert!(!output.contains("[y/N]"), "{output}");
}

#[test]
fn the_aws_step_defaults_region_and_ssh_user_when_the_answers_are_blank() {
    let account = AwsAccount {
        account: "123456789012".into(),
        arn: "arn:aws:iam::123456789012:user/dev".into(),
        region: Some("us-east-1".into()),
    };
    let mut input = b"y\nhel-runson\n\n\n\n".as_slice();
    let mut output = Vec::new();

    let aws = prompt_aws_target(&mut input, &mut output, Some(&account))
        .unwrap()
        .unwrap();

    assert_eq!(
        aws,
        AwsTargetInput {
            launch_template: "hel-runson".into(),
            region: "us-east-1".into(),
            ssh_user: DEFAULT_AWS_SSH_USER.into(),
            identity_file: None,
        }
    );
    let config = build_config_with_runtime(&[], None, None, Some(&aws), None);
    let TargetTemplate::AwsEc2 {
        region,
        launch_template,
        ssh_user,
        ..
    } = &config.targets[AWS_TARGET_ID]
    else {
        panic!("setup must write an aws-ec2 target");
    };
    assert_eq!(region, "us-east-1");
    assert_eq!(launch_template, "hel-runson");
    assert_eq!(ssh_user, DEFAULT_AWS_SSH_USER);
    config.validate().unwrap();
}

const SSH_CONFIG_FIXTURE: &str = r#"
# Personal hosts
Host *
ServerAliveInterval 60

Host builder build.example.com
HostName build.example.com
User dev

Host bastion
  HostName 10.0.0.1
  IdentityFile ~/.ssh/id_ed25519

Host prod-*
User deploy

Host !staging *.internal
User deploy

Host builder
Compression yes
"#;

#[test]
fn ssh_config_parsing_keeps_concrete_aliases_and_drops_pattern_blocks() {
    let aliases = ssh_config_aliases(SSH_CONFIG_FIXTURE);

    assert_eq!(
        aliases,
        vec!["builder", "build.example.com", "bastion"],
        "wildcard, negated, and duplicate entries must not appear"
    );
}

#[test]
fn ssh_config_parsing_returns_nothing_for_a_config_of_only_wildcards() {
    assert!(
        ssh_config_aliases(
            "Host *
  User dev
"
        )
        .is_empty()
    );
    assert!(ssh_config_aliases("").is_empty());
}

#[test]
fn the_ssh_step_asks_nothing_when_the_ssh_config_has_no_aliases() {
    let mut input = b"".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        prompt_ssh_target(&mut input, &mut output, &[], &BTreeMap::new()).unwrap(),
        None
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("skipping the SSH target"), "{output}");
    assert!(!output.contains("[y/N]"), "{output}");
}

#[test]
fn declining_the_ssh_step_writes_no_ssh_target() {
    let aliases = vec!["builder".to_owned()];
    let mut input = b"\n".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        prompt_ssh_target(&mut input, &mut output, &aliases, &BTreeMap::new()).unwrap(),
        None
    );
    let config = build_config_with_runtime(&[], None, None, None, None);
    assert!(!config.targets.values().any(|target| matches!(
        target,
        TargetTemplate::SshBare { .. }
            | TargetTemplate::SshPodman { .. }
            | TargetTemplate::SshDocker { .. }
    )));
}

#[test]
fn accepting_the_ssh_step_can_write_a_docker_target() {
    let aliases = vec!["builder".to_owned()];
    // yes, host 1, Docker, default image, and the default target name.
    let mut input = b"y\n1\ndocker\n\n\n".as_slice();
    let mut output = Vec::new();

    let ssh = prompt_ssh_target(&mut input, &mut output, &aliases, &BTreeMap::new())
        .unwrap()
        .unwrap();
    assert_eq!(
        ssh.kind,
        SshTargetKind::Docker {
            image: DEFAULT_IMAGE.into()
        }
    );
    let config = build_config_with_runtime(&[], None, None, None, Some(&ssh));
    let TargetTemplate::SshDocker { ssh, container } = &config.targets["builder"] else {
        panic!("setup must write an ssh-docker target");
    };
    assert_eq!(ssh.host, "builder");
    assert_eq!(container.image, DEFAULT_IMAGE);
    config.validate().unwrap();
}

#[test]
fn the_ssh_step_rejects_an_unknown_runtime_before_asking_for_an_image() {
    let aliases = vec!["builder".to_owned()];
    let mut input = b"y\n1\ncontainerd\ndocker\n\n\n".as_slice();
    let mut output = Vec::new();

    let ssh = prompt_ssh_target(&mut input, &mut output, &aliases, &BTreeMap::new())
        .unwrap()
        .unwrap();

    assert!(matches!(ssh.kind, SshTargetKind::Docker { .. }));
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Runtime must be"), "{output}");
    assert_eq!(output.matches("Container image").count(), 1);
}

#[test]
fn accepting_the_ssh_step_writes_an_ssh_podman_target_with_the_default_image() {
    let aliases = vec!["builder".to_owned(), "bastion".to_owned()];
    // yes, host 1, podman (default), default image and name.
    let mut input = b"y\n1\n\n\n\n".as_slice();
    let mut output = Vec::new();

    let ssh = prompt_ssh_target(&mut input, &mut output, &aliases, &BTreeMap::new())
        .unwrap()
        .unwrap();

    assert_eq!(
        ssh,
        SshTargetInput {
            name: "builder".into(),
            host: "builder".into(),
            kind: SshTargetKind::Podman {
                image: DEFAULT_IMAGE.into()
            },
        }
    );
    let config = build_config_with_runtime(&[], None, None, None, Some(&ssh));
    let TargetTemplate::SshPodman { ssh, container, .. } = &config.targets["builder"] else {
        panic!("setup must write an ssh-podman target");
    };
    assert_eq!(ssh.host, "builder");
    assert_eq!(ssh.user, None);
    assert_eq!(ssh.identity_file, None);
    assert_eq!(container.image, DEFAULT_IMAGE);
    config.validate().unwrap();
}

#[test]
fn accepting_the_ssh_step_writes_an_ssh_bare_target_under_a_chosen_name() {
    let aliases = vec!["builder".to_owned()];
    // yes, typed host, default guardian permissions, no podman, custom name.
    let mut input = b"y\nother.example.com\nn\n\nremote\n".as_slice();
    let mut output = Vec::new();

    let ssh = prompt_ssh_target(&mut input, &mut output, &aliases, &BTreeMap::new())
        .unwrap()
        .unwrap();

    assert_eq!(
        ssh,
        SshTargetInput {
            name: "remote".into(),
            host: "other.example.com".into(),
            kind: SshTargetKind::Bare {
                permissions: PermissionMode::Guardian,
            },
        }
    );
    let config = build_config_with_runtime(&[], None, None, None, Some(&ssh));
    let TargetTemplate::SshBare {
        ssh, permissions, ..
    } = &config.targets["remote"]
    else {
        panic!("setup must write an ssh-bare target");
    };
    assert_eq!(ssh.host, "other.example.com");
    assert_eq!(*permissions, PermissionMode::Guardian);
    config.validate().unwrap();
}

#[test]
fn the_ssh_step_reasks_until_the_name_is_a_free_and_valid_target_id() {
    let aliases = vec!["builder".to_owned()];
    let configured = build_config_with_runtime(
        &[],
        None,
        Some((RuntimeKind::Podman, DEFAULT_IMAGE)),
        None,
        None,
    )
    .targets;
    // yes, host 1, no podman, then: a name that is already taken, a name
    // that is not a usable id, and finally a free one.
    let mut input = b"y\n1\nn\n\npodman\nbuild host\nbuilder\n".as_slice();
    let mut output = Vec::new();

    let ssh = prompt_ssh_target(&mut input, &mut output, &aliases, &configured)
        .unwrap()
        .unwrap();

    assert_eq!(ssh.name, "builder");
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("Target podman is already configured"),
        "{output}"
    );
    assert!(output.contains("invalid target id"), "{output}");
}

#[test]
fn the_ssh_step_stops_asking_for_a_name_once_the_input_ends() {
    let aliases = vec!["podman".to_owned()];
    let configured = build_config_with_runtime(
        &[],
        None,
        Some((RuntimeKind::Podman, DEFAULT_IMAGE)),
        None,
        None,
    )
    .targets;
    // yes, host 1, no podman, then nothing: the default name collides, so
    // the question can never be answered.
    let mut input = b"y\n1\nn\n\n".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        prompt_ssh_target(&mut input, &mut output, &aliases, &configured).unwrap(),
        None
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Input ended; skipping"), "{output}");
}

#[test]
fn an_ssh_target_never_replaces_a_target_configured_earlier() {
    let ssh = SshTargetInput {
        name: "podman".into(),
        host: "builder".into(),
        kind: SshTargetKind::Bare {
            permissions: PermissionMode::Guardian,
        },
    };

    let config = build_config_with_runtime(
        &[],
        None,
        Some((RuntimeKind::Podman, DEFAULT_IMAGE)),
        None,
        Some(&ssh),
    );

    assert!(matches!(
        config.targets["podman"],
        TargetTemplate::LocalPodman { .. }
    ));
    assert!(matches!(
        config.targets["podman-2"],
        TargetTemplate::SshBare { .. }
    ));
    config.validate().unwrap();
}

#[test]
fn the_github_origin_is_discovered_through_the_shared_executor() {
    let executor = RuntimeProbeExecutor::new([ok(b"git@github.com:BrokkAi/hel.git\n")]);

    let repository = discover_github_repository(&executor, Path::new("/work/hel")).unwrap();

    assert_eq!(repository.source(), "BrokkAi/hel");
    let commands = executor.commands.borrow();
    assert_eq!(commands[0].program, "git");
    assert_eq!(
        commands[0].args,
        ["-C", "/work/hel", "remote", "get-url", "origin"]
    );
}

#[test]
fn no_github_origin_is_reported_when_the_probe_fails() {
    let failing = RuntimeProbeExecutor::new([failed(b"not a git repository")]);
    assert_eq!(
        discover_github_repository(&failing, Path::new("/work/plain")),
        None
    );

    let missing = RuntimeProbeExecutor::new([]);
    assert_eq!(
        discover_github_repository(&missing, Path::new("/work/plain")),
        None
    );
}

#[test]
fn declining_the_aws_step_writes_no_aws_target() {
    let account = AwsAccount {
        account: "123456789012".into(),
        arn: "arn:aws:iam::123456789012:user/dev".into(),
        region: None,
    };
    let mut input = b"\n".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        prompt_aws_target(&mut input, &mut output, Some(&account)).unwrap(),
        None
    );
    let config = build_config_with_runtime(&[], None, None, None, None);
    assert!(!config.targets.contains_key(AWS_TARGET_ID));
}

/// The first smoke test pulls the image, which for the default image is about
/// 2 GB and prints nothing while it runs, so setup says so before it starts.
#[test]
fn smoke_test_announces_the_image_download_before_it_starts() {
    for (runtime, engine) in [
        (RuntimeKind::Podman, "Podman"),
        (RuntimeKind::Docker, "Docker"),
    ] {
        let mut output = Vec::new();
        run_smoke_test(
            &mut output,
            &smoke_target(runtime, mj_core::config::DEFAULT_CONTAINER_IMAGE),
            &FakeExecutor::succeeds(),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains(mj_core::config::DEFAULT_CONTAINER_IMAGE),
            "{output}"
        );
        assert!(
            output.contains(&format!("{engine} downloads it first")),
            "{output}"
        );
        assert!(output.contains("about 2 GB"), "{output}");
    }

    let mut output = Vec::new();
    run_smoke_test(
        &mut output,
        &smoke_target(RuntimeKind::Podman, "ubuntu:24.04"),
        &FakeExecutor::succeeds(),
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("ubuntu:24.04"), "{output}");
    assert!(
        !output.contains("2 GB"),
        "a custom image's size is unknown: {output}"
    );
}

#[test]
fn smoke_test_removes_the_container_after_a_failed_command() {
    let executor = FakeExecutor {
        commands: RefCell::new(vec![]),
        statuses: vec![0, 1, 0],
    };
    let mut output = Vec::new();

    assert!(
        run_smoke_test(
            &mut output,
            &smoke_target(RuntimeKind::Podman, "ubuntu:24.04"),
            &executor
        )
        .is_err()
    );
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[2].args[0], "rm");
}

#[test]
fn docker_smoke_test_exercises_the_managed_overlay_attachment_path() {
    let executor = FakeExecutor::succeeds();
    let mut output = Vec::new();

    run_smoke_test(
        &mut output,
        &smoke_target(RuntimeKind::Docker, "ubuntu:24.04"),
        &executor,
    )
    .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].program, "sh");
    assert!(commands[0].args[1].contains("docker volume create"));
    assert!(commands[0].args[1].contains("type=overlay"));
    assert_eq!(commands[1].program, "docker");
    assert_eq!(commands[1].args[0], "exec");
    assert_eq!(commands[2].program, "sh");
    assert!(commands[2].args[1].contains("docker volume rm --force"));
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("writable OverlayFS attachment")
    );
}

#[test]
fn setup_preserves_working_configuration_and_discovers_new_installations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let home = DiscoveredHome {
        kind: HarnessKind::Codex,
        path: directory.path().join("codex"),
        authenticated: true,
    };
    let repository = GithubRepository {
        owner: "BrokkAi".into(),
        repository: "muse-acp".into(),
    };
    let mut original = build_config_with_runtimes(
        std::slice::from_ref(&home),
        Some(&repository),
        &[],
        None,
        None,
    );
    original.profiles.get_mut("codex").unwrap().enabled = false;
    original
        .profiles
        .get_mut("codex")
        .unwrap()
        .environment
        .insert("KEEP".into(), "custom".into());
    original.phone.enabled = false;
    original.save_to(&path).unwrap();
    let discovery = SetupDiscovery {
        homes: vec![
            home,
            DiscoveredHome {
                kind: HarnessKind::Muse,
                path: directory.path().join("muse"),
                authenticated: true,
            },
        ],
        repository: Some(GithubRepository {
            owner: "BrokkAi".into(),
            repository: "mjolnir".into(),
        }),
        ..discovery_without_runtimes()
    };
    let executor = FakeExecutor::succeeds();
    for _ in 0..2 {
        let mut output = Vec::new();
        run_setup_dialog_with(
            &mut b"y\n".as_slice(),
            &mut output,
            &path,
            &discovery,
            &executor,
            &executor,
        )
        .unwrap();
        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.profiles["codex"], original.profiles["codex"]);
        assert_eq!(
            saved.bundles["current-repository"],
            original.bundles["current-repository"]
        );
        assert_eq!(saved.targets, original.targets);
        assert_eq!(saved.phone, original.phone);
        assert_eq!(saved.profiles.len(), 2);
        assert_eq!(saved.profiles["muse"].kind, HarnessKind::Muse);
        assert_eq!(saved.bundles.len(), 2);
        assert_eq!(
            saved.bundles["mjolnir"].repositories[0].github.as_deref(),
            Some("BrokkAi/mjolnir")
        );
    }
}

#[test]
fn setup_target_conflict_keeps_working_target_and_can_add_alternative() {
    let original = build_config_with_runtimes(
        &[],
        None,
        &[(RuntimeKind::Podman, "original:image")],
        None,
        None,
    );
    for (answer, expected_count) in [("\n", 1), ("a\n", 2)] {
        let discovered = build_config_with_runtimes(
            &[],
            None,
            &[(RuntimeKind::Podman, "new:image")],
            None,
            None,
        );
        let mut output = Vec::new();
        let additions =
            reconcile_setup(&mut answer.as_bytes(), &mut output, &original, discovered).unwrap();
        let mut saved = original.clone();
        apply_setup_additions(&mut saved, &additions).unwrap();
        assert_eq!(saved.targets["podman"], original.targets["podman"]);
        assert_eq!(
            saved.targets.len(),
            original.targets.len() + expected_count - 1
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Existing sessions will keep using it")
        );
        if expected_count == 2 {
            assert_eq!(
                saved.targets["podman-2"],
                local_runtime_target(RuntimeKind::Podman, "new:image").1
            );
        }
    }
}

#[test]
fn setup_reloads_concurrent_changes_and_cancellation_writes_nothing() {
    struct ConcurrentEdit<'a> {
        path: &'a Path,
        answer: &'a str,
        conflict: bool,
    }
    impl SetupPrompter for ConcurrentEdit<'_> {
        fn read_prompt(&mut self, _: &mut dyn Write, label: &str) -> Result<Option<String>> {
            assert!(label.starts_with("Write this configuration?"));
            Config::update_to(self.path, |config| {
                config.phone.enabled = false;
                if self.conflict {
                    config.targets.insert(
                        "localhost".into(),
                        local_runtime_target(RuntimeKind::Docker, "concurrent:image").1,
                    );
                }
                Ok(())
            })?;
            Ok(Some(self.answer.into()))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let executor = FakeExecutor::succeeds();
    let discovery = discovery_without_runtimes();
    for (answer, conflict) in [("y", false), ("n", false), ("y", true)] {
        Config::default().save_to(&path).unwrap();
        let outcome = run_setup_dialog_inner(
            &mut ConcurrentEdit {
                path: &path,
                answer,
                conflict,
            },
            &mut Vec::new(),
            &path,
            &discovery,
            &executor,
            &executor,
        );
        let saved = Config::load_from(&path).unwrap();
        assert!(!saved.phone.enabled);
        if conflict {
            assert!(
                outcome
                    .unwrap_err()
                    .to_string()
                    .contains("changed while setup was open")
            );
            assert_eq!(
                saved.targets["localhost"],
                local_runtime_target(RuntimeKind::Docker, "concurrent:image").1
            );
        } else if answer == "n" {
            assert_eq!(outcome.unwrap(), SetupOutcome::Cancelled);
            assert!(saved.targets.is_empty());
        } else {
            assert_eq!(outcome.unwrap(), SetupOutcome::Written);
            assert!(matches!(
                saved.targets["localhost"],
                TargetTemplate::LocalBare
            ));
        }
    }
}

#[test]
fn dialog_configures_every_usable_runtime_as_a_normal_target() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let discovery = SetupDiscovery {
        homes: vec![DiscoveredHome {
            kind: HarnessKind::Codex,
            path: PathBuf::from("/profiles/codex"),
            authenticated: true,
        }],
        repository: Some(GithubRepository {
            owner: "BrokkAi".into(),
            repository: "hel".into(),
        }),
        runtimes: vec![
            RuntimeProbe {
                kind: RuntimeKind::Podman,
                usable: true,
                detail: "podman version 5".into(),
                remediation: None,
            },
            RuntimeProbe {
                kind: RuntimeKind::Docker,
                usable: true,
                detail: "docker version 29".into(),
                remediation: None,
            },
        ],
        aws: None,
        ssh_hosts: vec![],
    };
    let executor = FakeExecutor::succeeds();
    let mut input = b"\ny\n".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        run_setup_dialog_with(
            &mut input,
            &mut output,
            &config_path,
            &discovery,
            &executor,
            &executor,
        )
        .unwrap(),
        SetupOutcome::Written
    );
    assert!(config_path.exists());
    let config = Config::load_from(&config_path).unwrap();
    assert!(matches!(
        config.targets["podman"],
        TargetTemplate::LocalPodman { .. }
    ));
    assert!(matches!(
        config.targets["docker"],
        TargetTemplate::LocalDocker { .. }
    ));
    let smoke = executor.commands.borrow()[..3]
        .iter()
        .map(|command| command.args[0].clone())
        .collect::<Vec<_>>();
    assert_eq!(smoke, ["run", "exec", "rm"]);
    let commands = executor.commands.borrow();
    assert!(commands.len() >= 6);
    assert_eq!(commands[3].program, "sh");
    assert_eq!(commands[4].program, "docker");
    assert_eq!(commands[5].program, "sh");
    drop(commands);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Podman target using"), "{output}");
    assert!(output.contains("Docker target using"), "{output}");
    assert!(!output.contains("Recommended runtime"), "{output}");
    assert!(!output.contains("Runtime ("), "{output}");
    assert!(output.ends_with(
        "Run `mj` to open Mjolnir, then press n in the Sessions pane to start your first session.\n"
    ));
}

#[test]
fn a_failed_smoke_test_becomes_a_fixable_line_in_the_closing_report() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let discovery = SetupDiscovery {
        runtimes: vec![RuntimeProbe {
            kind: RuntimeKind::Podman,
            usable: true,
            detail: "podman version 5".into(),
            remediation: None,
        }],
        ..discovery_without_runtimes()
    };
    // Create the container, fail the command inside it, remove it.
    let executor = FakeExecutor {
        commands: RefCell::new(vec![]),
        statuses: vec![0, 1, 0],
    };
    let mut input = b"\ny\n".as_slice();
    let mut output = Vec::new();

    let outcome = run_setup_dialog_with(
        &mut input,
        &mut output,
        &config_path,
        &discovery,
        &executor,
        &executor,
    )
    .unwrap();

    assert_eq!(outcome, SetupOutcome::Written);
    assert!(config_path.exists());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("fixable Podman smoke test"), "{output}");
    assert!(
        output.contains("remediation: Fix the configured image or the Podman runtime"),
        "{output}"
    );
    // The report the user was promised still runs, and still ends with the
    // instruction to apply the remediations it just listed.
    assert!(
        output.contains("Running `mj doctor` checks on the new config..."),
        "{output}"
    );
    assert!(
        output.contains("Apply the remediations above, then rerun `mj doctor`."),
        "{output}"
    );
    assert!(
        output.ends_with("Run `mj` to open Mjolnir, then press n in the Sessions pane to start your first session.\n"),
        "{output}"
    );
}

#[test]
fn setup_finishes_with_the_standard_doctor_report_for_the_config_it_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let discovery = SetupDiscovery {
        homes: vec![DiscoveredHome {
            kind: HarnessKind::Codex,
            path: directory.path().join("missing-codex-home"),
            authenticated: false,
        }],
        ..discovery_without_runtimes()
    };
    let executor = FakeExecutor::succeeds();
    let mut input = b"y\n".as_slice();
    let mut output = Vec::new();

    run_setup_dialog_with(
        &mut input,
        &mut output,
        &config_path,
        &discovery,
        &executor,
        &executor,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    // The report is doctor's own rendering: a status-prefixed line per
    // check, plus the remediation doctor would print for the missing home.
    assert!(
        output.contains(&format!(
            "ready Mjolnir configuration: {} is valid",
            config_path.display()
        )),
        "{output}"
    );
    assert!(output.contains("fixable Harness profile codex"), "{output}");
    assert!(
        output.contains("  remediation: Run `mj login --profile codex`"),
        "{output}"
    );
    assert!(
        output.contains("Apply the remediations above, then rerun `mj doctor`."),
        "{output}"
    );
}

#[test]
fn dialog_configures_raw_localhost_without_a_container_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let discovery = SetupDiscovery {
        homes: vec![DiscoveredHome {
            kind: HarnessKind::Kimi,
            path: PathBuf::from("/profiles/kimi"),
            authenticated: true,
        }],
        repository: None,
        runtimes: vec![RuntimeProbe {
            kind: RuntimeKind::Podman,
            usable: false,
            detail: "not installed".into(),
            remediation: Some("Install Podman.".into()),
        }],
        aws: None,
        ssh_hosts: vec![],
    };
    let executor = FakeExecutor::succeeds();
    let mut input = b"y\n".as_slice();
    let mut output = Vec::new();

    assert_eq!(
        run_setup_dialog_with(
            &mut input,
            &mut output,
            &config_path,
            &discovery,
            &executor,
            &executor,
        )
        .unwrap(),
        SetupOutcome::Written
    );
    let config = Config::load_from(&config_path).unwrap();
    assert!(matches!(
        config.targets["localhost"],
        TargetTemplate::LocalBare
    ));
    // No smoke test runs without a runtime; the trailing commands belong to
    // the doctor report.
    assert!(
        executor
            .commands
            .borrow()
            .iter()
            .all(|command| command.program != "podman" || command.args[0] != "run")
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("DANGER"));
    assert!(output.contains("has no guardian approval mode"));
    assert!(output.contains("raw localhost will still be configured"));
}

#[test]
fn discovered_homes_warn_for_harnesses_without_guardian_approvals() {
    let warning = |kind: HarnessKind| {
        let mut output = Vec::new();
        write_discovered_homes(
            &mut output,
            &[DiscoveredHome {
                kind,
                path: PathBuf::from("/profiles/harness"),
                authenticated: true,
            }],
        )
        .unwrap();
        String::from_utf8(output).unwrap()
    };

    for kind in [HarnessKind::Kimi, HarnessKind::Muse] {
        let output = warning(kind);
        assert!(output.contains("DANGER"), "{kind:?}: {output}");
        assert!(
            output.contains("has no guardian approval mode"),
            "{kind:?}: {output}"
        );
        assert!(output.contains("raw, unsandboxed target"), "{output}");
    }

    for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Grok] {
        assert!(!warning(kind).contains("DANGER"), "{kind:?}");
    }
}
