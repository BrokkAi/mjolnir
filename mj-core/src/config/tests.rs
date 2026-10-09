use super::*;

/// R4-6: `[targets.x] kind = "podman"` without `image` stopped the daemon
/// from starting ("missing field `image`"), and `extra_run_args`, which is not
/// a target setting, was accepted without a word.
// Hard-won: 412f0adc: a missing container image stopped daemon startup and unknown config keys vanished silently
#[test]
fn a_container_target_without_an_image_uses_the_default_and_names_unknown_keys() {
    for kind in ["podman", "docker"] {
        let config: Config = toml::from_str(&format!(
            "version = {CONFIG_VERSION}\n[targets.r4-{kind}]\nkind = \"{kind}\"\n\
             extra_run_args = [\"--network=host\"]\n"
        ))
        .unwrap_or_else(|error| panic!("{kind}: {error:#}"));
        let (TargetTemplate::LocalPodman { container } | TargetTemplate::LocalDocker { container }) =
            &config.targets[&format!("r4-{kind}")]
        else {
            panic!("{kind} changed kind")
        };
        assert_eq!(container.image, DEFAULT_CONTAINER_IMAGE);
    }

    let table = serde_json::json!({
        "kind": "podman", "image": "a:1", "machine": "local", "pull_policy": "never",
        "platform": "linux/amd64", "cpus": "2", "memory": "4g", "environment": {},
        "workspace_storage": {"kind": "container-layer"}, "extra_run_args": ["--network=host"],
    });
    let table = table.as_object().unwrap();
    assert_eq!(
        newly_unknown_target_keys("r4-unknown-keys", "podman", table),
        ["extra_run_args"]
    );
    assert!(
        newly_unknown_target_keys("r4-unknown-keys", "podman", table).is_empty(),
        "each unknown key is reported once"
    );
    let bare = serde_json::json!({"kind": "bare", "machine": "box", "permissions": "guardian"});
    assert!(newly_unknown_target_keys("r4-bare", "bare", bare.as_object().unwrap()).is_empty());

    // The list of known keys is every setting a container table can carry.
    let every_setting = ContainerTemplate {
        image: "a:1".into(),
        pull_policy: ImagePullPolicy::Never,
        platform: Some("linux/amd64".into()),
        cpus: Some("2".into()),
        memory: Some("4g".into()),
        environment: BTreeMap::from([("A".into(), "1".into())]).into(),
        workspace_storage: PodmanWorkspaceStorage::ContainerLayer,
        build_cache: Some(TargetBuildCache {
            enabled: Some(true),
            directory: None,
            max_total_size: None,
            scheduler: Default::default(),
        }),
    };
    let serialized = serde_json::to_value(&every_setting).unwrap();
    let mut keys: Vec<_> = serialized.as_object().unwrap().keys().cloned().collect();
    let mut known: Vec<_> = targets::CONTAINER_TEMPLATE_KEYS.map(str::to_owned).to_vec();
    keys.sort();
    known.sort();
    assert_eq!(keys, known);
}

/// A machine whose file never named a workspace directory keeps using the
/// directory its workspaces are already in, under the former product name;
/// new machines get Mjolnir's own. Launch campaign finding C-17.
#[test]
fn a_machine_without_a_workspace_directory_keeps_the_existing_one() {
    let config: Config = toml::from_str(&format!(
        "version = {CONFIG_VERSION}\n[machines.box]\nkind = \"ssh\"\nhost = \"box\"\n"
    ))
    .unwrap();
    let Some(Machine::Ssh {
        workspace_prefix, ..
    }) = config.machines.get("box")
    else {
        panic!("an SSH machine");
    };
    assert_eq!(workspace_prefix, Path::new(LEGACY_WORKSPACE_PREFIX));
    assert_eq!(LEGACY_WORKSPACE_PREFIX, ".local/share/hel/workspaces");
    assert_eq!(DEFAULT_WORKSPACE_PREFIX, ".local/share/mjolnir/workspaces");
}

fn zai_profile(home: &Path, environment: BTreeMap<String, String>) -> HarnessProfile {
    fs::write(
        home.join("config.toml"),
        "model = \"glm-5.3\"\n\
         model_provider = \"zai\"\n\
         [model_providers.zai]\n\
         base_url = \"https://api.z.ai/api/v1\"\n\
         env_key = \"ZAI_API_KEY\"\n\
         wire_api = \"responses\"\n",
    )
    .expect("write Codex configuration");
    HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.to_path_buf(),
        environment: environment.into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    }
}

/// #1160: a Codex profile that signs in with ChatGPT must never be able to use
/// an API key. It loses every variable Codex would take a credential or an
/// address from; a profile that uses an API key keeps them.
// Hard-won: c4e2838d: a leaked API key broke ChatGPT Codex turns
#[test]
fn only_a_codex_profile_that_uses_an_api_key_keeps_the_openai_key_variables() {
    let environment = || {
        BTreeMap::from([
            ("OPENAI_API_KEY".to_owned(), "sk-svcacct-test".to_owned()),
            ("CODEX_API_KEY".to_owned(), "sk-test".to_owned()),
            ("CODEX_ACCESS_TOKEN".to_owned(), "token".to_owned()),
            (
                "OPENAI_BASE_URL".to_owned(),
                "https://example.invalid/v1".to_owned(),
            ),
            ("RUST_LOG".to_owned(), "info".to_owned()),
        ])
    };
    let profile = |kind: HarnessKind, home: &Path| HarnessProfile {
        enabled: true,
        kind,
        home: home.to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };
    let every_name = CODEX_CREDENTIAL_ENVIRONMENT.map(str::to_owned).to_vec();

    // A ChatGPT login, as `codex login` writes it.
    let chatgpt = tempfile::tempdir().expect("temporary home");
    fs::write(
        chatgpt.path().join("auth.json"),
        r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":"a"}}"#,
    )
    .expect("write login");
    let chatgpt = profile(HarnessKind::Codex, chatgpt.path());
    assert_eq!(chatgpt.codex_login(), Some(CodexLogin::ChatGpt));
    let mut launched = environment();
    assert_eq!(
        chatgpt.exclude_harness_environment(&mut launched),
        every_name
    );
    assert_eq!(launched.keys().collect::<Vec<_>>(), ["RUST_LOG"]);

    // A profile with no login yet has no key of its own either.
    let fresh = profile(HarnessKind::Codex, Path::new("/does/not/exist"));
    assert_eq!(fresh.codex_login(), Some(CodexLogin::ChatGpt));
    let mut launched = environment();
    assert_eq!(fresh.exclude_harness_environment(&mut launched), every_name);
    assert_eq!(launched.keys().collect::<Vec<_>>(), ["RUST_LOG"]);

    // `codex login --with-api-key` stores the key and says so.
    let api_key = tempfile::tempdir().expect("temporary home");
    fs::write(
        api_key.path().join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
    )
    .expect("write login");
    let api_key = profile(HarnessKind::Codex, api_key.path());
    assert_eq!(api_key.codex_login(), Some(CodexLogin::ApiKey));
    let mut launched = environment();
    assert!(
        api_key
            .exclude_harness_environment(&mut launched)
            .is_empty()
    );
    assert_eq!(launched, environment());

    // A custom provider authenticates however its configuration says.
    let provider = tempfile::tempdir().expect("temporary home");
    let provider = zai_profile(provider.path(), BTreeMap::new());
    assert_eq!(provider.codex_login(), None);
    let mut launched = environment();
    assert!(
        provider
            .exclude_harness_environment(&mut launched)
            .is_empty()
    );
    assert_eq!(launched, environment());

    // Other harnesses do not read these variables for their own login.
    let claude = tempfile::tempdir().expect("temporary home");
    let claude = profile(HarnessKind::Claude, claude.path());
    assert_eq!(claude.codex_login(), None);
    let mut launched = environment();
    assert!(claude.exclude_harness_environment(&mut launched).is_empty());
    assert_eq!(launched, environment());
}

#[test]
fn an_api_key_codex_profile_needs_its_key_in_the_profile_environment() {
    let home = tempfile::tempdir().expect("temporary home");
    let without_key = zai_profile(home.path(), BTreeMap::new());
    without_key
        .validate("glm")
        .expect("the key lives outside Mjolnir's file, so the configuration is valid");
    let error = without_key
        .ensure_ready("glm")
        .expect_err("a missing key leaves the profile unusable")
        .to_string();
    assert!(error.contains("ZAI_API_KEY = { from_secret"), "{error}");
    assert!(error.contains("[profiles.glm.environment]"), "{error}");

    let with_key = zai_profile(
        home.path(),
        [("ZAI_API_KEY".to_owned(), "secret".to_owned())]
            .into_iter()
            .collect(),
    );
    with_key
        .validate("glm")
        .expect("a configured key validates");
    with_key
        .ensure_ready("glm")
        .expect("a configured key is ready");
    assert_eq!(
        with_key.auth_scheme(),
        AuthScheme::ApiKey {
            env_key: "ZAI_API_KEY".to_owned()
        }
    );
    assert_eq!(with_key.codex_provider_api_key().as_deref(), Some("secret"));
    assert_eq!(
        with_key.authentication_marker(),
        home.path().join("config.toml"),
        "the Codex configuration proves an API-key profile is set up"
    );
    assert_eq!(with_key.credential_freshness(b"{}"), None);
    assert_eq!(with_key.credential_expiry(b"{}"), None);
}

