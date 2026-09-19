use super::launch::*;
use super::*;
use crate::controller::test_support::{IsolatedTest, test_name};
use mj_core::hex::lower_hex;
use mj_core::targets::ProcessExecutor;

use anyhow::Result;

use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec, SshTarget};
use mj_core::config::ExecutionPolicy;

use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::BTreeMap;

use std::path::{Path, PathBuf};

/// A `reqwest::blocking::Client` owns a private Tokio runtime that it drops
/// with the client. The session-move lifecycle and the sub-agent spawn path
/// drive catalog staging inside a Tokio context (`Handle::block_on` and a
/// runtime worker respectively), where dropping that runtime panics with
/// "Cannot drop a runtime in a context where blocking is not allowed" and
/// strands the session. The HTTP helper must run off that context and
/// return an ordinary error instead of panicking.
#[test]
fn fetch_catalog_over_https_does_not_panic_inside_a_runtime_context() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Port 1 refuses the connection at once, so no network is needed; the
    // point is that the client's runtime is created and dropped without a
    // panic while a Tokio context is entered on this thread.
    let result = runtime
        .block_on(async { fetch_catalog_over_https("http://127.0.0.1:1/models", "unused-key") });
    assert!(
        result.is_err(),
        "expected a connection error, got {result:?}"
    );
}

/// The session's stored choice decides, with the global setting as the
/// fallback, and a child never gets the tools whatever either says.
#[test]
fn the_session_choice_decides_whether_mjolnir_replaces_native_delegation() {
    let claude = |choice| {
        let mut session = crate::controller::test_support::checkpoint_test_session("s-1");
        session.harness_kind = HarnessKind::Claude;
        session.mjolnir_subagents = choice;
        session
    };

    assert!(!subagent_tools_enabled(&claude(Some(false)), true, false));
    assert!(subagent_tools_enabled(&claude(Some(true)), false, false));
    assert!(subagent_tools_enabled(&claude(None), true, false));
    assert!(!subagent_tools_enabled(&claude(None), false, false));
    assert!(!subagent_tools_enabled(&claude(Some(true)), true, true));

    let mut grok = claude(Some(true));
    grok.harness_kind = HarnessKind::Grok;
    assert!(!subagent_tools_enabled(&grok, true, false));

    let mut codex = claude(None);
    codex.harness_kind = HarnessKind::Codex;
    assert!(subagent_tools_enabled(&codex, true, false));
    codex.mjolnir_subagents = Some(false);
    assert!(!subagent_tools_enabled(&codex, true, false));
}

#[cfg(unix)]
#[test]
fn node_preflight_checks_missing_old_and_supported_tools_on_profile_path() {
    let directory = tempfile::tempdir().unwrap();
    let profile = HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: directory.path().into(),
        environment: std::collections::BTreeMap::from([(
            "PATH".into(),
            directory.path().to_string_lossy().into_owned(),
        )]),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    let check = || {
        preflight_harness(
            &mj_core::config::TargetTemplate::LocalBare,
            &profile,
            &ProcessExecutor,
        )
    };
    let write_tool = |name: &str, body: &str| {
        crate::controller::test_support::install_fake_command(
            directory.path(),
            name,
            &format!("#!/bin/sh\n{body}\n"),
        );
    };
    assert!(format!("{:#}", check().unwrap_err()).contains("Node.js is missing"));
    write_tool("node", "exit 1");
    assert!(format!("{:#}", check().unwrap_err()).contains("Node.js 22 or newer is required"));
    write_tool("node", "exit 0");
    assert!(format!("{:#}", check().unwrap_err()).contains("npm is missing or unusable"));
    write_tool("npm", "exit 0");
    check().unwrap();
}

#[test]
fn a_stored_setup_token_reaches_only_claude_workers_that_do_not_set_their_own() {
    use mj_core::config::HarnessKind;
    use mj_core::credentials::{CLAUDE_OAUTH_TOKEN_ENV, write_claude_oauth_token};

    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("profiles/claude/claude-oauth-token");
    let missing = directory.path().join("profiles/absent/claude-oauth-token");
    write_claude_oauth_token(&token_path, b"sk-ant-oat01-stored").unwrap();

    let mut claude = BTreeMap::new();
    apply_claude_setup_token(&mut claude, HarnessKind::Claude, &token_path);
    assert_eq!(
        claude.get(CLAUDE_OAUTH_TOKEN_ENV).map(String::as_str),
        Some("sk-ant-oat01-stored")
    );

    // Every other harness ignores the variable, so it must not appear.
    for kind in HarnessKind::ALL
        .into_iter()
        .filter(|kind| *kind != HarnessKind::Claude)
    {
        let mut environment = BTreeMap::new();
        apply_claude_setup_token(&mut environment, kind, &token_path);
        assert!(environment.is_empty(), "{kind:?} must not read the token");
    }

    // A profile that sets the variable itself stays authoritative.
    let mut overridden = BTreeMap::from([(
        CLAUDE_OAUTH_TOKEN_ENV.to_owned(),
        "profile-token".to_owned(),
    )]);
    apply_claude_setup_token(&mut overridden, HarnessKind::Claude, &token_path);
    assert_eq!(
        overridden.get(CLAUDE_OAUTH_TOKEN_ENV).map(String::as_str),
        Some("profile-token")
    );

    // A profile with no stored token launches exactly as before.
    let mut without = BTreeMap::new();
    apply_claude_setup_token(&mut without, HarnessKind::Claude, &missing);
    assert!(without.is_empty());
}

#[test]
fn packaged_worker_names_match_release_archives() {
    let directory = Path::new("/opt/hel/bin");
    assert_eq!(
        packaged_worker_binary_path(directory, "x86_64-unknown-linux-musl"),
        directory.join("mj-worker-x86_64-unknown-linux-musl")
    );
    assert_eq!(
        packaged_worker_binary_path(directory, "aarch64-unknown-linux-musl"),
        directory.join("mj-worker-aarch64-unknown-linux-musl")
    );
}

#[test]
fn pinned_snapshot_keeps_native_and_portable_sources_stable() {
    let directory = tempfile::tempdir().unwrap();
    let native = directory.path().join("native-worker");
    let x86 = directory.path().join("x86-worker");
    let arm = directory.path().join("arm-worker");
    std::fs::write(&native, b"native bytes").unwrap();
    std::fs::write(&x86, b"x86 bytes").unwrap();
    std::fs::write(&arm, b"arm bytes").unwrap();
    let cache = directory.path().join("cache");
    let snapshot = WorkerBinarySourceSnapshot::capture(&cache, |arch, requirement| {
        let path = match requirement {
            WorkerBinaryRequirement::LocalHost => &native,
            WorkerBinaryRequirement::PortableLinux if arch == "x86_64" => &x86,
            WorkerBinaryRequirement::PortableLinux => &arm,
        };
        Ok(WorkerBinaryAvailability::Local {
            path: path.clone(),
            source: format!("{arch}-{requirement:?}"),
        })
    });

    let native = snapshot
        .resolve(std::env::consts::ARCH, WorkerBinaryRequirement::LocalHost)
        .unwrap();
    let x86 = snapshot
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap();
    let arm = snapshot
        .resolve("aarch64", WorkerBinaryRequirement::PortableLinux)
        .unwrap();
    let WorkerBinaryAvailability::Local { path: native, .. } = native else {
        panic!("native source should be local");
    };
    let WorkerBinaryAvailability::Local { path: x86, .. } = x86 else {
        panic!("x86 source should be local");
    };
    let WorkerBinaryAvailability::Local { path: arm, .. } = arm else {
        panic!("arm source should be local");
    };
    assert_eq!(std::fs::read(native).unwrap(), b"native bytes");
    assert_eq!(std::fs::read(x86).unwrap(), b"x86 bytes");
    assert_eq!(std::fs::read(arm).unwrap(), b"arm bytes");
}

#[test]
fn pinned_snapshot_survives_source_replacement_and_missing_candidate_install() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, b"before").unwrap();
    let cache = directory.path().join("cache");
    let resolve_source = |_: &str, _: WorkerBinaryRequirement| {
        Ok(WorkerBinaryAvailability::Local {
            path: source.clone(),
            source: "test source".into(),
        })
    };
    let pinned = WorkerBinarySourceSnapshot::capture(&cache, resolve_source);

    std::fs::write(&source, b"in-place mutation").unwrap();
    let WorkerBinaryAvailability::Local { path, .. } = pinned
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be local");
    };
    assert_eq!(std::fs::read(path).unwrap(), b"before");

    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, b"after").unwrap();
    std::fs::rename(replacement, &source).unwrap();
    let WorkerBinaryAvailability::Local { path, .. } = pinned
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be local");
    };
    assert_eq!(std::fs::read(path).unwrap(), b"before");
    let fresh_replaced = WorkerBinarySourceSnapshot::capture(&cache, resolve_source);
    let WorkerBinaryAvailability::Local { path, .. } = fresh_replaced
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be local");
    };
    assert_eq!(std::fs::read(path).unwrap(), b"after");

    let missing = directory.path().join("missing-worker");
    let missing_snapshot = WorkerBinarySourceSnapshot::capture(&cache, {
        let missing = missing.clone();
        move |_: &str, _: WorkerBinaryRequirement| {
            if missing.is_file() {
                Ok(WorkerBinaryAvailability::Local {
                    path: missing.clone(),
                    source: "new source".into(),
                })
            } else {
                Err(anyhow::anyhow!("candidate is unavailable"))
            }
        }
    });
    std::fs::write(&missing, b"now installed").unwrap();
    assert!(
        missing_snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .is_err()
    );
    let fresh_snapshot = WorkerBinarySourceSnapshot::capture(&cache, {
        let missing = missing.clone();
        move |_: &str, _: WorkerBinaryRequirement| {
            Ok(WorkerBinaryAvailability::Local {
                path: missing.clone(),
                source: "new source".into(),
            })
        }
    });
    assert!(
        fresh_snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .is_ok()
    );

    let remote_url = std::cell::RefCell::new("https://old.example/{target}".to_owned());
    let remote_snapshot =
        WorkerBinarySourceSnapshot::capture(&directory.path().join("remote-cache"), |arch, _| {
            Ok(WorkerBinaryAvailability::Remote {
                url: remote_url.borrow().replace("{target}", arch),
                sha256: "a".repeat(64),
                triple: format!("{arch}-unknown-linux-musl"),
            })
        });
    *remote_url.borrow_mut() = "https://new.example/{target}".into();
    let WorkerBinaryAvailability::Remote { url, .. } = remote_snapshot
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be remote");
    };
    assert_eq!(url, "https://old.example/x86_64");

    let blocked_cache = directory.path().join("blocked-cache");
    std::fs::write(&blocked_cache, b"not a directory").unwrap();
    let failed_snapshot = WorkerBinarySourceSnapshot::capture(&blocked_cache, resolve_source);
    assert!(
        failed_snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .is_err()
    );
}

#[test]
fn dev_checkout_prefers_the_dedicated_musl_worker() {
    let controller = PathBuf::from("target/debug/mj");
    let musl = PathBuf::from("target/worker/x86_64-unknown-linux-musl/debug/mj-worker");
    let shared_target_worker = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj-worker");
    let legacy = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj");
    let present = [
        controller.clone(),
        musl.clone(),
        shared_target_worker,
        legacy,
    ];
    let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
        present.iter().any(|p| p == path)
    });
    assert_eq!(
        selected,
        Some((musl, "isolated development musl worker")),
        "the dedicated worker must win over legacy artifacts"
    );
}

