use super::*;

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
        environment,
        context_window_bytes: None,
        guardian_review_model: None,
    }
}

#[test]
fn an_api_key_codex_profile_needs_its_key_in_the_profile_environment() {
    let home = tempfile::tempdir().expect("temporary home");
    let without_key = zai_profile(home.path(), BTreeMap::new());
    let error = without_key
        .validate("glm")
        .expect_err("a missing key is a configuration error")
        .to_string();
    assert!(error.contains("ZAI_API_KEY"), "{error}");
    assert!(error.contains("glm"), "{error}");

    let with_key = zai_profile(
        home.path(),
        [("ZAI_API_KEY".to_owned(), "secret".to_owned())]
            .into_iter()
            .collect(),
    );
    with_key
        .validate("glm")
        .expect("a configured key validates");
    assert_eq!(
        with_key.auth_scheme(),
        AuthScheme::ApiKey {
            env_key: "ZAI_API_KEY".to_owned()
        }
    );
    assert_eq!(
        with_key.authentication_marker(),
        home.path().join("config.toml"),
        "the Codex configuration proves an API-key profile is set up"
    );
    assert_eq!(with_key.credential_freshness(b"{}"), None);
    assert_eq!(with_key.credential_expiry(b"{}"), None);
}

#[test]
fn a_codex_profile_may_not_supply_its_own_model_catalog() {
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
    let error = profile
        .validate("glm")
        .expect_err("Mjolnir owns the catalog")
        .to_string();
    assert!(error.contains("model_catalog_json"), "{error}");
}

#[test]
fn guardian_review_model_accepts_its_three_forms_only_on_a_custom_provider() {
    let home = tempfile::tempdir().expect("temporary home");
    let mut profile = zai_profile(
        home.path(),
        [("ZAI_API_KEY".to_owned(), "secret".to_owned())]
            .into_iter()
            .collect(),
    );
    for accepted in [
        GUARDIAN_REVIEW_NEWEST_FLASH,
        GUARDIAN_REVIEW_SESSION,
        "glm-5.3",
    ] {
        profile.guardian_review_model = Some(accepted.to_owned());
        profile
            .validate("glm")
            .unwrap_or_else(|error| panic!("{accepted} should validate: {error}"));
    }

    profile.guardian_review_model = Some("   ".to_owned());
    let error = profile
        .validate("glm")
        .expect_err("a blank reviewer names no model")
        .to_string();
    assert!(error.contains("guardian_review_model"), "{error}");

    // A native Codex profile has no Mjolnir-generated catalog to pick a
    // reviewer from, so the setting would silently do nothing.
    let native = tempfile::tempdir().expect("temporary home");
    fs::write(native.path().join("config.toml"), "model = \"gpt-5.5\"\n").expect("write");
    let native_profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: native.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: Some(GUARDIAN_REVIEW_SESSION.to_owned()),
    };
    let error = native_profile
        .validate("work")
        .expect_err("no custom provider means no generated catalog")
        .to_string();
    assert!(error.contains("guardian_review_model"), "{error}");
    assert!(error.contains("work"), "{error}");
}

#[test]
fn a_codex_profile_with_no_home_yet_reports_a_native_login() {
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: PathBuf::from("/does/not/exist"),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    assert_eq!(profile.auth_scheme(), AuthScheme::NativeLogin);
    assert_eq!(
        profile.authentication_marker(),
        PathBuf::from("/does/not/exist/auth.json")
    );
    profile
        .validate("fresh")
        .expect("discovery creates profiles before their homes exist");
}

#[test]
fn local_targets_need_no_setup_and_preserve_explicit_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let raw = Config::load_from(&path).unwrap();
    assert!(raw.targets.is_empty());
    let available = raw.with_local_targets();
    assert!(matches!(
        available.targets["docker"],
        TargetTemplate::LocalDocker { .. }
    ));
    assert!(matches!(
        available.targets["podman"],
        TargetTemplate::LocalPodman { .. }
    ));
    assert!(
        !path.exists(),
        "runtime defaults must not write configuration"
    );
    let mut custom = Config::default();
    custom
        .targets
        .insert("docker".into(), TargetTemplate::LocalBare);
    let resolved = custom.with_local_targets();
    assert_eq!(resolved.targets["docker"], TargetTemplate::LocalBare);
    assert_eq!(resolved.clone().with_local_targets(), resolved);
}

