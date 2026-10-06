use std::cell::RefCell;
use std::fs;

use super::*;
use crate::targets::CommandOutput;
use anyhow::Result;

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
        subagents: Default::default(),
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
        "github.com:BrokkAi/hel",
        "https://github.com/BrokkAi/hel.git",
        "git@github.com:BrokkAi/hel.git",
        "ssh://git@github.com/BrokkAi/hel.git",
        "ssh://git@ssh.github.com:443/BrokkAi/hel.git",
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
fn runtime_probe_requires_podman_rootless_preflight_on_linux() {
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 5.4.2\n"),
        ok(b"0 1000 1\n1 100000 65536\n"),
        ok(b"29.0.1 linux\n"),
    ]);
    let runtimes = probe_local_runtimes(&executor, &ApplePlatform::Linux);

    assert_eq!(runtimes.len(), 2);
    assert_eq!(executor.commands.borrow()[0].program, "podman");
    assert_eq!(executor.commands.borrow()[0].args, ["--version"]);
    assert_eq!(
        executor.commands.borrow()[1].args,
        ["unshare", "cat", "/proc/self/uid_map"]
    );
    assert_eq!(executor.commands.borrow()[2].program, "docker");
    assert!(runtimes.iter().all(|runtime| runtime.usable()));
}

#[test]
fn unusable_podman_carries_the_doctor_remediation_into_the_runtime_list() {
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 3.4.7\n"),
        failed(b"docker is unavailable"),
    ]);

    let runtimes = probe_local_runtimes(&executor, &ApplePlatform::Linux);

    assert_eq!(runtimes.len(), 2);
    assert!(!runtimes[0].usable());
    let remediation = runtimes[0].remediation.as_deref().unwrap();
    assert!(
        remediation.contains("Install or upgrade Podman"),
        "{remediation}"
    );
}

#[test]
fn macos_setup_skips_unsupported_runtimes_without_install_advice() {
    for (architecture, major_version) in [("aarch64", 15), ("x86_64", 26)] {
        let executor = RuntimeProbeExecutor::new([ok(b"29.0.1 linux\n")]);
        let runtimes = probe_local_runtimes(
            &executor,
            &ApplePlatform::Macos {
                architecture: architecture.into(),
                major_version,
            },
        );
        assert_eq!(executor.commands.borrow().len(), 1);
        assert_eq!(executor.commands.borrow()[0].program, "docker");
        for kind in [RuntimeKind::Podman, RuntimeKind::AppleContainer] {
            let runtime = runtimes
                .iter()
                .find(|runtime| runtime.kind == kind)
                .unwrap();
            assert_eq!(runtime.status, CheckStatus::Unsupported);
            assert!(!runtime.usable());
            assert!(runtime.remediation.is_none());
        }
    }
}

#[test]
fn supported_macos_setup_probes_apple_daemon_without_running_a_smoke_test() {
    let platform = ApplePlatform::Macos {
        architecture: "aarch64".into(),
        major_version: 26,
    };
    let executor = RuntimeProbeExecutor::new([
        ok(b"29.0.1 linux\n"),
        ok(b"container version 1\n"),
        ok(b"running\n"),
    ]);
    let runtimes = probe_local_runtimes(&executor, &platform);
    let apple = runtimes
        .iter()
        .find(|runtime| runtime.kind == RuntimeKind::AppleContainer)
        .unwrap();
    assert!(apple.usable());
    assert_eq!(executor.commands.borrow().len(), 3);

    let missing = RuntimeProbeExecutor::new([ok(b"29.0.1 linux\n")]);
    let runtimes = probe_local_runtimes(&missing, &platform);
    let apple = runtimes
        .iter()
        .find(|runtime| runtime.kind == RuntimeKind::AppleContainer)
        .unwrap();
    assert_eq!(apple.status, CheckStatus::Fixable);
    assert!(
        apple
            .remediation
            .as_deref()
            .unwrap()
            .contains("official signed package")
    );
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
