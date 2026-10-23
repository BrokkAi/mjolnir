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
        legacy_subagents: Default::default(),
        guardian_review_model: None,
        codex_profile: None,
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

fn podman_info(rootless: bool) -> CommandOutput {
    let json = serde_json::json!({
        "host": {
            "security": { "rootless": rootless },
            "serviceIsRemote": false,
            "idMappings": {
                "uidmap": [
                    { "container_id": 0, "host_id": 1000, "size": 1 },
                    { "container_id": 1, "host_id": 100000, "size": 65536 },
                ],
                "gidmap": [
                    { "container_id": 0, "host_id": 1000, "size": 1 },
                    { "container_id": 1, "host_id": 100000, "size": 65536 },
                ],
            },
        }
    })
    .to_string();
    ok(json.as_bytes())
}

#[test]
fn discovers_default_and_overridden_homes_with_authentication_markers() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let codex = home.join(".codex");
    let kimi = home.join(".kimi-code");
    let grok = home.join(".grok");
    let mimo = home.join(".mimocode");
    let claude = directory.path().join("claude-override");
    fs::create_dir_all(&codex).unwrap();
    fs::create_dir_all(kimi.join("credentials")).unwrap();
    fs::create_dir_all(&grok).unwrap();
    fs::create_dir_all(mimo.join("data")).unwrap();
    fs::create_dir_all(&claude).unwrap();
    fs::write(codex.join("auth.json"), "{}").unwrap();
    fs::write(codex.join("ds.config.toml"), "model = \"deepseek-v5\"\n").unwrap();
    // `codex --profile` refuses a name with a dot, so this is not an overlay.
    fs::write(codex.join("a.b.config.toml"), "").unwrap();
    fs::write(kimi.join("credentials/kimi-code.json"), "{}").unwrap();
    fs::write(grok.join("auth.json"), "{}").unwrap();
    fs::write(mimo.join("data/auth.json"), "{}").unwrap();
    fs::write(claude.join(".credentials.json"), "{}").unwrap();

    let executor = FakeExecutor::succeeds();
    let homes = discover_harness_homes_with_executor(
        Some(&home),
        [(HarnessKind::Claude, claude.clone())],
        &executor,
    );

    assert_eq!(homes.len(), 6);
    assert!(homes.iter().all(|home| home.authenticated));
    let codex_profiles = homes
        .iter()
        .filter(|home| home.path == codex)
        .map(|home| home.codex_profile.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(codex_profiles, [None, Some("ds")]);
    assert!(homes.iter().any(|home| home.path == claude));
    assert!(homes.iter().any(|home| home.path == kimi));
    assert!(
        homes
            .iter()
            .any(|home| home.path == grok && home.kind == HarnessKind::Grok)
    );
    assert!(
        homes
            .iter()
            .any(|home| home.path == mimo && home.kind == HarnessKind::Mimo)
    );
}

#[test]
fn an_installed_mimo_is_offered_before_its_first_login() {
    let directory = tempfile::tempdir().unwrap();
    let executor = FakeExecutor {
        commands: RefCell::new(Vec::new()),
        statuses: vec![1, 1, 1, 1, 1, 1, 0, 1],
    };
    let mut homes = Vec::new();

    discover_installed_harnesses(
        Some(directory.path()),
        &BTreeMap::new(),
        &mut homes,
        &executor,
    );

    assert_eq!(
        homes,
        [DiscoveredHome {
            kind: HarnessKind::Mimo,
            path: directory.path().join(".mimocode"),
            codex_profile: None,
            authenticated: false,
        }]
    );
    assert!(
        executor
            .commands
            .borrow()
            .iter()
            .any(|command| command.program == "mimo")
    );
}

// Hard-won: 1bc3cfe43: dsh-acp harness detection keyed off the dsh-acp bridge
// and ~/.dsh-acp, so a machine with DeepSeek's own CLI installed was never
// detected: the bridge is installed on demand and proves nothing about a
// DeepSeek installation.
#[test]
fn an_installed_dsh_is_offered_before_its_first_login() {
    let directory = tempfile::tempdir().unwrap();
    let executor = FakeExecutor {
        commands: RefCell::new(Vec::new()),
        statuses: vec![1, 1, 1, 1, 1, 1, 1, 0],
    };
    let mut homes = Vec::new();

    discover_installed_harnesses(
        Some(directory.path()),
        &BTreeMap::new(),
        &mut homes,
        &executor,
    );

    assert_eq!(
        homes,
        [DiscoveredHome {
            kind: HarnessKind::Dsh,
            path: directory.path().join(".dsh"),
            codex_profile: None,
            authenticated: false,
        }]
    );
    let commands = executor.commands.borrow();
    let dsh_probes = commands
        .iter()
        .filter(|command| command.program == "dsh")
        .count();
    assert_eq!(dsh_probes, 1, "the probe keys off DeepSeek's own CLI");
    assert!(
        commands.iter().all(|command| command.program != "dsh-acp"),
        "the dsh-acp bridge is installed on demand and is not a detection signal"
    );
}

