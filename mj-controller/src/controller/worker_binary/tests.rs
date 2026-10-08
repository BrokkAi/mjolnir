use super::launch::*;
use super::*;

fn stamped_worker(body: &[u8]) -> Vec<u8> {
    let mut bytes = body.to_vec();
    bytes.extend_from_slice(mj_core::worker_build::WORKER_BUILD_STAMP.as_bytes());
    bytes
}

#[test]
fn skips_stale_and_unstamped_candidates_and_reports_them_when_none_match() {
    let directory = tempfile::tempdir().unwrap();
    let controller = directory.path().join("target/debug/mj");
    let stale = controller
        .parent()
        .unwrap()
        .join("mj-worker-x86_64-unknown-linux-musl");
    let legacy = directory
        .path()
        .join("target/worker/x86_64-unknown-linux-musl/debug/mj-worker");
    let matching = directory
        .path()
        .join("target/x86_64-unknown-linux-musl/debug/mj-worker");
    for path in [&controller, &stale, &legacy, &matching] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"unstamped").unwrap();
    }
    let stale_stamp = format!("\0MJ-WORKER-BUILD:2.0.0+{}\0", "a".repeat(40));
    std::fs::write(&stale, stale_stamp).unwrap();
    std::fs::write(&matching, stamped_worker(b"current")).unwrap();
    let resolve = || {
        worker_binary_prerequisite_for_current(
            "x86_64",
            WorkerBinaryRequirement::PortableLinux,
            &controller,
            &|path| path.is_file(),
        )
    };
    assert!(
        matches!(resolve().unwrap(), WorkerBinaryAvailability::Local { path, .. } if path == matching)
    );
    std::fs::remove_file(matching).unwrap();
    let error = format!("{:#}", resolve().unwrap_err());
    for expected in [
        stale.to_str().unwrap(),
        legacy.to_str().unwrap(),
        "2.0.0+",
        BUILD_ID,
        "missing worker build stamp",
        "cargo build --target x86_64-unknown-linux-musl",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

// Hard-won: 0cc3263: an explicit stale worker override previously fell through and replaced the named worker
#[test]
fn a_stale_worker_binary_override_fails_instead_of_falling_back() {
    const CHILD: &str = "MJ_STALE_WORKER_OVERRIDE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("override");
        std::fs::write(&worker, b"legacy override").unwrap();
        let workers = directory.path().join("workers");
        std::fs::create_dir(&workers).unwrap();
        std::fs::write(
            workers.join("mj-worker-x86_64-unknown-linux-musl"),
            stamped_worker(b"current"),
        )
        .unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "a_stale_worker_binary_override_fails_instead_of_falling_back",
        ))
        .isolated_store(directory.path())
        .env("MJ_INSTANCE", "issue-1138-override")
        .env(CHILD, "1")
        .env("MJ_WORKER_BINARY", worker)
        .env("MJ_WORKER_DIR", workers)
        .run();
        return;
    }
    // A matching worker in MJ_WORKER_DIR must not replace the named override.
    let error = format!(
        "{:#}",
        worker_binary_prerequisite_for_arch("x86_64").unwrap_err()
    );
    assert!(error.contains("MJ_WORKER_BINARY does not match"), "{error}");
    assert!(error.contains("override"), "{error}");
    assert!(error.contains("missing worker build stamp"), "{error}");
}

#[test]
fn stale_pins_are_re_resolved() {
    const CHILD: &str = "MJ_STALE_WORKER_PIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let workers = directory.path().join("workers");
        std::fs::create_dir(&workers).unwrap();
        std::fs::write(
            workers.join("mj-worker-x86_64-unknown-linux-musl"),
            stamped_worker(b"current"),
        )
        .unwrap();
        IsolatedTest::new(test_name(module_path!(), "stale_pins_are_re_resolved"))
            .isolated_store(directory.path())
            .env("MJ_INSTANCE", "issue-1138-pins")
            .env(CHILD, "1")
            .env("MJ_WORKER_DIR", workers)
            .run();
        return;
    }
    let selected = worker_binary_prerequisite_for_arch("x86_64").unwrap();
    assert!(
        matches!(selected, WorkerBinaryAvailability::Local { ref source, .. } if source == "MJ_WORKER_DIR")
    );
    pin_worker_binary_sources().unwrap();
    let WorkerBinaryAvailability::Local { path: pinned, .. } =
        worker_binary_prerequisite_for_arch("x86_64").unwrap()
    else {
        panic!("local source")
    };
    // Model a cache removed between daemon handoffs, then restored with stale bytes.
    std::fs::write(&pinned, b"stale restored cache").unwrap();
    let error = worker_binary_prerequisite_for_arch("x86_64").unwrap_err();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
    std::fs::remove_file(&pinned).unwrap();
    let WorkerBinaryAvailability::Local { path, .. } =
        worker_binary_prerequisite_for_arch("x86_64").unwrap()
    else {
        panic!("local source")
    };
    verify_worker_build(&path).unwrap();
    assert!(path.starts_with(data_dir().join("workers/pinned")));
}

/// A client replacing the daemon pins the new build's workers during the
/// handoff. The daemon it then starts must find them already indexed, and
/// must still take the pinned snapshot itself.
// Hard-won: daa70d7: worker source warming delayed daemon reconnect under large-session load
#[test]
fn warming_pins_the_sources_for_the_next_daemon_without_taking_its_snapshot() {
    const CHILD: &str = "MJ_WARM_WORKER_PIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let workers = directory.path().join("workers");
        std::fs::create_dir(&workers).unwrap();
        std::fs::write(
            workers.join("mj-worker-x86_64-unknown-linux-musl"),
            stamped_worker(b"current"),
        )
        .unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "warming_pins_the_sources_for_the_next_daemon_without_taking_its_snapshot",
        ))
        .isolated_store(directory.path())
        .env("MJ_INSTANCE", "handoff-warm-pins")
        .env(CHILD, "1")
        .env("MJ_WORKER_DIR", &workers)
        .run();
        return;
    }
    let source = std::path::PathBuf::from(std::env::var_os("MJ_WORKER_DIR").unwrap())
        .join("mj-worker-x86_64-unknown-linux-musl");
    let cache_root = data_dir().join("workers").join("pinned");
    assert!(indexed_pinned_worker(&cache_root, &source).is_none());

    warm_worker_binary_sources().unwrap();
    let warmed = indexed_pinned_worker(&cache_root, &source)
        .expect("warming indexed the source for the next daemon");
    assert!(
        PINNED_WORKER_BINARY_SOURCES.get().is_none(),
        "warming must leave the pinned snapshot to the daemon"
    );

    pin_worker_binary_sources().unwrap();
    let WorkerBinaryAvailability::Local { path, .. } =
        worker_binary_prerequisite_for_arch("x86_64").unwrap()
    else {
        panic!("local source")
    };
    assert_eq!(path, warmed);
}

/// Test-and-fix M-4: the daemon log named neither the worker it chose for a
/// target nor where it came from.
// Hard-won: deb1c66: shipped startup logs omitted the selected worker source and build diagnostics
#[test]
fn pinning_logs_each_selected_worker_with_its_source_and_build() {
    // Other tests install subscribers that make tracing cache "no interest"
    // in this event, so it is checked alone, in a child with a global one.
    const CHILD: &str = "MJ_PINNING_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "pinning_logs_each_selected_worker_with_its_source_and_build",
        ))
        .env(CHILD, "1")
        .isolated_store(root.path())
        .run();
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("worker");
    std::fs::write(&worker, stamped_worker(b"logged bytes")).unwrap();
    let cache = directory.path().join("cache");
    let log = crate::test_log::CapturedLog::default();
    tracing::subscriber::set_global_default(log.clone()).expect("the only global subscriber");
    WorkerBinarySourceSnapshot::capture(&cache, |arch, requirement| {
        if arch == "x86_64" && requirement == WorkerBinaryRequirement::PortableLinux {
            Ok(WorkerBinaryAvailability::Local {
                path: worker.clone(),
                source: "beside the mj binary".into(),
            })
        } else {
            bail!("no worker")
        }
    });
    let selected: Vec<String> = log
        .at_or_above(tracing::Level::INFO)
        .into_iter()
        .filter(|event| event.contains("worker source selected"))
        .collect();
    assert_eq!(selected.len(), 1, "{selected:#?}");
    let line = &selected[0];
    for expected in [
        "beside the mj binary",
        "x86_64-unknown-linux-musl",
        &worker.display().to_string(),
        BUILD_ID,
    ] {
        assert!(line.contains(expected), "missing {expected:?} in {line}");
    }
}

#[test]
fn pinning_rejects_stale_sources_before_publication() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, b"legacy worker").unwrap();
    let cache = directory.path().join("cache");
    let snapshot = WorkerBinarySourceSnapshot::capture(&cache, |_, _| {
        Ok(WorkerBinaryAvailability::Local {
            path: source.clone(),
            source: "stale fixture".into(),
        })
    });
    assert!(
        snapshot
            .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
            .is_err()
    );
    assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
}

#[test]
fn downloads_reject_stale_builds_even_with_the_expected_checksum() {
    const CHILD: &str = "MJ_STALE_DOWNLOAD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "downloads_reject_stale_builds_even_with_the_expected_checksum",
        ))
        .isolated_store(directory.path())
        .env("MJ_INSTANCE", "issue-1138-download")
        .env(CHILD, "1")
        .run();
        return;
    }
    let bytes = vec![b'x'; 150_000];
    let digest = lower_hex(Sha256::digest(&bytes));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/worker", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        use std::io::BufRead;
        let mut request = std::io::BufReader::new(&mut stream);
        loop {
            let mut line = String::new();
            assert!(request.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .unwrap();
        stream.write_all(&bytes).unwrap();
    });
    let error = download_worker(&url, &digest, "x86_64-unknown-linux-musl").unwrap_err();
    server.join().unwrap();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
    let cached = data_dir().join("workers/pinned").join(&digest).join("hel");
    assert!(!cached.exists());
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, vec![b'x'; 150_000]).unwrap();
    let error = download_worker(&url, &digest, "x86_64-unknown-linux-musl").unwrap_err();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
}

#[test]
fn a_matching_digest_never_authorizes_a_stale_worker() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, b"same stale worker on host and target").unwrap();
    let digest = mj_core::worker_launch::worker_executable_digest(&source).unwrap();
    let executor = DigestExecutor {
        installed_line: format!("{digest}  /root/hel\n"),
        commands: RefCell::new(Vec::new()),
    };
    let error = replace_target_worker_binary_if_stale(
        &executor,
        &ssh_bare_locator("session-remote"),
        "session-remote",
        &CommandSpec::new("true", Vec::<String>::new()),
        &source,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
    assert!(executor.commands.borrow().is_empty());
}

#[cfg(unix)]
#[test]
fn upgrade_preparation_leaves_the_installed_worker_unchanged_until_promotion() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "12121212121212121212121212121212";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir(&worker_root).unwrap();
    let installed = worker_root.join("hel");
    let source = directory.path().join("new-worker");
    std::fs::write(&installed, b"running-worker").unwrap();
    std::fs::write(&source, stamped_worker(b"replacement-worker")).unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: worker_root.to_string_lossy().into_owned(),
    };
    let executor = targets::ProcessExecutor;
    assert!(
        stage_worker_binary_for_upgrade(
            &executor,
            &locator,
            session_id,
            &directory.path().join("missing")
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&installed).unwrap(), b"running-worker");
    let staging =
        stage_worker_binary_for_upgrade(&executor, &locator, session_id, &source).unwrap();
    assert_eq!(std::fs::read(&installed).unwrap(), b"running-worker");
    let abandoned =
        stage_worker_binary_for_upgrade(&executor, &locator, session_id, &source).unwrap();
    assert_ne!(&*staging, &*abandoned);
    let abandoned_path = worker_root.join(&*abandoned);
    assert!(abandoned_path.exists());
    drop(abandoned);
    assert!(!abandoned_path.exists());
    assert!(worker_root.join(&*staging).exists());
    assert_eq!(std::fs::read(&installed).unwrap(), b"running-worker");
    let owner = crate::worker_lifecycle::WorkerPermit::try_acquire(session_id, "test upgrade")
        .unwrap()
        .unwrap();
    install_staged_worker_binary(&owner, &staging, &executor, &locator, session_id).unwrap();
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        stamped_worker(b"replacement-worker")
    );
}

/// The incident on precision-3260: every failed upload left a truncated
/// `hel.prepared-*.next` behind, 39 of them in one worker root. A failed
/// upload now removes its own partial file, and the next staging removes any
/// left by an earlier daemon, while a staging still in use is kept.
#[cfg(unix)]
// Hard-won: f54a670: a full disk left partial worker uploads and truncated files
#[test]
fn failed_worker_staging_leaves_no_partial_upload_and_sweeps_stale_ones() {
    /// Runs commands for real, except that the upload writes 32 KiB of its
    /// file and then fails the way scp does on a full disk.
    struct FullDiskUpload {
        fail: std::cell::Cell<bool>,
    }
    impl CommandExecutor for FullDiskUpload {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if self.fail.get() && command.purpose == "stage replacement Mjolnir worker" {
                std::fs::write(&command.args[1], vec![0; 32 * 1024]).unwrap();
                return Ok(CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: b"cp: error writing: No space left on device".to_vec(),
                });
            }
            targets::ProcessExecutor.execute(command)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let session_id = "34343434343434343434343434343434";
    let worker_root = directory.path().join(session_id);
    std::fs::create_dir(&worker_root).unwrap();
    let source = directory.path().join("new-worker");
    std::fs::write(&source, stamped_worker(b"replacement-worker")).unwrap();
    let stale = worker_root.join("hel.prepared-upgrade-stage-0123.next");
    std::fs::write(&stale, vec![0; 32 * 1024]).unwrap();
    std::fs::write(worker_root.join("hel"), b"running-worker").unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: worker_root.to_string_lossy().into_owned(),
    };
    let executor = FullDiskUpload {
        fail: std::cell::Cell::new(false),
    };
    let held = stage_worker_binary_for_upgrade(&executor, &locator, session_id, &source).unwrap();
    assert!(!stale.exists(), "a stale partial upload survived staging");
    executor.fail.set(true);
    let error = stage_worker_binary_for_upgrade(&executor, &locator, session_id, &source)
        .err()
        .expect("the upload fails");
    assert!(format!("{error:#}").contains("No space left on device"));
    let mut remaining = std::fs::read_dir(&worker_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    remaining.sort();
    assert_eq!(remaining, vec!["hel".to_owned(), held.to_string()]);
}
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

