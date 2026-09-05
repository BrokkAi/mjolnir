use super::*;

fn container_template() -> hel::hel_config::ContainerTemplate {
    hel::hel_config::ContainerTemplate {
        image: "agent-dev:latest".into(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: BTreeMap::new(),
        workspace_storage: Default::default(),
    }
}

#[test]
fn imported_sessions_default_to_podman() {
    let mut config = HelConfig::default();
    config.targets.insert(
        "apple".into(),
        TargetTemplate::AppleContainer {
            container: container_template(),
        },
    );
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: container_template(),
        },
    );

    assert_eq!(default_import_target_id(&config), "podman");
}

#[test]
fn imported_sessions_prefer_a_custom_named_local_podman_target() {
    let mut config = HelConfig::default();
    config.targets.insert(
        "apple".into(),
        TargetTemplate::AppleContainer {
            container: container_template(),
        },
    );
    config.targets.insert(
        "workstation".into(),
        TargetTemplate::LocalPodman {
            container: container_template(),
        },
    );

    assert_eq!(default_import_target_id(&config), "workstation");
}

fn initialize_repository(path: &Path, id: &str) {
    fs::create_dir_all(path).unwrap();
    for arguments in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.name", "Hel Test"],
        vec!["config", "user.email", "hel@example.test"],
        vec![
            "remote",
            "add",
            "origin",
            &format!("https://github.com/example/{id}.git"),
        ],
    ] {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(path.join("README.md"), id).unwrap();
    let output = Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = Command::new("git")
        .args(["commit", "-qm", "base"])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    // Import deltas against the tracked remote, so a realistic checkout
    // needs a remote-tracking ref.
    for arguments in [
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        vec!["branch", "--set-upstream-to", "origin/main", "main"],
    ] {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn session_targets_include_edited_roots_and_keep_cwd_primary() {
    let directory = durable_fixture_directory();
    let app = directory.path().join("app");
    let sibling = directory.path().join("sibling");
    initialize_repository(&app, "app");
    initialize_repository(&sibling, "sibling");
    let transcript = ClaudeTranscript {
        cwd: app.clone(),
        edited_paths: vec![sibling.join("src/lib.rs")],
        events: Vec::new(),
    };

    let targets = session_edit_targets(&transcript, &directory.path().join("profile")).unwrap();
    assert_eq!(targets.git_roots.len(), 2);
    assert!(targets.git_roots.contains(&fs::canonicalize(app).unwrap()));
    assert!(
        targets
            .git_roots
            .contains(&fs::canonicalize(sibling).unwrap())
    );
    assert!(targets.non_git_dirs.is_empty());
    assert!(targets.scratch_git_roots.is_empty());
}

#[test]
fn session_targets_omit_repositories_under_temporary_directories() {
    let scratch_home = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    let sibling = directory.path().join("sibling");
    let throwaway = scratch_home.path().join("throwaway");
    initialize_repository(&app, "app");
    initialize_repository(&sibling, "sibling");
    initialize_repository(&throwaway, "throwaway");
    let prefixes = [fs::canonicalize(scratch_home.path()).unwrap()];
    let transcript = ClaudeTranscript {
        cwd: app.clone(),
        edited_paths: vec![sibling.join("src/lib.rs"), throwaway.join("notes.md")],
        events: Vec::new(),
    };

    let targets = session_edit_targets_with_scratch_prefixes(
        &transcript,
        &directory.path().join("profile"),
        &prefixes,
    )
    .unwrap();

    assert_eq!(
        targets.git_roots,
        [
            fs::canonicalize(&app).unwrap(),
            fs::canonicalize(&sibling).unwrap()
        ]
    );
    assert_eq!(
        targets.scratch_git_roots,
        [fs::canonicalize(&throwaway).unwrap()]
    );
}

#[test]
fn session_targets_keep_a_cwd_repository_under_a_temporary_directory() {
    let scratch_home = tempfile::tempdir().unwrap();
    let throwaway = scratch_home.path().join("throwaway");
    initialize_repository(&throwaway, "throwaway");
    let prefixes = [fs::canonicalize(scratch_home.path()).unwrap()];
    let transcript = ClaudeTranscript {
        cwd: throwaway.clone(),
        edited_paths: vec![throwaway.join("notes.md")],
        events: Vec::new(),
    };

    let targets = session_edit_targets_with_scratch_prefixes(
        &transcript,
        &scratch_home.path().join("profile"),
        &prefixes,
    )
    .unwrap();

    assert_eq!(targets.git_roots, [fs::canonicalize(&throwaway).unwrap()]);
    assert!(targets.scratch_git_roots.is_empty());
}

#[test]
fn codex_extracts_only_completed_file_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/work/app","history_mode":"paginated"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"text":"edit"}]}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","status":"completed","changes":{"/work/a.txt":{"type":"add"}}}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","status":"failed","changes":{"/work/b.txt":{"type":"add"}}}}}"#,
                "\n",
            ),
        )
        .unwrap();
    let transcript = read_codex_transcript(&path).unwrap();
    assert_eq!(transcript.edited_paths, [PathBuf::from("/work/a.txt")]);
}

#[test]
fn claude_prefers_file_history_and_accepts_successful_edit_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    fs::write(
            &path,
            concat!(
                r#"{"type":"user","cwd":"/work/app","message":{"content":"edit"}}"#,
                "\n",
                r#"{"type":"file-history-delta","trackingPath":"src/lib.rs","backup":{"realParentDir":"/work/app/src"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"ok","name":"Write","input":{"file_path":"relative.txt"}},{"type":"tool_use","id":"bad","name":"Edit","input":{"file_path":"bad.txt"}}]}}"#,
                "\n",
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ok","content":"done"},{"type":"tool_result","tool_use_id":"bad","is_error":true,"content":"failed"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
    let transcript = read_claude_transcript(&path).unwrap();
    assert_eq!(
        transcript.edited_paths,
        [
            PathBuf::from("/work/app/src/lib.rs"),
            PathBuf::from("relative.txt")
        ]
    );
}

#[test]
fn kimi_extracts_successful_edits_from_all_agents() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("agents/main")).unwrap();
    fs::write(
            directory.path().join("agents/main/wire.jsonl"),
            concat!(
                r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"ok","name":"Write","args":{"path":"one.txt"}}}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"ok","result":{"output":"done"}}}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"bad","name":"Edit","args":{"path":"bad.txt"}}}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"bad","result":{"isError":true}}}"#,
                "\n",
            ),
        )
        .unwrap();
    assert_eq!(
        kimi_edited_paths(directory.path()).unwrap(),
        [PathBuf::from("one.txt")]
    );
}

#[test]
fn resolve_bundle_requires_an_exact_root_set() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    let sibling = directory.path().join("sibling");
    initialize_repository(&app, "app");
    initialize_repository(&sibling, "sibling");
    let targets = SessionEditTargets {
        git_roots: vec![
            fs::canonicalize(&app).unwrap(),
            fs::canonicalize(&sibling).unwrap(),
        ],
        scratch_git_roots: Vec::new(),
        non_git_dirs: Vec::new(),
    };
    let config = HelConfig::default();
    let BundleResolution::Synthesized { bundle, .. } =
        resolve_bundle(&config, &app, &targets, None).unwrap()
    else {
        panic!("expected synthesized bundle");
    };
    assert_eq!(bundle.repositories.len(), 2);
    assert_eq!(bundle.primary_repo, "app");

    let mut config = HelConfig::default();
    config.bundles.insert("multi".into(), bundle);
    assert_eq!(
        resolve_bundle(&config, &app, &targets, None).unwrap(),
        BundleResolution::Existing("multi".into())
    );
    let app_only = SessionEditTargets {
        git_roots: vec![fs::canonicalize(&app).unwrap()],
        scratch_git_roots: Vec::new(),
        non_git_dirs: Vec::new(),
    };
    assert!(resolve_bundle(&config, &app, &app_only, Some("multi")).is_err());
}