/// Saving edits the user's file in place: comments, blank lines, and the
/// order of sections and keys survive a save that changes one value.
/// Launch campaign finding C-21.
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
fn stopped_session_visibility_defaults_off_and_uses_the_advanced_section() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let legacy = "version = 6\nshow_stopped_sessions = true\n";
    fs::write(&path, legacy).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert!(config.show_stopped_sessions);
    assert!(!config.advanced.show_stopped_sessions);
    assert_eq!(fs::read_to_string(&path).unwrap(), legacy);

    config.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(!body.contains("show_stopped_sessions"));

    let (saved, ()) = Config::update_to(&path, |config| {
        config.advanced.show_stopped_sessions = true;
        Ok(())
    })
    .unwrap();
    assert_eq!(Config::load_from(&path).unwrap(), saved);
    assert!(saved.advanced.show_stopped_sessions);
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("[advanced]"));
    assert!(body.contains("show_stopped_sessions = true"));
    assert_eq!(saved.version, CONFIG_VERSION);
}

#[test]
fn muse_home_mapping_keeps_config_credentials_and_session_data_together() {
    let home = Path::new("/private/session/muse");
    let mut environment = BTreeMap::from([("XDG_DATA_HOME".into(), "/unrelated".into())]);
    HarnessKind::Muse.configure_home_environment(home, HarnessHost::Other, &mut environment);
    assert_eq!(environment["XDG_CONFIG_HOME"], "/private/session");
    assert_eq!(environment["XDG_DATA_HOME"], "/private/session/muse/.data");
    assert_eq!(
        HarnessKind::Muse.home_from_environment(&environment["XDG_CONFIG_HOME"]),
        home
    );
    assert_eq!(
        harness_authentication_marker(HarnessKind::Muse, home),
        home.join("auth.json")
    );
}

#[test]
fn codex_target_policy_selects_mode_without_replacing_host_config() {
    for (policy, mode) in [
        (ExecutionPolicy::ConfiguredApprovals, "agent"),
        (ExecutionPolicy::Unconstrained, "agent-full-access"),
    ] {
        let config = r#"{"default_permissions":"project","model":"configured-model"}"#;
        let mut environment = BTreeMap::from([("CODEX_CONFIG".into(), config.into())]);
        HarnessKind::Codex
            .configure_execution_environment(policy, &mut environment)
            .unwrap();
        assert_eq!(environment["INITIAL_AGENT_MODE"], mode);
        assert_eq!(environment["CODEX_CONFIG"], config);
    }
}

/// The launch argument joins an argv the user already set, and repeated
/// enforcement never appends it twice.
#[test]
fn muse_unconstrained_launch_keeps_one_disable_sandbox_argument() {
    let mut environment = BTreeMap::from([(
        "MUSE_SERVE_ARGS".into(),
        "--sandbox-network restricted".into(),
    )]);
    for _ in 0..2 {
        HarnessKind::Muse
            .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut environment)
            .unwrap();
    }
    assert_eq!(environment["MUSE_APPROVAL_MODE"], "allowAll");
    assert_eq!(
        environment["MUSE_SERVE_ARGS"],
        "--sandbox-network restricted --disable-sandbox"
    );

    let mut carried = BTreeMap::from([("MUSE_SERVE_ARGS".into(), "--disable-sandbox".into())]);
    HarnessKind::Muse
        .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut carried)
        .unwrap();
    assert_eq!(carried["MUSE_SERVE_ARGS"], "--disable-sandbox");
}

#[test]
fn muse_runs_unconstrained_on_every_target_and_other_harnesses_keep_the_target_policy() {
    for kind in HarnessKind::ALL {
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            let expected = if kind == HarnessKind::Muse {
                ExecutionPolicy::Unconstrained
            } else {
                policy
            };
            assert_eq!(
                kind.effective_execution_policy(policy),
                expected,
                "{kind:?} {policy:?}"
            );
        }
    }
}

#[test]
fn only_unconstrained_claude_turns_off_the_session_sandbox() {
    for kind in HarnessKind::ALL {
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            let sandbox = kind
                .execution_enforcement(policy)
                .and_then(ExecutionEnforcement::session_sandbox);
            let expected =
                (kind == HarnessKind::Claude && policy.is_unconstrained()).then_some(false);
            assert_eq!(sandbox, expected, "{kind:?} {policy:?}");
        }
    }
}

fn muse_staged_setting() -> StagedSetting {
    HarnessKind::Muse
        .execution_enforcement(ExecutionPolicy::Unconstrained)
        .and_then(ExecutionEnforcement::staged_setting)
        .expect("Muse stages its permission profile")
}