/// The stored choice selects delegation for supported parent harnesses, but
/// a child never receives its parent's Mjolnir delegation tools.
#[test]
fn the_session_choice_decides_whether_mjolnir_replaces_native_delegation() {
    let claude = |choice: Option<bool>| {
        let mut session = crate::controller::test_support::checkpoint_test_session("s-1");
        session.harness_kind = HarnessKind::Claude;
        session.subagents = choice.map(|enabled| {
            if enabled {
                mj_core::subagent::SubagentPolicy::AllModels
            } else {
                mj_core::subagent::SubagentPolicy::Native
            }
        });
        session
    };

    assert!(!subagent_tools_enabled(&claude(Some(false)), false));
    assert!(subagent_tools_enabled(&claude(Some(true)), false));
    assert!(!subagent_tools_enabled(&claude(None), false));
    assert!(!subagent_tools_enabled(&claude(Some(true)), true));

    let mut grok = claude(Some(true));
    grok.harness_kind = HarnessKind::Grok;
    assert!(!subagent_tools_enabled(&grok, false));

    let mut codex = claude(None);
    codex.harness_kind = HarnessKind::Codex;
    assert!(!subagent_tools_enabled(&codex, false));
    codex.subagents = Some(mj_core::subagent::SubagentPolicy::AllModels);
    assert!(subagent_tools_enabled(&codex, false));
}

/// On a host with neither Codex nor Node.js, the launch failed with "Codex
/// launch preflight failed on local host; Node.js 22+ and npm must be
/// available on the target PATH: Node.js is missing from PATH", which never
/// says Codex is missing (launch finding R13-1). The failure now starts by
/// saying so, with the install command `mj login` gives.
#[cfg(unix)]
// Hard-won: f8b8002: missing Codex was reported only as an incidental Node failure
#[test]
fn node_preflight_says_the_agent_is_not_installed_before_it_mentions_node() {
    let directory = tempfile::tempdir().unwrap();
    let profile = |kind| HarnessProfile {
        enabled: true,
        kind,
        home: directory.path().into(),
        environment: std::collections::BTreeMap::from([(
            "PATH".into(),
            directory.path().to_string_lossy().into_owned(),
        )])
        .into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };
    let failure = |kind| {
        format!(
            "{:#}",
            preflight_harness(
                &mj_core::config::TargetTemplate::LocalBare,
                &profile(kind),
                &ProcessExecutor,
            )
            .unwrap_err()
        )
    };

    let codex = failure(HarnessKind::Codex);
    assert!(
        codex.starts_with(
            "Codex is not installed on local host: `codex` is not on PATH. Install it with \
             `npm install -g @openai/codex` (Node.js 22 or newer), sign in to it, then retry \
             the launch: Node.js is missing from PATH"
        ),
        "{codex}"
    );
    let claude = failure(HarnessKind::Claude);
    assert!(
        claude.starts_with(
            "Claude Code is not installed on local host: `claude` is not on PATH. Install it \
             with `npm install -g @anthropic-ai/claude-code`, sign in to it, then retry the \
             launch: Node.js is missing from PATH"
        ),
        "{claude}"
    );

    // With the agent's own command installed, only Node.js is missing, and
    // the failure says that as before.
    mj_core::test_hooks::install_fake_command(directory.path(), "codex", "#!/bin/sh\nexit 0\n");
    let codex = failure(HarnessKind::Codex);
    assert!(
        codex.starts_with(
            "Codex launch preflight failed on local host; Node.js 22+ and npm must be available \
             on the target PATH: Node.js is missing from PATH"
        ),
        "{codex}"
    );
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
fn pinned_snapshot_survives_source_replacement_and_missing_candidate_install() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, stamped_worker(b"before")).unwrap();
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
    assert_eq!(std::fs::read(path).unwrap(), stamped_worker(b"before"));

    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, stamped_worker(b"after")).unwrap();
    std::fs::rename(replacement, &source).unwrap();
    let WorkerBinaryAvailability::Local { path, .. } = pinned
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be local");
    };
    assert_eq!(std::fs::read(path).unwrap(), stamped_worker(b"before"));
    let fresh_replaced = WorkerBinarySourceSnapshot::capture(&cache, resolve_source);
    let WorkerBinaryAvailability::Local { path, .. } = fresh_replaced
        .resolve("x86_64", WorkerBinaryRequirement::PortableLinux)
        .unwrap()
    else {
        panic!("source should be local");
    };
    assert_eq!(std::fs::read(path).unwrap(), stamped_worker(b"after"));

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
    std::fs::write(&missing, stamped_worker(b"now installed")).unwrap();
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

    let remote_url = std::sync::Mutex::new("https://old.example/{target}".to_owned());
    let remote_snapshot =
        WorkerBinarySourceSnapshot::capture(&directory.path().join("remote-cache"), |arch, _| {
            Ok(WorkerBinaryAvailability::Remote {
                url: remote_url.lock().unwrap().replace("{target}", arch),
                sha256: "a".repeat(64),
                triple: format!("{arch}-unknown-linux-musl"),
            })
        });
    *remote_url.lock().unwrap() = "https://new.example/{target}".into();
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
        detail.contains("no worker for riscv64-unknown-linux-musl"),
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
        detail.contains("no worker for riscv64-unknown-linux-musl"),
        "{detail}"
    );
    assert!(!detail.contains("restart the Mjolnir daemon"), "{detail}");
}

/// The worker writes its records without a trailing newline. The probe read
/// them out of a text dump by searching for the next section marker, which
/// that missing newline hid, so every startup step read as none and the
/// readiness wait never extended for a worker that was making progress. The
/// probe now runs the real script and reads one JSON document.
///
/// The root contains spaces because macOS puts worker roots under
/// `~/Library/Application Support/...`; an unquoted root split the script
/// into separate words.
// Hard-won: 2fd45e3: worker records without a trailing newline were mistaken for empty output
#[test]
fn probing_a_dead_worker_reads_its_records_as_the_worker_wrote_them() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Application Support").join("hel worker");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(mj_core::relay::WORKER_STARTUP_FILE),
        br#"{
  "step": "harness-resolve",
  "pid": 999999999,
  "steps": [
    { "step": "start", "at": "2026-09-30T03:20:42.270Z" },
    { "step": "harness-resolve", "at": "2026-09-30T03:20:44.173Z" }
  ]
}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(mj_core::relay::WORKER_EXIT_FILE),
        b"{\n  \"reason\": \"panic\",\n  \"refusal\": null\n}",
    )
    .unwrap();
    let root = root.to_str().unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: root.into(),
    };

    let probe = probe_worker(&ProcessExecutor, &locator, root).unwrap();

    assert_eq!(probe.step(), Some("harness-resolve"));
    assert_eq!(
        probe.exit.as_ref().map(|exit| exit.reason.as_str()),
        Some("panic")
    );
    // No worker runs for this temporary root.
    assert!(!probe.alive(), "{probe:?}");
    assert_eq!(probe.to_string(), "the worker exited: panic");
}

/// A running worker is found by the process for its root, and a root with
/// no records yet reads as a running worker that recorded nothing.
#[test]
fn probing_a_running_worker_reports_its_process() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("hel worker");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.to_str().unwrap();
    // The trailing `:` keeps `sh` from replacing itself with `sleep`, so its
    // command line keeps the worker's arguments.
    let worker = Child(
        std::process::Command::new("sh")
            .args([
                "-c",
                "sleep 60; :",
                &format!("hel worker run --root {root}"),
            ])
            .spawn()
            .unwrap(),
    );
    std::fs::write(
        std::path::Path::new(root).join(mj_core::relay::WORKER_PID_FILE),
        worker.0.id().to_string(),
    )
    .unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: root.into(),
    };

    let probe = probe_worker(&ProcessExecutor, &locator, root).unwrap();

    assert_eq!(probe.pids, vec![worker.0.id()]);
    assert_eq!(probe.step(), None);
    assert_eq!(
        probe.to_string(),
        "the worker is running and recorded no startup step"
    );
}

/// Output that is not the probe's document is an error, not a worker with
/// nothing to report.
#[test]
fn a_probe_that_prints_something_else_is_an_error() {
    struct GarbageExecutor;
    impl CommandExecutor for GarbageExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 0,
                stdout: b"{\"startup\":{\"step\":\"start\"}--- worker.log".to_vec(),
                stderr: Vec::new(),
            })
        }
    }
    let locator = targets::TargetLocator::LocalBare {
        worker_root: "/nonexistent".into(),
    };

    let error = probe_worker(&GarbageExecutor, &locator, "/nonexistent").unwrap_err();

    assert!(
        format!("{error:#}").contains("read the worker probe"),
        "{error:#}"
    );
}