#[test]
fn a_bedrock_codex_profile_uses_the_aws_chain_without_an_api_key_or_login_file() {
    let home = tempfile::tempdir().expect("temporary home");
    fs::write(
        home.path().join("config.toml"),
        "model = \"global.openai.gpt-6-luna\"\n\
         model_provider = \"amazon-bedrock-runtime\"\n\
         [model_providers.amazon-bedrock-runtime.aws]\n\
         region = \"us-east-1\"\n",
    )
    .expect("write Codex configuration");
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new().into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    profile
        .ensure_ready("bedrock")
        .expect("the AWS chain does not need a profile API key");
    assert_eq!(profile.auth_scheme(), AuthScheme::AwsCredentialChain);
    assert!(!profile.auth_scheme().uses_native_login_file());
    assert_eq!(
        profile.authentication_marker(),
        home.path().join("config.toml")
    );
    assert_eq!(profile.credential_freshness(b"{}"), None);
    assert_eq!(profile.credential_expiry(b"{}"), None);
    assert!(
        crate::credentials::login_command(&profile).is_err(),
        "an AWS role profile has no interactive Codex login"
    );
}

#[test]
fn an_inline_token_codex_profile_is_authenticated_without_a_login_file() {
    let home = tempfile::tempdir().expect("temporary home");
    // An inline-token provider never writes `auth.json`, so the profile is
    // proven by the same `config.toml` that carries the key.
    fs::write(
        home.path().join("config.toml"),
        "model = \"deepseek-flash\"\n\
         model_provider = \"deepseek\"\n\
         \n\
         [model_providers.deepseek]\n\
         base_url = \"https://api.deepseek.com/v1\"\n\
         experimental_bearer_token = \"inline-deepseek-key\"\n\
         wire_api = \"responses\"\n",
    )
    .expect("write Codex configuration");
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new().into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    profile
        .validate("deepseek")
        .expect("an inline-token profile is valid");
    profile
        .ensure_ready("deepseek")
        .expect("an inline token needs no profile environment");
    assert_eq!(profile.auth_scheme(), AuthScheme::InlineApiKey);
    assert!(profile.auth_scheme().is_api_key());
    assert!(!profile.auth_scheme().uses_native_login_file());
    assert_eq!(
        profile.authentication_marker(),
        home.path().join("config.toml"),
        "the Codex configuration proves an inline-key profile is set up"
    );
    assert_eq!(profile.credential_freshness(b"{}"), None);
    assert_eq!(profile.credential_expiry(b"{}"), None);
    assert_eq!(
        profile.codex_provider_api_key().as_deref(),
        Some("inline-deepseek-key")
    );
    let error = crate::credentials::login_command(&profile)
        .expect_err("an inline-token profile has no interactive login")
        .to_string();
    assert!(error.contains("inlines its provider API key"), "{error}");
}

/// A Codex home that names its key variable works the way standalone Codex
/// does: an exported key reaches the profile without an `environment` entry,
/// and is never written to config.toml.
// Hard-won: d7a1bb61: an exported provider key worked standalone but failed under Mjolnir
#[test]
fn a_codex_profile_inherits_its_provider_key_from_the_environment_mjolnir_runs_in() {
    let home = tempfile::tempdir().expect("temporary home");
    zai_profile(home.path(), BTreeMap::new());
    let text = format!(
        "version = {CONFIG_VERSION}\n\n[profiles.glm]\nkind = \"codex\"\nhome = {:?}\n",
        home.path().to_string_lossy()
    );
    let read = |process: &[(&str, &str)]| -> Config {
        let process = process
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        with_secret_resolver(SecretResolver::fixed(process, BTreeMap::new()), || {
            toml::from_str(&text)
        })
        .expect("read the configuration")
    };

    let exported = read(&[("ZAI_API_KEY", "from-the-shell")]);
    let profile = &exported.profiles["glm"];
    assert_eq!(profile.environment["ZAI_API_KEY"], "from-the-shell");
    profile
        .ensure_ready("glm")
        .expect("the exported key is enough");
    let written = toml::to_string(&exported).expect("serialize");
    assert!(!written.contains("ZAI_API_KEY"), "{written}");
    assert!(!written.contains("from-the-shell"), "{written}");

    // An entry the profile sets wins over the environment.
    let explicit = format!("{text}\n[profiles.glm.environment]\nZAI_API_KEY = \"configured\"\n");
    let configured: Config = with_secret_resolver(
        SecretResolver::fixed(
            [("ZAI_API_KEY".to_owned(), "from-the-shell".to_owned())].into(),
            BTreeMap::new(),
        ),
        || toml::from_str(&explicit),
    )
    .expect("read the configuration");
    assert_eq!(
        configured.profiles["glm"].environment["ZAI_API_KEY"],
        "configured"
    );

    // Unset everywhere: the configuration still reads, the profile says how
    // to supply the key.
    let unset = read(&[]);
    let error = unset.profiles["glm"]
        .ensure_ready("glm")
        .expect_err("no key anywhere")
        .to_string();
    assert!(error.contains("export ZAI_API_KEY"), "{error}");
    assert!(error.contains("from_secret"), "{error}");
}

// Hard-won: ee59e575: users were told to delete their supported profile model catalog
#[test]
fn a_codex_profile_may_name_its_own_model_catalog() {
    let home = tempfile::tempdir().expect("temporary home");
    let mut profile = zai_profile(
        home.path(),
        [("ZAI_API_KEY".to_owned(), "secret".to_owned())]
            .into_iter()
            .collect(),
    );
    let body = fs::read_to_string(home.path().join("config.toml")).expect("read");
    fs::write(
        home.path().join("config.toml"),
        format!("model_catalog_json = \"mine.json\"\n{body}"),
    )
    .expect("write");
    profile.enabled = true;
    // Mjolnir stages its own catalog and replaces the key there, so a
    // profile-authored file is not a reason to refuse.
    profile
        .validate("glm")
        .expect("a profile-authored catalog validates");
    assert_eq!(
        profile
            .codex_provider()
            .expect("read the profile's provider")
            .expect("a custom provider")
            .model_catalog_json,
        Some(std::path::PathBuf::from("mine.json")),
        "the file the profile names is recorded, though Mjolnir stages its own"
    );
}

/// Saving edits the user's file in place: comments, blank lines, and the
/// order of sections and keys survive a save that changes one value.
/// Launch campaign finding C-21.
// Hard-won: 25a04304: saving config deleted comments and rewrote user layout
#[test]
fn saving_keeps_the_files_comments_and_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let original = format!(
        "# My Mjolnir settings\nversion = {CONFIG_VERSION}\n\n\
         # Quiet, please.\n[notify]\ntitle = false # no counts in the title\nbell = true\n\n\
         [advanced]\n# Clocks help me debug.\ndetailed_activity_clocks = true\n"
    );
    fs::write(&path, &original).unwrap();
    let mut config = Config::load_from(&path).unwrap();
    config.notify.bell = false;
    config.save_to(&path).unwrap();

    let saved = fs::read_to_string(&path).unwrap();
    assert_eq!(saved, original.replace("bell = true", "bell = false"));
    assert_eq!(Config::load_from(&path).unwrap(), config);

    // A version 12 file is marked as this build's, in place.
    fs::write(&path, "# keep me\nversion = 12 # the file format\n").unwrap();
    Config::load_from(&path).unwrap().save_to(&path).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("# keep me\nversion = {CONFIG_VERSION} # the file format\n")
    );
}