/// A repository without a remote takes the `Local` identity, which is the
/// case a linked worktree used to split into its own project.
fn initialize_local_repository(path: &Path, id: &str) {
    initialize_repository(path, id);
    run_git(path, &["remote", "remove", "origin"]);
}

fn run_git(path: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn add_worktree(repository: &Path, worktree: &Path, branch: &str) -> PathBuf {
    run_git(
        repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            worktree.to_str().unwrap(),
        ],
    );
    fs::canonicalize(worktree).unwrap()
}

#[test]
fn resolve_bundle_matches_an_existing_bundle_from_a_linked_worktree() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_local_repository(&app, "app");
    let worktree = add_worktree(&app, &directory.path().join("app2"), "side");
    let app = fs::canonicalize(&app).unwrap();

    let mut config = HelConfig::default();
    config.bundles.insert(
        "app".into(),
        ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![ProjectRepository {
                id: "app".into(),
                local: Some(app.clone()),
                github: None,
                destination: PathBuf::from("app"),
                git_ref: None,
            }],
        },
    );
    let targets = SessionEditTargets {
        git_roots: vec![worktree.clone()],
        non_git_dirs: Vec::new(),
        scratch_git_roots: Vec::new(),
    };

    assert_eq!(
        resolve_bundle(&config, &worktree, &targets, None).unwrap(),
        BundleResolution::Existing("app".into())
    );
}

#[test]
fn resolve_bundle_synthesizes_a_worktree_session_as_its_main_repository() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_local_repository(&app, "app");
    let worktree = add_worktree(&app, &directory.path().join("app2"), "side");
    let app = fs::canonicalize(&app).unwrap();
    let targets = SessionEditTargets {
        git_roots: vec![worktree.clone(), app.clone()],
        non_git_dirs: Vec::new(),
        scratch_git_roots: Vec::new(),
    };

    let BundleResolution::Synthesized { id, bundle } =
        resolve_bundle(&HelConfig::default(), &worktree, &targets, None).unwrap()
    else {
        panic!("expected synthesized bundle");
    };
    assert_eq!(id, "app");
    assert_eq!(bundle.primary_repo, "app");
    assert_eq!(bundle.repositories.len(), 1);
    assert_eq!(bundle.repositories[0].id, "app");
    assert_eq!(bundle.repositories[0].local.as_deref(), Some(app.as_path()));
}

#[test]
fn synthesized_bundles_keep_one_repository_per_shared_origin() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    let worktree = directory.path().join("app-review");
    initialize_repository(&app, "app");
    initialize_repository(&worktree, "app");
    let targets = SessionEditTargets {
        git_roots: vec![
            fs::canonicalize(&app).unwrap(),
            fs::canonicalize(&worktree).unwrap(),
        ],
        scratch_git_roots: Vec::new(),
        non_git_dirs: Vec::new(),
    };

    let BundleResolution::Synthesized { bundle, .. } =
        resolve_bundle(&HelConfig::default(), &app, &targets, None).unwrap()
    else {
        panic!("expected synthesized bundle");
    };

    assert_eq!(bundle.repositories.len(), 1);
    assert_eq!(bundle.primary_repo, "app");
    assert_eq!(
        bundle.repositories[0].github.as_deref(),
        Some("example/app")
    );
}

#[test]
fn synthesized_bundles_keep_separate_local_repositories() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    let tools = directory.path().join("tools");
    for (path, id) in [(&app, "app"), (&tools, "tools")] {
        initialize_repository(path, id);
        let output = Command::new("git")
            .args(["remote", "remove", "origin"])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let targets = SessionEditTargets {
        git_roots: vec![
            fs::canonicalize(&app).unwrap(),
            fs::canonicalize(&tools).unwrap(),
        ],
        scratch_git_roots: Vec::new(),
        non_git_dirs: Vec::new(),
    };

    let BundleResolution::Synthesized { bundle, .. } =
        resolve_bundle(&HelConfig::default(), &app, &targets, None).unwrap()
    else {
        panic!("expected synthesized bundle");
    };

    assert_eq!(bundle.repositories.len(), 2);
    assert_eq!(bundle.primary_repo, "app");
    assert_eq!(
        bundle
            .repositories
            .iter()
            .map(|repository| repository.id.as_str())
            .collect::<Vec<_>>(),
        ["app", "tools"]
    );
}

#[test]
fn edit_targets_filter_profile_state_and_report_non_git_directories() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    let profile = directory.path().join("profile");
    let outside = directory.path().join("notes");
    initialize_repository(&app, "app");
    fs::create_dir_all(&profile).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let transcript = ClaudeTranscript {
        cwd: app.clone(),
        edited_paths: vec![profile.join("memory.md"), outside.join("draft.md")],
        events: Vec::new(),
    };

    let targets = session_edit_targets(&transcript, &profile).unwrap();
    assert_eq!(targets.git_roots, [fs::canonicalize(app).unwrap()]);
    assert_eq!(targets.non_git_dirs, [outside]);
}

#[cfg(unix)]
#[test]
fn edit_targets_compare_profile_paths_after_resolving_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let actual = directory.path().join("actual");
    let alias = directory.path().join("alias");
    let app = actual.join("app");
    let profile = actual.join("profile");
    let outside = actual.join("notes");
    initialize_repository(&app, "app");
    fs::create_dir_all(&profile).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&actual, &alias).unwrap();
    let transcript = ClaudeTranscript {
        cwd: alias.join("app"),
        edited_paths: vec![
            alias.join("profile/memory.md"),
            alias.join("notes/draft.md"),
        ],
        events: Vec::new(),
    };

    let targets = session_edit_targets(&transcript, &alias.join("profile")).unwrap();

    assert_eq!(targets.git_roots, [fs::canonicalize(app).unwrap()]);
    assert_eq!(targets.non_git_dirs, [alias.join("notes")]);
}

#[test]
fn import_safety_reports_dirty_roots_and_non_git_omissions() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    fs::write(app.join("README.md"), "dirty").unwrap();
    fs::write(app.join("untracked.txt"), "new").unwrap();
    let omitted = directory.path().join("notes");
    let scratch = directory.path().join("scratch");
    let issues = import_safety_issues(&SessionEditTargets {
        git_roots: vec![app.clone()],
        scratch_git_roots: vec![scratch.clone()],
        non_git_dirs: vec![omitted.clone()],
    })
    .unwrap();

    assert_eq!(issues.dirty_git_roots.len(), 1);
    assert_eq!(issues.dirty_git_roots[0].0, app);
    assert_eq!(
        issues.dirty_git_roots[0].1,
        "1 tracked change · 1 untracked path"
    );
    assert!(issues.has_untracked_files);
    assert_eq!(issues.omitted_non_git_dirs, [omitted]);
    assert_eq!(issues.scratch_git_roots, [scratch]);
}

