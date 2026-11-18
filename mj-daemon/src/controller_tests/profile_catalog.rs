//! Tests of `mj_controller::profile_catalog` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use mj_controller::profile_catalog::*;
use mj_core::config::HarnessKind;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// Hard-won: a2afafcb3: a model probe exempted its requested hidden model.
#[tokio::test]
async fn a_model_probe_cannot_exempt_a_hidden_requested_model() {
    let probe: Arc<Probe> = Arc::new(|id| {
        Box::pin(async move {
            let mut choices = test_choices(&id);
            choices.models.push(mj_core::acp::SessionConfigChoice {
                value: "hidden".into(),
                name: "hidden".into(),
                description: None,
            });
            Ok(choices)
        })
    });
    let default_probe = probe.clone();
    let catalog = ProfileCatalog::build(
        CancellationToken::new(),
        default_probe,
        Arc::new(move |id, model| {
            let probe = probe.clone();
            Box::pin(async move {
                let mut choices = probe(id).await?;
                choices.model = Some(model);
                Ok(choices)
            })
        }),
    );
    let mut config = test_config(&[("p", HarnessKind::Codex)]);
    config.profiles.get_mut("p").unwrap().models = vec!["allowed".into()];
    catalog.sync_now(&config).await;
    let choices = catalog
        .model_capabilities("p".into(), "hidden".into())
        .await
        .unwrap();
    assert!(crate::server::api::validate_selectors(&choices, Some("hidden"), None).is_err());
    assert!(
        choices
            .models
            .iter()
            .any(|choice| choice.value == "p-model")
    );
}
