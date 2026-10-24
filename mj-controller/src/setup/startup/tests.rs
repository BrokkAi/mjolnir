use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

use super::*;
use mj_core::config::TargetTemplate;

fn homes(directory: &Path, kinds: &[HarnessKind]) -> Vec<DiscoveredHome> {
    kinds
        .iter()
        .map(|kind| DiscoveredHome {
            kind: *kind,
            path: directory.join(format!("custom-{}", kind.id())),
            codex_profile: None,
            authenticated: false,
        })
        .collect()
}

fn repository() -> GithubRepository {
    GithubRepository {
        owner: "BrokkAi".into(),
        repository: "mjolnir".into(),
    }
}

#[test]
fn first_run_adds_codex_claude_or_both_even_before_login() {
    for kinds in [
        vec![HarnessKind::Codex],
        vec![HarnessKind::Claude],
        vec![HarnessKind::Codex, HarnessKind::Claude],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let profiles = homes(directory.path(), &kinds);
        let report = run_setup_with_discovery(
            &path,
            directory.path(),
            false,
            &|| false,
            || {},
            || Ok((profiles.clone(), Some(repository()))),
        )
        .unwrap()
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.profiles.len(), kinds.len());
        for profile in &profiles {
            assert!(
                config
                    .profiles
                    .values()
                    .any(|saved| saved.kind == profile.kind
                        && saved.home == profile.path
                        && saved.enabled)
            );
        }
        assert_eq!(
            config.bundles.values().next().unwrap().repositories[0]
                .github
                .as_deref(),
            Some("BrokkAi/mjolnir")
        );
        assert_eq!(report.agents.len(), kinds.len());
        assert_eq!(
            std::fs::read(directory.path().join("setup-state")).unwrap(),
            b"complete\n"
        );
        let before = std::fs::read(&path).unwrap();
        assert!(
            run_setup_with_discovery(
                &path,
                directory.path(),
                false,
                &|| false,
                || panic!("welcome repeated"),
                || panic!("discovery repeated")
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}

#[test]
fn forced_setup_adds_new_accounts_without_duplicates_or_overwriting_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let codex = homes(directory.path(), &[HarnessKind::Codex]);
    run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || Ok((codex, None)),
    )
    .unwrap();
    Config::update_to(&path, |config| {
        config.phone.enabled = false;
        config.profiles.get_mut("codex").unwrap().enabled = false;
        config
            .targets
            .insert("custom".into(), TargetTemplate::LocalBare);
        Ok(())
    })
    .unwrap();
    for _ in 0..2 {
        run_setup_with_discovery(
            &path,
            directory.path(),
            true,
            &|| false,
            || {},
            || {
                Ok((
                    homes(directory.path(), &[HarnessKind::Codex, HarnessKind::Claude]),
                    Some(repository()),
                ))
            },
        )
        .unwrap();
    }
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.profiles.len(), 2);
    assert_eq!(config.bundles.len(), 1);
    assert!(!config.profiles["codex"].enabled);
    assert!(!config.phone.enabled);
    assert!(config.targets.contains_key("custom"));
}

#[test]
fn automatic_setup_leaves_existing_installations_and_comments_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let body = "version = 2\n# Keep my custom target\n[targets.custom]\nkind = 'local-bare'\n";
    std::fs::write(&path, body).unwrap();
    assert!(
        run_setup_with_discovery(
            &path,
            directory.path(),
            false,
            &|| false,
            || panic!("old installation welcomed"),
            || panic!("old installation discovered")
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), body);
}

// Hard-won: 1c14f7b9b: discovery ran only on the first launch, so agents installed later never got profiles.
#[test]
fn every_launch_adds_newly_installed_agents_and_keeps_disabled_profiles_off() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let installed = homes(directory.path(), &[HarnessKind::Codex]);
    run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || Ok((installed.clone(), None)),
    )
    .unwrap()
    .unwrap();
    Config::update_to(&path, |config| {
        config
            .profiles
            .values_mut()
            .for_each(|profile| profile.enabled = false);
        Ok(())
    })
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        add_profiles_for(&path, &installed, &|| false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let mut later = homes(directory.path(), &[HarnessKind::Codex, HarnessKind::Claude]);
    // A Codex profile overlay written into the same home later.
    later.push(DiscoveredHome {
        codex_profile: Some("ds".to_owned()),
        ..later[0].clone()
    });
    let added = add_profiles_for(&path, &later, &|| false).unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(added, ["claude", "codex-ds"]);
    assert_eq!(config.profiles["claude"].kind, HarnessKind::Claude);
    assert_eq!(
        config.profiles["codex-ds"].codex_profile.as_deref(),
        Some("ds")
    );
    assert_eq!(config.profiles["codex-ds"].home, later[0].path);
    assert_eq!(config.profiles.len(), 3);
    assert!(
        config
            .profiles
            .iter()
            .all(|(id, profile)| profile.enabled == added.contains(id))
    );
    assert!(
        add_profiles_for(&path, &later, &|| false)
            .unwrap()
            .is_empty(),
        "the overlay's profile is not added again on the next launch"
    );
}