#[test]
fn projects_jsonl_projects_user_and_assistant_text_in_source_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    fs::write(
            &path,
            concat!(
                r#"{"type":"user","cwd":"/work/app","message":{"content":"<mj-project-memory>private</mj-project-memory>\nfirst prompt"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"first reply"}]}}"#,
                "\n",
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ignored"}]}}"#,
                "\n",
                r#"{"type":"user","isMeta":true,"message":{"content":"ignored meta"}}"#,
                "\n",
                r#"{"type":"user","message":{"content":"second prompt"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"second "},{"type":"text","text":"reply"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

    let transcript = read_claude_transcript(&path).unwrap();
    assert_eq!(transcript.cwd, PathBuf::from("/work/app"));
    assert_eq!(
        transcript
            .events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert!(matches!(
        &transcript.events[0].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "first prompt"
    ));
    assert_eq!(
        agent_text(&transcript.events[1].event).as_deref(),
        Some("first reply")
    );
    assert!(matches!(
        &transcript.events[2].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "second prompt"
    ));
    assert_eq!(
        agent_text(&transcript.events[4].event).as_deref(),
        Some("reply")
    );
    assert!(matches!(
        transcript.events[5].event,
        WorkerEvent::TurnCompleted
    ));
}

#[test]
fn bundle_origin_mapping_matches_configured_primary_repository() {
    let mut config = HelConfig::default();
    config.bundles.insert(
        "hel".into(),
        ProjectBundle {
            primary_repo: "hel".into(),
            repositories: vec![ProjectRepository {
                id: "hel".into(),
                github: Some("BrokkAi/hel".into()),
                local: None,
                destination: "hel".into(),
                git_ref: None,
            }],
        },
    );
    let origin = github_repository_from_origin("git@github.com:brokkai/HEL.git").unwrap();
    assert_eq!(
        configured_bundle_for_origin(&config, &origin).as_deref(),
        Some("hel")
    );
}

#[test]
fn local_repository_collection_preserves_bundle_order() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let app = workspace.join("app");
    initialize_repository(&app, "app");
    initialize_repository(&workspace.join("worker"), "worker");
    let bundle = ProjectBundle {
        primary_repo: "app".into(),
        repositories: vec![
            ProjectRepository {
                id: "worker".into(),
                github: Some("example/worker".into()),
                local: None,
                destination: "worker".into(),
                git_ref: None,
            },
            ProjectRepository {
                id: "app".into(),
                github: Some("example/app".into()),
                local: None,
                destination: "app".into(),
                git_ref: None,
            },
        ],
    };

    let snapshots =
        collect_local_repositories(&bundle, &[app.clone(), workspace.join("worker")], None)
            .unwrap();
    let ids = snapshots
        .iter()
        .map(|snapshot| snapshot.metadata.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["worker", "app"]);
}

#[test]
fn local_source_repository_import_carries_no_committed_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    let output = Command::new("git")
        .args(["remote", "remove", "origin"])
        .current_dir(&app)
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::write(app.join("dirty.txt"), "dirty").unwrap();
    let bundle = ProjectBundle {
        primary_repo: "app".into(),
        repositories: vec![ProjectRepository {
            id: "app".into(),
            github: None,
            local: Some(app.clone()),
            destination: "app".into(),
            git_ref: None,
        }],
    };

    let snapshots = collect_local_repositories(&bundle, &[app], None).unwrap();

    assert!(snapshots[0].committed_bundle.is_empty());
    assert_eq!(snapshots[0].metadata.origin, "mj-local:app");
    assert!(!snapshots[0].untracked_tar.is_empty());
}

#[test]
fn import_without_remote_tracking_refs_reports_how_to_recover() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    let output = Command::new("git")
        .args(["update-ref", "-d", "refs/remotes/origin/main"])
        .current_dir(&app)
        .output()
        .unwrap();
    assert!(output.status.success());
    let bundle = ProjectBundle {
        primary_repo: "app".into(),
        repositories: vec![ProjectRepository {
            id: "app".into(),
            github: Some("example/app".into()),
            local: None,
            destination: "app".into(),
            git_ref: None,
        }],
    };

    let error = collect_local_repositories(&bundle, &[app], None).unwrap_err();

    assert!(
        format!("{error:#}").contains("has no remote-tracking refs to import against"),
        "{error:#}"
    );
}

#[test]
fn codex_jsonl_projects_user_and_agent_messages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"session_id":"019feb6c-5ffc-7c12-ad99-bdeaeb6be79d","cwd":"/work/app","history_mode":"paginated"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":"<mj-project-memory>private</mj-project-memory>","text_elements":[]},{"type":"text","text":"first prompt","text_elements":[]}]}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"agent-1","content":[{"type":"Text","text":"first reply"}],"phase":"final_answer"}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"turn_complete","turn_id":"turn-1"}}"#,
                "\n",
            ),
        )
        .unwrap();

    let transcript = read_codex_transcript(&path).unwrap();
    assert_eq!(transcript.cwd, PathBuf::from("/work/app"));
    assert!(matches!(
        &transcript.events[0].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "first prompt"
    ));
    assert_eq!(
        agent_text(&transcript.events[1].event).as_deref(),
        Some("first reply")
    );
    assert!(matches!(
        transcript.events[2].event,
        WorkerEvent::TurnCompleted
    ));
}