/// A reference under `environment` resolves from `secrets.toml` beside the
/// configuration, and a save writes the reference back, never the value.
#[test]
fn environment_references_resolve_from_the_secrets_file_and_survive_a_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let secrets = directory.path().join(SECRETS_FILE);
    fs::write(&secrets, "PROVIDER_API_KEY = \"stored-value\"\n").unwrap();
    let original = format!(
        "version = {CONFIG_VERSION}\n\n[notify]\ntitle = false\nbell = true\n\n\
         [profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n\
         [profiles.codex.environment]\nPROVIDER_API_KEY = {{ from_secret = \"PROVIDER_API_KEY\" }}\n\
         PLAIN = \"value\"\n"
    );
    fs::write(&path, &original).unwrap();
    let mut config = Config::load_from(&path).unwrap();
    let environment = &config.profiles["codex"].environment;
    assert_eq!(environment["PROVIDER_API_KEY"], "stored-value");
    assert_eq!(environment["PLAIN"], "value");
    assert_eq!(
        environment.sources()["PROVIDER_API_KEY"],
        EnvironmentValue::FromSecret("PROVIDER_API_KEY".into())
    );

    config.notify.bell = false;
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("stored-value"), "{saved}");
    assert_eq!(saved, original.replace("bell = true", "bell = false"));
    assert_eq!(Config::load_from(&path).unwrap(), config);

    // A missing secret makes the profile unusable, not the configuration
    // unreadable, and an unrelated save still writes the reference back.
    fs::write(&secrets, "").unwrap();
    let mut config = Config::load_from(&path).unwrap();
    assert_eq!(
        config.profiles["codex"].environment.get("PROVIDER_API_KEY"),
        None
    );
    config.notify.bell = true;
    config.save_to(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

/// Test-and-fix C-8 reported a missing secret as a TOML parse error with a
/// caret hundreds of columns wide. Now the configuration loads, and the
/// profile names the entry and the file in one line where it is used.
// Hard-won: 871a4d9b: one unresolved profile secret prevented daemon startup
#[test]
fn a_missing_secret_is_named_in_one_line_where_its_profile_is_used() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let padding = "X".repeat(400);
    fs::write(
        &path,
        format!(
            "version = {CONFIG_VERSION}\n\n[profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n\
             [profiles.codex.environment]\nFAKE_TOKEN = {{ from_secret = \"FAKE_TOKEN\" }}\nPAD = \"{padding}\"\n"
        ),
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    let error = config.profiles["codex"].ensure_ready("codex").unwrap_err();
    // The person's to fix, so a client is told the reason, not "internal error".
    let refusal = crate::refusal::Refusal::of(&error).expect("the failure is a refusal");
    assert_eq!(refusal.kind(), crate::refusal::RefusalKind::Precondition);
    let error = format!("{error:#}");
    assert!(
        error
            .starts_with("profile \"codex\": FAKE_TOKEN = { from_secret = \"FAKE_TOKEN\" } needs "),
        "{error}"
    );
    assert!(error.contains(SECRETS_FILE), "{error}");
    assert!(!error.contains('\n'), "{error}");
}

#[test]
fn obsolete_startup_settings_are_ignored_and_removed_when_saving() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let expected = sample_config();
    let original = toml::to_string(&expected).unwrap();
    for enabled in [true, false] {
        fs::write(&path, format!("{original}\n[startup]\nenabled = {enabled}\nprompt = false\nprofile = \"missing\"\ntarget = \"missing\"\n")).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, expected);
        assert!(
            serde_json::to_value(&loaded)
                .unwrap()
                .get("startup")
                .is_none()
        );
        loaded.save_to(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("[startup]"));
    }
}

#[test]
fn retired_stopped_session_filters_load_and_are_dropped_on_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let legacy =
        "version = 6\nshow_stopped_sessions = true\n\n[advanced]\nshow_stopped_sessions = true\n";
    fs::write(&path, legacy).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), legacy);

    config.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(!body.contains("show_stopped_sessions"), "{body}");
    assert_eq!(Config::load_from(&path).unwrap(), config);
}

#[test]
fn nested_home_mapping_keeps_config_credentials_and_session_data_together() {
    for (kind, credential) in [
        (HarnessKind::Muse, "auth.json"),
        (HarnessKind::OpenCode, ".data/opencode/auth.json"),
    ] {
        let home = Path::new("/private/session").join(kind.id());
        let mut environment = BTreeMap::from([("XDG_DATA_HOME".into(), "/unrelated".into())]);
        kind.configure_home_environment(&home, &mut environment);
        assert_eq!(environment["XDG_CONFIG_HOME"], "/private/session");
        assert_eq!(
            environment["XDG_DATA_HOME"],
            home.join(".data").to_string_lossy()
        );
        assert_eq!(
            kind.home_from_environment(&environment["XDG_CONFIG_HOME"]),
            home
        );
        assert_eq!(
            harness_authentication_marker(kind, &home),
            home.join(credential),
            "{kind:?}"
        );
    }
}

fn sample_config() -> Config {
    Config {
        version: CONFIG_VERSION,
        keys: Default::default(),
        sessions_side: Default::default(),
        advanced: Default::default(),
        notify: Default::default(),
        spinner: SpinnerStyle::default(),
        theme: Default::default(),
        phone: PhoneConfig::default(),
        github: GithubConfig::default(),
        continuation: Default::default(),
        mailbox: Default::default(),
        review: ReviewConfig::default(),
        sessionwiki: SessionWikiConfig::default(),
        subagents: SubagentConfig::default(),
        jev: Default::default(),
        legacy_startup: (),
        default_targets: Default::default(),
        machines: BTreeMap::new(),
        profiles: BTreeMap::from([(
            "codex-1".into(),
            HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/test/.codex-one"),
                environment: BTreeMap::from([("RUST_LOG".into(), "info".into())]).into(),
                subagents: Default::default(),
                guardian_review_model: None,
            },
        )]),
        bundles: BTreeMap::from([(
            "hel".into(),
            ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![ProjectRepository {
                    id: "app".into(),
                    github: Some("BrokkAi/hel".into()),
                    local: None,
                    destination: PathBuf::from("app"),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman-default".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    build_cache: None,
                    image: "ubuntu:24.04".into(),
                    pull_policy: ImagePullPolicy::Auto,
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: Default::default(),
                    workspace_storage: Default::default(),
                },
            },
        )]),
    }
}

/// A session or probe runs from a home Mjolnir staged for it, so every harness
/// is pointed at that home, macOS included.
#[test]
fn every_harness_is_pointed_at_its_staged_home() {
    for kind in HarnessKind::ALL {
        let mut environment = BTreeMap::new();
        let home = Path::new("/private/session").join(kind.id());
        kind.configure_home_environment(&home, &mut environment);
        assert!(environment.contains_key(kind.home_env()), "{kind:?}");
        assert_eq!(
            kind.home_from_environment(&environment[kind.home_env()]),
            home,
            "{kind:?}"
        );
    }
}

/// Commands that act on the person's own profile home leave Claude's variable
/// unset on macOS, where the login lives in the Keychain, and set it
/// everywhere else.
#[test]
fn only_claude_on_macos_keeps_its_own_home_for_profile_commands() {
    let home = Path::new("/home/me/.claude-work");

    let mut mac = BTreeMap::new();
    HarnessKind::Claude.configure_profile_home_environment(home, HarnessHost::MacOs, &mut mac);
    assert!(mac.is_empty(), "{mac:?}");

    let mut linux = BTreeMap::new();
    HarnessKind::Claude.configure_profile_home_environment(home, HarnessHost::Other, &mut linux);
    assert_eq!(linux["CLAUDE_CONFIG_DIR"], home.to_string_lossy());

    for kind in HarnessKind::ALL {
        let mut environment = BTreeMap::new();
        kind.configure_profile_home_environment(
            Path::new("/home/me/muse"),
            HarnessHost::MacOs,
            &mut environment,
        );
        assert_eq!(
            environment.contains_key(kind.home_env()),
            kind != HarnessKind::Claude,
            "{kind:?}"
        );
    }
}

#[test]
fn harness_mapping_and_permission_modes_are_fixed() {
    assert_eq!(HarnessKind::Codex.home_env(), "CODEX_HOME");
    assert_eq!(HarnessKind::Claude.home_env(), "CLAUDE_CONFIG_DIR");
    assert_eq!(HarnessKind::Kimi.home_env(), "KIMI_CODE_HOME");
    assert_eq!(HarnessKind::Grok.home_env(), "GROK_HOME");
    let codex = HarnessKind::Codex
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    assert_eq!(codex.acp_mode(), Some("agent-full-access"));
    assert_eq!(codex.label(), "agent-full-access");
    let claude = HarnessKind::Claude
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    assert_eq!(claude.acp_mode(), Some("bypassPermissions"));
    let kimi = HarnessKind::Kimi
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    assert_eq!(kimi.acp_mode(), Some("auto"));
}

#[test]
fn unconstrained_enforcement_splits_acp_modes_from_launch_controls() {
    for kind in [HarnessKind::Codex, HarnessKind::Kimi] {
        let enforcement = kind
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(enforcement.acp_mode(), Some(enforcement.label()));
        assert_eq!(enforcement.launch_flag(), None);
    }
    assert_eq!(
        HarnessKind::Codex
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap()
            .launch_environment(),
        Some(("INITIAL_AGENT_MODE", "agent-full-access"))
    );
    let grok = HarnessKind::Grok
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    assert_eq!(grok.acp_mode(), None);
    assert_eq!(grok.launch_flag(), Some("--always-approve"));
    assert_eq!(grok.label(), "always-approve / sandbox-off");
    assert_eq!(grok.launch_environment(), Some(("GROK_SANDBOX", "off")));
    let claude = HarnessKind::Claude
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    assert_eq!(claude.acp_mode(), Some("bypassPermissions"));
    assert_eq!(claude.label(), "bypassPermissions / sandbox-off");
    let muse = HarnessKind::Muse
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .unwrap();
    // allowAll emits no permission requests, so the adapter's client-side
    // auto-review would never fire. Unconstrained Muse reviews instead.
    assert_eq!(muse.acp_mode(), Some("promptUnmatched"));
    assert_eq!(
        muse.label(),
        "promptUnmatched / auto-review / sandbox-off / :ask-me"
    );
    assert_eq!(muse.acp_setting(), Some(("auto_review", "on")));
    assert_eq!(
        muse.launch_environment(),
        Some(("MUSE_APPROVAL_MODE", "promptUnmatched"))
    );
    assert_eq!(
        muse.launch_argument(),
        Some(("MUSE_SERVE_ARGS", "--disable-sandbox"))
    );
    assert_eq!(
        muse.staged_setting().map(|setting| setting.value),
        Some(":ask-me")
    );
}