#[test]
fn a_staged_setting_keeps_the_rest_of_the_document() {
    let mut document = serde_json::json!({
        "schema_version": 1,
        "provider": "anthropic",
        "permissions": {"schema_version": 2, "default_profile": ":auto-review"}
    });

    muse_staged_setting()
        .apply(document.as_object_mut().unwrap())
        .unwrap();

    assert_eq!(document["provider"], "anthropic");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["permissions"]["schema_version"], 2);
    assert_eq!(document["permissions"]["default_profile"], ":unrestricted");
}

#[test]
fn a_staged_setting_creates_the_objects_and_versions_it_needs() {
    let mut root = serde_json::Map::new();

    muse_staged_setting().apply(&mut root).unwrap();

    assert_eq!(
        serde_json::Value::Object(root),
        serde_json::json!({
            "schema_version": 1,
            "permissions": {"schema_version": 1, "default_profile": ":unrestricted"}
        })
    );
}

#[test]
fn a_staged_setting_reports_a_traversed_value_that_is_not_an_object() {
    let mut document = serde_json::json!({"permissions": []});

    let error = muse_staged_setting()
        .apply(document.as_object_mut().unwrap())
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("permissions must be a JSON object"),
        "error should name the key: {error:#}"
    );
}

fn sample_config() -> Config {
    Config {
        version: CONFIG_VERSION,
        keys: Default::default(),
        sessions_side: Default::default(),
        advanced: Default::default(),
        notify: Default::default(),
        show_stopped_sessions: false,
        spinner: SpinnerStyle::default(),
        theme: Default::default(),
        phone: PhoneConfig::default(),
        continuation: Default::default(),
        review: ReviewConfig::default(),
        sessionwiki: SessionWikiConfig::default(),
        subagents: SubagentConfig::default(),
        build_cache: BuildCacheConfig::default(),
        jev: Default::default(),
        legacy_startup: (),
        machines: BTreeMap::new(),
        profiles: BTreeMap::from([(
            "codex-1".into(),
            HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/test/.codex-one"),
                environment: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
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
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        )]),
    }
}

#[test]
fn harness_profiles_reject_the_removed_executable_override() {
    let error = toml::from_str::<HarnessProfile>(
        "kind = \"codex\"\nhome = \"/profiles/codex\"\nexecutable = \"/opt/codex-acp\"\n",
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field `executable`"));
}

#[test]
fn claude_takes_no_home_variable_on_macos_and_keeps_one_elsewhere() {
    let home = Path::new("/private/session/profile");

    let mut mac = BTreeMap::new();
    HarnessKind::Claude.configure_home_environment(home, HarnessHost::MacOs, &mut mac);
    assert!(
        mac.is_empty(),
        "CLAUDE_CONFIG_DIR scopes nothing on macOS, so nothing may be set: {mac:?}"
    );

    let mut linux = BTreeMap::new();
    HarnessKind::Claude.configure_home_environment(home, HarnessHost::Other, &mut linux);
    assert_eq!(linux["CLAUDE_CONFIG_DIR"], home.to_string_lossy());
}

#[test]
fn every_harness_but_claude_scopes_its_home_on_macos() {
    for kind in HarnessKind::ALL {
        let mut environment = BTreeMap::new();
        let home = Path::new("/private/session/muse");
        kind.configure_home_environment(home, HarnessHost::MacOs, &mut environment);
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
    assert_eq!(muse.acp_mode(), Some("allowAll"));
    assert_eq!(muse.label(), "allowAll / sandbox-off / :unrestricted");
    assert_eq!(
        muse.launch_environment(),
        Some(("MUSE_APPROVAL_MODE", "allowAll"))
    );
    assert_eq!(
        muse.launch_argument(),
        Some(("MUSE_SERVE_ARGS", "--disable-sandbox"))
    );
    assert_eq!(
        muse.staged_setting().map(|setting| setting.value),
        Some(":unrestricted")
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

    for kind in [HarnessKind::Kimi, HarnessKind::Grok] {
        assert_eq!(
            kind.execution_enforcement(ExecutionPolicy::ConfiguredApprovals),
            None,
            "{kind:?}"
        );
    }
}

#[test]
fn harness_names_and_ids_round_trip() {
    for kind in HarnessKind::ALL {
        assert_eq!(kind.id().parse::<HarnessKind>().unwrap(), kind);
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::Value::String(kind.id().to_owned())
        );
        assert!(!kind.display_name().is_empty());
        assert!(kind.default_home_leaf().starts_with('.'));
    }
    assert_eq!(HarnessKind::Grok.id(), "grok");
    assert_eq!(HarnessKind::Grok.display_name(), "Grok Build");
    assert_eq!(HarnessKind::Grok.default_home_leaf(), ".grok");
    assert!("nope".parse::<HarnessKind>().is_err());
    // The DSH harness was removed; a stored `deepseek` value must be
    // rejected rather than silently resolving to another harness.
    assert!("deepseek".parse::<HarnessKind>().is_err());
    assert!(
        serde_json::from_value::<HarnessKind>(serde_json::Value::String("deepseek".into()))
            .is_err()
    );
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
fn only_unconstrained_grok_carries_the_blanket_approval_flag() {
    assert_eq!(
        HarnessKind::Grok.launch_flag_for(ExecutionPolicy::ConfiguredApprovals),
        None
    );
    assert_eq!(
        HarnessKind::Grok.launch_flag_for(ExecutionPolicy::Unconstrained),
        Some("--always-approve")
    );
    for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Kimi] {
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            assert_eq!(kind.launch_flag_for(policy), None, "{kind:?}");
        }
    }
}

#[test]
fn guardian_support_is_declared_per_harness() {
    for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Grok] {
        assert!(kind.supports_guardian_approvals(), "{kind:?}");
    }
    for kind in [HarnessKind::Kimi, HarnessKind::Muse] {
        assert!(!kind.supports_guardian_approvals(), "{kind:?}");
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
        repository.local = Some(PathBuf::from("/home/test/src/app"));
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
        tier: crate::review::lanes::ReviewTier::Extended,
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
fn detailed_activity_clocks_default_off_and_round_trip_without_breaking_old_configs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 2\n").unwrap();
    let old = Config::load_from(&path).unwrap();
    assert!(!old.advanced.detailed_activity_clocks);

    let mut config = old;
    config.advanced.detailed_activity_clocks = true;
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("[advanced]"));
    assert!(saved.contains("detailed_activity_clocks = true"));
    assert!(
        Config::load_from(&path)
            .unwrap()
            .advanced
            .detailed_activity_clocks
    );
}