#[test]
fn nonempty_codex_import_materializes_and_validates_canonical_archive() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");

    let codex_home = directory.path().join("codex");
    let native_session_id = "019feb6c-5ffc-7c12-ad99-bdeaeb6be79d";
    let rollout = codex_home
        .join("sessions/2026/08/14")
        .join(format!("rollout-{native_session_id}.jsonl"));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let records = [
        json!({
            "timestamp": "2026-08-14T12:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": native_session_id,
                "cwd": app,
                "history_mode": "paginated"
            }
        }),
        json!({
            "timestamp": "2026-08-14T12:00:01.250Z",
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "UserMessage",
                    "content": [{"type": "text", "text": "import this"}]
                }
            }
        }),
        json!({
            "timestamp": "2026-08-14T12:00:02.500Z",
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "AgentMessage",
                    "content": [{"type": "Text", "text": "imported"}]
                }
            }
        }),
        json!({
            "timestamp": "2026-08-14T12:00:03.750Z",
            "type": "event_msg",
            "payload": {"type": "turn_complete", "turn_id": "turn-1"}
        }),
    ];
    fs::write(
        &rollout,
        records
            .into_iter()
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let transcript = read_codex_transcript(&rollout).unwrap();
    let expected_user_time = DateTime::parse_from_rfc3339("2026-08-14T12:00:01.250Z")
        .unwrap()
        .timestamp_millis();
    let expected_agent_time = DateTime::parse_from_rfc3339("2026-08-14T12:00:02.500Z")
        .unwrap()
        .timestamp_millis();
    let expected_activity = DateTime::parse_from_rfc3339("2026-08-14T12:00:03.750Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        transcript
            .events
            .iter()
            .map(|event| event.recorded_at_ms)
            .collect::<Vec<_>>(),
        [
            Some(expected_user_time),
            Some(expected_agent_time),
            Some(expected_activity)
        ]
    );
    let metadata = fs::metadata(&rollout).unwrap();
    let source = LocatedCodexSession {
        natively_archived: false,
        native_session_id: native_session_id.into(),
        jsonl_path: rollout,
        modified_at: metadata.modified().unwrap(),
        title: "Imported Codex session".into(),
        cwd: app,
        git_branch: "main".into(),
        size_bytes: metadata.len(),
        history_mode: CodexHistoryMode::Paginated,
    };
    let mut config = HelConfig::default();
    config.bundles.insert(
        "app".into(),
        ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![ProjectRepository {
                id: "app".into(),
                github: Some("example/app".into()),
                local: None,
                destination: "app".into(),
                git_ref: None,
            }],
        },
    );
    let archive_directory = directory.path().join("archives");
    fs::create_dir_all(&archive_directory).unwrap();
    let mut state = HelState::default();

    let imported = import_codex_session(
        &config,
        &mut state,
        CodexImportRequest {
            codex_home: &codex_home,
            source: &source,
            transcript: &transcript,
            bundle_id: "app",
            profile_id: None,
            title: None,
            archive_directory: &archive_directory,
        },
    )
    .unwrap();
    let verified = hel::hel_archive::verify_archive_streaming(&imported.archive_path).unwrap();
    assert_eq!(verified.canonical_session.event_frontier, 3);
    assert_eq!(
        verified.canonical_session.session.last_activity_at_ms,
        Some(expected_activity)
    );
    assert_eq!(verified.canonical_session.transcript.len(), 2);
    assert_eq!(
        verified.canonical_session.transcript[0].created_at_ms,
        expected_user_time
    );
    assert_eq!(
        verified.canonical_session.transcript[1].created_at_ms,
        expected_agent_time
    );
    assert!(state.sessions.contains_key(&imported.session_id));
}

const IMPORT_FIXTURE_SESSION: &str = "019feb6c-5ffc-7c12-ad99-bdeaeb6be79d";

fn github_bundle(ids: &[&str]) -> ProjectBundle {
    ProjectBundle {
        primary_repo: ids[0].to_owned(),
        repositories: ids
            .iter()
            .map(|id| ProjectRepository {
                id: (*id).to_owned(),
                github: Some(format!("example/{id}")),
                local: None,
                destination: PathBuf::from(id),
                git_ref: None,
            })
            .collect(),
    }
}

fn codex_import_source(
    codex_home: &Path,
    cwd: &Path,
    edited_paths: &[PathBuf],
) -> LocatedCodexSession {
    let rollout = codex_home
        .join("sessions/2026/08/14")
        .join(format!("rollout-{IMPORT_FIXTURE_SESSION}.jsonl"));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let mut records = vec![
        json!({
            "timestamp": "2026-08-14T12:00:00.000Z",
            "type": "session_meta",
            "payload": {"id": IMPORT_FIXTURE_SESSION, "cwd": cwd, "history_mode": "paginated"}
        }),
        json!({
            "timestamp": "2026-08-14T12:00:01.250Z",
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "UserMessage",
                    "content": [{"type": "text", "text": "import this"}]
                }
            }
        }),
    ];
    for path in edited_paths {
        let mut changes = serde_json::Map::new();
        changes.insert(path.to_string_lossy().into_owned(), json!({"type": "add"}));
        records.push(json!({
            "timestamp": "2026-08-14T12:00:02.500Z",
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "FileChange",
                    "status": "completed",
                    "changes": Value::Object(changes)
                }
            }
        }));
    }
    fs::write(
        &rollout,
        records
            .into_iter()
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let metadata = fs::metadata(&rollout).unwrap();
    LocatedCodexSession {
        natively_archived: false,
        native_session_id: IMPORT_FIXTURE_SESSION.into(),
        jsonl_path: rollout,
        modified_at: metadata.modified().unwrap(),
        title: "Imported Codex session".into(),
        cwd: cwd.to_path_buf(),
        git_branch: "main".into(),
        size_bytes: metadata.len(),
        history_mode: CodexHistoryMode::Paginated,
    }
}

fn import_codex_fixture(
    config: &HelConfig,
    state: &mut HelState,
    source: &LocatedCodexSession,
    bundle_id: &str,
    codex_home: &Path,
    archive_directory: &Path,
) -> ImportedCodexSession {
    fs::create_dir_all(archive_directory).unwrap();
    let transcript = read_codex_transcript(&source.jsonl_path).unwrap();
    import_codex_session(
        config,
        state,
        CodexImportRequest {
            codex_home,
            source,
            transcript: &transcript,
            bundle_id,
            profile_id: None,
            title: None,
            archive_directory,
        },
    )
    .unwrap()
}

fn import_test_targets(local_bare: bool) -> BTreeMap<String, TargetTemplate> {
    let mut targets = BTreeMap::from([(
        "podman".to_owned(),
        TargetTemplate::LocalPodman {
            container: container_template(),
        },
    )]);
    if local_bare {
        targets.insert("localhost".to_owned(), TargetTemplate::LocalBare);
    }
    targets
}

/// Durable fixtures must sit outside every temporary directory, because
/// import now treats repositories under those as scratch.
fn durable_fixture_directory() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/import-fixtures");
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("durable")
        .tempdir_in(base)
        .unwrap()
}

#[test]
fn single_repository_import_becomes_a_raw_project_session() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    let codex_home = directory.path().join("codex");
    let source = codex_import_source(&codex_home, &app, &[]);
    let config = HelConfig {
        bundles: BTreeMap::from([("app".to_owned(), github_bundle(&["app"]))]),
        targets: import_test_targets(true),
        ..HelConfig::default()
    };
    let mut state = HelState::default();

    let imported = import_codex_fixture(
        &config,
        &mut state,
        &source,
        "app",
        &codex_home,
        &directory.path().join("archives"),
    );

    let record = &state.sessions[&imported.session_id];
    assert_eq!(
        record.project_directory,
        Some(fs::canonicalize(&app).unwrap())
    );
    assert_eq!(record.target_template_id, "localhost");
    assert_eq!(record.bundle_id, "app");
}

#[test]
fn single_repository_import_without_a_local_bare_target_stays_a_bundle_session() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    let codex_home = directory.path().join("codex");
    let source = codex_import_source(&codex_home, &app, &[]);
    let config = HelConfig {
        bundles: BTreeMap::from([("app".to_owned(), github_bundle(&["app"]))]),
        targets: import_test_targets(false),
        ..HelConfig::default()
    };
    let mut state = HelState::default();

    let imported = import_codex_fixture(
        &config,
        &mut state,
        &source,
        "app",
        &codex_home,
        &directory.path().join("archives"),
    );

    let record = &state.sessions[&imported.session_id];
    assert_eq!(record.project_directory, None);
    assert_eq!(record.target_template_id, "podman");
}

