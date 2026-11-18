//! Receiver-owned negotiation for staged profile files.
use super::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Cursor, Seek};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const RECEIVER: &str = include_str!("profile_receiver.pl");
const PROBE: &str =
    "perl -MDigest::SHA -MFcntl -MFile::Copy -MFile::Path -MFile::Temp -MJSON::PP -e 1";

#[derive(serde::Serialize)]
struct ManifestEntry {
    // Unix filenames may contain newlines or non-UTF-8 bytes.
    path: String,
    directory: bool,
    mode: u32,
    size: u64,
    key: Option<String>,
}

/// Content inventory for a staged profile snapshot. Keep the staging tree
/// alive until transfer completes so claimed bodies match the recorded digest.
pub struct ProfileContentManifest {
    source: PathBuf,
    entries: Vec<ManifestEntry>,
    objects: BTreeMap<String, (PathBuf, u64)>,
    private: Vec<PathBuf>,
}

impl ProfileContentManifest {
    /// Use only roots declared by the profile stager. Unknown entries travel
    /// in the private bundle and are never offered to the receiver.
    pub fn from_staged(source: &Path, shared_paths: &[&Path]) -> Result<Self> {
        let entries = profile_archive_entries(source, shared_paths)?;
        let mut manifest = Self {
            source: source.to_owned(),
            entries: Vec::new(),
            objects: BTreeMap::new(),
            private: Vec::new(),
        };
        for entry in entries {
            let path = entry.path;
            let absolute = source.join(&path);
            if !entry.directory && entry.key.is_none() {
                manifest.private.push(path);
                continue;
            }
            let metadata = std::fs::metadata(&absolute)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            let key = entry.key.map(|key| {
                // The legacy store's executable suffix belongs to placement,
                // while negotiated content identity is the digest alone.
                let key = key.strip_suffix(".x").unwrap_or(&key).to_owned();
                manifest
                    .objects
                    .entry(key.clone())
                    .or_insert_with(|| (path.clone(), metadata.len()));
                key
            });
            // Normalize separators at the wire boundary. A literal backslash
            // in a Unix filename remains part of that filename.
            let wire_path = path
                .components()
                .map(|component| component.as_os_str().as_encoded_bytes())
                .collect::<Vec<_>>()
                .join(&b'/');
            manifest.entries.push(ManifestEntry {
                path: mj_core::hex::lower_hex(wire_path),
                directory: entry.directory,
                mode: header.mode()? & 0o777,
                size: metadata.len(),
                key,
            });
        }
        Ok(manifest)
    }

    fn private_bundle(&self) -> Result<Option<File>> {
        if self.private.is_empty() {
            return Ok(None);
        }
        Ok(Some(archive(|archive| {
            for path in &self.private {
                archive.append_path_with_name(self.source.join(path), path)?;
            }
            Ok(())
        })?))
    }

    /// Transfer to a fresh staging home, or prewarm just the non-secret
    /// objects when the destination is absent. A false result means the host
    /// lacks the receiver runtime. No profile bytes have been sent then.
    pub fn try_transfer(
        &self,
        executor: &impl CommandExecutor,
        ssh: &SshTarget,
        destination: Option<&str>,
    ) -> Result<bool> {
        let executor = targets::WithoutDeadline::new(executor);
        if !supported(&executor, ssh)? {
            return Ok(false);
        }
        transfer(&executor, ssh, self, destination)?;
        Ok(true)
    }

    fn batch(&self, negotiation: &Negotiation, materialize: bool) -> Result<File> {
        let private = if materialize {
            self.private_bundle()?
        } else {
            None
        };
        archive(|archive| {
            for key in &negotiation.claimed {
                let (path, _) = &self.objects[key];
                archive.append_path_with_name(self.source.join(path), format!("objects/{key}"))?;
            }
            if materialize {
                let manifest = serde_json::to_vec(&self.entries)?;
                let mut header = tar::Header::new_gnu();
                header.set_size(manifest.len() as u64);
                header.set_mode(0o600);
                archive.append_data(&mut header, "manifest.json", manifest.as_slice())?;
                if let Some(private) = private {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(private.metadata()?.len());
                    header.set_mode(0o600);
                    archive.append_data(&mut header, "private.tar.gz", private)?;
                }
            }
            Ok(())
        })
    }
}

