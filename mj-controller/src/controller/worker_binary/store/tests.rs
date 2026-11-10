use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

fn source(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    let mut bytes = vec![b'x'; 256 * 1024];
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(mj_core::worker_build::WORKER_BUILD_STAMP.as_bytes());
    std::fs::write(&path, bytes).unwrap();
    path
}

fn target(root: &Path, id: &str) -> targets::TargetLocator {
    let root = root.join(id);
    std::fs::create_dir_all(&root).unwrap();
    targets::TargetLocator::LocalBare {
        worker_root: root.to_string_lossy().into_owned(),
    }
}

fn install(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    id: &str,
    source: &Path,
) {
    install_worker_binary(
        executor,
        locator,
        id,
        &WorkerInstall::Host(source.to_owned()),
        "hel",
    )
    .unwrap();
}

#[derive(Default)]
struct Transfers(AtomicUsize);
impl CommandExecutor for Transfers {
    fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
        if command.purpose == "stage replacement Mjolnir worker" {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        targets::ProcessExecutor.execute(command)
    }
}

#[cfg(unix)]
#[test]
fn concurrent_sessions_share_one_verified_upload_and_swaps_leave_other_links_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let first = "11111111111111111111111111111111";
    let second = "22222222222222222222222222222222";
    let a = target(temp.path(), first);
    let b = target(temp.path(), second);
    let old = source(temp.path(), "old");
    let new = source(temp.path(), "new");
    let executor = Transfers::default();
    let others: Vec<_> = (0..2)
        .map(|n| {
            let id = format!("{n:032}");
            (target(temp.path(), &id), id)
        })
        .collect();
    std::thread::scope(|scope| {
        scope.spawn(|| install(&executor, &a, first, &old));
        scope.spawn(|| install(&executor, &b, second, &old));
        for (locator, id) in &others {
            scope.spawn(|| install(&executor, locator, id, &old));
        }
    });
    // A repeated install and a normal promotion can both name the inode that
    // is already installed. Plain mv reports these two hard links as an error.
    install(&executor, &a, first, &old);
    let staged = super::super::upgrade::stage_worker_binary_for_upgrade(
        &executor,
        &a,
        first,
        &WorkerInstall::Host(old.clone()),
    )
    .unwrap();
    let owner =
        crate::worker_lifecycle::WorkerPermit::try_acquire(first, "test current worker promotion")
            .unwrap()
            .unwrap();
    staged.install(&owner).unwrap();
    drop(staged);
    drop(owner);
    let a_path = temp.path().join(first).join("hel");
    let b_path = temp.path().join(second).join("hel");
    let digest = mj_core::worker_launch::worker_executable_digest(&old).unwrap();
    let cached = store_root(&a, first).unwrap().join(digest).join("hel");
    assert_eq!(executor.0.load(Ordering::Relaxed), 1);
    assert_eq!(
        a_path.metadata().unwrap().ino(),
        b_path.metadata().unwrap().ino()
    );
    assert_eq!(
        a_path.metadata().unwrap().ino(),
        cached.metadata().unwrap().ino()
    );
    // Initial install, not just upgrade promotion, must replace the directory
    // entry when the old executable shares its inode with another session.
    let profile = temp.path().join("empty-profile");
    std::fs::create_dir(&profile).unwrap();
    let config = temp.path().join("config");
    std::fs::write(&config, b"{}").unwrap();
    install_worker_files(
        &executor,
        &a,
        first,
        a_path.parent().unwrap().to_str().unwrap(),
        &temp.path().join("profile").to_string_lossy(),
        &WorkerInstall::Host(new.clone()),
        &config,
        &config,
        &profile,
        None,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&a_path).unwrap(),
        std::fs::read(&new).unwrap()
    );
    assert_eq!(
        std::fs::read(&b_path).unwrap(),
        std::fs::read(&old).unwrap()
    );
    assert_eq!(
        std::fs::read(&cached).unwrap(),
        std::fs::read(&old).unwrap()
    );
    std::fs::remove_dir_all(b_path.parent().unwrap()).unwrap();
    assert!(cached.exists(), "session teardown removed the shared entry");
}