#[test]
fn local_bare_may_use_a_native_worker_beside_the_controller() {
    let controller = PathBuf::from("target/debug/mj");
    let worker = PathBuf::from("target/debug/mj-worker");
    let selected = worker_binary_prerequisite_for_current(
        std::env::consts::ARCH,
        WorkerBinaryRequirement::LocalHost,
        &controller,
        &|path| path == controller || path == worker,
    )
    .unwrap();
    assert_eq!(
        selected,
        WorkerBinaryAvailability::Local {
            path: worker,
            source: "native worker beside mj".into(),
        }
    );
}

#[test]
fn local_bare_prefers_the_isolated_native_development_worker() {
    let controller = PathBuf::from("target/debug/mj");
    let worker = PathBuf::from("target/worker/debug/mj-worker");
    let packaged = PathBuf::from("target/debug/mj-worker");
    let selected = worker_binary_prerequisite_for_current(
        std::env::consts::ARCH,
        WorkerBinaryRequirement::LocalHost,
        &controller,
        &|path| path == controller || path == worker || path == packaged,
    )
    .unwrap();
    assert_eq!(
        selected,
        WorkerBinaryAvailability::Local {
            path: worker,
            source: "isolated native development worker".into(),
        }
    );
}

#[cfg(target_os = "linux")]
#[test]
fn replaced_dev_controller_still_finds_its_musl_sibling() {
    let controller = PathBuf::from("target/debug/mj (deleted)");
    let musl = PathBuf::from("target/x86_64-unknown-linux-musl/debug/mj");
    let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
        path == musl
    });

    assert_eq!(selected, Some((musl, "development musl sibling")));
}

#[cfg(target_os = "linux")]
#[test]
fn replaced_dev_controller_never_selects_the_new_glibc_controller_as_its_worker() {
    let controller = PathBuf::from("target/debug/mj (deleted)");
    let replacement = PathBuf::from("target/debug/mj");
    let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
        path == replacement
    });

    assert_eq!(selected, None);
}

/// A configured container template for the preflight tests. Only the
/// platform matters here; the rest is the smallest valid template.
fn container_template(platform: Option<&str>) -> mj_core::config::ContainerTemplate {
    mj_core::config::ContainerTemplate {
        build_cache: None,
        image: "example.invalid/mj-test:latest".into(),
        pull_policy: Default::default(),
        platform: platform.map(str::to_owned),
        cpus: None,
        memory: None,
        environment: BTreeMap::new(),
        workspace_storage: Default::default(),
    }
}

fn ssh_connection() -> mj_core::config::SshConnection {
    mj_core::config::SshConnection {
        host: "builder".into(),
        user: Some("dev".into()),
        identity_file: None,
        extra_args: Vec::new(),
    }
}

#[test]
fn recovery_workspace_uses_the_launch_directory_for_bare_targets_only() {
    let cwd = PathBuf::from("/workspace/session/project");
    let local = worker_workspace_for_recovery(
        &targets::TargetLocator::LocalBare {
            worker_root: "/workspace/session/worker".into(),
        },
        &cwd,
    )
    .expect("local bare targets need a workspace probe");
    assert_eq!(local.directory, cwd);
    assert_eq!(local.target, mj_core::state::ManagedWorktreeTarget::Local);

    let remote = worker_workspace_for_recovery(
        &targets::TargetLocator::SshBare {
            worker_id: None,
            ssh: SshTarget {
                destination: "dev@builder".into(),
                ssh_args: vec!["-oBatchMode=yes".into()],
            },
            workspace: "/workspace/session".into(),
        },
        &cwd,
    )
    .expect("SSH bare targets need a workspace probe");
    assert_eq!(remote.directory, cwd);
    assert_eq!(
        remote.target,
        mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: "dev@builder".into(),
            ssh_args: vec!["-oBatchMode=yes".into()],
        }
    );

    assert!(
        worker_workspace_for_recovery(
            &targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: "container".into(),
                workspace_storage: Default::default(),
            },
            &cwd,
        )
        .is_none()
    );
    assert!(
        worker_workspace_for_recovery(
            &targets::TargetLocator::AwsEc2 {
                profile: "default".into(),
                region: "us-east-1".into(),
                instance_id: "i-test".into(),
                ssh: SshTarget {
                    destination: "dev@builder".into(),
                    ssh_args: Vec::new(),
                },
                workspace: "/workspace/session".into(),
            },
            &cwd,
        )
        .is_none()
    );
}

#[test]
fn preflight_reads_the_architecture_a_template_names() {
    use mj_core::config::TargetTemplate;

    for (platform, expected) in [
        ("linux/arm64", "aarch64"),
        ("linux/arm64/v8", "aarch64"),
        ("linux/amd64", "x86_64"),
        ("aarch64", "aarch64"),
    ] {
        assert_eq!(
            preflight_architectures(&TargetTemplate::LocalPodman {
                container: container_template(Some(platform)),
            }),
            vec![expected],
            "platform {platform}"
        );
    }
    // A named platform decides a remote container target too, so a resume
    // onto an arm64 container never asks about the host's architecture.
    assert_eq!(
        preflight_architectures(&TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container_template(Some("linux/arm64")),
        }),
        vec!["aarch64"]
    );
}

#[test]
fn preflight_uses_the_host_architecture_for_a_local_target() {
    use mj_core::config::TargetTemplate;

    for template in [
        TargetTemplate::LocalBare,
        TargetTemplate::LocalPodman {
            container: container_template(None),
        },
        TargetTemplate::LocalDocker {
            container: container_template(None),
        },
        TargetTemplate::AppleContainer {
            container: container_template(None),
        },
    ] {
        assert_eq!(
            preflight_architectures(&template),
            vec![std::env::consts::ARCH],
            "{template:?}"
        );
    }
}

#[test]
fn preflight_accepts_either_linux_architecture_for_a_remote_target() {
    use mj_core::config::TargetTemplate;

    // Nothing in the configuration says what a remote machine runs, so the
    // preflight passes as long as one architecture could be served; the
    // real architecture is read from the live target during provisioning.
    for template in [
        TargetTemplate::SshBare {
            ssh: ssh_connection(),
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
        },
        TargetTemplate::SshPodman {
            ssh: ssh_connection(),
            container: container_template(None),
        },
        TargetTemplate::AwsEc2 {
            aws_profile: None,
            region: "us-east-1".into(),
            launch_template: "lt-mj".into(),
            launch_template_version: None,
            ssh_user: "dev".into(),
            address_source: Default::default(),
            identity_file: None,
            ssh_args: Vec::new(),
        },
    ] {
        assert_eq!(
            preflight_architectures(&template),
            vec!["x86_64", "aarch64"],
            "{template:?}"
        );
    }
}

#[test]
fn dev_checkout_still_finds_a_hel_named_sibling() {
    let controller = PathBuf::from("target/debug/hel");
    let musl = PathBuf::from("target/x86_64-unknown-linux-musl/debug/hel");
    let present = [controller.clone(), musl.clone()];
    let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
        present.iter().any(|p| p == path)
    });
    assert_eq!(selected, Some((musl, "development musl sibling")));
}

/// An architecture no host builds for, so the lookup cannot take one of
/// the "native mj binary" shortcuts and reaches the end on any machine.
const FOREIGN_ARCH: &str = "riscv64";

/// A rebuilt or renamed checkout leaves a running daemon pointing at a
/// path that holds nothing. Searching beside that path finds nothing and
/// blames the user for a worker that may well be installed correctly.
#[test]
fn a_replaced_controller_is_reported_instead_of_a_missing_worker() {
    let stale = PathBuf::from("/src/.backup-vHXvCs/target/debug/mj (deleted)");
    let probed = RefCell::new(Vec::new());

    let error = worker_binary_prerequisite_for_current(
        FOREIGN_ARCH,
        WorkerBinaryRequirement::PortableLinux,
        &stale,
        &|path| {
            probed.borrow_mut().push(path.to_path_buf());
            false
        },
    )
    .unwrap_err();

    let detail = format!("{error:#}");
    assert!(
        detail.contains("was replaced or removed on disk"),
        "{detail}"
    );
    assert!(detail.contains("restart the Mjolnir daemon"), "{detail}");
    // The path is named without the kernel's deletion marker.
    assert!(
        detail.contains("/src/.backup-vHXvCs/target/debug/mj)"),
        "{detail}"
    );
    assert!(!detail.contains("(deleted)"), "{detail}");
    assert_eq!(
        probed.into_inner(),
        vec![stale],
        "nothing beside a path that no longer exists is worth probing"
    );
}

/// The guard is about a controller path that no longer exists and nothing
/// else: a controller still on disk keeps its whole sibling lookup, and
/// keeps the plain "no Linux worker" answer when that lookup comes up
/// empty. A present controller is never its own portable worker, so with
/// nothing installed beside it the lookup ends in that plain answer.
#[test]
fn a_present_controller_still_looks_beside_itself() {
    let controller = PathBuf::from("/opt/brokk/mj");
    let probed = RefCell::new(Vec::new());

    let error = worker_binary_prerequisite_for_current(
        FOREIGN_ARCH,
        WorkerBinaryRequirement::PortableLinux,
        &controller,
        &|path| {
            probed.borrow_mut().push(path.to_path_buf());
            path == controller
        },
    )
    .unwrap_err();

    let probed = probed.into_inner();
    assert!(
        probed
            .iter()
            .any(|path| path.ends_with("mj-worker-riscv64-unknown-linux-musl")),
        "the packaged worker name must still be probed: {probed:?}"
    );
    let detail = format!("{error:#}");
    assert!(
        detail.contains("no Linux worker for riscv64-unknown-linux-musl"),
        "{detail}"
    );
    assert!(!detail.contains("restart the Mjolnir daemon"), "{detail}");

    // With nothing beside it either, a present controller still gets the
    // generic message; only a replaced one is told to restart.
    let root = PathBuf::from("/");
    let error = worker_binary_prerequisite_for_current(
        FOREIGN_ARCH,
        WorkerBinaryRequirement::PortableLinux,
        &root,
        &|path| path == root,
    )
    .unwrap_err();
    let detail = format!("{error:#}");
    assert!(
        detail.contains("no Linux worker for riscv64-unknown-linux-musl"),
        "{detail}"
    );
    assert!(!detail.contains("restart the Mjolnir daemon"), "{detail}");
}

const WORKER_BINARY_OVERRIDE_CHILD: &str = "MJ_WORKER_BINARY_OVERRIDE_CHILD";

/// The override names a worker outright, so it does not care where the
/// controller lives or whether that path still exists.
#[test]
fn a_replaced_controller_still_honors_the_worker_binary_override() {
    // MJ_WORKER_BINARY is process-global and other tests resolve worker
    // binaries, so set it only in an exact child test.
    if std::env::var_os(WORKER_BINARY_OVERRIDE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("mj-worker");
        std::fs::write(&worker, b"worker").unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "a_replaced_controller_still_honors_the_worker_binary_override",
        ))
        .env(WORKER_BINARY_OVERRIDE_CHILD, "1")
        .env("MJ_WORKER_BINARY", &worker)
        .run();
        return;
    }

    let stale = PathBuf::from("/src/.backup-vHXvCs/target/debug/mj (deleted)");
    let availability = worker_binary_prerequisite_for_current(
        FOREIGN_ARCH,
        WorkerBinaryRequirement::PortableLinux,
        &stale,
        &|path| path.is_file(),
    )
    .unwrap();

    match availability {
        WorkerBinaryAvailability::Local { source, .. } => {
            assert_eq!(source, "MJ_WORKER_BINARY");
        }
        other => panic!("expected the override to resolve, got {other:?}"),
    }
}