#[test]
fn every_previous_config_version_upgrades_with_compatible_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for version in 1..CONFIG_VERSION {
        fs::write(&path, format!("version = {version}\n")).unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(!config.show_stopped_sessions);
        assert_eq!(config.theme, UiTheme::Midnight);
        assert!(!config.advanced.detailed_activity_clocks);
        assert!(!config.advanced.show_stopped_sessions);
    }
}

#[test]
fn spinner_preferences_round_trip_without_replacing_other_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = sample_config();
    config.phone.enabled = false;
    config.save_to(&path).unwrap();

    for spinner in SpinnerStyle::ALL {
        Config::update_to(&path, |config| {
            config.spinner = spinner;
            Ok(())
        })
        .unwrap();
        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.spinner, spinner);
        assert_eq!(reloaded.phone, config.phone);
        assert_eq!(reloaded.profiles, config.profiles);
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
    assert_eq!(UiTheme::ALL.len(), 5);
    config.save_to(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("theme = \"darcula\""), "{saved}");
    assert!(!saved.contains("dracula"), "{saved}");
}

#[test]
fn unknown_theme_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\ntheme = \"unknown\"\n"),
    )
    .unwrap();
    assert!(Config::load_from(&path).is_err());
}

#[test]
fn explicit_container_layer_and_host_helper_storage_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = sample_config();
    if let TargetTemplate::LocalPodman { container } =
        config.targets.get_mut("podman-default").unwrap()
    {
        container.workspace_storage = PodmanWorkspaceStorage::HostHelper {
            root: PathBuf::from("/srv/mj-workspaces"),
            helper: vec!["sudo".into(), "-n".into(), "/opt/mj-helper".into()],
        };
    }
    config.save_to(&path).unwrap();
    assert_eq!(Config::load_from(&path).unwrap(), config);

    if let TargetTemplate::LocalPodman { container } =
        config.targets.get_mut("podman-default").unwrap()
    {
        container.workspace_storage = PodmanWorkspaceStorage::ContainerLayer;
    }
    config.save_to(&path).unwrap();
    assert_eq!(Config::load_from(&path).unwrap(), config);
}

#[test]
fn local_docker_target_round_trips_with_its_public_kind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = sample_config();
    let container = match config.targets.remove("podman-default").unwrap() {
        TargetTemplate::LocalPodman { container } => container,
        _ => unreachable!(),
    };
    config
        .targets
        .insert("docker".into(), TargetTemplate::LocalDocker { container });

    config.save_to(&path).unwrap();

    let rendered = fs::read_to_string(&path).unwrap();
    assert!(rendered.contains("kind = \"docker\""), "{rendered}");
    assert_eq!(Config::load_from(&path).unwrap(), config);
}