#[test]
fn a_dsh_home_is_discovered_in_deepseek_clis_conventional_location() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let dsh = home.join(".dsh");
    fs::create_dir_all(&dsh).unwrap();
    let executor = FakeExecutor::succeeds();

    let homes = discover_harness_homes_with_executor(Some(&home), [], &executor);

    assert_eq!(homes.len(), 1);
    assert_eq!(homes[0].kind, HarnessKind::Dsh);
    assert_eq!(homes[0].path, dsh);
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
            codex_profile: None,
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
            codex_profile: None,
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
fn golden_setup_origin_discovery() {
    use std::fmt::Write as _;

    fn append_summary(out: &mut String, repository: Option<GithubRepository>) {
        let summary = SetupReport {
            agents: Vec::new(),
            repository: repository.as_ref().map(GithubRepository::source),
        }
        .summary();
        if summary.is_empty() {
            writeln!(out, "repository prompt: <none>").unwrap();
            writeln!(out, "setup summary: <no project line>").unwrap();
        } else {
            writeln!(out, "repository prompt: {}", repository.unwrap().source()).unwrap();
            writeln!(out, "setup summary: {}", summary.join(" ")).unwrap();
        }
    }

    let mut out = String::new();
    for (label, origin) in [
        ("HTTPS remote", "https://github.com/BrokkAi/hel.git"),
        ("SSH remote", "git@github.com:BrokkAi/hel.git"),
        ("SSH URL", "ssh://git@github.com/BrokkAi/hel.git"),
    ] {
        writeln!(out, "=== {label} (1 setup summary) ===").unwrap();
        append_summary(&mut out, github_repository_from_origin(origin));
    }
    writeln!(out, "=== non-GitHub remote (1 setup summary) ===").unwrap();
    append_summary(
        &mut out,
        github_repository_from_origin("https://example.com/hel"),
    );

    let executor = RuntimeProbeExecutor::new([ok(b"git@github.com:BrokkAi/hel.git\n")]);
    let discovered = discover_github_repository(&executor, Path::new("/work/hel"));
    let commands = executor.commands.borrow();
    assert_eq!(commands[0].program, "git");
    assert_eq!(
        commands[0].args,
        ["-C", "/work/hel", "remote", "get-url", "origin"]
    );
    writeln!(out, "=== shared executor success (1 setup summary) ===").unwrap();
    writeln!(
        out,
        "probe: git -C <working-directory> remote get-url origin"
    )
    .unwrap();
    append_summary(&mut out, discovered);

    let failing = RuntimeProbeExecutor::new([failed(b"not a git repository")]);
    writeln!(out, "=== shared executor failure (1 setup summary) ===").unwrap();
    writeln!(out, "probe: command failed").unwrap();
    append_summary(
        &mut out,
        discover_github_repository(&failing, Path::new("/work/plain")),
    );
    let missing = RuntimeProbeExecutor::new([]);
    writeln!(out, "=== shared executor unavailable (1 setup summary) ===").unwrap();
    writeln!(out, "probe: no result").unwrap();
    append_summary(
        &mut out,
        discover_github_repository(&missing, Path::new("/work/plain")),
    );

    mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "setup-origin-discovery", &out);
}

#[test]
fn runtime_probe_requires_podman_rootless_preflight_on_linux() {
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 5.4.2\n"),
        podman_info(true),
        ok(b"29.0.1 linux\n"),
    ]);
    let runtimes = probe_local_runtimes(&executor, &ApplePlatform::Linux);

    assert_eq!(runtimes.len(), 2);
    assert_eq!(executor.commands.borrow()[0].program, "podman");
    assert_eq!(executor.commands.borrow()[0].args, ["--version"]);
    assert_eq!(
        executor.commands.borrow()[1].args,
        ["info", "--format", "json"]
    );
    assert_eq!(executor.commands.borrow()[2].program, "docker");
    assert!(runtimes.iter().all(|runtime| runtime.usable()));
}

#[test]
fn macos_setup_probes_rootless_podman_and_keeps_platform_gated_runtimes_out() {
    for (architecture, major_version) in [("aarch64", 15), ("x86_64", 26)] {
        let executor = RuntimeProbeExecutor::new([
            ok(b"podman version 5.4.2\n"),
            podman_info(true),
            ok(b"29.0.1 linux\n"),
        ]);
        let runtimes = probe_local_runtimes(
            &executor,
            &ApplePlatform::Macos {
                architecture: architecture.into(),
                major_version,
            },
        );
        assert_eq!(executor.commands.borrow().len(), 3);
        assert_eq!(executor.commands.borrow()[0].program, "podman");
        assert_eq!(executor.commands.borrow()[2].program, "docker");

        let podman = runtimes
            .iter()
            .find(|runtime| runtime.kind == RuntimeKind::Podman)
            .unwrap();
        assert!(podman.usable());

        let apple = runtimes
            .iter()
            .find(|runtime| runtime.kind == RuntimeKind::AppleContainer)
            .unwrap();
        assert_eq!(apple.status, CheckStatus::Unsupported);
        assert!(!apple.usable());
        assert!(apple.remediation.is_none());
    }
}

#[test]
fn supported_macos_setup_probes_apple_daemon_without_running_a_smoke_test() {
    let platform = ApplePlatform::Macos {
        architecture: "aarch64".into(),
        major_version: 26,
    };
    let executor = RuntimeProbeExecutor::new([
        ok(b"podman version 5.4.2\n"),
        podman_info(true),
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
    assert_eq!(executor.commands.borrow().len(), 5);

    let missing = RuntimeProbeExecutor::new([
        ok(b"podman version 5.4.2\n"),
        podman_info(true),
        ok(b"29.0.1 linux\n"),
    ]);
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