#[test]
fn sibling_lookup_falls_back_to_the_legacy_hel_name_beside_an_mj_controller() {
    let controller = PathBuf::from("/opt/brokk/mj");
    let legacy = PathBuf::from("/opt/brokk/hel");
    let selected = select_sibling_worker(&controller, "x86_64-unknown-linux-musl", |path| {
        path == legacy
    });
    assert_eq!(selected, Some((legacy, "beside the running executable")));
}

#[test]
fn worker_diagnosis_surfaces_a_loader_failure_from_the_installed_binary() {
    struct FailedProbe;

    impl CommandExecutor for FailedProbe {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"libc.so.6: version `GLIBC_2.39' not found\n".to_vec(),
            })
        }
    }

    let failure = worker_binary_probe_failure(
        &FailedProbe,
        &targets::TargetLocator::LocalBare {
            worker_root: "/worker/root".into(),
        },
        "/worker/root",
    )
    .expect("an unsuccessful --version probe should explain the dead worker");

    assert!(failure.contains("GLIBC_2.39"), "{failure}");
    assert!(failure.contains("provide a musl worker"), "{failure}");
}

/// macOS puts worker roots under `~/Library/Application Support/...`.
/// An unquoted root split the diagnostic script into separate words, so
/// the probe silently reported nothing exactly when it was needed.
#[test]
fn worker_last_words_reads_a_root_containing_spaces() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Application Support").join("hel worker");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("worker-exit.json"),
        b"{\n  \"reason\": \"panic\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("worker.log"),
        b"Mjolnir worker exited with an error\n",
    )
    .unwrap();
    let root = root.to_str().unwrap();

    let locator = targets::TargetLocator::LocalBare {
        worker_root: root.into(),
    };
    let reported = worker_last_words(&ProcessExecutor, &locator, root)
        .expect("the probe reads a root containing spaces");
    assert!(reported.contains(WORKER_EXIT_RECORD_MARKER), "{reported}");
    assert!(reported.contains("\"reason\": \"panic\""), "{reported}");
    assert!(
        reported.contains("Mjolnir worker exited with an error"),
        "{reported}"
    );
    // No worker runs for this temporary root, so the process section must
    // say so rather than being omitted.
    assert!(reported.contains("--- worker process ---"), "{reported}");
    assert!(reported.contains("absent"), "{reported}");

    let recorder = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    worker_last_words(&recorder, &locator, root);
    let commands = recorder.commands.borrow();
    let script = commands
        .iter()
        .flat_map(|command| command.args.iter())
        .find(|argument| argument.contains("worker-exit.json"))
        .expect("the probe builds a diagnostic script");
    assert!(
        script.contains(&format!("'{root}'")),
        "the root must be single-quoted: {script}"
    );
}

/// A worker that died leaves an exit record behind. Starting a new worker
/// must clear it first, or the startup connect loop reads the previous
/// death as this worker's and gives up on a healthy daemon.
#[test]
fn starting_a_worker_clears_stale_runtime_files_before_launching() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    for locator in [
        targets::TargetLocator::LocalBare {
            worker_root: "/worker/root".into(),
        },
        targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "container-1".into(),
            workspace_storage: Default::default(),
        },
    ] {
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        start_worker(&executor, &locator, "/worker/root").unwrap();

        let commands = executor.commands.borrow();
        let script = commands
            .iter()
            .flat_map(|command| command.args.iter())
            .find(|argument| argument.contains("worker-exit.json"))
            .unwrap_or_else(|| panic!("no launch script cleared the exit record: {commands:?}"));
        let cleared = script.find("rm -f").expect("the exit record is removed");
        let launched = script.find("worker").expect("the daemon is launched");
        assert!(
            script.contains("control.sock"),
            "the stale relay endpoint must be cleared before startup: {script}"
        );
        assert!(
            cleared < launched,
            "stale runtime files must be cleared before the daemon starts: {script}"
        );
    }
}
#[test]
fn stopping_a_worker_runs_the_daemon_stop_script() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let locator = targets::TargetLocator::SshBare {
        worker_id: None,
        ssh: SshTarget {
            destination: "user@example.test".into(),
            ssh_args: Vec::new(),
        },
        workspace: "/workspace".into(),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    stop_worker(&executor, &locator, "/worker/root").unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].purpose, "stop Mjolnir worker daemon");
    assert!(
        commands[0]
            .args
            .last()
            .is_some_and(|remote| remote.starts_with("'sh' '-c' ")),
        "raw SSH worker management must not source login profiles: {commands:?}"
    );
    let script = commands[0]
        .args
        .iter()
        .find(|argument| argument.contains("worker run --root"))
        .unwrap_or_else(|| panic!("stop script missing from {commands:?}"));
    assert!(
        script.contains("hel_match=\"hel worker run --root $hel_root\""),
        "stop must match only this session's worker: {script}"
    );
    assert!(
        script.contains("hel_match_home=\"hel worker run --root $HOME/$hel_root\""),
        "stop must also match a login-home-absolute --root: {script}"
    );
    assert!(
        !script.contains("grep -F"),
        "leftover detection must not grep the match string: {script}"
    );
}
#[test]
fn checkpoint_worker_stop_restores_a_stopped_podman_target_first() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
        outputs: RefCell<Vec<CommandOutput>>,
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(self.outputs.borrow_mut().remove(0))
        }
    }

    let session = "0123456789abcdef0123456789abcdef";
    let container_id = targets::resource_name(session).unwrap();
    let inspection = |status: &str| CommandOutput {
        status: 0,
        stdout: serde_json::to_vec(&serde_json::json!([{
            "Config": { "Labels": {
                (targets::MANAGED_LABEL): "true",
                (targets::SESSION_LABEL): session,
            }},
            "State": { "Status": status },
        }]))
        .unwrap(),
        stderr: Vec::new(),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
        outputs: RefCell::new(vec![
            CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            inspection("exited"),
            CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            inspection("running"),
            CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]),
    };
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id,
        workspace_storage: Default::default(),
    };

    stop_worker_after_target_recovery(&executor, &locator, session, "/worker/root").unwrap();

    let commands = executor.commands.borrow();
    let purposes = commands
        .iter()
        .map(|command| command.purpose.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        purposes,
        [
            "check for Mjolnir session container",
            "inspect Mjolnir session container",
            "start stopped Mjolnir session container",
            "inspect Mjolnir session container",
            "stop Mjolnir worker daemon",
        ]
    );
}

struct PodmanInstallExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    worker_cached: bool,
}
impl CommandExecutor for PodmanInstallExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        let probing_cache = command
            .args
            .iter()
            .any(|argument| argument.contains("'test' '-f'"));
        let status = if probing_cache && !self.worker_cached {
            1
        } else {
            0
        };
        Ok(CommandOutput {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}
struct PodmanInstallFixture {
    _root: tempfile::TempDir,
    worker_binary: PathBuf,
    launch_config: PathBuf,
    ownership: PathBuf,
    profile_stage: PathBuf,
    locator: targets::TargetLocator,
    digest: String,
}
fn podman_install_fixture() -> PodmanInstallFixture {
    let root = tempfile::tempdir().unwrap();
    let worker_binary = root.path().join("hel");
    std::fs::write(&worker_binary, b"worker-binary-bytes").unwrap();
    let launch_config = root.path().join("launch.json");
    std::fs::write(&launch_config, b"{}").unwrap();
    let ownership = root.path().join("ownership.json");
    std::fs::write(&ownership, b"{}").unwrap();
    let profile_stage = root.path().join("profile");
    std::fs::create_dir_all(&profile_stage).unwrap();
    let digest = lower_hex(Sha256::digest(b"worker-binary-bytes"));
    PodmanInstallFixture {
        _root: root,
        worker_binary,
        launch_config,
        ownership,
        profile_stage,
        locator: targets::TargetLocator::SshPodman {
            borrowed_from: None,
            ssh: SshTarget {
                destination: "user@example.test".into(),
                ssh_args: Vec::new(),
            },
            container_id: "container-1".into(),
            workspace_storage: Default::default(),
        },
        digest,
    }
}
fn run_podman_install(worker_cached: bool) -> (Vec<CommandSpec>, PodmanInstallFixture) {
    let fixture = podman_install_fixture();
    let executor = PodmanInstallExecutor {
        commands: RefCell::new(Vec::new()),
        worker_cached,
    };
    install_worker_files(
        &executor,
        &fixture.locator,
        "0123456789abcdef0123456789abcdef",
        "/workspace/.hel/worker",
        "/workspace/.hel/profile",
        &fixture.worker_binary,
        &fixture.launch_config,
        &fixture.ownership,
        &fixture.profile_stage,
    )
    .unwrap();
    let commands = executor.commands.borrow().clone();
    (commands, fixture)
}
fn rendered(commands: &[CommandSpec]) -> Vec<String> {
    commands
        .iter()
        .map(|command| format!("{} {}", command.program, command.args.join(" ")))
        .collect()
}
#[test]
fn ssh_podman_install_caches_the_worker_binary_on_a_cache_miss() {
    let (commands, fixture) = run_podman_install(false);
    let lines = rendered(&commands);
    let digest = &fixture.digest;
    let cache_dir = format!(".cache/mjolnir/workers/{digest}");
    let session = "0123456789abcdef0123456789abcdef";

    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("ssh") && line.contains("'test' '-f'")),
        "expected a cache probe, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains('~')),
        "remote staging paths must be home-relative: ssh arguments are \
             single-quoted so a tilde stays literal in the remote shell while \
             scp expands it, got {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("ssh")
                && line.contains(&format!("'mkdir' '-p' '{cache_dir}'"))),
        "expected the cache directory to be created, got {lines:#?}"
    );
    let partial = format!("{cache_dir}/hel.partial-{session}");
    assert!(
        lines.iter().any(|line| line.starts_with("scp ")
            && line.ends_with(&format!(
                "{} user@example.test:{partial}",
                fixture.worker_binary.display()
            ))),
        "expected the worker to be uploaded to the partial cache path, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("ssh")
            && line.contains(&format!("'mv' '{partial}' '{cache_dir}/hel'"))),
        "expected an atomic rename into the cache, got {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("'podman' 'cp'")
                && line.contains(&format!("'{cache_dir}/hel'"))),
        "expected podman cp to read the cached worker, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("scp")
            && line.ends_with(&format!(
                "user@example.test:.cache/mjolnir/uploads/{session}/hel"
            ))),
        "the worker must not be staged in the per-session upload directory, got {lines:#?}"
    );
}
#[test]
fn ssh_podman_install_skips_the_worker_upload_on_a_cache_hit() {
    let (commands, fixture) = run_podman_install(true);
    let lines = rendered(&commands);
    let digest = &fixture.digest;
    let cache_dir = format!(".cache/mjolnir/workers/{digest}");
    let session = "0123456789abcdef0123456789abcdef";

    assert!(
        !lines.iter().any(|line| line.starts_with("scp")
            && line.contains(&fixture.worker_binary.display().to_string())),
        "a cached worker must not be re-uploaded, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("'mv'")),
        "a cache hit must not rename anything, got {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("'podman' 'cp'")
                && line.contains(&format!("'{cache_dir}/hel'"))),
        "expected podman cp to read the cached worker, got {lines:#?}"
    );
    for name in ["launch.json", "ownership.json"] {
        assert!(
            lines.iter().any(|line| line.starts_with("scp")
                && line.ends_with(&format!(
                    "user@example.test:.cache/mjolnir/uploads/{session}/{name}"
                ))),
            "expected {name} to still be uploaded per session, got {lines:#?}"
        );
    }
}

