//! Tests of `mj_controller::worker_client` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use mj_controller::worker_client::test_fixtures::*;
use mj_controller::worker_client::*;
use std::time::Duration;

// An open AWS failure outlives the task that reported it: the replacement
// task's first clean cycle is reported too, so the notices end the failure and
// report a later recurrence.
#[tokio::test]
async fn a_replaced_renewal_task_still_ends_the_failure_its_predecessor_reported() {
    use crate::pollers::CredentialSyncNotices;

    let home = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let token = scratch.path().join("token-source");
    std::fs::write(&token, "pat").unwrap();
    let cli = granting_aws_cli(scratch.path());
    let root = scratch.path().join("session");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("install-fails"), "").unwrap();
    let target = aws_sync_session_named(home.path(), &root, &token, &cli, "replaced", None);
    let mut coordinator = CredentialSyncCoordinator::spawn();
    let handle = coordinator.handle();
    handle.set_targets(vec![target.clone()]);
    let next_partial = async |coordinator: &mut CredentialSyncCoordinator| {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let result = coordinator.result().await.expect("coordinator stopped");
                if result.partial {
                    return result;
                }
            }
        })
        .await
        .expect("a renewal result arrived")
    };
    let mut notices = CredentialSyncNotices::default();
    let state = mj_core::state::State::default();

    let failed = next_partial(&mut coordinator).await;
    assert!(failed.failures().next().is_some(), "{failed:?}");
    assert!(notices.notice(&failed, None, &state).is_some());

    // The target changes (a role edit): its task is replaced, and the worker
    // now accepts the install.
    std::fs::remove_file(root.join("install-fails")).unwrap();
    let mut changed = target.clone();
    changed
        .aws_credentials
        .as_mut()
        .unwrap()
        .environment
        .insert("AWS_REGION".into(), "us-east-2".into());
    handle.set_targets(vec![changed]);
    let clean = next_partial(&mut coordinator).await;
    assert!(clean.failures().next().is_none(), "{clean:?}");
    assert_eq!(notices.notice(&clean, None, &state), None);

    // The failure ended, so it is reported again when it recurs.
    assert!(notices.notice(&failed, None, &state).is_some());
}