#[test]
fn configured_approvals_preserve_other_profiles_and_select_guardians() {
    let codex = HarnessKind::Codex
        .execution_enforcement(ExecutionPolicy::ConfiguredApprovals)
        .expect("Codex ACP selects guardian explicitly");
    assert_eq!(codex.acp_mode(), Some("agent"));
    assert_eq!(
        codex.launch_environment(),
        Some(("INITIAL_AGENT_MODE", "agent"))
    );
    let claude = HarnessKind::Claude
        .execution_enforcement(ExecutionPolicy::ConfiguredApprovals)
        .expect("Claude selects its Auto mode as guardian");
    assert_eq!(claude.acp_mode(), Some("auto"));
    assert_eq!(claude.label(), "auto / guardian");
    assert_eq!(claude.session_sandbox(), None);
    assert_eq!(claude.staged_setting(), None);
    let muse = HarnessKind::Muse
        .execution_enforcement(ExecutionPolicy::ConfiguredApprovals)
        .expect("Muse selects promptUnmatched + auto-review as guardian");
    assert_eq!(muse.acp_mode(), Some("promptUnmatched"));
    assert_eq!(muse.acp_setting(), Some(("auto_review", "on")));
    assert_eq!(muse.label(), "promptUnmatched / auto-review / :ask-me");
    assert_eq!(
        muse.launch_environment(),
        Some(("MUSE_APPROVAL_MODE", "promptUnmatched"))
    );
    assert_eq!(muse.launch_argument(), None);
    assert_eq!(
        muse.staged_setting().map(|setting| setting.value),
        Some(":ask-me")
    );

    for kind in [HarnessKind::Kimi, HarnessKind::Grok] {
        assert_eq!(
            kind.execution_enforcement(ExecutionPolicy::ConfiguredApprovals),
            None,
            "{kind:?}"
        );
    }
}

#[test]
fn bridge_args_carry_the_acp_subcommand_per_harness() {
    for policy in [
        ExecutionPolicy::ConfiguredApprovals,
        ExecutionPolicy::Unconstrained,
    ] {
        assert!(HarnessKind::Codex.bridge_args(policy).is_empty());
        assert!(HarnessKind::Claude.bridge_args(policy).is_empty());
        assert_eq!(HarnessKind::Kimi.bridge_args(policy), ["acp"]);
        assert_eq!(
            HarnessKind::Grok.bridge_args(policy),
            if policy.is_unconstrained() {
                vec!["agent", "--always-approve", "stdio"]
            } else {
                vec!["agent", "stdio"]
            },
            "policy: {policy:?}"
        );
    }
}

#[test]
fn bundle_rejects_traversal_and_duplicate_destinations() {
    let mut config = sample_config();
    config.bundles.get_mut("hel").unwrap().repositories[0].destination = PathBuf::from("../escape");
    assert!(format!("{:#}", config.validate().unwrap_err()).contains("'..'"));

    let mut config = sample_config();
    let bundle = config.bundles.get_mut("hel").unwrap();
    bundle.repositories.push(ProjectRepository {
        id: "docs".into(),
        github: Some("BrokkAi/docs".into()),
        local: None,
        destination: PathBuf::from("app"),
        git_ref: None,
    });
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("overlapping destinations")
    );
}

#[test]
fn bundle_requires_existing_primary_repository() {
    let mut config = sample_config();
    config.bundles.get_mut("hel").unwrap().primary_repo = "missing".into();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
}

#[test]
fn bundle_rejects_non_github_sources() {
    let mut config = sample_config();
    config.bundles.get_mut("hel").unwrap().repositories[0].github =
        Some("https://example.com/owner/repo".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not a supported GitHub source")
    );
}

#[test]
fn bundle_accepts_one_absolute_local_source() {
    let mut config = sample_config();
    {
        let repository = &mut config.bundles.get_mut("hel").unwrap().repositories[0];
        repository.github = None;
        // A local source is a directory on this controller.
        repository.local = Some(std::env::temp_dir().join("app"));
    }
    config.validate().unwrap();

    config.bundles.get_mut("hel").unwrap().repositories[0].local =
        Some(PathBuf::from("relative/app"));
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("absolute")
    );
}

#[test]
fn bundle_requires_exactly_one_repository_source() {
    let mut config = sample_config();
    config.bundles.get_mut("hel").unwrap().repositories[0].local =
        Some(PathBuf::from("/home/test/src/app"));
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exactly one")
    );
}

#[test]
fn config_toml_round_trip_is_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/config.toml");
    let config = sample_config();
    config.save_to(&path).unwrap();
    assert_eq!(Config::load_from(&path).unwrap(), config);
    assert!(!fs::read_to_string(&path).unwrap().contains("pull_policy"));
    assert_eq!(
        fs::read_to_string(path)
            .unwrap()
            .matches("kind = \"podman\"")
            .count(),
        1
    );
    assert!(
        fs::read_dir(directory.path().join("nested"))
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
    );
}

#[test]
fn github_app_configuration_is_optional_and_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 1\n").unwrap();
    let legacy = Config::load_from(&path).unwrap();
    assert_eq!(legacy.github, GithubConfig::default());
    assert!(legacy.mailbox.enabled);
    assert!(legacy.agent_mailboxes_enabled());
    legacy.save_to(&path).unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains("[github"));

    let mut configured = Config::default();
    configured.github.app = Some(GithubAppConfig {
        app_id: 1234,
        private_key_path: PathBuf::from("/controller/keys/app.pem"),
        installations: BTreeMap::from([("Acme".into(), 5678)]),
        session_permissions: Some(BTreeMap::from([(
            "contents".into(),
            GithubPermissionLevel::Write,
        )])),
        token_permissions: Some(BTreeMap::from([(
            "statuses".into(),
            GithubPermissionLevel::Read,
        )])),
    });
    configured.github.watch.interval_seconds = 30;
    configured.github.watch.api_base = "http://127.0.0.1:9411".into();
    configured.mailbox.enabled = false;
    assert!(!configured.agent_mailboxes_enabled());
    configured.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("[github.app]"), "{body}");
    assert!(body.contains("[github.app.installations]"), "{body}");
    assert!(body.contains("[github.app.session_permissions]"), "{body}");
    assert!(body.contains("[github.app.token_permissions]"), "{body}");
    assert!(body.contains("[github.watch]"), "{body}");
    assert!(body.contains("[mailbox]\nenabled = false"), "{body}");
    let serialized: toml::Value = toml::from_str(&body).unwrap();
    assert!(serialized["github"]["watch"].get("enabled").is_none());
    assert_eq!(Config::load_from(&path).unwrap(), configured);

    let mut jev_off = Config::default();
    jev_off.jev.enabled = false;
    assert!(!jev_off.agent_mailboxes_enabled());

    fs::write(
        &path,
        "version = 14\n[github.app]\napp_id = 0\nprivate_key_path = 'app.pem'\n",
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(
        error.contains("app_id must be a positive integer"),
        "{error}"
    );
}

#[test]
fn save_review_reloads_latest_config_and_preserves_unrelated_sections() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let initial = sample_config();
    initial.save_to(&path).unwrap();

    // Simulate a concurrent dashboard changing an unrelated section after
    // the editor opened. The review save must start from this newer file.
    let mut latest = Config::load_from(&path).unwrap();
    latest.phone.enabled = false;
    latest
        .profiles
        .get_mut("codex-1")
        .unwrap()
        .environment
        .insert("LATEST_SETTING".into(), "kept".into());
    latest.save_to(&path).unwrap();

    let review = ReviewConfig {
        enabled: true,
        tier: None,
        profile: Some("codex-1".into()),
        model: Some("review-model".into()),
        effort: Some("high".into()),
    };
    let saved = Config::save_review_to(&path, review.clone()).unwrap();
    assert_eq!(saved.review, review);
    assert!(!saved.phone.enabled);
    assert_eq!(
        saved.profiles["codex-1"].environment.get("LATEST_SETTING"),
        Some(&"kept".to_owned())
    );
    assert_eq!(Config::load_from(&path).unwrap(), saved);
}