#[test]
fn ssh_docker_install_uses_docker_for_remote_container_operations() {
    let mut fixture = podman_install_fixture();
    fixture.locator = targets::TargetLocator::SshDocker {
        borrowed_from: None,
        ssh: SshTarget {
            destination: "user@example.test".into(),
            ssh_args: Vec::new(),
        },
        container_id: "container-1".into(),
    };
    let executor = PodmanInstallExecutor {
        commands: RefCell::new(Vec::new()),
        worker_cached: true,
    };
    install_worker_files(
        &executor,
        &fixture.locator,
        "0123456789abcdef0123456789abcdef",
        "/workspace/.hel/worker",
        "/workspace/.hel/profile",
        &fixture.worker_binary,
        &fixture.launch_config,
        &fixture.ownership,
        &fixture.profile_stage,
    )
    .unwrap();

    let lines = rendered(&executor.commands.borrow());
    assert!(
        lines.iter().any(|line| line.contains("'docker' 'cp'")),
        "expected Docker to copy the cached worker, got {lines:#?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("'docker' 'exec'")),
        "expected Docker to prepare the worker directories, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("'podman'")),
        "Docker installation accidentally used Podman: {lines:#?}"
    );
}

#[test]
#[ignore = "requires Docker and the locally installed agent-dev image"]
fn docker_uploads_and_replacements_are_usable_by_the_non_root_worker() {
    let fixture = podman_install_fixture();
    let session = mj_core::state::new_session_id().unwrap();
    let container_id = targets::resource_name(&session).unwrap();
    let locator = targets::TargetLocator::LocalDocker {
        borrowed_from: None,
        container_id: container_id.clone(),
    };
    execute_checked(
        &ProcessExecutor,
        CommandSpec::new(
            "docker",
            [
                "run",
                "--pull=never",
                "-d",
                "--name",
                &container_id,
                "ghcr.io/brokkai/mjolnir/agent-dev:latest",
                "sleep",
                "infinity",
            ],
        ),
    )
    .unwrap();
    let result = (|| -> Result<()> {
        let root = targets::worker_root(&locator, &session)?;
        let profile = format!("{root}/profile");
        std::fs::write(fixture.profile_stage.join("credential"), "private")?;
        install_worker_files(
            &ProcessExecutor,
            &locator,
            &session,
            &root,
            &profile,
            &fixture.worker_binary,
            &fixture.launch_config,
            &fixture.ownership,
            &fixture.profile_stage,
        )?;
        replace_installed_worker_binary(
            &ProcessExecutor,
            &locator,
            &session,
            &fixture.worker_binary,
        )?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "exec",
                    &container_id,
                    "sh",
                    "-c",
                    "test \"$(id -u)\" != 0 && test -x \"$1/hel\" && test -r \"$1/launch.json\" && test -r \"$1/ownership.json\" && test -r \"$1/profile/credential\" && test -w \"$1/profile/credential\"",
                    "sh",
                    &root,
                ],
            ),
        )?;
        Ok(())
    })();
    let cleanup = execute_checked(
        &ProcessExecutor,
        CommandSpec::new("docker", ["rm", "-f", &container_id]),
    );
    result.unwrap();
    cleanup.unwrap();
}

#[test]
fn replacing_an_installed_podman_worker_writes_through_a_next_path() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }
    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let session = "0123456789abcdef0123456789abcdef";
    let container_id = targets::resource_name(session).unwrap();
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: container_id.clone(),
        workspace_storage: Default::default(),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    replace_installed_worker_binary(&executor, &locator, session, Path::new("/controller/hel"))
        .unwrap();

    let mut lines = rendered(&executor.commands.borrow());
    let ownership = lines.remove(1);
    assert!(ownership.starts_with(&format!("podman exec --user 0 {container_id} sh -c")));
    assert!(ownership.contains("chown -R"));
    assert!(ownership.ends_with(&format!("/var/lib/hel/workers/{session}/hel.next")));
    assert_eq!(
        lines,
        vec![
            format!(
                "podman cp /controller/hel {container_id}:/var/lib/hel/workers/{session}/hel.next"
            ),
            format!(
                "podman exec {container_id} mv -f /var/lib/hel/workers/{session}/hel.next /var/lib/hel/workers/{session}/hel"
            ),
            format!("podman exec {container_id} chmod 700 /var/lib/hel/workers/{session}/hel"),
        ]
    );
}
#[test]
fn default_bridges_pin_command_capable_adapter_versions() {
    let (codex_command, codex_arguments) = bridge_launch(
        mj_core::config::HarnessKind::Codex,
        ExecutionPolicy::Unconstrained,
    );
    assert_eq!(codex_command, "sh");
    assert_eq!(codex_arguments[0], "-c");
    assert!(codex_arguments[1].contains("@brokkai/codex-acp@1.11.4"));
    assert!(codex_arguments[1].contains("codex-acp --version"));
    assert!(codex_arguments[1].contains("npx -y @brokkai/codex-acp@1.11.4"));

    let (claude_command, claude_arguments) = bridge_launch(
        mj_core::config::HarnessKind::Claude,
        ExecutionPolicy::Unconstrained,
    );
    assert_eq!(claude_command, "sh");
    assert_eq!(claude_arguments[0], "-c");
    assert!(claude_arguments[1].contains("@agentclientprotocol/claude-agent-acp@0.79.0"));
}

#[test]
fn readiness_stage_names_only_install_capable_default_harnesses() {
    let profile = |kind| mj_core::config::HarnessProfile {
        enabled: true,
        kind,
        home: PathBuf::from("/profiles/test"),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    for harness in HarnessKind::ALL {
        assert_eq!(
            bridge_readiness_stage(&profile(harness)),
            ProvisionStage::Installing(harness)
        );
    }
}
#[test]
fn codex_execution_environment_follows_the_target_policy() {
    let mut podman_environment =
        BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
    mj_core::config::HarnessKind::Codex
        .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut podman_environment)
        .unwrap();
    assert_eq!(
        podman_environment
            .get("INITIAL_AGENT_MODE")
            .map(String::as_str),
        Some("agent-full-access")
    );

    let mut bare_environment =
        BTreeMap::from([("INITIAL_AGENT_MODE".to_owned(), "read-only".to_owned())]);
    mj_core::config::HarnessKind::Codex
        .configure_execution_environment(
            ExecutionPolicy::ConfiguredApprovals,
            &mut bare_environment,
        )
        .unwrap();
    assert_eq!(
        bare_environment
            .get("INITIAL_AGENT_MODE")
            .map(String::as_str),
        Some("agent"),
        "Codex uses guardian on raw localhost"
    );
}
#[test]
fn bare_targets_use_managed_harnesses_but_containers_stay_ambient() {
    let ssh = SshTarget {
        destination: "user@example.test".into(),
        ssh_args: Vec::new(),
    };
    let targets = [
        (
            targets::TargetLocator::LocalBare {
                worker_root: "/worker".into(),
            },
            HarnessRuntimePolicy::Managed,
        ),
        (
            targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: "container".into(),
                workspace_storage: Default::default(),
            },
            HarnessRuntimePolicy::Ambient,
        ),
        (
            targets::TargetLocator::SshBare {
                worker_id: None,
                ssh: ssh.clone(),
                workspace: "/workspace/session".into(),
            },
            HarnessRuntimePolicy::Managed,
        ),
        (
            targets::TargetLocator::AwsEc2 {
                profile: "profile".into(),
                region: "us-east-1".into(),
                instance_id: "i-test".into(),
                ssh,
                workspace: "/workspace/session".into(),
            },
            HarnessRuntimePolicy::Managed,
        ),
    ];

    for (target, expected) in targets {
        assert_eq!(harness_runtime_policy(&target), expected, "{target:?}");
    }
}
#[test]
fn grok_sandbox_environment_follows_the_target_policy() {
    let mut isolated = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
    mj_core::config::HarnessKind::Grok
        .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut isolated)
        .unwrap();
    assert_eq!(
        isolated.get("GROK_SANDBOX").map(String::as_str),
        Some("off")
    );

    let mut local = BTreeMap::from([("GROK_SANDBOX".to_owned(), "strict".to_owned())]);
    mj_core::config::HarnessKind::Grok
        .configure_execution_environment(ExecutionPolicy::ConfiguredApprovals, &mut local)
        .unwrap();
    assert_eq!(
        local.get("GROK_SANDBOX").map(String::as_str),
        Some("strict"),
        "raw localhost must preserve the profile's configured sandbox"
    );
}
#[test]
fn bridge_fallback_pins_match_the_agent_dev_containerfile() {
    const CONTAINERFILE: &str = include_str!("../../../../containers/Containerfile.agent-dev");

    let codex = format!("codex-acp@{CODEX_ACP_VERSION}");
    assert!(
        CONTAINERFILE.contains(&codex),
        "containers/Containerfile.agent-dev must install {codex}. The image and the \
             bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
             session and an npx session run different adapter versions."
    );

    let claude = format!("claude-agent-acp@{CLAUDE_ACP_VERSION}");
    assert!(
        CONTAINERFILE.contains(&claude),
        "containers/Containerfile.agent-dev must install {claude}. The image and the \
             bridge_launch() npx fallbacks have to stay in lockstep, otherwise a container \
             session and an npx session run different adapter versions."
    );
}
#[test]
fn kimi_default_bridge_is_non_login_and_uses_bash_for_the_official_installer() {
    let (command, arguments) = bridge_launch(
        mj_core::config::HarnessKind::Kimi,
        ExecutionPolicy::Unconstrained,
    );
    assert_eq!(command, "sh");
    assert_eq!(arguments[0], "-c");
    assert!(arguments[1].contains("install.sh | bash &&"));
    assert!(arguments[1].contains("$HOME/.kimi-code/bin/kimi"));
    assert!(arguments[1].contains("Mjolnir needs compatible Kimi Code"));
    assert!(!arguments[1].contains("Hel"));
}
#[test]
fn grok_default_bridge_is_non_login_and_uses_bash_for_the_official_installer() {
    let (command, arguments) = bridge_launch(
        mj_core::config::HarnessKind::Grok,
        ExecutionPolicy::ConfiguredApprovals,
    );
    assert_eq!(command, "sh");
    assert_eq!(arguments[0], "-c");
    let script = &arguments[1];
    assert!(script.contains("https://x.ai/cli/install.sh | bash &&"));
    assert!(script.contains("command -v grok"));
    assert!(script.contains("[ -x \"$GROK_HOME/bin/grok\" ]"));
    assert!(script.contains("[ -x \"$HOME/.grok/bin/grok\" ]"));
    assert!(script.contains("exit 127"));
    assert!(script.contains("exec grok agent stdio"));
    assert!(!script.contains("--always-approve"));
    assert!(script.contains("Mjolnir needs compatible Grok Build"));
    assert!(!script.contains("Hel"));
}
#[test]
fn node_bootstrap_errors_name_mjolnir() {
    let script = ensure_node_script();
    assert!(script.contains("Mjolnir needs Node.js, npm, and npx"));
    assert!(!script.contains("sudo"));
    assert!(!script.contains("apt-get"));
    assert!(!script.contains("Hel"));
}
#[test]
fn grok_default_bridge_adds_the_always_approve_flag_when_unrestricted() {
    let (_, arguments) = bridge_launch(
        mj_core::config::HarnessKind::Grok,
        ExecutionPolicy::Unconstrained,
    );
    let script = &arguments[1];
    assert!(script.contains("exec grok agent --always-approve stdio"));
    assert!(script.contains("exec \"$GROK_HOME/bin/grok\" agent --always-approve stdio"));
    assert!(script.contains("exec \"$HOME/.grok/bin/grok\" agent --always-approve stdio"));
}
#[test]
fn kimi_uses_runtime_aware_memory_delivery_only_on_staged_targets() {
    let local = targets::TargetLocator::LocalBare {
        worker_root: "/worker".into(),
    };
    let podman = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "container".into(),
        workspace_storage: Default::default(),
    };

    assert_eq!(
        project_memory_mcp_delivery(mj_core::config::HarnessKind::Kimi, &local),
        ProjectMemoryMcpDelivery::Acp
    );
    assert_eq!(
        project_memory_mcp_delivery(mj_core::config::HarnessKind::Kimi, &podman),
        ProjectMemoryMcpDelivery::HarnessProfile
    );
    assert_eq!(
        project_memory_mcp_delivery(mj_core::config::HarnessKind::Codex, &podman),
        ProjectMemoryMcpDelivery::Acp
    );
}
/// A catalog cache backed by an isolated copy of Mjolnir's own
/// `profile_config_cache` table, so the fallback path is exercised against
/// the real schema without touching the live store.
struct IsolatedCatalogCache(std::path::PathBuf);