#[cfg(unix)]
#[test]
fn store_collection_reclaims_only_old_unlinked_entries_and_corruption_is_replaced_privately() {
    let temp = tempfile::tempdir().unwrap();
    let id = "33333333333333333333333333333333";
    let locator = target(temp.path(), id);
    let bytes = source(temp.path(), "worker");
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    let root = store_root(&locator, id).unwrap();
    let digest = mj_core::worker_launch::worker_executable_digest(&bytes).unwrap();
    let linked = root.join(&digest).join("hel");
    let unlinked = root.join("a".repeat(64)).join("hel");
    let young = root.join("b".repeat(64)).join("hel");
    let abandoned = root.join(".upload-abandoned");
    std::fs::create_dir(&abandoned).unwrap();
    std::fs::write(abandoned.join("hel.next"), b"partial").unwrap();
    for path in [&unlinked, &young] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"unused").unwrap();
    }
    execute_checked(
        &targets::ProcessExecutor,
        CommandSpec::new(
            "touch",
            [
                "-t",
                "200001010000",
                linked.to_str().unwrap(),
                unlinked.to_str().unwrap(),
                abandoned.to_str().unwrap(),
            ],
        ),
    )
    .unwrap();
    collect(
        &targets::ProcessExecutor,
        &locator,
        root.to_str().unwrap(),
        None,
    )
    .unwrap();
    assert!(linked.exists());
    assert!(!unlinked.exists());
    assert!(!abandoned.exists());
    assert!(young.exists());
    // An old transport-cache entry may have no session links just before an
    // installer needs it. Collection must preserve that in-progress build.
    std::fs::remove_file(temp.path().join(id).join("hel")).unwrap();
    let cached = Transfers::default();
    install(&cached, &locator, id, &bytes);
    assert_eq!(cached.0.load(Ordering::Relaxed), 0);
    // Simulate external corruption via replacement; a repair must not write
    // through any existing inode or trust an unchecked content-addressed name.
    std::fs::remove_file(&linked).unwrap();
    std::fs::write(&linked, b"corrupt").unwrap();
    let executor = Transfers::default();
    install(&executor, &locator, id, &bytes);
    assert_eq!(
        std::fs::read(&linked).unwrap(),
        std::fs::read(&bytes).unwrap()
    );
    assert_eq!(executor.0.load(Ordering::Relaxed), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn cross_filesystem_link_uses_private_copy_without_changing_existing_links() {
    let temp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir_in("/dev/shm").unwrap();
    assert_ne!(
        temp.path().metadata().unwrap().dev(),
        other.path().metadata().unwrap().dev()
    );
    let id = "44444444444444444444444444444444";
    // The session root may be a mount while its sibling store is on the host
    // filesystem. A symlink reproduces that boundary without mount privileges.
    std::os::unix::fs::symlink(other.path(), temp.path().join(id)).unwrap();
    let locator = targets::TargetLocator::LocalBare {
        worker_root: temp.path().join(id).to_string_lossy().into_owned(),
    };
    let bytes = source(temp.path(), "worker");
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    let installed = other.path().join("hel");
    assert_eq!(
        std::fs::read(installed).unwrap(),
        std::fs::read(&bytes).unwrap()
    );
}

#[test]
fn a_filesystem_without_hard_links_still_installs_verified_private_copies() {
    struct NoLinks {
        bin: PathBuf,
    }
    impl CommandExecutor for NoLinks {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            let mut command = command.clone();
            command.env.insert(
                "PATH".into(),
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            );
            targets::ProcessExecutor.execute(&command)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    mj_core::test_hooks::install_fake_command(&bin, "ln", "#!/bin/sh\nexit 1\n");
    let id = "66666666666666666666666666666666";
    let locator = target(temp.path(), id);
    let bytes = source(temp.path(), "worker");
    install(&NoLinks { bin }, &locator, id, &bytes);
    assert_eq!(
        std::fs::read(temp.path().join(id).join("hel")).unwrap(),
        std::fs::read(bytes).unwrap()
    );
}

// Hard-won: 7f871fd03: the wait deadline expired during setup before the installer reached the held store gate.
#[test]
fn a_waiting_installer_observes_its_deadline_without_cancelling_the_store_owner() {
    use std::sync::mpsc;
    struct HeldUpload {
        reached: mpsc::Sender<()>,
        release: std::sync::Mutex<mpsc::Receiver<()>>,
    }
    impl CommandExecutor for HeldUpload {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            if command.purpose == "stage replacement Mjolnir worker" {
                self.reached.send(()).unwrap();
                // Keep the original deadlock watchdog, but fail on expiry:
                // never release the gate as though the waiter had finished.
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("deadline test waiter did not finish before the deadlock watchdog");
            }
            targets::ProcessExecutor.execute(command)
        }
    }
    #[derive(Default)]
    struct DeadlineAtWait(OnceLock<targets::CancellableProcessExecutor>);
    impl CommandExecutor for DeadlineAtWait {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            match self.0.get() {
                Some(executor) => executor.execute(command),
                None => targets::ProcessExecutor.execute(command),
            }
        }

        fn cancellation_requested(&self) -> bool {
            self.0.get().is_some_and(|executor| executor.is_cancelled())
        }

        fn stage_started(&self, stage: targets::ProvisionStage) {
            if stage == targets::ProvisionStage::WaitingForWorkerStore {
                // Expire only once queued, without racing setup or a clock.
                self.0.get_or_init(|| {
                    targets::CancellableProcessExecutor::with_timeout(Duration::ZERO)
                });
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let first = "77777777777777777777777777777777";
    let second = "88888888888888888888888888888888";
    let a = target(temp.path(), first);
    let b = target(temp.path(), second);
    let bytes = source(temp.path(), "worker");
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = HeldUpload {
        reached: reached_tx,
        release: std::sync::Mutex::new(release_rx),
    };
    std::thread::scope(|scope| {
        // Drop the sender on a setup panic too, so scope can join the owner.
        let release = release_tx;
        let owner_bytes = &bytes;
        let running = scope.spawn(move || install(&owner, &a, first, owner_bytes));
        // If setup fails, dropping the owner's sender wakes this receive.
        reached_rx.recv().unwrap();
        let executor = DeadlineAtWait::default();
        let result = install_worker_binary(
            &executor,
            &b,
            second,
            &WorkerInstall::Host(bytes.clone()),
            "hel",
        );
        release.send(()).unwrap();
        running.join().unwrap();
        assert_eq!(
            result.unwrap_err().to_string(),
            "timed out after 0 s while wait for the shared Mjolnir worker binary"
        );
        assert!(temp.path().join(first).join("hel").exists());
        assert!(!temp.path().join(second).join("hel").exists());
    });
}

#[test]
fn published_builds_install_without_waiting_on_each_other_or_on_an_upload_or_collection() {
    use std::sync::mpsc;
    struct Overlap {
        published_digest: String,
        expected: usize,
        arrived: AtomicUsize,
        overlapped: AtomicUsize,
        collects: AtomicUsize,
        reached: std::sync::Mutex<mpsc::Sender<()>>,
        release: std::sync::Mutex<mpsc::Receiver<()>>,
    }
    impl CommandExecutor for Overlap {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            match command.purpose.as_str() {
                // Another build's upload stays in flight for the whole test.
                "stage replacement Mjolnir worker" => {
                    self.reached.lock().unwrap().send(()).unwrap();
                    let _ = self
                        .release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20));
                }
                // Every link of the published build must be in flight at once;
                // serialized installs would leave each one alone here until its
                // deadline.
                "link shared Mjolnir worker into session"
                    if command
                        .args
                        .iter()
                        .any(|arg| arg.contains(&self.published_digest)) =>
                {
                    self.arrived.fetch_add(1, Ordering::SeqCst);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while self.arrived.load(Ordering::SeqCst) < self.expected
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if self.arrived.load(Ordering::SeqCst) >= self.expected {
                        self.overlapped.fetch_add(1, Ordering::SeqCst);
                    }
                }
                "collect unused Mjolnir worker binaries" => {
                    self.collects.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            targets::ProcessExecutor.execute(command)
        }
    }
    const SESSIONS: usize = 6;
    let temp = tempfile::tempdir().unwrap();
    let published = source(temp.path(), "published");
    let uploading = source(temp.path(), "uploading");
    let seed = "cccccccccccccccccccccccccccccccc";
    install(
        &targets::ProcessExecutor,
        &target(temp.path(), seed),
        seed,
        &published,
    );
    let sessions: Vec<_> = (0..SESSIONS)
        .map(|n| {
            let id = format!("d{n:031}");
            (target(temp.path(), &id), id)
        })
        .collect();
    let uploader = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let uploader_target = target(temp.path(), uploader);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let executor = Overlap {
        published_digest: mj_core::worker_launch::worker_executable_digest(&published).unwrap(),
        expected: SESSIONS,
        arrived: AtomicUsize::new(0),
        overlapped: AtomicUsize::new(0),
        collects: AtomicUsize::new(0),
        reached: std::sync::Mutex::new(reached_tx),
        release: std::sync::Mutex::new(release_rx),
    };
    std::thread::scope(|scope| {
        let upload = scope.spawn(|| install(&executor, &uploader_target, uploader, &uploading));
        reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let installs: Vec<_> = sessions
            .iter()
            .map(|(locator, id)| scope.spawn(|| install(&executor, locator, id, &published)))
            .collect();
        for install in installs {
            install.join().unwrap();
        }
        // The upload is still held: nobody queued behind it.
        let _ = release_tx.send(());
        upload.join().unwrap();
    });
    assert_eq!(executor.overlapped.load(Ordering::SeqCst), SESSIONS);
    // Only the upload collects; cached installs skip while it owns the store.
    assert_eq!(executor.collects.load(Ordering::SeqCst), 1);
}

// Hard-won: shared-store review: a foreign collector can unlink a fallback source before cp opens it.
#[test]
fn fallback_reuploads_a_source_retired_between_link_and_copy_but_preserves_other_errors() {
    struct RetiringCopy {
        bin: PathBuf,
        transfers: AtomicUsize,
    }
    impl CommandExecutor for RetiringCopy {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            if command.purpose == "stage replacement Mjolnir worker" {
                self.transfers.fetch_add(1, Ordering::Relaxed);
            }
            let mut command = command.clone();
            command.env.insert(
                "PATH".into(),
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            );
            targets::ProcessExecutor.execute(&command)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let id = "99999999999999999999999999999999";
    let locator = target(temp.path(), id);
    let bytes = source(temp.path(), "worker");
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    std::fs::remove_file(temp.path().join(id).join("hel")).unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    mj_core::test_hooks::install_fake_command(&bin, "ln", "#!/bin/sh\nexit 1\n");
    // Emulate the other collector after the shell has checked source existence,
    // immediately before cp opens it. Subsequent retries use real cp.
    mj_core::test_hooks::install_fake_command(
        &bin,
        "cp",
        &format!(
            r#"#!/bin/sh
case "$2" in */worker-binaries/*/hel)
    if [ ! -f '{marker}' ]; then
        touch '{marker}'
        rm -f -- "$2"
    fi;;
esac
exec /bin/cp "$@"
"#,
            marker = temp.path().join("retired").display()
        ),
    );
    let executor = RetiringCopy {
        bin: bin.clone(),
        transfers: AtomicUsize::new(0),
    };
    install(&executor, &locator, id, &bytes);
    assert_eq!(executor.transfers.load(Ordering::Relaxed), 1);
    assert_eq!(
        std::fs::read(temp.path().join(id).join("hel")).unwrap(),
        std::fs::read(&bytes).unwrap()
    );
    // A failing copy with its source still present must fail, not retry uploads.
    mj_core::test_hooks::install_fake_command(&bin, "cp", "#!/bin/sh\nexit 1\n");
    let error = install_worker_binary(&executor, &locator, id, &WorkerInstall::Host(bytes), "hel")
        .unwrap_err();
    assert!(
        error.to_string().contains("install shared Mjolnir worker"),
        "{error:#}"
    );
    assert_eq!(executor.transfers.load(Ordering::Relaxed), 1);
}

// Hard-won: shared-store review: SIGKILL skips shell EXIT traps and used to leak a permanent store reference.
#[test]
fn killed_store_pins_are_swept_on_the_next_install_while_live_pins_survive() {
    struct KillLink;
    impl CommandExecutor for KillLink {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            let mut command = command.clone();
            if command.purpose == "link shared Mjolnir worker into session" {
                command.args[1] = command.args[1].replace(
                    "# The private link pins",
                    "kill -KILL $$\n# The private link pins",
                );
            }
            targets::ProcessExecutor.execute(&command)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let locator = target(temp.path(), id);
    let bytes = source(temp.path(), "worker");
    let root = temp.path().join(id);
    let error = install_worker_binary(
        &KillLink,
        &locator,
        id,
        &WorkerInstall::Host(bytes.clone()),
        "hel",
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("install shared Mjolnir worker"),
        "{error:#}"
    );
    let stale: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("hel.store-")
        })
        .collect();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].metadata().unwrap().nlink(), 2);
    let live_name = "hel.store-live";
    let live = super::super::upgrade::LiveStaging::register_and_sweep(
        &targets::ProcessExecutor,
        &locator,
        id,
        root.to_str().unwrap(),
        live_name,
    )
    .unwrap();
    assert!(!stale[0].exists());
    let digest = mj_core::worker_launch::worker_executable_digest(&bytes).unwrap();
    let cached = store_root(&locator, id).unwrap().join(digest).join("hel");
    let live_path = root.join(live_name);
    std::fs::hard_link(&cached, &live_path).unwrap();
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    assert!(
        live_path.exists(),
        "the next install swept a registered pin"
    );
    drop(live);
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    assert!(!live_path.exists());
    assert_eq!(cached.metadata().unwrap().nlink(), 2);
}

// Hard-won: shared-store review: copying capacity was required even when a cached hard link needed no bytes.
#[test]
fn cached_installs_collect_and_link_on_full_targets_but_uploads_and_fallback_copies_are_refused() {
    use crate::controller::test_support::{IsolatedTest, test_name};
    if std::env::var_os("MJ_WORKER_STORE_CAPACITY_CHILD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), "cached_installs_collect_and_link_on_full_targets_but_uploads_and_fallback_copies_are_refused"))
            .isolated_store(temp.path()).env("MJ_WORKER_STORE_CAPACITY_CHILD", "1").run();
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let locator = target(temp.path(), id);
    let bytes = source(temp.path(), "worker");
    let other = source(temp.path(), "new");
    install(&targets::ProcessExecutor, &locator, id, &bytes);
    let store = store_root(&locator, id).unwrap();
    let garbage = store.join("c".repeat(64)).join("hel");
    std::fs::create_dir_all(garbage.parent().unwrap()).unwrap();
    std::fs::write(&garbage, b"unused").unwrap();
    execute_checked(
        &targets::ProcessExecutor,
        CommandSpec::new("touch", ["-t", "200001010000", garbage.to_str().unwrap()]),
    )
    .unwrap();
    let mut sample = crate::target_storage::tests::sample("local", 0, 0);
    sample.filesystems.truncate(1);
    sample.filesystems[0].paths = vec![temp.path().to_string_lossy().into_owned()];
    crate::target_storage::record_samples(&[sample], 1);
    // The first install above claimed this interval's collection.
    forget_collections();
    let executor = Transfers::default();
    install(&executor, &locator, id, &bytes);
    let profile = temp.path().join("empty-profile");
    std::fs::create_dir(&profile).unwrap();
    let config = temp.path().join("config");
    std::fs::write(&config, b"{}").unwrap();
    install_worker_files(
        &executor,
        &locator,
        id,
        temp.path().join(id).to_str().unwrap(),
        temp.path().join("profile").to_str().unwrap(),
        &WorkerInstall::Host(bytes.clone()),
        &config,
        &config,
        &profile,
        None,
    )
    .unwrap();
    super::super::upgrade::replace_target_worker_binary_if_stale(
        &executor,
        &locator,
        id,
        &CommandSpec::new("true", Vec::<String>::new()),
        &bytes,
    )
    .unwrap();
    let prepared = super::super::upgrade::stage_worker_binary_for_upgrade(
        &executor,
        &locator,
        id,
        &WorkerInstall::Host(bytes.clone()),
    )
    .unwrap();
    drop(prepared);
    assert!(!garbage.exists(), "capacity refusal skipped collection");
    assert_eq!(executor.0.load(Ordering::Relaxed), 0);
    let error = install_worker_binary(&executor, &locator, id, &WorkerInstall::Host(other), "hel")
        .unwrap_err();
    assert!(crate::target_storage::is_storage_full(&error), "{error:#}");
    struct NoLinks;
    impl CommandExecutor for NoLinks {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            if command.purpose == "link shared Mjolnir worker into session" {
                return Ok(targets::CommandOutput {
                    status: 76,
                    stdout: vec![],
                    stderr: vec![],
                });
            }
            targets::ProcessExecutor.execute(command)
        }
    }
    let error = install_worker_binary(&NoLinks, &locator, id, &WorkerInstall::Host(bytes), "hel")
        .unwrap_err();
    assert!(crate::target_storage::is_storage_full(&error), "{error:#}");
}

/// A wait behind a held gate is its own stage, open from the first failed
/// attempt until the gate is taken, so a lifecycle watching for stalls can
/// tell a queue from a stuck command. A free gate reports nothing.
#[test]
fn a_wait_behind_a_held_store_gate_is_reported_as_a_queue_stage() {
    #[derive(Default)]
    struct Stages(Mutex<Vec<(bool, targets::ProvisionStage)>>);
    impl CommandExecutor for Stages {
        fn execute(&self, command: &CommandSpec) -> Result<targets::CommandOutput> {
            unreachable!("waiting for a gate runs no command: {command:?}");
        }
        fn stage_started(&self, stage: targets::ProvisionStage) {
            self.0.lock().unwrap().push((true, stage));
        }
        fn stage_finished(&self, stage: targets::ProvisionStage) {
            self.0.lock().unwrap().push((false, stage));
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let locator = target(temp.path(), "33333333333333333333333333333333");
    let gate = store_gate(&locator, "queue-stage-test");
    let stages = Stages::default();
    drop(lock_gate(&stages, &gate, "free", "take a free gate").unwrap());
    assert!(stages.0.lock().unwrap().is_empty());

    let held = gate.try_lock("holder", "hold the gate").unwrap();
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            drop(lock_gate(&stages, &gate, "waiter", "wait for the gate").unwrap());
        });
        while stages.0.lock().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *stages.0.lock().unwrap(),
            vec![(true, targets::ProvisionStage::WaitingForWorkerStore)],
            "the stage is open while the gate is held"
        );
        drop(held);
        waiter.join().unwrap();
    });
    assert_eq!(
        *stages.0.lock().unwrap(),
        vec![
            (true, targets::ProvisionStage::WaitingForWorkerStore),
            (false, targets::ProvisionStage::WaitingForWorkerStore)
        ]
    );
}