#[test]
fn update_to_serializes_disjoint_process_edits() {
    const CHILD: &str = "HEL_CONFIG_UPDATE_CHILD";
    const PATH: &str = "HEL_CONFIG_UPDATE_PATH";
    const READY: &str = "HEL_CONFIG_UPDATE_READY";
    const SECOND_STARTED: &str = "HEL_CONFIG_UPDATE_SECOND_STARTED";
    const RELEASE: &str = "HEL_CONFIG_UPDATE_RELEASE";
    let Some(role) = std::env::var_os(CHILD) else {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        sample_config().save_to(&path).unwrap();
        let ready = directory.path().join("ready");
        let second_started = directory.path().join("second-started");
        let release = directory.path().join("release");

        let executable = std::env::current_exe().unwrap();
        let mut first = std::process::Command::new(&executable)
            .args([
                "--exact",
                "config::tests::update_to_serializes_disjoint_process_edits",
                "--nocapture",
            ])
            .env(CHILD, "phone")
            .env(PATH, &path)
            .env(READY, &ready)
            .env(SECOND_STARTED, &second_started)
            .env(RELEASE, &release)
            .spawn()
            .unwrap();
        // The first child writes this only after it has acquired the
        // sibling lock and entered its edit closure.
        let first_entered = (0..1000).any(|_| {
            if ready.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        }) || ready.exists();

        let mut second = std::process::Command::new(&executable)
            .args([
                "--exact",
                "config::tests::update_to_serializes_disjoint_process_edits",
                "--nocapture",
            ])
            .env(CHILD, "profile")
            .env(PATH, &path)
            .env(READY, &ready)
            .env(SECOND_STARTED, &second_started)
            .env(RELEASE, &release)
            .spawn()
            .unwrap();
        let second_reached_update = (0..1000).any(|_| {
            if second_started.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        }) || second_started.exists();
        let second_blocked = second.try_wait().unwrap().is_none();
        // Always release the first child before asserting, so a failed
        // observation cannot leave a child waiting after this test exits.
        fs::write(&release, b"release").unwrap();
        let first_status = first.wait().unwrap();
        let second_status = second.wait().unwrap();
        assert!(first_entered, "first config update child never entered");
        assert!(
            second_reached_update,
            "second config update child never reached its update"
        );
        assert!(second_blocked, "second config update child was not blocked");
        assert!(
            first_status.success(),
            "first config update child failed: {first_status}"
        );
        assert!(
            second_status.success(),
            "second config update child failed: {second_status}"
        );

        let config = Config::load_from(&path).unwrap();
        assert!(!config.phone.enabled);
        assert_eq!(
            config.profiles["codex-1"].environment.get("CONCURRENT"),
            Some(&"kept".to_owned())
        );
        return;
    };

    let path = PathBuf::from(std::env::var_os(PATH).unwrap());
    let role = role.to_string_lossy();
    let ready = PathBuf::from(std::env::var_os(READY).unwrap());
    let second_started = PathBuf::from(std::env::var_os(SECOND_STARTED).unwrap());
    let release = PathBuf::from(std::env::var_os(RELEASE).unwrap());
    if role == "profile" {
        // Confirm the first process owns the stable lock before telling
        // the parent it can release it. This cannot pass merely because
        // the second process was slow to reach update_to.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(config_lock_path(&path))
            .unwrap();
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        fs::write(&second_started, b"started").unwrap();
    }
    Config::update_to(&path, |config| {
        match role.as_ref() {
            "phone" => {
                fs::write(&ready, b"entered").unwrap();
                while !release.exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                config.phone.enabled = false;
            }
            "profile" => {
                config
                    .profiles
                    .get_mut("codex-1")
                    .unwrap()
                    .environment
                    .insert("CONCURRENT".into(), "kept".into());
            }
            other => panic!("unknown config update child role {other:?}"),
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn update_to_failure_leaves_the_previous_file_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    sample_config().save_to(&path).unwrap();
    let before = fs::read(&path).unwrap();

    let error = Config::update_to(&path, |config| {
        config.phone.bind = "not-an-address".into();
        Ok(())
    })
    .unwrap_err();

    assert!(error.to_string().contains("parse phone bind"));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn update_to_refuses_a_newer_config_before_editing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let body = format!("version = {}\nfuture = true\n", CONFIG_VERSION + 1);
    fs::write(&path, &body).unwrap();

    let error = Config::update_to(&path, |config| {
        config.phone.enabled = false;
        Ok(())
    })
    .unwrap_err();

    assert!(error.to_string().contains("newer Mjolnir"));
    assert_eq!(fs::read_to_string(&path).unwrap(), body);
}

#[test]
fn version_one_podman_config_upgrades_to_isolated_workspace_storage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "version = 1\n\n[targets.podman]\nkind = \"local-podman\"\nimage = \"ubuntu:24.04\"\n",
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.version, CONFIG_VERSION);
    let TargetTemplate::LocalPodman { container } = &config.targets["podman"] else {
        panic!("version-one Podman target changed kind")
    };
    assert_eq!(
        container.workspace_storage,
        PodmanWorkspaceStorage::PodmanVolume
    );
    assert!(fs::read_to_string(path).unwrap().starts_with("version = 1"));
}

#[test]
fn old_config_restores_scan_without_rewriting_until_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 2\n").unwrap();

    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.spinner, SpinnerStyle::Scan);
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(fs::read_to_string(&path).unwrap(), "version = 2\n");
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with(&format!("version = {CONFIG_VERSION}")));
    assert!(!saved.contains("spinner"));
}

#[test]
fn every_previous_config_version_upgrades_with_compatible_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for version in 1..CONFIG_VERSION {
        fs::write(&path, format!("version = {version}\n")).unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.theme, UiTheme::Midnight);
        assert!(!config.advanced.detailed_activity_clocks);
    }
}

#[test]
fn theme_preferences_upgrade_and_round_trip_without_replacing_other_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let old = "version = 4\nshow_stopped_sessions = false\n";
    fs::write(&path, old).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.theme, UiTheme::Midnight);
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(fs::read_to_string(&path).unwrap(), old);

    for theme in UiTheme::ALL {
        Config::update_to(&path, |config| {
            config.theme = theme;
            Ok(())
        })
        .unwrap();
        let mut expected = config.clone();
        expected.theme = theme;
        assert_eq!(Config::load_from(&path).unwrap(), expected);
    }
}

#[test]
fn legacy_dracula_theme_loads_and_saves_as_darcula() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\ntheme = \"dracula\"\n"),
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.theme, UiTheme::Darcula);
    assert_eq!(UiTheme::ALL.len(), 12);
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("theme = \"darcula\""), "{saved}");
    assert!(!saved.contains("dracula"), "{saved}");
}

#[test]
fn raw_ssh_permissions_are_required_and_podman_rejects_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = sample_config();
    let container = match config.targets.remove("podman-default").unwrap() {
        TargetTemplate::LocalPodman { container } => container,
        _ => unreachable!(),
    };
    let ssh = SshConnection {
        host: "builder".into(),
        user: None,
        identity_file: None,
        extra_args: Vec::new(),
    };
    config.targets = BTreeMap::from([
        (
            "builder-guardian".into(),
            TargetTemplate::SshBare {
                ssh: ssh.clone(),
                permissions: PermissionMode::Guardian,
                workspace_prefix: default_named_machine_prefix(),
            },
        ),
        (
            "builder-yolo".into(),
            TargetTemplate::SshBare {
                ssh: ssh.clone(),
                permissions: PermissionMode::Yolo,
                workspace_prefix: default_named_machine_prefix(),
            },
        ),
        (
            "builder-podman".into(),
            TargetTemplate::SshPodman { ssh, container },
        ),
    ]);

    config.save_to(&path).unwrap();

    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("permissions = \"guardian\""), "{body}");
    assert!(body.contains("permissions = \"yolo\""), "{body}");
    assert_eq!(body.matches("permissions = ").count(), 2, "{body}");
    // Saving names the host the three runtimes share, so the file that comes
    // back has the machine the in-memory config never spelled out.
    config.machines.insert(
        "builder".into(),
        Machine::Ssh {
            ssh: SshConnection {
                host: "builder".into(),
                user: None,
                identity_file: None,
                extra_args: Vec::new(),
            },
            workspace_prefix: default_named_machine_prefix(),
            build_cache: None,
        },
    );
    assert_eq!(body.matches("[machines.builder]").count(), 1, "{body}");
    assert_eq!(Config::load_from(&path).unwrap(), config);

    fs::write(
        &path,
        "version = 1\n[targets.builder]\nkind = \"ssh-bare\"\nhost = \"builder\"\n",
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(error.contains("permissions"), "{error}");

    fs::write(
        &path,
        "version = 1\n[targets.builder]\nkind = \"ssh-podman\"\nhost = \"builder\"\npermissions = \"guardian\"\nimage = \"example.invalid/agent:latest\"\n",
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(error.contains("only applies to a bare runtime"), "{error}");
}

#[test]
fn missing_config_uses_clean_v1_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config::load_from(&directory.path().join("missing.toml")).unwrap();
    assert_eq!(config, Config::default());
    assert!(config.phone.enabled);
    assert!(config.phone.tailscale_detect);
}

#[test]
fn omitted_phone_fields_enable_the_web_viewer_and_tailscale_detection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 1\n[phone]\nbind = \"127.0.0.1:4765\"\n").unwrap();

    let config = Config::load_from(&path).unwrap();

    assert!(config.phone.enabled);
    assert!(config.phone.tailscale_detect);
    assert_eq!(config.phone.bind, "127.0.0.1:4765");
}