#[test]
fn setup_can_add_an_alternative_to_a_maximum_length_target_name() {
    let id = "x".repeat(64);
    let mut original = Config::default();
    original
        .targets
        .insert(id.clone(), TargetTemplate::LocalBare);
    let mut discovered = Config::default();
    discovered
        .targets
        .insert(id, sample_config().targets["podman-default"].clone());
    let additions = original.setup_additions(&discovered);
    additions.validate().unwrap();
    assert_eq!(additions.targets.len(), 1);
    original.targets.extend(additions.targets);
    assert_eq!(original.targets.len(), 2);
    assert!(original.setup_additions(&discovered).targets.is_empty());
}

#[test]
fn explicit_image_pull_policy_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = sample_config();
    let TargetTemplate::LocalPodman { container } =
        config.targets.get_mut("podman-default").unwrap()
    else {
        unreachable!()
    };
    container.pull_policy = ImagePullPolicy::Never;

    config.save_to(&path).unwrap();

    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("pull_policy = \"never\"")
    );
    assert_eq!(Config::load_from(&path).unwrap(), config);
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

#[test]
fn review_rejects_disabled_profile_references() {
    let profile =
        "[profiles.work]\nenabled = false\nkind = \"claude\"\nhome = \"/profiles/work\"\n";
    let reference = "[review]\nprofile = \"work\"\n";
    let error =
        toml::from_str::<Config>(&format!("version = {CONFIG_VERSION}\n{reference}{profile}"))
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
    assert!(error.contains("disabled"), "{error}");
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
    assert!(config.subagents.enabled);
    assert_eq!(config.subagents.max_concurrent, 6);
    assert!(config.subagents.eligible_profiles.is_empty());
    assert!(config.subagents.profile_is_eligible("work", "work"));
    assert!(!config.subagents.profile_is_eligible("work", "other"));
}

#[test]
fn the_jev_switch_defaults_on_round_trips_and_stops_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, format!("version = {CONFIG_VERSION}\n")).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert!(config.jev.enabled);
    assert!(config.automatic_continuation_enabled());

    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\n[jev]\nenabled = false\n"),
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert!(!config.jev.enabled);
    assert!(config.continuation.enabled);
    assert!(!config.automatic_continuation_enabled());
    config.save_to(&path).unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("[jev]\nenabled = false")
    );
    assert!(!Config::load_from(&path).unwrap().jev.enabled);
}

#[test]
fn build_cache_sizes_accept_the_spellings_mbx_accepts() {
    for (text, bytes) in [
        ("100", 100),
        ("100B", 100),
        ("20GB", 20_000_000_000),
        ("20GiB", 20 * 1024 * 1024 * 1024),
        ("1TiB", 1024_u64.pow(4)),
        (" 4MiB ", 4 * 1024 * 1024),
    ] {
        assert_eq!(parse_build_cache_size(text), Some(bytes), "{text}");
    }
    for text in ["", "GiB", "-1", "20gib", "20 gigabytes", "1.5GiB"] {
        assert_eq!(parse_build_cache_size(text), None, "{text}");
    }
}

#[test]
fn build_cache_sizes_convert_to_and_from_whole_gigabytes() {
    for (text, gigabytes) in [
        ("20GB", 20),
        ("100GiB", 107),
        ("500GiB", 537),
        ("25000000000B", 25),
        ("400MB", 0),
        ("600MB", 1),
    ] {
        assert_eq!(build_cache_size_gigabytes(text), Some(gigabytes), "{text}");
    }
    assert_eq!(build_cache_size_gigabytes("20 gigabytes"), None);
    assert_eq!(build_cache_size_from_gigabytes(25), "25GB");
    assert_eq!(
        build_cache_size_gigabytes(&build_cache_size_from_gigabytes(7)),
        Some(7)
    );
}

#[test]
fn subagents_reject_invalid_limits_and_unavailable_profiles() {
    let profile = "[profiles.work]\nenabled = false\nkind = \"grok\"\nhome = \"/profiles/work\"\n";
    for section in [
        "[subagents]\nmax_concurrent = 0\n",
        "[subagents.eligible_profiles]\nmissing = true\n",
    ] {
        let error =
            toml::from_str::<Config>(&format!("version = {CONFIG_VERSION}\n{section}{profile}"))
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("max_concurrent") || error.contains("not defined"),
            "{error}"
        );
    }
}

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

/// A profile that exists, so a `[review]` section has something to name.
fn config_with_profile(profile: &str) -> String {
    format!("version = 1\n\n[profiles.{profile}]\nkind = \"claude\"\nhome = \"/home/u/.claude\"\n")
}