/// A worker that died leaves an exit record behind. Starting a new worker
/// must clear it first, or the startup connect loop reads the previous
/// death as this worker's and gives up on a healthy daemon.
// Hard-won: a6ad3cc: concurrent checkpoint, upgrade, and recovery paths shared worker launch logs
#[test]
fn starting_a_worker_uses_private_logs_without_touching_incumbent_files() {
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
            worker_root: "/worker/root/13131313131313131313131313131313".into(),
        },
        targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: targets::resource_name("13131313131313131313131313131313").unwrap(),
            workspace_storage: Default::default(),
        },
    ] {
        let executor = RecordingExecutor {
            commands: RefCell::new(Vec::new()),
        };
        start_worker(
            &crate::worker_lifecycle::WorkerPermit::try_acquire(
                "13131313131313131313131313131313",
                "test start",
            )
            .unwrap()
            .unwrap(),
            &executor,
            &locator,
            &targets::worker_root(&locator, "13131313131313131313131313131313").unwrap(),
        )
        .unwrap();

        let commands = executor.commands.borrow();
        let script = commands
            .iter()
            .flat_map(|command| command.args.iter())
            .find(|argument| argument.contains("worker-launch.XXXXXXXX"))
            .expect("a launch has a private diagnostic log");
        assert!(
            !script.contains("rm -f"),
            "launchers cannot clear incumbent files: {script}"
        );
        assert!(!script.contains("worker-exit.json"));
        assert!(!script.contains("control.sock"));
        assert!(!script.contains("/worker.log"));
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
        workspace: "/workspace/14141414141414141414141414141414".into(),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    stop_worker(
        &crate::worker_lifecycle::WorkerPermit::try_acquire(
            "14141414141414141414141414141414",
            "test stop",
        )
        .unwrap()
        .unwrap(),
        &executor,
        &locator,
        &targets::worker_root(&locator, "14141414141414141414141414141414").unwrap(),
    )
    .unwrap();

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

    let session = "17171717171717171717171717171717";
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

    crate::worker_lifecycle::WorkerPermit::try_acquire(session, "test stop")
        .unwrap()
        .unwrap()
        .scope_blocking(|| {
            stop_worker_after_target_recovery(
                &executor,
                &locator,
                session,
                &targets::worker_root(&locator, session).unwrap(),
            )
        })
        .unwrap();

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
    std::fs::write(&worker_binary, stamped_worker(b"worker-binary-bytes")).unwrap();
    let launch_config = root.path().join("launch.json");
    std::fs::write(&launch_config, b"{}").unwrap();
    let ownership = root.path().join("ownership.json");
    std::fs::write(&ownership, b"{}").unwrap();
    let profile_stage = root.path().join("profile");
    std::fs::create_dir_all(&profile_stage).unwrap();
    let digest = lower_hex(Sha256::digest(stamped_worker(b"worker-binary-bytes")));
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
fn rendered(commands: &[CommandSpec]) -> Vec<String> {
    commands
        .iter()
        .map(|command| format!("{} {}", command.program, command.args.join(" ")))
        .collect()
}

/// An SSH host that remembers which files exist, so consecutive installs see
/// the cache an earlier install left behind. A `test -f` succeeds only for a
/// path the host holds, and an `mv` adds its destination, which is how a
/// completed upload enters the cache.
#[derive(Default)]
struct SshHostExecutor {
    commands: RefCell<Vec<CommandSpec>>,
    host_files: RefCell<HashSet<String>>,
}
impl CommandExecutor for SshHostExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(command.clone());
        let mut status = 0;
        if command.program == "ssh" {
            let remote = command.args.last().cloned().unwrap_or_default();
            let words = remote
                .split(' ')
                .map(|word| word.trim_matches('\''))
                .collect::<Vec<_>>();
            match words.as_slice() {
                ["test", "-f", path] if !self.host_files.borrow().contains(*path) => status = 1,
                ["mv", _, destination] => {
                    self.host_files
                        .borrow_mut()
                        .insert((*destination).to_owned());
                }
                _ => {}
            }
        }
        Ok(CommandOutput {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}
fn install_on_ssh_host(
    executor: &SshHostExecutor,
    fixture: &PodmanInstallFixture,
    locator: &targets::TargetLocator,
    session: &str,
) -> Vec<String> {
    let root = session_worker_root(session);
    install_worker_files(
        executor,
        locator,
        session,
        &root,
        &format!("{root}/profile"),
        &fixture.worker_binary,
        &fixture.launch_config,
        &fixture.ownership,
        &fixture.profile_stage,
    )
    .unwrap();
    rendered(&executor.commands.take())
}
/// The home-relative worker root SSH-bare and EC2 sessions use (see
/// `targets::worker_root`), used for every install here so the roots are easy
/// to name in assertions.
fn session_worker_root(session: &str) -> String {
    format!(".local/share/hel/workers/{session}")
}
/// An SSH-bare session on the same host as [`podman_install_fixture`]'s
/// container.
fn bare_locator_on_the_container_host(session: &str) -> targets::TargetLocator {
    targets::TargetLocator::SshBare {
        worker_id: None,
        ssh: SshTarget {
            destination: "user@example.test".into(),
            ssh_args: Vec::new(),
        },
        workspace: format!(".local/share/hel/workspaces/{session}"),
    }
}
/// Scp commands that carry the worker binary itself.
fn worker_uploads(lines: &[String], fixture: &PodmanInstallFixture) -> Vec<String> {
    let source = format!("{} ", fixture.worker_binary.display());
    lines
        .iter()
        .filter(|line| line.starts_with("scp ") && line.contains(&source))
        .cloned()
        .collect()
}
fn position_of(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("expected {needle:?} in {lines:#?}"))
}
/// R3-4 (J-16): every SSH-bare session uploaded its own 139 MB worker, so ten
/// parallel creates on one host each held a daemon action slot for minutes.
/// The binary now crosses the network once per build and host; each session
/// still gets its own copy in its own worker root.
// Hard-won: 62a232f: every SSH-bare session uploaded its full 139 MB worker and stalled creates
#[test]
fn ssh_bare_installs_upload_the_worker_once_per_host_and_copy_it_per_session() {
    let fixture = podman_install_fixture();
    let executor = SshHostExecutor::default();
    let first = "0123456789abcdef0123456789abcdef";
    let second = "fedcba9876543210fedcba9876543210";
    let cache_dir = format!(".cache/mjolnir/workers/{}", fixture.digest);
    let cached = format!("{cache_dir}/hel");
    assert_eq!(
        targets::worker_root(&bare_locator_on_the_container_host(first), first).unwrap(),
        session_worker_root(first)
    );

    let lines = install_on_ssh_host(
        &executor,
        &fixture,
        &bare_locator_on_the_container_host(first),
        first,
    );
    let partial = format!("{cache_dir}/hel.partial-{first}");
    assert_eq!(
        worker_uploads(&lines, &fixture),
        [format!(
            "scp {} user@example.test:{partial}",
            fixture.worker_binary.display()
        )],
        "the first session on a host uploads the worker once, to its own \
         partial name in the cache, got {lines:#?}"
    );
    let probe = position_of(&lines, &format!("'test' '-f' '{cached}'"));
    let publish = position_of(&lines, &format!("'mv' '{partial}' '{cached}'"));
    let root = session_worker_root(first);
    let copy = position_of(&lines, &format!("'cp' '{cached}' '{root}/hel'"));
    let executable = position_of(&lines, &format!("'chmod' '700' '{root}/hel'"));
    assert!(
        probe < publish && publish < copy && copy < executable,
        "probe, publish, copy, then chmod, got {lines:#?}"
    );

    let lines = install_on_ssh_host(
        &executor,
        &fixture,
        &bare_locator_on_the_container_host(second),
        second,
    );
    assert!(
        worker_uploads(&lines, &fixture).is_empty(),
        "the second session on the host must not upload the worker again, got {lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("'mv'")),
        "a cache hit renames nothing, got {lines:#?}"
    );
    position_of(&lines, &format!("'test' '-f' '{cached}'"));
    let root = session_worker_root(second);
    let copy = position_of(&lines, &format!("'cp' '{cached}' '{root}/hel'"));
    let executable = position_of(&lines, &format!("'chmod' '700' '{root}/hel'"));
    assert!(copy < executable, "copy before chmod, got {lines:#?}");
    for name in ["launch.json", "ownership.json"] {
        assert!(
            lines.iter().any(|line| line.starts_with("scp ")
                && line.ends_with(&format!("user@example.test:{root}/{name}"))),
            "{name} is still uploaded per session, got {lines:#?}"
        );
    }
}
// Hard-won: 62a232f: SSH-bare and SSH-container installs duplicated the host worker cache
#[test]
fn ssh_bare_and_ssh_container_installs_share_one_worker_cache() {
    let fixture = podman_install_fixture();
    let executor = SshHostExecutor::default();
    let bare = "0123456789abcdef0123456789abcdef";
    install_on_ssh_host(
        &executor,
        &fixture,
        &bare_locator_on_the_container_host(bare),
        bare,
    );

    let container = "fedcba9876543210fedcba9876543210";
    let lines = install_on_ssh_host(&executor, &fixture, &fixture.locator, container);
    assert!(
        worker_uploads(&lines, &fixture).is_empty(),
        "a container session on the same host reuses the cached worker, got {lines:#?}"
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
        let owner = crate::worker_lifecycle::WorkerPermit::try_acquire(
            &session,
            "test binary replacement",
        )?
        .context("test worker is owned")?;
        owner.scope_blocking(|| {
            replace_installed_worker_binary(
                &ProcessExecutor,
                &locator,
                &session,
                &fixture.worker_binary,
            )
        })?;
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
#[ignore = "requires Docker, agent-dev image, MJ_WORKER_BINARY and MJ_INSTANCE=issue-1138-docker"]
fn stopped_docker_session_recovers_with_the_current_worker_build() {
    assert_eq!(mj_core::config::instance_identity(), "issue-1138-docker");
    let source =
        PathBuf::from(std::env::var_os("MJ_WORKER_BINARY").expect("portable Linux worker"));
    verify_worker_build(&source).unwrap();
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
                "--label",
                "dev.mj.managed=true",
                "--label",
                &format!("dev.mj.session={session}"),
                "--label",
                "dev.mj.instance=issue-1138-docker",
                "ghcr.io/brokkai/mjolnir/agent-dev:latest",
                "sleep",
                "infinity",
            ],
        ),
    )
    .unwrap();
    let result = (|| -> Result<()> {
        let root = targets::worker_root(&locator, &session)?;
        execute_checked(
            &ProcessExecutor,
            targets::locator_command(
                &locator,
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "mkdir -p {root} && printf legacy > {root}/hel && printf old-config > {root}/launch.json"
                    ),
                ],
            ),
        )?;
        let relay = tempfile::tempdir()?;
        drop(mj_worker::relay::DurableRelay::open(
            relay.path(),
            &session,
            "2.0.0",
        )?);
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                [
                    "cp".to_owned(),
                    relay.path().join(".").to_string_lossy().into_owned(),
                    format!("{container_id}:{root}"),
                ],
            ),
        )?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new(
                "docker",
                container_upload_ownership_args("docker", &container_id, &root, &[&root]),
            ),
        )?;
        execute_checked(
            &ProcessExecutor,
            CommandSpec::new("docker", ["stop", &container_id]),
        )?;
        let launch: WorkerLaunchConfig = serde_json::from_value(serde_json::json!({
            "session_id": session, "harness": "codex", "bridge_command": "/not-used",
            "bridge_args": [], "environment": {}, "target_environment": {},
            "cwd": "/tmp", "execution_policy": "configured_approvals", "run_mode": "checkpoint_only"
        }))?;
        let plan = WorkerRecoveryPlan {
            source_target: mj_core::state::TargetLocator::LocalDocker {
                container_id: container_id.clone(),
                borrowed_from: None,
            },
            target: targets::target_recovery_plan(&locator, &session)?,
            workspace: None,
            exit_record: None,
            liveness_probe: worker_liveness_command(&locator, &root),
            binary_refresh: worker_binary_refresh_plan(&locator, &session)?,
            launch_refresh: Some(worker_launch_refresh_plan(&locator, &session, &launch)?),
            restart: CommandPlan {
                description: "restart isolated Docker worker".into(),
                commands: vec![start_worker_command(&locator, &root)],
            },
        };
        crate::session_manager::recover_worker_controlled(plan, false, None, &ProcessExecutor)?;
        let installed = execute_checked(
            &ProcessExecutor,
            installed_file_digest_command(
                &locator,
                &format!("{root}/hel"),
                "check recovered worker",
            ),
        )?;
        ensure!(
            String::from_utf8(installed.stdout)?
                .starts_with(&mj_core::worker_launch::worker_executable_digest(&source)?),
            "recovery did not install the current worker"
        );
        let reconnect = targets::reconnect_plan(&locator, &session)?
            .commands
            .remove(0);
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut connection = loop {
                match crate::worker_client::RelayClient::connect(&reconnect, &session).await {
                    Ok(connection) => break connection,
                    Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            };
            ensure!(
                connection.status().await?.checkpoint_only,
                "recovered worker must read the new launch configuration"
            );
            ensure!(
                connection.worker_build()
                    == Some(mj_core::worker_launch::worker_executable_digest(&source)?.as_str()),
                "the recovered process must run the installed build"
            );
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(())
    })();
    // Stop the owning container before removing its files, even on failure.
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

    let session = "16161616161616161616161616161616";
    let container_id = targets::resource_name(session).unwrap();
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: container_id.clone(),
        workspace_storage: Default::default(),
    };
    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let fixture = podman_install_fixture();
    let owner =
        crate::worker_lifecycle::WorkerPermit::try_acquire(session, "test binary replacement")
            .unwrap()
            .unwrap();
    owner
        .scope_blocking(|| {
            replace_installed_worker_binary(&executor, &locator, session, &fixture.worker_binary)
        })
        .unwrap();

    let mut lines = rendered(&executor.commands.borrow());
    let ownership = lines.remove(1);
    assert!(ownership.starts_with(&format!("podman exec --user 0:0 {container_id} sh -c")));
    assert!(ownership.contains("chown -R"));
    assert!(ownership.ends_with(&format!("/var/lib/hel/workers/{session}/hel.next")));
    assert_eq!(
        lines,
        vec![
            format!(
                "podman cp {} {container_id}:/var/lib/hel/workers/{session}/hel.next",
                fixture.worker_binary.display()
            ),
            format!(
                "podman exec {container_id} mv -f /var/lib/hel/workers/{session}/hel.next /var/lib/hel/workers/{session}/hel"
            ),
            format!("podman exec {container_id} chmod 700 /var/lib/hel/workers/{session}/hel"),
        ]
    );
}

#[test]
fn podman_worker_start_and_upgrade_use_each_containers_recorded_identity() {
    let session = "26262626262626262626262626262626";
    let source = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(source.path(), stamped_worker(b"replacement"))
        .expect("write a stamped replacement worker");
    let local_name = targets::resource_name(session).unwrap();
    let ssh = SshTarget {
        destination: "user@example.test".into(),
        ssh_args: Vec::new(),
    };
    let locators = [
        (
            "local",
            targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: local_name.clone(),
                workspace_storage: Default::default(),
            },
        ),
        (
            "ssh",
            targets::TargetLocator::SshPodman {
                borrowed_from: None,
                ssh,
                container_id: local_name.clone(),
                workspace_storage: Default::default(),
            },
        ),
    ];

    for (kind, locator) in locators {
        let start = start_worker_command(&locator, "/var/lib/hel/workers/session");
        let start_text = format!("{} {}", start.program, start.args.join(" "));
        assert!(
            !start_text.contains("--user"),
            "{kind} worker start must inherit the configured container user: {start_text}"
        );

        let upgrade =
            installed_worker_binary_replacement_plan(&locator, session, source.path()).unwrap();
        let commands = rendered(&upgrade.commands);
        let ownership = commands
            .iter()
            .find(|command| command.contains("chown -R"))
            .expect("the upload owner is normalized");
        assert!(
            ownership.contains("--user") && ownership.contains("0:0"),
            "{kind} ownership normalization still needs container root: {ownership}"
        );
        for command in commands.iter().filter(|command| command.contains("exec ")) {
            if !command.contains("chown -R") {
                assert!(
                    !command.contains("--user"),
                    "{kind} worker promotion must inherit the configured user: {command}"
                );
            }
        }
    }
}

#[test]
fn bridge_fallback_pins_match_the_agent_dev_containerfile() {
    use mj_core::harness_runtime::CLAUDE_CLI_VERSION;

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

    let claude_code = format!("@anthropic-ai/claude-code@{CLAUDE_CLI_VERSION}");
    assert!(
        CONTAINERFILE.contains(&claude_code),
        "containers/Containerfile.agent-dev must install {claude_code}"
    );
    assert!(CONTAINERFILE.contains("ENV CLAUDE_CODE_EXECUTABLE=/usr/local/bin/claude"));

    let package: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../mj-worker/assets/harnesses/claude/package.json"
    ))
    .unwrap();
    let lock: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../mj-worker/assets/harnesses/claude/package-lock.json"
    ))
    .unwrap();
    for (name, version) in [
        ("@agentclientprotocol/claude-agent-acp", CLAUDE_ACP_VERSION),
        ("@anthropic-ai/claude-code", CLAUDE_CLI_VERSION),
    ] {
        assert_eq!(package["dependencies"][name], version);
        assert_eq!(lock["packages"][""]["dependencies"][name], version);
        assert_eq!(
            lock["packages"][format!("node_modules/{name}")]["version"],
            version
        );
    }
}

/// Runs a default bridge script with no harness installed, a fake `curl`
/// that serves `installer`, and nothing else from the host's harnesses on
/// PATH. Returns the script's stdout and stderr.
#[cfg(unix)]
fn run_default_bridge_install(
    harness: mj_core::config::HarnessKind,
    installer: &str,
) -> (String, String) {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!("#!/bin/sh\ncat <<'INSTALLER'\n{installer}\nINSTALLER\n"),
    )
    .unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (command, arguments) = bridge_launch(harness, ExecutionPolicy::ConfiguredApprovals);
    let mut process = std::process::Command::new(command);
    process
        .args(arguments)
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let output = mj_core::subprocess::run_with_input(&mut process, &[]).unwrap();
    assert!(output.status.success(), "bridge script failed: {output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// A Kimi launcher's installer lines on stdout reached the ACP transport and
/// broke initialize (#1136). The installer's output goes to stderr, which the
/// worker keeps, and the ACP server starts with a clean stdout.
#[cfg(unix)]
// Hard-won: 2fe3b29: Kimi installer output contaminated ACP stdout
#[test]
fn kimi_default_bridge_sends_installer_output_to_stderr() {
    let (stdout, stderr) = run_default_bridge_install(
        mj_core::config::HarnessKind::Kimi,
        r#"echo "==> Detected target: linux-x64"
echo "==> Installing $KIMI_VERSION"
mkdir -p "$HOME/.kimi-code/bin"
printf '#!/bin/sh\necho "{\\"jsonrpc\\":\\"2.0\\",\\"args\\":\\"$*\\"}"\n' > "$HOME/.kimi-code/bin/kimi"
chmod +x "$HOME/.kimi-code/bin/kimi""#,
    );
    assert_eq!(stdout, "{\"jsonrpc\":\"2.0\",\"args\":\"acp\"}\n");
    assert!(
        stderr.contains("==> Detected target: linux-x64"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "==> Installing {}",
            mj_core::harness_runtime::KIMI_VERSION
        )),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn grok_default_bridge_sends_installer_output_to_stderr() {
    let (stdout, stderr) = run_default_bridge_install(
        mj_core::config::HarnessKind::Grok,
        r#"echo "==> Installing $1"
mkdir -p "$HOME/.grok/bin"
printf '#!/bin/sh\necho "{\\"jsonrpc\\":\\"2.0\\",\\"args\\":\\"$*\\"}"\n' > "$HOME/.grok/bin/grok"
chmod +x "$HOME/.grok/bin/grok""#,
    );
    assert_eq!(stdout, "{\"jsonrpc\":\"2.0\",\"args\":\"agent stdio\"}\n");
    assert!(
        stderr.contains(&format!(
            "==> Installing {}",
            mj_core::harness_runtime::GROK_VERSION
        )),
        "{stderr}"
    );
}

#[test]
fn project_memory_replica_is_separate_from_controller_attachment_directories() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    for (index, kind) in HarnessKind::ALL.into_iter().enumerate() {
        let session_id = format!("{:032x}", index + 1);
        let worker_root = directory.path().join("workers").join(&session_id);
        let backend = targets::TargetLocator::LocalBare {
            worker_root: worker_root.to_string_lossy().into_owned(),
        };
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: directory.path().join(format!("{}-home", kind.id())),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };
        let mut session = crate::controller::test_support::checkpoint_test_session(&session_id);
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.clone());
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });

        let (launch, memory, _) = worker_launch_config(
            &session,
            &profile,
            None,
            &backend,
            LaunchWorkspace {
                session_id: &session_id,
                container: None,
                parent_worktree: None,
            },
            &mj_core::state::TargetRuntimeSettings::from(
                &mj_core::config::TargetTemplate::LocalBare,
            ),
        )
        .unwrap();

        assert!(
            !launch.additional_directories.contains(&memory.root),
            "{kind:?} keeps the target-local memory replica out of attachment directories"
        );
        assert_eq!(
            launch.project_memory.as_ref().unwrap().root,
            memory.root,
            "{kind:?} retains its memory replica in the dedicated launch field"
        );
    }
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
        environment: BTreeMap::from([("ZAI_API_KEY".to_owned(), "coding-plan-key".to_owned())])
            .into(),
        context_window_bytes: None,
        subagents: Default::default(),
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
    // The key is top-level, so Codex reads it, and the user's own lines survive.
    let config = std::fs::read_to_string(staged.path().join("config.toml")).unwrap();
    assert_eq!(
        config.matches("model_catalog_json").count(),
        1,
        "exactly one top-level key names the catalog: {config}"
    );
    assert!(
        config.contains("model_catalog_json = \"models.json\""),
        "{config}"
    );
    for line in ZAI_CONFIG.lines().filter(|line| !line.is_empty()) {
        assert!(config.contains(line), "missing {line:?} in {config}");
    }
    assert_eq!(
        mj_core::codex_provider::codex_provider(staged.path())
            .unwrap()
            .unwrap()
            .model_catalog_json
            .as_deref(),
        Some(Path::new("models.json")),
        "Codex reads the staged catalog as a top-level key"
    );
    // The staged copy is what the session runs from, on every target.
    assert!(
        !home.path().join("models.json").exists(),
        "the user's own profile home stays untouched"
    );
}

