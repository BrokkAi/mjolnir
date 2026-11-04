use super::*;
use mj_core::config::{HarnessKind, HarnessProfile};
use mj_core::profile_capabilities::ProfileCapabilities;
use mj_core::worker_launch::ProfileConfig;

// Catalog publications cross the daemon/client boundary independently of Settings.
#[tokio::test]
async fn settings_wait_for_selected_model_efforts_and_ignore_other_profile_identities() {
    let mut config = Config::default();
    config.profiles.insert(
        "reviewer".into(),
        HarnessProfile {
            home: "/reviewer".into(),
            enabled: true,
            kind: HarnessKind::Codex,
            environment: Default::default(),
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            muse_auto_review: None,
        },
    );
    let identity = config.profiles["reviewer"].capabilities_key("reviewer");
    let choice = |value: &str| SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    };
    let models = vec![choice("first"), choice("selected")];
    let mut entry = ProfileCapabilities {
        choices: CapabilityState::Ready(ProfileConfig {
            model: Some("first".into()),
            models: models.clone(),
            efforts: vec![choice("wrong")],
            observed_at: 0,
        }),
        ..ProfileCapabilities::default()
    };
    let (publisher, mut catalog) = watch::channel(ProfileCapabilitiesSnapshot::default());
    let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let request = ReviewDiscoveryRequest {
        profile: "reviewer".into(),
        model: Some("selected".into()),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let operation = {
        let flag = cancelled.clone();
        tokio::spawn(async move {
            discover_from_catalog(&config, &mut catalog, &request, &flag, &progress).await
        })
    };
    publisher
        .send(ProfileCapabilitiesSnapshot {
            profiles: [("old identity".into(), entry.clone())].into(),
        })
        .unwrap();
    tokio::task::yield_now().await;
    assert!(updates.try_recv().is_err());
    publisher
        .send(ProfileCapabilitiesSnapshot {
            profiles: [(identity.clone(), entry.clone())].into(),
        })
        .unwrap();
    let initial = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.model_choices, models);
    assert!(!initial.effort_capabilities_discovered);
    assert!(initial.effort_choices.is_empty());
    assert!(!operation.is_finished());
    entry.efforts.insert(
        "selected".into(),
        CapabilityState::Ready(vec![choice("high")]),
    );
    publisher
        .send(ProfileCapabilitiesSnapshot {
            profiles: [(identity, entry)].into(),
        })
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ReviewDiscoveryOutcome::Available { choices, .. } = result else {
        panic!("selected profile is ready")
    };
    assert_eq!(choices.effort_choices, vec![choice("high")]);
    assert!(choices.effort_capabilities_discovered);
}

#[tokio::test]
async fn cancelling_a_pending_catalog_read_does_not_wait_for_another_publication() {
    let mut config = Config::default();
    config.profiles.insert(
        "reviewer".into(),
        HarnessProfile {
            home: "/reviewer".into(),
            enabled: true,
            kind: HarnessKind::Codex,
            environment: Default::default(),
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
            muse_auto_review: None,
        },
    );
    let (_publisher, mut catalog) = watch::channel(ProfileCapabilitiesSnapshot::default());
    let (progress, _updates) = tokio::sync::mpsc::unbounded_channel();
    let request = ReviewDiscoveryRequest {
        profile: "reviewer".into(),
        model: None,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let operation = {
        let flag = cancelled.clone();
        tokio::spawn(async move {
            discover_from_catalog(&config, &mut catalog, &request, &flag, &progress).await
        })
    };
    tokio::task::yield_now().await;
    cancelled.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
}