#[test]
fn import_of_a_session_with_a_second_repository_stays_a_bundle_session() {
    let directory = durable_fixture_directory();
    let app = directory.path().join("app");
    let tools = directory.path().join("tools");
    initialize_repository(&app, "app");
    initialize_repository(&tools, "tools");
    let codex_home = directory.path().join("codex");
    let source = codex_import_source(&codex_home, &app, &[tools.join("script.sh")]);
    let config = HelConfig {
        bundles: BTreeMap::from([("app".to_owned(), github_bundle(&["app", "tools"]))]),
        targets: import_test_targets(true),
        ..HelConfig::default()
    };
    let mut state = HelState::default();

    let imported = import_codex_fixture(
        &config,
        &mut state,
        &source,
        "app",
        &codex_home,
        &directory.path().join("archives"),
    );

    let record = &state.sessions[&imported.session_id];
    assert_eq!(record.project_directory, None);
    assert_eq!(record.target_template_id, "podman");
}

#[test]
fn stop_sequence_claude_import_produces_idle_raw_project_session() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("app");
    initialize_repository(&app, "app");
    let claude_home = directory.path().join("claude");
    let transcript_path = claude_home
        .join("projects/-work-app")
        .join(format!("{IMPORT_FIXTURE_SESSION}.jsonl"));
    fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    fs::write(
        &transcript_path,
        [
            json!({
                "timestamp": "2026-08-14T12:00:00.000Z",
                "type": "user",
                "cwd": app,
                "message": {"content": "import this"}
            }),
            json!({
                "timestamp": "2026-08-14T12:00:01.000Z",
                "type": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "imported"}],
                    "stop_reason": "stop_sequence"
                }
            }),
        ]
        .map(|record| record.to_string())
        .join("\n"),
    )
    .unwrap();
    let transcript = read_claude_transcript(&transcript_path).unwrap();
    let metadata = fs::metadata(&transcript_path).unwrap();
    let source = LocatedClaudeSession {
        native_session_id: IMPORT_FIXTURE_SESSION.into(),
        jsonl_path: transcript_path.clone(),
        modified_at: metadata.modified().unwrap(),
        title: "Imported Claude session".into(),
        cwd: app.clone(),
        git_branch: "main".into(),
        size_bytes: metadata.len(),
    };
    let config = HelConfig {
        bundles: BTreeMap::from([("app".to_owned(), github_bundle(&["app"]))]),
        targets: import_test_targets(true),
        ..HelConfig::default()
    };
    let archive_directory = directory.path().join("archives");
    fs::create_dir_all(&archive_directory).unwrap();
    let mut state = HelState::default();

    let imported = import_claude_session(
        &config,
        &mut state,
        ClaudeImportRequest {
            claude_home: &claude_home,
            source: &source,
            transcript: &transcript,
            bundle_id: "app",
            profile_id: None,
            title: None,
            archive_directory: &archive_directory,
        },
    )
    .unwrap();

    let record = &state.sessions[&imported.session_id];
    let verified = hel::hel_archive::verify_archive_streaming(&imported.archive_path).unwrap();
    assert_eq!(
        verified.canonical_session.session.execution,
        hel::hel_archive::CanonicalExecutionState::Idle
    );
    assert_eq!(
        record.project_directory,
        Some(fs::canonicalize(&app).unwrap())
    );
    assert_eq!(record.target_template_id, "localhost");
}

#[test]
fn codex_paginated_import_ignores_compaction_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"cwd":"/work/app","history_mode":"paginated"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"compaction","encrypted_content":"opaque"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"user-1","content":[{"type":"text","text":"prompt after compaction"}]}}}"#,
                "\n",
            ),
        )
        .unwrap();
    let transcript = read_codex_transcript(&path).unwrap();
    assert!(matches!(
        &transcript.events[0].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "prompt after compaction"
    ));
    assert_eq!(transcript.events.len(), 2);
}

#[test]
fn codex_import_rejects_legacy_history_with_migration_guidance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session_meta","payload":{"cwd":"/work/app"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"raw prompt"}}"#,
            "\n",
        ),
    )
    .unwrap();
    let error = read_codex_transcript(&path).unwrap_err().to_string();
    assert!(error.contains("Legacy Codex history cannot be imported"));
    assert!(error.contains("codex migrate-rollouts --apply"));
}

#[test]
fn claude_import_rejects_a_compaction_summary_without_raw_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    fs::write(
        &path,
        concat!(
            r#"{"type":"system","subtype":"compact_boundary","cwd":"/work/app"}"#,
            "\n",
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}"#,
            "\n",
        ),
    )
    .unwrap();
    assert!(
        read_claude_transcript(&path)
            .unwrap_err()
            .to_string()
            .contains("before recoverable raw history")
    );
}

#[test]
fn codex_locator_uses_rollout_id_when_session_id_names_a_parent() {
    let directory = tempfile::tempdir().unwrap();
    let rollout = directory
            .path()
            .join("sessions/2026/08/10/rollout-2026-08-10T00-00-00-019feb6c-6b55-7111-a210-6d85ee0772cd.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(
            &rollout,
            r#"{"type":"session_meta","payload":{"session_id":"019feb6c-5ffc-7c12-ad99-bdeaeb6be79d","id":"019feb6c-6b55-7111-a210-6d85ee0772cd"}}"#,
        )
        .unwrap();

    let located = locate_codex_session(
        directory.path(),
        &CodexSessionSelection::NativeSessionId("019feb6c-6b55-7111-a210-6d85ee0772cd".into()),
    )
    .unwrap();
    assert_eq!(
        located.native_session_id,
        "019feb6c-6b55-7111-a210-6d85ee0772cd"
    );
    assert_eq!(located.jsonl_path, rollout);
}

#[test]
fn codex_listing_uses_native_title_and_session_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "019feb6c-6b55-7111-a210-6d85ee0772cd";
    let rollout = directory.path().join("sessions/rollout.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(
            &rollout,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{session_id}","cwd":"/work/app","git":{{"branch":"feature"}}}}}}"#
            ),
        )
        .unwrap();
    fs::write(
        directory.path().join("session_index.jsonl"),
        format!(r#"{{"id":"{session_id}","thread_name":"native title\ncontinued"}}"#),
    )
    .unwrap();

    let sessions = list_codex_sessions(directory.path()).unwrap();
    assert_eq!(sessions[0].title, "native title continued");
    assert_eq!(sessions[0].cwd, PathBuf::from("/work/app"));
    assert_eq!(sessions[0].git_branch, "feature");
    assert!(sessions[0].size_bytes > 0);
    assert_eq!(sessions[0].history_mode, CodexHistoryMode::Legacy);
}

#[test]
fn codex_history_text_does_not_become_the_session_name() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("history.jsonl"),
        r#"{"session_id":"session-1","text":"first user message"}"#,
    )
    .unwrap();

    assert_eq!(
        codex_native_titles(directory.path()).unwrap()["session-1"],
        "session-1"
    );
}

