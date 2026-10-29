use super::*;

#[test]
fn results_from_a_closed_setup_do_not_change_the_new_draft() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    let old = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    dashboard.cancel_modal();
    dashboard.begin_setup();
    let new = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
    let original = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
    let mut detected = dashboard.config.clone();
    detected.targets.insert(
        "stale-discovery".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    dashboard.setup_discovered(old, Ok(detection(DetectScope::Runtimes, detected)));
    dashboard.setup_saved(old, Ok(dashboard.config.clone()));
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    assert_eq!(dialog.generation, new);
    assert_eq!(dialog.draft, original);
}

#[test]
fn detection_report_keeps_a_long_fix_reachable_until_settings_closes() {
    let mut dashboard = dashboard_with_session(stopped_session());
    dashboard.begin_setup();
    choose(&mut dashboard, "targets");
    let Mode::Setup(dialog) = &dashboard.mode else {
        panic!("settings");
    };
    let generation = dialog.generation;
    let fix = format!(
        "To enforce them, have systemd delegate the controllers to user sessions: create \
         `/etc/systemd/system/user@.service.d/80-delegate.conf` containing `[Service]` and \
         `Delegate=pids memory cpu`, run `sudo systemctl daemon-reload`, then log in again. {} \
         The tail of the fix is only reachable by scrolling.",
        "Padding so the report outgrows the band. ".repeat(50)
    );
    dashboard.setup_discovered(
        generation,
        Ok(crate::setup::SetupDetection {
            scope: DetectScope::Runtimes,
            config: Config::default(),
            rejected_runtimes: vec![RejectedRuntime {
                kind: "podman".into(),
                label: "Podman on nas".into(),
                detail: "systemd does not delegate the cpu and memory cgroup controllers to this \
                         user, so sessions start without them."
                    .into(),
                remediation: Some(fix.clone()),
            }],
            machines: Vec::new(),
        }),
    );
    // The status line counts and points at the view; it never carries the
    // fix, which a band this size would truncate.
    let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
    let notice = dialog.notice.as_deref().unwrap();
    assert!(
        notice.contains("1 was left out; Detection results says why."),
        "{notice}"
    );
    assert!(!notice.contains("delegate"), "{notice}");

    activate(&mut dashboard, SetupControl::DetectionResults);
    // The event loop renders between keys; the view's scroll maximum comes
    // from that render.
    let top = drawn(&mut dashboard, 140, 34).join("\n");
    assert!(
        !top.contains("only reachable by scrolling"),
        "the tail must start below the fold:\n{top}"
    );
    dashboard.handle_key(key(KeyCode::End));
    let shown = drawn(&mut dashboard, 140, 34).join("\n");
    assert!(
        shown.contains("The tail of the fix is only reachable by scrolling."),
        "the end of the report must reach the tail of the fix:\n{shown}"
    );

    // Esc leaves the report; the page keeps the report until Settings closes.
    dashboard.handle_key(key(KeyCode::Esc));
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .report
            .is_none()
    );
    activate(&mut dashboard, SetupControl::Back);
    choose(&mut dashboard, "targets");
    assert_eq!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .detection_rejected
            .len(),
        1
    );
    activate(&mut dashboard, SetupControl::DetectionResults);
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .report
            .is_some()
    );

    // A fresh Settings knows nothing of the previous run's report.
    dashboard.cancel_modal();
    dashboard.begin_setup();
    assert!(
        setup_dialog_mut(&mut dashboard.mode)
            .unwrap()
            .detection_rejected
            .is_empty()
    );
}
