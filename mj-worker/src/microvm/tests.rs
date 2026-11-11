//! The vsock protocol between an engine client and the guest agent, over a
//! Unix socket pair. The pair is the same byte stream a vsock connection
//! gives; only the transport differs.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::client::{self, Client};
use super::guest_agent::test_support::RecordingSystem;
use super::guest_agent::{Agent, DirectSpawner};
use super::protocol::{
    ExecRequest, ExitInfo, FrameKind, FrameReader, FrameSender, Hello, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION, Request,
};

struct Harness {
    agent: Arc<Agent>,
    system: Arc<RecordingSystem>,
}

impl Harness {
    fn new() -> Self {
        let system = Arc::new(RecordingSystem::default());
        Self {
            agent: Agent::new(Arc::new(DirectSpawner), system.clone()),
            system,
        }
    }

    /// A client connected to a fresh agent connection, and the agent's thread.
    fn connect(&self) -> (UnixStream, JoinHandle<anyhow::Result<()>>) {
        let (client_side, agent_side) = UnixStream::pair().unwrap();
        let agent = Arc::clone(&self.agent);
        let thread = std::thread::spawn(move || agent.serve(agent_side));
        (client_side, thread)
    }

    fn client(&self) -> Client<UnixStream> {
        let (stream, _thread) = self.connect();
        client::handshake(stream).unwrap()
    }
}

fn exec_request(argv: &[&str]) -> ExecRequest {
    ExecRequest {
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        ..ExecRequest::default()
    }
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i % 251) as u8).collect()
}

#[test]
fn exec_moves_more_than_a_pipe_buffer_in_both_directions_at_once() {
    // `cat` echoes while it reads, so a client that sent all its input before
    // reading output would deadlock once either 64 KiB pipe filled.
    let harness = Harness::new();
    let input = pattern(1 << 20);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = harness
        .client()
        .exec(
            exec_request(&["sh", "-c", "cat; head -c 200000 /dev/zero | tr '\\0' x >&2"]),
            Some(Box::new(std::io::Cursor::new(input.clone()))),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
    assert_eq!(exit.code, Some(0));
    assert!(stdout == input, "stdout differs from the {} byte input", input.len());
    assert_eq!(stderr, vec![b'x'; 200_000]);
}

#[test]
fn exec_survives_a_process_that_stops_reading_stdin_early() {
    let harness = Harness::new();
    let mut stdout = Vec::new();
    let exit = harness
        .client()
        .exec(
            exec_request(&["head", "-c", "10"]),
            Some(Box::new(std::io::Cursor::new(pattern(4 << 20)))),
            &mut stdout,
            &mut Vec::new(),
        )
        .unwrap();
    assert_eq!(exit.code, Some(0));
    assert_eq!(stdout, pattern(10));
}

/// Hands out one line, then waits to be released before reporting end of input.
struct HeldInput {
    sent_first: bool,
    release: mpsc::Receiver<()>,
}

impl Read for HeldInput {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.sent_first {
            self.sent_first = true;
            buffer[..6].copy_from_slice(b"hello\n");
            return Ok(6);
        }
        self.release.recv().ok();
        Ok(0)
    }
}

/// Releases the input the first time output arrives.
struct ReleaseOnOutput {
    release: Option<mpsc::Sender<()>>,
    seen: Vec<u8>,
}