#[test]
fn codex_listing_matches_native_interactive_visibility_and_title_priority() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    let archived = directory.path().join("archived_sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&archived).unwrap();
    let interactive_id = "019feb6c-6b55-7111-a210-6d85ee0772cd";
    for (path, id, source) in [
        (
            sessions.join("interactive.jsonl"),
            interactive_id,
            json!("cli"),
        ),
        (
            sessions.join("exec.jsonl"),
            "019feb6c-6b55-7111-a210-6d85ee0772ce",
            json!("exec"),
        ),
        (
            sessions.join("subagent.jsonl"),
            "019feb6c-6b55-7111-a210-6d85ee0772cf",
            json!({"subagent": {"thread_spawn": {"parent_thread_id": interactive_id}}}),
        ),
        (
            sessions.join("ephemeral.jsonl"),
            "019feb6c-6b55-7111-a210-6d85ee0772d1",
            json!("cli"),
        ),
        (
            archived.join("archived.jsonl"),
            "019feb6c-6b55-7111-a210-6d85ee0772d0",
            json!("cli"),
        ),
    ] {
        let ephemeral = path.ends_with("ephemeral.jsonl");
        fs::write(
            &path,
            json!({
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "source": source,
                    "cwd": "/work/app",
                    "ephemeral": ephemeral
                }
            })
            .to_string(),
        )
        .unwrap();
    }
    fs::write(
        directory.path().join("history.jsonl"),
        json!({"session_id": interactive_id, "text": "Generated history title"}).to_string(),
    )
    .unwrap();
    fs::write(
        directory.path().join("session_index.jsonl"),
        json!({"id": interactive_id, "thread_name": "Explicit native title"}).to_string(),
    )
    .unwrap();

    let listed = list_codex_sessions(directory.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].native_session_id, interactive_id);
    assert_eq!(listed[0].title, "Explicit native title");
}

#[test]
fn codex_listing_uses_native_index_order_and_includes_all_persisted_threads() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let connection = rusqlite::Connection::open(directory.path().join("state_5.sqlite")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    updated_at INTEGER,
                    name TEXT,
                    title TEXT,
                    cwd TEXT,
                    git_branch TEXT,
                    archived INTEGER,
                    source TEXT,
                    preview TEXT,
                    history_mode TEXT
                );",
        )
        .unwrap();
    for index in 0..30_u64 {
        let id = format!("019feb6c-6b55-7111-a210-{index:012x}");
        let rollout = sessions.join(format!("rollout-{id}.jsonl"));
        fs::write(&rollout, "indexed").unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 'cli', 'visible', 'paginated')",
                rusqlite::params![
                    id,
                    rollout.to_string_lossy(),
                    index as i64,
                    (index == 29).then_some("Explicit newest title"),
                    format!("Generated {index}"),
                    "/work/app",
                    "feature"
                ],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO threads VALUES \
                 (?1, NULL, 100, 'Ephemeral', 'Ephemeral', '/work/app', 'HEAD', 0, 'cli', \
                  'visible', 'legacy')",
            ["019feb6c-6b55-7111-a210-ffffffffffff"],
        )
        .unwrap();
    drop(connection);

    let listed = list_codex_sessions(directory.path()).unwrap();
    assert_eq!(listed.len(), 30);
    assert_eq!(listed[0].title, "Explicit newest title");
    assert_eq!(listed[0].history_mode, CodexHistoryMode::Paginated);
    assert!(listed[0].modified_at > listed[1].modified_at);
    assert_eq!(listed[29].title, "Generated 0");
    assert!(listed.iter().all(|session| session.title != "Ephemeral"));
}

/// Hel mirrors Codex's own archive flag one way: the thread is listed and
/// flagged so the resume dialog can hide it, and `--latest` still follows
/// Codex's default view. Nothing is written back to Codex.
#[test]
fn codex_listing_flags_natively_archived_threads_and_latest_skips_them() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let database = directory.path().join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT,
                    updated_at INTEGER,
                    name TEXT,
                    title TEXT,
                    cwd TEXT,
                    git_branch TEXT,
                    archived INTEGER,
                    source TEXT,
                    preview TEXT,
                    history_mode TEXT
                );",
        )
        .unwrap();
    let live_id = "019feb6c-6b55-7111-a210-000000000001";
    let archived_id = "019feb6c-6b55-7111-a210-000000000002";
    for (id, updated_at, archived) in [(live_id, 10_i64, 0), (archived_id, 20_i64, 1)] {
        let rollout = sessions.join(format!("rollout-{id}.jsonl"));
        fs::write(&rollout, "indexed").unwrap();
        connection
                .execute(
                    "INSERT INTO threads VALUES \
                     (?1, ?2, ?3, ?4, NULL, '/work/app', 'main', ?5, 'cli', 'visible', 'paginated')",
                    rusqlite::params![
                        id,
                        rollout.to_string_lossy(),
                        updated_at,
                        format!("Thread {id}"),
                        archived
                    ],
                )
                .unwrap();
    }
    let modified_before = fs::metadata(&database).unwrap().modified().unwrap();
    drop(connection);

    let listed = list_codex_sessions(directory.path()).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].native_session_id, archived_id);
    assert!(listed[0].natively_archived);
    assert_eq!(listed[1].native_session_id, live_id);
    assert!(!listed[1].natively_archived);

    // `--latest` follows Codex's default view and skips what it archived.
    let latest = locate_codex_session(directory.path(), &CodexSessionSelection::Latest).unwrap();
    assert_eq!(latest.native_session_id, live_id);
    // Naming an archived thread still finds it, so it stays importable.
    let by_id = locate_codex_session(
        directory.path(),
        &CodexSessionSelection::NativeSessionId(archived_id.into()),
    )
    .unwrap();
    assert!(by_id.natively_archived);

    assert_eq!(
        fs::metadata(&database).unwrap().modified().unwrap(),
        modified_before,
        "Hel must never write Codex's own database"
    );
}

#[test]
fn codex_scan_reports_progress_and_emits_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    for (name, session_id) in [
        ("first.jsonl", "019feb6c-6b55-7111-a210-6d85ee0772cd"),
        ("second.jsonl", "019feb6c-6b55-7111-a210-6d85ee0772ce"),
    ] {
        fs::write(
            sessions.join(name),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{session_id}"}}}}"#),
        )
        .unwrap();
    }
    let mut updates = Vec::new();
    scan_codex_sessions(directory.path(), |progress| {
        updates.push((
            progress.scanned,
            progress.total,
            progress.session.map(|session| session.modified_at),
        ));
    })
    .unwrap();

    assert_eq!(updates.len(), 3);
    assert_eq!((updates[0].0, updates[0].1), (0, 2));
    assert!(updates[0].2.is_none());
    assert_eq!((updates[1].0, updates[1].1), (1, 2));
    assert_eq!((updates[2].0, updates[2].1), (2, 2));
    assert!(updates[1].2 >= updates[2].2);
}

#[test]
fn claude_listing_uses_ai_title_and_native_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let rollout = directory
        .path()
        .join("projects/work/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(
            &rollout,
            concat!(
                r#"{"type":"user","cwd":"/work/app","gitBranch":"feature","message":{"content":"fallback"}}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Native Claude title"}"#,
            ),
        )
        .unwrap();

    let sessions = list_claude_sessions(directory.path()).unwrap();
    assert_eq!(sessions[0].title, "Native Claude title");
    assert_eq!(sessions[0].cwd, PathBuf::from("/work/app"));
    assert_eq!(sessions[0].git_branch, "feature");
    assert!(sessions[0].size_bytes > 0);
}