// Hard-won: ee59e57: a valid profile catalog key caused validation failure and a duplicate staged key
#[test]
fn a_profile_authored_catalog_is_discarded_and_the_staged_key_replaces_its_own() {
    let home = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let profile = deepseek_profile(home.path());
    // A catalog of the profile's own, named the way Codex names one.
    std::fs::write(
        home.path().join("mine.models.json"),
        r#"{"models":[
            {"slug":"deepseek-v4-pro","supported_reasoning_levels":["low","high","max"]},
            {"slug":"deepseek-private","display_name":"DeepSeek Private"}
        ]}"#,
    )
    .unwrap();
    let authored = format!(
        "model_catalog_json = \"mine.models.json\"\n{}",
        std::fs::read_to_string(home.path().join("config.toml")).unwrap()
    );
    std::fs::write(home.path().join("config.toml"), &authored).unwrap();

    stage_profile(&profile, staged.path()).unwrap();
    stage_codex_catalog(
        "deepseek",
        &profile,
        staged.path(),
        &|_, _| Ok(DEEPSEEK_LIST.as_bytes().to_vec()),
        &IsolatedCatalogCache(store.path().join("cache.sqlite3")),
    )
    .expect("a profile-authored catalog is merged, not refused");

    let catalog =
        mj_core::codex_catalog::parse(&std::fs::read(staged.path().join("models.json")).unwrap())
            .unwrap();
    assert_eq!(
        catalog.slugs(),
        ["deepseek-flash", "deepseek-v4-pro"],
        "the file named by model_catalog_json is ignored; only the provider's list and the profile's models.json shape the staged catalog"
    );
    let config = std::fs::read_to_string(staged.path().join("config.toml")).unwrap();
    assert_eq!(
        config.matches("model_catalog_json").count(),
        1,
        "the staged copy has one key, not a duplicate Codex would reject: {config}"
    );
    assert!(
        config.contains("model_catalog_json = \"models.json\""),
        "{config}"
    );
    assert!(
        !config.contains("mine.models.json"),
        "no staged key points into the profile's own home: {config}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        authored,
        "the profile's own file is never modified"
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

// Hard-won: 55b8ea7: provider catalog and discovered model rows evicted each other from SQLite
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
        environment: BTreeMap::from([("DEEPSEEK_API_KEY".to_owned(), "deepseek-key".to_owned())])
            .into(),
        context_window_bytes: None,
        subagents: Default::default(),
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert_eq!(
        std::fs::read_to_string(staged.path().join(".claude.json")).unwrap(),
        identity
    );
}

#[cfg(unix)]
// Hard-won: c607200: symlinked Claude settings and instructions were silently dropped
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();
    stage_managed_skills(
        profile.kind,
        staged.path(),
        mj_core::skills::SkillsScope::Localhost,
    )
    .unwrap();

    let expected = mj_core::skills::session_skills(
        profile.kind,
        home.path(),
        mj_core::skills::SkillsArchiveFormat::Gzip,
        mj_core::skills::SkillsScope::Localhost,
    )
    .unwrap();
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

#[test]
fn isolated_profile_staging_removes_the_host_cli_skill_and_matches_sync() {
    for kind in HarnessKind::ALL {
        let home = tempfile::tempdir().unwrap();
        let directory = mj_core::skills::mj_skill_directory(kind);
        let mj = home.path().join(&directory);
        std::fs::create_dir_all(mj.join("references")).unwrap();
        std::fs::write(mj.join("SKILL.md"), "old mj").unwrap();
        std::fs::write(mj.join("references/old.md"), "old reference").unwrap();
        let other = home
            .path()
            .join(kind.synced_skill_dirs()[0])
            .join("review/SKILL.md");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "review").unwrap();
        let mut profile = zai_profile(home.path());
        profile.kind = kind;
        let stage = tempfile::tempdir().unwrap();
        stage_profile(&profile, stage.path()).unwrap();
        stage_managed_skills(kind, stage.path(), mj_core::skills::SkillsScope::Isolated).unwrap();
        assert!(!stage.path().join(&directory).exists(), "{kind:?}");
        let installed = mj_core::skills::collect_skills(kind, stage.path()).unwrap();
        let expected = mj_core::skills::session_skills(
            kind,
            home.path(),
            mj_core::skills::SkillsArchiveFormat::Gzip,
            mj_core::skills::SkillsScope::Isolated,
        )
        .unwrap();
        assert_eq!(installed, expected, "{kind:?}");
        assert_eq!(
            std::fs::read(stage.path().join(other.strip_prefix(home.path()).unwrap())).unwrap(),
            b"review"
        );
    }
}

/// Claude Code provisions `skills/synced/` from the user's claude.ai account,
/// and keeps `skills/.trash/`, in whatever home it runs from, the session's
/// included; the Codex CLI does the same with its built-in skills in
/// `skills/.system/`. Launch leaves these to the harness rather than copying
/// them into every session (4 MB of them for Claude on the launch host).
#[test]
fn staging_leaves_harness_owned_skills_to_the_harness() {
    use mj_core::config::HarnessKind;
    for kind in HarnessKind::ALL {
        let owned: &[&str] = match kind {
            HarnessKind::Claude => &[
                "skills/synced/.bucket-org_user",
                "skills/synced/org_user/manifest.json",
                "skills/synced/org_user/docx/SKILL.md",
                "skills/.trash/1789646711611/pdf/SKILL.md",
            ],
            HarnessKind::Codex => &[
                "skills/.system/.codex-system-skills.marker",
                "skills/.system/imagegen/SKILL.md",
            ],
            HarnessKind::Kimi | HarnessKind::Grok | HarnessKind::Muse | HarnessKind::OpenCode => {
                &[]
            }
        };
        let home = tempfile::tempdir().unwrap();
        for relative in std::iter::once(&"skills/review/SKILL.md").chain(owned) {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{relative}\n")).unwrap();
        }

        let staged = tempfile::tempdir().unwrap();
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: home.path().to_path_buf(),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();
        stage_managed_skills(
            profile.kind,
            staged.path(),
            mj_core::skills::SkillsScope::Localhost,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(staged.path().join("skills/review/SKILL.md")).unwrap(),
            "skills/review/SKILL.md\n",
            "{kind:?}"
        );
        for path in kind.harness_owned_skill_paths() {
            assert!(
                owned
                    .iter()
                    .any(|relative| relative.starts_with(&format!("{path}/"))),
                "no test file under {kind:?} {path}"
            );
            assert!(!staged.path().join(path).exists(), "{kind:?} {path}");
        }
        for relative in owned {
            assert!(
                !staged.path().join(relative).exists(),
                "{kind:?} {relative}"
            );
        }
        let expected = mj_core::skills::session_skills(
            profile.kind,
            home.path(),
            mj_core::skills::SkillsArchiveFormat::Gzip,
            mj_core::skills::SkillsScope::Localhost,
        )
        .unwrap();
        let installed = mj_core::skills::collect_skills(profile.kind, staged.path()).unwrap();
        assert_eq!(installed, expected, "{kind:?}");
    }
}

/// Launch finding R4-8: a profile home linked `skills/tufte-viz` from
/// elsewhere, and the linked skill held a 2.2 MB demo. Staging copied both
/// through the link; the session's worker then failed every skills poll on the
/// large file, and the sync's own copy of the home did not read through the
/// link at all. Stage and sync now agree, so the first sync neither fails nor
/// removes the linked skill. A large file that compresses under the per-file
/// limit is part of both trees; one that does not is left out of both.
#[cfg(unix)]
// Hard-won: 17a0618: a linked oversized skill made worker sync fail repeatedly
#[test]
fn staging_and_sync_agree_on_linked_and_oversized_skills() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("viz/demos")).unwrap();
    std::fs::write(outside.path().join("viz/SKILL.md"), "viz skill\n").unwrap();
    std::fs::write(
        outside.path().join("viz/demos/sunspot-pretty.html"),
        "<tr><td>1749-01</td><td>96.7</td></tr>\n".repeat(60_000),
    )
    .unwrap();
    let mut incompressible =
        vec![0; usize::try_from(mj_core::skills::MAX_SKILLS_FILE_BYTES).unwrap() + 200_000];
    getrandom::fill(&mut incompressible).unwrap();
    std::fs::write(outside.path().join("viz/demos/large.bin"), incompressible).unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills/review")).unwrap();
    std::fs::write(home.path().join("skills/review/SKILL.md"), "review skill\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("viz"), home.path().join("skills/viz")).unwrap();

    let staged = tempfile::tempdir().unwrap();
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: mj_core::config::HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();
    stage_managed_skills(
        profile.kind,
        staged.path(),
        mj_core::skills::SkillsScope::Localhost,
    )
    .unwrap();

    let expected = mj_core::skills::session_skills(
        profile.kind,
        home.path(),
        mj_core::skills::SkillsArchiveFormat::Gzip,
        mj_core::skills::SkillsScope::Localhost,
    )
    .unwrap();
    let installed = mj_core::skills::collect_skills(profile.kind, staged.path()).unwrap();
    assert_eq!(installed, expected);
    assert!(
        expected
            .entries()
            .iter()
            .any(|entry| entry.path == "skills/viz/SKILL.md"),
        "the linked skill is part of the canonical tree"
    );
    assert!(
        expected
            .entries()
            .iter()
            .any(|entry| entry.path == "skills/viz/demos/sunspot-pretty.html"),
        "the large page compresses under the limit and is part of both sides"
    );
    assert!(
        !expected
            .entries()
            .iter()
            .any(|entry| entry.path == "skills/viz/demos/large.bin"),
        "the oversized file is left out of both sides"
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    stage_profile(&profile, staged.path()).unwrap();

    assert!(!staged.path().join("CLAUDE.md").exists());
    assert!(staged.path().join("settings.json").is_file());
}

/// `[jev] enabled = false` reaches the worker as its launch environment and
/// takes the Jev key out of both the worker's and the harness's environment.
#[test]
fn the_jev_switch_reaches_the_worker_and_removes_the_key() {
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
    let mut launch = worker_launch_config(
        &session,
        &profile,
        Some(&bundle),
        &locator,
        LaunchWorkspace {
            session_id,
            container: Some(&workspace),
            parent_worktree: None,
        },
        &mj_core::state::TargetRuntimeSettings::from(&template),
    )
    .unwrap()
    .0;
    for environment in [&mut launch.target_environment, &mut launch.environment] {
        environment.insert("TYPESAFE_API_KEY".into(), "secret".into());
    }

    let mut on = launch.clone();
    apply_jev_switch(&mut on, true);
    assert_eq!(on.target_environment, launch.target_environment);
    assert_eq!(on.environment, launch.environment);

    apply_jev_switch(&mut launch, false);
    for environment in [&launch.target_environment, &launch.environment] {
        assert!(!environment.contains_key("TYPESAFE_API_KEY"));
        assert_eq!(
            environment.get(mj_core::jev::DISABLED_ENVIRONMENT),
            Some(&"1".to_owned())
        );
    }

    // The continuation switch travels the same way, and only when off.
    let mut continuing = launch.clone();
    super::launch::apply_continuation_switch(&mut continuing, true);
    assert_eq!(continuing.environment, launch.environment);
    super::launch::apply_continuation_switch(&mut launch, false);
    for environment in [&launch.target_environment, &launch.environment] {
        assert_eq!(
            environment.get(mj_core::jev::CONTINUATION_DISABLED_ENVIRONMENT),
            Some(&"1".to_owned())
        );
    }
}

#[test]
fn installing_the_build_cache_places_marked_shared_copy_shims_on_the_session_path() {
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

    let executor = RecordingExecutor::default();
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "hel-session".into(),
        workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
    };

    install_mbx_shims(
        &executor,
        &locator,
        "/home/hel/.hel/worker",
        Path::new("/mnt/fast/mbx-cache/.mjolnir/bin/mbx"),
        Path::new("/cache/.mjolnir/config/mbx"),
        &[],
    )
    .unwrap();

    let commands = executor.commands();
    let shim = commands
        .iter()
        .find(|line| line.contains("MBX_CARGO_SHIM_MODE=1"))
        .expect("the supported mbx Cargo launcher is written in the container");
    assert!(
        shim.contains("MBX_CARGO_SHIM_PATH=$(command -v \"$0\")"),
        "{shim}"
    );
    assert!(shim.contains("# mjolnir-mbx-shim"), "{shim}");
    assert!(
        shim.contains("exec '/mnt/fast/mbx-cache/.mjolnir/bin/mbx' \"$@\""),
        "both wrappers execute the synchronized cache copy: {shim}"
    );
    assert!(!commands.iter().any(|line| line.contains(" cp ")));
    assert!(
        commands.iter().any(|line| {
            line.contains("exec -i hel-session sh -c") && line.contains("config/mbx")
        }),
        "the machine mbx configuration is linked into the container: {commands:#?}"
    );
}