impl CatalogCache for IsolatedCatalogCache {
    fn load(&self, profile_id: &str, key: &str) -> Option<String> {
        crate::database::load_profile_config_cache_from(&self.0, profile_id, key, key)
            .ok()
            .flatten()
    }

    fn store(&self, profile_id: &str, key: &str, body: &str) {
        crate::database::save_profile_config_cache_at(&self.0, profile_id, key, key, body)
            .expect("write the isolated catalog cache");
    }
}

const ZAI_CONFIG: &str = "model = \"glm-5.3\"\n\
                          model_provider = \"zai\"\n\
                          \n\
                          [model_providers.zai]\n\
                          base_url = \"https://api.z.ai/api/v1\"\n\
                          env_key = \"ZAI_API_KEY\"\n\
                          wire_api = \"responses\"\n";

const ZAI_CATALOG: &str = r#"{"models":[
    {"slug":"glm-5.3","supported_reasoning_levels":["low","high","max"]},
    {"slug":"glm-5.3-flash","supported_reasoning_levels":["low","high","max"]}
]}"#;

fn zai_profile(home: &Path) -> mj_core::config::HarnessProfile {
    std::fs::write(home.join("config.toml"), ZAI_CONFIG).unwrap();
    mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Codex,
        home: home.to_path_buf(),
        environment: BTreeMap::from([("ZAI_API_KEY".to_owned(), "coding-plan-key".to_owned())]),
        context_window_bytes: None,
        guardian_review_model: None,
    }
}

#[test]
fn staging_a_custom_provider_profile_writes_a_catalog_the_session_can_pick_from() {
    let home = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let cache = IsolatedCatalogCache(cache.path().join("cache.sqlite3"));
    let asked = std::cell::RefCell::new(Vec::new());

    stage_profile(&profile, staged.path()).unwrap();
    stage_codex_catalog(
        "glm",
        &profile,
        staged.path(),
        &|url, key| {
            asked.borrow_mut().push((url.to_owned(), key.to_owned()));
            Ok(ZAI_CATALOG.as_bytes().to_vec())
        },
        &cache,
    )
    .unwrap();

    assert_eq!(
        asked.into_inner(),
        vec![(
            "https://api.z.ai/api/v1/models".to_owned(),
            "coding-plan-key".to_owned()
        )],
        "the provider's own key authorizes its catalog fetch"
    );
    let catalog =
        mj_core::codex_catalog::parse(&std::fs::read(staged.path().join("models.json")).unwrap())
            .unwrap();
    assert_eq!(catalog.slugs(), ["glm-5.3", "glm-5.3-flash"]);
    for model in &catalog.models {
        assert_eq!(
            model["auto_review_model_override"],
            serde_json::Value::from("glm-5.3-flash"),
            "Guardian reviews run on the newest flash model"
        );
    }
    // The key must be top-level, so it precedes the provider table, and the
    // user's own lines survive unchanged.
    let config = std::fs::read_to_string(staged.path().join("config.toml")).unwrap();
    assert!(
        config.starts_with("model_catalog_json = \"models.json\"\n"),
        "{config}"
    );
    assert!(config.ends_with(ZAI_CONFIG), "{config}");
    assert_eq!(
        mj_core::codex_provider::codex_provider(staged.path())
            .unwrap()
            .unwrap()
            .model_catalog_json
            .as_deref(),
        Some(Path::new("models.json")),
        "Codex reads the staged catalog as a top-level key"
    );
    // The staged copy is what the session runs from, so a session on a
    // local bare target must not use the profile home directly.
    assert!(super::super::requires_private_profile_home(&profile));
    assert!(
        !home.path().join("models.json").exists(),
        "the user's own profile home stays untouched"
    );
}

#[test]
fn a_failed_catalog_fetch_falls_back_to_the_last_cached_catalog() {
    let home = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let cache = IsolatedCatalogCache(store.path().join("cache.sqlite3"));

    stage_codex_catalog(
        "glm",
        &profile,
        staged.path(),
        &|_, _| Ok(ZAI_CATALOG.as_bytes().to_vec()),
        &cache,
    )
    .unwrap();
    std::fs::remove_file(staged.path().join("models.json")).unwrap();

    stage_codex_catalog(
        "glm",
        &profile,
        staged.path(),
        &|_, _| bail!("the provider is unreachable"),
        &cache,
    )
    .expect("a provider outage must not block a launch");
    let catalog =
        mj_core::codex_catalog::parse(&std::fs::read(staged.path().join("models.json")).unwrap())
            .unwrap();
    assert_eq!(catalog.slugs(), ["glm-5.3", "glm-5.3-flash"]);

    // With nothing cached for a different provider, the launch fails and
    // says which profile and URL could not be reached.
    let empty = tempfile::tempdir().unwrap();
    let error = stage_codex_catalog(
        "glm",
        &profile,
        staged.path(),
        &|_, _| bail!("the provider is unreachable"),
        &IsolatedCatalogCache(empty.path().join("empty.sqlite3")),
    )
    .expect_err("no catalog and no cache cannot launch")
    .to_string();
    assert!(error.contains("glm"), "{error}");
    assert!(error.contains("https://api.z.ai/api/v1/models"), "{error}");
}

#[test]
fn caching_a_catalog_keeps_the_discovered_configuration_of_the_default_model() {
    let home = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let path = store.path().join("cache.sqlite3");
    let profile = zai_profile(home.path());
    // What `mj models` answers for the profile's default model: the discovered
    // configuration is kept under the empty model key.
    crate::database::save_profile_config_cache_at(
        &path,
        "glm",
        "",
        "profile-key",
        "{\"discovered\":true}",
    )
    .unwrap();

    stage_codex_catalog(
        "glm",
        &profile,
        staged.path(),
        &|_, _| Ok(ZAI_CATALOG.as_bytes().to_vec()),
        &IsolatedCatalogCache(path.clone()),
    )
    .unwrap();

    assert_eq!(
        crate::database::load_profile_config_cache_from(&path, "glm", "", "profile-key").unwrap(),
        Some("{\"discovered\":true}".to_owned()),
        "a staged catalog must not evict the discovered configuration",
    );
    assert!(
        IsolatedCatalogCache(path)
            .load("glm", &catalog_cache_key("https://api.z.ai/api/v1"))
            .is_some(),
        "the catalog must still be there to fall back on",
    );
}

const DEEPSEEK_CONFIG: &str = "model = \"deepseek-v4-pro\"\n\
                               model_provider = \"deepseek\"\n\
                               \n\
                               [model_providers.deepseek]\n\
                               base_url = \"https://api.deepseek.com/v1\"\n\
                               env_key = \"DEEPSEEK_API_KEY\"\n\
                               wire_api = \"responses\"\n";

const DEEPSEEK_LIST: &str = r#"{"object":"list","data":[
    {"id":"deepseek-flash","object":"model","owned_by":"deepseek"},
    {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
]}"#;

fn deepseek_profile(home: &Path) -> mj_core::config::HarnessProfile {
    std::fs::write(home.join("config.toml"), DEEPSEEK_CONFIG).unwrap();
    mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Codex,
        home: home.to_path_buf(),
        environment: BTreeMap::from([("DEEPSEEK_API_KEY".to_owned(), "deepseek-key".to_owned())]),
        context_window_bytes: None,
        guardian_review_model: None,
    }
}

fn stage_catalog_for(
    profile: &mj_core::config::HarnessProfile,
    body: &str,
    staged: &Path,
    store: &Path,
) -> Result<mj_core::codex_catalog::CodexCatalog> {
    stage_codex_catalog(
        "deepseek",
        profile,
        staged,
        &|_, _| Ok(body.as_bytes().to_vec()),
        &IsolatedCatalogCache(store.to_path_buf()),
    )?;
    mj_core::codex_catalog::parse(&std::fs::read(staged.join("models.json")).unwrap())
}

#[test]
fn a_plain_model_list_becomes_a_catalog_the_profiles_overrides_refine() {
    let home = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let profile = deepseek_profile(home.path());
    std::fs::write(
        home.path().join("models.json"),
        r#"{"models":[
            {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high"]},
            {"slug":"deepseek-preview","display_name":"DeepSeek Preview"}
        ]}"#,
    )
    .unwrap();

    let catalog = stage_catalog_for(
        &profile,
        DEEPSEEK_LIST,
        staged.path(),
        &store.path().join("cache.sqlite3"),
    )
    .expect("an OpenAI-format model list stages a catalog");

    assert_eq!(
        catalog.slugs(),
        ["deepseek-flash", "deepseek-v4-pro", "deepseek-preview"],
        "the override adds a model the provider's list omits"
    );
    assert_eq!(
        catalog.models[1]["supported_reasoning_levels"],
        serde_json::json!(["low", "high"]),
        "the override gives the translated entry its reasoning levels"
    );
    assert_eq!(
        catalog.models[0]["auto_review_model_override"],
        serde_json::Value::from("deepseek-flash"),
        "the newest flash model reviews by default"
    );
}

#[test]
fn the_guardian_review_setting_picks_which_model_reviews() {
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let mut profile = deepseek_profile(home.path());
    let cache = store.path().join("cache.sqlite3");

    profile.guardian_review_model = Some("session".to_owned());
    let staged = tempfile::tempdir().unwrap();
    let catalog = stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache).expect("stage");
    assert!(
        catalog
            .models
            .iter()
            .all(|model| !model.contains_key("auto_review_model_override")),
        "with \"session\" Codex reviews with the session model, so nothing is stamped"
    );

    profile.guardian_review_model = Some("deepseek-v4-pro".to_owned());
    let staged = tempfile::tempdir().unwrap();
    let catalog = stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache).expect("stage");
    for model in &catalog.models {
        assert_eq!(
            model["auto_review_model_override"],
            serde_json::Value::from("deepseek-v4-pro"),
            "a named slug reviews whichever model the session runs on"
        );
    }

    profile.guardian_review_model = Some("deepseek-nonesuch".to_owned());
    let staged = tempfile::tempdir().unwrap();
    let error = stage_catalog_for(&profile, DEEPSEEK_LIST, staged.path(), &cache)
        .expect_err("a reviewer the provider does not serve cannot review")
        .to_string();
    assert!(error.contains("deepseek-nonesuch"), "{error}");
    assert!(error.contains("deepseek"), "{error}");
    assert!(error.contains("deepseek-flash"), "{error}");
    assert!(
        !staged.path().join("models.json").exists(),
        "a rejected reviewer stages no catalog at all"
    );
}