#[test]
fn version_seven_profiles_upgrade_enabled_and_disabled_round_trips_explicitly() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "version = 7\n[profiles.work]\nkind = \"codex\"\nhome = \"/profiles/work\"\n",
    )
    .unwrap();

    let mut config = Config::load_from(&path).unwrap();
    assert_eq!(config.version, CONFIG_VERSION);
    assert!(config.profiles["work"].enabled);
    assert_eq!(
        config
            .enabled_profiles()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec!["work"]
    );

    config.save_to(&path).unwrap();
    let enabled = fs::read_to_string(&path).unwrap();
    assert!(
        enabled.starts_with(&format!("version = {CONFIG_VERSION}")),
        "{enabled}"
    );
    assert!(!enabled.contains("enabled = true"), "{enabled}");

    config.profiles.get_mut("work").unwrap().enabled = false;
    config.save_to(&path).unwrap();
    let disabled = fs::read_to_string(&path).unwrap();
    assert!(disabled.contains("enabled = false"), "{disabled}");
    assert!(!Config::load_from(&path).unwrap().profiles["work"].enabled);
}

/// The global `[subagents] enabled` switch was removed; whether a session
/// uses Mjolnir sub-agents is now stored per session. A config file left
/// over from before the removal must still load, and a save must drop the
/// key.
#[test]
fn a_legacy_subagents_enabled_key_still_loads_and_is_dropped_on_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\n[subagents]\nenabled = true\n"),
    )
    .unwrap();

    let mut config = Config::load_from(&path).unwrap();
    assert_eq!(config.subagents.max_concurrent, 6);
    assert!(config.subagents.eligible_profiles.is_empty());
    // profile_is_eligible no longer consults the deprecated flag at all.
    assert!(config.subagents.profile_is_eligible("work", "work"));

    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("enabled"), "{saved}");
    assert!(!saved.contains("[subagents]"), "{saved}");
    assert_eq!(
        Config::load_from(&path).unwrap().subagents.max_concurrent,
        6
    );

    // Other settings in the same section survive the same round trip.
    config.subagents.max_concurrent = 4;
    config.save_to(&path).unwrap();
    assert_eq!(
        Config::load_from(&path).unwrap().subagents.max_concurrent,
        4
    );
}

#[test]
fn version_eight_enables_parent_only_subagents_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "version = 8\n[profiles.work]\nkind = \"codex\"\nhome = \"/profiles/work\"\n",
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();

    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(config.subagents.max_concurrent, 6);
    assert!(config.subagents.eligible_profiles.is_empty());
    assert!(config.subagents.profile_is_eligible("work", "work"));
    assert!(!config.subagents.profile_is_eligible("work", "other"));
}

#[test]
fn legacy_global_cache_opt_out_is_saved_on_each_machine_without_other_edits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for version in [12, 13] {
        let source = format!(
            r#"# Keep my configuration notes.
version = {version}
[build_cache]
enabled = false
[machines.builder]
kind = "ssh"
host = "builder.example.com" # Keep this host note.
[machines.builder.build_cache]
enabled = true
max_total_size = "50GiB"
target_max_size = "10GB"
directory = "/cache"
[machines.other]
kind = "ssh"
host = "other.example.com"
[targets.podman]
kind = "podman"
[targets.remote]
kind = "docker"
machine = "builder"
"#
        );
        fs::write(&path, &source).unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            source,
            "load is read-only"
        );
        for id in ["local", "builder", "other"] {
            assert_eq!(
                config.machines[id].build_cache().unwrap().enabled,
                Some(false)
            );
        }
        let cache = config.machines["builder"].build_cache().unwrap();
        assert_eq!(cache.max_total_size.as_deref(), Some("50GiB"));
        assert_eq!(cache.directory.as_deref(), Some(Path::new("/cache")));
        for target in config.targets.values() {
            let (TargetTemplate::LocalPodman { container }
            | TargetTemplate::SshDocker { container, .. }) = target
            else {
                panic!("container runtime")
            };
            assert_eq!(container.build_cache.as_ref().unwrap().enabled, Some(false));
        }
        config.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("[build_cache]"), "{saved}");
        assert!(saved.contains("# Keep my configuration notes."), "{saved}");
        assert!(saved.contains("# Keep this host note."), "{saved}");
        assert_eq!(Config::load_from(&path).unwrap(), config, "{saved}");
        config.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    }
}

#[test]
fn cache_opt_out_migration_covers_implicit_local_and_all_older_config_versions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for version in 1..=13 {
        fs::write(
            &path,
            format!("version = {version}\n[build_cache]\nenabled = false\n"),
        )
        .unwrap();
        let mut config = Config::load_from(&path).unwrap();
        assert_eq!(
            config.machines["local"].build_cache().unwrap().enabled,
            Some(false)
        );
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
        // A newly added machine uses the normal default, not a hidden veto.
        config.machines.insert(
            "new".into(),
            serde_json::from_value(serde_json::json!({"kind":"ssh", "host":"new.example.com"}))
                .unwrap(),
        );
        config.save_to(&path).unwrap();
        assert!(
            Config::load_from(&path).unwrap().machines["new"]
                .build_cache()
                .is_none()
        );
    }
}

#[test]
fn enabled_or_absent_legacy_global_cache_keeps_machine_policy() {
    for legacy in ["", "[build_cache]\nenabled = true\n"] {
        let config: Config = toml::from_str(&format!(
            r#"version = 13
{legacy}
[machines.off]
kind = "ssh"
host = "off.example.com"
[machines.off.build_cache]
enabled = false
[machines.on]
kind = "ssh"
host = "on.example.com"
[machines.on.build_cache]
enabled = true
"#
        ))
        .unwrap();
        assert!(!config.machines.contains_key("local"));
        assert_eq!(
            config.machines["off"].build_cache().unwrap().enabled,
            Some(false)
        );
        assert_eq!(
            config.machines["on"].build_cache().unwrap().enabled,
            Some(true)
        );
        let saved = toml::to_string_pretty(&config).unwrap();
        assert!(!saved.contains("[build_cache]"));
        assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
    }
}

#[test]
fn legacy_global_cache_opt_out_is_applied_after_fused_target_conversion() {
    let source = format!("{VERSION_TEN_CONFIG}\n[build_cache]\nenabled = false\n");
    let config: Config = toml::from_str(&source).unwrap();
    for id in ["local", "builder.example.com"] {
        assert_eq!(
            config.machines[id].build_cache().unwrap().enabled,
            Some(false)
        );
    }
    assert_eq!(
        config.machines["local"]
            .build_cache()
            .unwrap()
            .max_total_size
            .as_deref(),
        Some("50GiB")
    );
    assert!(config.machines["aws"].build_cache().is_none());
    for target in config.targets.values() {
        match target {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::SshPodman { container, .. } => {
                assert_eq!(container.build_cache.as_ref().unwrap().enabled, Some(false));
            }
            _ => {}
        }
    }
    assert_eq!(
        toml::from_str::<Config>(&toml::to_string_pretty(&config).unwrap()).unwrap(),
        config
    );
}

#[test]
fn saving_can_reenable_one_machine_during_global_cache_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 13\n[build_cache]\nenabled = false\n").unwrap();
    let mut config = Config::load_from(&path).unwrap();
    config
        .machines
        .insert("local".into(), Machine::Local { build_cache: None });
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("build_cache"), "{saved}");
    assert!(
        !Config::load_from(&path)
            .unwrap()
            .machines
            .contains_key("local")
    );
}

#[test]
fn legacy_build_budgets_are_ignored_without_changing_host_placement() {
    for legacy in [
        serde_json::json!({"max_size": "500GiB", "target_max_size": "250GiB"}),
        serde_json::json!({"max_size": [false], "target_max_size": {"invalid": true}}),
    ] {
        let mut settings = legacy;
        settings["enabled"] = serde_json::json!(false);
        settings["directory"] = serde_json::json!("/cache");
        let cache: TargetBuildCache = serde_json::from_value(settings.clone()).unwrap();
        cache.validate("builder").unwrap();
        assert_eq!(cache.enabled, Some(false));
        assert_eq!(cache.directory.as_deref(), Some(Path::new("/cache")));
        assert_eq!(cache.max_total_size, None);
        let source = toml::to_string(&serde_json::json!({
            "version": CONFIG_VERSION,
            "machines": {"builder": {
                "kind": "ssh", "host": "builder.example.com", "build_cache": settings
            }}
        }))
        .unwrap();
        let config: Config = toml::from_str(&source).unwrap();
        assert_eq!(config.machines["builder"].build_cache(), Some(&cache));
        let serialized = serde_json::to_value(&cache).unwrap();
        assert!(serialized.get("max_size").is_none());
        assert!(serialized.get("target_max_size").is_none());
        settings["max_total_size"] = serde_json::json!("2TB");
        let reset: TargetBuildCache = serde_json::from_value(settings).unwrap();
        assert_eq!(reset.max_total_size.as_deref(), Some("2TB"));
    }
    assert!(
        serde_json::from_value::<TargetBuildCache>(serde_json::json!({"max_totl_size": "2TB"}))
            .is_err()
    );
}

// Hard-won: dcc60664: a disabled eligible profile stopped every mj command
#[test]
fn a_disabled_eligible_subagent_profile_loads_instead_of_failing() {
    // A profile that is both disabled and listed for sub-agent use must not
    // stop the daemon from starting. `mj doctor` warns about it, and the
    // consumers that offer profiles for delegation exclude it because it is
    // disabled (they filter on `enabled`).
    let config = toml::from_str::<Config>(&format!(
        "version = {CONFIG_VERSION}\n\
         [subagents.eligible_profiles]\nwork = true\n\
         [profiles.work]\nenabled = false\nkind = \"grok\"\nhome = \"/profiles/work\"\n"
    ))
    .unwrap();
    config.validate().unwrap();
    assert!(!config.profiles["work"].enabled);
}