// Hard-won: e473d344a: a detected DSH profile ignored a key kept in secrets.toml and reported itself unauthenticated.
#[test]
fn a_detected_dsh_profile_takes_its_key_from_secrets_toml_when_it_is_there() {
    for with_secret in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        if with_secret {
            std::fs::write(
                directory.path().join("secrets.toml"),
                "DEEPSEEK_API_KEY = \"sk-test\"\n",
            )
            .unwrap();
        }
        // The first run and every later launch build profiles the same way.
        run_setup_with_discovery(
            &path,
            directory.path(),
            false,
            &|| false,
            || {},
            || Ok((homes(directory.path(), &[HarnessKind::Dsh]), None)),
        )
        .unwrap()
        .unwrap();
        let later = homes(directory.path(), &[HarnessKind::Dsh, HarnessKind::Codex]);
        let mut later_dsh = later[0].clone();
        later_dsh.path = directory.path().join("another-dsh");
        add_profiles_for(&path, &[later[1].clone(), later_dsh], &|| false).unwrap();

        let written: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let profiles = written["profiles"].as_table().unwrap();
        let reference = |id: &str| {
            profiles[id]
                .get("environment")
                .and_then(|environment| environment.get("DEEPSEEK_API_KEY"))
                .and_then(|value| value.get("from_secret"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        };
        let expected = with_secret.then(|| "DEEPSEEK_API_KEY".to_owned());
        assert_eq!(reference("dsh"), expected, "first run");
        assert_eq!(reference("dsh-2"), expected, "later launch");
        assert!(
            profiles["codex"].get("environment").is_none(),
            "a harness with a login of its own gets no key entry"
        );
    }
}

#[test]
fn setup_without_an_agent_or_repository_still_completes_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let report = run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || Ok((vec![], None)),
    )
    .unwrap()
    .unwrap();
    assert!(report.summary().is_empty());
    assert!(Config::load_from(&path).unwrap().profiles.is_empty());
    assert!(directory.path().join("setup-state").exists());
}

#[test]
fn interrupted_setup_recovers_after_configuration_was_already_saved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let discovered = homes(directory.path(), &[HarnessKind::Claude]);
    build_config(&discovered, Some(&repository()))
        .save_to(&path)
        .unwrap();
    atomic_write(&directory.path().join("setup-state"), b"pending\n").unwrap();
    run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || Ok((discovered, Some(repository()))),
    )
    .unwrap()
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.profiles.len(), 1);
    assert_eq!(config.bundles.len(), 1);
    assert_eq!(
        std::fs::read(directory.path().join("setup-state")).unwrap(),
        b"complete\n"
    );
}

#[test]
fn failed_discovery_is_retryable_and_never_marks_setup_complete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let error = run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || bail!("discovery failed"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("discovery failed"));
    assert_eq!(
        std::fs::read(directory.path().join("setup-state")).unwrap(),
        b"pending\n"
    );
    assert!(!path.exists());
    run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || Ok((vec![], None)),
    )
    .unwrap()
    .unwrap();
}

#[test]
fn concurrent_first_launches_have_one_setup_and_one_welcome() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let jobs: Vec<_> = (0..2)
        .map(|_| {
            let dir = directory.path().to_path_buf();
            let barrier = barrier.clone();
            let started = started.clone();
            std::thread::spawn(move || {
                barrier.wait();
                run_setup_with_discovery(
                    &dir.join("config.toml"),
                    &dir,
                    false,
                    &|| false,
                    || {
                        started.fetch_add(1, Ordering::SeqCst);
                    },
                    || {
                        Ok((
                            homes(&dir, &[HarnessKind::Codex, HarnessKind::Claude]),
                            None,
                        ))
                    },
                )
                .unwrap()
            })
        })
        .collect();
    let reports: Vec<_> = jobs
        .into_iter()
        .filter_map(|job| job.join().unwrap())
        .collect();
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(reports.len(), 1);
    assert_eq!(
        Config::load_from(&directory.path().join("config.toml"))
            .unwrap()
            .profiles
            .len(),
        2
    );
}

#[test]
fn discovery_merges_a_settings_edit_that_happened_while_it_ran() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    run_setup_with_discovery(
        &path,
        directory.path(),
        false,
        &|| false,
        || {},
        || {
            let existing = homes(directory.path(), &[HarnessKind::Codex]);
            build_config(&existing, None).save_to(&path)?;
            let mut discovered = homes(directory.path(), &[HarnessKind::Codex]);
            discovered[0].path = directory.path().join("different-codex-home");
            Ok((discovered, None))
        },
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.profiles.len(), 2);
    assert_eq!(
        config.profiles["codex"].home,
        directory.path().join("custom-codex")
    );
    assert_eq!(
        config.profiles["codex-2"].home,
        directory.path().join("different-codex-home")
    );
}

#[test]
fn cancelled_setup_does_not_wait_for_another_owner() {
    let directory = tempfile::tempdir().unwrap();
    let _owner = acquire_setup_lock(&directory.path().join("setup.lock"), &|| false).unwrap();
    let result = run_setup_with_discovery(
        &directory.path().join("config.toml"),
        directory.path(),
        true,
        &|| true,
        || panic!("cancelled setup accepted"),
        || panic!("cancelled setup discovered"),
    );
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}