#[test]
fn a_native_codex_profile_gets_no_generated_catalog() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "model = \"gpt-5.5\"\n").unwrap();
    let staged = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();
    stage_codex_catalog(
        "work",
        &profile,
        staged.path(),
        &|_, _| panic!("a profile with no custom provider must not fetch a catalog"),
        &IsolatedCatalogCache(store.path().join("cache.sqlite3")),
    )
    .unwrap();

    assert!(!staged.path().join("models.json").exists());
    assert_eq!(
        std::fs::read_to_string(staged.path().join("config.toml")).unwrap(),
        "model = \"gpt-5.5\"\n"
    );
    assert!(!super::super::requires_private_profile_home(&profile));
}

#[test]
fn stage_grok_profile_copies_authentication_and_agent_identity() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        "{\"https://auth.x.ai::1\":{}}",
    )
    .unwrap();
    std::fs::write(home.path().join("agent_id"), "stable-agent-id").unwrap();
    std::fs::write(home.path().join("config.toml"), "model = \"grok-4.6\"\n").unwrap();
    // Native session storage is checkpointed, never staged.
    std::fs::create_dir(home.path().join("sessions")).unwrap();
    std::fs::write(home.path().join("sessions/session_search.sqlite"), "x").unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Grok,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert_eq!(
        std::fs::read_to_string(staged.path().join("agent_id")).unwrap(),
        "stable-agent-id"
    );
    assert!(staged.path().join("auth.json").is_file());
    assert!(staged.path().join("config.toml").is_file());
    assert!(!staged.path().join("sessions").exists());
}
#[test]
fn stage_claude_profile_preserves_rollout_identity() {
    let home = tempfile::tempdir().unwrap();
    let identity = r#"{
            "machineID": "stable-machine",
            "userID": "stable-user",
            "cachedGrowthBookFeatures": {
                "tengu_velvet_mallet_fable_5": true
            }
        }"#;
    std::fs::write(home.path().join(".claude.json"), identity).unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert_eq!(
        std::fs::read_to_string(staged.path().join(".claude.json")).unwrap(),
        identity
    );
}

#[cfg(unix)]
#[test]
fn stage_claude_profile_follows_symlinked_entries() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("settings.json"), "{\"model\":\"opus\"}").unwrap();
    std::fs::write(outside.path().join("CLAUDE.md"), "# linked instructions\n").unwrap();
    let skills = outside.path().join("skills");
    std::fs::create_dir_all(skills.join("review")).unwrap();
    std::fs::write(skills.join("review/SKILL.md"), "review skill\n").unwrap();
    // A dangling link inside a copied tree must not fail staging.
    std::os::unix::fs::symlink(outside.path().join("missing"), skills.join("dangling.md")).unwrap();

    let home = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("settings.json"),
        home.path().join("settings.json"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("CLAUDE.md"),
        home.path().join("CLAUDE.md"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&skills, home.path().join("skills")).unwrap();

    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    for (relative, contents) in [
        ("settings.json", "{\"model\":\"opus\"}"),
        ("CLAUDE.md", "# linked instructions\n"),
        ("skills/review/SKILL.md", "review skill\n"),
    ] {
        let path = staged.path().join(relative);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(
            metadata.file_type().is_file(),
            "{relative} should be staged as a regular file"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }
    assert!(!staged.path().join("skills/dangling.md").exists());
}

#[test]
fn staging_reproduces_the_skills_tree_the_sync_will_push() {
    // The launch stage and the credential sync must agree byte for byte:
    // the first reconciliation replaces the whole tree, so a stage the sync
    // does not reproduce would have its managed skills wiped a minute later.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review skill\n").unwrap();
    std::fs::create_dir_all(home.path().join("skills/mj")).unwrap();
    std::fs::write(home.path().join("skills/mj/SKILL.md"), "the user's own\n").unwrap();

    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();
    stage_managed_skills(profile.kind, staged.path()).unwrap();

    let expected = mj_core::skills::session_skills(profile.kind, home.path()).unwrap();
    let installed = mj_core::skills::collect_skills(profile.kind, staged.path()).unwrap();
    assert_eq!(installed.fingerprint(), expected.fingerprint());
    assert_eq!(installed, expected);
    assert_eq!(
        std::fs::read_to_string(staged.path().join("skills/review/SKILL.md")).unwrap(),
        "review skill\n"
    );
    assert_ne!(
        std::fs::read_to_string(staged.path().join("skills/mj/SKILL.md")).unwrap(),
        "the user's own\n"
    );
}

#[cfg(unix)]
#[test]
fn stage_claude_profile_skips_dangling_allowlist_symlinks() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("missing"),
        home.path().join("CLAUDE.md"),
    )
    .unwrap();
    std::fs::write(home.path().join("settings.json"), "{}").unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert!(!staged.path().join("CLAUDE.md").exists());
    assert!(staged.path().join("settings.json").is_file());
}

fn staged_muse_settings(body: &str) -> (tempfile::TempDir, PathBuf) {
    let staged = tempfile::tempdir().unwrap();
    let path = staged.path().join("settings.json");
    std::fs::write(&path, body).unwrap();
    (staged, path)
}

fn stage_muse_settings(profile_stage: &Path) {
    apply_staged_execution_setting(
        HarnessKind::Muse,
        ExecutionPolicy::Unconstrained,
        profile_stage,
    )
    .unwrap();
}

#[test]
fn muse_staged_settings_select_the_unrestricted_profile() {
    let (staged, path) = staged_muse_settings(
        r#"{
            "schema_version": 1,
            "provider": "anthropic",
            "model": "muse-1",
            "tui": {"theme": "dark"},
            "permissions": {"schema_version": 1, "default_profile": ":auto-review"}
        }"#,
    );

    stage_muse_settings(staged.path());

    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(document["provider"], "anthropic");
    assert_eq!(document["model"], "muse-1");
    assert_eq!(document["tui"]["theme"], "dark");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["permissions"]["schema_version"], 1);
    assert_eq!(document["permissions"]["default_profile"], ":unrestricted");
}

#[test]
fn muse_staged_settings_are_created_when_absent() {
    let staged = tempfile::tempdir().unwrap();

    stage_muse_settings(staged.path());

    let body = std::fs::read_to_string(staged.path().join("settings.json")).unwrap();
    assert!(body.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "schema_version": 1,
            "permissions": {"schema_version": 1, "default_profile": ":unrestricted"}
        })
    );
}

#[test]
fn a_harness_without_a_staged_setting_leaves_the_profile_untouched() {
    let source = r#"{"schema_version": 1, "permissions": {"default_profile": ":ask-me"}}"#;

    for (kind, policy) in [
        (HarnessKind::Claude, ExecutionPolicy::Unconstrained),
        (HarnessKind::Muse, ExecutionPolicy::ConfiguredApprovals),
    ] {
        let (staged, path) = staged_muse_settings(source);

        apply_staged_execution_setting(kind, policy, staged.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), source, "{kind:?}");
    }
}

#[test]
fn muse_settings_that_are_not_an_object_report_the_staged_file() {
    let (staged, path) = staged_muse_settings("[]");

    let error = apply_staged_execution_setting(
        HarnessKind::Muse,
        ExecutionPolicy::Unconstrained,
        staged.path(),
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains(&path.display().to_string()),
        "error should name the staged file: {error:#}"
    );
}

#[test]
fn a_build_cache_session_carries_mbx_settings_into_the_target_environment() {
    let home = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let session_id = "0123456789abcdef0123456789abcdef";
    let workspace = targets::new_container_workspace(session_id).unwrap();
    let bundle = crate::controller::test_support::local_bundle(Path::new("/src/project"));
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(session_id).unwrap(),
        workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
    };
    let template = mj_core::config::TargetTemplate::LocalPodman {
        container: mj_core::config::ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".to_owned(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
        },
    };
    let mut session = crate::controller::test_support::checkpoint_test_session(session_id);
    session.harness_kind = HarnessKind::Codex;
    session.last_profile = "glm".into();
    session.project_directory = None;
    session.container_workspace = Some(workspace.clone());

    let without = worker_launch_config(
        &session,
        &profile,
        Some(&bundle),
        &locator,
        session_id,
        Some(&workspace),
        &template,
    )
    .unwrap()
    .0;
    assert!(!without.target_environment.contains_key("MBX_CACHE_DIR"));
    assert!(!without.environment.contains_key("MBX_CACHE_DIR"));

    session.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "local-podman".into(),
        directory: PathBuf::from("/mnt/fast/mbx-cache"),
        max_size: Some("100000000000B".into()),
        target_root: None,
    });
    let with = worker_launch_config(
        &session,
        &profile,
        Some(&bundle),
        &locator,
        session_id,
        Some(&workspace),
        &template,
    )
    .unwrap()
    .0;
    // `target_environment` is what reaches the harness, its terminals, and
    // the reviewer sidecar, not just the harness process.
    assert_eq!(
        with.target_environment
            .get("MBX_CACHE_DIR")
            .map(String::as_str),
        Some("/mnt/fast/mbx-cache")
    );
    assert_eq!(
        with.target_environment
            .get("MBX_GC_MAX_TOTAL_SIZE")
            .map(String::as_str),
        Some("100000000000B")
    );
    assert_eq!(
        with.environment.get("MBX_CACHE_DIR").map(String::as_str),
        Some("/mnt/fast/mbx-cache")
    );
    // The summary and savings lines are suppressed in sessions.
    assert_eq!(
        with.target_environment
            .get("MBX_SUMMARY")
            .map(String::as_str),
        Some("off")
    );
    assert_eq!(
        with.target_environment
            .get("MBX_SAVINGS")
            .map(String::as_str),
        Some("off")
    );
    assert!(!without.target_environment.contains_key("MBX_SUMMARY"));
}

#[test]
fn installing_the_build_cache_places_mbx_and_its_cargo_shim_on_the_session_path() {
    #[derive(Default)]
    struct RecordingExecutor {
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingExecutor {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(format!(
                "{} {}",
                command.program,
                command.args.join(" ")
            ));
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let binary = tempfile::NamedTempFile::new().unwrap();
    let executor = RecordingExecutor::default();
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "hel-session".into(),
        workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
    };

    install_mbx_files(
        &executor,
        &locator,
        "session-1",
        "/home/hel/.hel/worker",
        binary.path(),
        Some("[gc]\nmax_size = \"500GiB\"\n"),
    )
    .unwrap();

    let commands = executor.commands();
    assert!(
        commands.iter().any(|line| line
            == &format!(
                "podman cp {} hel-session:/home/hel/.hel/worker/bin/mbx",
                binary.path().display()
            )),
        "{commands:#?}"
    );
    assert!(
        commands.iter().any(|line| line.contains("ln -f")
            && line.contains("/home/hel/.hel/worker/bin/mbx")
            && line.contains("/home/hel/.hel/worker/bin/cargo")),
        "{commands:#?}"
    );
    assert!(
        commands
            .iter()
            .any(|line| line.contains("exec -i hel-session sh -c") && line.contains("config/mbx")),
        "the host mbx configuration is written into the container: {commands:#?}"
    );
}

#[test]
fn a_child_opens_its_parents_container_workspace() {
    let home = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "1123456789abcdef0123456789abcdef";
    let parent_workspace = targets::new_container_workspace(parent_id).unwrap();
    let bundle = crate::controller::test_support::local_bundle(Path::new("/src/project"));
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(parent_id).unwrap(),
        workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
    };
    let template = mj_core::config::TargetTemplate::LocalPodman {
        container: mj_core::config::ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".to_owned(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
        },
    };
    // The child record copies its parent's workspace when it is created,
    // and `prepare_worker_files` reads the same value off the parent, so
    // both point the child's harness at the parent's checkout rather than
    // at a workspace named after the child.
    let mut child = crate::controller::test_support::checkpoint_test_session(child_id);
    child.harness_kind = HarnessKind::Codex;
    child.last_profile = "glm".into();
    child.project_directory = None;
    child.container_workspace = Some(parent_workspace.clone());

    let (launch, _, _) = worker_launch_config(
        &child,
        &profile,
        Some(&bundle),
        &locator,
        parent_id,
        Some(&parent_workspace),
        &template,
    )
    .unwrap();

    assert_eq!(
        launch.cwd,
        PathBuf::from(format!("/workspace/{parent_id}/project"))
    );
}