#[test]
fn a_session_review_choice_is_stored_in_a_stable_shape() {
    // Stored in `sessions.review_json`; older and newer releases read it.
    let on = SessionReview::On {
        model: Some("gpt-6-astra".into()),
        effort: None,
        tier: None,
    };
    assert_eq!(
        serde_json::to_string(&on).unwrap(),
        r#"{"mode":"on","model":"gpt-6-astra"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionReview::Off).unwrap(),
        r#"{"mode":"off"}"#
    );
    assert_eq!(
        serde_json::from_str::<SessionReview>(r#"{"mode":"on"}"#).unwrap(),
        SessionReview::On {
            model: None,
            effort: None,
            tier: None,
        }
    );
    // Existing records may include a deprecated tier and remain readable.
    assert_eq!(
        serde_json::from_str::<SessionReview>(
            r#"{"mode":"on","model":"gpt-6-luna","effort":"max"}"#
        )
        .unwrap(),
        SessionReview::On {
            model: Some("gpt-6-luna".into()),
            effort: Some("max".into()),
            tier: None,
        }
    );
    let extended = SessionReview::On {
        model: None,
        effort: None,
        tier: Some("extended".into()),
    };
    assert_eq!(
        serde_json::to_string(&extended).unwrap(),
        r#"{"mode":"on"}"#
    );
    assert_eq!(
        serde_json::from_str::<SessionReview>(r#"{"mode":"on","tier":"extended"}"#).unwrap(),
        extended
    );
}

#[test]
fn deprecated_review_tier_is_accepted_but_omitted_from_serialization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "version = {CONFIG_VERSION}\n\n[profiles.reviewer]\nkind = \"claude\"\nhome = \"/profiles/reviewer\"\n\n[review]\nenabled = true\ntier = \"extended\"\nprofile = \"reviewer\"\n"
        ),
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.review.tier.as_deref(), Some("extended"));

    let serialized = toml::to_string_pretty(&config).unwrap();
    assert!(!serialized.contains("tier"), "{serialized}");
}

#[test]
fn explicit_web_viewer_opt_out_survives_serialization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = Config::default();
    config.phone.enabled = false;
    config.phone.tailscale_detect = false;

    config.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();

    assert!(body.contains("enabled = false"), "{body}");
    assert!(body.contains("tailscale_detect = false"), "{body}");
    assert_eq!(Config::load_from(&path).unwrap(), config);
}

#[test]
fn phone_config_requires_tls_off_loopback_and_complete_key_pairs() {
    let mut config = Config::default();
    config.phone.enabled = true;
    config.phone.bind = "0.0.0.0:3765".into();
    assert!(config.validate().unwrap_err().to_string().contains("TLS"));

    config.phone.tls_cert = Some(PathBuf::from("certificate.pem"));
    assert!(config.validate().unwrap_err().to_string().contains("both"));
    config.phone.tls_key = Some(PathBuf::from("private-key.pem"));
    config.validate().unwrap();
}

#[test]
fn empty_config_uses_clean_v1_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "\n\t").unwrap();
    assert_eq!(Config::load_from(&path).unwrap(), Config::default());
}

#[test]
fn a_newer_config_is_refused_without_touching_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let body = format!(
        "version = {}\nsetting_from_the_future = true\n",
        CONFIG_VERSION + 1
    );
    fs::write(&path, &body).unwrap();

    let error = Config::load_from(&path).unwrap_err().to_string();

    assert!(error.contains("newer Mjolnir"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), body);
}

#[test]
fn a_newer_config_written_after_load_still_blocks_a_save() {
    // Another Hel may upgrade the file between this build's load and its
    // save; the save must re-check the file rather than trust its marker.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let config = sample_config();
    config.save_to(&path).unwrap();

    let body = format!("version = {}\n", CONFIG_VERSION + 1);
    fs::write(&path, &body).unwrap();

    let error = config.save_to(&path).unwrap_err().to_string();
    assert!(error.contains("newer Mjolnir"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), body);
}

/// A hand-written config.toml without its `version` line stopped `mj` with
/// the raw TOML error "missing field `version`", which did not say what to
/// add (launch finding R14-4, reverify-14 tmux/026).
// Hard-won: 0870826d: R14-4 left hand-written config users with a generic missing-version error
#[test]
fn a_config_without_a_version_names_the_line_to_add() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "[profiles.codex]\nkind = \"codex\"\nhome = \"/nonexistent\"\n",
    )
    .unwrap();

    let error = Config::load_from(&path).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        format!(
            "{}: config.toml needs a `version = {CONFIG_VERSION}` line at the top (the current \
             configuration schema); see https://mjolnir.brokk.ai/configuration/",
            path.display()
        )
    );

    // Any other parse error keeps the parser's words after the file's path.
    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\n[profiles.codex\n"),
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(
        error.starts_with(&format!("parse Mjolnir config {}: ", path.display())),
        "{error}"
    );
    assert!(error.contains("TOML parse error"), "{error}");
}

#[test]
fn removed_profile_overrides_have_an_actionable_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "version = 1\n[profiles.codex]\nkind = \"codex\"\nhome = \"/tmp/codex\"\nmodel = \"gpt-old\"\n",
    )
    .unwrap();
    let error = Config::load_from(&path).unwrap_err().to_string();
    assert!(error.contains("`model` is no longer supported"));
    assert!(error.contains("/config"));
}

#[test]
fn profile_cannot_override_its_isolated_home() {
    let mut config = sample_config();
    config
        .profiles
        .get_mut("codex-1")
        .unwrap()
        .environment
        .insert("CODEX_HOME".into(), "/shared-and-racy".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must use `home`")
    );
}

// Hard-won: 02e00ba4: named instances collided on the default viewer port and API commands stopped
#[test]
fn named_instances_default_to_their_own_stable_viewer_port() {
    assert_eq!(default_phone_bind_for(None), "127.0.0.1:3765");
    let port = |name: &str| -> u16 {
        let bind: std::net::SocketAddr = default_phone_bind_for(Some(name)).parse().unwrap();
        assert!(bind.ip().is_loopback(), "{name} binds beyond loopback");
        bind.port()
    };
    let launch = port("launch-i1");
    assert_eq!(
        launch,
        port("launch-i1"),
        "the default must survive restarts"
    );
    assert!(
        (INSTANCE_VIEWER_PORTS).contains(&launch),
        "{launch} is outside the documented range"
    );
    assert_ne!(launch, 3765);
    assert_ne!(port("dev"), port("dev-2"));
}

#[test]
fn instance_names_accept_single_segment_identifiers() {
    for valid in ["dev", "dev-2", "x.y_z", "A1", "a".repeat(64).as_str()] {
        assert!(is_valid_instance_name(valid), "rejects valid {valid:?}");
    }
}

#[test]
fn instance_names_reject_empty_and_path_escapes() {
    for invalid in [
        "",
        "   ",
        ".",
        "..",
        "dev/dev",
        "../evil",
        "..\\evil",
        "has space",
        "semi;colon",
        "uniçode",
        "a".repeat(65).as_str(),
    ] {
        assert!(
            !is_valid_instance_name(invalid),
            "accepts invalid {invalid:?}"
        );
    }
}

#[test]
fn apply_instance_flag_rejects_bad_names_without_touching_the_environment() {
    // Validation runs before any environment mutation, so these cases
    // cannot leak state even though the environment is process-global.
    for invalid in ["", "../evil", "has space"] {
        let error = apply_instance_flag(Some(invalid)).unwrap_err();
        assert!(
            error.to_string().contains("invalid instance id"),
            "unexpected error for {invalid:?}: {error:#}"
        );
    }
}

// Hard-won: 4ce2f9a7: test data directory overrides could read or write the user session index
#[test]
fn an_overridden_data_directory_gets_its_own_session_index() {
    // Without the override the user's own index is the right one.
    assert_eq!(session_index_dir_for(None, None), None);
    let overridden = std::ffi::OsString::from("/tmp/lab/data");
    assert_eq!(
        session_index_dir_for(None, Some(overridden.as_os_str())),
        Some(PathBuf::from("/tmp/lab/data/sessionwiki")),
        "a daemon with its own data directory indexes into its own directory"
    );
    let chosen = std::ffi::OsString::from("/tmp/elsewhere");
    assert_eq!(
        session_index_dir_for(Some(chosen.as_os_str()), Some(overridden.as_os_str())),
        None,
        "an explicit choice is never overridden"
    );
}

// Hard-won: 18de1bb4: recovery could act on workers belonging to another instance
#[test]
fn instance_identity_prefers_a_valid_instance_name() {
    let dir = Path::new("/home/user/.local/share/mjolnir");
    assert_eq!(instance_identity_for(Some("qa0916"), dir), "qa0916");
    assert_eq!(
        instance_identity_for(Some("../escape"), dir),
        instance_identity_for(None, dir),
        "an invalid name falls back to the data-dir fingerprint"
    );
}