impl Write for ReleaseOnOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.seen.extend_from_slice(data);
        if let Some(release) = self.release.take() {
            release.send(()).unwrap();
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn exec_input_closed_by_the_client_after_output_has_started() {
    let harness = Harness::new();
    let (release, held) = mpsc::channel();
    let mut stdout = ReleaseOnOutput {
        release: Some(release),
        seen: Vec::new(),
    };
    let exit = harness
        .client()
        .exec(
            exec_request(&["sh", "-c", "read line; echo got $line; cat; echo done"]),
            Some(Box::new(HeldInput {
                sent_first: false,
                release: held,
            })),
            &mut stdout,
            &mut Vec::new(),
        )
        .unwrap();
    assert_eq!(exit.code, Some(0));
    assert_eq!(String::from_utf8_lossy(&stdout.seen), "got hello\ndone\n");
}

#[test]
fn exec_reports_exit_codes_signals_and_missing_programs() {
    let harness = Harness::new();
    let run = |argv: &[&str]| -> (ExitInfo, String) {
        let mut stderr = Vec::new();
        let exit = harness
            .client()
            .exec(exec_request(argv), None, &mut Vec::new(), &mut stderr)
            .unwrap();
        (exit, String::from_utf8_lossy(&stderr).into_owned())
    };
    let (exit, _) = run(&["sh", "-c", "exit 7"]);
    assert_eq!((exit.code, exit.signal, exit.shell_status()), (Some(7), None, 7));
    let (exit, _) = run(&["sh", "-c", "kill -KILL $$"]);
    assert_eq!((exit.code, exit.signal, exit.shell_status()), (None, Some(9), 137));
    let (exit, stderr) = run(&["definitely-not-a-program"]);
    assert_eq!(exit.code, Some(127));
    assert!(stderr.contains("not found"), "{stderr}");
}

#[test]
fn detached_exec_answers_at_once_and_the_process_keeps_running() {
    let harness = Harness::new();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("marker");
    let pid = harness
        .client()
        .exec_detached(exec_request(&["sh", "-c", &format!("sleep 0.3; touch {}", marker.display())]))
        .unwrap();
    assert!(pid > 0);
    assert!(!marker.exists());
    for _ in 0..100 {
        if marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the detached process never ran");
}

fn raw_hello(stream: &UnixStream, hello: &Hello) -> FrameSender<UnixStream> {
    let sender = FrameSender::new(stream.try_clone().unwrap());
    sender.send_json(FrameKind::Hello, hello).unwrap();
    sender
}

#[test]
fn a_peer_with_no_common_protocol_version_is_refused_with_a_reason() {
    let harness = Harness::new();
    let (stream, thread) = harness.connect();
    let _sender = raw_hello(&stream, &Hello { version: 0, min_version: 0 });
    let mut frames = FrameReader::new(stream);
    let reply = frames.expect("the refusal").unwrap();
    assert_eq!(reply.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&reply.payload).contains("do not overlap"));
    assert!(thread.join().unwrap().is_err());
}

#[test]
fn a_newer_peer_is_served_at_the_version_both_speak() {
    let harness = Harness::new();
    let (stream, thread) = harness.connect();
    let sender = raw_hello(
        &stream,
        &Hello {
            version: PROTOCOL_VERSION + 5,
            min_version: MIN_PROTOCOL_VERSION,
        },
    );
    let mut frames = FrameReader::new(stream);
    let reply = frames.expect("the agent hello").unwrap();
    assert_eq!(reply.kind, FrameKind::Hello);
    let hello: Hello = serde_json::from_slice(&reply.payload).unwrap();
    assert_eq!(hello.version, PROTOCOL_VERSION);
    sender
        .send_json(FrameKind::Request, &Request::Exec(exec_request(&["true"])))
        .unwrap();
    let exit = frames.expect("the exit frame").unwrap();
    assert_eq!(exit.kind, FrameKind::Exit);
    thread.join().unwrap().unwrap();
}

#[test]
fn copy_moves_a_directory_in_and_out_with_podman_naming() {
    let harness = Harness::new();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("tree");
    std::fs::create_dir_all(source.join("sub")).unwrap();
    std::fs::write(source.join("big.bin"), pattern(300_000)).unwrap();
    std::fs::write(source.join("sub/a.txt"), "alpha").unwrap();
    std::os::unix::fs::symlink("sub/a.txt", source.join("link")).unwrap();

    // Into an existing directory: the tree lands inside it under its own name.
    let inbox = work.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    harness.client().copy_in(&source, inbox.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read(inbox.join("tree/big.bin")).unwrap(), pattern(300_000));
    assert_eq!(std::fs::read_to_string(inbox.join("tree/sub/a.txt")).unwrap(), "alpha");
    assert_eq!(std::fs::read_link(inbox.join("tree/link")).unwrap().to_str(), Some("sub/a.txt"));

    // To a path that does not exist: the destination becomes the tree's name.
    let back = work.path().join("back");
    harness
        .client()
        .copy_out(inbox.join("tree").to_str().unwrap(), &back, None)
        .unwrap();
    assert_eq!(std::fs::read(back.join("big.bin")).unwrap(), pattern(300_000));
    assert_eq!(std::fs::read_to_string(back.join("sub/a.txt")).unwrap(), "alpha");

    // A missing destination parent is the agent's refusal, relayed to the client.
    let error = harness
        .client()
        .copy_in(&source, "/no/such/parent/tree")
        .unwrap_err();
    assert!(format!("{error:#}").contains("does not exist"), "{error:#}");
}

#[test]
fn unpacking_never_leaves_the_destination_or_writes_through_a_symlink() {
    let work = tempfile::tempdir().unwrap();
    let destination = work.path().join("dest");
    std::fs::create_dir(&destination).unwrap();
    let outside = work.path().join("outside");
    std::fs::create_dir(&outside).unwrap();

    let tar_with = |entries: &[(&str, tar::EntryType, &str)]| -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, target) in entries {
            let mut header = tar::Header::new_gnu();
            // Written into the header directly: `set_path` itself refuses "..".
            let name = &mut header.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(*kind);
            header.set_size(if *kind == tar::EntryType::Regular { 3 } else { 0 });
            header.set_mode(if *kind == tar::EntryType::Directory { 0o755 } else { 0o644 });
            if !target.is_empty() {
                header.set_link_name(target).unwrap();
            }
            header.set_cksum();
            builder.append(&header, &b"abc"[..if *kind == tar::EntryType::Regular { 3 } else { 0 }]).unwrap();
        }
        builder.into_inner().unwrap()
    };

    let traversal = tar_with(&[("top/../../escaped", tar::EntryType::Regular, "")]);
    let error = super::tarcopy::unpack(&traversal[..], &destination, None).unwrap_err();
    assert!(format!("{error:#}").contains("leaves its destination"), "{error:#}");
    assert!(!work.path().join("escaped").exists());

    let through_link = tar_with(&[
        ("top", tar::EntryType::Directory, ""),
        ("top/out", tar::EntryType::Symlink, outside.to_str().unwrap()),
        ("top/out/file", tar::EntryType::Regular, ""),
    ]);
    let error = super::tarcopy::unpack(&through_link[..], &destination, None).unwrap_err();
    assert!(format!("{error:#}").contains("not a directory"), "{error:#}");
    assert!(!outside.join("file").exists());

    let device = tar_with(&[("top", tar::EntryType::Char, "")]);
    assert!(super::tarcopy::unpack(&device[..], &destination, None).is_err());
}

#[test]
fn stop_flushes_before_it_answers_and_then_powers_off() {
    let harness = Harness::new();
    harness.client().stop().unwrap();
    assert!(harness.system.quiesced.load(Ordering::SeqCst));
    for _ in 0..100 {
        if harness.system.powered_off.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the agent never powered the machine off");
}