#[test]
fn claude_listing_matches_native_title_priority_and_visibility() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("projects/work");
    fs::create_dir_all(&project).unwrap();
    fs::write(
            project.join("interactive.jsonl"),
            concat!(
                r#"{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"fallback"}}"#,
                "\n",
                r#"{"type":"agent-name","agentName":"renamed-agent"}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"Native custom title"}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Generated title"}"#,
            ),
        )
        .unwrap();
    fs::write(
            project.join("print-mode.jsonl"),
            r#"{"type":"user","entrypoint":"sdk-cli","cwd":"/work/app","message":{"content":"<local-command-caveat>usage poll</local-command-caveat>"}}"#,
        )
        .unwrap();
    fs::write(
            project.join("sidechain.jsonl"),
            r#"{"type":"user","entrypoint":"cli","isSidechain":true,"cwd":"/work/app","message":{"content":"subagent"}}"#,
        )
        .unwrap();

    let sessions = list_claude_sessions(directory.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, "Native custom title");
    assert_eq!(sessions[0].native_session_id, "interactive");
}

#[test]
fn claude_listing_prefers_agent_name_to_generated_title() {
    let directory = tempfile::tempdir().unwrap();
    let rollout = directory.path().join("session.jsonl");
    fs::write(
            &rollout,
            concat!(
                r#"{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"fallback"}}"#,
                "\n",
                r#"{"type":"agent-name","agentName":"restic-cleanup"}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Clean up fulldata directory organization"}"#,
            ),
        )
        .unwrap();

    let (title, _, _) = claude_native_metadata(&rollout).unwrap().unwrap();
    assert_eq!(title, "restic-cleanup");
}

#[test]
fn claude_first_user_message_is_not_used_as_a_session_name_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let rollout = directory.path().join("session.jsonl");
    fs::write(
            &rollout,
            r#"{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{"content":"first user message"}}"#,
        )
        .unwrap();

    let (title, _, _) = claude_native_metadata(&rollout).unwrap().unwrap();
    assert_eq!(title, "Untitled session");
}

#[test]
fn claude_listing_uses_native_all_projects_limit() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("projects/work");
    fs::create_dir_all(&project).unwrap();
    for index in 0..51 {
        fs::write(
                project.join(format!("session-{index:02}.jsonl")),
                format!(
                    r#"{{"type":"user","entrypoint":"cli","cwd":"/work/app","message":{{"content":"Session {index}"}}}}"#
                ),
            )
            .unwrap();
    }

    assert_eq!(list_claude_sessions(directory.path()).unwrap().len(), 50);
    let oldest = locate_claude_session(
        directory.path(),
        &ClaudeSessionSelection::NativeSessionId("session-00".into()),
    )
    .unwrap();
    assert_eq!(oldest.native_session_id, "session-00");
}

#[test]
fn kimi_wire_projects_user_prompt_and_text_without_thought() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("agents/main")).unwrap();
    fs::write(
        directory.path().join("state.json"),
        r#"{"workDir":"/work/app"}"#,
    )
    .unwrap();
    let wire_path = directory.path().join("agents/main/wire.jsonl");
    fs::write(
            &wire_path,
            concat!(
                r#"{"type":"turn.prompt","origin":{"kind":"user"},"input":[{"type":"text","text":"<mj-project-memory>private</mj-project-memory>"},{"type":"text","text":"first prompt"}]}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"hidden"}}}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"first reply"}}}"#,
                "\n",
                r#"{"type":"turn.steer","origin":{"kind":"user"},"input":[{"type":"text","text":"follow up"}]}"#,
                "\n",
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"second reply"}}}"#,
                "\n",
            ),
        )
        .unwrap();
    let expected_fallback =
        DateTime::<Utc>::from(fs::metadata(&wire_path).unwrap().modified().unwrap())
            .timestamp_millis();

    let transcript = read_kimi_transcript(directory.path()).unwrap();
    assert_eq!(transcript.cwd, PathBuf::from("/work/app"));
    assert!(matches!(
        &transcript.events[0].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "first prompt"
    ));
    assert_eq!(
        agent_text(&transcript.events[1].event).as_deref(),
        Some("first reply")
    );
    assert!(matches!(
        transcript.events[2].event,
        WorkerEvent::TurnCompleted
    ));
    assert!(matches!(
        &transcript.events[3].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "follow up"
    ));
    assert_eq!(
        agent_text(&transcript.events[4].event).as_deref(),
        Some("second reply")
    );
    assert!(matches!(
        transcript.events[5].event,
        WorkerEvent::TurnCompleted
    ));
    assert!(
        transcript
            .events
            .iter()
            .all(|event| event.recorded_at_ms == Some(expected_fallback))
    );
}

/// Synthetic `chat_history.jsonl`, modeled on the shape Grok Build writes:
/// internally tagged items, user content as typed parts, reasoning
/// summaries beside the assistant turn, and `search_replace` tool calls
/// paired with their results.
fn grok_session(directory: &Path, cwd: &str, history: &str) -> PathBuf {
    let session = directory.join("sessions/%2Fwork%2Fapp/01a00c3a-553f-71e0-95ab-aa04396d3ad7");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("summary.json"),
        json!({
            "info": {"id": "01a00c3a-553f-71e0-95ab-aa04396d3ad7", "cwd": cwd},
            "session_summary": "",
            "num_chat_messages": 4,
            "current_model_id": "grok-4.6",
            "grok_home": "/home/me/.grok",
        })
        .to_string(),
    )
    .unwrap();
    fs::write(session.join("chat_history.jsonl"), history).unwrap();
    session
}

#[test]
fn grok_chat_history_projects_prompts_thoughts_and_replies() {
    let directory = tempfile::tempdir().unwrap();
    let history = concat!(
        r#"{"type":"system","content":"You are Grok."}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>ignore me</system-reminder>"}],"synthetic_reason":"system_reminder"}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"<mj-project-memory>private</mj-project-memory>"},{"type":"text","text":"first prompt"}],"prompt_index":0}"#,
        "\n",
        r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking it over"}],"encrypted_content":"opaque"}"#,
        "\n",
        r#"{"type":"assistant","content":"first reply","model_id":"grok-4.6"}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"follow up"}],"prompt_index":1}"#,
        "\n",
        r#"{"type":"assistant","content":"second reply","model_id":"grok-4.6"}"#,
        "\n",
    );
    let session = grok_session(directory.path(), "/work/app", history);
    let expected_fallback = DateTime::<Utc>::from(
        fs::metadata(session.join("chat_history.jsonl"))
            .unwrap()
            .modified()
            .unwrap(),
    )
    .timestamp_millis();

    let transcript = read_grok_transcript(&session).unwrap();

    assert_eq!(transcript.cwd, PathBuf::from("/work/app"));
    assert!(matches!(
        &transcript.events[0].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "first prompt"
    ));
    assert_eq!(
        agent_text(&transcript.events[1].event).as_deref(),
        Some("thinking it over")
    );
    assert_eq!(
        agent_text(&transcript.events[2].event).as_deref(),
        Some("first reply")
    );
    assert!(matches!(
        transcript.events[3].event,
        WorkerEvent::TurnCompleted
    ));
    assert!(matches!(
        &transcript.events[4].event,
        WorkerEvent::PromptAccepted { text, .. } if text == "follow up"
    ));
    assert_eq!(
        agent_text(&transcript.events[5].event).as_deref(),
        Some("second reply")
    );
    assert!(matches!(
        transcript.events[6].event,
        WorkerEvent::TurnCompleted
    ));
    assert_eq!(transcript.events.len(), 7);
    assert!(
        transcript
            .events
            .iter()
            .all(|event| event.recorded_at_ms == Some(expected_fallback))
    );
}