#[test]
fn container_mbx_version_must_match_the_synced_copy_before_shim_installation() {
    #[derive(Default)]
    struct VersionExecutor {
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl CommandExecutor for VersionExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(format!(
                "{} {}",
                command.program,
                command.args.join(" ")
            ));
            Ok(CommandOutput {
                status: 0,
                stdout: b"mbx 1.23.0".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let executor = VersionExecutor::default();
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "hel-session".into(),
        workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
    };
    let error = verify_mbx_binary(
        &executor,
        &locator,
        Path::new("/mnt/fast/mbx-cache/.mjolnir/bin/mbx"),
        "1.22.0",
    )
    .unwrap_err();
    assert!(error.to_string().contains("container reports mbx 1.23.0"));
    let commands = executor.commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    assert!(
        commands[0]
            .contains("podman exec hel-session /mnt/fast/mbx-cache/.mjolnir/bin/mbx --version")
    );
}
/// A Codex profile whose `auth.json` records this `auth_mode`, and whose own
/// environment sets an API key.
fn codex_login_profile(home: &Path, auth_mode: &str) -> mj_core::config::HarnessProfile {
    std::fs::write(
        home.join("auth.json"),
        serde_json::json!({"auth_mode": auth_mode, "OPENAI_API_KEY": null}).to_string(),
    )
    .unwrap();
    mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.to_path_buf(),
        environment: BTreeMap::from([
            ("OPENAI_API_KEY".to_owned(), "sk-svcacct-profile".to_owned()),
            ("PROFILE_SETTING".to_owned(), "kept".to_owned()),
        ])
        .into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    }
}

/// The launch config of one session of `profile` on this machine, in its own
/// container, and as a sub-agent child in its parent's container. Every target
/// sets `CODEX_API_KEY` and `OPENAI_BASE_URL` in its own environment.
fn launches_on_every_target(
    profile: &mj_core::config::HarnessProfile,
) -> Vec<(&'static str, mj_core::worker_launch::WorkerLaunchConfig)> {
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "1123456789abcdef0123456789abcdef";
    let parent_workspace = targets::new_container_workspace(parent_id).unwrap();
    let bundle = crate::controller::test_support::local_bundle(Path::new("/src/project"));
    let container = mj_core::config::TargetTemplate::LocalPodman {
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
    let with_target_keys = |template: &mj_core::config::TargetTemplate| {
        let mut settings = mj_core::state::TargetRuntimeSettings::from(template);
        settings
            .environment
            .insert("CODEX_API_KEY".into(), "sk-target".into());
        settings.environment.insert(
            "OPENAI_BASE_URL".into(),
            "https://example.invalid/v1".into(),
        );
        settings
    };

    let project = tempfile::tempdir().unwrap();
    let mut local = crate::controller::test_support::checkpoint_test_session(parent_id);
    local.target_template_id = "localhost".into();
    local.project_directory = Some(project.path().to_path_buf());
    let local_root = format!("/home/me/.local/share/hel/workers/{parent_id}");
    local.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: local_root.clone().into(),
    });
    let localhost = worker_launch_config(
        &local,
        profile,
        None,
        &targets::TargetLocator::LocalBare {
            worker_root: local_root,
        },
        LaunchWorkspace {
            session_id: parent_id,
            container: None,
            parent_worktree: None,
        },
        &with_target_keys(&mj_core::config::TargetTemplate::LocalBare),
    )
    .unwrap()
    .0;

    let mut parent = crate::controller::test_support::checkpoint_test_session(parent_id);
    parent.project_directory = None;
    parent.container_workspace = Some(parent_workspace.clone());
    let in_container = worker_launch_config(
        &parent,
        profile,
        Some(&bundle),
        &targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: targets::resource_name(parent_id).unwrap(),
            workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
        },
        LaunchWorkspace {
            session_id: parent_id,
            container: Some(&parent_workspace),
            parent_worktree: None,
        },
        &with_target_keys(&container),
    )
    .unwrap()
    .0;

    let mut child = crate::controller::test_support::checkpoint_test_session(child_id);
    child.project_directory = None;
    child.container_workspace = Some(parent_workspace.clone());
    let as_child = worker_launch_config(
        &child,
        profile,
        Some(&bundle),
        &targets::TargetLocator::LocalPodman {
            borrowed_from: Some(parent_id.into()),
            container_id: targets::resource_name(parent_id).unwrap(),
            workspace_storage: targets::PodmanWorkspaceLocator::ContainerLayer,
        },
        LaunchWorkspace {
            session_id: parent_id,
            container: Some(&parent_workspace),
            parent_worktree: None,
        },
        &with_target_keys(&container),
    )
    .unwrap()
    .0;
    vec![
        ("localhost", localhost),
        ("container", in_container),
        ("sub-agent", as_child),
    ]
}

#[test]
fn podman_harnesses_inherit_container_home_and_claude_gets_the_sandbox_marker() {
    let home = tempfile::tempdir().unwrap();
    let codex = codex_login_profile(home.path(), "chatgpt");
    for (target, launch) in launches_on_every_target(&codex) {
        if target == "localhost" {
            assert!(!launch.environment.contains_key("HOME"));
            assert!(
                !launch
                    .environment
                    .contains_key(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
            );
            continue;
        }
        assert!(!launch.target_environment.contains_key("HOME"), "{target}");
        assert!(!launch.environment.contains_key("HOME"), "{target}");
        assert_eq!(
            launch
                .target_environment
                .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
                .map(String::as_str),
            Some("/home/hel/.gitconfig"),
            "{target}"
        );
        assert_eq!(
            launch
                .environment
                .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
                .map(String::as_str),
            Some("/home/hel/.gitconfig"),
            "{target}"
        );
        assert!(!launch.environment.contains_key("IS_SANDBOX"), "{target}");
    }

    let claude = mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Claude,
        home: home.path().to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };
    for (target, launch) in launches_on_every_target(&claude) {
        if target == "localhost" {
            assert!(!launch.environment.contains_key("IS_SANDBOX"));
            continue;
        }
        assert!(!launch.environment.contains_key("HOME"), "{target}");
        assert_eq!(
            launch
                .environment
                .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
                .map(String::as_str),
            Some("/home/hel/.gitconfig"),
            "{target}"
        );
        assert_eq!(launch.environment["IS_SANDBOX"], "1", "{target}");
    }
}

#[test]
fn only_localhost_harnesses_receive_the_owning_daemons_configuration_paths() {
    let home = tempfile::tempdir().unwrap();
    let profile = codex_login_profile(home.path(), "chatgpt");
    for (target, launch) in launches_on_every_target(&profile) {
        if target == "localhost" {
            assert_eq!(
                launch.environment["MJ_CONFIG_DIR"],
                std::path::absolute(mj_core::config::config_dir())
                    .unwrap()
                    .to_string_lossy()
            );
            assert_eq!(
                launch.environment["MJ_DATA_DIR"],
                std::path::absolute(data_dir()).unwrap().to_string_lossy()
            );
            assert_eq!(
                launch.environment.get("MJ_INSTANCE"),
                mj_core::config::instance_name().as_ref()
            );
        } else {
            for name in ["MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE"] {
                assert!(!launch.environment.contains_key(name), "{target}: {name}");
            }
        }
    }
}

/// #1160: eleven Codex children of a ChatGPT profile died on their first
/// request, some with a service-account API key the ChatGPT backend rejected.
/// A ChatGPT profile's harness environment carries no such key on any target,
/// and the launch tells the worker to remove the same variables from the
/// target's own login environment, which only the worker sees.
// Hard-won: c4e2838: shipped ChatGPT Codex children failed when an inherited API key reached chatgpt.com
#[test]
fn a_chatgpt_codex_launch_carries_no_api_key_on_any_target() {
    let home = tempfile::tempdir().unwrap();
    let profile = codex_login_profile(home.path(), "chatgpt");
    for (target, launch) in launches_on_every_target(&profile) {
        for name in mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT {
            assert!(
                !launch.environment.contains_key(name),
                "{target}: the harness environment still sets {name}"
            );
        }
        assert_eq!(
            launch.excluded_environment,
            mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT.map(str::to_owned),
            "{target}: the worker is not told what to remove"
        );
        assert_eq!(launch.environment["PROFILE_SETTING"], "kept", "{target}");
    }
}

/// A Codex profile that signs in with an API key keeps the variables that
/// carry it, whether `codex login --with-api-key` stored it or a custom
/// provider names it.
#[test]
fn an_api_key_codex_launch_keeps_its_key_on_every_target() {
    let home = tempfile::tempdir().unwrap();
    let profile = codex_login_profile(home.path(), "apikey");
    for (target, launch) in launches_on_every_target(&profile) {
        assert_eq!(
            launch.environment["OPENAI_API_KEY"], "sk-svcacct-profile",
            "{target}"
        );
        assert_eq!(launch.environment["CODEX_API_KEY"], "sk-target", "{target}");
        assert!(launch.excluded_environment.is_empty(), "{target}");
    }

    let home = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    for (target, launch) in launches_on_every_target(&profile) {
        assert_eq!(
            launch.environment["ZAI_API_KEY"], "coding-plan-key",
            "{target}"
        );
        assert_eq!(launch.environment["CODEX_API_KEY"], "sk-target", "{target}");
        assert!(launch.excluded_environment.is_empty(), "{target}");
    }
}

#[test]
fn a_bedrock_codex_launch_passes_aws_settings_to_every_target() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "model = \"global.openai.gpt-6-luna\"\n\
         model_provider = \"amazon-bedrock-runtime\"\n\
         [model_providers.amazon-bedrock-runtime.aws]\n\
         region = \"us-east-1\"\n",
    )
    .unwrap();
    let aws_environment = BTreeMap::from([
        ("AWS_REGION".to_owned(), "us-east-1".to_owned()),
        ("AWS_DEFAULT_REGION".to_owned(), "us-east-1".to_owned()),
        ("AWS_PROFILE".to_owned(), "bedrock".to_owned()),
        (
            "AWS_CONFIG_FILE".to_owned(),
            "/home/hel/.aws/config".to_owned(),
        ),
        (
            "AWS_SHARED_CREDENTIALS_FILE".to_owned(),
            "/home/hel/.aws/credentials".to_owned(),
        ),
        ("AWS_EC2_METADATA_DISABLED".to_owned(), "false".to_owned()),
    ]);
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Codex,
        home: home.path().to_path_buf(),
        environment: aws_environment.clone().into(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    for (target, launch) in launches_on_every_target(&profile) {
        for (name, value) in &aws_environment {
            assert_eq!(
                launch.environment.get(name),
                Some(value),
                "{target}: {name}"
            );
        }
        for name in mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT {
            assert!(
                !launch.environment.contains_key(name),
                "{target}: inherited OpenAI key {name}"
            );
        }
        assert_eq!(
            launch.excluded_environment,
            mj_core::config::CODEX_CREDENTIAL_ENVIRONMENT.map(str::to_owned),
            "{target}"
        );
    }
}

/// #1160: a sub-agent child in its parent's container on a remote machine
/// runs from a staged home of its own, named after the child, and the
/// launch tells its worker which file there holds the login. That file is
/// where a credential sync push lands, so a login refreshed by `mj login`
/// reaches the child and not its parent's home.
#[test]
fn a_child_in_a_remote_container_takes_its_login_in_its_own_staged_home() {
    let home = tempfile::tempdir().unwrap();
    let profile = codex_login_profile(home.path(), "chatgpt");
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "1123456789abcdef0123456789abcdef";
    let parent_workspace = targets::new_container_workspace(parent_id).unwrap();
    let bundle = crate::controller::test_support::local_bundle(Path::new("/src/project"));
    let mut child = crate::controller::test_support::checkpoint_test_session(child_id);
    child.last_profile = "codex4".into();
    child.project_directory = None;
    child.container_workspace = Some(parent_workspace.clone());
    let template = mj_core::config::TargetTemplate::SshPodman {
        ssh: mj_core::config::SshConnection {
            host: "morannon".into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        },
        container: mj_core::config::ContainerTemplate {
            build_cache: None,
            image: "ghcr.io/brokkai/mjolnir/agent-dev:latest".to_owned(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
        },
    };

    let (launch, _, target_home) = worker_launch_config(
        &child,
        &profile,
        Some(&bundle),
        &targets::TargetLocator::SshPodman {
            borrowed_from: Some(parent_id.into()),
            ssh: SshTarget {
                destination: "morannon".into(),
                ssh_args: Vec::new(),
            },
            container_id: "c".repeat(64),
            workspace_storage: Default::default(),
        },
        LaunchWorkspace {
            session_id: parent_id,
            container: Some(&parent_workspace),
            parent_worktree: None,
        },
        &mj_core::state::TargetRuntimeSettings::from(&template),
    )
    .unwrap();

    assert_eq!(launch.harness_home, PathBuf::from(&target_home));
    assert_eq!(
        launch
            .environment
            .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
            .map(String::as_str),
        Some("/home/hel/.gitconfig")
    );
    assert_eq!(
        launch.harness_home,
        PathBuf::from(format!("/var/lib/hel/profiles/{child_id}")),
        "the child's staged home is its own, not its parent's"
    );
    assert_eq!(launch.environment["CODEX_HOME"], target_home);
    assert_eq!(launch.authentication_marker.as_deref(), Some("auth.json"));
    assert_eq!(
        launch.cwd,
        PathBuf::from(format!("/workspace/{parent_id}/project")),
        "the child works in its parent's checkout"
    );
}

#[test]
fn a_custom_provider_session_carries_its_key_and_runs_from_a_private_home() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let profile = zai_profile(home.path());
    let mut session = crate::controller::test_support::checkpoint_test_session("session-glm");
    session.harness_kind = HarnessKind::Codex;
    session.last_profile = "glm".into();
    session.target_template_id = "localhost".into();
    session.project_directory = Some(project.path().to_path_buf());
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/home/me/.local/share/hel/workers/session-glm".into(),
    });

    let (launch, _, target_home) = worker_launch_config(
        &session,
        &profile,
        None,
        &targets::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/workers/session-glm".into(),
        },
        LaunchWorkspace {
            session_id: &session.id,
            container: None,
            parent_worktree: None,
        },
        &mj_core::state::TargetRuntimeSettings::from(&mj_core::config::TargetTemplate::LocalBare),
    )
    .unwrap();

    assert_eq!(launch.environment["ZAI_API_KEY"], "coding-plan-key");
    assert_eq!(launch.environment["CODEX_HOME"], target_home);
    assert_eq!(
        target_home, "/home/me/.local/share/hel/workers/session-glm/profile",
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

fn staged_muse_settings(body: &str) -> (tempfile::TempDir, PathBuf) {
    let staged = tempfile::tempdir().unwrap();
    let path = staged.path().join("settings.json");
    std::fs::write(&path, body).unwrap();
    (staged, path)
}

// Hard-won: a24070f: Muse launches failed when staged settings retained the shipped :auto-review profile
#[test]
fn muse_staged_settings_replace_the_auto_review_profile_under_every_policy() {
    for (policy, profile) in [
        (ExecutionPolicy::Unconstrained, ":unrestricted"),
        (ExecutionPolicy::ConfiguredApprovals, ":ask-me"),
    ] {
        let (staged, path) = staged_muse_settings(
            r#"{
                "schema_version": 1,
                "provider": "anthropic",
                "model": "muse-1",
                "tui": {"theme": "dark"},
                "permissions": {"schema_version": 1, "default_profile": ":auto-review"}
            }"#,
        );

        apply_staged_execution_setting(HarnessKind::Muse, policy, staged.path()).unwrap();

        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["provider"], "anthropic");
        assert_eq!(document["model"], "muse-1");
        assert_eq!(document["tui"]["theme"], "dark");
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["permissions"]["schema_version"], 1);
        assert_eq!(
            document["permissions"]["default_profile"], profile,
            "{policy:?}"
        );
    }
}