#[test]
fn a_custom_provider_session_carries_its_key_and_runs_from_a_private_home() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let mut session = crate::controller::test_support::checkpoint_test_session("s-glm");
    session.harness_kind = HarnessKind::Codex;
    session.last_profile = "glm".into();
    session.target_template_id = "localhost".into();
    session.project_directory = Some(project.path().to_path_buf());
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/home/me/.local/share/hel/worker".into(),
    });

    let (launch, _, target_home) = worker_launch_config(
        &session,
        &profile,
        None,
        &targets::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/worker".into(),
        },
        &session.id,
        None,
        &mj_core::config::TargetTemplate::LocalBare,
    )
    .unwrap();

    assert_eq!(launch.environment["ZAI_API_KEY"], "coding-plan-key");
    assert_eq!(launch.environment["CODEX_HOME"], target_home);
    assert_eq!(
        target_home, "/home/me/.local/share/hel/worker/profile",
        "the session runs from the staged copy, not the user's profile home"
    );
    assert_eq!(
        launch.authentication_marker.as_deref(),
        Some("config.toml"),
        "the worker checks the Codex configuration, not a ChatGPT auth file"
    );
    // Guardian still applies: a raw local target keeps configured approvals.
    assert_eq!(
        launch.execution_policy,
        ExecutionPolicy::ConfiguredApprovals
    );
    assert_eq!(launch.environment["INITIAL_AGENT_MODE"], "agent");
    assert!(profile.supports_guardian_approvals());
}

/// Muse has no guardian mode, so even a raw local target launches it
/// unconstrained.
#[test]
fn raw_local_muse_launches_unconstrained() {
    let project = tempfile::tempdir().unwrap();
    let mut session = crate::controller::test_support::checkpoint_test_session("s-muse");
    session.harness_kind = HarnessKind::Muse;
    session.last_profile = "muse".into();
    session.target_template_id = "localhost".into();
    session.project_directory = Some(project.path().to_path_buf());
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/home/me/.local/share/hel/worker".into(),
    });
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Muse,
        home: PathBuf::from("/profiles/muse"),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    let (launch, _, _) = worker_launch_config(
        &session,
        &profile,
        None,
        &targets::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/worker".into(),
        },
        &session.id,
        None,
        &mj_core::config::TargetTemplate::LocalBare,
    )
    .unwrap();

    assert_eq!(launch.execution_policy, ExecutionPolicy::Unconstrained);
    assert_eq!(launch.environment["MUSE_APPROVAL_MODE"], "allowAll");
    assert_eq!(launch.environment["MUSE_SERVE_ARGS"], "--disable-sandbox");
}

#[test]
fn stage_kimi_profile_preserves_device_identity() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "default_model = \"k3\"\n").unwrap();
    std::fs::write(home.path().join("device_id"), "stable-device-id").unwrap();
    std::fs::create_dir(home.path().join("credentials")).unwrap();
    std::fs::write(
        home.path().join("credentials/kimi-code.json"),
        "{\"access_token\":\"secret\"}",
    )
    .unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Kimi,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert_eq!(
        std::fs::read_to_string(staged.path().join("device_id")).unwrap(),
        "stable-device-id"
    );
    assert!(staged.path().join("credentials/kimi-code.json").is_file());
}
#[test]
fn staged_kimi_profile_binds_project_memory_to_the_target_runtime() {
    let home = tempfile::tempdir().unwrap();
    let original = serde_json::json!({
        "mcpServers": {
            "user-server": {
                "command": "user-mcp",
                "args": ["serve"]
            }
        },
        "userSetting": true
    });
    let original_body = serde_json::to_vec_pretty(&original).unwrap();
    std::fs::write(home.path().join("mcp.json"), &original_body).unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Kimi,
        home: home.path().to_path_buf(),
        environment: BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };
    stage_profile(&profile, staged.path()).unwrap();
    let memory = ProjectMemoryLaunchConfig {
        project_key: "project".into(),
        root: "/var/lib/hel/profiles/session/projects/project/memory".into(),
        baseline_root: PathBuf::new(),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
    };

    configure_kimi_project_memory_mcp(staged.path(), "/var/lib/hel/workers/session", &memory)
        .unwrap();

    let configured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(staged.path().join("mcp.json")).unwrap()).unwrap();
    assert_eq!(configured["userSetting"], true);
    assert_eq!(
        configured["mcpServers"]["user-server"]["command"],
        "user-mcp"
    );
    assert_eq!(
        configured["mcpServers"]["mj-memory"],
        serde_json::json!({
            "transport": "stdio",
            "command": "/var/lib/hel/workers/session/hel",
            "args": [
                "worker",
                "memory-mcp",
                "--root",
                "/var/lib/hel/profiles/session/projects/project/memory"
            ],
            "runtime_id": "local"
        })
    );
    assert_eq!(
        std::fs::read(home.path().join("mcp.json")).unwrap(),
        original_body,
        "the controller-side Kimi profile must remain unchanged"
    );
}

#[test]
fn staged_kimi_project_memory_resolves_ssh_paths_from_target_home() {
    let staged = tempfile::tempdir().unwrap();
    let memory = ProjectMemoryLaunchConfig {
        project_key: "project".into(),
        root: ".local/share/hel/profiles/session/projects/project/memory".into(),
        baseline_root: PathBuf::new(),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
    };

    configure_kimi_project_memory_mcp(staged.path(), ".local/share/hel/workers/session", &memory)
        .unwrap();

    let configured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(staged.path().join("mcp.json")).unwrap()).unwrap();
    let server = &configured["mcpServers"]["mj-memory"];
    assert_eq!(server["command"], "sh");
    assert_eq!(server["runtime_id"], "local");
    assert_eq!(
        server["args"],
        serde_json::json!([
            "-c",
            "exec \"$HOME/$1\" worker memory-mcp --root \"$HOME/$2\"",
            "mj-memory",
            ".local/share/hel/workers/session/hel",
            ".local/share/hel/profiles/session/projects/project/memory"
        ])
    );
}
#[test]
fn disposable_container_guidance_reaches_each_harness_without_touching_home() {
    let target = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "container".into(),
        workspace_storage: Default::default(),
    };
    for (kind, instructions) in [
        (mj_core::config::HarnessKind::Codex, "AGENTS.md"),
        (mj_core::config::HarnessKind::Claude, "CLAUDE.md"),
        (mj_core::config::HarnessKind::Kimi, "AGENTS.md"),
        (mj_core::config::HarnessKind::Grok, "AGENTS.md"),
        (mj_core::config::HarnessKind::Muse, "AGENTS.md"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let original = "# Controller instructions\n\nKeep this source unchanged.\n";
        let source_instructions = home.path().join(instructions);
        std::fs::write(&source_instructions, original).unwrap();
        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: home.path().to_path_buf(),
            environment: std::collections::BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();
        append_hel_target_environment(kind, staged.path(), &target).unwrap();

        let guidance = std::fs::read_to_string(staged.path().join(instructions)).unwrap();
        assert_eq!(
            guidance,
            format!("{original}\n{MJ_CONTAINER_ENVIRONMENT}"),
            "{instructions} receives the section in the staged profile"
        );
        assert!(guidance.contains("## Mjolnir disposable environment"));
        assert!(!guidance.contains("## Hel disposable environment"));
        assert_eq!(
            std::fs::read_to_string(source_instructions).unwrap(),
            original,
            "{instructions} in the controller-side home stays untouched"
        );
    }
}
#[test]
fn kimi_guidance_uses_agents_md_without_mutating_the_system_override() {
    let home = tempfile::tempdir().unwrap();
    let system_override = "# Custom Kimi system prompt\n";
    std::fs::write(home.path().join("SYSTEM.md"), system_override).unwrap();
    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Kimi,
        home: home.path().to_path_buf(),
        environment: std::collections::BTreeMap::new(),
        context_window_bytes: None,
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();
    append_hel_target_environment(
        profile.kind,
        staged.path(),
        &targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "container".into(),
            workspace_storage: Default::default(),
        },
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(staged.path().join("AGENTS.md")).unwrap(),
        MJ_CONTAINER_ENVIRONMENT
    );
    assert_eq!(
        std::fs::read_to_string(staged.path().join("SYSTEM.md")).unwrap(),
        system_override
    );
    assert!(!home.path().join("AGENTS.md").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join("SYSTEM.md")).unwrap(),
        system_override
    );
}

#[test]
fn ec2_guidance_names_its_real_workspace_and_ssh_bare_gets_none() {
    let ec2 = tempfile::tempdir().unwrap();
    append_hel_target_environment(
        mj_core::config::HarnessKind::Codex,
        ec2.path(),
        &targets::TargetLocator::AwsEc2 {
            profile: "profile".into(),
            region: "region".into(),
            instance_id: "instance".into(),
            ssh: targets::SshTarget {
                destination: "host".into(),
                ssh_args: Vec::new(),
            },
            workspace: ".local/share/hel/workspaces/session".into(),
        },
    )
    .unwrap();
    let guidance = std::fs::read_to_string(ec2.path().join("AGENTS.md")).unwrap();
    assert_eq!(
        guidance,
        "## Mjolnir disposable environment\n\nThis session runs on a disposable Mjolnir EC2 instance. When the session closes, Mjolnir checkpoints everything in project workspace directories under `$HOME/.local/share/hel/workspaces/session`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then terminates the instance.\n\nEverything outside `$HOME/.local/share/hel/workspaces/session`, including installed packages, the rest of `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n"
    );
    assert!(!guidance.contains("## Hel disposable environment"));

    let ssh_bare = tempfile::tempdir().unwrap();
    append_hel_target_environment(
        mj_core::config::HarnessKind::Codex,
        ssh_bare.path(),
        &targets::TargetLocator::SshBare {
            worker_id: None,
            ssh: targets::SshTarget {
                destination: "host".into(),
                ssh_args: Vec::new(),
            },
            workspace: ".local/share/hel/workspaces/session".into(),
        },
    )
    .unwrap();
    assert!(!ssh_bare.path().join("AGENTS.md").exists());
}

#[test]
fn project_memory_replicas_are_session_private() {
    let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        project_memory_replica_slug(key, "session-a"),
        "hel-0123456789abcdef-session-a"
    );
    assert_ne!(
        project_memory_replica_slug(key, "session-a"),
        project_memory_replica_slug(key, "session-b")
    );
}