fn archive(
    populate: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<&mut File>>) -> Result<()>,
) -> Result<File> {
    let mut file = tempfile::tempfile().context("create private profile transfer spool")?;
    let gzip = flate2::write::GzEncoder::new(&mut file, flate2::Compression::fast());
    let mut archive = tar::Builder::new(gzip);
    archive.follow_symlinks(true);
    populate(&mut archive)?;
    archive.into_inner()?.finish()?;
    file.rewind()?;
    Ok(file)
}

#[derive(Default)]
struct Negotiation {
    claimed: Vec<String>,
    busy: bool,
}

impl Negotiation {
    fn read(mut response: impl BufRead, manifest: &ProfileContentManifest) -> Result<Self> {
        let mut result = Self::default();
        let mut line = String::new();
        for expected in manifest.objects.keys() {
            line.clear();
            response.read_line(&mut line)?;
            let (status, key) = line
                .strip_suffix('\n')
                .and_then(|line| line.split_once(' '))
                .context("invalid profile negotiation reply")?;
            ensure!(
                key == expected,
                "profile receiver replied with unexpected object {key:?}"
            );
            match status {
                "claimed" => result.claimed.push(key.into()),
                "present" => {}
                "busy" => result.busy = true,
                _ => bail!("invalid profile negotiation state {status:?}"),
            }
        }
        line.clear();
        response.read_line(&mut line)?;
        ensure!(line == "\n", "incomplete profile negotiation reply");
        Ok(result)
    }
}

struct Replies {
    chunks: mpsc::Receiver<Option<Vec<u8>>>,
    next: Cursor<Vec<u8>>,
}

impl Read for Replies {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.next.position() == self.next.get_ref().len() as u64 {
            let Some(bytes) = self.chunks.recv().ok().flatten() else {
                return Ok(0);
            };
            self.next = Cursor::new(bytes);
        }
        self.next.read(buffer)
    }
}

struct BatchInput<'a> {
    manifest: &'a ProfileContentManifest,
    materialize: bool,
    offer: Cursor<Vec<u8>>,
    replies: Option<Replies>,
    archive: Option<File>,
    negotiation: Option<Negotiation>,
    bytes_sent: u64,
    send_started: Option<Instant>,
    send_elapsed: Duration,
}

impl Read for BatchInput<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.offer.read(buffer)?;
        if count > 0 {
            return Ok(count);
        }
        if let Some(replies) = self.replies.take() {
            let negotiation = Negotiation::read(BufReader::new(replies), self.manifest)
                .map_err(std::io::Error::other)?;
            let materialize = self.materialize && !negotiation.busy;
            if materialize || !negotiation.claimed.is_empty() {
                self.archive = Some(
                    self.manifest
                        .batch(&negotiation, materialize)
                        .map_err(std::io::Error::other)?,
                );
                self.send_started = Some(Instant::now());
            }
            self.negotiation = Some(negotiation);
        }
        let Some(archive) = &mut self.archive else {
            return Ok(0);
        };
        let count = archive.read(buffer)?;
        self.bytes_sent += count as u64;
        if count == 0 {
            self.send_elapsed = self.send_started.expect("profile send started").elapsed();
        }
        Ok(count)
    }
}

fn supported(executor: &impl CommandExecutor, ssh: &SshTarget) -> Result<bool> {
    let command = targets::ssh_command(SshTrafficClass::Control, ssh, ["sh", "-c", PROBE])
        .purpose("check negotiated profile receiver support");
    let output = executor.execute(&command)?;
    match output.status {
        0 => Ok(true),
        2 | 127 => Ok(false), // Perl compilation failure or Perl absent.
        _ => {
            targets::checked_command_output(&command, output)?;
            unreachable!()
        }
    }
}