/// A raw local target keeps configured approvals for Muse: muse-acp's
/// auto-review is its guardian, and Muse keeps its sandbox.
// Hard-won: 4a9dcb5: raw local Muse sessions used a permission profile Muse serve refused
#[test]
fn raw_local_muse_launches_with_guardian_approvals() {
    let project = tempfile::tempdir().unwrap();
    let mut session = crate::controller::test_support::checkpoint_test_session("session-muse");
    session.harness_kind = HarnessKind::Muse;
    session.last_profile = "muse".into();
    session.target_template_id = "localhost".into();
    session.project_directory = Some(project.path().to_path_buf());
    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: "/home/me/.local/share/hel/workers/session-muse".into(),
    });
    let profile = mj_core::config::HarnessProfile {
        enabled: true,
        kind: HarnessKind::Muse,
        home: PathBuf::from("/profiles/muse"),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };

    let (launch, _, _) = worker_launch_config(
        &session,
        &profile,
        None,
        &targets::TargetLocator::LocalBare {
            worker_root: "/home/me/.local/share/hel/workers/session-muse".into(),
        },
        LaunchWorkspace {
            session_id: &session.id,
            container: None,
            parent_worktree: None,
        },
        &mj_core::state::TargetRuntimeSettings::from(&mj_core::config::TargetTemplate::LocalBare),
    )
    .unwrap();

    assert_eq!(
        launch.execution_policy,
        ExecutionPolicy::ConfiguredApprovals
    );
    assert_eq!(launch.environment["MUSE_APPROVAL_MODE"], "promptUnmatched");
    assert!(!launch.environment.contains_key("MUSE_SERVE_ARGS"));
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
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
fn staged_kimi_profile_binds_history_tools_to_the_target_runtime() {
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    };
    stage_profile(&profile, staged.path()).unwrap();
    let memory = ProjectMemoryLaunchConfig {
        history_socket: Some("/var/lib/hel/workers/session/control.sock".into()),
        project_key: "project".into(),
        root: "/var/lib/hel/profiles/session/projects/project/memory".into(),
        baseline_root: PathBuf::new(),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
    };

    configure_kimi_history_mcp(
        staged.path(),
        "/var/lib/hel/workers/session",
        memory.history_socket.as_deref(),
    )
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
                "--history-socket",
                "/var/lib/hel/workers/session/control.sock"
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
fn staged_kimi_history_mcp_resolves_ssh_paths_from_target_home() {
    let staged = tempfile::tempdir().unwrap();
    let memory = ProjectMemoryLaunchConfig {
        history_socket: Some(".local/share/hel/workers/session/control.sock".into()),
        project_key: "project".into(),
        root: ".local/share/hel/profiles/session/projects/project/memory".into(),
        baseline_root: PathBuf::new(),
        repository_roots: BTreeMap::new(),
        mcp_delivery: ProjectMemoryMcpDelivery::HarnessProfile,
    };

    configure_kimi_history_mcp(
        staged.path(),
        ".local/share/hel/workers/session",
        memory.history_socket.as_deref(),
    )
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
            "exec \"$HOME/$1\" worker memory-mcp --history-socket \"$HOME/$2\"",
            "mj-memory",
            ".local/share/hel/workers/session/hel",
            ".local/share/hel/workers/session/control.sock"
        ])
    );
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
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
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
        subagents: mj_core::subagent::SubagentPolicy::Native,
        handback_tool: false,
        agent_mailboxes_enabled: true,
        initial_model: None,
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
        environment: Default::default(),
        excluded_environment: Vec::new(),
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

#[cfg(target_os = "linux")]
#[test]
fn legacy_worker_upgrade_relinks_cache_configuration_without_native_mbx() {
    struct CacheLinkExecutor {
        commands: RefCell<Vec<CommandSpec>>,
        worker_bin: PathBuf,
        configuration: PathBuf,
        home: PathBuf,
    }

    impl CommandExecutor for CacheLinkExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let mut args = vec![
                "-c".into(),
                command.args[4].clone(),
                "sh".into(),
                self.worker_bin.to_string_lossy().into_owned(),
                self.configuration.to_string_lossy().into_owned(),
            ];
            args.extend(command.args[8..].iter().cloned());
            let mut local = CommandSpec::new("/bin/sh", args);
            local
                .env
                .insert("HOME".into(), self.home.to_string_lossy().into_owned());
            targets::CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(5))
                .execute(&local)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let shared_cache = directory.path().join("shared-cache");
    let configuration = shared_cache.join(".mjolnir/config/mbx");
    std::fs::create_dir_all(&configuration).unwrap();
    std::fs::write(
        configuration.join("config.toml"),
        "[gc]\nmax_total_size = '50GB'\n",
    )
    .unwrap();
    let worker_bin = directory.path().join("worker/bin");
    std::fs::create_dir_all(&worker_bin).unwrap();
    let legacy_binary = worker_bin.join("mbx");
    std::fs::write(&legacy_binary, b"legacy copied mbx executable").unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let controller = crate::controller::Controller {
        config: mj_core::config::Config::default(),
        state: mj_core::state::State::default(),
    };
    let mut session = crate::controller::test_support::checkpoint_test_session("legacy-cache");
    session.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "local".into(),
        directory: shared_cache,
        max_size: None,
        target_root: None,
    });
    let backend = targets::TargetLocator::LocalPodman {
        container_id: targets::resource_name(&session.id).unwrap(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    };
    let launch = WorkerLaunchConfig {
        goal_resume_request: None,
        run_mode: Default::default(),
        session_id: session.id.clone(),
        subagents: mj_core::subagent::SubagentPolicy::Native,
        handback_tool: false,
        agent_mailboxes_enabled: true,
        initial_model: None,
        review_capture: false,
        target_environment: Default::default(),
        seed_image_environment: true,
        harness: HarnessKind::Codex,
        harness_home: directory.path().join("profile"),
        authentication_marker: None,
        bridge_command: "codex".into(),
        bridge_args: Vec::new(),
        harness_runtime: HarnessRuntimePolicy::Ambient,
        environment: Default::default(),
        excluded_environment: Vec::new(),
        cwd: directory.path().to_path_buf(),
        additional_directories: Vec::new(),
        native_session_id: None,
        project_memory: None,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
    };
    let executor = CacheLinkExecutor {
        commands: RefCell::new(Vec::new()),
        worker_bin,
        configuration,
        home: home.clone(),
    };

    // This is the cache-configuration preflight called by
    // upgrade_session_worker before it prepares or swaps the worker.
    controller
        .prepare_build_cache_links(&session, &backend, &launch, &executor)
        .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(
        commands.len(),
        1,
        "legacy preparation must not probe the host"
    );
    assert_eq!(
        commands[0].purpose,
        "inspect legacy mbx and relink its shared configuration"
    );
    assert_eq!(commands[0].program, "podman");
    assert!(commands[0].args.iter().any(|argument| {
        argument.ends_with("/bin") && argument.starts_with("/var/lib/hel/workers/")
    }));
    assert_eq!(
        std::fs::read(&legacy_binary).unwrap(),
        b"legacy copied mbx executable"
    );
    assert!(home.join(".config/mbx/config.toml").is_symlink());
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
        subagents: mj_core::subagent::SubagentPolicy::Native,
        handback_tool: false,
        agent_mailboxes_enabled: true,
        initial_model: None,
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
        excluded_environment: Vec::new(),
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

// Hard-won: 5461a2c: recovery kept relaunching an incompatible remote worker that could not start

#[test]
fn a_remote_worker_with_a_mismatched_binary_is_replaced_before_restart() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, stamped_worker(b"fresh musl worker")).unwrap();
    let executor = DigestExecutor {
        installed_line: format!("{}  /root/hel\n", "0".repeat(64)),
        commands: RefCell::new(Vec::new()),
    };
    let replaced = replace_target_worker_binary_if_stale(
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
fn a_remote_recovery_plan_defers_binary_refresh_to_the_recovery_task() {
    let locator = ssh_bare_locator("session-remote");
    let refresh = worker_binary_refresh_plan(&locator, "session-remote")
        .unwrap()
        .expect("a remote target now gets a binary refresh");
    match refresh {
        WorkerBinaryRefresh::Deferred(remote) => {
            assert_eq!(remote.session_id, "session-remote");
            assert_eq!(remote.locator, locator);
        }
        WorkerBinaryRefresh::Prepared(_) => {
            panic!("a remote target must defer, not prepare, its binary refresh")
        }
    }
}

#[cfg(unix)]
#[test]
fn recovery_preserves_launch_config_until_a_matching_worker_source_is_available() {
    const CHILD: &str = "MJ_RECOVERY_BINARY_PAIR_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "recovery_preserves_launch_config_until_a_matching_worker_source_is_available",
        ))
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .env("MJ_INSTANCE", "issue-1138-recovery")
        .env("MJ_WORKER_BINARY", directory.path().join("not-built-yet"))
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let root = directory.path().join(session_id);
    std::fs::create_dir(&root).unwrap();
    let binary = root.join("hel");
    let launch = root.join("launch.json");
    let restarted = root.join("restarted");
    std::fs::write(&binary, b"old worker").unwrap();
    std::fs::write(&launch, b"old launch schema").unwrap();
    let next_launch = directory.path().join("next-launch.json");
    std::fs::write(&next_launch, b"new launch schema").unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: root.to_string_lossy().into_owned(),
    };
    let plan = WorkerRecoveryPlan {
        source_target: mj_core::state::TargetLocator::LocalBare { worker_root: root },
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["dead\n"]),
        binary_refresh: worker_binary_refresh_plan(&locator, session_id).unwrap(),
        launch_refresh: Some(WorkerLaunchRefreshPlan {
            expected_sha256: lower_hex(Sha256::digest(b"new launch schema")),
            installed_digest: installed_file_digest_command(
                &locator,
                &launch.to_string_lossy(),
                "identify test launch config",
            ),
            replace: CommandPlan {
                description: "install new launch schema".into(),
                commands: vec![CommandSpec::new(
                    "cp",
                    [
                        next_launch.to_string_lossy().into_owned(),
                        launch.to_string_lossy().into_owned(),
                    ],
                )],
            },
        }),
        restart: CommandPlan {
            description: "restart test worker".into(),
            commands: vec![CommandSpec::new(
                "touch",
                [restarted.to_string_lossy().into_owned()],
            )],
        },
    };
    let mut record = crate::controller::test_support::checkpoint_test_session(session_id);
    record.target = Some(plan.source_target.clone());
    crate::database::save_session(&record).unwrap();
    let error = crate::session_manager::recover_worker_controlled(
        plan.clone(),
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("MJ_WORKER_BINARY is not a file"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&binary).unwrap(), b"old worker");
    assert_eq!(std::fs::read(&launch).unwrap(), b"old launch schema");
    assert!(!restarted.exists());

    std::fs::write(
        std::env::var_os("MJ_WORKER_BINARY").unwrap(),
        b"legacy worker",
    )
    .unwrap();
    let error = crate::session_manager::recover_worker_controlled(
        plan.clone(),
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing worker build stamp"));
    assert_eq!(std::fs::read(&binary).unwrap(), b"old worker");
    assert_eq!(std::fs::read(&launch).unwrap(), b"old launch schema");
    assert!(!restarted.exists());

    // The same recovery plan retries after the matching worker is installed;
    // no controller restart or replanning is needed.
    std::fs::write(
        std::env::var_os("MJ_WORKER_BINARY").unwrap(),
        stamped_worker(b"new worker"),
    )
    .unwrap();
    crate::session_manager::recover_worker_controlled(
        plan,
        false,
        Some(session_id),
        &ProcessExecutor,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(binary).unwrap(),
        stamped_worker(b"new worker")
    );
    assert_eq!(std::fs::read(launch).unwrap(), b"new launch schema");
    assert!(restarted.exists());
}

/// A daemon pins its worker sources once, at startup. A pin that no longer
/// names a file must send the lookup back to resolution rather than failing
/// every session until someone restarts the daemon (#1068).
// Hard-won: 924190f: a cache reaper removed a pinned worker source and sessions failed until restart
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