#[test]
fn review_is_off_and_quick_until_the_config_says_otherwise() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, config_with_profile("reviewer")).unwrap();

    let config = Config::load_from(&path).unwrap();

    assert!(!config.review.enabled, "review is opt-in");
    assert_eq!(config.review.tier, crate::review::lanes::ReviewTier::Quick);
    assert_eq!(config.review.reviewer_profile(), None);
}

#[test]
fn a_review_section_names_the_profile_that_reviews() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "{}\n[review]\nenabled = true\ntier = \"extended\"\nprofile = \"reviewer\"\nmodel = \"opus\"\n",
            config_with_profile("reviewer")
        ),
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();

    assert!(config.review.enabled);
    assert_eq!(
        config.review.tier,
        crate::review::lanes::ReviewTier::Extended
    );
    assert_eq!(config.review.reviewer_profile(), Some("reviewer"));
    assert_eq!(config.review.model.as_deref(), Some("opus"));
    assert_eq!(config.review.effort, None);
}

#[test]
fn auto_review_can_be_enabled_without_an_explicit_profile() {
    let config = Config {
        continuation: Default::default(),
        review: ReviewConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    config.validate().expect("Auto resolves at review start");
}

#[test]
fn auto_rejects_manual_model_overrides() {
    let config = Config {
        continuation: Default::default(),
        review: ReviewConfig {
            model: Some("custom".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("name a profile")
    );
}

#[test]
fn a_review_profile_that_names_nothing_is_refused() {
    let config = Config {
        continuation: Default::default(),
        review: ReviewConfig {
            profile: Some("missing".into()),
            ..ReviewConfig::default()
        },
        ..Config::default()
    };
    let error = config
        .validate()
        .expect_err("a reviewer must be a profile in this file");
    assert!(
        format!("{error:#}").contains("not a profile defined in this config"),
        "unexpected error: {error:#}"
    );
}

/// A one-off `/review` needs a reviewer without automatic review, so a
/// profile with `enabled = false` is a valid configuration.
#[test]
fn a_reviewer_without_automatic_review_is_valid() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "{}\n[review]\nprofile = \"reviewer\"\n",
            config_with_profile("reviewer")
        ),
    )
    .unwrap();

    let config = Config::load_from(&path).unwrap();
    assert!(!config.review.enabled);
    assert_eq!(config.review.reviewer_profile(), Some("reviewer"));
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

#[test]
fn an_older_config_version_is_still_rejected() {
    // Hel has no downgrade migration, so an unrecognized older schema
    // keeps reporting an error rather than guessing.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 0\n").unwrap();

    let error = Config::load_from(&path).unwrap_err().to_string();
    assert!(
        error.contains("unsupported Mjolnir config version 0"),
        "{error}"
    );
}

#[test]
fn a_malformed_newer_config_is_still_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "version = 2\nthis is not toml\n").unwrap();

    let error = Config::load_from(&path).unwrap_err().to_string();
    assert!(error.contains("parse Mjolnir config"), "{error}");
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