/// Returns a fixed digest line for every command and records what it ran,
/// so a remote refresh can be driven without a real ssh host.
struct DigestExecutor {
    installed_line: String,
    commands: RefCell<Vec<CommandSpec>>,
}

impl CommandExecutor for DigestExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        Ok(CommandOutput {
            status: 0,
            stdout: self.installed_line.clone().into_bytes(),
            stderr: Vec::new(),
        })
    }
}

// The SshBare worker_root guard requires the workspace to end in the exact
// session ID, so build the locator around the session under test.
fn ssh_bare_locator(session_id: &str) -> targets::TargetLocator {
    targets::TargetLocator::SshBare {
        worker_id: None,
        ssh: SshTarget {
            destination: "user@host.test".into(),
            ssh_args: Vec::new(),
        },
        workspace: format!("/srv/mj/{session_id}"),
    }
}

#[test]
fn remote_upgrade_prepares_managed_harness_without_touching_running_worker() {
    let session = "session-remote";
    let executor = DigestExecutor {
        installed_line: String::new(),
        commands: RefCell::new(Vec::new()),
    };
    let launch = WorkerLaunchConfig {
        subagent_tools: false,
        review_capture: false,
        goal_resume_request: Default::default(),
        target_environment: Default::default(),
        seed_image_environment: false,
        run_mode: Default::default(),
        session_id: session.into(),
        harness: HarnessKind::Codex,
        harness_home: PathBuf::new(),
        authentication_marker: None,
        bridge_command: "ignored".into(),
        bridge_args: Vec::new(),
        harness_runtime: HarnessRuntimePolicy::Managed,
        environment: BTreeMap::new(),
        cwd: "/srv/mj/session-remote/project".into(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
    };

    prepare_managed_harness_for_upgrade(
        &executor,
        &ssh_bare_locator(session),
        session,
        Path::new("/controller/hel"),
        &launch,
    )
    .unwrap();

    let commands = executor.commands.borrow();
    let purposes = commands
        .iter()
        .map(|command| command.purpose.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        purposes,
        vec![
            "clear managed harness preparation staging",
            "create managed harness preparation staging",
            "stage current worker for managed harness preparation",
            "stage managed harness launch configuration",
            "make managed harness preparation worker executable",
            "prepare exact managed harness",
            "remove managed harness preparation staging",
        ]
    );
    assert!(commands.iter().all(|command| {
        !command.purpose.contains("stop Mjolnir worker")
            && !command.purpose.contains("start Mjolnir worker")
            && !command
                .purpose
                .contains("install the current Mjolnir worker binary")
    }));
    let prepare = commands
        .iter()
        .find(|command| command.purpose == "prepare exact managed harness")
        .unwrap();
    let rendered = format!("{} {}", prepare.program, prepare.args.join(" "));
    assert!(rendered.contains("worker' 'prepare-harness' '--config'"));
}

#[test]
fn local_upgrade_preflight_uses_current_binary_and_preserves_launch_policy() {
    struct ConfigRecordingExecutor {
        command: RefCell<Option<CommandSpec>>,
        launch: RefCell<Option<WorkerLaunchConfig>>,
    }

    impl CommandExecutor for ConfigRecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let config_path = command
                .args
                .get(3)
                .context("local prepare command did not include its config path")?;
            *self.command.borrow_mut() = Some(command.clone());
            *self.launch.borrow_mut() = Some(WorkerLaunchConfig::read(Path::new(config_path))?);
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    struct FailingExecutor {
        purposes: RefCell<Vec<String>>,
    }

    impl CommandExecutor for FailingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.borrow_mut().push(command.purpose.clone());
            Err(anyhow::anyhow!("managed harness installation failed"))
        }
    }

    let executor = ConfigRecordingExecutor {
        command: RefCell::new(None),
        launch: RefCell::new(None),
    };
    let launch = WorkerLaunchConfig {
        subagent_tools: false,
        review_capture: false,
        goal_resume_request: Default::default(),
        target_environment: Default::default(),
        seed_image_environment: false,
        run_mode: Default::default(),
        session_id: "session-local".into(),
        harness: HarnessKind::Codex,
        harness_home: PathBuf::new(),
        authentication_marker: None,
        bridge_command: "ignored".into(),
        bridge_args: Vec::new(),
        harness_runtime: HarnessRuntimePolicy::Managed,
        environment: BTreeMap::from([("CODEX_HOME".into(), "/configured/profile/home".into())]),
        cwd: "/workspace/project".into(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
    };
    let locator = targets::TargetLocator::LocalBare {
        worker_root: "/worker/session-local".into(),
    };

    prepare_managed_harness_for_upgrade(
        &executor,
        &locator,
        "session-local",
        Path::new("/controller/hel"),
        &launch,
    )
    .unwrap();

    {
        let command = executor.command.borrow();
        let command = command.as_ref().unwrap();
        assert_eq!(command.purpose, "prepare exact managed harness");
        assert_eq!(command.program, "/controller/hel");
        assert_eq!(
            &command.args[..3],
            ["worker", "prepare-harness", "--config"]
        );
        assert!(!command.args[3].contains("/worker/session-local"));
    }

    let prepared = executor.launch.borrow();
    let prepared = prepared.as_ref().unwrap();
    assert_eq!(
        prepared.environment.get("CODEX_HOME").map(String::as_str),
        Some("/configured/profile/home")
    );
    assert_eq!(
        prepared.execution_policy,
        ExecutionPolicy::ConfiguredApprovals
    );

    let failing = FailingExecutor {
        purposes: RefCell::new(Vec::new()),
    };
    let error = prepare_managed_harness_for_upgrade(
        &failing,
        &locator,
        "session-local",
        Path::new("/controller/hel"),
        &launch,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("managed harness installation failed")
    );
    assert_eq!(
        failing.purposes.borrow().as_slice(),
        ["prepare exact managed harness"]
    );
}

#[test]
fn initial_bare_provision_prepares_the_harness_from_installed_files() {
    let session = "session-remote";
    let executor = DigestExecutor {
        installed_line: String::new(),
        commands: RefCell::new(Vec::new()),
    };
    let mut launch = WorkerLaunchConfig {
        subagent_tools: false,
        review_capture: false,
        goal_resume_request: Default::default(),
        target_environment: Default::default(),
        seed_image_environment: false,
        run_mode: Default::default(),
        session_id: session.into(),
        harness: HarnessKind::Kimi,
        harness_home: PathBuf::new(),
        authentication_marker: None,
        bridge_command: "ignored".into(),
        bridge_args: Vec::new(),
        harness_runtime: HarnessRuntimePolicy::Managed,
        environment: BTreeMap::new(),
        cwd: "/srv/mj/session-remote/project".into(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
    };

    let locator = ssh_bare_locator(session);
    prepare_installed_managed_harness(&executor, &locator, "/worker/root", &launch).unwrap();
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].purpose,
        "prepare exact managed harness before worker startup"
    );
    let rendered = format!("{} {}", commands[0].program, commands[0].args.join(" "));
    assert!(rendered.contains("'/worker/root/hel' 'worker' 'prepare-harness'"));
    drop(commands);

    let local = targets::TargetLocator::LocalBare {
        worker_root: "/worker/session-remote".into(),
    };
    prepare_installed_managed_harness(&executor, &local, "/worker/session-remote", &launch)
        .unwrap();
    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1].program, "/worker/session-remote/hel");
    assert_eq!(
        commands[1].args,
        vec![
            "worker".to_owned(),
            "prepare-harness".to_owned(),
            "--config".to_owned(),
            "/worker/session-remote/launch.json".to_owned(),
        ]
    );
    drop(commands);

    launch.harness_runtime = HarnessRuntimePolicy::Ambient;
    prepare_installed_managed_harness(&executor, &locator, "/worker/root", &launch).unwrap();
    assert_eq!(executor.commands.borrow().len(), 2);
}

#[test]
fn a_remote_worker_with_a_mismatched_binary_is_replaced_before_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, b"fresh musl worker").unwrap();
    let executor = DigestExecutor {
        installed_line: format!("{}  /root/hel\n", "0".repeat(64)),
        commands: RefCell::new(Vec::new()),
    };
    let replaced = replace_remote_worker_binary_if_stale(
        &executor,
        &ssh_bare_locator("session-remote"),
        "session-remote",
        &CommandSpec::new("true", Vec::<String>::new()),
        &source,
    )
    .unwrap();
    assert!(replaced, "a stale remote binary must be replaced");
    assert!(
        executor.commands.borrow().len() > 1,
        "the digest probe must be followed by replacement commands"
    );
}

#[test]
fn a_remote_worker_already_current_is_restarted_without_recopying() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, b"fresh musl worker").unwrap();
    let current = mj_core::worker_launch::worker_executable_digest(&source).unwrap();
    let executor = DigestExecutor {
        installed_line: format!("{current}  /root/hel\n"),
        commands: RefCell::new(Vec::new()),
    };
    let replaced = replace_remote_worker_binary_if_stale(
        &executor,
        &ssh_bare_locator("session-remote"),
        "session-remote",
        &CommandSpec::new("true", Vec::<String>::new()),
        &source,
    )
    .unwrap();
    assert!(!replaced, "a current remote binary must not be recopied");
    assert_eq!(
        executor.commands.borrow().len(),
        1,
        "only the digest probe runs when the binary is already current"
    );
}

#[test]
fn a_remote_recovery_plan_defers_binary_refresh_to_the_recovery_task() {
    let locator = ssh_bare_locator("session-remote");
    let refresh = worker_binary_refresh_plan(&locator, "session-remote")
        .unwrap()
        .expect("a remote target now gets a binary refresh");
    match refresh {
        WorkerBinaryRefresh::Remote(remote) => {
            assert_eq!(remote.session_id, "session-remote");
            assert_eq!(remote.locator, locator);
        }
        WorkerBinaryRefresh::Prepared(_) => {
            panic!("a remote target must defer, not prepare, its binary refresh")
        }
    }
}

/// A daemon pins its worker sources once, at startup. A pin that no longer
/// names a file must send the lookup back to resolution rather than failing
/// every session until someone restarts the daemon (#1068).
#[test]
fn a_pinned_worker_source_whose_file_is_gone_is_resolved_again() {
    let present = PathBuf::from("/pinned/hel");
    let exists = |path: &Path| path == present;

    let live = Ok(WorkerBinaryAvailability::Local {
        path: present.clone(),
        source: "pinned".into(),
    });
    assert!(
        pinned_source_is_usable(&live, &exists),
        "a pin that still names a file is used as it stands"
    );

    let reaped = Ok(WorkerBinaryAvailability::Local {
        path: PathBuf::from("/reaped/build/hel"),
        source: "pinned".into(),
    });
    assert!(
        !pinned_source_is_usable(&reaped, &exists),
        "a pin whose build directory was removed must be resolved again"
    );

    let remote = Ok(WorkerBinaryAvailability::Remote {
        url: "https://example.invalid/hel".into(),
        sha256: "a".repeat(64),
        triple: "x86_64-unknown-linux-musl".into(),
    });
    assert!(
        pinned_source_is_usable(&remote, &exists),
        "a remote source is a URL and does not stop existing"
    );

    let never_pinned: Result<WorkerBinaryAvailability> = Err(anyhow::anyhow!(
        "no Linux worker for x86_64-unknown-linux-musl"
    ));
    assert!(
        !pinned_source_is_usable(&never_pinned, &exists),
        "a source that never resolved must be tried again, not repeated back"
    );
}