/// A fixture harness home shaped like a real one: the login and settings a
/// session needs, next to native history, logs, caches and the skills the
/// harness maintains itself. Each entry is `(home-relative path, whether a
/// staged home gets it)`.
fn fixture_home_entries(kind: HarnessKind) -> &'static [(&'static str, bool)] {
    match kind {
        HarnessKind::Codex => &[
            ("auth.json", true),
            ("config.toml", true),
            ("AGENTS.md", true),
            ("skills/review/SKILL.md", true),
            (
                "sessions/2026/09/25/rollout-2026-09-25T09-00-00-native.jsonl",
                false,
            ),
            ("history.jsonl", false),
            ("session_index.jsonl", false),
            ("state_5.sqlite", false),
            ("logs_2.sqlite", false),
            ("thread_history_1.sqlite", false),
            ("models_cache.json", false),
            ("shell_snapshots/snapshot.sh", false),
            (
                "projects/hel-0123456789abcdef-0123456789abcdef0123456789abcdef/memory/MEMORY.md",
                false,
            ),
            ("skills/.system/imagegen/SKILL.md", false),
        ],
        HarnessKind::Claude => &[
            (".credentials.json", true),
            (".claude.json", true),
            ("settings.json", true),
            ("CLAUDE.md", true),
            ("skills/review/SKILL.md", true),
            ("projects/-home-me-app/native.jsonl", false),
            ("history.jsonl", false),
            ("todos/native.json", false),
            ("shell-snapshots/snapshot.sh", false),
            ("statsig/cache", false),
            ("skills/synced/account/SKILL.md", false),
        ],
        HarnessKind::Kimi => &[
            ("credentials/kimi-code.json", true),
            ("config.toml", true),
            ("device_id", true),
            ("skills/review/SKILL.md", true),
            ("sessions/native/context.jsonl", false),
            ("session_index.jsonl", false),
            ("user-history/history.jsonl", false),
            ("logs/kimi.log", false),
            ("workspaces.json", false),
            ("telemetry/events.json", false),
        ],
        HarnessKind::Grok => &[
            ("auth.json", true),
            ("config.toml", true),
            ("agent_id", true),
            ("skills/review/SKILL.md", true),
            ("sessions/native/session_search.sqlite", false),
            ("active_sessions.json", false),
            ("logs/grok.log", false),
            ("models_cache.json", false),
            ("memory-v2/store.json", false),
        ],
        HarnessKind::Muse => &[
            ("auth.json", true),
            ("settings.json", true),
            ("trust.json", true),
            ("skills/review/SKILL.md", true),
            ("cache/models.json", false),
            ("logs/muse.log", false),
        ],
        HarnessKind::OpenCode => &[
            ("opencode.json", true),
            ("AGENTS.md", true),
            (".data/opencode/auth.json", true),
            ("skills/review/SKILL.md", true),
            (".data/opencode/opencode.db", false),
            (".data/opencode/opencode.db-wal", false),
            (".data/opencode/log/opencode.log", false),
            (".data/opencode/repos/native", false),
        ],
    }
}

/// Every regular file under `root`, as `/`-separated relative paths.
fn files_under(root: &Path) -> std::collections::BTreeSet<String> {
    let mut files = std::collections::BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).unwrap();
                files.insert(
                    relative
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
    files
}

/// A staged home is what a session runs from on every target, this machine
/// included. It gets exactly the login and settings the session needs, so the
/// harness is signed in, and none of the profile home's native history, logs
/// or caches.
#[test]
fn a_staged_home_gets_the_login_and_settings_and_no_native_history() {
    for kind in HarnessKind::ALL {
        let home = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        for (path, _) in fixture_home_entries(kind) {
            let path = home.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, br#"{"fixture":true}"#).unwrap();
        }
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: home.path().to_path_buf(),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };

        stage_profile(&profile, staged.path()).unwrap();

        let expected = fixture_home_entries(kind)
            .iter()
            .filter(|(_, staged)| *staged)
            .map(|(path, _)| (*path).to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(files_under(staged.path()), expected, "{kind:?}");
        assert!(
            mj_core::config::harness_authentication_marker(kind, staged.path()).is_file(),
            "{kind:?} is signed in from its staged home"
        );
    }
}

/// A login as each harness writes it. A higher `generation` is a fresher copy
/// of the same grant, which is what the credential sync orders copies by.
fn login_bytes(kind: HarnessKind, generation: i64) -> Vec<u8> {
    let login = match kind {
        HarnessKind::Codex => serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": format!("access-{generation}"),
                "refresh_token": format!("refresh-{generation}"),
            },
            "last_refresh": format!("2026-09-{:02}T09:00:00Z", 10 + generation),
        }),
        HarnessKind::Claude => serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("access-{generation}"),
                "refreshToken": format!("refresh-{generation}"),
                "expiresAt": 1_790_000_000_000_i64 + generation * 1000,
            }
        }),
        HarnessKind::Kimi => serde_json::json!({
            "access_token": format!("access-{generation}"),
            "refresh_token": format!("refresh-{generation}"),
            "expires_at": 1_790_000_000_i64 + generation,
        }),
        HarnessKind::Grok => serde_json::json!({
            "https://auth.x.ai::1": {
                "key": format!("access-{generation}"),
                "refresh_token": format!("refresh-{generation}"),
                "expires_at": format!("2026-10-{:02}T09:00:00Z", 10 + generation),
            }
        }),
        HarnessKind::Muse => serde_json::json!({ "token": format!("token-{generation}") }),
        // OpenCode stores one grant per provider under the provider id, with
        // the OAuth expiry in epoch milliseconds.
        HarnessKind::OpenCode => serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": format!("refresh-{generation}"),
                "access": format!("access-{generation}"),
                "expires": 1_790_000_000_000_i64 + generation * 1000,
            }
        }),
    };
    serde_json::to_vec(&login).unwrap()
}

/// The credential sync pushes a rotated login into a local session's staged
/// home as it does into a container session's. The launch configuration tells
/// the worker which file of its staged home holds the login. The sync compares
/// that file with the profile's, finds the profile's fresher, and the worker's
/// install writes it into the file the harness reads.
// Hard-won: 7b4cb35: Kimi login sync wrote credentials at home root instead of credentials/kimi-code.json
#[test]
fn a_rotated_login_reaches_the_staged_home_of_a_session_on_this_machine() {
    use mj_core::config::harness_authentication_marker;
    use mj_core::credentials::{
        SyncAction, read_credential_file, reconcile, write_credential_file,
    };

    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (index, kind) in HarnessKind::ALL.into_iter().enumerate() {
        let session_id = format!("{:032x}", index + 1);
        let home = directory.path().join(format!("{}-home", kind.id()));
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: home.clone(),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };
        let canonical = profile.authentication_marker();
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        std::fs::write(&canonical, login_bytes(kind, 1)).unwrap();
        let worker_root = directory.path().join("workers").join(&session_id);
        let locator = targets::TargetLocator::LocalBare {
            worker_root: worker_root.to_string_lossy().into_owned(),
        };
        let mut session = crate::controller::test_support::checkpoint_test_session(&session_id);
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.clone());
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });

        let (launch, _, target_home) = worker_launch_config(
            &session,
            &profile,
            None,
            &locator,
            LaunchWorkspace {
                session_id: &session_id,
                container: None,
                parent_worktree: None,
            },
            &mj_core::state::TargetRuntimeSettings::from(
                &mj_core::config::TargetTemplate::LocalBare,
            ),
        )
        .unwrap();

        // The session runs from its own staged home, never the profile home,
        // and its harness is pointed there on every operating system.
        assert_eq!(launch.harness_home, PathBuf::from(&target_home), "{kind:?}");
        assert_ne!(launch.harness_home, home, "{kind:?}");
        assert_eq!(
            kind.home_from_environment(&launch.environment[kind.home_env()]),
            launch.harness_home,
            "{kind:?}"
        );
        // Where the worker's credential endpoint reads and installs, which
        // must be the file the harness reads its login from.
        let endpoint = launch
            .harness_home
            .join(launch.authentication_marker.as_deref().unwrap());
        assert_eq!(
            endpoint,
            harness_authentication_marker(kind, &launch.harness_home),
            "{kind:?}"
        );
        // Muse's staged root lies under the data directory; the rest of the
        // exchange is the same for it, except that Muse stores no refresh
        // time, so the sync never orders two different Muse copies.
        if kind == HarnessKind::Muse {
            continue;
        }
        stage_profile(&profile, &launch.harness_home).unwrap();

        // The profile's login rotates while the session runs.
        std::fs::write(&canonical, login_bytes(kind, 2)).unwrap();
        let (profile_copy, profile_bytes) = read_credential_file(kind, &canonical).unwrap();
        let (session_copy, _) = read_credential_file(kind, &endpoint).unwrap();
        assert_eq!(
            reconcile(&profile_copy, &session_copy),
            SyncAction::Push,
            "{kind:?}"
        );
        write_credential_file(kind, &endpoint, &profile_bytes).unwrap();

        assert_eq!(
            std::fs::read(harness_authentication_marker(kind, &launch.harness_home)).unwrap(),
            login_bytes(kind, 2),
            "{kind:?}"
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), login_bytes(kind, 2));
    }
}

/// A local session's project-memory replica lands in its staged home and goes
/// with the session. Closing the session removes the staged home with the
/// replica inside, both for a harness staged under the worker root and for
/// Muse, whose root lies under the data directory. The profile home it was
/// staged from keeps its login and gains no `projects/` directory.
#[cfg(unix)]
#[test]
fn closing_a_local_session_removes_its_staged_home_and_memory_replica() {
    use crate::controller::test_support::{IsolatedTest, test_name};

    const CHILD: &str = "MJ_CLOSE_REMOVES_REPLICA_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(
            module_path!(),
            "closing_a_local_session_removes_its_staged_home_and_memory_replica",
        ))
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (index, kind) in [HarnessKind::Codex, HarnessKind::Kimi, HarnessKind::Muse]
        .into_iter()
        .enumerate()
    {
        let session_id = format!("{:032x}", index + 1);
        let home = directory.path().join(format!("{}-home", kind.id()));
        let profile = mj_core::config::HarnessProfile {
            enabled: true,
            kind,
            home: home.clone(),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };
        let marker = profile.authentication_marker();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, login_bytes(kind, 1)).unwrap();
        let worker_root = directory.path().join("workers").join(&session_id);
        let locator = targets::TargetLocator::LocalBare {
            worker_root: worker_root.to_string_lossy().into_owned(),
        };
        let mut session = crate::controller::test_support::checkpoint_test_session(&session_id);
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        session.target_template_id = "localhost".into();
        session.project_directory = Some(project.clone());
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });
        let (_, memory, target_home) = worker_launch_config(
            &session,
            &profile,
            None,
            &locator,
            LaunchWorkspace {
                session_id: &session_id,
                container: None,
                parent_worktree: None,
            },
            &mj_core::state::TargetRuntimeSettings::from(
                &mj_core::config::TargetTemplate::LocalBare,
            ),
        )
        .unwrap();
        let target_home = PathBuf::from(target_home);
        assert!(memory.root.starts_with(&target_home), "{kind:?}");

        // Stage and install as `prepare_worker_files` does on this machine.
        let canonical = canonical_memory_root(&memory.project_key);
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("MEMORY.md"), "- a remembered fact\n").unwrap();
        let stage = tempfile::tempdir().unwrap();
        stage_profile(&profile, stage.path()).unwrap();
        stage_memory_replica(&memory, &target_home, stage.path()).unwrap();
        std::fs::create_dir_all(&worker_root).unwrap();
        for entry in std::fs::read_dir(stage.path()).unwrap() {
            let entry = entry.unwrap();
            copy_profile_entry(&entry.path(), &target_home.join(entry.file_name())).unwrap();
        }
        assert!(memory.root.join("MEMORY.md").is_file(), "{kind:?}");

        targets::close_plan(&locator, &session_id)
            .unwrap()
            .execute(&targets::ProcessExecutor)
            .unwrap();

        assert!(!memory.root.exists(), "{kind:?}: the replica goes");
        assert!(!target_home.exists(), "{kind:?}: the staged home goes");
        assert!(!worker_root.exists(), "{kind:?}");
        assert!(marker.is_file(), "{kind:?}: the profile keeps its login");
        assert!(!home.join("projects").exists(), "{kind:?}");
        assert!(
            canonical.join("MEMORY.md").is_file(),
            "the canonical project memory outlives the session"
        );
    }
}

/// Launch finding R11-1: a Claude child on a model without Auto mode ran in
/// Accept edits, and Claude asked a person before it would run the child's own
/// `handback`, so the child could not report without one. The staged settings
/// allow every tool the role's `mj-agents` server lists, whatever the mode, and
/// keep the person's own settings and rules.
// Hard-won: 91244ea: a Claude child asked the person for permission before its own handback
#[test]
fn the_staged_claude_profile_allows_its_own_sub_agent_tools() {
    use mj_core::subagent::SubagentMcpRole;

    let allowed = |stage: &Path| -> (serde_json::Value, Vec<String>) {
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(stage.join("settings.json")).unwrap()).unwrap();
        let allow = settings["permissions"]["allow"]
            .as_array()
            .expect("the staged settings have an allow list")
            .iter()
            .map(|rule| rule.as_str().unwrap().to_owned())
            .collect();
        (settings, allow)
    };

    // A child's only tool is handback.
    let stage = tempfile::tempdir().unwrap();
    std::fs::write(
        stage.path().join("settings.json"),
        r#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"],"deny":["WebFetch"]}}"#,
    )
    .unwrap();
    configure_claude_subagent_mcp(stage.path(), "/worker", SubagentMcpRole::Child, true).unwrap();
    let (settings, allow) = allowed(stage.path());
    assert_eq!(allow, ["Bash(ls:*)", "mcp__mj-agents__handback"]);
    assert_eq!(
        settings["permissions"]["deny"],
        serde_json::json!(["WebFetch"])
    );
    assert_eq!(settings["model"], "opus");

    // A profile with no settings file gets one.
    let stage = tempfile::tempdir().unwrap();
    configure_claude_subagent_mcp(stage.path(), "/worker", SubagentMcpRole::Child, true).unwrap();
    assert_eq!(allowed(stage.path()).1, ["mcp__mj-agents__handback"]);

    // A parent delegates without asking; a rule the person already has is
    // kept once, in its place.
    let stage = tempfile::tempdir().unwrap();
    std::fs::write(
        stage.path().join("settings.json"),
        r#"{"permissions":{"allow":["mcp__mj-agents__wait"]}}"#,
    )
    .unwrap();
    configure_claude_subagent_mcp(stage.path(), "/worker", SubagentMcpRole::Parent, true).unwrap();
    let (_, allow) = allowed(stage.path());
    assert_eq!(allow[0], "mcp__mj-agents__wait");
    assert_eq!(
        allow
            .iter()
            .filter(|rule| *rule == "mcp__mj-agents__wait")
            .count(),
        1,
        "{allow:?}"
    );
    for tool in [
        "list_profiles",
        "spawn",
        "list_agents",
        "send_message",
        "wait",
        "close",
    ] {
        assert!(
            allow.contains(&format!("mcp__mj-agents__{tool}")),
            "{tool}: {allow:?}"
        );
    }
    assert!(
        !allow.iter().any(|rule| rule.ends_with("__handback")),
        "a parent has no handback: {allow:?}"
    );
}