#[test]
fn container_size_hosts_group_local_runtimes_and_exact_ssh_hosts() {
    let container = ContainerTemplate {
        build_cache: None,
        image: "agent:latest".into(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: BTreeMap::new(),
        workspace_storage: Default::default(),
    };
    let podman = TargetTemplate::LocalPodman {
        container: container.clone(),
    };
    let apple = TargetTemplate::AppleContainer {
        container: container.clone(),
    };
    let ssh = TargetTemplate::SshPodman {
        ssh: SshConnection {
            host: "builder.example.test".into(),
            user: Some("dev".into()),
            identity_file: None,
            extra_args: Vec::new(),
        },
        container,
    };

    assert_eq!(container_size_host(&podman), Some("local"));
    assert_eq!(container_size_host(&apple), Some("local"));
    assert_eq!(container_size_host(&ssh), Some("builder.example.test"));
    assert_eq!(container_size_host(&TargetTemplate::LocalBare), None);
}
#[test]
fn ssh_docker_target_round_trips_and_rejects_podman_storage() {
    let text = r#"kind = "ssh-docker"
host = "builder"
user = "ubuntu"
image = "ubuntu:24.04"
"#;
    let target: TargetTemplate = toml::from_str(text).unwrap();
    target.validate("remote-docker").unwrap();
    assert_eq!(
        toml::from_str::<TargetTemplate>(&toml::to_string(&target).unwrap()).unwrap(),
        target
    );
    assert_eq!(container_size_host(&target), Some("builder"));
    let TargetTemplate::SshDocker { ssh, mut container } = target else {
        panic!("wrong kind")
    };
    container.workspace_storage = PodmanWorkspaceStorage::ContainerLayer;
    assert!(
        TargetTemplate::SshDocker { ssh, container }
            .validate("remote-docker")
            .unwrap_err()
            .to_string()
            .contains("only supported by Podman")
    );
}

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

fn full_container(build_cache: Option<TargetBuildCache>) -> ContainerTemplate {
    ContainerTemplate {
        image: "example.invalid/agent:latest".into(),
        pull_policy: ImagePullPolicy::Newer,
        platform: Some("linux/amd64".into()),
        cpus: Some("4".into()),
        memory: Some("8g".into()),
        environment: BTreeMap::from([("RUST_LOG".into(), "debug".into())]),
        workspace_storage: PodmanWorkspaceStorage::PodmanVolume,
        build_cache,
    }
}

fn every_kind_config() -> Config {
    let local_cache = TargetBuildCache {
        enabled: Some(true),
        directory: Some(PathBuf::from("/var/cache/mbx")),
        max_size: Some("50GiB".into()),
    };
    let builder_cache = TargetBuildCache {
        enabled: Some(false),
        directory: Some(PathBuf::from("/srv/cache/mbx")),
        max_size: Some("20GB".into()),
    };
    let ssh = SshConnection {
        host: "builder.example.com".into(),
        user: Some("dev".into()),
        identity_file: Some(PathBuf::from("/keys/builder")),
        extra_args: vec!["-p".into(), "2222".into()],
    };
    let aws = |ssh_args: Vec<String>| TargetTemplate::AwsEc2 {
        aws_profile: Some("work".into()),
        region: "us-east-1".into(),
        launch_template: "lt-0123".into(),
        launch_template_version: Some("7".into()),
        ssh_user: "ubuntu".into(),
        address_source: AwsAddressSource::PrivateIp,
        identity_file: Some(PathBuf::from("/keys/fleet")),
        ssh_args,
    };
    let TargetTemplate::AwsEc2 {
        aws_profile,
        region,
        launch_template,
        launch_template_version,
        ssh_user,
        address_source,
        identity_file,
        ssh_args,
    } = aws(vec!["-o".into(), "StrictHostKeyChecking=no".into()])
    else {
        unreachable!()
    };
    let mut podman_storage = full_container(Some(local_cache.clone()));
    podman_storage.workspace_storage = PodmanWorkspaceStorage::HostHelper {
        root: PathBuf::from("/srv/mj-workspaces"),
        helper: vec!["sudo".into(), "-n".into(), "/opt/mj-helper".into()],
    };
    Config {
        machines: BTreeMap::from([
            (
                "local".into(),
                Machine::Local {
                    build_cache: Some(local_cache.clone()),
                },
            ),
            (
                "builder".into(),
                Machine::Ssh {
                    ssh: ssh.clone(),
                    workspace_prefix: PathBuf::from("work/spaces"),
                    build_cache: Some(builder_cache.clone()),
                },
            ),
            (
                "fleet".into(),
                Machine::AwsEc2 {
                    aws_profile,
                    region,
                    launch_template,
                    launch_template_version,
                    ssh_user,
                    address_source,
                    identity_file,
                    ssh_args,
                },
            ),
        ]),
        targets: BTreeMap::from([
            ("localhost".into(), TargetTemplate::LocalBare),
            (
                "podman".into(),
                TargetTemplate::LocalPodman {
                    container: podman_storage,
                },
            ),
            (
                "docker".into(),
                TargetTemplate::LocalDocker {
                    container: full_container(Some(local_cache.clone())),
                },
            ),
            (
                "apple".into(),
                TargetTemplate::AppleContainer {
                    container: full_container(Some(local_cache)),
                },
            ),
            (
                "builder-bare".into(),
                TargetTemplate::SshBare {
                    ssh: ssh.clone(),
                    permissions: PermissionMode::Yolo,
                    workspace_prefix: PathBuf::from("work/spaces"),
                },
            ),
            (
                "builder-podman".into(),
                TargetTemplate::SshPodman {
                    ssh: ssh.clone(),
                    container: full_container(Some(builder_cache.clone())),
                },
            ),
            (
                "builder-docker".into(),
                TargetTemplate::SshDocker {
                    ssh,
                    container: full_container(Some(builder_cache)),
                },
            ),
            (
                "fleet-bare".into(),
                aws(vec!["-o".into(), "StrictHostKeyChecking=no".into()]),
            ),
        ]),
        ..sample_config()
    }
}

#[test]
fn every_machine_and_runtime_kind_survives_the_stored_shape() {
    let config = every_kind_config();
    config.validate().unwrap();
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(
        serde_json::from_value::<Config>(json).unwrap(),
        config,
        "the JSON the settings screen edits must rebuild the same config"
    );
    let text = toml::to_string_pretty(&config).unwrap();
    assert_eq!(toml::from_str::<Config>(&text).unwrap(), config, "{text}");
    // The EC2 machine's own fields, not the runtime's, carry the launch
    // template, and the runtime is a plain bare harness on it.
    assert!(text.contains("[machines.fleet]"), "{text}");
    assert!(text.contains("launch_template = \"lt-0123\""), "{text}");
    assert!(
        !text.contains("build_cache") || text.matches("build_cache").count() == 2,
        "build caches belong to the two machines that have them: {text}"
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
max_size = "50GiB"

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
        max_size: Some("50GiB".into()),
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
        "max_size = \"50GiB\"",
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

#[test]
fn a_runtime_must_name_a_machine_that_exists() {
    let error = format!(
        "{:#}",
        toml::from_str::<Config>(
            &format!(
                "version = {CONFIG_VERSION}\n[targets.remote]\nkind = \"podman\"\nmachine = \"builder\"\nimage = \"a:1\"\n"
            )
        )
        .unwrap_err()
    );
    assert!(error.contains("which is not defined"), "{error}");
}

#[test]
fn settings_that_belong_to_a_machine_are_refused_on_a_runtime() {
    for (body, expected) in [
        (
            "[targets.here]\nkind = \"bare\"\npermissions = \"yolo\"\n",
            "only applies to a bare runtime",
        ),
        (
            "[machines.fleet]\nkind = \"aws-ec2\"\nregion = \"us-east-1\"\nlaunch_template = \"lt-1\"\nssh_user = \"ubuntu\"\n\
             [targets.fleet-podman]\nkind = \"podman\"\nmachine = \"fleet\"\nimage = \"a:1\"\n",
            "bare harness only",
        ),
        (
            "[targets.podman]\nkind = \"podman\"\nimage = \"a:1\"\n[targets.podman.build_cache]\nmax_size = \"1GiB\"\n",
            "belongs to [machines.local]",
        ),
        (
            "[machines.one]\nkind = \"ssh\"\nhost = \"builder\"\n[machines.two]\nkind = \"ssh\"\nhost = \"builder\"\n",
            "describe the same host",
        ),
    ] {
        let error = format!(
            "{:#}",
            toml::from_str::<Config>(&format!("version = {CONFIG_VERSION}\n{body}"))
                .map_err(anyhow::Error::from)
                .and_then(|config| config.validate())
                .unwrap_err()
        );
        assert!(error.contains(expected), "expected {expected:?}: {error}");
    }
}

#[test]
fn config_load_reports_key_errors_fatally() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "version = {CONFIG_VERSION}\n\
             [keys]\n\
             help = \"prefix+space\"\n\
             pane_preset = \"prefix+space\"\n"
        ),
    )
    .unwrap();

    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(
        error.contains("keys.pane_preset = \"prefix+space\""),
        "{error}"
    );
    assert!(error.contains("keys.help"), "{error}");

    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\n[keys]\nprefix = \"nope\"\n"),
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(error.contains("keys.prefix = \"nope\""), "{error}");
}

#[test]
fn unknown_keys_fields_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!("version = {CONFIG_VERSION}\n[keys]\nnew_sesion = \"prefix+c\"\n"),
    )
    .unwrap();
    let error = format!("{:#}", Config::load_from(&path).unwrap_err());
    assert!(error.contains("new_sesion"), "{error}");
}

#[test]
fn keys_section_is_omitted_from_serialized_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let config = Config::default();
    config.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(!body.contains("[keys]"), "{body}");

    let mut rebound = Config::default();
    rebound.keys.refresh = BindingConfig::from(["prefix+shift+r", "f5"]);
    rebound.save_to(&path).unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("[keys]"), "{body}");
    assert_eq!(Config::load_from(&path).unwrap(), rebound);
    assert_eq!(
        rebound.keybinds().labels(KeyAction::Refresh),
        vec!["ctrl+b shift+r".to_owned(), "f5".to_owned()]
    );
}

#[test]
fn automatic_continuation_defaults_on_and_disabled_setting_survives_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut config = Config::default();
    assert!(config.continuation.enabled);
    config.continuation.enabled = false;
    config.save_to(&path).unwrap();
    assert!(!Config::load_from(&path).unwrap().continuation.enabled);
}