// Hard-won: 18de1bb4: recovery could act on workers belonging to another instance
#[test]
fn instance_identity_fingerprints_the_data_dir_stably() {
    let first = instance_identity_for(None, Path::new("/srv/mj/one"));
    let same = instance_identity_for(None, Path::new("/srv/mj/one"));
    let other = instance_identity_for(None, Path::new("/srv/mj/two"));
    assert_eq!(first, same);
    assert_ne!(first, other);
    assert_eq!(first.len(), 16);
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
}

#[test]
fn instance_directories_nest_under_instances_and_reject_escapes() {
    let base = PathBuf::from("/base/mjolnir");
    assert_eq!(
        with_instance_dir(base.clone(), Some("dev")),
        PathBuf::from("/base/mjolnir/instances/dev")
    );
    assert_eq!(with_instance_dir(base.clone(), None), base);
    // An invalid name never becomes a path segment, even if a future
    // caller skips startup validation: it falls back to the base directory.
    assert_eq!(with_instance_dir(base.clone(), Some("../evil")), base);
    assert_eq!(with_instance_dir(base.clone(), Some("")), base);
}

// Hard-won: f7272bb0: a development build migrated the live store and replaced its daemon
#[test]
fn development_builds_may_not_control_the_default_store() {
    let root = tempfile::tempdir().unwrap();
    let default_store = root.path().join("share/mjolnir");
    let named_store = default_store.join("instances/dev");
    fs::create_dir_all(&named_store).unwrap();
    let profile = root
        .path()
        .join("checkout/target/x86_64-unknown-linux-musl/release");
    fs::create_dir_all(profile.join(".fingerprint")).unwrap();
    fs::create_dir_all(profile.join("deps")).unwrap();
    let installed = root.path().join("cargo/bin/mj");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();

    for executable in [
        profile.join("mj"),
        // Linux names a rebuilt executable this way through /proc/self/exe.
        profile.join("mj (deleted)"),
        profile.join("deps/mj-0123456789abcdef"),
    ] {
        assert_eq!(
            development_build_controlling_default_store(
                &executable,
                &default_store,
                &default_store
            ),
            Some(profile.as_path()),
            "{}",
            executable.display()
        );
        // The same store reached through a different spelling is still the default.
        assert!(
            development_build_controlling_default_store(
                &executable,
                &default_store.join("instances/.."),
                &default_store
            )
            .is_some()
        );
        assert_eq!(
            development_build_controlling_default_store(&executable, &named_store, &default_store),
            None
        );
    }
    assert_eq!(
        development_build_controlling_default_store(&installed, &default_store, &default_store),
        None
    );
}

/// The version 10 file from the plan's acceptance step, and what saving it
/// writes back.
const VERSION_TEN_CONFIG: &str = r#"version = 10

[targets.localhost]
kind = "local-bare"

[targets.podman]
kind = "local-podman"
image = "example.invalid/agent:latest"

[targets.podman.build_cache]
max_total_size = "50GiB"

[targets.docker]
kind = "local-docker"
image = "example.invalid/agent:latest"

[targets.builder]
kind = "ssh-bare"
host = "builder.example.com"
permissions = "guardian"

[targets.builder-podman]
kind = "ssh-podman"
host = "builder.example.com"
image = "example.invalid/agent:latest"

[targets.aws]
kind = "aws-ec2"
region = "us-east-1"
launch_template = "lt-0123"
ssh_user = "ubuntu"
"#;

#[test]
fn a_version_ten_config_becomes_machines_and_runtimes_on_the_next_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, VERSION_TEN_CONFIG).unwrap();

    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(
        config.machines.keys().collect::<Vec<_>>(),
        ["aws", "builder.example.com", "local"]
    );
    let cache = TargetBuildCache {
        enabled: None,
        directory: None,
        max_total_size: Some("50GiB".into()),
        scheduler: Default::default(),
    };
    assert_eq!(
        config.machines["local"],
        Machine::Local {
            build_cache: Some(cache.clone())
        }
    );
    // Both local container runtimes now share the one host cache.
    for id in ["podman", "docker"] {
        let (TargetTemplate::LocalPodman { container } | TargetTemplate::LocalDocker { container }) =
            &config.targets[id]
        else {
            panic!("{id} changed kind")
        };
        assert_eq!(container.build_cache.as_ref(), Some(&cache));
    }
    assert_eq!(config.targets["localhost"], TargetTemplate::LocalBare);
    assert!(matches!(
        config.targets["builder"],
        TargetTemplate::SshBare {
            permissions: PermissionMode::Guardian,
            ..
        }
    ));
    assert!(matches!(
        config.targets["aws"],
        TargetTemplate::AwsEc2 { .. }
    ));

    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    println!("{saved}");
    assert!(
        saved.starts_with(&format!("version = {CONFIG_VERSION}")),
        "{saved}"
    );
    for expected in [
        "[machines.local]",
        "[machines.local.build_cache]",
        "max_total_size = \"50GiB\"",
        "[machines.\"builder.example.com\"]",
        "kind = \"ssh\"",
        "host = \"builder.example.com\"",
        "[machines.aws]",
        "kind = \"aws-ec2\"",
        "[targets.localhost]\nkind = \"bare\"\n",
        "[targets.podman]\nkind = \"podman\"\n",
        "machine = \"builder.example.com\"",
    ] {
        assert!(saved.contains(expected), "missing {expected:?} in {saved}");
    }
    assert!(!saved.contains("local-podman"), "{saved}");
    assert_eq!(saved.matches("build_cache").count(), 1, "{saved}");
    // A second load of the rewritten file is the same configuration.
    assert_eq!(Config::load_from(&path).unwrap(), config);
}

#[test]
fn the_current_version_refuses_the_old_fused_kinds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    // Version 11 is the last one that may still name a fused kind, because
    // that version belongs to the key-binding change rather than this split.
    fs::write(
        &path,
        "version = 11\n[targets.podman]\nkind = \"local-podman\"\nimage = \"a:1\"\n",
    )
    .unwrap();
    assert!(matches!(
        Config::load_from(&path).unwrap().targets["podman"],
        TargetTemplate::LocalPodman { .. }
    ));

    fs::write(
        &path,
        format!(
            "version = {CONFIG_VERSION}\n[targets.podman]\nkind = \"local-podman\"\nimage = \"a:1\"\n"
        ),
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(error.contains("\"podman\""), "{error}");
    assert!(error.contains("local-podman"), "{error}");
    assert!(error.contains("machine"), "{error}");
}

/// A single `build.rs` run compiles one channel, so the rule both channels
/// follow is checked directly here, and the baked value is checked against it.
#[test]
fn the_baked_default_image_agrees_with_the_channel_rule() {
    let release = env!("MJ_AGENT_DEV_IMAGE_RELEASE") == "1";
    assert_eq!(
        DEFAULT_CONTAINER_IMAGE,
        container_image_for(env!("CARGO_PKG_VERSION"), release)
    );
}

#[test]
fn a_release_pins_its_version_and_a_development_build_uses_latest() {
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        container_image_for(version, true),
        format!("{CONTAINER_IMAGE_REPOSITORY}:{version}")
    );
    assert_eq!(
        container_image_for(version, false),
        LEGACY_DEFAULT_CONTAINER_IMAGE
    );
}

fn container_target(image: &str) -> TargetTemplate {
    TargetTemplate::LocalDocker {
        container: ContainerTemplate {
            image: image.to_owned(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
            build_cache: None,
        },
    }
}

#[test]
fn migrating_the_legacy_default_image_rewrites_only_the_default() {
    let release_default = "ghcr.io/brokkai/mjolnir/agent-dev:9.9.9";
    let mut default = container_target(LEGACY_DEFAULT_CONTAINER_IMAGE);
    migrate_legacy_default_image(&mut default, release_default);
    assert_eq!(default.container().unwrap().image, release_default);

    let mut custom = container_target("example.invalid/agent:1");
    migrate_legacy_default_image(&mut custom, release_default);
    assert_eq!(custom.container().unwrap().image, "example.invalid/agent:1");
}

/// A file written before the running build's default image existed must follow
/// that build instead of pinning the image that was current when it was written.
#[test]
fn a_version_14_file_follows_the_running_builds_default_image() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "version = 14\n\
             [targets.podman]\nkind = \"podman\"\nimage = \"{LEGACY_DEFAULT_CONTAINER_IMAGE}\"\n\
             [targets.custom]\nkind = \"docker\"\nimage = \"example.invalid/agent:1\"\n"
        ),
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(
        config.targets["podman"].container().unwrap().image,
        DEFAULT_CONTAINER_IMAGE
    );
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains(&format!("version = {CONFIG_VERSION}")),
        "{saved}"
    );
    // The default is implicit now; only the customized image stays pinned.
    assert!(
        !saved.contains(&format!("image = \"{LEGACY_DEFAULT_CONTAINER_IMAGE}\"")),
        "{saved}"
    );
    assert!(saved.contains("example.invalid/agent:1"), "{saved}");
}