fn transfer(
    executor: &impl CommandExecutor,
    ssh: &SshTarget,
    manifest: &ProfileContentManifest,
    destination: Option<&str>,
) -> Result<()> {
    let offered: u64 = manifest.objects.values().map(|(_, size)| size).sum();
    let mut offer = String::new();
    for (key, (_, size)) in &manifest.objects {
        use std::fmt::Write as _;
        writeln!(offer, "{key} {size}").expect("write to String");
    }
    offer.push('\n');
    let mut bytes_sent = 0;
    let mut objects_sent = 0;
    let mut claim_wait = Duration::ZERO;
    let mut send_time = Duration::ZERO;
    let reused = loop {
        let (emit, chunks) = mpsc::channel();
        let mut input = BatchInput {
            manifest,
            materialize: destination.is_some(),
            offer: Cursor::new(offer.as_bytes().to_vec()),
            replies: Some(Replies {
                chunks,
                next: Cursor::new(Vec::new()),
            }),
            archive: None,
            negotiation: None,
            bytes_sent: 0,
            send_started: None,
            send_elapsed: Duration::ZERO,
        };
        let command = targets::ssh_command(
            SshTrafficClass::Bulk,
            ssh,
            ["perl", "-e", RECEIVER, PROFILE_CONTENT_STORE]
                .into_iter()
                .chain(destination),
        )
        .purpose("send negotiated profile content")
        .warn_on_stderr()
        .stage(ProvisionStage::Syncing)
        .with_stream_reply(move |chunk| {
            // Input can finish before the child's stdout closes.
            let _ = emit.send(chunk.map(<[u8]>::to_vec));
        });
        let output = executor.execute_with_stdin(&command, &mut input)?;
        targets::checked_command_output(&command, output)?;
        let negotiation = input.negotiation.context("missing profile negotiation")?;
        bytes_sent += input.bytes_sent;
        objects_sent += negotiation.claimed.len();
        send_time += input.send_elapsed;
        if !negotiation.busy {
            break manifest.objects.len().saturating_sub(objects_sent);
        }
        let start = Instant::now();
        {
            let _queued = ProvisionStageGuard::new(executor, ProvisionStage::WaitingForWorkerStore);
            ensure!(
                targets::sleep_unless_cancelled(Duration::from_millis(250), &|| executor
                    .cancellation_requested()),
                "operation cancelled while waiting for profile content claims"
            );
        }
        claim_wait += start.elapsed();
    };
    tracing::info!(
        bytes_offered = offered,
        bytes_sent,
        objects_reused = reused,
        objects_sent,
        claim_wait_ms = claim_wait.as_millis() as u64,
        send_ms = send_time.as_millis() as u64,
        target = %ssh.destination,
        destination,
        "send profile content"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Hold = (mpsc::Sender<()>, mpsc::Receiver<()>);

    struct LocalReceiver<'a> {
        home: &'a Path,
        batches: Mutex<Vec<Vec<u8>>>,
        hold: Mutex<Option<Hold>>,
        pid: Option<&'a Path>,
        nofile: Option<u32>,
    }

    impl<'a> LocalReceiver<'a> {
        fn new(home: &'a Path) -> Self {
            Self {
                home,
                batches: Mutex::new(Vec::new()),
                hold: Mutex::new(None),
                pid: None,
                nofile: None,
            }
        }

        fn claimed_objects(&self) -> Vec<BTreeSet<String>> {
            self.batches
                .lock()
                .unwrap()
                .iter()
                .map(|bytes| {
                    let data = bytes.windows(2).position(|bytes| bytes == b"\n\n").unwrap() + 2;
                    if data == bytes.len() {
                        return BTreeSet::new();
                    }
                    let mut archive =
                        tar::Archive::new(flate2::read::GzDecoder::new(&bytes[data..]));
                    archive
                        .entries()
                        .unwrap()
                        .map(|entry| entry.unwrap().path().unwrap().into_owned())
                        .filter_map(|path| {
                            path.strip_prefix("objects")
                                .ok()
                                .map(|path| path.to_string_lossy().into_owned())
                        })
                        .collect()
                })
                .collect()
        }
    }

    impl CommandExecutor for LocalReceiver<'_> {
        fn pause_deadline(&self) -> targets::DeadlinePause {
            targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute_with_stdin(command, &mut std::io::empty())
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn Read + Send),
        ) -> Result<CommandOutput> {
            struct Record<'a> {
                input: &'a mut (dyn Read + Send),
                bytes: Vec<u8>,
                hold: Option<Hold>,
            }
            impl Read for Record<'_> {
                fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                    let count = self.input.read(buffer)?;
                    if self.bytes.ends_with(b"\n\n")
                        && count > 0
                        && let Some((reached, release)) = self.hold.take()
                    {
                        reached.send(()).unwrap();
                        release.recv_timeout(Duration::from_secs(15)).unwrap();
                    }
                    self.bytes.extend_from_slice(&buffer[..count]);
                    Ok(count)
                }
            }

            let mut local = command.clone();
            let mut script = command.args.last().unwrap().clone();
            let mut args = Vec::new();
            if let Some(pid) = self.pid {
                script = format!("printf '%s\\n' \"$$\" > \"$1\"; exec {script}");
                args.extend(["test-receiver".into(), pid.display().to_string()]);
            }
            if let Some(limit) = self.nofile {
                script = format!("ulimit -Sn {limit} || exit; {script}");
            }
            local.args = vec!["-c".into(), script];
            local.args.extend(args);
            local.program = "sh".into();
            local.ssh_session = None;
            local.ssh_destination = None;
            local.cwd = Some(self.home.to_owned());
            local
                .env
                .insert("HOME".into(), self.home.display().to_string());
            let mut record = Record {
                input,
                bytes: Vec::new(),
                hold: self.hold.lock().unwrap().take(),
            };
            let output = targets::ProcessExecutor.execute_with_stdin(&local, &mut record)?;
            self.batches.lock().unwrap().push(record.bytes);
            Ok(output)
        }
    }

    fn upload(
        executor: &LocalReceiver<'_>,
        ssh: &SshTarget,
        source: &Path,
        destination: &str,
        shared_paths: &[&Path],
    ) -> Result<()> {
        transfer(
            executor,
            ssh,
            &ProfileContentManifest::from_staged(source, shared_paths)?,
            Some(destination),
        )
    }

    fn dummy_ssh() -> SshTarget {
        SshTarget {
            destination: "unused".into(),
            ssh_args: vec![],
        }
    }

    fn digest(bytes: &[u8]) -> String {
        mj_core::hex::lower_hex(Sha256::digest(bytes))
    }

    #[test]
    fn overlapping_profile_offers_share_claims_and_recover_a_dead_sender() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let first = home.join("first");
        let second = home.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for (profile, unique) in [(&first, "only first"), (&second, "only second")] {
            std::fs::write(profile.join("common"), b"both profiles").unwrap();
            std::fs::write(profile.join("unique"), unique).unwrap();
        }
        let roots = ["common", "unique", "recover"].map(Path::new);
        let first_done = AtomicBool::new(false);
        let second_done = AtomicBool::new(false);
        let owner = LocalReceiver::new(home);
        let contender = LocalReceiver::new(home);
        let (reached, ready) = mpsc::channel();
        let (release, held) = mpsc::channel();
        *owner.hold.lock().unwrap() = Some((reached, held));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                upload(&owner, &dummy_ssh(), &first, "first-out", &roots).unwrap();
                first_done.store(true, Ordering::Release);
            });
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            scope.spawn(|| {
                upload(&contender, &dummy_ssh(), &second, "second-out", &roots).unwrap();
                second_done.store(true, Ordering::Release);
            });
            let disjoint = home
                .join(PROFILE_CONTENT_STORE)
                .join(digest(b"only second"));
            let start = Instant::now();
            while !disjoint.exists() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(disjoint.exists());
            assert!(!first_done.load(Ordering::Acquire));
            assert!(!second_done.load(Ordering::Acquire));
            release.send(()).unwrap();
        });
        assert_eq!(
            std::fs::read(home.join("second-out/common")).unwrap(),
            b"both profiles"
        );
        assert_eq!(
            owner.claimed_objects(),
            [BTreeSet::from([
                digest(b"both profiles"),
                digest(b"only first")
            ])]
        );
        let contender_objects: Vec<_> = contender.claimed_objects().into_iter().flatten().collect();
        assert_eq!(contender_objects, [digest(b"only second")]);

        std::fs::write(first.join("recover"), b"dead owner bytes").unwrap();
        let pid = home.join("receiver.pid");
        let failed = LocalReceiver {
            pid: Some(&pid),
            ..LocalReceiver::new(home)
        };
        let (reached, ready) = mpsc::channel();
        let (release, held) = mpsc::channel();
        *failed.hold.lock().unwrap() = Some((reached, held));
        std::thread::scope(|scope| {
            let original =
                scope.spawn(|| upload(&failed, &dummy_ssh(), &first, "dead-out", &roots));
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            let pid = std::fs::read_to_string(&pid).unwrap();
            execute_checked(
                &targets::ProcessExecutor,
                CommandSpec::new("kill", ["-KILL", pid.trim()]),
            )
            .unwrap();
            release.send(()).unwrap();
            assert!(original.join().unwrap().is_err());
        });
        let manifest = ProfileContentManifest::from_staged(&first, &roots).unwrap();
        let recover_key = digest(b"dead owner bytes");
        let store = home.join(PROFILE_CONTENT_STORE);
        let abandoned_claim = store.join(".stage-negotiated/claims").join(&recover_key);
        let abandoned = std::fs::metadata(&abandoned_claim).unwrap();

        // Start two receivers against the same dead claim and hold whichever
        // receives it before any bytes are published. The other receiver
        // must observe the fresh link as busy while the winner still owns it.
        let contenders = [LocalReceiver::new(home), LocalReceiver::new(home)];
        let (reached, ready) = mpsc::channel();
        let releases = contenders.each_ref().map(|receiver| {
            let (release, held) = mpsc::channel();
            *receiver.hold.lock().unwrap() = Some((reached.clone(), held));
            release
        });
        std::thread::scope(|scope| {
            for contender in &contenders {
                let manifest = &manifest;
                scope.spawn(move || transfer(contender, &dummy_ssh(), manifest, None).unwrap());
            }
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            let start = Instant::now();
            while contenders
                .iter()
                .all(|receiver| receiver.batches.lock().unwrap().is_empty())
                && start.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                contenders
                    .iter()
                    .any(|receiver| !receiver.batches.lock().unwrap().is_empty())
            );
            assert!(!store.join(&recover_key).exists());
            assert_ne!(
                std::fs::metadata(&abandoned_claim).unwrap().ino(),
                abandoned.ino()
            );
            for release in releases {
                // The receiver that got only a busy reply dropped its hold.
                let _ = release.send(());
            }
        });
        let recovered: Vec<_> = contenders
            .iter()
            .flat_map(LocalReceiver::claimed_objects)
            .flatten()
            .collect();
        assert_eq!(recovered.as_slice(), std::slice::from_ref(&recover_key));
        assert_eq!(
            std::fs::read(store.join(recover_key)).unwrap(),
            b"dead owner bytes"
        );
        assert!(!abandoned_claim.exists());
    }

    // Hard-won: c4831a676: cold EC2 receivers exhausted 1,024 file descriptors holding one lock per object.
    #[test]
    fn cold_profile_claims_more_objects_than_the_receiver_descriptor_limit() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let source = home.join("source");
        let objects = source.join("rules");
        std::fs::create_dir_all(&objects).unwrap();
        let limit = 1024_u32;
        let mut expected = BTreeSet::new();
        for index in 0..limit + 128 {
            let bytes = index.to_le_bytes();
            std::fs::write(objects.join(index.to_string()), bytes).unwrap();
            expected.insert(digest(&bytes));
        }
        let receiver = LocalReceiver {
            nofile: Some(limit),
            ..LocalReceiver::new(home)
        };
        let roots = shared_profile_paths(HarnessKind::Codex);
        let manifest = ProfileContentManifest::from_staged(&source, &roots).unwrap();

        transfer(&receiver, &dummy_ssh(), &manifest, None).unwrap();
        transfer(&receiver, &dummy_ssh(), &manifest, None).unwrap();

        let store = home.join(PROFILE_CONTENT_STORE);
        assert_eq!(receiver.claimed_objects(), [expected, BTreeSet::new()]);
        assert_eq!(
            std::fs::read_dir(store.join(".stage-negotiated/claims"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn negotiated_profile_stream_sends_large_replica_once_and_keeps_tokens_private() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let source = home.join("source");
        std::fs::create_dir_all(source.join("replica")).unwrap();
        std::fs::create_dir_all(source.join("baseline")).unwrap();
        let mut bytes = vec![0; 96 * 1024 + 17];
        getrandom::fill(&mut bytes).unwrap();
        std::fs::write(source.join("replica/topic"), &bytes).unwrap();
        std::fs::write(source.join("baseline/topic"), &bytes).unwrap();
        std::fs::write(source.join("auth.json"), b"a private login").unwrap();
        std::fs::write(source.join("settings.json"), b"{\"enabled\":true}").unwrap();
        std::fs::write(
            source.join("config.toml"),
            b"experimental_bearer_token = 'a literal secret'",
        )
        .unwrap();
        let receiver = LocalReceiver::new(home);
        let roots = ["replica", "baseline"].map(Path::new);
        let manifest = ProfileContentManifest::from_staged(&source, &roots).unwrap();
        transfer(&receiver, &dummy_ssh(), &manifest, None).unwrap();
        assert!(!home.join("warm").exists());
        upload(&receiver, &dummy_ssh(), &source, "warm", &roots).unwrap();
        assert_eq!(
            receiver.claimed_objects(),
            [BTreeSet::from([digest(&bytes)]), BTreeSet::new(),]
        );
        assert!(receiver.batches.lock().unwrap()[0].len() > 64 * 1024);
        for file in [
            "baseline/topic",
            "replica/topic",
            "auth.json",
            "config.toml",
            "settings.json",
        ] {
            assert_eq!(
                std::fs::read(home.join("warm").join(file)).unwrap(),
                std::fs::read(source.join(file)).unwrap()
            );
        }
        let store = home.join(PROFILE_CONTENT_STORE);
        assert!(!store.join(digest(b"a private login")).exists());
        assert!(
            !store
                .join(digest(b"experimental_bearer_token = 'a literal secret'"))
                .exists()
        );
    }
    // Hard-won: 75f2a04dd: opaque configuration arguments could enter the shared store.
    #[test]
    fn unclassified_profile_entries_stay_private() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let source = home.join("source");
        std::fs::create_dir_all(source.join("plugins")).unwrap();
        let plugin = b"declared plugin asset";
        std::fs::write(source.join("plugins/asset"), plugin).unwrap();
        let entries = [
            ("future-harness-entry", b"unknown staged entry".as_slice()),
            ("mcp.json", br#"{"mcpServers":{"example":{"command":"example","args":["--token","synthetic-argument-secret"]}}}"#.as_slice()),
        ];
        for (name, bytes) in entries {
            std::fs::write(source.join(name), bytes).unwrap();
        }
        let receiver = LocalReceiver::new(home);
        let roots = shared_profile_paths(HarnessKind::Kimi);
        upload(&receiver, &dummy_ssh(), &source, "installed", &roots).unwrap();
        assert_eq!(
            receiver.claimed_objects(),
            [BTreeSet::from([digest(plugin)])]
        );
        for (name, bytes) in entries {
            assert_eq!(
                std::fs::read(home.join("installed").join(name)).unwrap(),
                bytes
            );
            assert!(
                !home
                    .join(PROFILE_CONTENT_STORE)
                    .join(digest(bytes))
                    .exists()
            );
        }
    }
}