#[test]
fn grok_reasoning_and_message_chunks_keep_their_own_update_kinds() {
    let directory = tempfile::tempdir().unwrap();
    let history = concat!(
        r#"{"type":"user","content":[{"type":"text","text":"go"}]}"#,
        "\n",
        r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"a"},{"type":"summary_text","text":"b"}]}"#,
        "\n",
        r#"{"type":"assistant","content":"done"}"#,
        "\n",
    );
    let session = grok_session(directory.path(), "/work/app", history);

    let transcript = read_grok_transcript(&session).unwrap();

    let update = |index: usize| match &transcript.events[index].event {
        WorkerEvent::Adapter { payload, .. } => payload
            .pointer("/update/sessionUpdate")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned(),
        other => panic!("expected an adapter event, got {other:?}"),
    };
    assert_eq!(update(1), "agent_thought_chunk");
    assert_eq!(
        agent_text(&transcript.events[1].event).as_deref(),
        Some("a\nb")
    );
    assert_eq!(update(2), "agent_message_chunk");
}

#[test]
fn grok_compaction_before_recoverable_history_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let history = concat!(
        r#"{"type":"user","content":[{"type":"text","text":"summary of earlier work"}],"synthetic_reason":"compaction_meta"}"#,
        "\n",
        r#"{"type":"user","content":[{"type":"text","text":"carry on"}]}"#,
        "\n",
    );
    let session = grok_session(directory.path(), "/work/app", history);

    let error = read_grok_transcript(&session).unwrap_err();

    assert!(
        format!("{error:#}").contains("compaction artifact before recoverable raw history"),
        "{error:#}"
    );
}

#[test]
fn grok_edited_paths_need_a_completed_search_replace_call() {
    let directory = tempfile::tempdir().unwrap();
    let history = concat!(
        r#"{"type":"user","content":[{"type":"text","text":"edit things"}]}"#,
        "\n",
        r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-1","name":"search_replace","arguments":"{\"file_path\":\"/work/app/src/lib.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}"},{"id":"call-2","name":"search_replace","arguments":"{\"file_path\":\"/work/app/never-ran.rs\"}"},{"id":"call-3","name":"read_file","arguments":"{\"file_path\":\"/work/app/README.md\"}"}]}"#,
        "\n",
        r#"{"type":"tool_result","tool_call_id":"call-1","content":"ok"}"#,
        "\n",
        r#"{"type":"tool_result","tool_call_id":"call-3","content":"ok"}"#,
        "\n",
    );
    let session = grok_session(directory.path(), "/work/app", history);

    let transcript = read_grok_transcript(&session).unwrap();

    // Only the completed write is reported: an unanswered call never ran,
    // and a read is not an edit.
    assert_eq!(
        transcript.edited_paths,
        [PathBuf::from("/work/app/src/lib.rs")]
    );
}

#[test]
fn grok_scan_lists_sessions_and_skips_the_shared_search_index() {
    let directory = tempfile::tempdir().unwrap();
    let history = concat!(
        r#"{"type":"user","content":[{"type":"text","text":"the very first thing\nand a second line"}]}"#,
        "\n",
    );
    grok_session(directory.path(), "/work/app", history);
    let sessions = directory.path().join("sessions");
    fs::write(sessions.join("session_search.sqlite"), b"index").unwrap();
    fs::write(
        sessions.join("%2Fwork%2Fapp").join("summary.json.lock"),
        b"",
    )
    .unwrap();

    let listed = list_grok_sessions(directory.path()).unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].native_session_id,
        "01a00c3a-553f-71e0-95ab-aa04396d3ad7"
    );
    assert_eq!(listed[0].cwd, PathBuf::from("/work/app"));
    // A user message is conversation content, not a session-name fallback.
    assert_eq!(listed[0].title, listed[0].native_session_id);

    let located = locate_grok_session(directory.path(), &GrokSessionSelection::Latest).unwrap();
    assert_eq!(located.native_session_id, listed[0].native_session_id);
}

#[test]
fn grok_cwd_directory_names_decode_back_to_their_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    let encoded = directory.path().join("%2Fwork%2Fmy%20app");
    fs::create_dir_all(&encoded).unwrap();
    assert_eq!(
        grok_decode_cwd_dirname(&encoded),
        Some(PathBuf::from("/work/my app"))
    );

    // The hash form is not reversible, so the recorded `.cwd` decides.
    let hashed = directory.path().join("app-0123456789abcdef");
    fs::create_dir_all(&hashed).unwrap();
    assert_eq!(grok_decode_cwd_dirname(&hashed), None);
    fs::write(hashed.join(".cwd"), "/work/a-very-long-path\n").unwrap();
    assert_eq!(
        grok_decode_cwd_dirname(&hashed),
        Some(PathBuf::from("/work/a-very-long-path"))
    );
}

#[test]
fn kimi_locator_retains_its_native_session_directory_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let id = "90c30a64-54f7-4261-90f1-e75b1c14311c";
    let session = directory
        .path()
        .join("sessions/project")
        .join(format!("session_{id}"));
    fs::create_dir_all(&session).unwrap();
    fs::write(
        directory.path().join("session_index.jsonl"),
        json!({
            "sessionId": format!("session_{id}"),
            "sessionDir": session,
            "workDir": "/work/app"
        })
        .to_string(),
    )
    .unwrap();

    let located = locate_kimi_session(
        directory.path(),
        &KimiSessionSelection::NativeSessionId(format!("session_{id}")),
    )
    .unwrap();
    assert_eq!(located.native_session_id, format!("session_{id}"));
    assert_eq!(
        located.session_path.canonicalize().unwrap(),
        session.canonicalize().unwrap()
    );
}

#[test]
fn kimi_listing_matches_native_index_visibility_and_title() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("sessions/project");
    fs::create_dir_all(&workspace).unwrap();
    let visible = workspace.join("session_visible");
    let archived = workspace.join("session_archived");
    let deleted = workspace.join("session_deleted");
    let unindexed = workspace.join("session_unindexed");
    for session in [&visible, &archived, &deleted, &unindexed] {
        fs::create_dir_all(session).unwrap();
    }
    fs::write(
        visible.join("state.json"),
        r#"{"workDir":"/work/native","title":"Generated","customTitle":"Native custom title"}"#,
    )
    .unwrap();
    fs::write(
        archived.join("state.json"),
        r#"{"workDir":"/work/app","title":"Archived","archived":true}"#,
    )
    .unwrap();
    let index = [
        json!({"sessionId":"session_visible","sessionDir":visible,"workDir":"/work/index"}),
        json!({"sessionId":"session_archived","sessionDir":archived,"workDir":"/work/app"}),
        json!({"sessionId":"session_deleted","sessionDir":deleted,"workDir":"/work/app"}),
        json!({"sessionId":"session_deleted","deleted":true}),
    ]
    .into_iter()
    .map(|record| record.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(directory.path().join("session_index.jsonl"), index).unwrap();

    let listed = list_kimi_sessions(directory.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].native_session_id, "session_visible");
    assert_eq!(listed[0].title, "Native custom title");
    assert_eq!(listed[0].cwd, PathBuf::from("/work/native"));
}

fn agent_text(event: &WorkerEvent) -> Option<String> {
    let WorkerEvent::Adapter { payload, .. } = event else {
        return None;
    };
    payload
        .pointer("/update/content/text")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}