#[test]
fn staged_claude_mailbox_hook_merges_and_restages_without_duplicates() {
    let stage = tempfile::tempdir().unwrap();
    let path = stage.path().join("settings.json");
    let user_hook = serde_json::json!({
        "hooks": [{"type": "command", "command": "user-hook"}]
    });
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "model": "opus",
            "permissions": {"deny": ["WebFetch"]},
            "hooks": {
                "PostToolBatch": [user_hook.clone()],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "before-tool"}]}]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let remote_root = ".local/share/hel/workers/session";
    configure_claude_mailbox_hook(stage.path(), remote_root, true).unwrap();
    let first = std::fs::read(&path).unwrap();
    configure_claude_mailbox_hook(stage.path(), remote_root, true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), first);

    let settings: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(settings["model"], "opus");
    assert_eq!(
        settings["permissions"]["deny"],
        serde_json::json!(["WebFetch"])
    );
    let groups = settings["hooks"]["PostToolBatch"].as_array().unwrap();
    assert_eq!(groups[0], user_hook);
    let mailbox_hooks = groups
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.starts_with("MJOLNIR_MAILBOX_HOOK=1 "))
        })
        .collect::<Vec<_>>();
    assert_eq!(mailbox_hooks.len(), 1);
    assert_eq!(
        mailbox_hooks[0]["command"],
        format!(
            "MJOLNIR_MAILBOX_HOOK=1 {} worker mailbox-hook --socket {} --event PostToolBatch",
            mj_core::targets::posix_quote(&format!("{remote_root}/hel")),
            mj_core::targets::posix_quote(&format!("{remote_root}/control.sock"))
        )
    );
    assert_eq!(
        mailbox_hooks[0]["timeout"],
        mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS
    );

    configure_claude_mailbox_hook(stage.path(), "/worker path/session", true).unwrap();
    let restaged: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let updated_commands = restaged["hooks"]["PostToolBatch"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| command.starts_with("MJOLNIR_MAILBOX_HOOK=1 "))
        .collect::<Vec<_>>();
    assert_eq!(updated_commands.len(), 1);
    assert!(updated_commands[0].contains("'/worker path/session/control.sock'"));
}

#[test]
fn staged_claude_mailbox_hook_is_absent_when_mailboxes_are_disabled() {
    let stage = tempfile::tempdir().unwrap();
    let path = stage.path().join("settings.json");
    let original =
        br#"{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"user-hook"}]}]}}"#;
    std::fs::write(&path, original).unwrap();

    configure_claude_mailbox_hook(stage.path(), "/worker/session", false).unwrap();

    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[cfg(unix)]
mod container_runtime {
    use super::*;
    use mj_core::harness_runtime::{CODEX_ACP_PACKAGE, npm_bridge};
    use mj_worker::worker_runtime::{
        AcpSupervisorSpec, PreparedHarnessLaunch, prepare_harness_launch,
    };
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn executable(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn package(root: &Path, name: &str, version: &str) -> PathBuf {
        let package = root.join("node_modules").join(name);
        let command = package.join("bin/cli");
        executable(
            &command,
            &format!("#!/bin/sh\nprintf '%s\\n' '{name} {version}'\n"),
        );
        std::fs::write(
            package.join("package.json"),
            serde_json::json!({"name": name, "version": version}).to_string(),
        )
        .unwrap();
        command
    }

    fn installed_bridge(root: &Path, harness: HarnessKind) -> PathBuf {
        let bridge = npm_bridge(harness).unwrap();
        let command = package(root, bridge.package, bridge.version);
        if harness == HarnessKind::Codex {
            let provider = package(
                root,
                "@openai/codex",
                mj_core::harness_runtime::CODEX_CLI_VERSION,
            );
            std::fs::rename(&provider, provider.with_file_name("codex.js")).unwrap();
        } else {
            package(root, "@anthropic-ai/claude-agent-sdk", "test-sdk");
        }
        let bin = root.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        symlink(
            format!("../{}/bin/cli", bridge.package),
            bin.join(bridge.command),
        )
        .unwrap();
        command
    }

    fn container_launch(root: &Path, harness: HarnessKind) -> WorkerLaunchConfig {
        let profile_home = root.join("profile");
        std::fs::create_dir_all(&profile_home).unwrap();
        let mut profile = codex_login_profile(&profile_home, "chatgpt");
        profile.kind = harness;
        let mut launch = launches_on_every_target(&profile)
            .into_iter()
            .find(|(target, _)| *target == "container")
            .unwrap()
            .1;
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Ambient);
        launch.cwd = root.to_path_buf();
        launch.environment.insert(
            "PATH".into(),
            root.join("node_modules/.bin").display().to_string(),
        );
        launch.environment.insert(
            "XDG_CACHE_HOME".into(),
            root.join("cache").display().to_string(),
        );
        launch.environment.insert(
            "CODEX_PATH".into(),
            root.join("node_modules/@openai/codex/bin/codex.js")
                .display()
                .to_string(),
        );
        launch
    }

    async fn prepare(launch: &WorkerLaunchConfig) -> PreparedHarnessLaunch {
        prepare_harness_launch(
            launch.harness,
            launch.harness_runtime,
            launch.execution_policy,
            AcpSupervisorSpec::from(launch),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn container_runtime_freezes_relative_path_and_provider_selection() {
        let temp = tempfile::tempdir().unwrap();
        let bridge = installed_bridge(temp.path(), HarnessKind::Codex);
        let mut launch = container_launch(temp.path(), HarnessKind::Codex);
        launch
            .environment
            .insert("PATH".into(), "node_modules/.bin".into());
        launch.environment.insert(
            "CODEX_PATH".into(),
            "node_modules/@openai/codex/bin/codex.js".into(),
        );
        let prepared = prepare(&launch).await;
        assert_eq!(prepared.spec.command, bridge.canonicalize().unwrap());
        assert!(Path::new(&prepared.spec.environment["CODEX_PATH"]).is_absolute());
        let link = temp.path().join("node_modules/.bin/codex-acp");
        std::fs::remove_file(&link).unwrap();
        symlink("/bin/false", &link).unwrap();
        // Execute the prepared spec: changing PATH's link cannot redirect it.
        let mut command = tokio::process::Command::new(&prepared.spec.command);
        command
            .args(&prepared.spec.args)
            .env_clear()
            .envs(&prepared.spec.environment)
            .current_dir(&prepared.spec.cwd);
        let output =
            mj_core::subprocess::run_bounded(&mut command, 1024, std::time::Duration::from_secs(5))
                .await
                .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("{CODEX_ACP_PACKAGE} {CODEX_ACP_VERSION}")
        );
    }

    #[test]
    fn container_runtime_prepares_before_startup_and_preserves_the_live_worker_on_upgrade_failure()
    {
        use targets::TargetLocator;
        struct PreparationExecutor {
            commands: RefCell<Vec<CommandSpec>>,
            fail_prepare: bool,
        }
        impl CommandExecutor for PreparationExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                self.commands.borrow_mut().push(command.clone());
                if self.fail_prepare
                    && command.purpose == "prepare exact container harness before worker upgrade"
                {
                    bail!("test preparation connection failed");
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let session = mj_core::state::new_session_id().unwrap();
        let container_id = targets::resource_name(&session).unwrap();
        let ssh = SshTarget {
            destination: "user@host.test".into(),
            ssh_args: Vec::new(),
        };
        let locators = [
            TargetLocator::LocalDocker {
                container_id: container_id.clone(),
                borrowed_from: None,
            },
            TargetLocator::LocalPodman {
                container_id: container_id.clone(),
                workspace_storage: Default::default(),
                borrowed_from: None,
            },
            TargetLocator::AppleContainer {
                container_id: container_id.clone(),
                borrowed_from: None,
            },
            TargetLocator::SshDocker {
                container_id: container_id.clone(),
                ssh: ssh.clone(),
                borrowed_from: None,
            },
            TargetLocator::SshPodman {
                container_id,
                ssh,
                workspace_storage: Default::default(),
                borrowed_from: None,
            },
        ];
        let launch = container_launch(temp.path(), HarnessKind::Codex);
        let worker = temp.path().join("worker");
        std::fs::write(&worker, stamped_worker(b"new worker")).unwrap();
        for locator in locators {
            let root = targets::worker_root(&locator, &session).unwrap();
            let executor = PreparationExecutor {
                commands: RefCell::new(Vec::new()),
                fail_prepare: false,
            };
            prepare_installed_managed_harness(&executor, &locator, &root, &launch).unwrap();
            let commands = executor.commands.borrow();
            assert_eq!(commands.len(), 1);
            assert!(commands[0].args.join(" ").contains("prepare-harness"));
            drop(commands);
            for fail_prepare in [false, true] {
                let executor = PreparationExecutor {
                    commands: RefCell::new(Vec::new()),
                    fail_prepare,
                };
                let result = prepare_managed_harness_for_upgrade(
                    &executor, &locator, &session, &worker, &launch,
                );
                assert_eq!(result.is_err(), fail_prepare);
                let commands = executor.commands.borrow();
                let prepare = commands
                    .iter()
                    .find(|command| {
                        command.purpose == "prepare exact container harness before worker upgrade"
                    })
                    .unwrap();
                assert!(prepare.args.join(" ").contains("harness-prepare-"));
                let stage = commands
                    .iter()
                    .find(|command| {
                        command.purpose == "stage container harness launch configuration"
                    })
                    .unwrap();
                assert!(stage.args.join(" ").contains("harness-prepare-"));
                for command in commands.iter() {
                    let rendered = command.args.join(" ");
                    assert!(!rendered.contains(&format!("'{root}/hel'")));
                    assert!(!rendered.contains(&format!("'{root}/launch.json'")));
                    assert!(!command.args.contains(&format!("{root}/hel")));
                    assert!(!command.args.contains(&format!("{root}/launch.json")));
                    assert!(!rendered.contains("control.sock"));
                    assert!(!rendered.contains("kill"));
                }
                assert_eq!(
                    commands
                        .iter()
                        .any(|command| command.purpose
                            == "remove container harness preparation staging"),
                    !fail_prepare
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths() {
    let directory = tempfile::tempdir().unwrap();
    // macOS ships shasum but not GNU sha256sum. Exercise the Linux command
    // shape there through the native digest tool as well.
    if cfg!(target_os = "macos") {
        mj_core::test_hooks::install_fake_command(
            directory.path(),
            "sha256sum",
            "#!/bin/sh\nexec /usr/bin/shasum -a 256 \"$@\"\n",
        );
    }
    let file = directory.path().join("worker's bytes");
    std::fs::write(&file, vec![0x5a; 128 * 1024]).unwrap();
    let expected = mj_core::worker_launch::worker_executable_digest(&file).unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: directory.path().to_string_lossy().into_owned(),
    };
    for os in ["Linux", "Darwin"] {
        mj_core::test_hooks::install_fake_command(
            directory.path(),
            "uname",
            &format!("#!/bin/sh\necho {os}\n"),
        );
        let mut command =
            installed_file_digest_command(&locator, &file.to_string_lossy(), "digest test");
        command.env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", directory.path().display()),
        );
        let output = ProcessExecutor.execute(&command).unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .split_whitespace()
                .next(),
            Some(expected.as_str())
        );
    }
}

#[test]
fn fixed_delegation_guidance_is_private_exact_and_idempotent() {
    use mj_core::subagent::SubagentPolicy;
    for harness in [HarnessKind::Claude, HarnessKind::Codex] {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join(harness.agent_instructions_file());
        std::fs::write(&source, "User instructions without final newline").unwrap();
        for policy in [
            SubagentPolicy::Native,
            SubagentPolicy::AllModels,
            SubagentPolicy::None,
            SubagentPolicy::SingleModel {
                model: "model".into(),
                effort: Some("high".into()),
            },
        ] {
            let stage = tempfile::tempdir().unwrap();
            let destination = stage.path().join(harness.agent_instructions_file());
            copy_profile_entry(&source, &destination).unwrap();
            append_subagent_policy(harness, stage.path(), &policy, 9).unwrap();
            append_subagent_policy(harness, stage.path(), &policy, 9).unwrap();
            let actual = std::fs::read_to_string(&destination).unwrap();
            let expected = if matches!(policy, SubagentPolicy::SingleModel { .. }) {
                format!(
                    "User instructions without final newline\n\n{}",
                    mj_core::subagent::delegation_policy(9)
                )
            } else {
                "User instructions without final newline".into()
            };
            assert_eq!(actual, expected);
            assert!(!actual.contains("$N"));
            assert_eq!(
                std::fs::read_to_string(&source).unwrap(),
                "User instructions without final newline"
            );
        }
    }
}

/// RCL-3: a restart re-hashed and re-copied every unchanged worker, which took
/// 30 s for one debug build on a busy disk. An unchanged source is now
/// recognised from its path, size, mtime, inode and ctime.
// Hard-won: 8b62e1b: rehashing unchanged worker sources delayed daemon startup by tens of seconds
#[test]
fn an_unchanged_worker_source_is_not_read_again_when_pinning() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, stamped_worker(&vec![7_u8; 4 << 20])).unwrap();
    let cache = directory.path().join("cache");

    let first = copy_worker_source_to_cache(&source, &cache).unwrap();
    verify_worker_build_indexed(&cache, &source).unwrap();

    // Damage the pinned copy without changing its length. A pin that read
    // the source or re-hashed the copy would notice; the indexed one does not.
    std::fs::write(
        &first,
        vec![0_u8; std::fs::metadata(&source).unwrap().len() as usize],
    )
    .unwrap();
    assert_eq!(copy_worker_source_to_cache(&source, &cache).unwrap(), first);
    verify_worker_build_indexed(&cache, &source).unwrap();

    // A rewritten source (new size or mtime) is read and checked in full.
    std::fs::write(&source, b"legacy worker").unwrap();
    assert!(verify_worker_build_indexed(&cache, &source).is_err());
    assert!(copy_worker_source_to_cache(&source, &cache).is_err());
}

#[test]
fn a_missing_pinned_copy_or_index_falls_back_to_a_full_pin() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("worker");
    std::fs::write(&source, stamped_worker(b"bytes")).unwrap();
    let cache = directory.path().join("cache");
    let first = copy_worker_source_to_cache(&source, &cache).unwrap();
    std::fs::remove_file(&first).unwrap();
    assert_eq!(copy_worker_source_to_cache(&source, &cache).unwrap(), first);
    verify_worker_build(&first).unwrap();
    std::fs::remove_dir_all(cache.join("index")).unwrap();
    assert_eq!(copy_worker_source_to_cache(&source, &cache).unwrap(), first);
}
